//! Config, cookies, and auto-approve precedence (writer A: config.rs).
//!
//! Faithful to `cli.py:load_config` / `save_config` /
//! `auto_approve_setting` and `gateway.py:load_cookies`, except for the
//! standardisations listed at the end of this header (all of them are
//! cases where the Python original raises an uncaught exception and
//! Rust `main.rs` has no traceback/exit-1 path — every `CliError` maps
//! to exit 2/3/4 — so a clean error is the only honest equivalent).
//!
//! Faithful details kept on purpose:
//! - Config dir is `~/.config/muse-cli` via home expansion. No
//!   `XDG_CONFIG_HOME` handling: the original does not do it either.
//! - `load_config` reads `config.json` if present, defaults
//!   `cookies_file` to `<config_dir>/cookies.txt`, seeds `vm_id` from
//!   `MUSE_VM_ID` only when the key is absent (and does NOT mark the
//!   file dirty for that alone), and generates `node_id` (16 lowercase
//!   hex chars) when the key is absent — from `MUSE_NODE_ID` if set
//!   (not saved), otherwise randomly generated (saved immediately,
//!   creating the directory). If `node_id` was generated, the whole
//!   config — including an env-seeded `vm_id` — is written out, exactly
//!   like Python's `json.dump(cfg, …)` on the `changed` path.
//! - `save_config` is atomic (tmp file + rename); the `load_config`
//!   write on the `changed` path is direct, like the original.
//! - Cookie parsing accepts the same 3 forms in the same order:
//!   JSON, `a=b; c=d` (when it mentions `hatch_sess=` or `datr=`),
//!   Netscape jar. Unparseable -> `Auth`.
//! - Auto-approve precedence: per-call flag > `MUSE_AUTO_APPROVE` env
//!   > config > off. Env on = always/on/1/true/yes; off =
//!   off/0/false/no/never/"" (an empty-string env still counts as
//!   "env is set", because Python checks `is not None`). Anything else
//!   -> `Value` error (exit 2). Config counts as on only when its
//!   lowercased value is exactly `"always"`.
//!
//! Standardisations (Python original -> Rust here), reported to the
//! coordinator — none of them changes a successful path:
//! - Malformed `config.json`, a non-object top level, or a wrongly
//!   typed known key -> `CliError::Value` (exit 2). Python propagates
//!   the JSON/`AttributeError` exception as a traceback, exit 1.
//! - `load_cookies` on a missing file -> `CliError::Auth` with the same
//!   "no cookies at …" message `connect()` uses in Python. Python's
//!   `load_cookies` itself would raise `FileNotFoundError` (traceback,
//!   exit 1) when called directly, as in `wake`; `connect()` checks
//!   existence first and raises `AuthError`. The Rust `wake` handler
//!   (writer C) therefore gets the CONTRACT-normalised Auth/exit-2
//!   behaviour from this function directly.
//! - `load_cookies` on a JSON value that parses but is not an object
//!   falls through to the other forms instead of raising
//!   `AttributeError` (Python `.items()` on a non-dict). A dict whose
//!   `cookies` value is not an object is serialised pair-by-pair with
//!   scalar values rendered Python-style (`True`/`False`/`None`);
//!   nested dict/list values render as compact JSON (Python would
//!   render its own `repr`, which is not a cookie format anyway).
//! - A `cookies_file`/`vm_id`/`node_id`/`auto_approve`/
//!   `subscription_action_id` key present with JSON `null` is treated
//!   as absent, except `vm_id: null` which counts as "key present"
//!   (so it does NOT get env-seeded, matching Python's
//!   `"vm_id" not in cfg` check). Python would keep a `null` node_id
//!   as `None`; CONTRACT types `node_id` as `String`, so null is
//!   treated as missing here.
//! - `save_config` drops keys the Rust `Config` struct does not know
//!   (Python's dict round-trips unknown keys). All keys the original
//!   ever writes (`cookies_file`, `vm_id`, `node_id`, `auto_approve`,
//!   `subscription_action_id`) are known fields, so this only affects
//!   hand-edited extra keys.
//! - The `config` source string in auto-approve reporting uses this
//!   config's own `config.json` path; with the default config dir it
//!   is identical to Python's global `CONFIG_FILE`.

