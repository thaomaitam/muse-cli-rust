//! Background update check + CHECK-ONLY `update` command
//! (writer A: update.rs).
//!
//! Mirrors `update.py:start_update_check` / `finish_update_check` and
//! the update-check state file, with one CONTRACT-mandated change:
//! the `update` COMMAND itself is CHECK-ONLY in the Rust build. It
//! reports the latest version when it can be fetched and prints the
//! upgrade instruction for the detected install type (git checkout /
//! uv / pipx / pip, same detection order as the original's
//! `upgrade_argv`), but it NEVER downloads, installs, or overwrites
//! any binary. The command says so itself in its output, so this is a
//! documented partial implementation, not a silent behaviour change.
//!
//! Update-check rules kept verbatim:
//! - Only when BOTH stdout and stderr are ttys, and neither
//!   `MUSE_NO_UPDATE_CHECK` nor `CI` is set to a non-empty value
//!   (an empty value counts as unset, like Python's falsy
//!   `os.environ.get()`; `"0"` still counts as set).
//! - State file `~/.config/muse-cli/update.json`
//!   (`{"checked_at", "latest"}`); a cached `latest` younger than
//!   24h is returned immediately with no fetch.
//! - Otherwise the fetch runs in a background thread (HTTP timeout
//!   2s, `User-Agent: muse-cli/<version>`, raw `__init__.py` on the
//!   fork's `main`); success saves `{checked_at: now, latest}`;
//!   failure saves `{checked_at: now - 24h + 1h, latest: <old>}` (try
//!   again in 1h, keep the old version).
//! - `finish_update_check` waits at most 1.5s for the thread; if the
//!   latest version is newer (numeric `x.y.z` only — pre-release or
//!   junk counts as absent) it prints the notice to stderr.
//! - The check never fails the command and never upgrades anything.
//!
//! `main.rs` contract: `start_update_check()` returns
//! `Option<UpdateNotice>` (`None` = notice off, including for the
//! `update` command itself, which `main.rs` skips), and
//! `finish_update_check(notice)` takes that same `Option` by value.
//! The `thread.join(timeout)` of Python is implemented with an
//! `mpsc` channel + `recv_timeout`, because `JoinHandle` has no
//! timed join; a timed-out thread keeps running detached (like a
//! Python daemon thread) and still saves its state file.

