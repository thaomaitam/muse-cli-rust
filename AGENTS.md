# AGENTS.md — muse-cli-rust

Read this first. It is enough for a new agent to work in this repo
without prior context. Code wins over this file when they disagree;
report the disagreement instead of silently following the file.

## What this is

A Rust rewrite of `muse-cli` 0.3.2, a terminal client for a personal
muse.ai agent. The Python original is a separate repo and is read-only
reference only: `~/workspace/muse-cli/`, fork
`github.com/thoitiettxl-cyber/muse-cli`, commit `746ae82`. This repo
is based on that fork, not on upstream `nikships/muse-cli` (the fork
carries fixes upstream lacks).

- Package: `muse-cli-rust`, version `0.3.2` (`Cargo.toml`).
- Binary: `muse-cli` (`src/main.rs`).
- Library: `muse_cli` (`src/lib.rs`).
- Data, not code: `assets/routes.json` (258 gateway methods),
  `assets/desc0.bin` (`noise_transport.proto` descriptor),
  `assets/desc1.bin` (`noise_envelope.proto` descriptor).

## Architecture: synchronous, no Tokio

The Python original is synchronous, and so is this rewrite. Do not
convert the crate to async/Tokio for a single command; if async is
ever genuinely needed, record the reason first and do not convert the
whole crate.

| Area | Implementation |
|---|---|
| CLI surface | `src/cli.rs` — clap derive, 39 top-level commands, 50 leaf commands (37 regular + `auth export` + 12 `fs` subcommands). No subcommand means `chat`. |
| Config / cookies | `src/config.rs` — config dir is `~/.config/muse-cli` (home expansion, no `XDG_CONFIG_HOME`). Cookie parse accepts 3 forms, in order: JSON object, `a=b; c=d` string mentioning `hatch_sess=` or `datr=`, Netscape jar. Unparseable cookies are an auth error, never a panic. |
| Errors / exit codes | `src/errors.rs`, `src/main.rs` — Auth -> stderr `auth error: …`, exit 2; Gateway -> `gateway error: …`, exit 3; Value/Io -> `error: …`, exit 2; Timeout -> `timeout: …`, exit 4. Command handlers needing their own code (wait/send/download/…) return it as `Ok(code)`. |
| JSON output | `src/output.rs`, `src/commands.rs` — pretty, indent 2. Key order inside printed JSON objects is serde's sorted order, not Python's insertion order (`src/commands.rs`, module doc). List order is preserved. JSON semantics are unchanged. `watch` rows use Python `json.dumps` separators (`, ` and `: `), re-spaced outside JSON strings only. |
| Gateway transport | `src/gateway.rs` — HTTPS to `muse.ai` for auth/token, then WebSocket to `hatch.metaaivm.com` + Noise XX (`src/noise.rs`, `snow`), protobuf framing (`src/proto.rs`, `prost`). `src/routes.rs` builds requests from `assets/routes.json` (path params, query, headers). |
| Wait state machine | `src/wait.rs` — pure `Waiter` plus `run_wait` orchestration. |
| System Files | `src/fsclient.rs` — see "Filesystem caps" below. |
| Interactive chat | `src/chat.rs`. |
| Usage / invite parsing | `src/usage.rs`. Media path/resource helpers: `src/media.rs`. |
| Update | `src/update.rs` — background check notice + `update` command. Both are check-only (see below). |
| Internal contract | `CONTRACT.md` — the original writer contract. It is historical; `README.md` and this file are the user/agent-facing docs. |

## Build, test, verify

Cargo lives in `~/.cargo`; it is not necessarily on `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build            # debug binary: target/debug/muse-cli
cargo build --release  # release binary: target/release/muse-cli
cargo test             # full suite, offline, no login, no live network
```

Verify after any change:

```sh
./target/debug/muse-cli --version   # must print exactly: muse-cli 0.3.2
./target/debug/muse-cli --help      # 39 top-level commands
./target/debug/muse-cli fs --help   # 12 fs subcommands
./target/debug/muse-cli auth --help # only: export
```

Individual integration suites:

```sh
cargo test --test binary_cli    # binary: --help/--version, parse errors, not-logged-in
cargo test --test library_pure  # pure library: cookies, auto-approve, media, routes, chunks
cargo test --test mock_gateway  # mock gateway: unary + subscribe over an in-memory transport
cargo test --test waiter        # Waiter state machine (pure)
cargo test --test fsclient      # fs (pure): norm, CHUNK 4 MiB, fmt_size, base64, replace
```

Measured suite (re-run 2026-10-24, offline): **203 passed / 0 failed**
(lib 150 + binary_cli 13 + fsclient 6 + library_pure 20 + mock_gateway 5
+ waiter 9). An older note claiming 205 is stale; use the measured
number. Tests use a temporary empty `HOME` for binary tests and fake
values only (`hatch_sess=FAKE-…`); they never touch the real
`~/.config/muse-cli` and never call muse.ai.

