//! Error type for the Rust muse-cli (writer A: errors.rs).
//!
//! Exact shape from CONTRACT.md. `main.rs` maps these to stderr + exit
//! code; this module must not decide exit codes itself.
//!
//! Mapping (in `main.rs`, not here): Auth -> exit 2, Gateway -> exit 3,
//! Value -> exit 2, Timeout -> exit 4, Io -> exit 2.
//!
//! No `unwrap()` / `expect()` in production code in this file.

use thiserror::Error;

/// Errors that cross the command boundary.
#[derive(Debug, Error)]
pub enum CliError {
    /// Authentication / login problem (missing, empty, or unparseable
    /// cookies; expired session). Displayed by main as `auth error: …`.
    #[error("{0}")]
    Auth(String),

    /// Gateway (or other HTTP API) failure with a numeric status and a
    /// payload string. `status == -1` is the original's sentinel for
    /// "no HTTP status" (e.g. stream reset, non-JSON response).
    #[error("gateway status={status} payload={payload:?}")]
    Gateway { status: i64, payload: String },

    /// Bad value: bad flag value, bad config value, bad path, bad
    /// user input. This is the Rust form of Python `ValueError`.
    #[error("{0}")]
    Value(String),

    /// A deadline expired at the gateway layer. This is NOT the same as
    /// a wait/send turn timeout (exit 124), which `wait.rs` reports as
    /// a successful `WaitResult`, not as this error.
    #[error("{0}")]
    Timeout(String),

    /// Local I/O failure (reading/writing a file, spawning a helper).
    #[error("{0}")]
    Io(String),
}

impl CliError {
    /// `Auth` helper (CONTRACT).
    pub fn auth(msg: impl Into<String>) -> Self {
        CliError::Auth(msg.into())
    }

    /// `Gateway` helper (CONTRACT).
    pub fn gateway(status: i64, payload: impl Into<String>) -> Self {
        CliError::Gateway {
            status,
            payload: payload.into(),
        }
    }

    /// `Value` helper (CONTRACT).
    pub fn value(msg: impl Into<String>) -> Self {
        CliError::Value(msg.into())
    }

    /// `Timeout` helper (CONTRACT).
    pub fn timeout(msg: impl Into<String>) -> Self {
        CliError::Timeout(msg.into())
    }

    /// `Io` helper (CONTRACT).
    pub fn io(msg: impl Into<String>) -> Self {
        CliError::Io(msg.into())
    }
}

impl From<std::io::Error> for CliError {
    fn from(err: std::io::Error) -> Self {
        CliError::Io(err.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(err: serde_json::Error) -> Self {
        CliError::Value(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_build_the_right_variants() {
        assert!(matches!(CliError::auth("x"), CliError::Auth(_)));
        assert!(matches!(
            CliError::gateway(3, "p"),
            CliError::Gateway { status: 3, .. }
        ));
        assert!(matches!(CliError::value("x"), CliError::Value(_)));
        assert!(matches!(CliError::timeout("x"), CliError::Timeout(_)));
        assert!(matches!(CliError::io("x"), CliError::Io(_)));
    }

    #[test]
    fn gateway_display_matches_the_python_str_shape() {
        let err = CliError::gateway(-1, "stream reset 2: gone");
        assert_eq!(
            err.to_string(),
            "gateway status=-1 payload=\"stream reset 2: gone\""
        );
    }

    #[test]
    fn io_error_converts_to_io_variant() {
        let err = CliError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert!(matches!(err, CliError::Io(_)));
    }
}