use crate::config::default_config_dir;
use crate::errors::CliError;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Seconds between update checks (Python `CHECK_INTERVAL`).
pub const CHECK_INTERVAL_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Seconds before retrying after a failed check (Python
/// `FAIL_BACKOFF`).
pub const FAIL_BACKOFF_SECS: f64 = 60.0 * 60.0;

/// How long `finish_update_check` waits for the background thread
/// (Python joins with `timeout=1.5`).
pub const JOIN_TIMEOUT: Duration = Duration::from_millis(1500);

/// Current version of this build (single-sourced from Cargo.toml,
/// which is `0.3.2`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fork whose `main` branch carries the version file.
pub const FORK: &str = "thoitiettxl-cyber/muse-cli";

/// Install spec the original's upgrade commands use.
pub const INSTALL_SPEC: &str = "git+https://github.com/thoitiettxl-cyber/muse-cli.git";

/// Raw URL of the version file (Python `VERSION_URL`).
pub const VERSION_URL: &str =
    "https://raw.githubusercontent.com/thoitiettxl-cyber/muse-cli/main/src/muse_cli/__init__.py";

/// Pending update-check state, as returned by [`start_update_check`].
pub struct UpdateNotice {
    /// Cached latest version (only set when the cache was fresh).
    latest: Option<String>,
    /// Receiver for the background fetch result (only set when a
    /// fetch was started). `None` on a cache hit.
    rx: Option<Receiver<Option<String>>>,
}

/// Whether an env var counts as "set" for the update-check gate:
/// any non-empty value counts (including `"0"`); an empty value
/// counts as unset, because Python's `os.environ.get()` is falsy
/// for `""` (fix round 2, TECHNICAL F03).
pub fn env_flag_set(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Pure gate for the update check: all four conditions must hold.
/// `no_update_check_set` / `ci_set` mean "the env var is set to a
/// non-empty value" (empty counts as unset, `"0"` counts as set).
pub fn should_update_check(
    no_update_check_set: bool,
    ci_set: bool,
    stdout_is_tty: bool,
    stderr_is_tty: bool,
) -> bool {
    !no_update_check_set && !ci_set && stdout_is_tty && stderr_is_tty
}

/// Stable `x.y.z` as a tuple, mirroring `update.py:version_key`:
/// leading `v`s stripped, `+build` suffix dropped, any alphabetic
/// character or unparseable part -> `None` (pre-release/junk counts
/// as absent). Tuple comparison in Python makes a shorter prefix
/// smaller (`(1, 2) < (1, 2, 0)`); `Vec<u64>` ordering matches.
pub fn version_key(value: &str) -> Option<Vec<u64>> {
    let text = value.trim().trim_start_matches('v');
    let text = text.split('+').next().unwrap_or("");
    if text.is_empty() || text.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    let mut out = Vec::new();
    for part in text.split('.') {
        match part.parse::<u64>() {
            Ok(n) => out.push(n),
            Err(_) => return None,
        }
    }
    Some(out)
}

/// Whether `latest` is newer than `current` (both must parse).
pub fn is_newer(latest: Option<&str>, current: &str) -> bool {
    match (latest.and_then(version_key), version_key(current)) {
        (Some(new), Some(old)) => new > old,
        _ => false,
    }
}

/// Parse `__version__ = "x.y.z"` out of a file body, mirroring
/// `update.py:parse_version` (multiline regex
/// `^__version__\s*=\s*["']([^"']+)["']`). No regex crate is used;
/// the line scan below implements the same pattern.
pub fn parse_version(text: &str) -> Result<String, CliError> {
    for line in text.lines() {
        let rest = match line.strip_prefix("__version__") {
            Some(rest) => rest,
            None => continue,
        };
        let rest = rest.trim_start();
        let rest = match rest.strip_prefix('=') {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        let (quote, rest) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => (q, &rest[q.len_utf8()..]),
            _ => continue,
        };
        match rest.find(quote) {
            Some(end) if end > 0 => return Ok(rest[..end].to_string()),
            _ => continue,
        }
    }
    Err(CliError::value("no __version__ found"))
}

/// Checked-at value to store after a failed fetch, so the next check
/// happens after the 1h backoff (Python: `now - CHECK_INTERVAL +
/// FAIL_BACKOFF`, keeping the old version).
pub fn fail_backoff_checked_at(now: f64) -> f64 {
    now - CHECK_INTERVAL_SECS + FAIL_BACKOFF_SECS
}

/// Whether a cached `checked_at` is still fresh at `now`.
pub fn cache_is_fresh(checked_at: f64, now: f64) -> bool {
    now - checked_at < CHECK_INTERVAL_SECS
}

struct UpdateState {
    checked_at: f64,
    latest: Option<String>,
}

fn update_state_file() -> PathBuf {
    default_config_dir().join("update.json")
}

fn load_update_state() -> UpdateState {
    let file = update_state_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(_) => {
            return UpdateState {
                checked_at: 0.0,
                latest: None,
            }
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return UpdateState {
                checked_at: 0.0,
                latest: None,
            }
        }
    };
    let map = match parsed.as_object() {
        Some(map) => map,
        None => {
            return UpdateState {
                checked_at: 0.0,
                latest: None,
            }
        }
    };
    let checked_at = map
        .get("checked_at")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let latest = match map.get("latest") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    UpdateState { checked_at, latest }
}

fn save_update_state(checked_at: f64, latest: Option<&str>) {
    // Failures are swallowed, like Python's _save_state (OSError: pass).
    let file = update_state_file();
    if let Some(parent) = file.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let mut map = serde_json::Map::new();
    map.insert("checked_at".to_string(), serde_json::json!(checked_at));
    map.insert(
        "latest".to_string(),
        match latest {
            Some(s) => serde_json::Value::String(s.to_string()),
            None => serde_json::Value::Null,
        },
    );
    let text = match serde_json::to_string(&serde_json::Value::Object(map)) {
        Ok(text) => text,
        Err(_) => return,
    };
    let tmp = file.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, &file);
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Fetch the latest version from the fork's `main` (HTTP timeout 2s,
/// `User-Agent: muse-cli/<version>`). A non-2xx status is a failure,
/// like Python `urllib` raising `HTTPError`.
fn fetch_latest_sync() -> Result<String, CliError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .user_agent(format!("muse-cli/{CURRENT_VERSION}"))
        .build()
        .map_err(|e| CliError::io(format!("update check client: {e}")))?;
    let response = client
        .get(VERSION_URL)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| CliError::io(format!("update check fetch: {e}")))?;
    let text = response
        .text()
        .map_err(|e| CliError::io(format!("update check body: {e}")))?;
    parse_version(&text)
}

