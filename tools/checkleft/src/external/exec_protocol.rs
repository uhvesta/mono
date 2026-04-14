use std::io::{Read, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::input::ChangeSet;
use crate::output::Finding;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecCheckRequest {
    pub changeset: ChangeSet,
    pub config: toml::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecCheckResponse {
    pub findings: Vec<Finding>,
}

pub fn read_exec_check_request_from_stdin() -> Result<ExecCheckRequest> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("failed reading exec check request from stdin")?;
    serde_json::from_slice(&bytes).context("stdin did not contain valid exec check JSON")
}

pub fn write_exec_check_response_to_stdout(response: &ExecCheckResponse) -> Result<()> {
    let bytes =
        serde_json::to_vec(response).context("failed serializing exec check response as JSON")?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(&bytes)
        .context("failed writing exec check response to stdout")?;
    stdout
        .flush()
        .context("failed flushing exec check stdout")?;
    Ok(())
}

pub fn write_exec_findings_to_stdout(findings: Vec<Finding>) -> Result<()> {
    write_exec_check_response_to_stdout(&ExecCheckResponse { findings })
}
