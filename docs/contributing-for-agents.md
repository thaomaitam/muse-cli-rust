# Contributing for agents

Read `AGENTS.md` (repo root) first — it is the entry map. This file
is the working procedure. Code wins over docs when they disagree;
report the disagreement instead of silently following the doc.

## 0. Hard limits (non-negotiable)

- Never edit, commit, push, or open a PR against the Python original
  (`~/workspace/muse-cli`), and never touch `~/workspace/mcp`,
  `~/workspace/codemode`, or `~/workspace/heal`.
- Never install globally (`cargo install`, copying the binary to
  `/usr/local/bin` or `~/.local/bin`), never change a
  `~/.local/bin` symlink, never create a cron/service for this repo.
- Never commit a secret: no `~/.config/muse-cli/cookies.txt` or
  `config.json`, no cookie/token/password/proxy password, no file from
  `~/workspace/user/files/`, no `target/`, no `*.log`. Sample configs
  must be empty templates, never real values.
- Never read cookie/token/password content, and never run a live
  command against muse.ai unless that specific run is explicitly
  authorised. Default verification is offline only.
- Do not add CI, hooks, or branch-protection changes without separate
  approval. Local checks are the validation owner.
- Never call the Python original to implement a feature: no subprocess
  to Python, no Python at runtime. The original is behaviour reference
  only; when a survey/README and the original code disagree, the
  original code wins.

## 1. Build, test, verify

Cargo lives in `~/.cargo` and is not necessarily on `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build            # debug binary: target/debug/muse-cli
cargo build --release  # release binary: target/release/muse-cli
cargo test             # full suite, offline, no login, no live network
```

After any change, verify:

```sh
./target/debug/muse-cli --version   # must print exactly: muse-cli 0.3.2
./target/debug/muse-cli --help      # 39 top-level commands
./target/debug/muse-cli fs --help   # 12 fs subcommands
./target/debug/muse-cli auth --help # only: export
```

Measured suite (2026-10-24, offline): **203 passed / 0 failed**
(lib 150 + binary_cli 13 + fsclient 6 + library_pure 20 + mock_gateway 5
+ waiter 9). An older note claiming 205 is stale; report the number
you actually measured. Individual suites:

```sh
cargo test --test binary_cli    # binary: --help/--version, parse errors, not-logged-in
cargo test --test library_pure  # pure library: cookies, auto-approve, media, routes, chunks
cargo test --test mock_gateway  # mock gateway: unary + subscribe over an in-memory transport
cargo test --test waiter        # Waiter state machine (pure)
cargo test --test fsclient      # fs (pure): norm, CHUNK 4 MiB, fmt_size, base64, replace
```

Binary tests run with a temporary empty `HOME` and fake values only
(`hatch_sess=FAKE-…`); they never touch the real `~/.config/muse-cli`
and never call muse.ai.

## 2. Linkage checks

Invariants of the form "defined in A must appear in B" live under
`scripts/checks/`. Run every script there before committing. As of
this writing there is no `run-all.sh` and no `scripts/checks/README.md`,
so run each script individually:

```sh
bash scripts/checks/commands-help.sh    # code vs README vs --help (39/50)
bash scripts/checks/routes-called.sh    # routes.json vs routes the code calls
bash scripts/checks/proto-descriptors.sh # desc*.bin vs src/proto.rs field numbers
bash scripts/checks/tests/fixtures/*/run-fixtures.sh  # fixture pass/fail per check
```

Rules:

- A script that reports an unfilled template, an error, or any
  offender is **not** a pass. Do not claim a check passed until you
  have run it and seen its PASS line, and do not claim a fixture
  proves anything until you have seen both its pass case and its
  fail case (the fail fixture must name its offender and exit 1).
- `commands-help.sh` also checks the docs: if you add, remove, or
  rename a command, update `docs/cli-surface.md` (and `README.md`)
  in the same change or the check will fail.
- If a new definition/usage pair is introduced (a new route, a new
  command, a new proto field, a new documented claim), add or extend
  the matching linkage check in the same change — do not leave a pair
  that can drift silently.