/// Begin an update check, or return a cached result. `None` when the
/// notice is off (not a tty, env opt-out, or — via `main.rs` — the
/// `update` command itself).
pub fn start_update_check() -> Option<UpdateNotice> {
    let gated = should_update_check(
        env_flag_set(std::env::var_os("MUSE_NO_UPDATE_CHECK").as_deref()),
        env_flag_set(std::env::var_os("CI").as_deref()),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    );
    if !gated {
        return None;
    }
    let state = load_update_state();
    let now = now_secs();
    if let Some(latest) = &state.latest {
        if cache_is_fresh(state.checked_at, now) {
            return Some(UpdateNotice {
                latest: Some(latest.clone()),
                rx: None,
            });
        }
    }
    let old_latest = state.latest.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || match fetch_latest_sync() {
        Ok(found) => {
            save_update_state(now_secs(), Some(&found));
            let _ = tx.send(Some(found));
        }
        Err(_) => {
            save_update_state(fail_backoff_checked_at(now_secs()), old_latest.as_deref());
            let _ = tx.send(old_latest);
        }
    });
    Some(UpdateNotice {
        latest: None,
        rx: Some(rx),
    })
}

/// Finish an update check: wait at most 1.5s, then print the notice
/// to stderr if a newer version is known. Never fails the command.
pub fn finish_update_check(notice: Option<UpdateNotice>) {
    let Some(mut notice) = notice else {
        return;
    };
    let latest = match notice.rx.take() {
        Some(rx) => match rx.recv_timeout(JOIN_TIMEOUT) {
            Ok(latest) => latest,
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        },
        None => notice.latest,
    };
    let Some(latest) = latest else {
        return;
    };
    if is_newer(Some(&latest), CURRENT_VERSION) {
        eprint!(
            "\nA new release of muse-cli is available: {CURRENT_VERSION} → {latest}\nTo upgrade, run: {}\n\n",
            upgrade_command()
        );
    }
}

/// Detected install type for the upgrade instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// A source checkout: `git -C <root> pull --ff-only`.
    Checkout(PathBuf),
    /// A `uv tool` install.
    Uv,
    /// A `pipx` install.
    Pipx,
    /// Anything else: the original's `pip` fallback.
    Pip,
}

/// Detect the install type (pure over its inputs, so it is testable).
///
/// Checkout detection is the Rust analogue of Python's
/// `source_checkout` (which walks from the package file): walk up
/// ONLY from the executable's directory for a directory containing
/// both `.git` and `Cargo.toml`. Fix round 2 removed the old walk
/// from the working directory — a checkout that merely happens to
/// be the cwd is not an install of this binary. (`current_dir` is
/// kept in the signature for caller stability and ignored.)
/// The uv/pipx checks mirror Python's `sys.prefix` checks in shape:
/// a `uv/tools` or `pipx` segment in the executable path, or a
/// receipt file next to the executable.
pub fn install_kind(exe_path: &Path, _current_dir: &Path) -> InstallKind {
    let mut dir = if exe_path.is_file() {
        exe_path.parent().map(Path::to_path_buf)
    } else {
        Some(exe_path.to_path_buf())
    };
    while let Some(candidate) = dir {
        if candidate.join(".git").exists() && candidate.join("Cargo.toml").is_file() {
            return InstallKind::Checkout(candidate);
        }
        dir = candidate.parent().map(Path::to_path_buf);
    }
    let exe_string = exe_path.to_string_lossy();
    let exe_dir = exe_path.parent();
    let has_uv_receipt = exe_dir
        .map(|d| d.join("uv-receipt.toml").is_file())
        .unwrap_or(false);
    if exe_string.contains("/uv/tools/") || has_uv_receipt {
        return InstallKind::Uv;
    }
    let has_pipx_receipt = exe_dir
        .map(|d| d.join("pipx_metadata.json").is_file())
        .unwrap_or(false);
    if exe_string.contains("/pipx/") || has_pipx_receipt {
        return InstallKind::Pipx;
    }
    InstallKind::Pip
}

