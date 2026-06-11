use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod commands;
mod creds;

#[derive(Debug, Parser)]
#[command(name = "hood", about = "Robinhood CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Args)]
struct CommonFlags {
    /// Robinhood username to use. Uses the most recently authenticated user when omitted.
    #[arg(long, short = 'u')]
    username: Option<String>,

    /// Robinhood account number to use. `default` is an alias for the default account.
    #[arg(long, default_value = "default")]
    account: String,
}

#[derive(Debug, Clone, Args)]
struct PositionsFlags {
    #[command(flatten)]
    common: CommonFlags,

    /// Refresh the positions table every 10 seconds.
    #[arg(long, short = 'f')]
    follow: bool,

    /// Output raw position data as comma-separated values.
    #[arg(long, conflicts_with = "follow")]
    csv: bool,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Authenticate with Robinhood and store OAuth credentials in the system keychain.
    Auth {
        /// Print extra diagnostics with sensitive fields redacted.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// List Robinhood accounts for an authenticated user.
    Accounts(CommonFlags),
    /// Verify stored credentials and connectivity to Robinhood APIs.
    Status(CommonFlags),
    /// List open positions for a Robinhood account.
    Positions(PositionsFlags),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Auth { verbose } => commands::auth::run(verbose).await?,
        Command::Accounts(common) => commands::accounts::run(common.username.as_deref(), &common.account).await?,
        Command::Status(common) => commands::status::run(common.username.as_deref(), &common.account).await?,
        Command::Positions(positions) => {
            commands::positions::run(
                positions.common.username.as_deref(),
                &positions.common.account,
                positions.follow,
                positions.csv,
            )
            .await?
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn accounts_defaults_account_to_default_alias() {
        let cli = Cli::parse_from(["hood", "accounts"]);

        match cli.command {
            Command::Accounts(common) => {
                assert_eq!(common.account, "default");
                assert_eq!(common.username, None);
            }
            _ => panic!("expected accounts command"),
        }
    }

    #[test]
    fn status_allows_overriding_common_flags() {
        let cli = Cli::parse_from(["hood", "status", "--username", "alice", "--account", "12345678"]);

        match cli.command {
            Command::Status(common) => {
                assert_eq!(common.username.as_deref(), Some("alice"));
                assert_eq!(common.account, "12345678");
            }
            _ => panic!("expected status command"),
        }
    }

    #[test]
    fn positions_defaults_account_to_default_alias() {
        let cli = Cli::parse_from(["hood", "positions"]);

        match cli.command {
            Command::Positions(positions) => {
                assert_eq!(positions.common.account, "default");
                assert_eq!(positions.common.username, None);
                assert!(!positions.follow);
                assert!(!positions.csv);
            }
            _ => panic!("expected positions command"),
        }
    }

    #[test]
    fn positions_supports_csv_flag() {
        let cli = Cli::parse_from([
            "hood",
            "positions",
            "--username",
            "alice",
            "--account",
            "12345678",
            "--csv",
        ]);

        match cli.command {
            Command::Positions(positions) => {
                assert_eq!(positions.common.username.as_deref(), Some("alice"));
                assert_eq!(positions.common.account, "12345678");
                assert!(positions.csv);
            }
            _ => panic!("expected positions command"),
        }
    }

    #[test]
    fn positions_accepts_follow_flag() {
        let cli = Cli::parse_from(["hood", "positions", "-f"]);

        match cli.command {
            Command::Positions(positions) => assert!(positions.follow),
            _ => panic!("expected positions command"),
        }
    }
}