- Claims in `AGENTS.md` and `docs/` vs real behaviour are themselves
  a linkage pair: after changing behaviour, update the docs in the
  same change.

## 3. Conventions

- **Synchronous, no Tokio.** `reqwest` blocking, `tungstenite` sync,
  `snow`, `prost`. Do not convert the crate to async for one command;
  record the reason first and do not convert the whole crate.
- **One file, one owner** (`CONTRACT.md` is the historical writer
  contract; keep its module map current if you move code).
- **No `unwrap()`/`expect()` in production code** (tests only).
  `expect()` in production needs a comment proving the invariant.
- **Only one `unsafe` block in production code**: the SIGPIPE
  disposition restore in `src/main.rs`. Any new `unsafe` needs a
  comment proving its invariant, and the memory-safety guidance
  applies (see `AGENTS.md`).
- **Error taxonomy is the contract** (`src/errors.rs`, `src/main.rs`):
  Auth -> exit 2, Gateway -> exit 3, Value/Io -> exit 2,
  Timeout -> exit 4; command-specific codes (wait/send/download/…)
  are returned as `Ok(code)`. Never print a traceback.
- **Clap is not argparse** (accepted): names, flags, defaults,
  required arguments, and exit codes match; help/error wording,
  formatting, and long-option abbreviation differ (clap rejects
  abbreviated long options; argparse accepts them by default).
  Clap's `help` subcommand is disabled at all three levels and `-V`
  is disabled (`--version` works).
- **JSON key order is sorted** (`serde_json::Map`); list order is
  preserved. Do not "fix" key order to match Python.
- **fs caps** (`src/fsclient.rs`): `CHUNK` = 4 MiB, `walk` depth
  `None` = unlimited, `fs tree` default depth 2, no total-byte cap.
  Do not add a cap without a new decision.
- **Do not invent parity.** If a behaviour is unproven live, write
  "not verified live" and put it in
  `docs/live-verification-checklist.md`. If a claim cannot be checked
  from code, `--help`, `README.md`, or `CONTRACT.md`, write
  "chưa kiểm chứng" / "not verified" — never fill the gap with a guess.

## 4. Known flaky test (do not "fix" by deleting it)

`fsclient::tests::edit_in_editor_failure_leaves_no_temp_file` scans the
shared `/tmp` `muse-cli-edit-*` namespace and mutates the process-global
`VISUAL` env var. Rust runs tests in one process in parallel, so it
flaked (observed: before=0, after=1) when another edit test's temp file
was still present at snapshot time. The fix is a serialising lock,
not a behaviour change:

```rust
static EDIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
// held (as `_guard`) by both edit_in_editor tests in src/fsclient.rs
```

Production `edit_in_editor` already removes its temp file on every
path; the flake was in the test, not the product. If you add a test
that touches `/tmp` `muse-cli-edit-*` files or the `VISUAL`/`EDITOR`
env vars, hold the same lock. If this test flakes again, report the
before/after counts and the competing test instead of re-running until
green and calling it fixed.

## 5. Change procedure

1. Restate the outcome and check `AGENTS.md` + the relevant `docs/`
   file for authority. If a materially different choice is open
   (schedule, push target, retention, notification rule, a new cap,
   a self-upgrade design), stop and ask before creating it.
2. Make the smallest coherent change. Do not refactor, rename, or
   restyle unrelated code; mention dead code, do not delete it, and
   remove only orphans your change created.
3. Run, in order: `cargo build`, `cargo test`, the `--version` /
   `--help` verifications, then every `scripts/checks/*.sh` script
   plus its fixtures.
4. Re-check secrets: `git status` must show no `target/`, `*.log`,
   login file, or file from `~/workspace/user/files/`; grep the diff
   for cookie/token/password/proxy-password material before committing.
5. Report, in this order: what changed (files), proof (build/test/
   linkage results with real numbers, not "done"), what is still
   unproven (labelled "not verified live" where applicable), and what
   needs a decision. Plans, checklists, and completion messages are
   not proof.
