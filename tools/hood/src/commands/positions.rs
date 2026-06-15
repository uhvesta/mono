use std::collections::{HashMap, HashSet};

use broker_robinhood::RobinhoodClient;
use broker_robinhood::RobinhoodClientError;
use broker_robinhood::client::RobinhoodAccount;
use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table, presets::UTF8_BORDERS_ONLY};
use console::set_colors_enabled;
use serde_json::Value;
use thiserror::Error;
use tokio::time::{Duration, sleep};

use crate::creds;

type Result<T> = std::result::Result<T, PositionsError>;

#[derive(Debug, Error)]
pub enum PositionsError {
    #[error(transparent)]
    Credentials(#[from] creds::CredentialsError),
    #[error(transparent)]
    RobinhoodClient(#[from] RobinhoodClientError),
    #[error("no default Robinhood account found")]
    MissingDefaultAccount,
    #[error("Robinhood account `{account}` not found")]
    UnknownAccount { account: String },
}

const QUOTE_LOOKUP_CHUNK: usize = 50;

#[derive(Clone, Debug, PartialEq)]
struct PositionRow {
    account_number: String,
    symbol: String,
    quantity: f64,
    equity: Option<f64>,
    percentage_change: Option<f64>,
    todays_return: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct QuoteSnapshot {
    last_trade_price: Option<f64>,
    previous_close: Option<f64>,
}

pub async fn run(username: Option<&str>, account: &str, follow: bool, csv: bool) -> Result<()> {
    set_colors_enabled(true);

    let (_, access_token) = creds::load_access_token(username)?;

    let client = RobinhoodClient::new()?;
    let accounts = client.fetch_accounts(&access_token).await?;

    if accounts.is_empty() {
        println!("No Robinhood accounts found.");
        return Ok(());
    }

    let selected_accounts = select_accounts(accounts, account)?;

    loop {
        let rows = build_position_rows(&client, &access_token, &selected_accounts).await?;

        if follow && !csv {
            print!("\x1B[2J\x1B[H");
        }

        if rows.is_empty() {
            if csv {
                print!("{}", render_positions_csv(&rows));
            } else {
                println!("No open positions found.");
            }
        } else if csv {
            print!("{}", render_positions_csv(&rows));
        } else {
            println!("{}", render_positions_table(&rows));
        }

        if !follow {
            return Ok(());
        }

        sleep(Duration::from_secs(10)).await;
    }
}

async fn build_position_rows(
    client: &RobinhoodClient,
    access_token: &str,
    selected_accounts: &[RobinhoodAccount],
) -> Result<Vec<PositionRow>> {
    let mut rows = Vec::new();
    for account in selected_accounts {
        let mut positions = client.fetch_positions(access_token, &account.account_number).await?;
        positions.sort_by(|left, right| left.symbol.cmp(&right.symbol));

        for position in positions {
            rows.push(PositionRow {
                account_number: account.account_number.clone(),
                symbol: position.symbol,
                quantity: position.quantity,
                equity: None,
                percentage_change: None,
                todays_return: None,
            });
        }
    }

    if rows.is_empty() {
        return Ok(rows);
    }

    let symbols = rows.iter().map(|row| row.symbol.clone()).collect::<Vec<String>>();
    let quotes = fetch_quote_snapshots(client, access_token, &symbols).await?;

    for row in &mut rows {
        let quote = quotes.get(&row.symbol);
        let (equity, percentage_change, todays_return) = calculate_position_metrics(row.quantity, quote);
        row.equity = equity;
        row.percentage_change = percentage_change;
        row.todays_return = todays_return;
    }

    rows.sort_by(|left, right| {
        left.account_number
            .cmp(&right.account_number)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });

    Ok(rows)
}

fn select_accounts(accounts: Vec<RobinhoodAccount>, account: &str) -> Result<Vec<RobinhoodAccount>> {
    if account == "default" {
        let default = accounts
            .into_iter()
            .find(|candidate| candidate.is_default)
            .ok_or(PositionsError::MissingDefaultAccount)?;
        return Ok(vec![default]);
    }

    let selected = accounts
        .into_iter()
        .filter(|candidate| candidate.account_number == account)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(PositionsError::UnknownAccount {
            account: account.to_string(),
        });
    }

    Ok(selected)
}

fn render_positions_table(rows: &[PositionRow]) -> String {
    let show_account_column = rows
        .iter()
        .map(|row| row.account_number.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1;
    let total_equity = sum_optional_values(rows.iter().map(|row| row.equity));
    let total_todays_return = sum_optional_values(rows.iter().map(|row| row.todays_return));
    let total_percentage_change = calculate_total_percentage_change(total_equity, total_todays_return);
    let quantity_values = rows.iter().map(|row| format_quantity(row.quantity)).collect::<Vec<_>>();
    let aligned_quantities = align_decimal_column(&quantity_values);

    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic);

    let mut header = Vec::new();
    if show_account_column {
        header.push(Cell::new("Account").add_attribute(Attribute::Bold));
    }
    header.extend([
        Cell::new("Symbol").add_attribute(Attribute::Bold),
        Cell::new("Quantity")
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
        Cell::new("Equity")
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
        Cell::new("% Change")
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
        Cell::new("Today's Return")
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
    ]);
    table.set_header(header);

    for (row, aligned_quantity) in rows.iter().zip(aligned_quantities.iter()) {
        let mut row_cells = Vec::new();
        if show_account_column {
            row_cells.push(Cell::new(&row.account_number).fg(Color::Cyan));
        }
        row_cells.extend([
            Cell::new(&row.symbol).fg(Color::Yellow),
            Cell::new(aligned_quantity)
                .fg(Color::White)
                .set_alignment(CellAlignment::Right),
            Cell::new(format_optional_currency(row.equity))
                .fg(Color::White)
                .set_alignment(CellAlignment::Right),
            Cell::new(format_optional_percentage(row.percentage_change))
                .fg(color_for_change(row.percentage_change))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_optional_signed_currency(row.todays_return))
                .fg(color_for_change(row.todays_return))
                .set_alignment(CellAlignment::Right),
        ]);
        table.add_row(row_cells);
    }

    let mut total_row = Vec::new();
    if show_account_column {
        total_row.push(Cell::new(""));
    }
    total_row.extend([
        Cell::new("Total").add_attribute(Attribute::Bold),
        Cell::new("").fg(Color::White).set_alignment(CellAlignment::Right),
        Cell::new(format_optional_currency(total_equity))
            .fg(Color::White)
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
        Cell::new(format_optional_percentage(total_percentage_change))
            .fg(color_for_change(total_percentage_change))
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
        Cell::new(format_optional_signed_currency(total_todays_return))
            .fg(color_for_change(total_todays_return))
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Right),
    ]);
    table.add_row(total_row);

