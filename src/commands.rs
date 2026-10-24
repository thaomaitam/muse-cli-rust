//! Command dispatch (writer C: commands.rs).
//!
//! `dispatch(cli)` returns the process exit code and prints output
//! itself, mirroring `cli.py`'s handlers. Connections are lazy: only
//! commands that need the gateway connect, and the pre-connect checks
//! (bare `auth`/`fs`, `approve` without exactly one of id/`--all`,
//! `fs rm` without `--yes`, `raw` with an unknown method/bad
//! `--body`/bad `--param`) all fail BEFORE any connection, like the
//! original.
//!
//! Deviations from the original / CONTRACT, all reported to the
//! coordinator (see the writer's report):
//! - `run_wait`'s factory closure is ALSO used for the history
//!   re-read. Fix round 2 removed the old deviation (factory
//!   failure yielding empty replies): a factory (connect) failure
//!   at the re-read stage now propagates like Python, where the
//!   re-read connect sits outside the `try` and only the
//!   `call_json` errors are swallowed. `wait`/`send` print exactly
//!   what `run_wait` returns (fix round 1 removed the extra re-read
//!   writer C had added on top of it); `WaitResult` now carries
//!   `status`, `replies`, `after_seq`, `widgets` and
//!   `needs_input_reason`, like Python's `run_wait` result dict, so
//!   no field is recomputed here. `status` is the last root-agent
//!   task status seen, or `null` when none was seen (Python prints
//!   `None` there); text mode prints `(status=None)` in that case.
//! - Key order inside printed JSON objects is serde's sorted order,
//!   not Python's insertion order (writer A's documented serde
//!   difference). JSON semantics are identical.
//! - `watch` rows use Python's `json.dumps` separators (`', '` and
//!   `': '`): the compact serde output is re-spaced outside JSON
//!   strings only (see `python_json_separators`), so string values
//!   are never altered. One JSON object per line, as in the original.
//! - Ctrl+C in interactive chat cannot print `\nBye.` (see chat.rs).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::cli::{AuthCommand, AutoApproveState, Cli, Command, FsCommand};
use crate::config::{
    auto_approve_setting, auto_approve_setting_source, cookies_file_exists, default_config_dir,
    load_config, load_cookies, save_config, Config,
};
use crate::errors::CliError;
use crate::fsclient::{apply_replace, edit_in_editor, fmt_size, FsClient};
use crate::gateway::{
    create_invite, fetch_invite, fetch_subscription, redeem_invite_code, wake_vm, Gateway,
};
use crate::media::{event_resources, local_name};
use crate::output::print_json;
use crate::routes;
use crate::update;
use crate::usage::{
    format_local_timestamp, local_offset_for_tz, summarize_invite, summarize_usage,
};
use crate::wait::{
    describe_approval, offers_forever, run_wait, user_ack, WaitOutcomeKind, WaitResult,
};

// ---------------------------------------------------------------------------
// Small Python-faithful value helpers (shared with chat.rs / fsclient.rs)
// ---------------------------------------------------------------------------

/// Quote a string the way Python's `repr()` does for a plain string.
pub(crate) fn python_repr(s: &str) -> String {
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

/// Render a JSON value the way Python's `str()` would for scalars
/// (`None`/`True`/`False`, numbers as-is, strings as-is). Compound
/// values render as compact JSON (Python's `repr` differs, but these
/// paths only ever see scalars in practice).
pub(crate) fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python truthiness for a JSON value.
pub(crate) fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Truthy value rendered as text, or `""` when falsy. Used for chat
/// message text (Python: `display_text or content or ""`).
pub(crate) fn json_truthy_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other if json_truthy(other) => python_str(other),
        _ => String::new(),
    }
}

/// First truthy value among the candidates, else `Null`.
fn first_truthy(values: &[Option<&Value>]) -> Value {
    for value in values.iter().flatten() {
        if json_truthy(value) {
            return (*value).clone();
        }
    }
    Value::Null
}

