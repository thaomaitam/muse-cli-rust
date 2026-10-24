//! CLI surface (writer A: cli.rs).
//!
//! clap derive, bin name `muse-cli`, version from Cargo.toml
//! (`0.3.2`). 39 top-level commands; 50 leaf commands (37 regular +
//! `auth export` + 12 `fs` subcommands). No subcommand at all means
//! `chat` — `main.rs`/dispatch treats `command == None` as chat, so
//! this enum must (and does) represent `None`.
//!
//! `auth` / `fs` with no subcommand are represented as
//! `Auth { op: None }` / `Fs { op: None }` so dispatch can report
//! exit 2 BEFORE connecting (the original's argparse `required=True`
//! also exits 2 there). Clap alone would accept the bare parent, so
//! dispatch must check [`Command::missing_subcommand`].
//!
//! Flag/default/mutual-exclusion rules come from
//! `~/workspace/plans/active/muse-cli-cli-surface.md` and the
//! original `cli.py:main()` parser. Help texts are copied from the
//! original where the original has one, and omitted where it has
//! none, so `--help` stays close to the original (clap's formatting
//! differs from argparse's; that is an accepted, documented
//! difference — exit codes still match: 2 for parse errors, 0 for
//! `--help`/`--version`).
//!
//! INTENTIONAL DEVIATION — `send --text` (verified against the
//! original's argparse behaviour, see the test
//! `send_text_flag_does_not_clobber_the_message`):
//! the original defines positional `text` AND `--text` (store_true)
//! with the same argparse dest `text`, so `send "hello" --text`
//! sends boolean `True` as the message, while `send --text "hello"`
//! sends `"hello"` but loses text mode. The Rust surface separates
//! them: positional `text` is always the message, flag `--text`
//! (field `text_output`) always selects text output. Writer C must
//! send the positional text in both cases.

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

/// Top-level parser. `command` is `None` exactly when the user gave
/// no subcommand, which means `chat` (original: `args.cmd is None`
/// -> `chat`).
#[derive(Debug, Clone, PartialEq, Parser)]
#[command(
    name = "muse-cli",
    version,
    about = "CLI for your personal muse.ai agent",
    // The Python original (argparse) has no `help` subcommand, so
    // clap must not add one at any level (see AuthCommand/FsCommand).
    disable_help_subcommand = true,
    // The original has only `--version` (argparse defines it
    // explicitly); clap's auto `-V` is disabled and `--version` is
    // re-added explicitly below, so `-V` is an unknown flag (exit 2)
    // while `--version` still prints `muse-cli 0.3.2` (exit 0).
    disable_version_flag = true
)]
pub struct Cli {
    #[arg(long, action = ArgAction::Version)]
    version: Option<bool>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Parse from the process arguments (called by `main.rs`).
pub fn parse_cli() -> Cli {
    Cli::parse()
}

/// Value for `auto-approve`'s positional `state` (original choices:
/// `on`, `always`, `off`, `status`; default `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum AutoApproveState {
    /// Write `auto_approve = "always"` to config.
    On,
    /// Alias of `on`.
    Always,
    /// Write `auto_approve = "off"` to config.
    Off,
    /// No write; just report the current setting.
    #[default]
    Status,
}