use crate::errors::CliError;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Runtime configuration (CONTRACT shape).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    /// `~/.config/muse-cli` (or the directory the config was loaded
    /// from). This field is NOT stored in `config.json` itself.
    #[serde(skip)]
    pub config_dir: PathBuf,
    /// Cookie file path (default `<config_dir>/cookies.txt`).
    pub cookies_file: PathBuf,
    /// VM id, if known (config, or `MUSE_VM_ID` seed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    /// Client node id (16 hex chars when generated).
    pub node_id: String,
    /// Raw config value, e.g. `"always"` / `"off"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<String>,
    /// Last subscription action id persisted by `usage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_action_id: Option<String>,
}

impl Config {
    /// Path of this config's `config.json`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }
}

/// Default config directory: `$HOME/.config/muse-cli`, the Rust form of
/// Python's `os.path.expanduser("~/.config/muse-cli")`.
pub fn default_config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("muse-cli");
        }
    }
    // `std::env::home_dir` falls back to the password database when
    // HOME is unset, which is what `expanduser` does too.
    #[allow(deprecated)]
    if let Some(home) = std::env::home_dir() {
        return home.join(".config").join("muse-cli");
    }
    PathBuf::from(".config").join("muse-cli")
}

/// Load the config from the default location (CONTRACT).
///
/// Creates `node_id` (16 hex chars) and saves if missing, like Python
/// `load_config` (see the header for the env-seed exceptions).
pub fn load_config() -> Result<Config, CliError> {
    let dir = default_config_dir();
    let vm_id_env = std::env::var("MUSE_VM_ID").ok();
    let node_id_env = std::env::var("MUSE_NODE_ID").ok();
    load_config_at(&dir, vm_id_env.as_deref(), node_id_env.as_deref())
}

/// Load the config from an explicit directory, with explicit env seeds.
///
/// `pub(crate)` seam for tests: production passes the real env values
/// through [`load_config`]. Env values that are empty strings do NOT
/// seed anything (Python checks truthiness, not just presence).
pub(crate) fn load_config_at(
    config_dir: &Path,
    vm_id_env: Option<&str>,
    node_id_env: Option<&str>,
) -> Result<Config, CliError> {
    let config_file = config_dir.join("config.json");
    let mut raw = serde_json::Map::new();
    if config_file.exists() {
        let text = std::fs::read_to_string(&config_file)
            .map_err(|e| CliError::io(format!("cannot read {}: {e}", config_file.display())))?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            CliError::value(format!(
                "invalid config JSON in {}: {e}",
                config_file.display()
            ))
        })?;
        match parsed {
            serde_json::Value::Object(map) => raw = map,
            _ => {
                return Err(CliError::value(format!(
                    "config file {} must contain a JSON object",
                    config_file.display()
                )));
            }
        }
    }

    let mut cookies_file = config_dir.join("cookies.txt");
    if let Some(value) = raw.get("cookies_file") {
        match value {
            serde_json::Value::String(s) => cookies_file = PathBuf::from(s),
            serde_json::Value::Null => {}
            other => {
                return Err(CliError::value(format!(
                    "config key 'cookies_file' must be a string, got {other}"
                )));
            }
        }
    }

    // vm_id: presence (even null) suppresses the env seed.
    let mut vm_id: Option<String> = None;
    if let Some(value) = raw.get("vm_id") {
        match value {
            serde_json::Value::String(s) => vm_id = Some(s.clone()),
            serde_json::Value::Null => {}
            other => {
                return Err(CliError::value(format!(
                    "config key 'vm_id' must be a string, got {other}"
                )));
            }
        }
    } else if let Some(env) = vm_id_env {
        if !env.is_empty() {
            vm_id = Some(env.to_string());
        }
    }

    let mut auto_approve: Option<String> = None;
    if let Some(value) = raw.get("auto_approve") {
        match value {
            serde_json::Value::String(s) => auto_approve = Some(s.clone()),
            serde_json::Value::Null => {}
            other => {
                return Err(CliError::value(format!(
                    "config key 'auto_approve' must be a string, got {other}"
                )));
            }
        }
    }

    let mut subscription_action_id: Option<String> = None;
    if let Some(value) = raw.get("subscription_action_id") {
        match value {
            serde_json::Value::String(s) => subscription_action_id = Some(s.clone()),
            serde_json::Value::Null => {}
            other => {
                return Err(CliError::value(format!(
                    "config key 'subscription_action_id' must be a string, got {other}"
                )));
            }
        }
    }

    // node_id: present-and-a-string wins (even empty, like Python);
    // otherwise env (not saved) or a fresh random 16-hex value (saved).
    let mut changed = false;
    let node_id = match raw.get("node_id") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => {
            if let Some(env) = node_id_env {
                if !env.is_empty() {
                    env.to_string()
                } else {
                    changed = true;
                    generate_node_id()
                }
            } else {
                changed = true;
                generate_node_id()
            }
        }
        Some(other) => {
            return Err(CliError::value(format!(
                "config key 'node_id' must be a string, got {other}"
            )));
        }
    };

    let cfg = Config {
        config_dir: config_dir.to_path_buf(),
        cookies_file,
        vm_id,
        node_id,
        auto_approve,
        subscription_action_id,
    };
    if changed {
        // Direct write, like Python's load_config (not atomic).
        std::fs::create_dir_all(config_dir)
            .map_err(|e| CliError::io(format!("cannot create {}: {e}", config_dir.display())))?;
        let text = serde_json::to_string_pretty(&cfg).map_err(CliError::from)?;
        std::fs::write(&config_file, text)
            .map_err(|e| CliError::io(format!("cannot write {}: {e}", config_file.display())))?;
    }
    Ok(cfg)
}