/// First non-empty string among the candidates.
fn first_nonempty(values: &[Option<&Value>]) -> Option<String> {
    for value in values.iter().flatten() {
        if let Value::String(s) = value {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// `str(v or "")` for an id-like field: missing/falsy -> `""`,
/// strings as-is, other truthy values via `python_str`.
fn opt_id_str(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) if json_truthy(other) => python_str(other),
        Some(_) => String::new(),
    }
}

/// `CliError` rendered the way Python's `str()` renders the matching
/// exception: for `Gateway`, `gateway status=N payload='<payload>'`
/// with a single-quoted repr (Rust's `Display` uses double quotes).
pub(crate) fn cli_error_message(err: &CliError) -> String {
    match err {
        CliError::Gateway { status, payload } => {
            format!("gateway status={status} payload={}", python_repr(payload))
        }
        other => other.to_string(),
    }
}

/// Clamp a user-supplied seconds value to a non-negative `Duration`
/// (a negative timeout must not wrap into a huge `u64`).
fn secs(value: i64) -> Duration {
    Duration::from_secs(value.max(0) as u64)
}

/// Local `YYYY-MM-DD HH:MM` for a Unix timestamp, mirroring
/// `time.strftime("%Y-%m-%d %H:%M", time.localtime(...))`.
pub(crate) fn local_datetime(secs: i64) -> String {
    let stamped = format_local_timestamp(secs);
    let short: String = stamped.chars().take(16).collect();
    short.replace('T', " ")
}

// --- TZ abbreviation (for the usage resets line) ---------------------------
// Python prints time.strftime('%Z'); writer A's usage.rs exposes the
// offset but not the abbreviation, so it is read from the zoneinfo
// file here. On any parse failure the numeric offset (+HHMM) is used.

fn be_u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be_i32_at(data: &[u8], at: usize) -> Option<i32> {
    be_u32_at(data, at).map(|v| v as i32)
}

fn be_i64_at(data: &[u8], at: usize) -> Option<i64> {
    data.get(at..at + 8)
        .map(|b| i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

fn tzif_abbrev(path: &str, timestamp: i64) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 44 || &data[0..4] != b"TZif" {
        return None;
    }
    let version = data[4];
    let mut transitions: Vec<i64> = Vec::new();
    let mut indices: Vec<usize> = Vec::new();
    let mut types: Vec<(i32, bool, usize)> = Vec::new();
    let mut abbrevs: Vec<u8> = Vec::new();
    let parse_block = |data: &[u8],
                       base: usize,
                       wide: bool,
                       transitions: &mut Vec<i64>,
                       indices: &mut Vec<usize>,
                       types: &mut Vec<(i32, bool, usize)>,
                       abbrevs: &mut Vec<u8>|
     -> Option<()> {
        let timecnt = be_u32_at(data, base + 32)? as usize;
        let typecnt = be_u32_at(data, base + 36)? as usize;
        let charcnt = be_u32_at(data, base + 40)? as usize;
        let mut at = base + 44;
        for _ in 0..timecnt {
            if wide {
                transitions.push(be_i64_at(data, at)?);
                at += 8;
            } else {
                transitions.push(be_i32_at(data, at)? as i64);
                at += 4;
            }
        }
        for _ in 0..timecnt {
            indices.push(*data.get(at)? as usize);
            at += 1;
        }
        for _ in 0..typecnt {
            let gmtoff = be_i32_at(data, at)?;
            let isdst = data.get(at + 4).copied().unwrap_or(0) != 0;
            let abbrind = data.get(at + 5).copied().unwrap_or(0) as usize;
            types.push((gmtoff, isdst, abbrind));
            at += 6;
        }
        for _ in 0..charcnt {
            abbrevs.push(*data.get(at)?);
            at += 1;
        }
        Some(())
    };
    if version == b'2' || version == b'3' || version == b'4' {
        let isutcnt = be_u32_at(&data, 20)? as usize;
        let isstdcnt = be_u32_at(&data, 24)? as usize;
        let leapcnt = be_u32_at(&data, 28)? as usize;
        let timecnt = be_u32_at(&data, 32)? as usize;
        let typecnt = be_u32_at(&data, 36)? as usize;
        let charcnt = be_u32_at(&data, 40)? as usize;
        let v1_size =
            timecnt * 4 + timecnt + typecnt * 6 + charcnt + leapcnt * 8 + isstdcnt + isutcnt;
        let base = 44 + v1_size;
        if data.len() < base + 44 || &data[base..base + 4] != b"TZif" {
            return None;
        }
        parse_block(
            &data,
            base,
            true,
            &mut transitions,
            &mut indices,
            &mut types,
            &mut abbrevs,
        )?;
    } else {
        parse_block(
            &data,
            0,
            false,
            &mut transitions,
            &mut indices,
            &mut types,
            &mut abbrevs,
        )?;
    }
    let mut chosen: Option<usize> = None;
    for (i, when) in transitions.iter().enumerate() {
        if *when <= timestamp {
            chosen = indices.get(i).copied();
        } else {
            break;
        }
    }
    let type_index = match chosen {
        Some(index) => index,
        None => types.iter().position(|(_, isdst, _)| !*isdst).unwrap_or(0),
    };
    let (_, _, abbrind) = types.get(type_index).copied().unwrap_or((0, false, 0));
    if abbrind >= abbrevs.len() {
        return None;
    }
    let end = abbrevs[abbrind..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| abbrind + p)
        .unwrap_or(abbrevs.len());
    String::from_utf8(abbrevs[abbrind..end].to_vec()).ok()
}

fn tz_abbrev(timestamp: i64) -> String {
    let tz_name = std::env::var("TZ").ok().and_then(|v| {
        let stripped = v.strip_prefix(':').unwrap_or(&v).to_string();
        if stripped.is_empty() {
            None
        } else {
            Some(stripped)
        }
    });
    let path = match &tz_name {
        Some(name) if !name.split('/').any(|part| part == "..") => {
            format!("/usr/share/zoneinfo/{name}")
        }
        _ => "/etc/localtime".to_string(),
    };
    if let Some(abbr) = tzif_abbrev(&path, timestamp) {
        return abbr;
    }
    let offset = local_offset_for_tz(tz_name.as_deref(), timestamp);
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

/// Absolute path, lexically normalised (Python `os.path.abspath`
/// without symlink resolution).
fn abspath(path: &Path) -> PathBuf {
    if let Ok(abs) = std::path::absolute(path) {
        return abs;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

// ---------------------------------------------------------------------------
// History helpers
// ---------------------------------------------------------------------------

/// Full `chat.history` response object (GET body becomes query).
pub(crate) fn history_response(
    gw: &mut Gateway,
    thread: Option<&str>,
    limit: i64,
) -> Result<Value, CliError> {
    let mut body = serde_json::json!({"limit": limit});
    if let Some(thread) = thread {
        body.as_object_mut()
            .map(|map| map.insert("session_id".to_string(), Value::String(thread.to_string())));
    }
    gw.call_json("chat.history", &[], Some(&body), None, secs(30))
}

/// `chat_events` of a history response (empty when absent/not a list).
pub(crate) fn history_events(
    gw: &mut Gateway,
    thread: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>, CliError> {
    let value = history_response(gw, thread, limit)?;
    Ok(value
        .get("chat_events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `cli.py:fmt_event`.
pub(crate) fn fmt_event(event: &Value) -> Value {
    let payload = event.get("payload").filter(|v| v.is_object());
    let role = first_truthy(&[payload.and_then(|p| p.get("role")), event.get("event_name")]);
    let mut text = first_truthy(&[
        payload.and_then(|p| p.get("display_text")),
        payload.and_then(|p| p.get("content")),
    ]);
    if text.is_null() {
        text = Value::String(String::new());
    }
    let message_id = first_truthy(&[
        payload.and_then(|p| p.get("message_id")),
        event.get("message_id"),
    ]);
    serde_json::json!({
        "seq": event.get("seq").cloned().unwrap_or(Value::Null),
        "role": role,
        "message_id": message_id,
        "text": text,
    })
}

fn event_has_text(event: &Value) -> bool {
    let payload = event.get("payload").filter(|v| v.is_object());
    payload
        .and_then(|p| p.get("display_text"))
        .is_some_and(json_truthy)
        || payload
            .and_then(|p| p.get("content"))
            .is_some_and(json_truthy)
}

fn event_matches_ref(event: &Value, reference: &str) -> bool {
    let payload = event.get("payload").filter(|v| v.is_object());
    let seq = event
        .get("seq")
        .map(python_str)
        .unwrap_or_else(|| "None".to_string());
    let candidates = [
        seq,
        opt_id_str(payload.and_then(|p| p.get("message_id"))),
        opt_id_str(event.get("message_id")),
        opt_id_str(payload.and_then(|p| p.get("id"))),
    ];
    candidates.iter().any(|id| id == reference)
}

// ---------------------------------------------------------------------------
// Media download orchestration (writer C owns this; pure parts are in
// media.rs). Mirrors cli.py:save_resources.
// ---------------------------------------------------------------------------

/// Write `data` to `dest` through a `<dest>.part` file (fix round
/// 2, SAFETY F2). The `.part` file is created with `create_new`,
/// so a pre-planted file or symlink at that path is refused
/// instead of being followed/truncated; before the rename, a
/// symlinked `dest` is refused, so its target is never overwritten
/// (the rename would otherwise replace the link itself, but
/// refusing is fail-closed). On failure, the `.part` file is
/// removed only when this call created it — a pre-existing `.part`
/// (e.g. a planted symlink, or a leftover from a crashed run) is
/// left untouched, never deleted.
fn write_file_via_part(data: &[u8], dest: &Path) -> Result<(), CliError> {
    let part = PathBuf::from(format!("{}.part", dest.to_string_lossy()));
    let mut created = false;
    let result = (|| -> Result<(), CliError> {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)?;
            created = true;
            file.write_all(data)?;
        }
        if std::fs::symlink_metadata(dest)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(CliError::value(format!(
                "refusing to overwrite symlink: {}",
                dest.display()
            )));
        }
        std::fs::rename(&part, dest)?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Download each resource into `directory` and return one result row
/// per resource. A file that already exists (and `overwrite` is false)
/// is skipped; a per-file download failure becomes an `error` row.
/// A server-supplied name that would escape the directory (`..`/`.`
/// basename) becomes an `error` row instead of writing outside it.
pub(crate) fn save_resources(
    gw: &mut Gateway,
    resources: &[Value],
    directory: &str,
    overwrite: bool,
) -> Result<Vec<Value>, CliError> {
    std::fs::create_dir_all(directory)?;
    let mut rows = Vec::new();
    for resource in resources {
        let Some(path) = resource.get("path").and_then(|v| v.as_str()) else {
            rows.push(serde_json::json!({
                "path": resource.get("path").cloned().unwrap_or(Value::Null),
                "error": "resource has no path",
            }));
            continue;
        };
        let dest = local_name(path, Path::new(directory));
        let name_safe = dest
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n != ".." && n != ".");
        if !name_safe {
            rows.push(serde_json::json!({
                "path": path,
                "file": abspath(&dest).to_string_lossy(),
                "error": format!("unsafe local filename for path {}", python_repr(path)),
            }));
            continue;
        }
        let mut row = serde_json::json!({
            "path": path,
            "file": abspath(&dest).to_string_lossy(),
        });
        if dest.exists() && !overwrite {
            let size = std::fs::metadata(&dest)?.len();
            row.as_object_mut().map(|map| {
                map.insert("skipped".to_string(), Value::String("exists".to_string()));
                map.insert("bytes".to_string(), Value::from(size));
            });
            rows.push(row);
            continue;
        }
        match gw.download_bytes(resource, Duration::from_secs(300)) {
            Ok(data) => {
                write_file_via_part(&data, &dest)?;
                let len = data.len();
                let mut warning: Option<String> = None;
                if let Some(byte_len) = resource.get("byte_len") {
                    if json_truthy(byte_len) {
                        let matches = match byte_len {
                            Value::Number(n) => n.as_f64() == Some(len as f64),
                            _ => false,
                        };
                        if !matches {
                            warning = Some(format!("expected {} bytes", python_str(byte_len)));
                        }
                    }
                }
                row.as_object_mut().map(|map| {
                    map.insert("bytes".to_string(), Value::from(len));
                    if let Some(warning) = warning {
                        map.insert("warning".to_string(), Value::String(warning));
                    }
                });
                rows.push(row);
            }
            Err(e @ (CliError::Gateway { .. } | CliError::Timeout(_) | CliError::Value(_))) => {
                row.as_object_mut().map(|map| {
                    map.insert("error".to_string(), Value::String(cli_error_message(&e)));
                });
                rows.push(row);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(rows)
}

fn dedup_resources(events: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for event in events {
        for resource in event_resources(event) {
            let path = resource
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !seen.contains(&path) {
                seen.push(path);
                out.push(resource);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Gateway wrapper: connect, run, close (even on error, like Python's
// try/finally gw.close()).
// ---------------------------------------------------------------------------

fn with_gateway<F>(f: F) -> Result<i32, CliError>
where
    F: FnOnce(&Config, &mut Gateway) -> Result<i32, CliError>,
{
    let cfg = load_config()?;
    let mut gw = Gateway::connect(&cfg)?;
    let result = f(&cfg, &mut gw);
    gw.close();
    result
}

/// `usage`/`invite`/`redeem` pre-check: a missing cookies file raises
/// the usual auth error (Python calls `connect(cfg)` just for that).
fn ensure_cookies(cfg: &Config) -> Result<(), CliError> {
    if !cookies_file_exists(cfg) {
        load_cookies(&cfg.cookies_file)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch one parsed CLI invocation. Returns the exit code.
pub fn dispatch(cli: Cli) -> Result<i32, CliError> {
    let Some(command) = cli.command else {
        return crate::chat::run_chat();
    };
    if command.missing_subcommand() {
        match &command {
            Command::Auth { .. } => {
                eprintln!("error: auth requires a subcommand (auth export)");
            }
            Command::Fs { .. } => {
                eprintln!("error: fs requires a subcommand");
            }
            _ => {}
        }
        return Ok(2);
    }
    match command {
        Command::Auth {
            op: Some(AuthCommand::Export),
        } => cmd_auth_export(),
        Command::Auth { .. } => Ok(2),
        Command::Chat => crate::chat::run_chat(),
        Command::Status => with_gateway(|_, gw| cmd_status(gw)),
        Command::Usage { json, raw } => cmd_usage(json, raw),
        Command::Invite { json, raw, create } => cmd_invite(json, raw, create),
        Command::Redeem { code, json } => cmd_redeem(code, json),
        Command::Threads { archived } => with_gateway(|_, gw| cmd_threads(gw, archived)),
        Command::History {
            thread,
            limit,
            raw,
            download,
        } => with_gateway(|_, gw| cmd_history(gw, thread, limit, raw, download)),
        Command::Send {
            text,
            thread,
            wait,
            until_done,
            text_output,
            ack_timeout,
            download,
            auto_approve,
            no_auto_approve,
        } => {
            let flag = if auto_approve {
                Some(true)
            } else if no_auto_approve {
                Some(false)
            } else {
                None
            };
            with_gateway(|cfg, gw| {
                cmd_send(
                    cfg,
                    gw,
                    text,
                    thread,
                    wait,
                    until_done,
                    text_output,
                    ack_timeout,
                    download,
                    flag,
                )
            })
        }
        Command::Wait {
            thread,
            after_seq,
            timeout,
            json,
            require_running,
            auto_approve,
            no_auto_approve,
        } => {
            let flag = if auto_approve {
                Some(true)
            } else if no_auto_approve {
                Some(false)
            } else {
                None
            };
            // Fix round 2 (RED-TEAM/TECHNICAL F02): resolve the
            // auto-approve setting BEFORE connecting, so a bogus
            // `MUSE_AUTO_APPROVE` value fails here instead of
            // after a gateway connection. `cmd_wait` resolves it
            // again below (idempotent). `send` deliberately keeps
            // the original connect-first order and is untouched.
            let pre_cfg = load_config()?;
            let _ = auto_approve_setting(&pre_cfg, flag)?;
            with_gateway(|cfg, gw| {
                cmd_wait(
                    cfg,
                    gw,
                    thread,
                    after_seq,
                    timeout,
                    json,
                    require_running,
                    flag,
                )
            })
        }
        Command::Approvals {
            recent,
            raw,
            approve_all: approve_all_flag,
        } => with_gateway(|_, gw| cmd_approvals(gw, recent, raw, approve_all_flag)),
        Command::Approve {
            id,
            all,
            once: _,
            session,
            today,
            always,
        } => {
            if all == id.is_some() {
                return Err(CliError::value(
                    "give an approval ID or --all (not both)".to_string(),
                ));
            }
            let decision = if always {
                "allow_always"
            } else if today {
                "allow_today"
            } else if session {
                "allow_session"
            } else {
                "allow_once"
            };
            with_gateway(|_, gw| cmd_approve(gw, id, all, decision))
        }
        Command::AutoApprove { state } => cmd_auto_approve(state),
        Command::Deny { id, always } => {
            let decision = if always { "deny_always" } else { "deny" };
            with_gateway(|_, gw| {
                let value = decide(gw, &id, decision)?;
                print_json(&value);
                Ok(0)
            })
        }
        Command::Files { thread, limit } => with_gateway(|_, gw| cmd_files(gw, thread, limit)),
        Command::Download {
            reference,
            output,
            thread,
            limit,
            force,
        } => with_gateway(|_, gw| cmd_download(gw, reference, output, thread, limit, force)),
        Command::Watch {
            timeout,
            n,
            all,
            thread,
        } => with_gateway(|_, gw| cmd_watch(gw, timeout, n, all, thread)),
        Command::Feed { limit, raw } => with_gateway(|_, gw| cmd_feed(gw, limit, raw)),
        Command::FeedStatus => with_gateway(|_, gw| {
            let value = gw.call_json("feed.status", &[], None, None, secs(30))?;
            print_json(&value);
            Ok(0)
        }),
        Command::FeedReact { unit, reaction } => with_gateway(|_, gw| {
            let value = gw.call_json(
                "feed.unit.reaction",
                &[("unit_id".to_string(), unit)],
                Some(&serde_json::json!({"reaction": reaction})),
                None,
                secs(30),
            )?;
            print_json(&value);
            Ok(0)
        }),
        Command::FeedPrompt => with_gateway(|_, gw| {
            let value = gw.call_json("feed.prompt.get", &[], None, None, secs(30))?;
            print_json(&value);
            Ok(0)
        }),
        Command::Goals => with_gateway(|_, gw| cmd_goals(gw)),
        Command::Goal { id } => with_gateway(|_, gw| {
            let value =
                gw.call_json("goals.get", &[("id".to_string(), id)], None, None, secs(30))?;
            print_json(&value);
            Ok(0)
        }),
        Command::Ideas => with_gateway(|_, gw| cmd_ideas(gw)),
        Command::Idea { id } => with_gateway(|_, gw| {
            let value = gw.call_json(
                "api.idea-cards.detail",
                &[("ideaCardId".to_string(), id)],
                None,
                None,
                secs(30),
            )?;
            print_json(&value);
            Ok(0)
        }),
        Command::IdeaExec { id, thread } => with_gateway(|_, gw| {
            let mut body = serde_json::json!({"ideaCardId": id, "mode": "full"});
            if let Some(thread) = thread {
                body.as_object_mut()
                    .map(|map| map.insert("session_id".to_string(), Value::String(thread)));
            }
            let value = gw.call_json(
                "api.idea-cards.execute",
                &[("ideaCardId".to_string(), id)],
                Some(&body),
                None,
                secs(30),
            )?;
            print_json(&value);
            Ok(0)
        }),
        Command::Unread => with_gateway(|_, gw| {
            let value = gw.call_json("chat.unread_count", &[], None, None, secs(30))?;
            print_json(&value);
            Ok(0)
        }),
        Command::Seen { thread } => with_gateway(|_, gw| {
            let value = gw.call_json(
                "chat.mark_seen",
                &[("thread_id".to_string(), thread)],
                None,
                None,
                secs(30),
            )?;
            print_json(&value);
            Ok(0)
        }),
        Command::SessionStart { title } => with_gateway(|_, gw| {
            let mut params = serde_json::json!({"origin": "fresh", "lifecycle": "persistent"});
            if let Some(title) = title {
                params
                    .as_object_mut()
                    .map(|map| map.insert("title".to_string(), Value::String(title)));
            }
            let value = gw.call_json(
                "session.start",
                &[],
                Some(&serde_json::json!({"method": "/api/session/start", "params": params})),
                None,
                secs(30),
            )?;
            print_json(&value);
            Ok(0)
        }),
        Command::SessionRename { id, title } => {
            with_gateway(|_, gw| cmd_session_op(gw, "rename", &id, Some(&title)))
        }
        Command::SessionPin { id } => with_gateway(|_, gw| cmd_session_op(gw, "pin", &id, None)),
        Command::SessionUnpin { id } => {
            with_gateway(|_, gw| cmd_session_op(gw, "unpin", &id, None))
        }
        Command::SessionArchive { id } => {
            with_gateway(|_, gw| cmd_session_op(gw, "archive", &id, None))
        }
        Command::SessionUnarchive { id } => {
            with_gateway(|_, gw| cmd_session_op(gw, "unarchive", &id, None))
        }
        Command::SessionDelete { id } => {
            with_gateway(|_, gw| cmd_session_op(gw, "delete", &id, None))
        }
        Command::Wake => cmd_wake(),
        Command::Update => Ok(update::run_update_check_only()),
        Command::Fs { op } => match op {
            Some(FsCommand::Ls {
                path,
                long,
                all,
                json,
            }) => with_gateway(|_, gw| cmd_fs_ls(gw, &path, long, all, json)),
            Some(FsCommand::Tree {
                path,
                depth,
                all,
                json,
            }) => with_gateway(|_, gw| cmd_fs_tree(gw, &path, depth, all, json)),
            Some(FsCommand::Stat { path }) => with_gateway(|_, gw| {
                let mut fs = FsClient::new(gw);
                let value = fs.stat(&path)?;
                print_json(&value);
                Ok(0)
            }),
            Some(FsCommand::Cat {
                path,
                max_bytes,
                binary,
            }) => with_gateway(|_, gw| cmd_fs_cat(gw, &path, max_bytes, binary)),
            Some(FsCommand::Get {
                path,
                output,
                recursive,
            }) => with_gateway(|_, gw| cmd_fs_get(gw, &path, &output, recursive)),
            Some(FsCommand::Find { name, limit }) => with_gateway(|_, gw| {
                let mut fs = FsClient::new(gw);
                let entries = fs.search(&name, limit)?;
                print_json(&Value::Array(entries));
                Ok(0)
            }),
            Some(FsCommand::Put {
                local,
                remote,
                recursive,
                no_clobber,
            }) => with_gateway(|_, gw| {
                let mut fs = FsClient::new(gw);
                let done = fs.put(Path::new(&local), &remote, recursive, !no_clobber)?;
                print_json(&Value::Array(done));
                Ok(0)
            }),
            Some(FsCommand::Write {
                remote,
                append,
                no_clobber,
            }) => {
                // Python reads all of stdin BEFORE connecting.
                let mut data = Vec::new();
                std::io::stdin().read_to_end(&mut data)?;
                with_gateway(|_, gw| {
                    let mut fs = FsClient::new(gw);
                    if append {
                        let mut combined = fs.read_file(&remote, None)?;
                        combined.extend_from_slice(&data);
                        let value = fs.write_file(&remote, &combined, true)?;
                        print_json(&value);
                    } else {
                        let value = fs.write_file(&remote, &data, !no_clobber)?;
                        print_json(&value);
                    }
                    Ok(0)
                })
            }
            Some(FsCommand::Edit { remote, replace }) => {
                let pairs: Vec<(String, String)> = replace
                    .chunks_exact(2)
                    .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
                    .collect();
                with_gateway(|_, gw| cmd_fs_edit(gw, &remote, &pairs))
            }
            Some(FsCommand::Mkdir { path }) => with_gateway(|_, gw| {
                let mut fs = FsClient::new(gw);
                let value = fs.mkdir(&path)?;
                print_json(&value);
                Ok(0)
            }),
            Some(FsCommand::Mv { src, dst, force }) => with_gateway(|_, gw| {
                let mut fs = FsClient::new(gw);
                let value = fs.rename(&src, &dst, force)?;
                print_json(&value);
                Ok(0)
            }),
            Some(FsCommand::Rm {
                path,
                recursive,
                yes,
            }) => {
                // Checked BEFORE connecting, like the original.
                if !yes {
                    eprintln!("refusing to delete without --yes");
                    return Ok(2);
                }
                with_gateway(|_, gw| {
                    let mut fs = FsClient::new(gw);
                    let value = fs.delete(&path, recursive)?;
                    print_json(&value);
                    Ok(0)
                })
            }
            None => Ok(2),
        },
        Command::Raw {
            method,
            body,
            param,
            timeout,
        } => cmd_raw(method, body, param, timeout),
    }
}

// ---------------------------------------------------------------------------
// Simple gateway commands
// ---------------------------------------------------------------------------

fn cmd_status(gw: &mut Gateway) -> Result<i32, CliError> {
    let sess = gw.call_json("sessions.list", &[], None, None, secs(30))?;
    let count = sess
        .get("sessions")
        .and_then(|v| v.as_array())
        .map(|sessions| sessions.len())
        .unwrap_or(0);
    let unread = gw.call_json("chat.unread_count", &[], None, None, secs(30))?;
    let identity = gw.call_json("identity", &[], None, None, secs(30))?;
    print_json(&serde_json::json!({
        "vm_id": gw.vm_id(),
        "sessions": count,
        "unread": unread,
        "identity": identity,
    }));
    Ok(0)
}

fn usage_text_lines(summary: &Value, sub: &Value) -> Vec<String> {
    let head = first_nonempty(&[summary.get("label"), summary.get("plan")])
        .unwrap_or_else(|| "Plan".to_string());
    let mut line = if let Some(value_label) = summary
        .get("value_label")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        format!("{head}: {value_label}")
    } else if let Some(percent) = summary.get("used_percent").filter(|v| !v.is_null()) {
        format!("{head}: {}% used", python_str(percent))
    } else {
        let state = summary
            .get("state")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        format!("{head}: {state}")
    };
    if let Some(resets_at) = summary.get("resets_at").and_then(|v| v.as_str()) {
        let short: String = resets_at.chars().take(16).collect();
        let short = short.replace('T', " ");
        let stamp = sub
            .get("usage")
            .and_then(|u| u.get("resetsAt"))
            .and_then(|v| v.as_f64())
            .map(|secs| secs as i64)
            .unwrap_or(0);
        line += &format!(" (resets {short} {})", tz_abbrev(stamp));
    }
    let mut lines = vec![line];
    if let Some(quota) = summary
        .get("quota_status")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "SUFFICIENT")
    {
        lines.push(format!("  quota: {quota}"));
    }
    match summary.get("bonus_tokens") {
        Some(Value::Object(bonus)) => {
            let label = bonus
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("Additional tokens");
            let value_label = bonus
                .get("value_label")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!("{label}: {value_label} (never expire)"));
        }
        _ => lines.push("Additional tokens: none".to_string()),
    }
    let upgrade = summary.get("upgrade").is_some_and(json_truthy);
    let paid = summary.get("paid").is_some_and(json_truthy);
    if upgrade && !paid {
        lines.push("Upgrade: https://muse.ai/settings".to_string());
    }
    lines
}

fn cmd_usage(json: bool, raw: bool) -> Result<i32, CliError> {
    let mut cfg = load_config()?;
    ensure_cookies(&cfg)?;
    let cookies = load_cookies(&cfg.cookies_file)?;
    let (sub, aid) = fetch_subscription(&cookies, cfg.subscription_action_id.as_deref(), false)?;
    if cfg.subscription_action_id.as_deref() != Some(aid.as_str()) {
        cfg.subscription_action_id = Some(aid);
        // Python writes config.json directly here (not via save_config).
        std::fs::create_dir_all(&cfg.config_dir)?;
        let text = serde_json::to_string_pretty(&cfg)?;
        std::fs::write(cfg.config_file(), text)?;
    }
    if raw {
        print_json(&sub);
        return Ok(0);
    }
    let summary = summarize_usage(&sub);
    if json {
        print_json(&summary);
        return Ok(0);
    }
    for line in usage_text_lines(&summary, &sub) {
        println!("{line}");
    }
    Ok(0)
}

fn invite_text_lines(summary: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if json_truthy(summary.get("code").unwrap_or(&Value::Null)) {
        lines.push(format!(
            "Invite code: {}",
            python_str(summary.get("code").unwrap_or(&Value::Null))
        ));
        if json_truthy(summary.get("link").unwrap_or(&Value::Null)) {
            lines.push(format!(
                "Link: {}",
                python_str(summary.get("link").unwrap_or(&Value::Null))
            ));
        }
        if let Some(remaining) = summary.get("uses_remaining").filter(|v| !v.is_null()) {
            let extra = match summary.get("max_uses").filter(|v| !v.is_null()) {
                Some(max) => format!(" of {}", python_str(max)),
                None => String::new(),
            };
            lines.push(format!("Uses left: {}{}", python_str(remaining), extra));
        }
    } else {
        let state = python_str(summary.get("state").unwrap_or(&Value::Null));
        lines.push(format!(
            "Invite code: none yet (state {state}). Open Invite friends in the muse.ai app once, or run `muse-cli invite --create`."
        ));
    }
    if let Some(Value::Object(reward)) = summary.get("reward") {
        let parts: Vec<&str> = [reward.get("title"), reward.get("body")]
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        lines.push(parts.join(" "));
    }
    if json_truthy(summary.get("offer").unwrap_or(&Value::Null)) {
        lines.push(python_str(summary.get("offer").unwrap_or(&Value::Null)));
    }
    lines
}

fn cmd_invite(json: bool, raw: bool, create: bool) -> Result<i32, CliError> {
    let cfg = load_config()?;
    ensure_cookies(&cfg)?;
    let cookies = load_cookies(&cfg.cookies_file)?;
    let mut data = fetch_invite(&cookies)?;
    if create && !json_truthy(data.get("code").unwrap_or(&Value::Null)) {
        create_invite(&cookies)?;
        data = fetch_invite(&cookies)?;
    }
    if raw {
        print_json(&data);
        return Ok(0);
    }
    let summary = summarize_invite(&data);
    if json {
        print_json(&summary);
        return Ok(0);
    }
    for line in invite_text_lines(&summary) {
        println!("{line}");
    }
    Ok(0)
}

fn cmd_redeem(code: String, json: bool) -> Result<i32, CliError> {
    let cfg = load_config()?;
    ensure_cookies(&cfg)?;
    let cookies = load_cookies(&cfg.cookies_file)?;
    let (status, body) = match redeem_invite_code(&cookies, &code) {
        Ok(pair) => pair,
        Err(CliError::Value(msg)) => {
            // Python catches the validation ValueError in the handler:
            // bare message on stderr, exit 2 (not main's "error: " form).
            eprintln!("{msg}");
            return Ok(2);
        }
        Err(e) => return Err(e),
    };
    let success = body.get("success").is_some_and(json_truthy);
    if json {
        let mut out = serde_json::json!({"status": status});
        if let Some(map) = body.as_object() {
            for (key, value) in map {
                out.as_object_mut()
                    .map(|o| o.insert(key.clone(), value.clone()));
            }
        } else {
            out.as_object_mut()
                .map(|o| o.insert("body".to_string(), body.clone()));
        }
        print_json(&out);
    } else if success {
        let redemption = body
            .get("redemptionStatus")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("ok");
        println!("Redeemed: {redemption}");
    } else {
        let reason = python_str(body.get("reason").unwrap_or(&Value::Null));
        let detail = match body.get("detail") {
            Some(value) if json_truthy(value) => python_str(value),
            _ => String::new(),
        };
        let line = format!("Not redeemed ({reason}): {detail}");
        println!("{}", line.trim_end_matches(&[':', ' '][..]));
    }
    if !success {
        return Ok(1);
    }
    Ok(0)
}

fn cmd_threads(gw: &mut Gateway, archived: bool) -> Result<i32, CliError> {
    let data = gw.call_json("sessions.list", &[], None, None, secs(30))?;
    let sessions = data
        .get("sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for session in &sessions {
        rows.push(serde_json::json!({
            "session_id": session.get("session_id").cloned().unwrap_or(Value::Null),
            "title": session.get("title").cloned().unwrap_or(Value::Null),
            "thread": session.get("is_thread").cloned().unwrap_or(Value::Null),
            "pinned": session.get("pinned").cloned().unwrap_or(Value::Null),
            "archived": session.get("archived").cloned().unwrap_or(Value::Null),
            "updated": local_datetime(
                session
                    .get("updated_at_ms")
                    .and_then(|v| v.as_f64())
                    .map(|ms| (ms / 1000.0) as i64)
                    .unwrap_or(0)
            ),
        }));
    }
    if !archived {
        rows.retain(|row| !row.get("archived").is_some_and(json_truthy));
    }
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_history(
    gw: &mut Gateway,
    thread: Option<String>,
    limit: i64,
    raw: bool,
    download: Option<String>,
) -> Result<i32, CliError> {
    let data = history_response(gw, thread.as_deref(), limit)?;
    let events = data
        .get("chat_events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(dir) = download {
        let resources = dedup_resources(&events);
        let downloads = save_resources(gw, &resources, &dir, false)?;
        if !raw {
            let rows: Vec<Value> = events
                .iter()
                .filter(|event| event_has_text(event))
                .map(fmt_event)
                .collect();
            print_json(&serde_json::json!({"messages": rows, "downloads": downloads}));
            return Ok(0);
        }
        let mut obj = if data.is_object() {
            data
        } else {
            serde_json::json!({"chat_events": events})
        };
        obj.as_object_mut()
            .map(|map| map.insert("downloads".to_string(), Value::Array(downloads)));
        print_json(&obj);
        return Ok(0);
    }
    if raw {
        print_json(&data);
        return Ok(0);
    }
    let rows: Vec<Value> = events
        .iter()
        .filter(|event| event_has_text(event))
        .map(fmt_event)
        .collect();
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_files(gw: &mut Gateway, thread: Option<String>, limit: i64) -> Result<i32, CliError> {
    let events = history_events(gw, thread.as_deref(), limit)?;
    let mut rows = Vec::new();
    for event in &events {
        let payload = event.get("payload").filter(|v| v.is_object());
        let message_id = first_truthy(&[
            payload.and_then(|p| p.get("message_id")),
            event.get("message_id"),
        ]);
        for resource in event_resources(event) {
            let mut row = serde_json::json!({
                "seq": event.get("seq").cloned().unwrap_or(Value::Null),
                "message_id": message_id,
            });
            if let Some(map) = resource.as_object() {
                for (key, value) in map {
                    row.as_object_mut()
                        .map(|o| o.insert(key.clone(), value.clone()));
                }
            }
            rows.push(row);
        }
    }
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_download(
    gw: &mut Gateway,
    reference: String,
    output: String,
    thread: Option<String>,
    limit: i64,
    force: bool,
) -> Result<i32, CliError> {
    let resources: Vec<Value> =
        if reference.starts_with("sandbox://") || reference.contains("/media/raw/") {
            vec![serde_json::json!({"path": reference})]
        } else {
            let events = history_events(gw, thread.as_deref(), limit)?;
            let mut resources = Vec::new();
            let mut seen: Vec<String> = Vec::new();
            for event in &events {
                if event_matches_ref(event, &reference) {
                    for resource in event_resources(event) {
                        let path = resource
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !seen.contains(&path) {
                            seen.push(path);
                            resources.push(resource);
                        }
                    }
                }
            }
            resources
        };
    if resources.is_empty() {
        let scope = match &thread {
            Some(t) => format!(" of thread {t}"),
            None => " of the main chat".to_string(),
        };
        eprintln!(
            "no files found for '{reference}' in the last {limit} messages{scope} (try --thread / --limit)"
        );
        return Ok(1);
    }
    let rows = save_resources(gw, &resources, &output, force)?;
    print_json(&Value::Array(rows.clone()));
    if rows.iter().any(|row| row.get("error").is_some()) {
        return Ok(3);
    }
    Ok(0)
}

/// One watch row, or `None` when the line is not JSON or is a
/// filtered-out `agent.status` event. Mirrors `_print_watch_row`
/// (text fields truncated to 300 characters).
pub(crate) fn watch_row(line: &str, show_all: bool) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    let event_name = if event.as_object().is_some_and(|m| m.contains_key("event")) {
        event.get("event").cloned().unwrap_or(Value::Null)
    } else {
        event.get("type").cloned().unwrap_or(Value::Null)
    };
    if event_name.as_str() == Some("agent.status") && !show_all {
        return None;
    }
    let payload = event.get("payload").filter(|v| v.is_object());
    let mut row = serde_json::json!({
        "event": event_name,
        "seq": event.get("seq").cloned().unwrap_or(Value::Null),
    });
    for key in ["display_text", "content", "text", "activity_text", "status"] {
        if let Some(value) = payload.and_then(|p| p.get(key)) {
            if json_truthy(value) {
                let text = python_str(value);
                let truncated: String = text.chars().take(300).collect();
                row.as_object_mut()
                    .map(|map| map.insert(key.to_string(), Value::String(truncated)));
            }
        }
    }
    serde_json::to_string(&row)
        .ok()
        .map(|compact| python_json_separators(&compact))
}

/// Re-space compact JSON to Python `json.dumps` default separators
/// (`', '` and `': '`): a space is inserted after every `,` and `:`
/// that is NOT inside a JSON string. The in-string tracker honours
/// backslash escapes, so `,`/`:` (and `\"`) inside string values are
/// left exactly as serde wrote them — no space is ever injected into
/// a value.
pub(crate) fn python_json_separators(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + compact.len() / 4);
    let mut in_string = false;
    let mut escaped = false;
    for c in compact.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                ',' | ':' => {
                    out.push(c);
                    out.push(' ');
                }
                _ => out.push(c),
            }
        }
    }
    out
}

fn cmd_watch(
    gw: &mut Gateway,
    timeout: i64,
    n: i64,
    all: bool,
    thread: Option<String>,
) -> Result<i32, CliError> {
    let mut body = serde_json::json!({"capabilities": ["chat_cancel", "delta_stream"]});
    if let Some(thread) = thread {
        body.as_object_mut()
            .map(|map| map.insert("session_id".to_string(), Value::String(thread)));
    }
    let mut on_record = |line: &str| -> bool {
        if let Some(row) = watch_row(line, all) {
            println!("{row}");
            let _ = std::io::stdout().flush();
        }
        false
    };
    gw.subscribe_records(
        "chat.subscribe",
        &[],
        Some(&body),
        None,
        secs(timeout),
        secs(timeout),
        n.max(0) as usize,
        &mut on_record,
    )?;
    Ok(0)
}

/// Flatten `feed.list` days/editions/units, mirroring `cmd_feed`.
pub(crate) fn flatten_feed(data: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    let days = data
        .get("days")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for day in &days {
        let editions = day
            .get("editions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for edition in &editions {
            let units = edition
                .get("units")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for unit in &units {
                rows.push(serde_json::json!({
                    "id": unit.get("unit_id").cloned().unwrap_or(Value::Null),
                    "title": unit.get("title").cloned().unwrap_or(Value::Null),
                    "date": day.get("local_date").cloned().unwrap_or(Value::Null),
                    "edition": edition.get("kind").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    rows
}

/// Python `rows[:limit] if limit else rows` (limit 0/None = no cut;
/// a negative limit drops that many rows from the end).
/// `unsigned_abs` (not `-limit`, which overflows on `i64::MIN` in
/// debug builds) makes `i64::MIN` behave like the drop-everything
/// branch and return an empty list.
pub(crate) fn apply_feed_limit(rows: Vec<Value>, limit: Option<i64>) -> Vec<Value> {
    match limit {
        None | Some(0) => rows,
        Some(limit) if limit > 0 => rows.into_iter().take(limit as usize).collect(),
        Some(limit) => {
            let drop = limit.unsigned_abs();
            let len = rows.len();
            if drop >= len as u64 {
                Vec::new()
            } else {
                rows.into_iter().take(len - drop as usize).collect()
            }
        }
    }
}

fn cmd_feed(gw: &mut Gateway, limit: Option<i64>, raw: bool) -> Result<i32, CliError> {
    let query = match limit {
        Some(limit) if limit != 0 => Some(serde_json::json!({"limit": limit})),
        _ => None,
    };
    let data = gw.call_json("feed.list", &[], None, query.as_ref(), secs(30))?;
    if raw {
        print_json(&data);
        return Ok(0);
    }
    let rows = apply_feed_limit(flatten_feed(&data), limit);
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_goals(gw: &mut Gateway) -> Result<i32, CliError> {
    let data = gw.call_json("goals.list", &[], None, None, secs(30))?;
    let goals = data
        .get("goals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = goals
        .iter()
        .map(|goal| {
            serde_json::json!({
                "id": first_truthy(&[goal.get("id"), goal.get("goal_id")]),
                "title": goal.get("title").cloned().unwrap_or(Value::Null),
                "status": goal.get("status").cloned().unwrap_or(Value::Null),
                "subtitle": goal.get("subtitle").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_ideas(gw: &mut Gateway) -> Result<i32, CliError> {
    let data = gw.call_json("api.idea-cards.list", &[], None, None, secs(30))?;
    let sections = data
        .get("sections")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for section in &sections {
        let cards = section
            .get("cards")
            .and_then(|v| v.as_array())
            .or_else(|| section.get("ideas").and_then(|v| v.as_array()))
            .cloned()
            .unwrap_or_default();
        for card in &cards {
            rows.push(serde_json::json!({
                "id": first_truthy(&[card.get("id"), card.get("ideaCardId")]),
                "title": card.get("title").cloned().unwrap_or(Value::Null),
                "section": section.get("title").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    print_json(&Value::Array(rows));
    Ok(0)
}

fn cmd_session_op(
    gw: &mut Gateway,
    kind: &str,
    id: &str,
    title: Option<&str>,
) -> Result<i32, CliError> {
    let body = if kind == "rename" {
        serde_json::json!({"session_id": id, "title": title.unwrap_or("")})
    } else {
        serde_json::json!({"method": format!("/api/session/{kind}"), "session_id": id})
    };
    let value = gw.call_json(&format!("session.{kind}"), &[], Some(&body), None, secs(30))?;
    print_json(&value);
    Ok(0)
}

fn cmd_wake() -> Result<i32, CliError> {
    // HTTPS only: never Gateway::connect (Noise). A missing cookies
    // file is the normalised Auth error (exit 2), per CONTRACT.
    let cfg = load_config()?;
    let cookies = load_cookies(&cfg.cookies_file)?;
    let result = wake_vm(&cookies, cfg.vm_id.as_deref())?;
    print_json(&result);
    Ok(0)
}

fn cmd_raw(
    method: String,
    body: Option<String>,
    param: Vec<String>,
    timeout: i64,
) -> Result<i32, CliError> {
    // Lookup (and input validation) BEFORE connecting, like Python.
    if routes::lookup(&method).is_err() {
        eprintln!(
            "unknown method '{method}' (see routes.json for the {} known methods)",
            routes::routes().len()
        );
        return Ok(2);
    }
    // Python: `json.loads(body) if body else None` — an empty
    // `--body ""` means "no body", not invalid JSON.
    let body: Option<Value> = match body {
        None => None,
        Some(text) if text.is_empty() => None,
        Some(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Null) => None,
            Ok(value) => Some(value),
            Err(e) => {
                eprintln!("invalid --body JSON: {e}");
                return Ok(2);
            }
        },
    };
    let mut path_params: Vec<(String, String)> = Vec::new();
    for kv in &param {
        match kv.split_once('=') {
            Some((key, value)) => {
                path_params.retain(|(existing, _)| existing != key);
                path_params.push((key.to_string(), value.to_string()));
            }
            None => {
                eprintln!("bad --param '{kv}': expected k=v");
                return Ok(2);
            }
        }
    }
    with_gateway(|_, gw| {
        let (_, data) = gw.request(&method, &path_params, body.as_ref(), None, secs(timeout))?;
        if data.is_empty() {
            print_json(&serde_json::json!({}));
            return Ok(0);
        }
        match serde_json::from_slice::<Value>(&data) {
            Ok(value) => print_json(&value),
            // Kept quirk: binary through raw is lossy-decoded, not
            // written to a file.
            Err(_) => println!("{}", String::from_utf8_lossy(&data)),
        }
        Ok(0)
    })
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

/// First non-empty list among the keys (Python `or` fallback chain).
fn pick_list(value: &Value, keys: &[&str]) -> Vec<Value> {
    let mut fallback: Option<Vec<Value>> = None;
    for key in keys {
        if let Some(Value::Array(items)) = value.get(*key) {
            if !items.is_empty() {
                return items.clone();
            }
            if fallback.is_none() {
                fallback = Some(items.clone());
            }
        }
    }
    fallback.unwrap_or_default()
}

fn pending(gw: &mut Gateway) -> Result<(Value, Vec<Value>), CliError> {
    let response = gw.call_json("egress.approvals", &[], None, None, secs(30))?;
    let pend = pick_list(&response, &["pending_approvals", "pending"]);
    Ok((response, pend))
}

fn decide(gw: &mut Gateway, approval_id: &str, decision: &str) -> Result<Value, CliError> {
    gw.call_json(
        "egress.approval.decide",
        &[("approval_id".to_string(), approval_id.to_string())],
        Some(&serde_json::json!({"decision": decision})),
        None,
        secs(30),
    )
}

fn display_or_none(value: Option<&Value>) -> String {
    match value {
        None => "None".to_string(),
        Some(value) => python_str(value),
    }
}

/// Decide every currently pending approval; mirrors `approve_all`.
/// `allow_always` skips approvals without a forever option (stderr,
/// no failure); other decisions never skip.
fn approve_all(gw: &mut Gateway, decision: &str) -> Result<Vec<Value>, CliError> {
    let (_, pend) = pending(gw)?;
    let mut done = Vec::new();
    for approval in &pend {
        let described = describe_approval(approval);
        let id = described
            .get("approval_id")
            .and_then(|v| v.as_str())
            .unwrap_or("None")
            .to_string();
        if decision == "allow_always" && !offers_forever(&described) {
            let options = match described.get("options") {
                Some(Value::Array(options)) => format!(
                    "[{}]",
                    options
                        .iter()
                        .map(|o| match o {
                            Value::String(s) => python_repr(s),
                            other => python_str(other),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => "None".to_string(),
            };
            eprintln!("skipped {id}: no forever option {options}");
            continue;
        }
        decide(gw, &id, decision)?;
        let row = serde_json::json!({
            "approval_id": described.get("approval_id").cloned().unwrap_or(Value::Null),
            "title": described.get("title").cloned().unwrap_or(Value::Null),
            "kind": described.get("kind").cloned().unwrap_or(Value::Null),
            "scope": described.get("scope").cloned().unwrap_or(Value::Null),
            "decision": decision,
        });
        eprintln!(
            "approved {id} ({decision}): {} [{} {}]",
            display_or_none(described.get("title")),
            display_or_none(described.get("kind")),
            display_or_none(described.get("scope")),
        );
        done.push(row);
    }
    Ok(done)
}

fn cmd_approvals(
    gw: &mut Gateway,
    recent: bool,
    raw: bool,
    approve_all_flag: bool,
) -> Result<i32, CliError> {
    if approve_all_flag {
        let done = approve_all(gw, "allow_always")?;
        print_json(&serde_json::json!({"approved": done}));
        return Ok(0);
    }
    let (response, pend) = pending(gw)?;
    if raw {
        print_json(&response);
        return Ok(0);
    }
    let mut obj = serde_json::json!({
        "pending": pend.iter().map(describe_approval).collect::<Vec<Value>>(),
    });
    if recent {
        let recents = pick_list(&response, &["recent_approvals", "recent"]);
        let rows: Vec<Value> = recents
            .iter()
            .map(|approval| {
                let mut described = describe_approval(approval);
                described.as_object_mut().map(|map| {
                    map.insert(
                        "decision".to_string(),
                        approval.get("decision").cloned().unwrap_or(Value::Null),
                    )
                });
                described
            })
            .collect();
        obj.as_object_mut()
            .map(|map| map.insert("recent".to_string(), Value::Array(rows)));
    }
    print_json(&obj);
    Ok(0)
}

fn cmd_approve(
    gw: &mut Gateway,
    id: Option<String>,
    all: bool,
    decision: &str,
) -> Result<i32, CliError> {
    if all {
        let done = approve_all(gw, decision)?;
        print_json(&serde_json::json!({"approved": done}));
    } else {
        // The id-or---all check ran in dispatch before connecting, so
        // `id` is present here.
        let id = id.unwrap_or_default();
        let value = decide(gw, &id, decision)?;
        print_json(&value);
    }
    Ok(0)
}

fn cmd_auto_approve(state: AutoApproveState) -> Result<i32, CliError> {
    let mut cfg = load_config()?;
    match state {
        AutoApproveState::On | AutoApproveState::Always => {
            cfg.auto_approve = Some("always".to_string());
            save_config(&cfg)?;
        }
        AutoApproveState::Off => {
            cfg.auto_approve = Some("off".to_string());
            save_config(&cfg)?;
        }
        AutoApproveState::Status => {}
    }
    let (enabled, source) = auto_approve_setting_source(&cfg, None)?;
    let config_value = match cfg.auto_approve.as_deref() {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => "off".to_string(),
    };
    let env = std::env::var("MUSE_AUTO_APPROVE")
        .ok()
        .map(Value::String)
        .unwrap_or(Value::Null);
    print_json(&serde_json::json!({
        "auto_approve": if enabled { "always" } else { "off" },
        "source": source,
        "config": config_value,
        "env": env,
    }));
    Ok(0)
}

// ---------------------------------------------------------------------------
// Wait / send output
// ---------------------------------------------------------------------------

fn outcome_str(kind: WaitOutcomeKind) -> &'static str {
    match kind {
        WaitOutcomeKind::Done => "done",
        WaitOutcomeKind::Failed => "failed",
        WaitOutcomeKind::NeedsInput => "needs_input",
        WaitOutcomeKind::NeedsApproval => "needs_approval",
        WaitOutcomeKind::Timeout => "timeout",
    }
}

/// Shared JSON shape for `wait` (and the wait part of `send`),
/// mirroring Python `run_wait`'s result dict. Every field comes
/// from `WaitResult` itself — no history re-read here.
/// `after_seq` is included only for `wait` (Python's send result has
/// no `after_seq` key).
fn wait_result_json(result: &WaitResult, after_seq: Option<i64>, auto_enabled: bool) -> Value {
    let mut obj = serde_json::json!({
        "outcome": outcome_str(result.kind),
        "status": result.status.clone().map(Value::String).unwrap_or(Value::Null),
        "final_text": result
            .final_text
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "replies": result.replies,
    });
    if let Some(after_seq) = after_seq {
        obj.as_object_mut()
            .map(|map| map.insert("after_seq".to_string(), Value::from(after_seq)));
    }
    if result.kind == WaitOutcomeKind::NeedsApproval {
        obj.as_object_mut().map(|map| {
            map.insert(
                "approvals".to_string(),
                Value::Array(result.approvals.clone()),
            )
        });
    }
    if auto_enabled {
        obj.as_object_mut().map(|map| {
            map.insert(
                "auto_approved".to_string(),
                Value::Array(result.auto_approved.clone()),
            )
        });
        if !result.auto_approve_errors.is_empty() {
            obj.as_object_mut().map(|map| {
                map.insert(
                    "auto_approve_errors".to_string(),
                    Value::Array(result.auto_approve_errors.clone()),
                )
            });
        }
    }
    if !result.widgets.is_empty() {
        obj.as_object_mut()
            .map(|map| map.insert("widgets".to_string(), Value::Array(result.widgets.clone())));
    }
    if result.kind == WaitOutcomeKind::NeedsInput {
        if let Some(reason) = &result.needs_input_reason {
            obj.as_object_mut().map(|map| {
                map.insert(
                    "needs_input_reason".to_string(),
                    Value::String(reason.clone()),
                )
            });
        }
        if let Some(question) = &result.question {
            obj.as_object_mut()
                .map(|map| map.insert("question".to_string(), Value::String(question.clone())));
        }
    }
    obj
}

/// Text form of a wait result, mirroring `_print_wait`. `status` is
/// the last task status seen; `None` prints as `None`, like Python's
/// `f"(status={res['status']})"` when `w.status` is `None`.
fn print_wait_text(
    replies: &[Value],
    question: Option<&str>,
    reason: Option<&str>,
    approvals: &[Value],
    outcome: &str,
    status: Option<&str>,
) {
    for reply in replies {
        let seq = reply.get("seq").map(python_str).unwrap_or_default();
        let text = reply.get("text").map(python_str).unwrap_or_default();
        println!("[{seq}] {text}");
        if let Some(Value::Array(widgets)) = reply.get("widgets") {
            for widget in widgets {
                let options = widget
                    .get("options")
                    .and_then(|v| v.as_array())
                    .map(|opts| opts.iter().map(python_str).collect::<Vec<_>>().join(" / "))
                    .unwrap_or_default();
                let widget_id = widget.get("widget_id").map(python_str).unwrap_or_default();
                let widget_text = widget.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if options.is_empty() {
                    println!("  ? widget {widget_id}: {widget_text}");
                } else {
                    println!("  ? widget {widget_id}: {widget_text} [{options}]");
                }
            }
        }
    }
    if let Some(question) = question.filter(|q| !q.is_empty()) {
        println!("NEEDS INPUT ({}): {question}", reason.unwrap_or(""));
    }
    for approval in approvals {
        println!(
            "NEEDS APPROVAL {}: {} - {}",
            display_or_none(approval.get("approval_id")),
            display_or_none(approval.get("title")),
            display_or_none(approval.get("question")),
        );
        if let Some(why) = approval.get("why").filter(|v| json_truthy(v)) {
            println!("  why: {}", python_str(why));
        }
    }
    eprintln!("-- {outcome} (status={})", status.unwrap_or("None"));
}

/// Question + reason for text output, straight from a `NeedsInput`
/// result (no recomputation, no re-read).
fn wait_question_reason(result: &WaitResult) -> (Option<String>, Option<String>) {
    if result.kind != WaitOutcomeKind::NeedsInput {
        return (None, None);
    }
    (result.question.clone(), result.needs_input_reason.clone())
}

#[allow(clippy::too_many_arguments)]
fn cmd_wait(
    cfg: &Config,
    gw: &mut Gateway,
    thread: Option<String>,
    after_seq: Option<i64>,
    timeout: i64,
    json: bool,
    require_running: bool,
    flag: Option<bool>,
) -> Result<i32, CliError> {
    let auto = auto_approve_setting(cfg, flag)?;
    let mut factory = || {
        let fresh_cfg = load_config()?;
        Gateway::connect(&fresh_cfg)
    };
    let result = run_wait(
        gw,
        thread.clone(),
        after_seq,
        secs(timeout),
        require_running,
        auto,
        &mut factory,
    )?;
    let outcome = outcome_str(result.kind);
    if json {
        print_json(&wait_result_json(&result, Some(result.after_seq), auto));
    } else {
        let (question, reason) = wait_question_reason(&result);
        let approvals: &[Value] = if result.kind == WaitOutcomeKind::NeedsApproval {
            &result.approvals
        } else {
            &[]
        };
        print_wait_text(
            &result.replies,
            question.as_deref(),
            reason.as_deref(),
            approvals,
            outcome,
            result.status.as_deref(),
        );
    }
    Ok(result.exit_code)
}

#[allow(clippy::too_many_arguments)]
fn cmd_send(
    cfg: &Config,
    gw: &mut Gateway,
    text: String,
    thread: Option<String>,
    wait: i64,
    until_done: bool,
    text_output: bool,
    ack_timeout: i64,
    download: Option<String>,
    flag: Option<bool>,
) -> Result<i32, CliError> {
    let mut params = serde_json::json!({
        "items": [{"type": "text", "text": text}],
        "node_id": cfg.node_id,
        "capabilities": {},
    });
    if let Some(thread) = &thread {
        params
            .as_object_mut()
            .map(|map| map.insert("session_id".to_string(), Value::String(thread.clone())));
    }
    // Baseline, swallowing only Gateway/Timeout errors like Python.
    let mut baseline = 0i64;
    match history_events(gw, thread.as_deref(), 1) {
        Ok(events) => {
            baseline = events
                .iter()
                .map(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0))
                .max()
                .unwrap_or(0);
        }
        Err(CliError::Gateway { .. } | CliError::Timeout(_)) => {}
        Err(e) => return Err(e),
    }
    let stream_sid = gw.open_stream("chat.stream", &[], Some(&params), None)?;
    // Delivery ack loop (poll history every 1s).
    let mut user_seq: Option<i64> = None;
    let ack_deadline = Instant::now() + secs(ack_timeout);
    while user_seq.is_none() && Instant::now() < ack_deadline {
        match history_events(gw, thread.as_deref(), 10) {
            Ok(events) => {
                user_seq = user_ack(&events, baseline, Some(&text));
            }
            Err(CliError::Gateway { .. } | CliError::Timeout(_)) => {}
            Err(e) => return Err(e),
        }
        if user_seq.is_none() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let Some(user_seq) = user_seq else {
        print_json(&serde_json::json!({
            "sent": true,
            "delivered": false,
            "user_seq": Value::Null,
            "stream": stream_sid,
            "note": format!("no delivery ack within {ack_timeout}s; check `muse-cli history`"),
        }));
        return Ok(5);
    };
    let wait_s = if until_done { 24 * 3600 } else { wait };
    if wait_s == 0 {
        print_json(&serde_json::json!({
            "sent": true,
            "delivered": true,
            "user_seq": user_seq,
            "stream": stream_sid,
        }));
        return Ok(0);
    }
    // Auto-approve is resolved here (after the ack), like Python's
    // run_wait resolving it after _send_finish's ack phase.
    let auto = auto_approve_setting(cfg, flag)?;
    let mut factory = || {
        let fresh_cfg = load_config()?;
        Gateway::connect(&fresh_cfg)
    };
    let result = run_wait(
        gw,
        thread.clone(),
        Some(user_seq),
        secs(wait_s),
        true,
        auto,
        &mut factory,
    )?;
    let outcome = outcome_str(result.kind);
    let mut combined = wait_result_json(&result, None, auto);
    combined.as_object_mut().map(|map| {
        map.insert("sent".to_string(), Value::Bool(true));
        map.insert("delivered".to_string(), Value::Bool(true));
        map.insert("user_seq".to_string(), Value::from(user_seq));
        map.insert("stream".to_string(), Value::from(stream_sid));
    });
    if let Some(last) = result.replies.last() {
        combined.as_object_mut().map(|map| {
            map.insert(
                "reply".to_string(),
                serde_json::json!({
                    "seq": last.get("seq").cloned().unwrap_or(Value::Null),
                    "role": "message.assistant",
                    "message_id": last.get("message_id").cloned().unwrap_or(Value::Null),
                    "text": last.get("text").cloned().unwrap_or(Value::Null),
                }),
            )
        });
    }
    if let Some(dir) = &download {
        if !result.replies.is_empty() {
            let mut dl_gw = Gateway::connect(cfg)?;
            let download_result = (|| -> Result<Vec<Value>, CliError> {
                let history = history_events(&mut dl_gw, thread.as_deref(), 30)?;
                let mut found: Vec<Value> = Vec::new();
                let mut seen: Vec<String> = Vec::new();
                for event in &history {
                    if event.get("seq").and_then(|v| v.as_i64()).unwrap_or(0) > user_seq {
                        for resource in event_resources(event) {
                            let path = resource
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !seen.contains(&path) {
                                seen.push(path);
                                found.push(resource);
                            }
                        }
                    }
                }
                save_resources(&mut dl_gw, &found, dir, false)
            })();
            dl_gw.close();
            match download_result {
                Ok(rows) => {
                    combined
                        .as_object_mut()
                        .map(|map| map.insert("downloads".to_string(), Value::Array(rows)));
                }
                Err(e @ (CliError::Gateway { .. } | CliError::Timeout(_))) => {
                    combined.as_object_mut().map(|map| {
                        map.insert(
                            "download_error".to_string(),
                            Value::String(cli_error_message(&e)),
                        )
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }
    if outcome == "timeout" {
        combined.as_object_mut().map(|map| {
            map.insert(
                "note".to_string(),
                Value::String(format!(
                    "turn not finished within {wait_s}s; run `muse-cli wait --after-seq {user_seq}`"
                )),
            )
        });
    }
    if text_output {
        println!("user_seq={user_seq}");
        let (question, reason) = wait_question_reason(&result);
        let approvals: &[Value] = if result.kind == WaitOutcomeKind::NeedsApproval {
            &result.approvals
        } else {
            &[]
        };
        print_wait_text(
            &result.replies,
            question.as_deref(),
            reason.as_deref(),
            approvals,
            outcome,
            result.status.as_deref(),
        );
    } else {
        print_json(&combined);
    }
    Ok(result.exit_code)
}

// ---------------------------------------------------------------------------
// fs commands
// ---------------------------------------------------------------------------

fn cmd_fs_ls(
    gw: &mut Gateway,
    path: &str,
    long: bool,
    all: bool,
    json: bool,
) -> Result<i32, CliError> {
    let mut fs = FsClient::new(gw);
    let mut entries = fs.list_dir(path)?;
    if !all {
        entries.retain(|entry| {
            !entry
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name.starts_with('.'))
        });
    }
    if json {
        print_json(&Value::Array(entries));
        return Ok(0);
    }
    for entry in &entries {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let suffix = if entry.get("type").and_then(|v| v.as_str()) == Some("directory") {
            "/"
        } else {
            ""
        };
        if long {
            let type_char = entry
                .get("type")
                .and_then(|v| v.as_str())
                .and_then(|t| t.chars().next())
                .unwrap_or('?');
            let size = fmt_size(entry.get("size").unwrap_or(&Value::Null));
            let modified: String = entry
                .get("modifiedAt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(19)
                .collect();
            println!("{type_char} {size:>7} {modified:<19} {name}{suffix}");
        } else {
            println!("{name}{suffix}");
        }
    }
    Ok(0)
}

fn cmd_fs_tree(
    gw: &mut Gateway,
    path: &str,
    depth: i64,
    all: bool,
    json: bool,
) -> Result<i32, CliError> {
    let mut fs = FsClient::new(gw);
    let rows = fs.walk(path, Some(depth))?;
    let rows: Vec<(String, Value, i64)> = rows
        .into_iter()
        .filter(|(rel, _, _)| all || !rel.split('/').any(|part| part.starts_with('.')))
        .collect();
    if json {
        // Python: `{"path": r, **e}` — the entry's own `path` key
        // wins when present; only insert when it is absent.
        let out: Vec<Value> = rows
            .iter()
            .map(|(rel, entry, _)| {
                let mut obj = entry.clone();
                obj.as_object_mut().map(|map| {
                    map.entry("path".to_string())
                        .or_insert_with(|| Value::String(rel.clone()));
                });
                obj
            })
            .collect();
        print_json(&Value::Array(out));
        return Ok(0);
    }
    println!("{}", if path.is_empty() { "/" } else { path });
    for (_, entry, level) in &rows {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let suffix = if entry.get("type").and_then(|v| v.as_str()) == Some("directory") {
            "/"
        } else {
            ""
        };
        println!("{}{name}{suffix}", "  ".repeat((level + 1).max(0) as usize));
    }
    Ok(0)
}

fn cmd_fs_cat(
    gw: &mut Gateway,
    path: &str,
    max_bytes: Option<i64>,
    binary: bool,
) -> Result<i32, CliError> {
    let mut fs = FsClient::new(gw);
    let data = fs.read_file(path, max_bytes)?;
    let head_len = data.len().min(8192);
    if data[..head_len].contains(&0) && !binary {
        eprintln!(
            "{path}: binary file ({} bytes); use `fs get` or --binary",
            data.len()
        );
        return Ok(1);
    }
    let mut stdout = std::io::stdout();
    stdout.write_all(&data)?;
    stdout.flush()?;
    Ok(0)
}

fn cmd_fs_get(
    gw: &mut Gateway,
    path: &str,
    output: &str,
    recursive: bool,
) -> Result<i32, CliError> {
    // Python creates the output dir inside the command (after connect).
    std::fs::create_dir_all(output)?;
    let mut fs = FsClient::new(gw);
    let saved = fs.get(path, Path::new(output), recursive)?;
    print_json(&serde_json::json!({"saved": saved}));
    Ok(0)
}

fn editor_suffix(remote: &str) -> String {
    let name = remote.rsplit('/').next().unwrap_or("");
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(index) => name[index..].to_string(),
    }
}

fn cmd_fs_edit(
    gw: &mut Gateway,
    remote: &str,
    pairs: &[(String, String)],
) -> Result<i32, CliError> {
    let mut fs = FsClient::new(gw);
    let before = fs.read_file(remote, None)?;
    let after = if !pairs.is_empty() {
        let mut text = String::from_utf8(before.clone())
            .map_err(|e| CliError::value(format!("invalid utf-8: {e}")))?;
        for (old, new) in pairs {
            text = apply_replace(&text, old, new)?;
        }
        text.into_bytes()
    } else {
        let suffix = editor_suffix(remote);
        edit_in_editor(&before, &suffix)?
    };
    if after == before {
        print_json(&serde_json::json!({"path": remote, "changed": false}));
        return Ok(0);
    }
    let mut result = fs.write_file(remote, &after, true)?;
    result
        .as_object_mut()
        .map(|map| map.insert("changed".to_string(), Value::Bool(true)));
    print_json(&result);
    Ok(0)
}

// ---------------------------------------------------------------------------
// auth export (mirrors cli.py:cmd_auth_export + _browser_* helpers)
// ---------------------------------------------------------------------------

fn which(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(program);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return Some(candidate);
                }
                continue;
            }
        }
        #[cfg(not(unix))]
        {
            return Some(candidate);
        }
    }
    None
}

fn set_mode_600(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Write the cookies file securely (fix round 2, SAFETY F1).
///
/// The original (`cli.py:cmd_auth_export`, lines 331-334) renames
/// the old file to `.bak`, writes the new file with the process
/// umask, and only then chmods it 600 — and never chmods or
/// removes the `.bak`. This version is deliberately stricter (and
/// better) than the original:
/// - if an old file exists it is renamed to `.bak` and the `.bak`
///   is set to mode 600 immediately, so a stale backup never sits
///   at the old file's looser mode;
/// - the new file is created with mode 600 from the start (Unix
///   `OpenOptions::mode(0o600)` + `create_new`, never `fs::write`
///   followed by a chmod, so there is no umask window, and a
///   planted symlink at the destination is refused rather than
///   followed);
/// - only after a successful write is the `.bak` removed; if the
///   write fails, the `.bak` is left at mode 600 for manual
///   recovery (the `Err` arm below deliberately does not touch it).
fn write_cookies_file_secure(path: &Path, contents: &str) -> Result<(), CliError> {
    let backup = PathBuf::from(format!("{}.bak", path.display()));
    let had_old = std::fs::symlink_metadata(path).is_ok();
    if had_old {
        std::fs::rename(path, &backup)?;
        if let Err(e) = set_mode_600(&backup) {
            // Restore the original rather than leaving a backup
            // whose mode we could not secure.
            let _ = std::fs::rename(&backup, path);
            return Err(e);
        }
    }
    match write_new_file_600(path, contents) {
        Ok(()) => {
            if had_old {
                let _ = std::fs::remove_file(&backup);
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Create `path` with mode 600 and write `contents` into it.
/// `create_new` refuses an already-existing path (including a
/// planted symlink) instead of following or truncating it.
#[cfg(unix)]
fn write_new_file_600(path: &Path, contents: &str) -> Result<(), CliError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_new_file_600(path: &Path, contents: &str) -> Result<(), CliError> {
    std::fs::write(path, contents)?;
    set_mode_600(path)
}

fn print_hand_copy() {
    eprintln!("Copy the login by hand instead:");
    eprintln!("  1. In Chrome, open https://muse.ai/ and log in.");
    eprintln!("  2. DevTools (F12) → Application → Cookies → https://muse.ai");
    eprintln!("  3. Write one line to ~/.config/muse-cli/cookies.txt");
    eprintln!("       hatch_sess=VALUE; other_name=other_value");
    eprintln!("     hatch_sess is required. Separate cookies with '; '.");
    eprintln!("  4. chmod 600 ~/.config/muse-cli/cookies.txt");
    eprintln!("  5. muse-cli status");
}

fn print_chrome_debug_steps(cookies_file: &Path) {
    eprintln!("Turn on remote debugging so muse-cli can read the muse.ai cookies.");
    eprintln!("Do this in the Chrome window where you already use muse.ai:");
    eprintln!();
    eprintln!("  1. Open chrome://inspect/#remote-debugging");
    eprintln!("  2. Turn on remote debugging");
    eprintln!("     (\"Allow remote debugging for this browser instance\").");
    eprintln!("  3. Open https://muse.ai/ and log in. Leave that tab open.");
    eprintln!("  4. Run `muse-cli auth export` again.");
    eprintln!("     If Chrome asks to allow the connection, click Allow.");
    eprintln!("     If this command already failed, run it again after Allow.");
    eprintln!();
    eprintln!("Stay in that same window. A second Chrome started with");
    eprintln!("--remote-debugging-port is another profile and has no muse.ai login.");
    eprintln!();
    eprintln!("Success looks like:");
    eprintln!("  saved N muse.ai cookies to {}", cookies_file.display());
    eprintln!();
    print_hand_copy();
}

fn print_need_agent_browser() {
    eprintln!("agent-browser is not installed, so muse-cli cannot read Chrome's cookies.");
    eprintln!();
    eprintln!("  1. Install Node.js LTS if `node --version` fails: https://nodejs.org");
    eprintln!("  2. Open a new terminal, then:");
    eprintln!("       npm i -g agent-browser");
    eprintln!("       agent-browser --version");
    eprintln!("  3. Run `muse-cli auth export` again and follow the Chrome steps it prints.");
    eprintln!();
    print_hand_copy();
}

fn export_error_kind(err: &str) -> &'static str {
    let lower = err.to_lowercase();
    if lower.contains("daemon already running") {
        return "daemon";
    }
    if lower.contains("no running chrome")
        || lower.contains("remote-debugging")
        || lower.contains("auto-launch failed")
        || lower.contains("auto-connect")
    {
        return "debug";
    }
    if err == "no muse.ai tab open in Chrome" {
        return "tab";
    }
    if lower.contains("https://muse.ai")
        || lower.starts_with("✓")
        || lower.starts_with("connected to chrome")
    {
        return "connected";
    }
    "other"
}

fn print_export_failure(err: &str, cookies_file: &Path) {
    eprintln!("cookie export failed.");
    match export_error_kind(err) {
        "daemon" => {
            eprintln!(
                "agent-browser is still running from an earlier attempt. Close it, then retry:"
            );
            eprintln!("  agent-browser close");
            eprintln!("  muse-cli auth export");
            eprintln!();
            print_chrome_debug_steps(cookies_file);
        }
        "debug" => print_chrome_debug_steps(cookies_file),
        "tab" => {
            eprintln!("Chrome is connected, but no muse.ai tab is open.");
            eprintln!("Open https://muse.ai/, log in, leave that tab open, and run");
            eprintln!("`muse-cli auth export` again.");
        }
        "connected" => {
            eprintln!("Chrome is connected and the muse.ai tab is open.");
            eprintln!("Remote debugging is already on. The cookie read did not come back.");
            eprintln!("Run `muse-cli auth export` again. If Chrome asks, click Allow.");
            eprintln!();
            print_hand_copy();
        }
        _ => {
            eprintln!("{err}");
            eprintln!();
            eprintln!("Check both of these, then run `muse-cli auth export` again:");
            eprintln!("  - chrome://inspect/#remote-debugging has remote debugging on");
            eprintln!("  - https://muse.ai/ is open in that same Chrome and you are logged in");
            eprintln!();
            print_hand_copy();
        }
    }
}

/// Parse one agent-browser JSON envelope, trying the whole output
/// then each JSON-looking line from the bottom up (mirrors
/// `_parse_browser_doc`).
fn parse_browser_doc(text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut blobs: Vec<&str> = vec![text];
    for line in text.lines().rev() {
        let line = line.trim();
        if line.starts_with('{') && !blobs.contains(&line) {
            blobs.push(line);
        }
    }
    for blob in blobs {
        if let Ok(value) = serde_json::from_str::<Value>(blob) {
            if value.is_object() {
                return Some(value);
            }
        }
    }
    None
}

/// Turn one agent-browser invocation into its data, mirroring
/// `_interpret_browser` (envelope `success: false` is an error;
/// `tab <id>` human output with exit 0 is success when JSON is not
/// required).
fn interpret_browser(
    code: i32,
    stdout: &str,
    stderr: &str,
    require_json: bool,
) -> Result<Value, String> {
    let doc = parse_browser_doc(stdout).or_else(|| parse_browser_doc(stderr));
    if let Some(Value::Object(map)) = doc {
        if map
            .get("success")
            .is_some_and(|v| v.as_bool() == Some(false))
        {
            let err = map
                .get("error")
                .map(python_str)
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(err.chars().take(400).collect());
        }
        match map.get("data") {
            Some(Value::Object(_)) => return Ok(map["data"].clone()),
            Some(Value::Array(items)) => return Ok(serde_json::json!({"items": items})),
            _ => {}
        }
        if map.contains_key("cookies") || map.contains_key("tabs") {
            return Ok(Value::Object(map));
        }
        if map.get("success").is_some_and(json_truthy) {
            return Ok(Value::Object(map));
        }
        return Ok(serde_json::json!({}));
    }
    let raw = if !stderr.is_empty() { stderr } else { stdout };
    let text = raw.trim();
    if code == 0 && !require_json {
        return Ok(serde_json::json!({}));
    }
    Err(if text.is_empty() {
        format!("exit {code}")
    } else {
        text.chars().take(400).collect()
    })
}

fn browser_run(argv: &[String], require_json: bool) -> Result<Value, String> {
    let Some(program) = argv.first() else {
        return Err("empty agent-browser argv".to_string());
    };
    let output = std::process::Command::new(program)
        .args(&argv[1..])
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    interpret_browser(code, &stdout, &stderr, require_json)
}

enum BrowserFlow {
    NoTab,
    Failed(String),
}

fn browser_cookies_attempt(base: &[String]) -> Result<Vec<Value>, BrowserFlow> {
    let mut argv = base.to_vec();
    argv.extend(["tab".into(), "list".into(), "--json".into()]);
    let data = browser_run(&argv, true).map_err(BrowserFlow::Failed)?;
    let tabs = data
        .get("tabs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let muse_tabs: Vec<&Value> = tabs
        .iter()
        .filter(|tab| {
            let url = tab.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let has_id =
                tab.get("id").is_some_and(json_truthy) || tab.get("tabId").is_some_and(json_truthy);
            url.contains("muse.ai") && has_id
        })
        .collect();
    if muse_tabs.is_empty() {
        return Err(BrowserFlow::NoTab);
    }
    let first = muse_tabs[0];
    let tab_id = first
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            first
                .get("tabId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            first
                .get("targetId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string();
    let mut focus = base.to_vec();
    focus.extend(["tab".into(), tab_id, "--json".into()]);
    browser_run(&focus, false).map_err(BrowserFlow::Failed)?;
    let mut get = base.to_vec();
    get.extend(["cookies".into(), "get".into(), "--json".into()]);
    let data = browser_run(&get, true).map_err(BrowserFlow::Failed)?;
    let raw: Vec<Value> = match &data {
        Value::Array(items) => items.clone(),
        Value::Object(map) => ["cookies", "items"]
            .iter()
            .find_map(|key| map.get(*key))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    // Python: "muse.ai" in (domain or url or "") — a truthy domain
    // wins over the url even when the url would match.
    let jar: Vec<Value> = raw
        .into_iter()
        .filter(|cookie| {
            let domain = cookie.get("domain").and_then(|v| v.as_str()).unwrap_or("");
            let url = cookie.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let haystack = if !domain.is_empty() { domain } else { url };
            haystack.contains("muse.ai")
        })
        .collect();
    Ok(jar)
}

fn browser_cookies() -> (Option<Vec<Value>>, Option<String>) {
    let base: Vec<String> = vec![
        "agent-browser".to_string(),
        "--headed".to_string(),
        "--auto-connect".to_string(),
    ];
    let mut last_err = "unknown error".to_string();
    for attempt in 0..3 {
        match browser_cookies_attempt(&base) {
            Ok(jar) => return (Some(jar), None),
            Err(BrowserFlow::NoTab) => {
                return (None, Some("no muse.ai tab open in Chrome".to_string()));
            }
            Err(BrowserFlow::Failed(msg)) => {
                last_err = msg.clone();
                if export_error_kind(&msg) == "debug"
                    || export_error_kind(&msg) == "daemon"
                    || attempt == 2
                {
                    return (None, Some(last_err));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    (None, Some(last_err))
}

fn cookie_expires(cookie: &Value) -> i64 {
    match cookie.get("expires") {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// `auth export`, mirroring `cli.py:cmd_auth_export`. The cookies
/// path is the global default (`~/.config/muse-cli/cookies.txt`),
/// not a `config.json` override — like the original. Never overwrites
/// an existing login when the new jar lacks `hatch_sess`.
pub fn cmd_auth_export() -> Result<i32, CliError> {
    let dir = default_config_dir();
    std::fs::create_dir_all(&dir)?;
    let cookies_file = dir.join("cookies.txt");
    if which("agent-browser").is_none() {
        print_need_agent_browser();
        return Ok(1);
    }
    eprintln!("Reading muse.ai cookies from Chrome...");
    let (jar, err) = browser_cookies();
    let Some(jar) = jar else {
        print_export_failure(err.as_deref().unwrap_or("unknown error"), &cookies_file);
        return Ok(1);
    };
    if !jar
        .iter()
        .any(|c| c.get("name").and_then(|v| v.as_str()) == Some("hatch_sess"))
    {
        eprintln!("Chrome answered, but the cookies do not include hatch_sess.");
        eprintln!("That cookie is set only after you log in at https://muse.ai/.");
        eprintln!("Log in, leave the muse.ai tab open, and run `muse-cli auth export` again.");
        eprintln!("Left {} unchanged.", cookies_file.display());
        return Ok(1);
    }
    let mut lines = vec!["# Netscape HTTP Cookie File".to_string()];
    for cookie in &jar {
        let domain = cookie.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        let path = cookie.get("path").and_then(|v| v.as_str()).unwrap_or("/");
        let secure = cookie.get("secure").is_some_and(json_truthy);
        let expires = cookie_expires(cookie);
        let name = cookie.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let value = cookie.get("value").and_then(|v| v.as_str()).unwrap_or("");
        lines.push(format!(
            "{domain}\t{}\t{path}\t{}\t{expires}\t{name}\t{value}",
            if domain.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            },
            if secure { "TRUE" } else { "FALSE" },
        ));
    }
    write_cookies_file_secure(&cookies_file, &(lines.join("\n") + "\n"))?;
    println!(
        "saved {} muse.ai cookies to {}",
        jar.len(),
        cookies_file.display()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_repr_escapes_quotes_and_backslashes() {
        assert_eq!(python_repr("a'b"), "'a\\'b'");
        assert_eq!(python_repr("a\\b"), "'a\\\\b'");
        assert_eq!(python_repr("plain"), "'plain'");
    }

    #[test]
    fn python_str_renders_scalars_like_python() {
        assert_eq!(python_str(&Value::Null), "None");
        assert_eq!(python_str(&serde_json::json!(true)), "True");
        assert_eq!(python_str(&serde_json::json!(false)), "False");
        assert_eq!(python_str(&serde_json::json!(42)), "42");
        assert_eq!(python_str(&serde_json::json!("x")), "x");
    }

    #[test]
    fn fmt_event_maps_role_message_and_text() {
        let event = serde_json::json!({
            "seq": 7,
            "event_name": "message.assistant",
            "payload": {"role": "assistant", "display_text": "hi", "message_id": "m1"},
        });
        let formatted = fmt_event(&event);
        assert_eq!(formatted["seq"], 7);
        assert_eq!(formatted["role"], "assistant");
        assert_eq!(formatted["message_id"], "m1");
        assert_eq!(formatted["text"], "hi");
        let bare = serde_json::json!({"seq": 1, "event_name": "message.user"});
        let formatted = fmt_event(&bare);
        assert_eq!(formatted["role"], "message.user");
        assert_eq!(formatted["text"], "");
        assert_eq!(formatted["message_id"], Value::Null);
    }

    #[test]
    fn event_matches_ref_covers_seq_and_message_ids() {
        let event = serde_json::json!({
            "seq": 12,
            "event_name": "message.assistant",
            "payload": {"message_id": "m-9", "id": "p-1"},
        });
        assert!(event_matches_ref(&event, "12"));
        assert!(event_matches_ref(&event, "m-9"));
        assert!(event_matches_ref(&event, "p-1"));
        assert!(!event_matches_ref(&event, "13"));
        let no_seq = serde_json::json!({"payload": {}});
        assert!(event_matches_ref(&no_seq, "None"));
    }

    #[test]
    fn watch_row_filters_agent_status_and_truncates_text() {
        let status = serde_json::json!({
            "event": "agent.status",
            "seq": 3,
            "payload": {"status": "running"},
        });
        assert_eq!(
            watch_row(&serde_json::to_string(&status).unwrap(), false),
            None
        );
        assert!(watch_row(&serde_json::to_string(&status).unwrap(), true).is_some());
        let long = "x".repeat(400);
        let event = serde_json::json!({
            "event": "message.assistant",
            "seq": 4,
            "payload": {"display_text": long},
        });
        let row = watch_row(&serde_json::to_string(&event).unwrap(), false).unwrap();
        let parsed: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(parsed["display_text"].as_str().unwrap().len(), 300);
        assert!(watch_row("not json", false).is_none());
    }

    #[test]
    fn python_json_separators_space_structural_only() {
        // Structural separators gain a space; separators inside a
        // string value (including after an escaped quote) do not.
        assert_eq!(
            python_json_separators(r#"{"event":"x","seq":4}"#),
            r#"{"event": "x", "seq": 4}"#
        );
        assert_eq!(
            python_json_separators(r#"{"text":"a: b, c \"d: e\", f","n":1}"#),
            r#"{"text": "a: b, c \"d: e\", f", "n": 1}"#
        );
        // Nested objects/arrays and empty values.
        assert_eq!(
            python_json_separators(r#"{"a":{"b":[1,2]},"c":null}"#),
            r#"{"a": {"b": [1, 2]}, "c": null}"#
        );
        // A backslash-escaped backslash must not swallow the quote.
        assert_eq!(
            python_json_separators(r#"{"text":"a\\","n":2}"#),
            r#"{"text": "a\\", "n": 2}"#
        );
    }

    #[test]
    fn watch_row_uses_python_json_separators() {
        let event = serde_json::json!({
            "event": "message.assistant",
            "seq": 4,
            "payload": {"display_text": "a: b, c"},
        });
        let row = watch_row(&serde_json::to_string(&event).unwrap(), false).unwrap();
        // Structural separators have spaces; the value is untouched.
        assert!(row.contains(r#""event": "message.assistant""#));
        assert!(row.contains(r#""display_text": "a: b, c""#));
        // Still parses as the same JSON.
        let parsed: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(parsed["display_text"], "a: b, c");
    }

    #[test]
    fn flatten_feed_and_limit_rules() {
        let data = serde_json::json!({
            "days": [
                {"local_date": "2026-10-24", "editions": [
                    {"kind": "daily", "units": [
                        {"unit_id": "u1", "title": "One"},
                        {"unit_id": "u2", "title": "Two"},
                    ]},
                    {"kind": "weekly", "units": [{"unit_id": "u3", "title": "Three"}]},
                ]},
            ],
        });
        let rows = flatten_feed(&data);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["id"], "u1");
        assert_eq!(rows[0]["date"], "2026-10-24");
        assert_eq!(rows[2]["edition"], "weekly");
        // Limit 0/None = no cut (the feed --limit 0 trap).
        assert_eq!(apply_feed_limit(rows.clone(), None).len(), 3);
        assert_eq!(apply_feed_limit(rows.clone(), Some(0)).len(), 3);
        assert_eq!(apply_feed_limit(rows.clone(), Some(2)).len(), 2);
        assert_eq!(apply_feed_limit(rows.clone(), Some(-1)).len(), 2);
        assert_eq!(apply_feed_limit(rows, Some(-99)).len(), 0);
    }

    #[test]
    fn apply_feed_limit_i64_min_returns_empty_without_overflow() {
        // `(-i64::MIN)` overflows in debug builds; `unsigned_abs`
        // makes it behave like the drop-everything branch.
        let rows = vec![serde_json::json!(1), serde_json::json!(2)];
        assert!(apply_feed_limit(rows, Some(i64::MIN)).is_empty());
    }

    fn secure_write_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "muse-cli-rust-secure-write-{tag}-{pid}-{n}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_cookies_file_secure_overwrites_at_600_and_removes_backup() {
        let dir = secure_write_dir("cookies");
        let path = dir.join("cookies.txt");
        std::fs::write(&path, "old-content").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        write_cookies_file_secure(&path, "new-content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new-content");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
        assert!(!dir.join("cookies.txt.bak").exists());
        // No old file: fresh create is 600 too, and no .bak appears.
        let fresh = dir.join("fresh.txt");
        write_cookies_file_secure(&fresh, "fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "fresh");
        #[cfg(unix)]
        assert_eq!(mode_of(&fresh), 0o600);
        assert!(!dir.join("fresh.txt.bak").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_cookies_file_secure_propagates_write_error() {
        // Destination under a regular file: create_new fails
        // (ENOTDIR) and no partial file is left behind.
        let dir = secure_write_dir("cookies-err");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        let path = blocker.join("cookies.txt");
        assert!(write_cookies_file_secure(&path, "x").is_err());
        assert!(!blocker.join("cookies.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_file_via_part_writes_and_cleans_up() {
        let dir = secure_write_dir("part");
        let dest = dir.join("out.bin");
        write_file_via_part(b"hello", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(!dir.join("out.bin.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn write_file_via_part_refuses_symlink_dest_and_planted_part() {
        use std::os::unix::fs::symlink;
        let dir = secure_write_dir("part-symlink");
        // Symlinked dest: the target must never be overwritten.
        let target = dir.join("target.bin");
        std::fs::write(&target, "sentinel").unwrap();
        let dest = dir.join("link.bin");
        symlink(&target, &dest).unwrap();
        assert!(write_file_via_part(b"evil", &dest).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        assert!(std::fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!dir.join("link.bin.part").exists());
        // Planted .part symlink: create_new refuses it, target intact.
        let dest2 = dir.join("plain.bin");
        let part2 = dir.join("plain.bin.part");
        symlink(&target, &part2).unwrap();
        assert!(write_file_via_part(b"evil", &dest2).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        assert!(!dest2.exists());
        assert!(std::fs::symlink_metadata(&part2)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn usage_text_lines_cover_label_percent_and_state_branches() {
        let with_label = serde_json::json!({
            "label": "Weekly", "value_label": "10 left",
            "quota_status": "SUFFICIENT", "bonus_tokens": null,
            "upgrade": null, "paid": false,
        });
        let lines = usage_text_lines(&with_label, &serde_json::json!({}));
        assert_eq!(lines[0], "Weekly: 10 left");
        assert_eq!(lines[1], "Additional tokens: none");
        let with_percent = serde_json::json!({
            "label": null, "plan": "Pro", "value_label": null,
            "used_percent": 25, "quota_status": "LOW",
            "bonus_tokens": {"label": "Bonus", "value_label": "5 left"},
            "upgrade": "Upgrade now", "paid": false,
        });
        let lines = usage_text_lines(&with_percent, &serde_json::json!({}));
        assert_eq!(lines[0], "Pro: 25% used");
        assert_eq!(lines[1], "  quota: LOW");
        assert_eq!(lines[2], "Bonus: 5 left (never expire)");
        assert_eq!(lines[3], "Upgrade: https://muse.ai/settings");
        let with_state = serde_json::json!({
            "label": null, "plan": null, "value_label": null,
            "used_percent": null, "state": null,
            "quota_status": null, "bonus_tokens": null,
            "upgrade": null, "paid": null,
        });
        let lines = usage_text_lines(&with_state, &serde_json::json!({}));
        assert_eq!(lines[0], "Plan: unknown");
    }

    #[test]
    fn invite_text_lines_cover_code_and_no_code() {
        let with_code = serde_json::json!({
            "code": "ABC123", "link": "https://muse.ai/i/ABC123",
            "max_uses": 5, "uses_remaining": 3,
            "reward": {"title": "Reward", "body": "Body"}, "offer": "Offer!",
        });
        let lines = invite_text_lines(&with_code);
        assert_eq!(lines[0], "Invite code: ABC123");
        assert_eq!(lines[1], "Link: https://muse.ai/i/ABC123");
        assert_eq!(lines[2], "Uses left: 3 of 5");
        assert_eq!(lines[3], "Reward Body");
        assert_eq!(lines[4], "Offer!");
        let no_code = serde_json::json!({
            "code": null, "state": "pending", "reward": null, "offer": null,
        });
        let lines = invite_text_lines(&no_code);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("Invite code: none yet (state pending)."));
    }

    #[test]
    fn export_error_kind_classifies_like_python() {
        assert_eq!(export_error_kind("daemon already running"), "daemon");
        assert_eq!(export_error_kind("no running chrome found"), "debug");
        assert_eq!(export_error_kind("no muse.ai tab open in Chrome"), "tab");
        assert_eq!(
            export_error_kind("✓ My Tab — https://muse.ai/chat"),
            "connected"
        );
        assert_eq!(export_error_kind("something else"), "other");
    }

    #[test]
    fn parse_browser_doc_prefers_last_json_line() {
        let text = "warning: something\n{\"success\": true, \"data\": {\"tabs\": []}}";
        let doc = parse_browser_doc(text).unwrap();
        assert_eq!(doc["success"], true);
        assert!(parse_browser_doc("no json here").is_none());
    }

    #[test]
    fn interpret_browser_success_false_is_an_error() {
        let err = interpret_browser(0, "{\"success\": false, \"error\": \"boom\"}", "", true)
            .unwrap_err();
        assert_eq!(err, "boom");
        let ok = interpret_browser(0, "✓ Title https://muse.ai", "", false).unwrap();
        assert_eq!(ok, serde_json::json!({}));
    }

    #[test]
    fn editor_suffix_matches_splitext() {
        assert_eq!(editor_suffix("notes.md"), ".md");
        assert_eq!(editor_suffix("a/b.tar.gz"), ".gz");
        assert_eq!(editor_suffix(".hidden"), "");
        assert_eq!(editor_suffix("noext"), "");
        assert_eq!(editor_suffix("dir.with.dot/file"), "");
    }
}