/// All top-level commands (39).
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Command {
    /// Auth helpers.
    Auth {
        #[command(subcommand)]
        op: Option<AuthCommand>,
    },
    /// Interactive chat (the default with no command).
    #[command(about = "interactive chat (the default with no command)")]
    Chat,
    /// VM, session count, unread, identity.
    #[command(about = "VM, session count, unread, identity")]
    Status,
    /// Plan usage: weekly limit % used, reset date, bonus tokens.
    #[command(about = "plan usage: weekly limit % used, reset date, bonus tokens")]
    Usage {
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
        /// Print the server's full subscription object.
        #[arg(long)]
        raw: bool,
    },
    /// Your invite code, uses left, rewards earned (read-only).
    #[command(about = "your invite code, uses left, rewards earned (read-only)")]
    Invite {
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
        /// Print the server's full invite object.
        #[arg(long)]
        raw: bool,
        /// Create your code first if you have none (a write; no-op if one exists).
        #[arg(long)]
        create: bool,
    },
    /// Redeem a friend's invite code (within 48h of joining).
    #[command(about = "redeem a friend's invite code (within 48h of joining)")]
    Redeem {
        code: String,
        #[arg(long)]
        json: bool,
    },
    /// List chats and side chats.
    #[command(about = "list chats and side chats")]
    Threads {
        /// Include archived.
        #[arg(long = "all")]
        archived: bool,
    },
    /// Read chat messages.
    #[command(about = "read chat messages")]
    History {
        #[arg(long)]
        thread: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        #[arg(long)]
        raw: bool,
        /// Also save files (images/videos) from these messages into DIR.
        #[arg(long, value_name = "DIR")]
        download: Option<String>,
    },
    /// Send the agent a message.
    #[command(about = "send the agent a message")]
    Send {
        text: String,
        #[arg(long)]
        thread: Option<String>,
        /// Max seconds to wait for the turn to finish (0 = only wait for delivery).
        #[arg(long, default_value_t = 90)]
        wait: i64,
        /// Wait with no practical time limit.
        #[arg(long)]
        until_done: bool,
        /// Print the final reply as plain text instead of JSON.
        #[arg(long = "text")]
        text_output: bool,
        /// Seconds to wait for the delivery ack.
        #[arg(long, default_value_t = 30)]
        ack_timeout: i64,
        /// Save files the reply produced into DIR (needs --wait).
        #[arg(long, value_name = "DIR")]
        download: Option<String>,
        /// Approve pending approvals with allow_always and keep waiting.
        #[arg(long = "auto-approve", conflicts_with = "no_auto_approve")]
        auto_approve: bool,
        /// Stop with exit 4 on approvals even if auto-approve is on.
        #[arg(long = "no-auto-approve")]
        no_auto_approve: bool,
    },
    /// Block until the agent's turn ends (0 ok, 2 failed, 4 needs human, 124 timeout).
    #[command(
        about = "block until the agent's turn ends (0 ok, 2 failed, 4 needs human, 124 timeout)"
    )]
    Wait {
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        after_seq: Option<i64>,
        #[arg(long, default_value_t = 600)]
        timeout: i64,
        #[arg(long)]
        json: bool,
        /// Ignore an already-finished status until a new run starts.
        #[arg(long)]
        require_running: bool,
        /// Approve pending approvals with allow_always and keep waiting.
        #[arg(long = "auto-approve", conflicts_with = "no_auto_approve")]
        auto_approve: bool,
        /// Stop with exit 4 on approvals even if auto-approve is on.
        #[arg(long = "no-auto-approve")]
        no_auto_approve: bool,
    },
    /// List pending approvals (network access etc.).
    #[command(about = "list pending approvals (network access etc.)")]
    Approvals {
        #[arg(long)]
        recent: bool,
        #[arg(long)]
        raw: bool,
        /// Approve every pending approval with allow_always.
        #[arg(long)]
        approve_all: bool,
    },
    /// Approve a pending request (or --all).
    #[command(about = "approve a pending request (or --all)")]
    Approve {
        id: Option<String>,
        /// Approve every pending approval.
        #[arg(long)]
        all: bool,
        #[arg(long, conflicts_with_all = ["session", "today", "always"])]
        once: bool,
        #[arg(long, conflicts_with_all = ["once", "today", "always"])]
        session: bool,
        #[arg(long, conflicts_with_all = ["once", "session", "always"])]
        today: bool,
        #[arg(long, conflicts_with_all = ["once", "session", "today"])]
        always: bool,
    },
    /// Let wait/send approve every approval with allow_always (on|off|status).
    #[command(about = "let wait/send approve every approval with allow_always (on|off|status)")]
    AutoApprove {
        #[arg(value_enum, default_value_t = AutoApproveState::Status)]
        state: AutoApproveState,
    },
    /// Deny a pending request.
    #[command(about = "deny a pending request")]
    Deny {
        id: String,
        #[arg(long)]
        always: bool,
    },
    /// List files (images/videos) in recent messages.
    #[command(about = "list files (images/videos) in recent messages")]
    Files {
        #[arg(long)]
        thread: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Download a file the agent produced.
    #[command(about = "download a file the agent produced")]
    Download {
        /// Sandbox:// path, media URL, message id, or history seq.
        #[arg(value_name = "REF")]
        reference: String,
        /// Directory to save into (default .).
        #[arg(short = 'o', long = "output", default_value = ".")]
        output: String,
        /// Side chat to search for a message id/seq.
        #[arg(long)]
        thread: Option<String>,
        /// Messages to search for a message id/seq.
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Overwrite existing files.
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Tail live agent events.
    #[command(about = "tail live agent events")]
    Watch {
        #[arg(long, default_value_t = 60)]
        timeout: i64,
        #[arg(long, default_value_t = 50)]
        n: i64,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        thread: Option<String>,
    },
    /// List feed units.
    #[command(about = "list feed units")]
    Feed {
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        raw: bool,
    },
    /// Feed refresh status.
    #[command(about = "feed refresh status")]
    FeedStatus,
    /// React to a feed unit.
    #[command(about = "react to a feed unit")]
    FeedReact {
        unit: String,
        #[arg(default_value = "love")]
        reaction: String,
    },
    /// Show feed prompt.
    #[command(about = "show feed prompt")]
    FeedPrompt,
    /// List goals.
    #[command(about = "list goals")]
    Goals,
    /// Show one goal.
    #[command(about = "show one goal")]
    Goal { id: String },
    /// List idea cards.
    #[command(about = "list idea cards")]
    Ideas,
    /// Show one idea.
    #[command(about = "show one idea")]
    Idea { id: String },
    /// Execute an idea (agent acts on it).
    #[command(about = "execute an idea (agent acts on it)")]
    IdeaExec {
        id: String,
        #[arg(long)]
        thread: Option<String>,
    },
    /// Unread counts.
    #[command(about = "unread counts")]
    Unread,
    /// Mark a thread seen.
    #[command(about = "mark a thread seen")]
    Seen { thread: String },
    /// Start a new side chat.
    #[command(about = "start a new side chat")]
    SessionStart {
        #[arg(long)]
        title: Option<String>,
    },
    /// Rename a session.
    #[command(about = "rename a session")]
    SessionRename { id: String, title: String },
    /// Pin a session.
    #[command(about = "pin a session")]
    SessionPin { id: String },
    /// Unpin a session.
    #[command(about = "unpin a session")]
    SessionUnpin { id: String },
    /// Archive a session.
    #[command(about = "archive a session")]
    SessionArchive { id: String },
    /// Unarchive a session.
    #[command(about = "unarchive a session")]
    SessionUnarchive { id: String },
    /// Delete a session.
    #[command(about = "delete a session")]
    SessionDelete { id: String },
    /// Request a VM wake.
    #[command(about = "request a VM wake")]
    Wake,
    /// Upgrade this install to the latest release (CHECK-ONLY in the
    /// Rust build: reports the latest version and prints the upgrade
    /// instruction; it never downloads or installs anything).
    #[command(about = "upgrade this install to the latest release")]
    Update,
    /// Browse/read/write Muse's System Files (agent home).
    #[command(about = "browse/read/write Muse's System Files (agent home)")]
    Fs {
        #[command(subcommand)]
        op: Option<FsCommand>,
    },
    /// Call any gateway method (escape hatch).
    #[command(about = "call any gateway method (escape hatch)")]
    Raw {
        method: String,
        #[arg(long)]
        body: Option<String>,
        /// Path param k=v (repeatable).
        #[arg(long, action = ArgAction::Append)]
        param: Vec<String>,
        #[arg(long, default_value_t = 30)]
        timeout: i64,
    },
}

/// Subcommands of `auth` (1 leaf: `export`).
#[derive(Debug, Clone, PartialEq, Subcommand)]
#[command(disable_help_subcommand = true)]
pub enum AuthCommand {
    /// Copy the muse.ai login out of Chrome (remote debugging must be on).
    #[command(
        about = "copy the muse.ai login out of Chrome (remote debugging must be on)",
        long_about = "Copy your muse.ai login out of Google Chrome into\n~/.config/muse-cli/cookies.txt (mode 600).\n\nChrome shares those cookies only after remote debugging is on.\nIn the Chrome window you already use for muse.ai:\n\n  1. Open chrome://inspect/#remote-debugging\n  2. Turn on remote debugging\n     (\"Allow remote debugging for this browser instance\")\n  3. Open https://muse.ai/ and log in. Leave that tab open.\n  4. Run this command. If Chrome asks to allow the connection,\n     click Allow, then run it again if it failed before the click.\n\nRequires agent-browser: npm i -g agent-browser\n(install Node.js LTS from https://nodejs.org first if npm is missing).\n\nStay in that same Chrome. Starting another Chrome with\n--remote-debugging-port uses a different profile and has no muse.ai login."
    )]
    Export,
}

