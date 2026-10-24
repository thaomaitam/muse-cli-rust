# Parity with the Python original

Original: `muse-cli` 0.3.2, `~/workspace/muse-cli/` at commit `746ae82`
(read-only). Rust: this repo. Status labels used in this document:

- **Offline** — verified without a live gateway: `cargo build`,
  `cargo test`, `--help`/`--version`/parse-error cases with a temporary
  empty `HOME`, mock-gateway tests, or direct code inspection against
  the Python source.
- **Live-readonly-11/11 (reported, not re-run here)** — the 11 read-only
  commands below were reported passing against a live gateway with a
  test account by the earlier live-test task. They were **not** re-run
  live for this docs task (this task forbids live calls), so treat them
  as prior evidence, not as a claim re-verified here.
- **Not verified live** — no live evidence exists yet. Never read this
  as "works".

## Live-readonly-11/11 (reported, not re-run here)

`status`, `threads`, `history --limit 5`, `usage`,
`invite` (without `--create`), `feed --limit 5`, `feed-status`,
`feed-prompt`, `goals`, `ideas`, `unread`.

Everything else that needs a live gateway is **not verified live**,
including every write, `chat`, `auth export`, and `update` against a
real install (see `docs/live-verification-checklist.md`).

## Per-feature parity

| Feature | Status | Notes |
|---|---|---|
| CLI surface: 39 top-level / 50 leaf, flags, defaults, required args | Offline | Counted from `--help` output + `src/cli.rs`; matches Python `cli.py` parser structure. Help/error wording differs (clap vs argparse); exit codes match. |
| Error mapping / exit codes | Offline | `src/main.rs`, `src/errors.rs`. See `docs/cli-surface.md` for the full table. |
| Config load/save, cookie parse (3 forms), auto-approve precedence | Offline | Pure tests in `tests/library_pure.rs` + `src/config.rs` unit tests. `save_config` drops unknown keys (deliberate difference, below). |
| Background update check (TTY-only, 24 h cache, 1 h fail backoff, 1.5 s join, stderr notice) | Offline | Pure helpers tested in `src/update.rs`; the TTY-gated live path is not verified live. |
| Noise XX handshake, protobuf envelopes, route table (258), `call_json` semantics, chunk reassembly/sort | Offline | Mock-gateway tests (`tests/mock_gateway.rs`, in-memory snow+prost responder) + descriptor cross-check test in `src/proto.rs`. Real-server handshake/cipher negotiation is not verified live. |
| Wait state machine (Done/Failed/NeedsInput/NeedsApproval/Timeout, exit 0/2/4/124, approval-before-status) | Offline | Pure tests in `tests/waiter.rs` + `src/wait.rs` unit tests. Live event shapes beyond the mock are not verified live. |
| `fs` pure logic: norm, 4 MiB `CHUNK`, `fmt_size`, base64, `apply_replace`, containment | Offline | `tests/fsclient.rs` + `src/fsclient.rs` unit tests. `fs` end-to-end against a real server is not verified live. |
| `usage`/`invite` pure summarising (`parse_tokens_left`, etc.), local-time formatting | Offline | Unit tests in `src/usage.rs`. The live `subscription_action_id` fallback chain is not verified live (below). |
| Media path resolution / event resources | Offline | Pure tests in `tests/library_pure.rs`. Real media download is not verified live. |
| `status`, `threads`, `history`, `usage`, `invite` (read-only), `feed`, `feed-status`, `feed-prompt`, `goals`, `ideas`, `unread` | Live-readonly-11/11 (reported, not re-run here) | See the list above. `usage`'s live subscription-action fallback was exercised only as part of that reported run, if at all — its continued validity is not verified live. |
| `send`, `wait`, `watch`, `chat`, `approvals`, `approve`, `deny`, `auto-approve` (server side), `files`, `download`, `seen`, `session-*`, `feed-react`, `idea-exec`, `goal`, `idea`, `redeem`, `raw`, all `fs` commands | Not verified live | Implemented from the Python source and covered only by offline/mock tests where applicable. `send`/`wait` exit codes and ack/timeout behaviour are implemented but unproven against a real server. |
| `auth export` (Chrome remote debugging + `agent-browser` + `hatch_sess` jar) | Not verified live | Implemented with safer file handling (below); never run against real Chrome in this project. |
| `update` against a real install | Not verified live | And, by design, it cannot do what Python's `update` does (below). |
| `wake` | Not verified live | Implemented; its missing-cookies behaviour is deliberately normalised (below). |

## Deliberate differences (not bugs, not silent omissions)

1. **`update` is check-only.** Python's `update` really upgrades via
   `uv`/`pipx`/`pip` (or prints `git pull` for a source checkout) and
   exits with the upgrade process's exit code. The Rust `update`
   (`src/update.rs`: `run_update_check_only`, `update_report`) only
   reports the latest version it could fetch and prints the upgrade
   instruction for the detected install kind (checkout/uv/pipx/pip).
   It never downloads, installs, or overwrites the binary. Its own
   output states this. Whether and how to implement a real self-upgrade
   is an open decision for the owner — do not invent one.
2. **Chrome TLS fingerprint not impersonated.** Python uses
   `curl_cffi impersonate="chrome"`; rustls sends its own fingerprint.
   Live parity is unproven (see `docs/gateway-protocol.md`).
