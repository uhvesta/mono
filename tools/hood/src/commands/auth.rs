use std::io::{self, Write};
use std::time::Duration;

use broker_robinhood::{AuthChallenge, RobinhoodClient, RobinhoodClientError, WorkflowRoute, WorkflowScreen};
use indicatif::{ProgressBar, ProgressStyle};
use rpassword::prompt_password;
use serde_json::Value;
use thiserror::Error;
use tokio::time::sleep;

use crate::creds;

const MAX_PUSH_ATTEMPTS: usize = 30;
const PUSH_POLL_DELAY: Duration = Duration::from_secs(2);
const PROGRESS_TICK_INTERVAL_MS: u64 = 120;
const REDACTED: &str = "<redacted>";

type Result<T> = std::result::Result<T, AuthError>;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("stdin closed while reading username")]
    ClosedStdin,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    RobinhoodClient(#[from] RobinhoodClientError),
    #[error(transparent)]
    Credentials(#[from] creds::CredentialsError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    ProgressStyle(#[from] indicatif::style::TemplateError),
}

pub async fn run(verbose: bool) -> Result<()> {
    let (username, password) = prompt_for_credentials()?;
    authenticate(&username, &password, verbose).await
}

fn prompt_for_credentials() -> Result<(String, String)> {
    let username = prompt_non_empty("Username: ")?;
    let password = prompt_non_empty_password("Password: ")?;
    Ok((username, password))
}

fn prompt_non_empty(prompt: &str) -> Result<String> {
    let mut input = String::new();
    loop {
        print!("{prompt}");
        io::stdout().flush()?;

        input.clear();
        let bytes_read = io::stdin().read_line(&mut input)?;
        if bytes_read == 0 {
            return Err(AuthError::ClosedStdin);
        }

        let value = input.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }

        eprintln!("username cannot be empty");
    }
}

fn prompt_non_empty_password(prompt: &str) -> Result<String> {
    loop {
        let password = prompt_password(prompt)?;
        if !password.trim().is_empty() {
            return Ok(password);
        }

        eprintln!("password cannot be empty");
    }
}

async fn authenticate(username: &str, password: &str, verbose: bool) -> Result<()> {
    let progress = new_spinner()?;
    progress.set_message("Connecting to Robinhood");

    let client = RobinhoodClient::new()?;

    progress.set_message("Initiating login");
    let challenge = client.initiate_login(username, password).await?;
    log_challenge(verbose, &challenge);

    let workflow_id = challenge.verification_workflow().id.clone();
    let device_token = challenge.device_token();
    let request_id = challenge.request_id();

    progress.set_message("Checking verification workflow");
    let verification_result = client.fetch_verification_result(&workflow_id).await?;
    vprintln(verbose, format_args!("Verification result: {verification_result}"));

    progress.set_message("Requesting device approval challenge");
    let route = client.advance_workflow_entry_point(&workflow_id).await?;
    log_route(verbose, &route);

    let challenge_id = extract_challenge_id(&route);
    let status = wait_for_push_validation(&client, challenge_id.as_deref(), &progress, verbose).await?;

    match status.as_deref() {
        Some("validated") => {}
        Some("expired") => {
            progress.abandon_with_message("Push challenge expired; rerun `hood auth` to retry.");
            return Ok(());
        }
        Some(other) => {
            progress.abandon_with_message(format!(
                "Push challenge ended with status `{other}`; rerun `hood auth`."
            ));
            return Ok(());
        }
        None => {
            progress.abandon_with_message("No push challenge was available; rerun `hood auth` to retry.");
            return Ok(());
        }
    }

    progress.set_message("Completing device approval");
    complete_device_approval(&client, &workflow_id, verbose).await?;

    progress.set_message("Finalizing login");
    let token = client
        .finalize_login(username, password, &device_token, &request_id)
        .await?;

    vprintln(
        verbose,
        format_args!(
            "Final OAuth token response (redacted):\n{}",
            serde_json::to_string_pretty(&redact_sensitive_value(token.clone()))?
        ),
    );

    progress.set_message("Saving credentials securely");
    creds::store_credentials(username, &token)?;

    progress.finish_with_message("Authenticated. Credentials stored in system keychain.");
    Ok(())
}

fn new_spinner() -> Result<ProgressBar> {
    let progress = ProgressBar::new_spinner();
    progress.set_style(ProgressStyle::with_template("{spinner:.green} {msg}")?);
    progress.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_INTERVAL_MS));
    Ok(progress)
}

fn extract_challenge_id(route: &WorkflowRoute) -> Option<String> {
    route
        .replace
        .as_ref()
        .and_then(|replace| replace.screen.device_approval_challenge_screen_params.as_ref())
        .and_then(|params| params.sheriff_challenge.as_ref())
        .and_then(|challenge| challenge.id.clone())
}