    table.to_string()
}

fn render_positions_csv(rows: &[PositionRow]) -> String {
    let mut output = String::from("account,symbol,quantity,equity,percentage_change,todays_return\n");
    for row in rows {
        let fields = [
            csv_escape(&row.account_number),
            csv_escape(&row.symbol),
            row.quantity.to_string(),
            format_optional_raw_number(row.equity),
            format_optional_raw_number(row.percentage_change),
            format_optional_raw_number(row.todays_return),
        ];
        output.push_str(&fields.join(","));
        output.push('\n');
    }
    output
}

async fn fetch_quote_snapshots(
    client: &RobinhoodClient,
    access_token: &str,
    symbols: &[String],
) -> Result<HashMap<String, QuoteSnapshot>> {
    let mut unique_symbols = Vec::new();
    let mut seen = HashSet::new();

    for symbol in symbols {
        let normalized = symbol.trim().to_uppercase();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            unique_symbols.push(normalized);
        }
    }

    if unique_symbols.is_empty() {
        return Ok(HashMap::new());
    }

    let mut quotes = HashMap::new();
    for chunk in unique_symbols.chunks(QUOTE_LOOKUP_CHUNK) {
        let mut url = client
            .base_url()
            .join("marketdata/quotes/")
            .map_err(|error| PositionsError::RobinhoodClient(RobinhoodClientError::InvalidEndpointUrl(error)))?;
        url.set_query(Some(&format!("symbols={}", chunk.join(","))));

        let response = client
            .http()
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| PositionsError::RobinhoodClient(RobinhoodClientError::HttpClient(error)))?;

        if !response.status().is_success() {
            return Err(PositionsError::RobinhoodClient(RobinhoodClientError::UnexpectedStatus(
                response.status(),
            )));
        }

        let body = response
            .bytes()
            .await
            .map_err(|error| PositionsError::RobinhoodClient(RobinhoodClientError::HttpClient(error)))?;
        let payload: Value = serde_json::from_slice(&body)
            .map_err(|error| PositionsError::RobinhoodClient(RobinhoodClientError::ResponseBodyParse(error)))?;

        let Some(results) = payload.get("results").and_then(Value::as_array) else {
            continue;
        };

        for entry in results {
            let Some(symbol) = entry.get("symbol").and_then(Value::as_str) else {
                continue;
            };
            let normalized = symbol.trim().to_uppercase();
            if normalized.is_empty() {
                continue;
            }

            quotes.insert(
                normalized,
                QuoteSnapshot {
                    last_trade_price: parse_quote_number(entry.get("last_trade_price")),
                    previous_close: parse_quote_number(entry.get("previous_close")),
                },
            );
        }
    }

    Ok(quotes)
}

