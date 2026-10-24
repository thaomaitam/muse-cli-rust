# Live verification checklist

> **Nothing in this file has been verified live during the publish
> task.** This task forbids live calls to muse.ai, so every item below
> is **not verified live** in this repo's current state, except for the
> explicitly labelled prior evidence in `docs/parity.md`
> (live-readonly-11/11, reported by an earlier task, not re-run here).
> Do not mark an item done from code reading, mock tests, or a prior
> session's word alone. An item is done only when the exact command
> below was run against a real gateway, with the observed output
> recorded (command, exit code, and a redacted summary — never a
> cookie, token, or proxy password).

## Preconditions for any live run

- A live run needs explicit authorisation for that run, a test account
  first (never the main account for an unproven write), and a real
  login in `~/.config/muse-cli/` (mode 600). Never commit login files,
  never paste cookie/token values into a report, a doc, or a log.
- Record results in the run's report (not by editing this checklist
  to claim success without evidence). If a result contradicts
  `docs/parity.md`, update the parity doc with the new evidence and
  the date, and keep the old claim visible as superseded.
- If a step fails, record the exact stderr + exit code and stop
  before the destructive steps below. Destructive steps (`fs rm`,
  `session-delete`, `deny`, `redeem`) need their own explicit go-ahead.

## A. Live writes (all not verified live)

| # | Command (test account first) | Expected if parity holds | Record |
|---|---|---|---|
| A1 | `muse-cli send "hello" --wait 90` then `muse-cli wait --timeout 600` | Turn completes; exit 0 on done, 2 on failed, 4 on needs-human, 124 on timeout; replies in JSON (or text with `--text`) | exit code, `status`, reply text summary |
| A2 | `muse-cli send "hello" --wait 0` | Delivery ack only; no `--download` run | exit code, ack behaviour |
| A3 | `muse-cli send "hello" --wait 90 --download /tmp/<test-dir>` | Files produced by the reply saved under the dir | files saved, exit code |
| A4 | `muse-cli fs put <local-test-file> <remote-test-path>` then `fs cat`, `fs stat`, `fs rm <remote-test-path> --yes` | Round-trip bytes identical; `fs cat` binary guard behaves (exit 1 without `--binary` on binary content) | byte comparison, exit codes |
| A5 | `muse-cli fs write <remote-test-path>` (stdin), `fs edit <remote-test-path> --replace OLD NEW`, `fs get -r` on a test dir, `fs tree --depth 2` | Writes land; `--replace` missing text exits 2; unchanged content writes nothing; recursive get respects containment | exit codes, before/after content |
| A6 | `muse-cli invite --create` | Creates a code only if none exists; otherwise no-op | before/after code presence (redacted: presence only, never the code value) |
| A7 | `muse-cli redeem <friend-code>` | Success path and the `--json` vs TEXT failure paths (TEXT + exit 1 without `--json`; JSON even on failure with `--json`; malformed code exit 2) | exit codes, output shape |
| A8 | `muse-cli approvals`, then `approve <id>`, `deny <id>`, `approve --all --always` (on test approvals only) | Decisions land; `approve` with both/neither id and `--all` exits 2 pre-connect | exit codes, approval states |
| A9 | `muse-cli download <REF>` for a sandbox path, a `/media/raw/` URL, a message id, and a history seq | Files saved; not-found exits 1; a failed row prints JSON first then exits 3 | exit codes per REF form |
| A10 | `muse-cli files`, `muse-cli seen <thread>`, `muse-cli session-start --title <t>`, `session-rename/pin/unpin/archive/unarchive/delete` on the test session, `muse-cli feed-react <unit>`, `muse-cli idea-exec <id>` | Each lands on the server and is reflected by the corresponding list/get command | exit codes, before/after list output |
| A11 | `muse-cli raw <method> --body '{}' --timeout 30` for one known method, and `raw` with an unknown method | Known method returns server JSON; unknown method exits 2 pre-connect | exit codes |

## B. Interactive chat (not verified live)