/// 16 lowercase hex chars, the Rust form of `secrets.token_hex(8)`.
fn generate_node_id() -> String {
    let bytes: [u8; 8] = rand::random();
    let mut out = String::with_capacity(16);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Save the config atomically via `<config.json>.tmp` + rename
/// (CONTRACT; mirrors Python `save_config`).
pub fn save_config(cfg: &Config) -> Result<(), CliError> {
    std::fs::create_dir_all(&cfg.config_dir)
        .map_err(|e| CliError::io(format!("cannot create {}: {e}", cfg.config_dir.display())))?;
    let config_file = cfg.config_file();
    let tmp = config_file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(CliError::from)?;
    std::fs::write(&tmp, text)
        .map_err(|e| CliError::io(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &config_file)
        .map_err(|e| CliError::io(format!("cannot replace {}: {e}", config_file.display())))?;
    Ok(())
}

/// Render a JSON scalar the way Python's `str()` / f-string would, for
/// cookie serialisation. Strings pass through; booleans become
/// `True`/`False`; null becomes `None`; numbers keep their JSON text.
fn python_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(true) => "True".to_string(),
        serde_json::Value::Bool(false) => "False".to_string(),
        serde_json::Value::Null => "None".to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Load cookies from a file (CONTRACT).
///
/// Three forms, in Python's order: JSON object (with a `cookies` dict,
/// or the whole top-level dict), a `a=b; c=d` line mentioning
/// `hatch_sess=` or `datr=`, or a Netscape jar. Unparseable -> `Auth`.
/// A missing file -> `Auth` with `connect()`'s message (see header).
/// Other read errors -> `Io`.
pub fn load_cookies(path: &Path) -> Result<String, CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::auth(format!(
                "no cookies at {}. Run `muse-cli auth export` and follow the steps it prints: \
                 turn on Chrome remote debugging at chrome://inspect/#remote-debugging, \
                 open https://muse.ai/ in that window, then export again.",
                path.display()
            )));
        }
        Err(e) => {
            return Err(CliError::io(format!("cannot read {}: {e}", path.display())));
        }
    };
    let raw = text.trim();
    if raw.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(serde_json::Value::Object(cookies)) = map.get("cookies") {
                return Ok(cookies
                    .iter()
                    .map(|(k, v)| format!("{k}={}", python_scalar(v)))
                    .collect::<Vec<_>>()
                    .join("; "));
            }
            return Ok(map
                .iter()
                .map(|(k, v)| format!("{k}={}", python_scalar(v)))
                .collect::<Vec<_>>()
                .join("; "));
        }
        // JSON that fails to parse, or is not an object, falls through
        // to the other forms (Python's except JSONDecodeError: pass).
    }
    if raw.contains("hatch_sess=") || raw.contains("datr=") {
        for line in raw.lines() {
            let line = line.trim();
            if !line.is_empty()
                && !line.starts_with('#')
                && line.contains('=')
                && line.contains(';')
            {
                return Ok(line.to_string());
            }
        }
        return Ok(raw.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    // Netscape cookie-jar format.
    let mut parts = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 7 {
            parts.push(format!("{}={}", cols[5], cols[6]));
        }
    }
    if parts.is_empty() {
        return Err(CliError::auth(format!(
            "could not parse cookies file {}",
            path.display()
        )));
    }
    Ok(parts.join("; "))
}