fn parse_quote_number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    }
}

fn calculate_position_metrics(quantity: f64, quote: Option<&QuoteSnapshot>) -> (Option<f64>, Option<f64>, Option<f64>) {
    let Some(quote) = quote else {
        return (None, None, None);
    };

    let equity = quote.last_trade_price.map(|price| price * quantity);
    let (percentage_change, todays_return) =
        if let (Some(last_trade_price), Some(previous_close)) = (quote.last_trade_price, quote.previous_close) {
            if previous_close.abs() > f64::EPSILON {
                let change = last_trade_price - previous_close;
                (Some((change / previous_close) * 100.0), Some(change * quantity))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

    (equity, percentage_change, todays_return)
}

fn format_quantity(quantity: f64) -> String {
    if quantity.abs() < f64::EPSILON {
        return "0".to_string();
    }

    let sign = if quantity < 0.0 { "-" } else { "" };
    let mut text = format!("{:.6}", quantity.abs());
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }

    let mut parts = text.split('.');
    let integer_part = parts.next().unwrap_or_default();
    let fractional_part = parts.next();
    let with_grouping = format_integer_with_grouping(integer_part);

    match fractional_part {
        Some(fractional) if !fractional.is_empty() => {
            format!("{sign}{with_grouping}.{fractional}")
        }
        _ => format!("{sign}{with_grouping}"),
    }
}

fn format_integer_with_grouping(integer: &str) -> String {
    let chars = integer.chars().collect::<Vec<_>>();
    let mut grouped = String::with_capacity(chars.len() + chars.len() / 3);

    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && (chars.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*ch);
    }

    grouped
}

fn align_decimal_column(values: &[String]) -> Vec<String> {
    let mut max_integer_width = 0usize;
    let mut max_fractional_width = 0usize;

    for value in values {
        if let Some((integer, fractional)) = value.split_once('.') {
            max_integer_width = max_integer_width.max(integer.len());
            max_fractional_width = max_fractional_width.max(fractional.len());
        } else {
            max_integer_width = max_integer_width.max(value.len());
        }
    }

    values
        .iter()
        .map(|value| {
            if let Some((integer, fractional)) = value.split_once('.') {
                format!("{integer:>max_integer_width$}.{fractional:<max_fractional_width$}")
            } else if max_fractional_width > 0 {
                format!(
                    "{value:>max_integer_width$} {padding}",
                    padding = " ".repeat(max_fractional_width)
                )
            } else {
                format!("{value:>max_integer_width$}")
            }
        })
        .collect()
}

fn format_optional_currency(value: Option<f64>) -> String {
    value.map(format_currency).unwrap_or_else(|| "N/A".to_string())
}

fn format_optional_percentage(value: Option<f64>) -> String {
    value.map(format_percentage_change).unwrap_or_else(|| "N/A".to_string())
}

fn format_optional_signed_currency(value: Option<f64>) -> String {
    value.map(format_signed_currency).unwrap_or_else(|| "N/A".to_string())
}

fn format_optional_raw_number(value: Option<f64>) -> String {
    value.map(|number| number.to_string()).unwrap_or_default()
}