/// Subcommands of `fs` (12 leaves).
#[derive(Debug, Clone, PartialEq, Subcommand)]
#[command(disable_help_subcommand = true)]
pub enum FsCommand {
    /// List a folder (default: System Files root).
    #[command(about = "list a folder (default: System Files root)")]
    Ls {
        #[arg(default_value = "")]
        path: String,
        #[arg(short = 'l', long = "long")]
        long: bool,
        /// Include dotfiles.
        #[arg(short = 'a', long = "all")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Recursive listing.
    #[command(about = "recursive listing")]
    Tree {
        #[arg(default_value = "")]
        path: String,
        #[arg(long, default_value_t = 2)]
        depth: i64,
        #[arg(short = 'a', long = "all")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// File/folder metadata.
    #[command(about = "file/folder metadata")]
    Stat { path: String },
    /// Print a file.
    #[command(about = "print a file")]
    Cat {
        path: String,
        #[arg(long)]
        max_bytes: Option<i64>,
        #[arg(long)]
        binary: bool,
    },
    /// Download a file (or folder with -r).
    #[command(about = "download a file (or folder with -r)")]
    Get {
        path: String,
        #[arg(short = 'o', long = "output", default_value = ".")]
        output: String,
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
    },
    /// Search by name substring.
    #[command(about = "search by name substring")]
    Find {
        name: String,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Upload a local file (or folder with -r).
    #[command(about = "upload a local file (or folder with -r)")]
    Put {
        local: String,
        remote: String,
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        #[arg(short = 'n', long = "no-clobber")]
        no_clobber: bool,
    },
    /// Write stdin to a remote file.
    #[command(about = "write stdin to a remote file")]
    Write {
        remote: String,
        #[arg(long)]
        append: bool,
        #[arg(short = 'n', long = "no-clobber")]
        no_clobber: bool,
    },
    /// Edit a remote file in $EDITOR or with --replace OLD NEW.
    #[command(about = "edit a remote file in $EDITOR or with --replace OLD NEW")]
    Edit {
        remote: String,
        /// Flat list of OLD/NEW pairs (clap cannot parse
        /// `Vec<Vec<String>>`); each `--replace` occurrence appends
        /// exactly 2 values, so the length is always even. Use
        /// [`FsCommand::replace_pairs`] to read them as pairs.
        #[arg(long = "replace", value_names = ["OLD", "NEW"], num_args = 2, action = ArgAction::Append)]
        replace: Vec<String>,
    },
    /// Create a folder (with parents).
    #[command(about = "create a folder (with parents)")]
    Mkdir { path: String },
    /// Move/rename.
    #[command(about = "move/rename")]
    Mv {
        src: String,
        dst: String,
        /// Overwrite destination.
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Delete (requires --yes).
    #[command(about = "delete (requires --yes)")]
    Rm {
        path: String,
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        #[arg(long)]
        yes: bool,
    },
}

impl FsCommand {
    /// `--replace` pairs for `fs edit`, as `(OLD, NEW)` pairs in
    /// order. `None` for other `fs` subcommands.
    pub fn replace_pairs(&self) -> Option<Vec<(&str, &str)>> {
        match self {
            FsCommand::Edit { replace, .. } => Some(
                replace
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| (pair[0].as_str(), pair[1].as_str()))
                    .collect(),
            ),
            _ => None,
        }
    }
}

impl Command {
    /// Display name of the leaf command (`"auth export"`,
    /// `"fs ls"`, `"send"`, …). `None` at the top level has no name;
    /// use `None` handling in `main.rs`/dispatch for the chat default.
    pub fn leaf_name(&self) -> &'static str {
        match self {
            Command::Auth { .. } => "auth export",
            Command::Chat => "chat",
            Command::Status => "status",
            Command::Usage { .. } => "usage",
            Command::Invite { .. } => "invite",
            Command::Redeem { .. } => "redeem",
            Command::Threads { .. } => "threads",
            Command::History { .. } => "history",
            Command::Send { .. } => "send",
            Command::Wait { .. } => "wait",
            Command::Approvals { .. } => "approvals",
            Command::Approve { .. } => "approve",
            Command::AutoApprove { .. } => "auto-approve",
            Command::Deny { .. } => "deny",
            Command::Files { .. } => "files",
            Command::Download { .. } => "download",
            Command::Watch { .. } => "watch",
            Command::Feed { .. } => "feed",
            Command::FeedStatus => "feed-status",
            Command::FeedReact { .. } => "feed-react",
            Command::FeedPrompt => "feed-prompt",
            Command::Goals => "goals",
            Command::Goal { .. } => "goal",
            Command::Ideas => "ideas",
            Command::Idea { .. } => "idea",
            Command::IdeaExec { .. } => "idea-exec",
            Command::Unread => "unread",
            Command::Seen { .. } => "seen",
            Command::SessionStart { .. } => "session-start",
            Command::SessionRename { .. } => "session-rename",
            Command::SessionPin { .. } => "session-pin",
            Command::SessionUnpin { .. } => "session-unpin",
            Command::SessionArchive { .. } => "session-archive",
            Command::SessionUnarchive { .. } => "session-unarchive",
            Command::SessionDelete { .. } => "session-delete",
            Command::Wake => "wake",
            Command::Update => "update",
            Command::Fs { op } => match op {
                Some(FsCommand::Ls { .. }) => "fs ls",
                Some(FsCommand::Tree { .. }) => "fs tree",
                Some(FsCommand::Stat { .. }) => "fs stat",
                Some(FsCommand::Cat { .. }) => "fs cat",
                Some(FsCommand::Get { .. }) => "fs get",
                Some(FsCommand::Find { .. }) => "fs find",
                Some(FsCommand::Put { .. }) => "fs put",
                Some(FsCommand::Write { .. }) => "fs write",
                Some(FsCommand::Edit { .. }) => "fs edit",
                Some(FsCommand::Mkdir { .. }) => "fs mkdir",
                Some(FsCommand::Mv { .. }) => "fs mv",
                Some(FsCommand::Rm { .. }) => "fs rm",
                None => "fs",
            },
            Command::Raw { .. } => "raw",
        }
    }

    /// True for bare `auth` / `fs` with no subcommand. Dispatch must
    /// reject these with exit 2 before connecting (the original's
    /// argparse `required=True` behaviour).
    pub fn missing_subcommand(&self) -> bool {
        matches!(self, Command::Auth { op: None } | Command::Fs { op: None })
    }

    /// The per-call auto-approve flag for `send`/`wait`:
    /// `Some(true)` for `--auto-approve`, `Some(false)` for
    /// `--no-auto-approve`, `None` when neither was given (fall
    /// through to env/config). `None` for all other commands.
    pub fn auto_approve_flag(&self) -> Option<bool> {
        match self {
            Command::Send {
                auto_approve,
                no_auto_approve,
                ..
            }
            | Command::Wait {
                auto_approve,
                no_auto_approve,
                ..
            } => {
                if *auto_approve {
                    Some(true)
                } else if *no_auto_approve {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The decision string for `approve` (`allow_once` default).
    /// `None` for other commands.
    pub fn approve_decision(&self) -> Option<&'static str> {
        match self {
            Command::Approve {
                session,
                today,
                always,
                ..
            } => {
                if *always {
                    Some("allow_always")
                } else if *today {
                    Some("allow_today")
                } else if *session {
                    Some("allow_session")
                } else {
                    // `once` set, or no flag at all: the original
                    // defaults to `allow_once`.
                    Some("allow_once")
                }
            }
            _ => None,
        }
    }

    /// The decision string for `deny` (`deny` / `deny_always`).
    /// `None` for other commands.
    pub fn deny_decision(&self) -> Option<&'static str> {
        match self {
            Command::Deny { always, .. } => {
                if *always {
                    Some("deny_always")
                } else {
                    Some("deny")
                }
            }
            _ => None,
        }
    }

    /// The 50 leaf command names (37 regular + `auth export` + 12
    /// `fs` subcommands), in CLI-surface order.
    pub fn leaf_names() -> Vec<&'static str> {
        vec![
            "chat",
            "status",
            "usage",
            "invite",
            "redeem",
            "threads",
            "history",
            "send",
            "wait",
            "approvals",
            "approve",
            "auto-approve",
            "deny",
            "files",
            "download",
            "watch",
            "feed",
            "feed-status",
            "feed-react",
            "feed-prompt",
            "goals",
            "goal",
            "ideas",
            "idea",
            "idea-exec",
            "unread",
            "seen",
            "session-start",
            "session-rename",
            "session-pin",
            "session-unpin",
            "session-archive",
            "session-unarchive",
            "session-delete",
            "wake",
            "update",
            "auth export",
            "fs ls",
            "fs tree",
            "fs stat",
            "fs cat",
            "fs get",
            "fs find",
            "fs put",
            "fs write",
            "fs edit",
            "fs mkdir",
            "fs mv",
            "fs rm",
            "raw",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(args).unwrap_err()
    }

    /// Recursively collect leaf command paths from clap's factory.
    fn leaves(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        for sub in cmd.get_subcommands() {
            let name = if prefix.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{prefix} {}", sub.get_name())
            };
            if sub.get_subcommands().next().is_none() {
                out.push(name);
            } else {
                leaves(sub, &name, out);
            }
        }
    }

    #[test]
    fn leaf_count_is_50_and_matches_the_static_list() {
        let mut from_clap = Vec::new();
        leaves(&Cli::command(), "", &mut from_clap);
        let mut expected: Vec<String> = Command::leaf_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        from_clap.sort();
        expected.sort();
        assert_eq!(from_clap.len(), 50);
        assert_eq!(from_clap, expected);
    }

    #[test]
    fn top_level_count_is_39() {
        assert_eq!(Cli::command().get_subcommands().count(), 39);
    }

    #[test]
    fn no_subcommand_means_chat_default() {
        let cli = parse(&["muse-cli"]);
        assert_eq!(cli.command, None);
    }

    #[test]
    fn bare_auth_and_fs_are_representable_for_dispatch_exit_2() {
        let cli = parse(&["muse-cli", "auth"]);
        assert!(cli.command.as_ref().unwrap().missing_subcommand());
        let cli = parse(&["muse-cli", "fs"]);
        assert!(cli.command.as_ref().unwrap().missing_subcommand());
        let cli = parse(&["muse-cli", "auth", "export"]);
        assert!(!cli.command.as_ref().unwrap().missing_subcommand());
        assert_eq!(cli.command.as_ref().unwrap().leaf_name(), "auth export");
    }

    #[test]
    fn send_defaults_match_the_original() {
        let cli = parse(&["muse-cli", "send", "hello"]);
        match cli.command.unwrap() {
            Command::Send {
                text,
                wait,
                ack_timeout,
                until_done,
                auto_approve,
                no_auto_approve,
                ..
            } => {
                assert_eq!(text, "hello");
                assert_eq!(wait, 90);
                assert_eq!(ack_timeout, 30);
                assert!(!until_done);
                assert!(!auto_approve);
                assert!(!no_auto_approve);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn send_text_flag_does_not_clobber_the_message() {
        // The original's argparse dest collision made
        // `send "hello" --text` send True; here the message survives
        // and text mode is still selected.
        let cli = parse(&["muse-cli", "send", "hello", "--text"]);
        match cli.command.unwrap() {
            Command::Send {
                text, text_output, ..
            } => {
                assert_eq!(text, "hello");
                assert!(text_output);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn wait_history_files_download_watch_defaults() {
        let cli = parse(&["muse-cli", "wait"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Wait { timeout: 600, .. }
        ));
        let cli = parse(&["muse-cli", "history"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::History { limit: 10, .. }
        ));
        let cli = parse(&["muse-cli", "files"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Files { limit: 20, .. }
        ));
        let cli = parse(&["muse-cli", "download", "sandbox://workspace/a.png"]);
        match cli.command.unwrap() {
            Command::Download {
                output,
                limit,
                force,
                ..
            } => {
                assert_eq!(output, ".");
                assert_eq!(limit, 50);
                assert!(!force);
            }
            other => panic!("wrong command: {other:?}"),
        }
        let cli = parse(&["muse-cli", "watch"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Watch {
                timeout: 60,
                n: 50,
                ..
            }
        ));
        let cli = parse(&["muse-cli", "raw", "chat.history"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Raw { timeout: 30, .. }
        ));
    }

    #[test]
    fn fs_defaults_match_the_original() {
        let cli = parse(&["muse-cli", "fs", "ls"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Fs {
                op: Some(FsCommand::Ls { path, .. })
            } if path.is_empty()
        ));
        let cli = parse(&["muse-cli", "fs", "tree"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Fs {
                op: Some(FsCommand::Tree { depth: 2, .. })
            }
        ));
        let cli = parse(&["muse-cli", "fs", "get", "a.txt"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Fs {
                op: Some(FsCommand::Get { output, .. })
            } if output == "."
        ));
        let cli = parse(&[
            "muse-cli",
            "fs",
            "edit",
            "a.txt",
            "--replace",
            "old",
            "new",
            "--replace",
            "x",
            "y",
        ]);
        match cli.command.unwrap() {
            Command::Fs {
                op: Some(ref op @ FsCommand::Edit { .. }),
            } => {
                assert_eq!(
                    op.replace_pairs().unwrap(),
                    vec![("old", "new"), ("x", "y")]
                );
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn threads_all_maps_to_archived_and_feed_react_defaults_to_love() {
        let cli = parse(&["muse-cli", "threads", "--all"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::Threads { archived: true }
        ));
        let cli = parse(&["muse-cli", "feed-react", "unit-1"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::FeedReact { reaction, .. } if reaction == "love"
        ));
        let cli = parse(&["muse-cli", "auto-approve"]);
        assert!(matches!(
            cli.command.unwrap(),
            Command::AutoApprove {
                state: AutoApproveState::Status
            }
        ));
    }

    #[test]
    fn mutually_exclusive_flags_are_parse_errors() {
        let err = parse_err(&["muse-cli", "approve", "id-1", "--once", "--always"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = parse_err(&[
            "muse-cli",
            "send",
            "hi",
            "--auto-approve",
            "--no-auto-approve",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = parse_err(&["muse-cli", "wait", "--auto-approve", "--no-auto-approve"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn missing_required_positionals_and_bad_choices_are_parse_errors() {
        assert!(parse_err(&["muse-cli", "redeem"])
            .to_string()
            .contains("required"));
        assert!(parse_err(&["muse-cli", "deny"])
            .to_string()
            .contains("required"));
        assert!(parse_err(&["muse-cli", "goal"])
            .to_string()
            .contains("required"));
        assert!(parse_err(&["muse-cli", "session-rename", "only-id"])
            .to_string()
            .contains("required"));
        let err = parse_err(&["muse-cli", "auto-approve", "bogus"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        let err = parse_err(&["muse-cli", "history", "--limit", "abc"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        let err = parse_err(&["muse-cli", "not-a-command"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        let err = parse_err(&["muse-cli", "status", "extra"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn approve_and_deny_decision_helpers() {
        let cli = parse(&["muse-cli", "approve", "id-1"]);
        assert_eq!(
            cli.command.as_ref().unwrap().approve_decision(),
            Some("allow_once")
        );
        let cli = parse(&["muse-cli", "approve", "--all", "--always"]);
        assert_eq!(
            cli.command.as_ref().unwrap().approve_decision(),
            Some("allow_always")
        );
        let cli = parse(&["muse-cli", "deny", "id-1", "--always"]);
        assert_eq!(
            cli.command.as_ref().unwrap().deny_decision(),
            Some("deny_always")
        );
        let cli = parse(&["muse-cli", "deny", "id-1"]);
        assert_eq!(cli.command.as_ref().unwrap().deny_decision(), Some("deny"));
        let cli = parse(&["muse-cli", "send", "hi", "--auto-approve"]);
        assert_eq!(
            cli.command.as_ref().unwrap().auto_approve_flag(),
            Some(true)
        );
        let cli = parse(&["muse-cli", "wait", "--no-auto-approve"]);
        assert_eq!(
            cli.command.as_ref().unwrap().auto_approve_flag(),
            Some(false)
        );
        let cli = parse(&["muse-cli", "wait"]);
        assert_eq!(cli.command.as_ref().unwrap().auto_approve_flag(), None);
    }

    #[test]
    fn raw_param_accumulates_and_leaf_names_cover_fs() {
        let cli = parse(&[
            "muse-cli",
            "raw",
            "chat.history",
            "--param",
            "a=1",
            "--param",
            "b=2",
        ]);
        match cli.command.unwrap() {
            Command::Raw { param, .. } => {
                assert_eq!(param, vec!["a=1".to_string(), "b=2".to_string()]);
            }
            other => panic!("wrong command: {other:?}"),
        }
        assert_eq!(Command::leaf_names().len(), 50);
        assert!(Command::leaf_names().contains(&"fs rm"));
        assert!(Command::leaf_names().contains(&"auth export"));
    }
}
