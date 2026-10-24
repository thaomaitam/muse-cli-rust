# CLI surface

Verified 2026-10-24 by running the built binary
(`./target/debug/muse-cli --help`, `fs --help`, `auth --help`, and every
top-level command's `--help`) and cross-checking `src/cli.rs`.
If this document and `--help` disagree, `--help` (and then `src/cli.rs`)
wins — update this document.

## Counting rule

- 39 top-level commands listed by `--help`.
- `auth` and `fs` are parent commands, not leaves.
- Leaves = 37 ordinary commands + `auth export` + 12 `fs` subcommands
  = **50**.
- No subcommand means `chat` (same as the Python original:
  `args.cmd is None` -> `chat`).

```sh
./target/debug/muse-cli --version   # muse-cli 0.3.2
./target/debug/muse-cli --help      # 39 top-level commands
./target/debug/muse-cli fs --help   # 12 fs subcommands
./target/debug/muse-cli auth --help # export only
./target/debug/muse-cli <cmd> --help
```

## Top-level commands

| Command | Args / flags (defaults) | Notes |
|---|---|---|
| `auth` | (parent only; subcommand `export`) | Bare `auth` exits 2. Its only leaf is `auth export` (see below). |
| `auth export` | — | Copy the muse.ai login out of Chrome (remote debugging must be on, `agent-browser` required) into `~/.config/muse-cli/cookies.txt` (mode 600). Success prints TEXT to stdout; failure prints to stderr, exit 1. Safer file handling than Python: old file -> `.bak` set to 600 immediately, new file created already at 600, `.bak` deleted only after a successful write. |
| `chat` | (no flags) | Interactive chat; also the default with no command. Slash commands: `/help`, `/?`, `/chats`, `/open N`, `/main`, `/new [title]`, `/history [N]`, `/login`, `/exit`, `/quit`; bare `exit`/`quit` also quit. |
| `status` | — | VM, session count, unread, identity (JSON). Needs login. |
| `usage` | `--json`, `--raw` | Plan usage: % limit used, reset date, bonus tokens. `--raw` beats `--json`. |
| `invite` | `--json`, `--raw`, `--create` | Your invite code, uses left, rewards. `--raw` beats `--json`. `--create` is a write (creates only if none exists; no-op otherwise) — the read-only form omits it. |
| `redeem` | `<CODE>`, `--json` | Redeem a friend's code (within 48 h of joining). Without `--json`, a failure prints TEXT and exits 1; a malformed code exits 2. |
| `threads` | `--all` | List chats and side chats. Default drops archived; `--all` keeps them. |
| `history` | `--thread ID`, `--limit N` (10), `--raw`, `--download DIR` | Read messages. |
| `send` | `<TEXT>`, `--thread ID`, `--wait N` (90), `--until-done`, `--text`, `--ack-timeout N` (30), `--download DIR`, `--auto-approve` / `--no-auto-approve` | `--wait 0` = delivery ack only; `--until-done` = 86400 s and beats `--wait`; `--download` runs only when the wait time is non-zero. Exit: 0 done, 2 failed, 4 needs human, 5 no delivery ack, 124 timeout. |
| `wait` | `--thread ID`, `--after-seq N`, `--timeout N` (600), `--json`, `--require-running`, `--auto-approve` / `--no-auto-approve` | Exit: 0 done, 2 failed, 4 needs human, 124 timeout. `--require-running` ignores an already-finished status until a new run starts. |
| `approvals` | `--recent`, `--raw`, `--approve-all` | `--approve-all` beats `--raw`/`--recent`. |
| `approve` | `[ID]`, `--all`, `--once` / `--session` / `--today` / `--always` | Decision flags are mutually exclusive; default `allow_once`. Missing both `ID` and `--all`, or giving both, exits 2 before connecting. |
| `auto-approve` | `[STATE]` (`on`/`always`/`off`/`status`, default `status`) | `on` and `always` both write `auto_approve = "always"` to config. Always prints JSON with `auto_approve`, `source`, `config`, `env`. |
| `deny` | `<ID>`, `--always` | `deny`, or `deny_always` with `--always`. No `--all`. |
| `files` | `--thread ID`, `--limit N` (20) | Files (images/videos) in recent messages. |
| `download` | `<REF>`, `-o/--output DIR` (`.`), `--thread ID`, `--limit N` (50), `-f/--force` | `REF` is a `sandbox://…` path, a URL containing `/media/raw/`, a message id, or a history seq. Not found -> exit 1; a row with a download error -> JSON printed first, then exit 3. |
| `watch` | `--timeout N` (60), `--n N` (50), `--all`, `--thread ID` | Tail live events, one JSON line per event. Default drops `agent.status` events; `--all` keeps them. |
| `feed` | `--limit N`, `--raw` | Feed units flattened from `days[].editions[].units[]`. No limit by default; `--limit 0` counts as no limit (falsy, as in Python). |
| `feed-status` | — | Feed refresh status (JSON). |
| `feed-react` | `<UNIT>`, `[REACTION]` (`love`) | Reaction value is not validated. |
| `feed-prompt` | — | Feed prompt (JSON). |
| `goals` | — | List goals. |
| `goal` | `<ID>` | Show one goal. |
| `ideas` | — | List idea cards. |
| `idea` | `<ID>` | Show one idea. |
| `idea-exec` | `<ID>`, `--thread ID` | Body always has `ideaCardId` and `mode: "full"`, plus `session_id` when `--thread` is given. This is a write. |
| `unread` | — | Unread counts (JSON). |
| `seen` | `<THREAD>` | Mark a thread seen. |
| `session-start` | `--title TITLE` | Create a side chat. |
| `session-rename` | `<ID> <TITLE>` | Rename a session. |
| `session-pin` | `<ID>` | Pin a session. |
| `session-unpin` | `<ID>` | Unpin a session. |
| `session-archive` | `<ID>` | Archive a session. |
| `session-unarchive` | `<ID>` | Unarchive a session. |
| `session-delete` | `<ID>` | Delete a session. |
| `wake` | — | Request a VM wake; prints JSON `{"status", "body"}`. Missing cookies -> `auth error`, exit 2 (Python would traceback, exit 1). |
| `update` | — | **Check-only in the Rust build** (see `docs/parity.md`). Its `--help` text still says "upgrade this install" because it mirrors the original; the command itself reports the latest version and an upgrade instruction and never self-upgrades. It does not run the background update check. |
| `raw` | `<METHOD>`, `--body JSON`, `--param K=V` (repeatable), `--timeout N` (30) | Escape hatch for any gateway method. Unknown method, non-JSON `--body`, or `--param` without `=` all exit 2 before connecting. Aliases are NOT resolved. |
| `fs` | (see below) | Parent only; bare `fs` exits 2. All `fs` commands need login. |

## `fs` subcommands (12)

Paths are normalised to root-relative form (`""` or `/` is root,
`sandbox://` prefix stripped, `\` -> `/`); `..` collapses via normpath
at the root (e.g. `../secret` -> `secret`), matching Python — this is
parity, not an exit-2 guard. Reads/writes use 4 MiB windows.

| Command | Args / flags (defaults) | Notes |
|---|---|---|
| `fs ls` | `[PATH]`, `-l/--long`, `-a/--all`, `--json` | Default root. `-a` keeps dotfiles. |
| `fs tree` | `[PATH]`, `--depth N` (2), `-a`, `--json` | Recursive. Default depth 2 = root + one child level. Dotfile filtering applies to every path component. |
| `fs stat` | `<PATH>` | Metadata (JSON). |
| `fs cat` | `<PATH>`, `--max-bytes N`, `--binary` | Prints raw bytes to stdout. A binary file (first 8192 bytes contain `\0`) without `--binary` -> stderr, exit 1. |
| `fs get` | `<PATH>`, `-o DIR` (`.`), `-r` | Download. A directory needs `-r`; without it, exit 2. Symlink/canonicalise containment: a final destination escaping the download dir (including via a planted symlink) is refused. |
| `fs find` | `<NAME>`, `--limit N` | Name-substring search. No limit by default; `--limit 0` = unlimited. |
| `fs put` | `<LOCAL> <REMOTE>`, `-r`, `-n/--no-clobber` | Upload. A directory needs `-r`. |
| `fs write` | `<REMOTE>`, `--append`, `-n` | Write stdin to a remote file. `--append` reads the destination first and rewrites the concatenation. |
| `fs edit` | `<REMOTE>`, `--replace OLD NEW` (repeatable) | Without `--replace`, opens `$VISUAL`, then `$EDITOR`, then `vi` — by **direct exec, not via a shell** (deliberate security change; see `docs/parity.md`). `--replace` text not found -> exit 2; unchanged content -> no write. |
| `fs mkdir` | `<PATH>` | Always creates parents. |
| `fs mv` | `<SRC> <DST>`, `-f` | Move/rename; `-f` overwrites. |
| `fs rm` | `<PATH>`, `-r`, `--yes` | **Missing `--yes` -> stderr `refusing to delete without --yes`, exit 2, before any connection.** |

## Exit codes (summary)

| Situation | Exit |
|---|---|
| Success | 0 |
| Auth error (`auth error: …`), Value error (`error: …`), I/O error | 2 |
| Gateway error (`gateway error: …`) | 3 |
| Gateway timeout (`timeout: …`) | 4 |
| `wait`/`send`: turn failed | 2 |
| `wait`/`send`: needs human | 4 |
| `wait`/`send`: timeout | 124 |
| `send`: no delivery ack within `--ack-timeout` | 5 |
| `auth export` failure / missing `agent-browser` / missing `hatch_sess` | 1 |
| `redeem` without `--json` on failure; `download` file not found; `fs cat` binary without `--binary` | 1 |
| `download` with a failed row (JSON printed first) | 3 |
| Parse error (unknown command, missing required argument, conflicting flags, bad choice/value) | 2 |

Parse errors are rendered by clap, so their wording differs from the
Python argparse original; the exit code (2) is the same.