/// Whether the config's cookies file exists (CONTRACT).
pub fn cookies_file_exists(cfg: &Config) -> bool {
    cfg.cookies_file.exists()
}

const AUTO_APPROVE_ON: &[&str] = &["always", "on", "1", "true", "yes"];
const AUTO_APPROVE_OFF: &[&str] = &["off", "0", "false", "no", "never", ""];

/// Quote a string the way Python's `repr()` does for a plain string
/// (single quotes, backslash and quote escapes). Used for the
/// `MUSE_AUTO_APPROVE` error message.
fn python_repr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// Auto-approve setting with an explicit env value (test seam).
///
/// `env` is `Some(value)` when `MUSE_AUTO_APPROVE` is set (even to
/// `""`), `None` when it is unset.
pub(crate) fn auto_approve_setting_with_env(
    cfg: &Config,
    flag: Option<bool>,
    env: Option<&str>,
) -> Result<(bool, String), CliError> {
    if let Some(enabled) = flag {
        return Ok((enabled, "flag".to_string()));
    }
    if let Some(raw) = env {
        let v = raw.trim().to_lowercase();
        if AUTO_APPROVE_ON.contains(&v.as_str()) {
            return Ok((true, "env MUSE_AUTO_APPROVE".to_string()));
        }
        if AUTO_APPROVE_OFF.contains(&v.as_str()) {
            return Ok((false, "env MUSE_AUTO_APPROVE".to_string()));
        }
        return Err(CliError::value(format!(
            "MUSE_AUTO_APPROVE must be always or off, not {}",
            python_repr(raw)
        )));
    }
    let enabled = cfg.auto_approve.as_deref().unwrap_or("off").to_lowercase() == "always";
    Ok((enabled, format!("config {}", cfg.config_file().display())))
}

/// Auto-approve setting with its source string.
///
/// Source is `"flag"`, `"env MUSE_AUTO_APPROVE"`, or
/// `"config <config.json path>"`. Writer C's `auto-approve` command
/// needs the source for its JSON output; [`auto_approve_setting`]
/// (CONTRACT) is the boolean-only form.
pub fn auto_approve_setting_source(
    cfg: &Config,
    flag: Option<bool>,
) -> Result<(bool, String), CliError> {
    let env = std::env::var("MUSE_AUTO_APPROVE").ok();
    auto_approve_setting_with_env(cfg, flag, env.as_deref())
}

