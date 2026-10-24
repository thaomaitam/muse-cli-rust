//! Binary entry point for `muse-cli` (Rust rewrite).
//!
//! Coordinator-owned file (see CONTRACT.md). Behaviour contract:
//! - `--version` prints `muse-cli 0.3.2` (clap, bin name `muse-cli`).
//! - No subcommand means `chat` (same as the Python original).
//! - Error mapping: Auth -> stderr `auth error: …`, exit 2;
//!   Gateway -> stderr `gateway error: …`, exit 3;
//!   Value -> stderr `error: …`, exit 2;
//!   Timeout -> stderr `timeout: …`, exit 4.
//!   Command handlers that need their own exit code (wait/send/download/…)
//!   return it as `Ok(code)`; the main function exits with that code.
//! - The background update-check notice (see update.rs) is started for
//!   every command except `update`, and only prints when both stdout and
//!   stderr are terminals and neither MUSE_NO_UPDATE_CHECK nor CI is set.
//!   It never fails the command and never upgrades anything by itself.

use muse_cli::cli::{parse_cli, Command};
use muse_cli::commands;
use muse_cli::errors::CliError;
use muse_cli::update;

fn main() {
    // Broken pipe, system behaviour: Rust's runtime ignores SIGPIPE on
    // startup, so a `println!` to a closed pipe panics (exit 101). A
    // Unix CLI is expected to die by SIGPIPE instead (shell reports
    // 141, like `head` pipelines), matching the system convention
    // rather than the Python original's traceback (exit 1). Restoring
    // the default disposition is the one `unsafe` block in production
    // code; it touches no data, only the process signal disposition.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // Live HTTPS (reqwest/tungstenite via rustls) panics with exit 101 if
    // no process-level CryptoProvider is installed: rustls 0.23 does not
    // pick one automatically from crate features in this build. Install
    // the ring provider once, before any network use. Ignoring the
    // error is correct: it only means a provider was already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = parse_cli();
    let is_update = matches!(cli.command, Some(Command::Update));
    let notice = if is_update {
        None
    } else {
        update::start_update_check()
    };
    let result = run(cli);
    update::finish_update_check(notice);
    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            let (message, code) = match &err {
                CliError::Auth(msg) => (format!("auth error: {msg}"), 2),
                CliError::Gateway { status, payload } => (
                    format!("gateway error: gateway status={status} payload={payload:?}"),
                    3,
                ),
                CliError::Value(msg) => (format!("error: {msg}"), 2),
                CliError::Timeout(msg) => (format!("timeout: {msg}"), 4),
                CliError::Io(msg) => (format!("error: {msg}"), 2),
            };
            eprintln!("{message}");
            std::process::exit(code);
        }
    }
}

fn run(cli: muse_cli::cli::Cli) -> Result<i32, CliError> {
    commands::dispatch(cli)
}
