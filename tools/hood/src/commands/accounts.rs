use std::borrow::Cow;

use broker_robinhood::RobinhoodClient;
use broker_robinhood::RobinhoodClientError;
use broker_robinhood::client::RobinhoodAccount;
use console::{set_colors_enabled, style};
use thiserror::Error;

use crate::creds;

type Result<T> = std::result::Result<T, AccountsError>;

#[derive(Debug, Error)]
pub enum AccountsError {
    #[error(transparent)]
    Credentials(#[from] creds::CredentialsError),
    #[error(transparent)]
    RobinhoodClient(#[from] RobinhoodClientError),
}

pub async fn run(username: Option<&str>, _account: &str) -> Result<()> {
    set_colors_enabled(true);

    let (_, access_token) = creds::load_access_token(username)?;

    let client = RobinhoodClient::new()?;
    let accounts = client.fetch_accounts(&access_token).await?;

    if accounts.is_empty() {
        println!("No Robinhood accounts found.");
        return Ok(());
    }

    for line in format_account_lines(&accounts) {
        println!("{line}");
    }

    Ok(())
}

fn format_account_lines(accounts: &[RobinhoodAccount]) -> Vec<String> {
    let account_number_width = accounts
        .iter()
        .map(|account| account.account_number.len())
        .max()
        .unwrap_or(0);
    let account_type_width = accounts
        .iter()
        .map(|account| human_readable_account_type(account.brokerage_account_type.as_deref()).len())
        .max()
        .unwrap_or(0);

    accounts
        .iter()
        .map(|account| format_account_line(account, account_number_width, account_type_width))
        .collect()
}

fn format_account_line(account: &RobinhoodAccount, account_number_width: usize, account_type_width: usize) -> String {
    let account_number = format!("{:<account_number_width$}", account.account_number);
    let account_type = format!(
        "{:<account_type_width$}",
        human_readable_account_type(account.brokerage_account_type.as_deref())
    );
    let suffix = if account.is_default {
        format!("  {}", style("[default]").green().bold())
    } else {
        String::new()
    };

    format!(
        "{}  {}{}",
        style(account_number).cyan().bold(),
        style(account_type).yellow(),
        suffix
    )
}

fn human_readable_account_type(account_type: Option<&str>) -> Cow<'_, str> {
    match account_type {
        Some("individual") => Cow::Borrowed("Individual"),
        Some("ira_traditional") => Cow::Borrowed("Traditional IRA"),
        Some("ira_roth") => Cow::Borrowed("Roth IRA"),
        Some("joint_tennancy_with_ros") | Some("joint_tenancy_with_ros") => Cow::Borrowed("Joint"),
        Some(other) => Cow::Borrowed(other),
        None => Cow::Borrowed("Unknown"),
    }
}

#[cfg(test)]
mod tests {
    use broker_robinhood::client::RobinhoodAccount;
    use console::strip_ansi_codes;

    use super::{format_account_line, format_account_lines};

    #[test]
    fn format_account_line_includes_default_marker_and_readable_type() {
        let account = RobinhoodAccount {
            account_number: "1234".to_string(),
            brokerage_account_type: Some("individual".to_string()),
            is_default: true,
        };

        assert_eq!(
            strip_ansi_codes(&format_account_line(&account, 4, 10)).to_string(),
            "1234  Individual  [default]"
        );
    }

    #[test]
    fn format_account_line_handles_missing_account_type() {
        let account = RobinhoodAccount {
            account_number: "5678".to_string(),
            brokerage_account_type: None,
            is_default: false,
        };

        assert_eq!(
            strip_ansi_codes(&format_account_line(&account, 4, 7))
                .trim_end()
                .to_string(),
            "5678  Unknown"
        );
    }

    #[test]
    fn format_account_lines_aligns_columns_and_maps_joint_aliases() {
        let accounts = vec![
            RobinhoodAccount {
                account_number: "5QT29231".to_string(),
                brokerage_account_type: Some("individual".to_string()),
                is_default: true,
            },
            RobinhoodAccount {
                account_number: "116748102690".to_string(),
                brokerage_account_type: Some("joint_tennancy_with_ros".to_string()),
                is_default: false,
            },
        ];
        let lines = format_account_lines(&accounts)
            .into_iter()
            .map(|line| strip_ansi_codes(&line).to_string())
            .collect::<Vec<_>>();

        assert_eq!(lines[0], "5QT29231      Individual  [default]");
        assert_eq!(lines[1].trim_end(), "116748102690  Joint");
    }
}