/// Auto-approve setting (CONTRACT): per-call flag > env > config > off.
pub fn auto_approve_setting(cfg: &Config, flag: Option<bool>) -> Result<bool, CliError> {
    Ok(auto_approve_setting_source(cfg, flag)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "muse-cli-rust-config-test-{tag}-{pid}-{n}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    fn test_config(dir: &Path) -> Config {
        Config {
            config_dir: dir.to_path_buf(),
            cookies_file: dir.join("cookies.txt"),
            vm_id: None,
            node_id: "0123456789abcdef".to_string(),
            auto_approve: None,
            subscription_action_id: None,
        }
    }

    #[test]
    fn load_config_missing_file_generates_16_hex_node_id_and_saves() {
        let dir = temp_dir("missing");
        let cfg = load_config_at(&dir, None, None).unwrap();
        assert_eq!(cfg.node_id.len(), 16);
        assert!(cfg.node_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(cfg.cookies_file, dir.join("cookies.txt"));
        // The generated id was persisted.
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(saved["node_id"], cfg.node_id);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_config_env_node_id_is_used_but_not_saved() {
        let dir = temp_dir("env-node");
        let cfg = load_config_at(&dir, None, Some("env-node-id")).unwrap();
        assert_eq!(cfg.node_id, "env-node-id");
        assert!(!dir.join("config.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_config_env_vm_id_seeds_but_is_not_saved_alone() {
        let dir = temp_dir("env-vm");
        let cfg = load_config_at(&dir, Some("vm-123"), Some("node-from-env")).unwrap();
        assert_eq!(cfg.vm_id.as_deref(), Some("vm-123"));
        assert!(!dir.join("config.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_config_generated_node_id_also_persists_env_vm_id() {
        // Python quirk: the `changed` dump writes the whole dict,
        // including the env-seeded vm_id.
        let dir = temp_dir("env-vm-saved");
        let cfg = load_config_at(&dir, Some("vm-123"), None).unwrap();
        assert_eq!(cfg.vm_id.as_deref(), Some("vm-123"));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(saved["vm_id"], "vm-123");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_config_existing_node_id_is_kept_and_file_untouched() {
        let dir = temp_dir("existing");
        write(
            &dir.join("config.json"),
            "{\"node_id\": \"aaaaaaaaaaaaaaaa\", \"auto_approve\": \"always\"}",
        );
        let cfg = load_config_at(&dir, Some("vm-ignored?"), None).unwrap();
        assert_eq!(cfg.node_id, "aaaaaaaaaaaaaaaa");
        assert_eq!(cfg.auto_approve.as_deref(), Some("always"));
        // vm_id key absent in the file, but the file already has a
        // node_id, so nothing is written and the env seed is NOT
        // persisted (it is still visible in memory, like Python).
        assert_eq!(cfg.vm_id.as_deref(), Some("vm-ignored?"));
        let saved = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!saved.contains("vm-ignored?"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_config_malformed_json_is_a_value_error_not_a_crash() {
        let dir = temp_dir("bad-json");
        write(&dir.join("config.json"), "{not json");
        let err = load_config_at(&dir, None, None).unwrap_err();
        assert!(matches!(err, CliError::Value(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_config_round_trips_through_load() {
        let dir = temp_dir("save");
        let mut cfg = test_config(&dir);
        cfg.auto_approve = Some("always".to_string());
        cfg.subscription_action_id = Some("deadbeef".to_string());
        save_config(&cfg).unwrap();
        let back = load_config_at(&dir, None, None).unwrap();
        assert_eq!(back, cfg);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_plain_pair_string() {
        let dir = temp_dir("cookie-plain");
        let path = dir.join("cookies.txt");
        write(&path, "hatch_sess=FAKE-SESS; datr=FAKE-DATR");
        assert_eq!(
            load_cookies(&path).unwrap(),
            "hatch_sess=FAKE-SESS; datr=FAKE-DATR"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_json_with_cookies_dict() {
        let dir = temp_dir("cookie-json");
        let path = dir.join("cookies.txt");
        write(
            &path,
            "{\"cookies\": {\"hatch_sess\": \"FAKE-SESS\", \"other\": \"FAKE-OTHER\"}}",
        );
        assert_eq!(
            load_cookies(&path).unwrap(),
            "hatch_sess=FAKE-SESS; other=FAKE-OTHER"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_json_top_level_dict() {
        let dir = temp_dir("cookie-json2");
        let path = dir.join("cookies.txt");
        write(&path, "{\"hatch_sess\": \"FAKE-SESS\"}");
        assert_eq!(load_cookies(&path).unwrap(), "hatch_sess=FAKE-SESS");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_netscape_jar() {
        let dir = temp_dir("cookie-netscape");
        let path = dir.join("cookies.txt");
        write(
            &path,
            "# Netscape HTTP Cookie File\n.muse.ai\tTRUE\t/\tTRUE\t0\thatch_sess\tFAKE-SESS\n.muse.ai\tTRUE\t/\tFALSE\t0\tdatr\tFAKE-DATR\n",
        );
        assert_eq!(
            load_cookies(&path).unwrap(),
            "hatch_sess=FAKE-SESS; datr=FAKE-DATR"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_multiline_pair_string_picks_the_pair_line() {
        let dir = temp_dir("cookie-multi");
        let path = dir.join("cookies.txt");
        write(
            &path,
            "# a comment\nhatch_sess=FAKE-SESS; other=FAKE-OTHER\n",
        );
        assert_eq!(
            load_cookies(&path).unwrap(),
            "hatch_sess=FAKE-SESS; other=FAKE-OTHER"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_unparseable_is_auth_error() {
        let dir = temp_dir("cookie-bad");
        let path = dir.join("cookies.txt");
        write(&path, "nothing parseable here");
        assert!(matches!(
            load_cookies(&path).unwrap_err(),
            CliError::Auth(_)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_cookies_missing_file_is_auth_error() {
        let dir = temp_dir("cookie-missing");
        let err = load_cookies(&dir.join("nope.txt")).unwrap_err();
        assert!(matches!(err, CliError::Auth(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cookies_file_exists_reports_presence() {
        let dir = temp_dir("exists");
        let cfg = test_config(&dir);
        assert!(!cookies_file_exists(&cfg));
        write(&cfg.cookies_file, "hatch_sess=FAKE");
        assert!(cookies_file_exists(&cfg));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_approve_flag_beats_env_and_config() {
        let cfg = test_config(&std::env::temp_dir());
        let (enabled, source) =
            auto_approve_setting_with_env(&cfg, Some(true), Some("off")).unwrap();
        assert!(enabled);
        assert_eq!(source, "flag");
        let (enabled, source) =
            auto_approve_setting_with_env(&cfg, Some(false), Some("always")).unwrap();
        assert!(!enabled);
        assert_eq!(source, "flag");
    }

    #[test]
    fn auto_approve_env_values_on_and_off() {
        let cfg = test_config(&std::env::temp_dir());
        for on in ["always", "on", "1", "true", "yes", " ALWAYS "] {
            let (enabled, source) = auto_approve_setting_with_env(&cfg, None, Some(on)).unwrap();
            assert!(enabled, "{on}");
            assert_eq!(source, "env MUSE_AUTO_APPROVE");
        }
        for off in ["off", "0", "false", "no", "never", ""] {
            let (enabled, _) = auto_approve_setting_with_env(&cfg, None, Some(off)).unwrap();
            assert!(!enabled, "{off:?}");
        }
    }

    #[test]
    fn auto_approve_bad_env_value_is_a_value_error() {
        let cfg = test_config(&std::env::temp_dir());
        let err = auto_approve_setting_with_env(&cfg, None, Some("maybe")).unwrap_err();
        match err {
            CliError::Value(msg) => {
                assert!(msg.contains("MUSE_AUTO_APPROVE must be always or off, not 'maybe'"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn auto_approve_config_only_always_counts_as_on() {
        let dir = std::env::temp_dir();
        let mut cfg = test_config(&dir);
        cfg.auto_approve = Some("always".to_string());
        assert!(auto_approve_setting_with_env(&cfg, None, None).unwrap().0);
        cfg.auto_approve = Some("on".to_string());
        assert!(!auto_approve_setting_with_env(&cfg, None, None).unwrap().0);
        cfg.auto_approve = Some("Always".to_string());
        assert!(auto_approve_setting_with_env(&cfg, None, None).unwrap().0);
        cfg.auto_approve = None;
        let (enabled, source) = auto_approve_setting_with_env(&cfg, None, None).unwrap();
        assert!(!enabled);
        assert!(source.starts_with("config "));
    }
}
