//! Pure-library integration tests (no network, no real HOME, no
//! secrets). Fixtures use obviously fake values (`FAKE-…`).
//!
//! NOTE on `fs`: pure fs coverage lives in `tests/fsclient.rs`
//! (`fsclient.rs` landed while this file was being written). The
//! chunk call-count of `FsClient::read_file`/`write_file` still needs
//! a body-aware mock gateway and is reported as follow-up.

use clap::Parser as _;
use muse_cli::cli::{AutoApproveState, Cli, Command};
use muse_cli::config::{auto_approve_setting, auto_approve_setting_source, load_cookies, Config};
use muse_cli::errors::CliError;
use muse_cli::gateway::join_chunks;
use muse_cli::media::{event_resources, local_name, media_path};
use muse_cli::proto::{ApplicationRequest, NoiseTransportFrame, ServiceFrame, ServiceFrameKind};
use muse_cli::routes::{build_request, lookup, quote_path_param, routes};
use muse_cli::update::{
    cache_is_fresh, fail_backoff_checked_at, is_newer, parse_version, should_update_check,
    version_key, CHECK_INTERVAL_SECS, FAIL_BACKOFF_SECS,
};
use muse_cli::usage::parse_tokens_left;
use prost::Message as _;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serialises the tests that mutate the process environment, because
/// `auto_approve_setting` reads the real `MUSE_AUTO_APPROVE` variable.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "muse-cli-rust-lib-test-{tag}-{pid}-{n}",
        pid = std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

fn test_config(auto_approve: Option<&str>) -> Config {
    Config {
        config_dir: temp_dir("cfg"),
        cookies_file: PathBuf::from("/nonexistent/cookies.txt"),
        vm_id: None,
        node_id: "0123456789abcdef".to_string(),
        auto_approve: auto_approve.map(|s| s.to_string()),
        subscription_action_id: None,
    }
}

// ---------------------------------------------------------------------------
// Cookies: 3 forms + errors (config.rs)
// ---------------------------------------------------------------------------

