# CONTRACT — muse-cli Rust rewrite (coordinator-owned)

Phase 0 (settled before any implementation, per subagent-orchestration):

- Outcome: binary at `target/debug/muse-cli` (and `target/release/muse-cli`
  on `cargo build --release`) that reproduces the Python muse-cli 0.3.2
  CLI surface.
- Metric: (1) `cargo build` pass, (2) `cargo test` pass with a real test
  count reported, (3) `--help` leaf-command count, (4) per-command
  parity table.
- Floor (FAIL if any missing): `cargo build` pass; `--version` prints
  exactly `muse-cli 0.3.2`; `--help` lists all 50 leaf commands
  (37 regular + `auth export` + 12 `fs` subcommands); `cargo test` pass;
  zero Python runtime code, zero subprocess calls to the Python original.
- Target: Floor + flag/default/output/error parity per command from
  `~/workspace/plans/active/muse-cli-cli-surface.md` + mock/fixture tests
  for gateway, wait, fs, media + README with an honest
  "not yet verified live" section.
- Verifier (not self-assessment): `cargo build`, `cargo test`,
  running the built binary with `--help`/`--version`/error cases, and
  counting leaf commands in its help output. Live muse.ai calls are
  NEVER the verifier in this task (no credentials, forbidden).

## Architecture decision (coordinator, binding unless a writer proves otherwise)

SYNC, not Tokio. The Python original is synchronous (thread + lock for
the subscription reader, `curl_cffi` blocking calls). Rust mirrors it:
`reqwest` blocking for HTTPS, `tungstenite` (sync) for WebSocket,
`snow` for Noise, `prost` for protobuf. Consequence: the
rust-async-patterns skill is NOT required for sync code; any writer who
wants to introduce Tokio/async must first read that skill and record
the reason in their report, and must not convert the whole crate to
async just for one command.

## Sources (read before coding; code beats README when they disagree)

- `~/workspace/plans/active/muse-cli-original-survey.md`
- `~/workspace/plans/active/muse-cli-cli-surface.md` (flag/default/exit
  tables, tester checklist, 12 README-vs-code differences)
- `~/workspace/plans/active/muse-cli-gateway-protocol.md` (§(b) framing,
  §(c.1) routes actually used, §(d) send/wait, §(e) media/fs, §8 writer
  checklist, §7 NEEDS-MEASUREMENT limits)
- Original, READ-ONLY: `~/workspace/muse-cli/` (commit 746ae82). When a
  survey and the code disagree, the code wins and the disagreement goes
  in the writer's report.
- `~/workspace/plans/active/muse-cli-rust-rewrite.md` (parent plan).

## File ownership (1 file = 1 writer at a time)

Coordinator-owned (children must NOT edit): `Cargo.toml` (except writer B
may edit ONLY the `[dependencies]` section to add/change crate versions
or features it needs, and must report every change), `src/lib.rs`,
`src/main.rs`, `CONTRACT.md`, `README.md` (writer D owns the first full
draft; later fix rounds may edit it only through the coordinator).

Round 1, writer A (scaffold + CLI surface + config + pure logic):
`src/cli.rs`, `src/config.rs`, `src/errors.rs`, `src/output.rs`,
`src/update.rs`, `src/usage.rs`, `src/media.rs` (pure parts only:
`media_path`, resource extraction from events; download orchestration
belongs to writer C).

Round 1, writer B (gateway core):
`src/routes.rs`, `src/proto.rs`, `src/noise.rs`, `src/gateway.rs`,
`src/wait.rs` (pure Waiter state machine + `run_wait` orchestration;
the auto-approver closure is supplied by the caller, see below).

Round 2, writer C (commands): `src/commands.rs`, `src/chat.rs`,
`src/fsclient.rs`. Writer C also owns the media-download orchestration
inside `src/commands.rs` (using `gateway.call_bytes` with the synthetic
`media.raw` route). Writer C must NOT change the public signatures in
this contract without coordinator approval.

