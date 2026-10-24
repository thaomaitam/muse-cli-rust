# Architecture

`muse-cli-rust` is a Rust rewrite of the Python `muse-cli` 0.3.2
(original: `~/workspace/muse-cli/`, commit `746ae82`, read-only reference).
Package `muse-cli-rust`, binary `muse-cli`, library `muse_cli`
(`Cargo.toml`). Version: `muse-cli 0.3.2` (`--version`).

## Design decisions (binding)

- **Synchronous, no Tokio.** The Python original is synchronous, so the
  Rust code mirrors it: `reqwest` (blocking) for HTTPS, `tungstenite`
  (sync) for WebSocket, `snow` for Noise, `prost` for protobuf
  (`CONTRACT.md`, `src/lib.rs`). Do not introduce Tokio/async for one
  command without recording the reason and getting coordinator approval.
- **No Python at runtime, no calling back into the Python original.**
  `assets/` holds data only (`routes.json`, `desc0.bin`, `desc1.bin`),
  never Python code.
- **One module, one responsibility** (`src/lib.rs`, `CONTRACT.md`).
  Do not move a module's code into another module without updating
  `CONTRACT.md` first.

## Module map

| File | Responsibility |
|---|---|
| `src/lib.rs` | Library root: declares all modules (`muse_cli`). Module ownership is fixed by `CONTRACT.md`. |
| `src/main.rs` | Entry point. Restores default SIGPIPE disposition (the only `unsafe` block in production code, Unix only), installs the rustls `ring` CryptoProvider (rustls 0.23 does not pick one automatically), parses CLI, starts the background update check (except for `update`), dispatches, maps `CliError` to stderr + exit code. |
| `src/cli.rs` | Clap surface: 39 top-level commands, 50 leaf commands (see `docs/cli-surface.md`). No subcommand means `chat`. Clap's `help` subcommand is disabled at all three levels (top, `auth`, `fs`) so `--help` lists real commands only. |
| `src/config.rs` | Config dir `~/.config/muse-cli` by home expansion (no `XDG_CONFIG_HOME`, same as the original). `config.json`, `cookies.txt` (3 parse forms, in order: JSON object, `a=b; c=d` mentioning `hatch_sess=`/`datr=`, Netscape jar), auto-approve precedence flag > env > config > off. Missing `node_id` is generated (16 lowercase hex chars) and saved immediately. `save_config` is atomic (temp file + rename) but drops unknown keys. |
| `src/errors.rs` | `CliError`: `Auth` -> `auth error: …`, exit 2; `Gateway{status, payload}` -> `gateway error: …`, exit 3; `Value` -> `error: …`, exit 2; `Timeout` -> `timeout: …`, exit 4; `Io` -> `error: …`, exit 2. No `unwrap()`/`expect()` in production code (tests only). |
| `src/output.rs` | JSON output: `serde_json::to_string_pretty` (indent 2, non-ASCII unescaped), mirroring Python `json.dumps(obj, indent=2, ensure_ascii=False)`. This module never decides exit codes. |
| `src/routes.rs` | Route table loaded from `assets/routes.json` (258 entries, embedded with `include_str!`). Only `method`/`http`/`path`/`service` are kept; `noiseOnly`, `streamMode`, `responseType`, `channel`, `aliases` are ignored, and `raw` does **not** resolve aliases — same as Python. The synthetic `media.raw` route is deliberately NOT in the table; callers build it via `Gateway::request_with_route`. Path params are percent-encoded with `/` -> `%2F` (Python `quote(s, safe="")`). |
| `src/proto.rs` | Prost structs for the two descriptors. Field numbers are asserted against `assets/desc0.bin` / `desc1.bin` by `descriptors_match_struct_field_numbers` (see `docs/gateway-protocol.md`). |
| `src/noise.rs` | Noise `Noise_XX_25519_AESGCM_SHA256` initiator via `snow`, fresh random 32-byte static key per connection, empty payloads for all three handshake messages. No `unsafe`. |
| `src/gateway.rs` | HTTPS auth + WebSocket/Noise transport + unary/subscription calls + `call_json`/`call_bytes` + synthetic `media.raw` download + proxy CONNECT tunnel (see `docs/gateway-protocol.md`). One lock guards Noise state and transport together; the subscription reader thread holds it for one `_read_frame` at a time. Unary `request` discards frames of other streams (does not buffer them). |
| `src/wait.rs` | Pure `Waiter` state machine plus `run_wait` orchestration. Outcomes: Done (0), Failed (2), NeedsInput/NeedsApproval (4), Timeout (124). Approval is checked before task status in the same event. |
| `src/usage.rs` | Pure usage/invite summarising: `parse_tokens_left`, `summarize_usage`, `summarize_invite`, local-time formatting from TZif with POSIX fallback (no `chrono` in the crate). The live RSC/`find_action_id` helpers live in `gateway.rs`, not here. |
| `src/media.rs` | Pure media helpers: `media_path` (dict priority variants > path > URL), event resource extraction, local file naming. |
| `src/update.rs` | Background update check (TTY-only, 24 h cache, 1 h fail backoff, 1.5 s join) plus the **check-only** `update` command (see `docs/parity.md`). It never downloads, installs, or overwrites the binary. |
| `src/commands.rs` | `dispatch` for every command: pre-connect validation first (so e.g. bare `auth`/`fs`, `fs rm` without `--yes`, unknown `raw` method fail before any gateway connection), then gateway calls and output formatting. |
| `src/chat.rs` | Interactive chat: slash commands `/help`, `/?`, `/chats`, `/open N`, `/main`, `/new [title]`, `/history [N]`, `/login`, `/exit`, `/quit` (bare `exit`/`quit` also quit); poll 1 s, first-reply wait 300 s, follow-up quiet 10 s. |
| `src/fsclient.rs` | System Files client: path normalisation, 4 MiB read/write windows (`CHUNK`), recursive walk, symlink/canonicalise containment check on `fs get`, `fs edit` via direct editor exec (no shell). |

