//! `boss-event` — a thin stdin-to-Unix-socket shim invoked by claude
//! hooks running inside a Boss-managed worker.
//!
//! Each claude hook is configured (via the engine's per-lease
//! `settings.json` template) to spawn this binary, with the hook
//! payload arriving on stdin. The shim reads stdin to EOF, opens the
//! engine's events socket at `$BOSS_EVENTS_SOCKET`, writes the
//! payload, and exits.
//!
//! The shim is intentionally minimal: no parsing, no retries, no
//! framing. The engine derives the worker's lease via `LOCAL_PEERPID`
//! on its side, so the shim doesn't need to embed the lease id, only
//! the raw hook JSON.
//!
//! Hooks fire on the worker's hot path; staying small and synchronous
//! keeps the per-hook overhead trivial. If the socket isn't reachable
//! (engine restarting, upgrading) we fail loudly with a non-zero exit
//! code so claude logs the hook failure rather than silently dropping
//! events.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};

const SOCKET_ENV: &str = "BOSS_EVENTS_SOCKET";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("boss-event: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let socket_path = std::env::var(SOCKET_ENV)
        .map_err(|_| anyhow!("{SOCKET_ENV} not set; refusing to deliver hook event"))?;

    let mut payload = Vec::new();
    io::stdin()
        .read_to_end(&mut payload)
        .context("reading hook payload from stdin")?;

    if payload.is_empty() {
        return Err(anyhow!("hook payload on stdin was empty"));
    }

    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("connecting to events socket at {socket_path}"))?;

    stream
        .write_all(&payload)
        .context("writing hook payload to events socket")?;

    // Half-close on our end signals end-of-message to the engine.
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutting down write half of events socket")?;

    Ok(())
}