Round 2, writer D (tests + README): `tests/*.rs` (integration tests
only — unit tests live in the module they test and belong to that
module's writer; writer D reports unit-test gaps instead of editing
other writers' files), `README.md` (first full draft).

Fix round (coordinator-directed): each fix goes to the file's owner.

## Public API (exact signatures; do not rename without coordinator approval)

```rust
// errors.rs — thiserror. Main maps these to stderr + exit code.
pub enum CliError {
    Auth(String),                              // -> "auth error: …", exit 2
    Gateway { status: i64, payload: String }, // -> "gateway error: …", exit 3
    Value(String),                             // -> "error: …", exit 2
    Timeout(String),                           // -> "timeout: …", exit 4
    Io(String),                                // -> "error: …", exit 2
}
// Helpers: CliError::auth(msg), ::gateway(status, payload), ::value(msg),
// ::timeout(msg), ::io(msg). No unwrap() in production code. unwrap() is
// allowed ONLY in tests. expect() in production only with a comment
// proving the invariant (e.g. a lock that cannot be poisoned by our own
// code path — prefer handling poisoning as CliError::Io instead).

// config.rs
pub struct Config {
    pub config_dir: PathBuf,          // ~/.config/muse-cli (see note below)
    pub cookies_file: PathBuf,
    pub vm_id: Option<String>,
    pub node_id: String,
    pub auto_approve: Option<String>, // raw config value, e.g. "always"
    pub subscription_action_id: Option<String>,
}
pub fn load_config() -> Result<Config, CliError>;   // creates node_id (16 hex chars) and saves if missing, like Python load_config
pub fn save_config(cfg: &Config) -> Result<(), CliError>;
pub fn load_cookies(path: &Path) -> Result<String, CliError>; // 3 forms: JSON, "a=b; c=d" with hatch_sess=/datr=, Netscape; unparseable -> Auth
pub fn cookies_file_exists(cfg: &Config) -> bool;
// Auto-approve precedence: per-call flag > MUSE_AUTO_APPROVE env > config > off.
// Env on: always/on/1/true/yes; off: off/0/false/no/never/""; anything else -> Value error (exit 2).
pub fn auto_approve_setting(cfg: &Config, flag: Option<bool>) -> Result<bool, CliError>;
// NOTE: read cli.py load_config (lines ~28-60) before assuming the config
// dir: the original uses ~/.config/muse-cli via expanduser; do NOT invent
// XDG_CONFIG_HOME handling unless the original code does it.

// output.rs
pub fn print_json(value: &serde_json::Value); // pretty, indent 2, like Python json.dumps(indent=2)
pub fn print_json_str(s: &str);               // for values already serialised
// Commands print their own output; output.rs must not decide exit codes.

// routes.rs
pub struct Route { pub method: String, pub http: String, pub path: String, pub service: Option<String> }
pub fn routes() -> &'static HashMap<String, Route>; // parsed once from embedded assets/routes.json; must contain 258 entries
pub fn lookup(method: &str) -> Result<&'static Route, CliError>; // unknown -> Value("unknown method '<m>' (see routes.json for the 258 known methods)")
pub struct BuiltRequest { pub verb: String, pub path: String, pub headers: Vec<(String, String)>, pub raw_body: Vec<u8> }
pub fn build_request(method: &str, path_params: &[(String, String)], body: Option<&serde_json::Value>, query: Option<&serde_json::Value>) -> Result<BuiltRequest, CliError>;
// build_request rules (gateway.py _open): substitute {k} with percent-encoding of the value with NO safe chars (including '/');
// query: urlencode, skipping None values, joined with ? or &; body None -> empty bytes, bytes -> as-is, other -> JSON bytes;
// headers: always x-request-id: <uuid v4>; content-type: application/json ONLY when raw_body is non-empty.
// NOTE: the GET body->query conversion (GET + body + no query => body becomes query) lives in Gateway::request, NOT here.

// proto.rs — prost structs, field numbers EXACTLY from gateway spec §(b.2).
// NoiseTransportFrame{1:chunk_id Option<i64>, 2:chunk_index Option<u32>, 3:total_chunks Option<u32>, 4:payload Option<Vec<u8>>}
// ServiceRequest{1:service Option<i32>, 2:payload Option<Vec<u8>>}
// ServiceResponse{1:payload Option<Vec<u8>>}
// ServiceFrame{1:stream_id Option<i64>, 2:request oneof, 3:response oneof, 4:body_chunk oneof, 5:reset oneof}
// ApplicationRequest{1:verb, 2:path, 3:headers repeated Header, 4:body, 5:end_body}
// ApplicationResponse{1:status, 2:headers, 3:body, 4:end_body}
// BodyChunk{1:data, 2:end_body}; Reset{1:code, 2:reason}; Header{1:key, 2:value}
// ServiceType: DAEMON=0, SENTINEL=1, VAULT=2, AUTHD=3. Reset.Code: 0..6 as in spec.
// Cross-check test required: decode assets/desc0.bin and desc1.bin as
// prost_types::FileDescriptorProto and assert the field numbers above match.

// noise.rs
pub struct NoiseSession; // wraps a snow Noise object, initiator role
impl NoiseSession {
    pub fn new_initiator() -> Result<Self, CliError>; // fresh random 32-byte static key per connection, pattern Noise_XX_25519_AESGCM_SHA256
    pub fn write_handshake_message(&mut self) -> Result<Vec<u8>, CliError>; // payload empty; called 3 times interleaved with reads
    pub fn read_handshake_message(&mut self, msg: &[u8]) -> Result<(), CliError>;
    pub fn handshake_finished(&self) -> bool;
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CliError>;
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, CliError>;
}
// The 3-message handshake (empty payloads) and the assert-finished step
// live in gateway.rs, using this API.

// gateway.rs
pub trait Transport: Send {
    fn send_bytes(&mut self, data: &[u8]) -> Result<(), CliError>;
    fn recv_bytes(&mut self) -> Result<Vec<u8>, CliError>;
}
pub struct Gateway; // holds transport + NoiseSession + stream counter (starts at 1, monotonic) + send/recv locks
impl Gateway {
    pub fn connect(cfg: &Config) -> Result<Self, CliError>; // 3 HTTPS calls in order (auth/check -> hatch/token -> session), then WS + Noise handshake; TLS verify ON
    pub fn connect_with_transport(cfg: &Config, transport: Box<dyn Transport>) -> Result<Self, CliError>; // same, but transport injected (tests use an in-memory pair)
    pub fn request(&mut self, method: &str, path_params: &[(String, String)], body: Option<&serde_json::Value>, query: Option<&serde_json::Value>, timeout: Duration) -> Result<(i32, Vec<u8>), CliError>;
    // request rules: GET + body + no query => body becomes query (gateway spec §(b.5)); frames of other streams are DISCARDED, not buffered;
    // response/body_chunk accumulate until end_body; reset -> Gateway(-1, "stream reset <code>: <reason>");
    // no response frame before deadline -> Timeout("no response for <method>"); status outside 200..300 -> Gateway(status, payload[..500] decoded utf-8 lossy);
    // quirk (keep): response seen but end_body never arrives before deadline -> return what was accumulated, do NOT raise.
    pub fn request_with_route(&mut self, verb: &str, path: &str, service: i32, body: Option<&[u8]>, timeout: Duration) -> Result<(i32, Vec<u8>), CliError>; // synthetic routes (media.raw)
    pub fn call_json(&mut self, method: &str, path_params: &[(String, String)], body: Option<&serde_json::Value>, query: Option<&serde_json::Value>, timeout: Duration) -> Result<serde_json::Value, CliError>;
    // call_json rules: empty payload -> {}; invalid JSON -> Gateway(-1, "non-JSON response: <first 200 bytes>"); dict with ok==false -> Gateway(-1, "api error: <error>"); dict with "result" -> that value, else whole dict; non-dict JSON -> as-is.
    pub fn call_bytes(&mut self, method: &str, path_params: &[(String, String)], body: Option<&serde_json::Value>, query: Option<&serde_json::Value>, timeout: Duration) -> Result<Vec<u8>, CliError>;
    pub fn subscribe_records(&mut self, method: &str, path_params: &[(String, String)], body: Option<&serde_json::Value>, query: Option<&serde_json::Value>, idle_timeout: Duration, overall_timeout: Duration, max_records: usize, on_record: &mut dyn FnMut(&str) -> bool) -> Result<Vec<String>, CliError>;
    // subscribe_records rules (gateway spec §(b.6)): reader thread + channel, 2s poll, idle timer reset ONLY by frames of our own stream,
    // other-stream frames discarded, response/body_chunk accumulate into a newline-split buffer, non-empty stripped lines are records,
    // on_record returning true halts (record already appended), trailing non-blank buffer becomes a final record (on_record called, result ignored),
    // reset breaks without error, end_body with empty buffer breaks. Defaults used by callers: run_wait max_records huge, watch max_records = --n.
    pub fn stream_counter(&self) -> i64;
}
pub fn join_chunks(partial: &mut HashMap<i64, HashMap<u32, Vec<u8>>>, frame: &NoiseTransportFrame) -> Option<Vec<u8>>; // total<=1 -> payload immediately; else collect by chunk_index, join sorted by index when complete; duplicate index overwrites
pub fn media_path(ref_: &serde_json::Value) -> Result<String, CliError>; // lives in gateway.rs OR media.rs — coordinator decision: media.rs owns the pure function; gateway.rs re-exports it. Do not duplicate the logic.

// wait.rs
pub struct ApprovalItem { pub id: String, pub title: String, pub kind: String, pub scope: String, pub has_forever_option: bool }
pub enum WaitOutcomeKind { Done, Failed, NeedsInput, NeedsApproval, Timeout }
pub struct WaitResult { pub kind: WaitOutcomeKind, pub exit_code: i32, pub final_text: Option<String>, pub question: Option<String>, pub auto_approved: Vec<serde_json::Value>, pub auto_approve_errors: Vec<serde_json::Value> }
pub struct Waiter; // pure event consumer: feed(event_json) drives DONE/FAILED sets, approval-before-task-status, require_running, root-agent-only scope
impl Waiter {
    pub fn new(thread: Option<String>, after_seq: Option<i64>, require_running: bool) -> Self;
    pub fn feed(&mut self, event: &serde_json::Value, approver: &mut dyn FnMut(&[ApprovalItem]) -> (Vec<serde_json::Value>, Vec<serde_json::Value>)) -> Option<WaitResult>;
    // feed returns Some(result) exactly when the wait is decided; None means keep waiting. All DONE/FAILED/needs_input rules come from
    // gateway spec §(d.1)-(d.5) and wait.py — read them, do not re-derive from the survey summary alone.
}
pub fn run_wait(gw: &mut Gateway, thread: Option<String>, after_seq: Option<i64>, timeout: Duration, require_running: bool, auto_approve: bool, approver_gateway_factory: &mut dyn FnMut() -> Result<Gateway, CliError>) -> Result<WaitResult, CliError>;
// run_wait: subscribes to chat.subscribe, feeds events to Waiter, then re-reads the final reply from history (limit 30) exactly like Python run_wait;
// TimeoutError from the gateway layer (exit 4) is NOT the same as WaitResult Timeout (exit 124) — keep them distinct.

// commands.rs
pub fn dispatch(cli: Cli) -> Result<i32, CliError>; // returns the process exit code; prints output itself
// dispatch rules: connect lazily per command (only commands that need the gateway connect; fs rm without --yes, raw with unknown method,
// approve without id and without --all, and argument errors must fail BEFORE connecting — see CLI surface doc);
// update-check notice is handled by main.rs, not here; chat (no subcommand) goes to chat.rs.

// chat.rs — interactive chat (slash commands /chats, /open N, /main, /new, /history, /login, /help, /?, /exit, /quit; poll history 1s, WAIT 300, QUIET 10).
// fsclient.rs — System Files via gateway: list/stat/read/write/mkdir/rename/delete/search + 4 MiB chunking + path normalisation (norm rules in gateway spec §(e.3)).
//   fs edit must exec $VISUAL/$EDITOR/vi DIRECTLY (no shell). fs rm without --yes fails before connecting, exit 2.
// update.rs — update-check state (update.json, 24h cache, 1h fail backoff, background thread joined max 1.5s, notice on stderr only when both streams are tty
//   and MUSE_NO_UPDATE_CHECK and CI are unset) + `update` command as CHECK-ONLY: report the latest version and print how a Python install would upgrade
//   (git pull for checkouts, uv/pipx/pip receipt detection); it must NOT download, install, or overwrite any binary. This is a documented partial
//   implementation (see README), not a silent behaviour change: the command prints that it does not self-upgrade.
```

## CLI surface (writer A owns; writer C must not redefine it)

`src/cli.rs` uses clap derive. Bin name `muse-cli`, version `0.3.2`.
Top-level commands (39): auth, chat, status, usage, invite, redeem,
threads, history, send, wait, approvals, approve, auto-approve, deny,
files, download, watch, feed, feed-status, feed-react, feed-prompt,
goals, goal, ideas, idea, idea-exec, unread, seen, session-start,
session-rename, session-pin, session-unpin, session-archive,
session-unarchive, session-delete, wake, update, fs, raw.
`auth` has a required subcommand `export`; `fs` has required
subcommands ls, tree, stat, cat, get, find, put, write, edit, mkdir,
mv, rm. No subcommand at all means `chat`. Every flag, default, and
required-ness comes from `~/workspace/plans/active/muse-cli-cli-surface.md`
— including: send `--wait` default 90, `--until-done` = 86400s and beats
`--wait`, `--wait 0` = ack only, `--ack-timeout` default 30,
`--download` ignored silently when wait is 0; wait `--timeout` default
600; history `--limit` default 10; files `--limit` default 20; download
`--limit` default 50, `-o` default `.`; watch `--timeout` default 60,
`--n` default 50; feed `--limit` default None; approve `--once/--session/
--today/--always` mutually exclusive, default `allow_once`, id optional
but id-or-`--all` required; deny has `--always` but no `--all`;
auto-approve state choices on/always/off/status, default status;
`--auto-approve`/`--no-auto-approve` mutually exclusive on send/wait.
Clap error format will differ from argparse in wording; that is an
accepted, documented difference — exit codes (2 for parse errors,
0 for --help, 0 for --version) must still match.
`auth`/`fs` with no subcommand must exit 2 (clap alone gives a
different shape; handle it explicitly in dispatch before connecting).

## Exit codes (all writers)

Auth/Value -> 2, Gateway -> 3, Timeout (gateway layer) -> 4.
wait/send: 0 done, 2 failed, 4 needs human, 124 timeout, send +5 no ack.
auth export failure -> 1, redeem failure -> 1, download not found -> 1,
download with a failed row -> 3 (after printing JSON), fs cat binary
(refused without --binary) -> 1, fs rm without --yes -> 2.
`wake` without a cookies file: NORMALISED to Auth (exit 2) — this
deliberately differs from the Python traceback/exit 1 and is listed in
README as an intentional difference.

## Hard constraints (FAIL if violated)

- Never touch `~/workspace/muse-cli/` (must stay `git status` clean),
  `~/workspace/mcp`, `~/workspace/codemode`, `~/workspace/heal`,
  `~/.local/bin`, or any file outside `~/workspace/muse-cli-rust/`
  (except /tmp scratch for self-checks, deleted afterwards).
- Never read `~/workspace/.secrets`; never use a real secret, cookie,
  or API key; never call live muse.ai or hatch.metaaivm.com with a
  real account. All live behaviour is "chưa kiểm chứng live" in README.
- No Python at runtime, no subprocess to the Python original, no
  `cargo install`, no global install, no cron/service, no publish,
  no git commit/push anywhere.
- No secret, cookie value, or token in logs, output, tests, or fixtures.
  Fixtures use obviously fake values (e.g. `hatch_sess=FAKE-…`).
- `auth export` may call the external `agent-browser` tool exactly like
  the original; if it is missing, print the original's friendly error.
  It must NOT fall back to Python.
- Path safety: fs/media paths must reject `..` components (and the
  other norm rejections in spec §(e.3)); local output paths for
  download/get must not escape the target directory via `..` or
  absolute paths in a server-supplied name — sanitise and report.
- If a feature cannot be recreated reasonably in Rust (Chrome TLS
  fingerprint via curl_cffi impersonation is the known one: reqwest/
  tungstenite cannot impersonate Chrome; live parity is therefore
  unproven, not claimed), implement the closest reasonable Rust
  behaviour, list it in README under "Khác biệt / chưa tái tạo được"
  with the reason, and list it in the writer's final report. Do NOT
  silently drop a feature and do NOT call the original to cover it.

## Self-check before reporting done (each writer, for their files)

- `cargo fmt` clean for your files.
- Copy the project to /tmp, add ONLY stub files for modules you do not
  own (stubs live in /tmp, never in the project), run
  `PATH="$HOME/.cargo/bin:$PATH" cargo check` there, and fix every
  error that belongs to your files. Report errors that belong to
  another writer's files to the coordinator instead of editing them.
- Unit tests for every pure function you wrote, following rust-testing
  (descriptive names, behaviour not implementation, error paths tested).
  `unwrap()` only in tests.