fn sum_optional_values(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total = 0.0;
    let mut has_value = false;

    for value in values.flatten() {
        total += value;
        has_value = true;
    }

    has_value.then_some(total)
}

fn calculate_total_percentage_change(total_equity: Option<f64>, total_todays_return: Option<f64>) -> Option<f64> {
    let (Some(total_equity), Some(total_todays_return)) = (total_equity, total_todays_return) else {
        return None;
    };

    let previous_close_equity = total_equity - total_todays_return;
    if previous_close_equity.abs() <= f64::EPSILON {
        return None;
    }

    Some((total_todays_return / previous_close_equity) * 100.0)
}

fn format_currency(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let absolute = value.abs();
    let text = format!("{absolute:.2}");
    let (integer, fractional) = text.split_once('.').unwrap_or((&text, "00"));
    let grouped = format_integer_with_grouping(integer);
    format!("{sign}${grouped}.{fractional}")
}

fn format_signed_currency(value: f64) -> String {
    let sign = if value < 0.0 {
        "-"
    } else if value > 0.0 {
        "+"
    } else {
        ""
    };
    let absolute = value.abs();
    let text = format!("{absolute:.2}");
    let (integer, fractional) = text.split_once('.').unwrap_or((&text, "00"));
    let grouped = format_integer_with_grouping(integer);
    format!("{sign}${grouped}.{fractional}")
}