/// Upgrade argv for an install kind (mirrors Python `upgrade_argv`,
/// except the checkout command names the Rust project root and the
/// pip fallback uses `python3`, since a Rust binary has no
/// `sys.executable`).
pub fn upgrade_argv_for(kind: &InstallKind) -> Vec<String> {
    match kind {
        InstallKind::Checkout(root) => vec![
            "git".to_string(),
            "-C".to_string(),
            root.to_string_lossy().into_owned(),
            "pull".to_string(),
            "--ff-only".to_string(),
        ],
        InstallKind::Uv => vec![
            "uv".to_string(),
            "tool".to_string(),
            "install".to_string(),
            "--upgrade".to_string(),
            INSTALL_SPEC.to_string(),
        ],
        InstallKind::Pipx => vec![
            "pipx".to_string(),
            "install".to_string(),
            "--force".to_string(),
            INSTALL_SPEC.to_string(),
        ],
        InstallKind::Pip => vec![
            "python3".to_string(),
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "-U".to_string(),
            INSTALL_SPEC.to_string(),
        ],
    }
}

/// Upgrade argv for this install (non-pure wrapper around
/// [`install_kind`] + [`upgrade_argv_for`]).
pub fn upgrade_argv() -> Vec<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("muse-cli"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    upgrade_argv_for(&install_kind(&exe, &cwd))
}

/// Upgrade command line for this install (mirrors Python
/// `upgrade_command`: space-joined argv, no quoting — same as the
/// original).
pub fn upgrade_command() -> String {
    upgrade_argv().join(" ")
}

/// Render the CHECK-ONLY `update` report (pure, testable).
///
/// The report always states that this build does not self-upgrade,
/// and always gives the upgrade instruction — that is the whole
/// point of the check-only command.
pub fn update_report(latest: Option<&str>, upgrade_cmd: &str) -> String {
    let mut lines = Vec::new();
    match latest {
        Some(version) => {
            lines.push(format!(
                "Latest release: {version} (installed: {CURRENT_VERSION})"
            ));
            if is_newer(Some(version), CURRENT_VERSION) {
                lines.push(format!("A newer release is available: {version}."));
            } else {
                lines.push("You are on the latest release.".to_string());
            }
        }
        None => {
            lines.push(
                "Could not determine the latest release (offline or unreachable).".to_string(),
            );
        }
    }
    lines.push("This Rust build does not self-upgrade.".to_string());
    lines.push(format!("To upgrade, run: {upgrade_cmd}"));
    lines.join("\n")
}