fn log_challenge(verbose: bool, challenge: &AuthChallenge) {
    if !verbose {
        return;
    }

    eprintln!("Verification workflow ID: {}", challenge.verification_workflow().id);
    eprintln!("Workflow status: {}", challenge.verification_workflow().workflow_status);
    eprintln!("Device token: {}", redact_string(&challenge.device_token().to_string()));
    eprintln!("Request ID: {}", redact_string(&challenge.request_id().to_string()));
}

fn log_route(verbose: bool, route: &WorkflowRoute) {
    if !verbose {
        return;
    }

    if let Some(replace) = &route.replace {
        eprintln!("Route action: replace");
        log_screen(&replace.screen);
    } else {
        eprintln!("Route action: none");
    }
}

fn log_screen(screen: &WorkflowScreen) {
    eprintln!("Screen name: {}", screen.name);
    if let Some(block_id) = &screen.block_id {
        eprintln!("Screen block ID: {block_id}");
    }

    if let Some(params) = &screen.device_approval_challenge_screen_params {
        eprintln!(
            "Device approval challenge flow ID: {}",
            params.sheriff_flow_id.as_deref().unwrap_or("<unknown>")
        );

        if let Some(challenge) = &params.sheriff_challenge {
            if let Some(id) = &challenge.id {
                eprintln!("Challenge ID: {}", redact_string(id));
            }
            if let Some(challenge_type) = &challenge.challenge_type {
                eprintln!("Challenge type: {challenge_type}");
            }
            if let Some(status) = &challenge.status {
                eprintln!("Challenge status: {status}");
            }
            if let Some(retries) = challenge.remaining_retries {
                eprintln!("Remaining retries: {retries}");
            }
            if let Some(attempts) = challenge.remaining_attempts {
                eprintln!("Remaining attempts: {attempts}");
            }
            if let Some(expires_at) = &challenge.expires_at {
                eprintln!("Challenge expires at: {expires_at}");
            }
        }

        if let Some(fallback) = &params.fallback_cta_text {
            eprintln!("Fallback CTA text: {fallback}");
        }
    }
}

async fn wait_for_push_validation(
    client: &RobinhoodClient,
    challenge_id: Option<&str>,
    progress: &ProgressBar,
    verbose: bool,
) -> Result<Option<String>> {
    let Some(challenge_id) = challenge_id else {
        vprintln(verbose, format_args!("No sheriff challenge to poll."));
        return Ok(None);
    };

    let mut last_status: Option<String> = None;

    for attempt in 1..=MAX_PUSH_ATTEMPTS {
        progress.set_message(format!("Waiting for push approval ({attempt}/{MAX_PUSH_ATTEMPTS})"));
        let status = client.fetch_push_prompt_status(challenge_id).await?;

        vprintln(
            verbose,
            format_args!("Push challenge status (attempt {attempt}/{MAX_PUSH_ATTEMPTS}): {status}"),
        );

        match status.as_str() {
            "validated" | "expired" => return Ok(Some(status)),
            _ => {
                last_status = Some(status);
                sleep(PUSH_POLL_DELAY).await;
            }
        }
    }

    vprintln(
        verbose,
        format_args!(
            "Push challenge did not validate within allotted attempts (last status: {}).",
            last_status.as_deref().unwrap_or("<unknown>")
        ),
    );
    Ok(last_status)
}

async fn complete_device_approval(client: &RobinhoodClient, workflow_id: &str, verbose: bool) -> Result<()> {
    let route = client.complete_device_approval(workflow_id).await?;

    if verbose {
        if let Some(exit) = route.exit {
            eprintln!("Workflow exit status: {}", exit.status);
        } else {
            eprintln!("Workflow completed without exit status.");
        }
    }

    Ok(())
}

fn redact_sensitive_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    if is_sensitive_key(&key) {
                        (key, Value::String(REDACTED.to_string()))
                    } else {
                        (key, redact_sensitive_value(value))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_sensitive_value).collect()),
        other => other,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "password", "secret", "credential", "authorization", "auth"]
        .iter()
        .any(|needle| key.contains(needle))
}

fn redact_string(value: &str) -> String {
    let visible = 4;
    if value.len() <= visible * 2 {
        REDACTED.to_string()
    } else {
        format!(
            "{}...{}",
            &value[..visible],
            &value[value.len().saturating_sub(visible)..]
        )
    }
}

fn vprintln(verbose: bool, args: std::fmt::Arguments<'_>) {
    if verbose {
        eprintln!("{args}");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{REDACTED, redact_sensitive_value};

    #[test]
    fn redact_sensitive_fields_in_verbose_output() {
        let input = json!({
            "access_token": "secret-access-token",
            "refresh_token": "secret-refresh-token",
            "token_type": "Bearer",
            "profile": {
                "email": "person@example.com"
            }
        });

        let redacted = redact_sensitive_value(input);

        assert_eq!(redacted["access_token"], REDACTED);
        assert_eq!(redacted["refresh_token"], REDACTED);
        assert_eq!(redacted["token_type"], REDACTED);
        assert_eq!(redacted["profile"]["email"], "person@example.com");
    }
}