fn format_percentage_change(value: f64) -> String {
    if value > 0.0 {
        format!("+{value:.2}%")
    } else {
        format!("{value:.2}%")
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn color_for_change(value: Option<f64>) -> Color {
    match value {
        Some(change) if change > 0.0 => Color::Green,
        Some(change) if change < 0.0 => Color::Red,
        _ => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use console::strip_ansi_codes;

    use broker_robinhood::client::RobinhoodAccount;

    use super::{
        PositionRow, QuoteSnapshot, align_decimal_column, calculate_position_metrics,
        calculate_total_percentage_change, format_currency, format_percentage_change, format_quantity,
        format_signed_currency, render_positions_csv, render_positions_table, select_accounts,
    };

    #[test]
    fn format_quantity_groups_and_trims_precision() {
        assert_eq!(format_quantity(1500.0), "1,500");
        assert_eq!(format_quantity(1618.57743), "1,618.57743");
        assert_eq!(format_quantity(-10000.5), "-10,000.5");
    }

    #[test]
    fn format_quantity_handles_zero() {
        assert_eq!(format_quantity(0.0), "0");
    }

    #[test]
    fn align_decimal_column_aligns_quantities_for_readability() {
        let aligned = align_decimal_column(&["1,500".to_string(), "12.5".to_string(), "1,618.57743".to_string()]);

        assert_eq!(aligned[0], "1,500      ");
        assert_eq!(aligned[1], "   12.5    ");
        assert_eq!(aligned[2], "1,618.57743");
    }

    #[test]
    fn calculate_position_metrics_uses_quote_snapshot() {
        let quote = QuoteSnapshot {
            last_trade_price: Some(110.0),
            previous_close: Some(100.0),
        };

        let (equity, percentage_change, todays_return) = calculate_position_metrics(2.0, Some(&quote));

        assert_eq!(equity, Some(220.0));
        assert_eq!(percentage_change, Some(10.0));
        assert_eq!(todays_return, Some(20.0));
    }

    #[test]
    fn calculate_total_percentage_change_uses_total_return_and_previous_close_equity() {
        let total_percentage_change = calculate_total_percentage_change(Some(3_500.0), Some(5.0)).expect("has totals");

        assert!((total_percentage_change - 0.1430615164520744).abs() < 1e-12);
    }

    #[test]
    fn render_positions_table_hides_account_for_single_account() {
        let rows = vec![
            PositionRow {
                account_number: "116748102690".to_string(),
                symbol: "AMZN".to_string(),
                quantity: 1_618.57743,
                equity: Some(300_000.12),
                percentage_change: Some(2.5),
                todays_return: Some(120.45),
            },
            PositionRow {
                account_number: "116748102690".to_string(),
                symbol: "V".to_string(),
                quantity: 1_500.0,
                equity: Some(150_000.0),
                percentage_change: Some(-1.2),
                todays_return: Some(-30.0),
            },
        ];

        let table = strip_ansi_codes(&render_positions_table(&rows)).to_string();

        assert!(!table.contains("Account"));
        assert!(table.contains("Symbol"));
        assert!(table.contains("Quantity"));
        assert!(table.contains("Equity"));
        assert!(table.contains("% Change"));
        assert!(table.contains("Today's Return"));
        assert!(table.contains("AMZN"));
        assert!(table.contains("1,618.57743"));
        assert!(table.contains("$300,000.12"));
        assert!(table.contains("+2.50%"));
        assert!(table.contains("+$120.45"));
        assert!(table.contains("-1.20%"));
        assert!(table.contains("-$30.00"));
        assert!(table.contains("V"));
        assert!(table.contains("1,500"));
        assert!(table.contains("Total"));
        assert!(table.contains("$450,000.12"));
        assert!(table.contains("+0.02%"));
        assert!(table.contains("+$90.45"));
    }

    #[test]
    fn render_positions_table_shows_account_for_multiple_accounts() {
        let rows = vec![
            PositionRow {
                account_number: "116748102690".to_string(),
                symbol: "AMZN".to_string(),
                quantity: 10.0,
                equity: Some(1500.0),
                percentage_change: Some(1.0),
                todays_return: Some(15.0),
            },
            PositionRow {
                account_number: "5QT29231".to_string(),
                symbol: "V".to_string(),
                quantity: 20.0,
                equity: Some(2000.0),
                percentage_change: Some(-0.5),
                todays_return: Some(-10.0),
            },
        ];

        let table = strip_ansi_codes(&render_positions_table(&rows)).to_string();

        assert!(table.contains("Account"));
        assert!(table.contains("116748102690"));
        assert!(table.contains("5QT29231"));
        assert!(table.contains("Total"));
        assert!(table.contains("$3,500.00"));
        assert!(table.contains("+0.14%"));
        assert!(table.contains("+$5.00"));
    }

    #[test]
    fn render_positions_csv_outputs_raw_values() {
        let rows = vec![
            PositionRow {
                account_number: "116748102690".to_string(),
                symbol: "AMZN".to_string(),
                quantity: 1618.57743,
                equity: Some(300000.12),
                percentage_change: Some(2.5),
                todays_return: Some(120.45),
            },
            PositionRow {
                account_number: "5QT29231".to_string(),
                symbol: "V".to_string(),
                quantity: 1500.0,
                equity: Some(150000.0),
                percentage_change: Some(-1.2),
                todays_return: Some(-30.0),
            },
        ];

        let csv = render_positions_csv(&rows);
        let lines = csv.lines().collect::<Vec<_>>();

        assert_eq!(
            lines[0],
            "account,symbol,quantity,equity,percentage_change,todays_return"
        );
        assert_eq!(lines[1], "116748102690,AMZN,1618.57743,300000.12,2.5,120.45");
        assert_eq!(lines[2], "5QT29231,V,1500,150000,-1.2,-30");
    }

    #[test]
    fn formatters_render_expected_output() {
        assert_eq!(format_currency(1234.5), "$1,234.50");
        assert_eq!(format_signed_currency(1234.5), "+$1,234.50");
        assert_eq!(format_signed_currency(-1234.5), "-$1,234.50");
        assert_eq!(format_percentage_change(1.234), "+1.23%");
        assert_eq!(format_percentage_change(-1.234), "-1.23%");
    }

    #[test]
    fn select_accounts_resolves_default_alias() {
        let accounts = vec![
            RobinhoodAccount {
                account_number: "1234".to_string(),
                brokerage_account_type: None,
                is_default: false,
            },
            RobinhoodAccount {
                account_number: "5678".to_string(),
                brokerage_account_type: None,
                is_default: true,
            },
        ];

        let selected = select_accounts(accounts, "default").expect("default account should resolve");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].account_number, "5678");
    }

    #[test]
    fn select_accounts_returns_unknown_account_error() {
        let accounts = vec![RobinhoodAccount {
            account_number: "1234".to_string(),
            brokerage_account_type: None,
            is_default: true,
        }];

        let error = select_accounts(accounts, "9999").expect_err("unknown account should error");
        assert_eq!(error.to_string(), "Robinhood account `9999` not found");
    }
}