## Request flow

```text
main.rs (parse, SIGPIPE, CryptoProvider, update-check)
  -> commands::dispatch (pre-connect validation, exit 2 on bad input)
  -> config::load_config + load_cookies
  -> gateway::Gateway::connect
       1. cookies -> HTTPS muse.ai auth (see docs/gateway-protocol.md)
       2. WebSocket wss://hatch.metaaivm.com/v1/noise + Noise XX handshake
  -> routes::lookup + gateway::call_json / call_bytes / subscribe
  -> wait::run_wait (for send/wait/chat) or direct output
  -> output::print_json / command-specific text / raw bytes (fs cat)
```

## Conventions a new agent must know

- **JSON key order is sorted.** `serde_json::Map` sorts keys
  alphabetically; Python preserves the server's order. List order is
  preserved. JSON semantics are unchanged, but byte-for-byte output
  differs from the original. (`README.md`, "JSON key order sorted".)
- **Clap is not argparse.** Command names, flags, defaults, required
  arguments, and exit codes are kept; only help/error wording and
  formatting differ. `-V` is rejected (exit 2); `--version` works.
  Verified: clap rejects abbreviated long options
  (`usage --jso` -> `unexpected argument '--jso'`), while argparse
  accepts abbreviations by default — so scripts written against the
  Python CLI must use full flag names.
  The original has no `help` subcommand, so clap's is disabled.
- **Error taxonomy is the contract.** Do not invent a new exit code or
  print a traceback: map every failure to `CliError` and let `main.rs`
  render it. Transport/Noise/parse errors become `Io` (exit 2) by
  deliberate standardisation, where Python would traceback (exit 1).
- **`update --help` wording is a trap.** It still says "upgrade this
  install to the latest release" (mirroring the original), but the Rust
  command is check-only and says so in its own output. See
  `docs/parity.md`.
- **Config quirks, kept for parity:** `vm_id: null` counts as "key
  present" (so it is NOT seeded from `MUSE_VM_ID`); other null keys
  count as absent. `save_config` drops hand-added unknown keys.
  `MUSE_NO_UPDATE_CHECK`/`CI` set to any non-empty value — including
  `"0"` — disables the background check; empty string counts as unset.