| # | Check | Expected if parity holds | Record |
|---|---|---|---|
| B1 | `muse-cli chat` (or bare `muse-cli`), send one message, wait for a reply | Banner, first reply within 300 s (`WAIT`), follow-up quiet window 10 s (`QUIET`), poll 1 s | timings, reply received |
| B2 | Slash commands in-chat: `/help`, `/chats`, `/open N`, `/main`, `/new [title]`, `/history [N]`, `/exit` (and bare `exit`/`quit`) | Each behaves as documented in `docs/cli-surface.md` | per-command result |
| B3 | `muse-cli watch --timeout 60 --n 50` during an active turn | One JSON line per event, `agent.status` dropped by default, kept with `--all` | event count, sample shapes (redacted) |

## C. `auth export` with real Chrome (not verified live)

| # | Check | Expected if parity holds | Record |
|---|---|---|---|
| C1 | Chrome with remote debugging on, a logged-in muse.ai tab, `agent-browser` available; run `muse-cli auth export` | `~/.config/muse-cli/cookies.txt` written in Netscape form at mode 600; old file -> `.bak` at 600, deleted only after successful write; success prints TEXT to stdout | file mode, `.bak` handling, exit code |
| C2 | `auth export` with remote debugging off / no `agent-browser` / a jar without `hatch_sess` | Failure to stderr, exit 1 | stderr, exit code |
| C3 | After a successful export, `muse-cli status` | Succeeds with the new login (this re-proves the read-only path with a Chrome-derived login) | exit code |

## D. `update` and the background check (not verified live)

| # | Check | Expected if parity holds | Record |
|---|---|---|---|
| D1 | `muse-cli update` on a checkout install, and on a uv/pipx/pip install if available | Reports the latest fetchable version and the correct upgrade instruction for that install kind; **does not** download, install, or overwrite the binary | output text, binary mtime/hash unchanged |
| D2 | A command run with both stdout and stderr on a real tty, cache older than 24 h, a newer release available | Notice on stderr, command itself unaffected | notice text, command exit code |
| D3 | A command run with `MUSE_NO_UPDATE_CHECK=1` (and separately `CI=1`, and with either set to empty string) | Notice suppressed for non-empty values (including `"0"`); empty string does not suppress | notice present/absent per case |
| D4 | Fetch failure of the latest version | 1 h backoff, old cached version kept, command unaffected | `update.json` state (redacted values only) |

## E. Gateway details that only a live run can settle (not verified live)

- Chrome TLS fingerprint / extra `curl_cffi` WebSocket headers: does
  the rustls fingerprint cause any server-side rejection or behaviour
  difference? Record any rejection verbatim (redacted).
- Default `subscription_action_id` still alive? Does bundle scanning
  find a replacement id when it is stale? Record which of the three
  fallback steps succeeded.
- Server chunk-splitting threshold: at what response size does the
  server start splitting into multiple `NoiseTransportFrame`s, and
  does it ever send chunks out of order? (The client sorts by index
  defensively; the threshold itself is unknown.)
- Full `chat.subscribe` event shapes beyond the mock tests, especially
  `message.assistant` in history vs in live events, widget shapes, and
  approval payloads.
- Body/response schemas for routes the Python code never calls
  (`routes.json` gives method/http/path/service only).
- `fs` end-to-end at scale: 4 MiB chunking on a large file, `fs cat`
  binary detection on real content, `fs edit` with a real editor,
  `fs get`/`put` recursion and symlink containment against the server.
- `send` end-to-end edge cases: exit 5 on missing delivery ack,
  `--download` after reply, `wait` 124 timeout and needs-input/approval
  against a real server.

## How to record a completed item

For each item, write: date, test account (never the main account for
an unproven write), exact command, exit code, and a redacted summary
of the output. Then update `docs/parity.md` (move the item from "not
verified live" to the appropriate status with the date) and note the
change in the run report. Never edit this checklist to delete an item
that failed; record the failure and keep the item open.