/// Run the CHECK-ONLY `update` command (writer C's dispatch calls
/// this for `Command::Update`).
///
/// Fetches the latest version synchronously (2s timeout, same as the
/// background check), prints the report to stderr — the original's
/// `cmd_update` also prints to stderr — and returns exit code 0.
/// It never runs the upgrade command, never downloads anything, and
/// never overwrites any binary. A failed version fetch still exits 0
/// (the guidance is printed either way); there is no "tool missing"
/// exit 1 here because no tool is executed at all.
pub fn run_update_check_only() -> i32 {
    let latest = fetch_latest_sync().ok();
    let report = update_report(latest.as_deref(), &upgrade_command());
    eprintln!("{report}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_flag_set_treats_empty_as_unset_and_zero_as_set() {
        use std::ffi::OsStr;
        assert!(!env_flag_set(None));
        assert!(!env_flag_set(Some(OsStr::new(""))));
        assert!(env_flag_set(Some(OsStr::new("0"))));
        assert!(env_flag_set(Some(OsStr::new("1"))));
    }

    #[test]
    fn should_update_check_needs_both_ttys_and_no_opt_out() {
        assert!(should_update_check(false, false, true, true));
        assert!(!should_update_check(true, false, true, true));
        assert!(!should_update_check(false, true, true, true));
        assert!(!should_update_check(false, false, false, true));
        assert!(!should_update_check(false, false, true, false));
        assert!(!should_update_check(false, false, false, false));
    }

    #[test]
    fn version_key_numeric_only() {
        assert_eq!(version_key("0.3.2"), Some(vec![0, 3, 2]));
        assert_eq!(version_key("v1.2"), Some(vec![1, 2]));
        assert_eq!(version_key("1.2.3+build"), Some(vec![1, 2, 3]));
        assert_eq!(version_key("1.2.3-rc1"), None);
        assert_eq!(version_key("1.2.x"), None);
        assert_eq!(version_key(""), None);
        assert_eq!(version_key("latest"), None);
    }

    #[test]
    fn is_newer_compares_numerically_not_lexically() {
        assert!(is_newer(Some("0.3.10"), "0.3.2"));
        assert!(!is_newer(Some("0.3.2"), "0.3.2"));
        assert!(!is_newer(Some("0.3.1"), "0.3.2"));
        assert!(!is_newer(Some("junk"), "0.3.2"));
        assert!(!is_newer(None, "0.3.2"));
        assert!(is_newer(Some("1.2.3"), "1.2"));
    }

    #[test]
    fn parse_version_examples() {
        assert_eq!(parse_version("__version__ = \"0.3.2\"\n").unwrap(), "0.3.2");
        assert_eq!(
            parse_version("# comment\n__version__='1.4.0'\n").unwrap(),
            "1.4.0"
        );
        assert!(matches!(
            parse_version("no version here\n").unwrap_err(),
            CliError::Value(_)
        ));
    }

    #[test]
    fn cache_freshness_and_fail_backoff_arithmetic() {
        assert!(cache_is_fresh(1000.0, 1000.0 + 3600.0));
        assert!(!cache_is_fresh(1000.0, 1000.0 + CHECK_INTERVAL_SECS));
        // After a failure at now=10000, the stored checked_at must be
        // exactly 1h before the 24h boundary: next check in 1h.
        assert_eq!(fail_backoff_checked_at(10_000.0), 10_000.0 - 82_800.0);
        assert!(cache_is_fresh(
            fail_backoff_checked_at(10_000.0),
            10_000.0 + 1800.0
        ));
        assert!(!cache_is_fresh(
            fail_backoff_checked_at(10_000.0),
            10_000.0 + 3600.0 + 1.0
        ));
    }

    #[test]
    fn upgrade_argv_for_each_install_kind() {
        assert_eq!(
            upgrade_argv_for(&InstallKind::Uv),
            vec![
                "uv",
                "tool",
                "install",
                "--upgrade",
                "git+https://github.com/thoitiettxl-cyber/muse-cli.git"
            ]
        );
        assert_eq!(
            upgrade_argv_for(&InstallKind::Pipx),
            vec![
                "pipx",
                "install",
                "--force",
                "git+https://github.com/thoitiettxl-cyber/muse-cli.git"
            ]
        );
        assert_eq!(
            upgrade_argv_for(&InstallKind::Pip),
            vec![
                "python3",
                "-m",
                "pip",
                "install",
                "-U",
                "git+https://github.com/thoitiettxl-cyber/muse-cli.git"
            ]
        );
        let root = PathBuf::from("/home/user/muse-cli-rust");
        assert_eq!(
            upgrade_argv_for(&InstallKind::Checkout(root)),
            vec!["git", "-C", "/home/user/muse-cli-rust", "pull", "--ff-only"]
        );
    }

    #[test]
    fn install_kind_detects_uv_and_pipx_path_segments() {
        let cwd = Path::new("/tmp");
        assert_eq!(
            install_kind(Path::new("/home/u/.local/share/uv/tools/muse-cli"), cwd),
            InstallKind::Uv
        );
        assert_eq!(
            install_kind(Path::new("/home/u/.local/pipx/venvs/x/bin/muse-cli"), cwd),
            InstallKind::Pipx
        );
        assert_eq!(
            install_kind(Path::new("/usr/local/bin/muse-cli"), cwd),
            InstallKind::Pip
        );
    }

    #[test]
    fn update_report_states_check_only_and_names_the_command() {
        let report = update_report(Some("0.4.0"), "uv tool install --upgrade X");
        assert!(report.contains("Latest release: 0.4.0"));
        assert!(report.contains("A newer release is available"));
        assert!(report.contains("does not self-upgrade"));
        assert!(report.contains("To upgrade, run: uv tool install --upgrade X"));
        let report = update_report(None, "git -C /r pull --ff-only");
        assert!(report.contains("Could not determine the latest release"));
        assert!(report.contains("does not self-upgrade"));
        let report = update_report(Some(CURRENT_VERSION), "pip install X");
        assert!(report.contains("You are on the latest release."));
    }

    #[test]
    fn finish_update_check_with_none_is_a_no_op() {
        finish_update_check(None);
    }
}