#[test]
fn cookie_parse_plain_pair_string() {
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
fn cookie_parse_json_forms() {
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
    write(&path, "{\"hatch_sess\": \"FAKE-SESS\"}");
    assert_eq!(load_cookies(&path).unwrap(), "hatch_sess=FAKE-SESS");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cookie_parse_netscape_jar() {
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
fn cookie_parse_errors_are_auth_not_crashes() {
    let dir = temp_dir("cookie-bad");
    let path = dir.join("cookies.txt");
    // Unparseable content.
    write(&path, "nothing parseable here");
    assert!(matches!(
        load_cookies(&path).unwrap_err(),
        CliError::Auth(_)
    ));
    // Broken JSON falls through to the other forms instead of crashing.
    write(&path, "{not json");
    assert!(matches!(
        load_cookies(&path).unwrap_err(),
        CliError::Auth(_)
    ));
    // Empty file.
    write(&path, "");
    assert!(matches!(
        load_cookies(&path).unwrap_err(),
        CliError::Auth(_)
    ));
    // Missing file.
    assert!(matches!(
        load_cookies(&dir.join("nope.txt")).unwrap_err(),
        CliError::Auth(_)
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Auto-approve precedence: flag > env > config > off (config.rs)
// ---------------------------------------------------------------------------

#[test]
fn auto_approve_precedence_flag_beats_env_and_config() {
    let _guard = ENV_LOCK.lock().unwrap();
    let cfg = test_config(Some("always"));
    std::env::set_var("MUSE_AUTO_APPROVE", "off");
    assert!(auto_approve_setting(&cfg, Some(true)).unwrap());
    assert_eq!(
        auto_approve_setting_source(&cfg, Some(true)).unwrap().1,
        "flag"
    );
    assert!(!auto_approve_setting(&cfg, Some(false)).unwrap());
    std::env::set_var("MUSE_AUTO_APPROVE", "always");
    assert!(!auto_approve_setting(&cfg, Some(false)).unwrap());
    std::env::remove_var("MUSE_AUTO_APPROVE");
}

#[test]
fn auto_approve_precedence_env_beats_config() {
    let _guard = ENV_LOCK.lock().unwrap();
    let cfg = test_config(Some("always"));
    std::env::set_var("MUSE_AUTO_APPROVE", "off");
    assert!(!auto_approve_setting(&cfg, None).unwrap());
    assert_eq!(
        auto_approve_setting_source(&cfg, None).unwrap().1,
        "env MUSE_AUTO_APPROVE"
    );
    let cfg_off = test_config(Some("off"));
    std::env::set_var("MUSE_AUTO_APPROVE", "always");
    assert!(auto_approve_setting(&cfg_off, None).unwrap());
    std::env::remove_var("MUSE_AUTO_APPROVE");
}

#[test]
fn auto_approve_env_on_off_values_and_bad_value_is_an_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    let cfg = test_config(None);
    for on in ["always", "on", "1", "true", "yes", " ALWAYS "] {
        std::env::set_var("MUSE_AUTO_APPROVE", on);
        assert!(auto_approve_setting(&cfg, None).unwrap(), "env {on:?}");
    }
    for off in ["off", "0", "false", "no", "never"] {
        std::env::set_var("MUSE_AUTO_APPROVE", off);
        assert!(!auto_approve_setting(&cfg, None).unwrap(), "env {off:?}");
    }
    // An empty-string env still counts as "set" (source says env) and
    // means off — it must not fall through to config.
    let cfg_always = test_config(Some("always"));
    std::env::set_var("MUSE_AUTO_APPROVE", "");
    assert!(!auto_approve_setting(&cfg_always, None).unwrap());
    assert_eq!(
        auto_approve_setting_source(&cfg_always, None).unwrap().1,
        "env MUSE_AUTO_APPROVE"
    );
    // Anything else is a Value error (exit 2 at the binary level).
    std::env::set_var("MUSE_AUTO_APPROVE", "maybe");
    let err = auto_approve_setting(&cfg, None).unwrap_err();
    match err {
        CliError::Value(msg) => {
            assert!(msg.contains("MUSE_AUTO_APPROVE must be always or off, not 'maybe'"));
        }
        other => panic!("wrong variant: {other:?}"),
    }
    std::env::remove_var("MUSE_AUTO_APPROVE");
}

#[test]
fn auto_approve_config_only_exact_always_counts_as_on() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("MUSE_AUTO_APPROVE");
    let cfg = test_config(Some("always"));
    assert!(auto_approve_setting(&cfg, None).unwrap());
    // Config value "on" is NOT on (only "always", case-insensitive).
    let cfg = test_config(Some("on"));
    assert!(!auto_approve_setting(&cfg, None).unwrap());
    let cfg = test_config(Some("Always"));
    assert!(auto_approve_setting(&cfg, None).unwrap());
    let cfg = test_config(None);
    let (enabled, source) = auto_approve_setting_source(&cfg, None).unwrap();
    assert!(!enabled);
    assert!(source.starts_with("config "));
}

// ---------------------------------------------------------------------------
// Media paths (media.rs)
// ---------------------------------------------------------------------------

#[test]
fn media_path_forms() {
    assert_eq!(
        media_path(&json!("sandbox://workspace/photo.png")).unwrap(),
        "/media/raw/workspace/photo.png"
    );
    assert_eq!(
        media_path(&json!("workspace/a.png")).unwrap(),
        "/media/raw/workspace/a.png"
    );
    assert_eq!(
        media_path(&json!("media/raw/a.png")).unwrap(),
        "/media/raw/a.png"
    );
    assert_eq!(
        media_path(&json!("/media/raw/a.png")).unwrap(),
        "/media/raw/a.png"
    );
    // URL: path only, query/fragment dropped, media/raw/ stripped once.
    assert_eq!(
        media_path(&json!("https://vm.example.com/media/raw/a/b.png?x=1#frag")).unwrap(),
        "/media/raw/a/b.png"
    );
    // Percent-decode then re-encode (safe "/"); "+" becomes "%2B".
    assert_eq!(
        media_path(&json!("sandbox://workspace/my%20photo.png")).unwrap(),
        "/media/raw/workspace/my%20photo.png"
    );
    assert_eq!(
        media_path(&json!("sandbox://workspace/a+b.png")).unwrap(),
        "/media/raw/workspace/a%2Bb.png"
    );
}

#[test]
fn media_path_dict_priority_variants_over_path_over_url() {
    let with_all = json!({
        "variants": {"original": "https://vm.example.com/media/raw/orig.png"},
        "path": "sandbox://workspace/path.png",
        "url": "https://vm.example.com/media/raw/url.png",
    });
    assert_eq!(media_path(&with_all).unwrap(), "/media/raw/orig.png");
    let with_path = json!({
        "path": "sandbox://workspace/path.png",
        "url": "https://vm.example.com/media/raw/url.png",
    });
    assert_eq!(
        media_path(&with_path).unwrap(),
        "/media/raw/workspace/path.png"
    );
}

#[test]
fn media_path_dotdot_and_empty_are_value_errors() {
    for bad in [
        json!("sandbox://workspace/../secret.txt"),
        json!(""),
        json!("sandbox://"),
        json!({}),
    ] {
        assert!(
            matches!(media_path(&bad).unwrap_err(), CliError::Value(_)),
            "input {bad:?} must be a Value error"
        );
    }
    let err = media_path(&json!({"mime": "image/png"})).unwrap_err();
    match err {
        CliError::Value(msg) => assert_eq!(msg, "resource has no path or variants.original"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn media_local_name_and_event_resources() {
    assert_eq!(
        local_name("sandbox://workspace/photo.png?x=1", Path::new("/tmp/out")),
        Path::new("/tmp/out/photo.png")
    );
    assert_eq!(local_name("", Path::new("out")), Path::new("out/download"));

    let ev = json!({
        "payload": {
            "data": {
                "videos": [{"path": "sandbox://workspace/v.mp4", "mime": "video/mp4", "byte_len": 42}],
                "images": [{"variants": {"original": "https://vm/media/raw/i.png"}}],
            }
        }
    });
    let rows = event_resources(&ev);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["kind"] == "video"));
    assert!(rows.iter().any(|r| r["kind"] == "image"));

    // Skips missing items and non-sandbox paths.
    let ev = json!({
        "payload": {"data": {"files": [
            {"path": "sandbox://workspace/gone.png", "missing": true},
            {"path": "/tmp/plain.png"},
            {"path": "sandbox://workspace/kept.png"},
        ]}}
    });
    let rows = event_resources(&ev);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["path"], "sandbox://workspace/kept.png");

    // sandbox:// links inside display text, kind "link".
    let ev = json!({
        "payload": {"display_text": "see sandbox://workspace/a.png and (sandbox://workspace/b.png) done"}
    });
    let rows = event_resources(&ev);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["kind"] == "link"));

    // Same path merges; first kind wins, later fields fill in.
    let ev = json!({
        "payload": {
            "data": {"images": [{"path": "sandbox://workspace/a.png", "mime": "image/png"}]},
            "resources": [{"path": "sandbox://workspace/a.png", "label": "A"}],
        }
    });
    let rows = event_resources(&ev);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "image");
    assert_eq!(rows[0]["label"], "A");
}

// ---------------------------------------------------------------------------
// Routes (routes.rs)
// ---------------------------------------------------------------------------

#[test]
fn routes_table_has_258_entries_and_lookup_errors() {
    assert_eq!(routes().len(), 258);
    assert!(lookup("chat.history").is_ok());
    let err = lookup("no.such.method").unwrap_err();
    match err {
        CliError::Value(msg) => assert_eq!(
            msg,
            "unknown method 'no.such.method' (see routes.json for the 258 known methods)"
        ),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn routes_aliases_are_not_resolved() {
    // routes.json carries aliases as a string on some entries, but raw
    // accepts exactly the method names. "/nodes/list" is an alias of
    // node.list, not a method itself.
    assert!(lookup("/nodes/list").is_err());
    assert!(lookup("custom_oauth_connector.register").is_err());
    assert!(lookup("custom_connectors.register").is_ok());
    assert!(
        lookup("media.raw").is_err(),
        "media.raw is synthetic, not in routes.json"
    );
}

#[test]
fn build_request_path_params_query_and_headers() {
    assert_eq!(quote_path_param("café/x"), "caf%C3%A9%2Fx");
    assert_eq!(quote_path_param("a~b-c_d.e"), "a~b-c_d.e");
    let built = build_request(
        "egress.approval.decide",
        &[("approval_id".to_string(), "a/b c".to_string())],
        None,
        None,
    )
    .unwrap();
    assert_eq!(built.path, "/approvals/a%2Fb%20c/decide");
    assert_eq!(built.verb, "POST");
    assert!(built.raw_body.is_empty());
    // x-request-id always present; content-type only with a body.
    assert_eq!(built.headers.len(), 1);
    assert_eq!(built.headers[0].0, "x-request-id");
    let with_body = build_request("chat.stream", &[], Some(&json!({"a": 1})), None).unwrap();
    assert_eq!(with_body.headers.len(), 2);
    assert_eq!(
        with_body.headers[1],
        ("content-type".to_string(), "application/json".to_string())
    );
    // Query skips None values.
    let built = build_request(
        "feed.list",
        &[],
        None,
        Some(&json!({"limit": 10, "cursor": null})),
    )
    .unwrap();
    assert_eq!(built.path, "/api/feed?limit=10");
    // NOTE: build_request does NOT do the GET body->query conversion;
    // that lives in Gateway::request (mock-gateway test covers the
    // unary path; the conversion itself is a gateway-layer rule).
    let built = build_request("chat.history", &[], Some(&json!({"limit": 5})), None).unwrap();
    assert_eq!(built.raw_body, b"{\"limit\":5}");
}

// ---------------------------------------------------------------------------
// Chunk reassembly (gateway.rs)
// ---------------------------------------------------------------------------

fn frame(chunk_id: i64, index: u32, total: u32, payload: &[u8]) -> NoiseTransportFrame {
    NoiseTransportFrame {
        chunk_id: Some(chunk_id),
        chunk_index: Some(index),
        total_chunks: Some(total),
        payload: Some(payload.to_vec()),
    }
}

#[test]
fn join_chunks_single_in_order_reverse_missing_and_duplicate() {
    let mut partial: HashMap<i64, HashMap<u32, Vec<u8>>> = HashMap::new();
    // total <= 1 returns the payload immediately.
    assert_eq!(
        join_chunks(&mut partial, &frame(1, 0, 1, b"only")).as_deref(),
        Some(b"only".as_slice())
    );
    // In order.
    assert_eq!(join_chunks(&mut partial, &frame(2, 0, 3, b"a")), None);
    assert_eq!(join_chunks(&mut partial, &frame(2, 1, 3, b"b")), None);
    assert_eq!(
        join_chunks(&mut partial, &frame(2, 2, 3, b"c")).as_deref(),
        Some(b"abc".as_slice())
    );
    // Reverse order still joins sorted by index.
    assert_eq!(join_chunks(&mut partial, &frame(3, 2, 3, b"c")), None);
    assert_eq!(join_chunks(&mut partial, &frame(3, 0, 3, b"a")), None);
    assert_eq!(
        join_chunks(&mut partial, &frame(3, 1, 3, b"b")).as_deref(),
        Some(b"abc".as_slice())
    );
    // Missing a chunk never completes.
    assert_eq!(join_chunks(&mut partial, &frame(4, 0, 3, b"a")), None);
    assert_eq!(join_chunks(&mut partial, &frame(4, 2, 3, b"c")), None);
    // Duplicate index overwrites and does not count twice: total 3,
    // indices 0 (twice, second wins), 1, 2 -> completes with "Xbc".
    assert_eq!(join_chunks(&mut partial, &frame(5, 0, 3, b"a")), None);
    assert_eq!(join_chunks(&mut partial, &frame(5, 0, 3, b"X")), None);
    assert_eq!(join_chunks(&mut partial, &frame(5, 1, 3, b"b")), None);
    assert_eq!(
        join_chunks(&mut partial, &frame(5, 2, 3, b"c")).as_deref(),
        Some(b"Xbc".as_slice())
    );
}

// ---------------------------------------------------------------------------
// CLI surface at the library level (cli.rs) + error helpers
// ---------------------------------------------------------------------------

#[test]
fn cli_leaf_names_and_helpers() {
    assert_eq!(Command::leaf_names().len(), 50);
    assert!(Command::leaf_names().contains(&"auth export"));
    assert!(Command::leaf_names().contains(&"fs rm"));
    assert!(Command::leaf_names().contains(&"raw"));
    let cli = Cli::try_parse_from(["muse-cli", "auth"]).unwrap();
    assert!(cli.command.as_ref().unwrap().missing_subcommand());
    let cli = Cli::try_parse_from(["muse-cli", "fs"]).unwrap();
    assert!(cli.command.as_ref().unwrap().missing_subcommand());
    let cli = Cli::try_parse_from(["muse-cli", "approve", "id-1"]).unwrap();
    assert_eq!(
        cli.command.as_ref().unwrap().approve_decision(),
        Some("allow_once")
    );
    let cli = Cli::try_parse_from(["muse-cli", "send", "hi", "--auto-approve"]).unwrap();
    assert_eq!(
        cli.command.as_ref().unwrap().auto_approve_flag(),
        Some(true)
    );
    let cli = Cli::try_parse_from(["muse-cli", "auto-approve"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::AutoApprove {
            state: AutoApproveState::Status
        }
    ));
    // Defaults from the CLI surface doc.
    let cli = Cli::try_parse_from(["muse-cli", "send", "hello"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Send {
            wait: 90,
            ack_timeout: 30,
            ..
        }
    ));
    let cli = Cli::try_parse_from(["muse-cli", "wait"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Wait { timeout: 600, .. }
    ));
    assert!(matches!(CliError::auth("x"), CliError::Auth(_)));
    assert!(matches!(
        CliError::gateway(-1, "p"),
        CliError::Gateway { status: -1, .. }
    ));
}

// ---------------------------------------------------------------------------
// Usage / update pure helpers (usage.rs, update.rs)
// ---------------------------------------------------------------------------

#[test]
fn usage_parse_tokens_left_examples() {
    assert_eq!(
        parse_tokens_left("0% used (3B tokens left)"),
        Some(3_000_000_000)
    );
    assert_eq!(
        parse_tokens_left("Đã dùng 4% (còn 12 tỷ token)"),
        Some(12_000_000_000)
    );
    assert_eq!(parse_tokens_left("no numbers here"), None);
    assert_eq!(parse_tokens_left(""), None);
}

#[test]
fn update_pure_helpers() {
    // Check gate: both streams must be ttys, neither env set.
    assert!(should_update_check(false, false, true, true));
    assert!(!should_update_check(true, false, true, true));
    assert!(!should_update_check(false, true, true, true));
    assert!(!should_update_check(false, false, false, true));
    assert!(!should_update_check(false, false, true, false));
    // Version ordering: numeric x.y.z only; pre-release/junk is absent.
    assert_eq!(version_key("0.3.2"), Some(vec![0, 3, 2]));
    assert_eq!(version_key("v1.2.3"), Some(vec![1, 2, 3]));
    assert_eq!(version_key("1.2.3-beta"), None);
    assert!(is_newer(Some("0.3.3"), "0.3.2"));
    assert!(!is_newer(Some("0.3.2"), "0.3.2"));
    assert!(!is_newer(Some("junk"), "0.3.2"));
    assert!(!is_newer(None, "0.3.2"));
    assert_eq!(parse_version("__version__ = \"0.3.2\"\n").unwrap(), "0.3.2");
    assert!(parse_version("nothing here").is_err());
    // Cache freshness and fail backoff arithmetic.
    assert!(cache_is_fresh(1000.0, 1000.0 + CHECK_INTERVAL_SECS - 1.0));
    assert!(!cache_is_fresh(1000.0, 1000.0 + CHECK_INTERVAL_SECS));
    assert_eq!(
        fail_backoff_checked_at(100_000.0),
        100_000.0 - CHECK_INTERVAL_SECS + FAIL_BACKOFF_SECS
    );
}

// ---------------------------------------------------------------------------
// Protobuf cross-check against the shipped descriptors (proto.rs)
// ---------------------------------------------------------------------------

#[test]
fn proto_descriptors_match_field_numbers() {
    use prost_types::FileDescriptorProto;
    let desc0 =
        FileDescriptorProto::decode(include_bytes!("../assets/desc0.bin").as_slice()).unwrap();
    let desc1 =
        FileDescriptorProto::decode(include_bytes!("../assets/desc1.bin").as_slice()).unwrap();
    assert_eq!(desc0.name.as_deref(), Some("noise_transport.proto"));
    assert_eq!(desc1.name.as_deref(), Some("noise_envelope.proto"));
    let numbers = |fd: &FileDescriptorProto, message: &str, field: &str| -> i32 {
        for m in &fd.message_type {
            if m.name.as_deref() == Some(message) {
                for f in &m.field {
                    if f.name.as_deref() == Some(field) {
                        return f.number.unwrap_or(-1);
                    }
                }
            }
        }
        -1
    };
    assert_eq!(numbers(&desc0, "NoiseTransportFrame", "chunk_id"), 1);
    assert_eq!(numbers(&desc0, "NoiseTransportFrame", "payload"), 4);
    assert_eq!(numbers(&desc1, "ServiceFrame", "stream_id"), 1);
    assert_eq!(numbers(&desc1, "ServiceFrame", "response"), 3);
    assert_eq!(numbers(&desc1, "ApplicationResponse", "status"), 1);
    assert_eq!(numbers(&desc1, "ApplicationResponse", "end_body"), 4);
    // And a real envelope round-trips through prost.
    let frame = ServiceFrame {
        stream_id: Some(7),
        kind: Some(ServiceFrameKind::Request(ApplicationRequest {
            verb: Some("GET".to_string()),
            path: Some("/chat/history".to_string()),
            headers: vec![],
            body: Some(Vec::new()),
            end_body: Some(true),
        })),
    };
    let bytes = frame.encode_to_vec();
    assert_eq!(ServiceFrame::decode(bytes.as_slice()).unwrap(), frame);
}