## Linkage checks

Invariants of the form "defined in A must appear in B" are checked by
scripts under `scripts/checks/` — run `bash scripts/checks/run-all.sh`
before committing (it runs every check plus its fixtures; see
`scripts/checks/README.md`). Do not invent check results, and do not
claim a check passed until you have run it.
Covered pairs:

- Command list in code (`src/cli.rs`) vs `docs/cli-surface.md` vs
  `--help` output (39 top-level / 50 leaf). The command tables in
  `README.md` are NOT script-enforced: on 2026-10-24 the parent
  manually verified every leaf name appears in `README.md`, but a
  future README edit is not caught by a script — do not claim
  otherwise.
- `assets/routes.json` vs the routes the code actually calls
  (direction b2a only: every literal call site must be in
  `routes.json`. The reverse direction is intentionally NOT enforced
  — `raw` can call any of the 258 methods at runtime, so only 27
  appear as literals in code).
- `assets/desc0.bin` / `assets/desc1.bin` vs `src/proto.rs` field numbers.
- Claims in this file and `docs/` vs real behaviour; intentional
  differences vs code.

There is no CI in this repo (as in the original repo, which has no
workflows — verified: no `.github/workflows/` there). Do not add GitHub Actions workflows,
hooks, or branch-protection changes without separate approval. Local
checks are the validation owner.

## Conventions

- **Never call the Python original to implement a feature.** No
  subprocess to Python, no Python at runtime. The original is reference
  for behaviour only; when a survey/README and the original code
  disagree, the original code wins.
- **No global install.** Never `cargo install`, never copy the binary
  to `/usr/local/bin` or `~/.local/bin`. Use the full
  `target/debug/muse-cli` / `target/release/muse-cli` path, or a
  personal shell alias.
- **clap differs from argparse (accepted).** Command names, flags,
  defaults, required arguments and exit codes match the original;
  help/error wording and formatting differ. Differences to know:
  clap's auto-generated `help` subcommand is disabled at all three
  levels (top-level, `auth`, `fs`) — `src/cli.rs`
  (`disable_help_subcommand = true`); clap's auto `-V` is disabled
  and `--version` is declared explicitly (`-V` -> exit 2, `--version`
  works); clap does not accept abbreviated long options, while
  argparse does by default.
- **No subcommand means `chat`**, same as the original. Bare `auth`
  or bare `fs` -> exit 2 (pre-connect checks run before any gateway
  connection, e.g. `fs rm` without `--yes` refuses with exit 2).
- **Auto-approve precedence:** flag > `MUSE_AUTO_APPROVE` env > config
  > off. Config counts as on only when its lowercase value is exactly
  `"always"`; config `"on"` does not enable it.
- **Config files:** `~/.config/muse-cli/cookies.txt` (Netscape, mode
  600, written by `auth export`), `config.json` (`cookies_file`,
  `vm_id`, `node_id`, `auto_approve`, `subscription_action_id`),
  `chat.json` (interactive chat state), `update.json` (background
  check cache, 24 h, 1 h backoff on fetch failure).
- **Env vars:** `MUSE_VM_ID`, `MUSE_NODE_ID`, `MUSE_AUTO_APPROVE`,
  `MUSE_NO_UPDATE_CHECK` (any non-empty value, including `"0"`,
  disables; empty counts as unset), `CI` (same rule),
  `VISUAL`/`EDITOR` (for `fs edit`, `VISUAL` first, then `EDITOR`,
  then `vi`).
- **Background update-check notice:** only when both stdout and stderr
  are terminals, neither `MUSE_NO_UPDATE_CHECK` nor `CI` is set
  non-empty, and the command is not `update`. It never fails a command
  and never upgrades anything.

## Intentional differences from the Python original

Every difference is deliberate and documented in `README.md`
("Khác biệt / chưa tái tạo được"). The ones a new agent most often
trips over:

- **`update` is check-only.** It reports the newest version if it can
  be fetched and prints upgrade instructions for the install kind
  (checkout/uv/pipx/pip). It never downloads, installs, or overwrites
  the binary. The original upgrades for real via uv/pipx/pip (source
  checkout prints `git pull`). Self-upgrade is an open item, not
  something to improvise.
- **Chrome TLS fingerprint is not impersonated.** The original uses
  `curl_cffi impersonate="chrome"`; `reqwest`/`tungstenite` cannot do
  that, so this build sends a Rustls fingerprint. Live parity on this
  point is unproven — do not claim equivalence.