3. **`fs edit` uses direct exec, not a shell.** Python opens the editor
   via `shell=True` with a `shlex.quote`d temp filename. Rust execs
   `$VISUAL`/`$EDITOR`/`vi` directly (`src/fsclient.rs`:
   `edit_in_editor`). Safer; behaviour differs for editors that are
   shell builtins or complex shell commands.
4. **`wake` (and `load_cookies` on a missing file) -> Auth/exit 2.**
   Python's `wake` does not check for the cookies file first, so a
   missing file raises an uncaught `FileNotFoundError` (traceback,
   exit 1). Rust normalises this to `auth error`, exit 2.
   Similarly, malformed `config.json` -> Value/exit 2 (Python:
   traceback, exit 1), and transport/Noise/parse errors -> `Io`/exit 2
   (Python: traceback, exit 1).
5. **`send --text` dest split.** Python's argparse shares one dest
   `text` between the positional and the `--text` flag, so
   `send "hello" --text` sends boolean `True` as the message. Rust
   keeps the positional as the message and `--text` as an output-mode
   flag. This fixes a Python bug deliberately.
6. **SIGPIPE restored to default.** Rust ignores SIGPIPE on startup,
   which would make a `println!` to a closed pipe panic (exit 101).
   `src/main.rs` restores the default disposition, so the process dies
   by SIGPIPE (shell reports 141), matching Unix CLI convention. This
   is the only `unsafe` block in production code; it touches no data,
   only the signal disposition. Python's behaviour here was a
   traceback (exit 1), not SIGPIPE death — this difference follows the
   system convention, not Python.
7. **rustls CryptoProvider installed in `main()`.** rustls 0.23 does not
   pick a provider automatically from crate features in this build;
   without the explicit `ring` provider install, live HTTPS panics
   (exit 101). Installing it (ignoring "already installed") is a
   build-level fix, not a behaviour difference.
8. **WebSocket egress proxy via CONNECT tunnel.** Python's `curl_cffi`
   honours the proxy environment; tungstenite does not. Rust adds a
   CONNECT tunnel for the WebSocket transport only
   (`src/gateway.rs`), honouring `NO_PROXY`, using proxy credentials
   only in the CONNECT header, never logging them. See
   `docs/gateway-protocol.md`. No connect timeout is enforced on the
   WebSocket (Python passes `timeout=20`; tungstenite takes none).
9. **fs caps, exactly as in Python:** `fs.read`/`fs.write` window
   `CHUNK` = 4 MiB (`src/fsclient.rs`); `walk` depth `None` means
   unlimited; `fs tree` default depth is 2. **There is no total-byte
   cap** in either implementation — do not add one without a new
   decision, and do not claim one exists.
10. **JSON key order sorted** (`serde_json::Map`), vs Python's
    server order. List order preserved; semantics unchanged.
11. **`save_config` drops unknown keys.** Python round-trips the whole
    dict; Rust serialises only its known fields. Hand-added extra keys
    in `config.json` are lost on the next save (only `auto-approve`
    and the `node_id`-generation path save).
12. **`auth export` file handling is safer than Python's.** Python
    renames the old file to `.bak`, writes the new file, then chmods
    600, and never chmods or deletes `.bak`. Rust sets `.bak` to 600
    immediately, creates the new file already at 600, and deletes
    `.bak` only after a successful write (a failed write leaves
    `.bak` at 600 for manual recovery).
13. **`subscription_action_id` fallback chain.** The default id
    (`4029ac5c4bd03136487d91ebde5de39c64b35229b4`, recorded
    2026-10-23) may be stale after a web-app redeploy. `usage` tries,
    in order: the id saved in config, the default id, then scanning JS
    bundles for `fetchSubscriptionAction` and retrying; only if all
    three fail does it report `Gateway(-1)`. Whether the default id is
    still alive, and whether bundle scanning finds a new one, is
    **not verified live**. The bundle scan is sequential in Rust
    (Python used a 16-thread pool); the first id found is the same,
    only slower.
14. **Other small, documented standardisations** (see `README.md`,
    "Khác biệt / chưa tái tạo được", for the full list): `wait`/`send`
    JSON carries the real `status`; `watch` rows use Python's
    `json.dumps` separators; `wake` with a non-empty non-JSON body
    returns `{"status", "body": null}` instead of erroring;
    `find_action_id` scans sequentially; usage local-time is
    best-effort from TZif with a POSIX fallback (no `chrono`);
    `vm_id: null` counts as "key present"; only `-V` is rejected
    (`--version` works); `WaitResult` gains an `approvals` field for
    the needs-approval JSON path.
15. **Clap help/error wording** differs from argparse; names, flags,
    defaults, required arguments, and exit codes do not.

## Known stale note in README.md (flagged, not fixed by this document)

The README's "Khác biệt" section still contains a note saying some
output formats "must be re-checked after writer C finishes, because
`commands.rs` did not exist when this README was written". That note is
stale: `src/commands.rs` now exists (3187 lines). The detailed per-command
output formats in this `docs/` set were checked against the current
`commands.rs` only at the `--help`/flag level and the documented
pre-connect error messages — a full line-by-line output audit against
live server responses has **not** been done and remains
**not verified live**. Do not delete the README note silently; the
owner should decide whether the per-command output audit is done
before removing it.