- **Egress proxy for WebSocket** (`src/gateway.rs`, "Egress-proxy
  support"): tungstenite connects directly, unlike curl_cffi, so on a
  host whose only outbound route is an HTTP egress proxy the
  handshake fails. The Rust code routes through the proxy with an
  HTTP CONNECT tunnel, then lets tungstenite do TLS + WebSocket over
  the tunnel. It honours `NO_PROXY`/`no_proxy` and
  `HTTPS_PROXY`/`https_proxy`/`ALL_PROXY`/`all_proxy`/`HTTP_PROXY`/
  `http_proxy` (in that order within each group). Proxy credentials
  from the proxy URL userinfo are used only for the CONNECT request's
  `Proxy-Authorization` header and are never logged, never put in an
  error message, and never sent to the gateway host. An `https://`
  proxy URL (second TLS layer) is out of scope. With no proxy
  configured this code is inert and the direct path is unchanged.
- **rustls CryptoProvider** (`src/main.rs`): rustls 0.23 does not pick
  a provider automatically in this build, and live HTTPS panicked with
  exit 101 without one. `main()` installs the ring provider once,
  before any network use. Ignoring its error is correct (it only
  means a provider was already installed).
- **SIGPIPE** (`src/main.rs`): Rust ignores SIGPIPE on startup, so a
  `println!` to a closed pipe panicked (exit 101). `main()` restores
  the default disposition, so the process dies by SIGPIPE like a
  normal Unix CLI (shell reports 141). This is the **only `unsafe`
  block in production code**; it touches no data, only the process
  signal disposition. Any new `unsafe` in production code needs a
  comment proving its invariant, and the memory-safety custom skill
  applies.
- **Filesystem caps** (`src/fsclient.rs`): `CHUNK` = 4 MiB, mirroring
  the original's `fs.py:CHUNK` / `WRITE_CHUNK`. `walk` with depth
  `None` is unlimited. `fs tree` defaults to depth 2 (root + one
  child level; `src/cli.rs`, `FsCommand::Tree`). There is no total
  byte cap.
- **`fs edit` executes the editor directly**, not via a shell
  (`shell=True` in the original). Safer, but behaviour differs for
  editors that are shell builtins or complex shell commands.
- **`wake` with missing cookies -> auth error, exit 2**, not the
  original's traceback/exit 1. Likewise malformed `config.json` ->
  Value/exit 2, and transport/Noise/parse errors -> Io/exit 2.
- **`send --text` dest split.** The original's argparse shares one
  dest between positional `text` and flag `--text`, so
  `send "hello" --text` sent boolean `True` as the message. The Rust
  version keeps positional and flag separate: the positional is always
  the message, `--text` always selects text output. This is a
  documented bug fix, not a parity gap to "restore".
- **`save_config` drops unknown keys.** The original round-trips the
  dict and keeps user-added keys; this build serialises only known
  fields (`cookies_file`, `vm_id`, `node_id`, `auto_approve`,
  `subscription_action_id`).
- **WebSocket connect has no 20 s timeout.** The original passes
  `timeout=20` to its WS connect; `tungstenite::connect` takes no
  timeout, so this step is unenforced (later handshake/read remain
  blocking as in the original).

## Verification status — what is proven, what is not

Proven offline (no login, no live network): `cargo build`, `cargo
test` 203/0, `--version`, `--help` counts above.

Reported from a prior session (2026-10-24, test account, read-only,
not re-run during this publish step): live read-only 11/11 passed —
`status`, `threads`, `history --limit 5`, `usage`, `invite` without
`--create`, `feed --limit 5`, `feed-status`, `feed-prompt`, `goals`,
`ideas`, `unread`. That session also found and fixed the two live-only
bugs behind the ring-provider and proxy items above.

**Not verified live — do not claim these work:**

- Live writes: `send`/`wait` end-to-end, `fs put`/`rm`/`write`,
  `invite --create`, `redeem`, `approve`/`deny` decisions against a
  real server, `download`.
- Interactive `chat` end-to-end.
- `auth export` with real Chrome (remote debugging, `agent-browser`).
- `update` against a newer real release, and the background
  update-check on a real tty.
- Whether the default `subscription_action_id` is still alive, and
  whether bundle scanning finds a new one.
- The 231 routes not used by the original code (258 total minus the
  27 that appear as literals in code; only method/http/
  path/service are known from `routes.json`; no body/response schema),
  full `chat.subscribe` event shapes, and server chunk-splitting
  thresholds.

## Hard limits

- Never edit, commit, push, or open a PR against the Python original
  (`~/workspace/muse-cli`) or touch `~/workspace/mcp`,
  `~/workspace/codemode`, `~/workspace/heal`.
- Never commit a secret: no `~/.config/muse-cli/cookies.txt`,
  `config.json`, cookie/token/password/proxy-password, no file from
  `~/workspace/user/files/`, no `target/`, no `*.log`. See
  `.gitignore`. Sample configs, if any are ever added, must be empty
  templates (e.g. `*.example.json`), never real values.
- Never read the content of a cookie/token/password, and never run a
  live command against muse.ai unless that specific run is explicitly
  authorised (offline `cargo build` / `cargo test` / `--help` /
  `--version` are the default verification here).
- Never install globally, change a `~/.local/bin` symlink, or create
  a cron/service for this repo.
- Do not add a PyPI install path, and do not point anything at
  upstream `nikships/muse-cli`.
