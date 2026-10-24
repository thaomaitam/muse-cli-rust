//! Binary-level integration tests for `muse-cli`.
//!
//! Group A (parse errors) and group B (bare parents / missing guards)
//! belong to the NOT-LOGGED-IN group: they are decided by clap or by
//! dispatch checks that run BEFORE any gateway connection, so they
//! must pass with an empty HOME and no cookies. Group C is the
//! not-logged-in group proper: with an empty HOME the command must
//! fail with `auth error:` on stderr and exit 2.
//!
//! Every test here spawns the real binary through the
//! `CARGO_BIN_EXE_muse-cli` / `CARGO_BIN_EXE_muse_cli` path that cargo
//! sets for integration tests, with HOME pointed at a fresh empty temp
//! directory. The real HOME and `~/.config` are never touched, no live
//! network is used, and no fixture contains a real secret or cookie.

use muse_cli::cli::Command;
use std::path::PathBuf;
use std::process::{Command as Proc, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn bin_path() -> PathBuf {
    // Cargo spells the variable with the binary name; dashes may be
    // underscores depending on the cargo version, so try both.
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_muse_cli") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_muse-cli") {
        return PathBuf::from(path);
    }
    panic!("CARGO_BIN_EXE_muse-cli not set (run via `cargo test`)");
}

/// Fresh empty directory to use as HOME for one test.
fn fresh_home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "muse-cli-rust-bin-test-{tag}-{pid}-{n}",
        pid = std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], home: &std::path::Path) -> Output {
    Proc::new(bin_path())
        .args(args)
        .env("HOME", home)
        .env("MUSE_NO_UPDATE_CHECK", "1")
        .env("CI", "1")
        .env_remove("MUSE_VM_ID")
        .env_remove("MUSE_NODE_ID")
        .env_remove("MUSE_AUTO_APPROVE")
        .output()
        .unwrap()
}

/// Like `run`, but with extra env vars applied (after the removals
/// above, so a test can set e.g. `MUSE_AUTO_APPROVE` deliberately).
fn run_with_env(args: &[&str], home: &std::path::Path, extra: &[(&str, &str)]) -> Output {
    let mut cmd = Proc::new(bin_path());
    cmd.args(args)
        .env("HOME", home)
        .env("MUSE_NO_UPDATE_CHECK", "1")
        .env("CI", "1")
        .env_remove("MUSE_VM_ID")
        .env_remove("MUSE_NODE_ID")
        .env_remove("MUSE_AUTO_APPROVE");
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit_2(output: &Output, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "{what}: expected exit 2, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
    assert!(
        !stderr(output).is_empty(),
        "{what}: expected an error on stderr"
    );
}

/// Names listed under the `Commands:` section of a clap `--help`.
fn help_command_names(args: &[&str], home: &std::path::Path) -> Vec<String> {
    let output = run(args, home);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{args:?}: --help must exit 0\nstderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in text.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            if line.trim().is_empty() {
                break;
            }
            if let Some(first) = line.trim_start().split_whitespace().next() {
                names.push(first.to_string());
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------
// --version / --help (not-logged-in group: no connection is attempted)
// ---------------------------------------------------------------------------

#[test]
fn version_prints_exactly_muse_cli_0_3_2() {
    let home = fresh_home("version");
    let output = run(&["--version"], &home);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output).trim(), "muse-cli 0.3.2");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn short_version_flag_is_rejected_but_long_version_works() {
    // The original (argparse) has only `--version`; clap's auto
    // `-V` is disabled, so `-V` is an unknown flag (exit 2).
    let home = fresh_home("version-flag");
    assert_exit_2(&run(&["-V"], &home), "-V must be rejected");
    let output = run(&["--version"], &home);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output).trim(), "muse-cli 0.3.2");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn wait_checks_auto_approve_env_before_connecting() {
    // Bogus env + empty HOME: the env error must win over the auth
    // error, proving the check runs before any connection.
    let home = fresh_home("wait-env");
    let output = run_with_env(&["wait"], &home, &[("MUSE_AUTO_APPROVE", "bogus")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("MUSE_AUTO_APPROVE"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("auth error:"),
        "stderr: {}",
        stderr(&output)
    );
    // `send` keeps the original connect-first order: the same bogus
    // env still fails with the auth error, not the env error.
    let output = run_with_env(&["send", "hi"], &home, &[("MUSE_AUTO_APPROVE", "bogus")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("auth error:"),
        "stderr: {}",
        stderr(&output)
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn raw_empty_body_means_no_body() {
    // Python: `json.loads(body) if body else None` — `--body ""`
    // is no body, not invalid JSON. With an empty HOME the command
    // must therefore reach the auth error, not the body error.
    let home = fresh_home("raw-empty-body");
    let output = run(&["raw", "chat.history", "--body", ""], &home);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        !stderr(&output).contains("invalid --body JSON"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("auth error:"),
        "stderr: {}",
        stderr(&output)
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn feed_i64_min_limit_does_not_panic() {
    // A debug build used to panic (exit 101) on `(-limit)` for
    // `i64::MIN`. With an empty HOME the command must reach the
    // auth error instead. (`apply_feed_limit` itself only runs
    // after rows arrive, so the unit test covers `i64::MIN`
    // directly; this test proves the parse/limit path is panic-free.)
    let home = fresh_home("feed-min");
    let output = run(&["feed", "--limit=-9223372036854775808"], &home);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("auth error:"),
        "stderr: {}",
        stderr(&output)
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn help_lists_50_leaf_commands() {
    // Counting method (documented, reproducible by hand):
    // - top-level `--help` lists exactly the 39 real top-level
    //   commands (including the two parents `auth` and `fs`, which
    //   are not leaves themselves). Clap's auto-generated `help`
    //   subcommand is disabled at all three levels (top, `auth`,
    //   `fs`) to match the Python original (argparse), which has
    //   no `help` subcommand — fix round 1 removed the deviation
    //   this test used to document;
    // - `auth --help` lists 1 leaf (`export`);
    // - `fs --help` lists 12 leaves;
    // - leaves = (39 - 2 parents) + 1 + 12 = 50.
    // This must agree with `Command::leaf_names()` (library test).
    let home = fresh_home("help-count");
    let top = help_command_names(&["--help"], &home);
    assert_eq!(top.len(), 39, "top-level help lines: {top:?}");
    assert!(!top.contains(&"help".to_string()));
    assert!(top.contains(&"auth".to_string()));
    assert!(top.contains(&"fs".to_string()));
    let auth = help_command_names(&["auth", "--help"], &home);
    assert_eq!(auth, vec!["export".to_string()]);
    let fs = help_command_names(&["fs", "--help"], &home);
    assert_eq!(fs.len(), 12, "fs subcommands: {fs:?}");
    for expected in [
        "ls", "tree", "stat", "cat", "get", "find", "put", "write", "edit", "mkdir", "mv", "rm",
    ] {
        assert!(fs.contains(&expected.to_string()), "fs missing {expected}");
    }
    let leaves = (top.len() - 2) + auth.len() + fs.len();
    assert_eq!(leaves, 50);
    assert_eq!(Command::leaf_names().len(), 50);
    // `help` is not a command at any level (argparse parity).
    assert_exit_2(&run(&["help"], &home), "bare help subcommand");
    assert_exit_2(&run(&["auth", "help"], &home), "auth help subcommand");
    assert_exit_2(&run(&["fs", "help"], &home), "fs help subcommand");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn every_leaf_help_exits_zero() {
    let home = fresh_home("leaf-help");
    for leaf in Command::leaf_names() {
        let mut args: Vec<&str> = leaf.split(' ').collect();
        args.push("--help");
        let output = run(&args, &home);
        assert_eq!(
            output.status.code(),
            Some(0),
            "leaf `{leaf}` --help must exit 0\nstderr: {}",
            stderr(&output)
        );
    }
    std::fs::remove_dir_all(&home).unwrap();
}

// ---------------------------------------------------------------------------
// Parse errors: exit 2, decided before any connection (not-logged-in)
// ---------------------------------------------------------------------------

#[test]
fn parse_errors_exit_2() {
    let home = fresh_home("parse");
    // Unknown command.
    assert_exit_2(&run(&["not-a-command"], &home), "unknown command");
    // Missing required positional.
    assert_exit_2(&run(&["redeem"], &home), "redeem without code");
    assert_exit_2(&run(&["deny"], &home), "deny without id");
    assert_exit_2(&run(&["goal"], &home), "goal without id");
    assert_exit_2(&run(&["send"], &home), "send without text");
    assert_exit_2(
        &run(&["session-rename", "only-id"], &home),
        "session-rename without title",
    );
    // Conflicting flags (approve decision flags, send/wait auto-approve).
    assert_exit_2(
        &run(&["approve", "id-1", "--once", "--always"], &home),
        "approve --once --always",
    );
    assert_exit_2(
        &run(
            &["send", "hi", "--auto-approve", "--no-auto-approve"],
            &home,
        ),
        "send --auto-approve --no-auto-approve",
    );
    assert_exit_2(
        &run(&["wait", "--auto-approve", "--no-auto-approve"], &home),
        "wait --auto-approve --no-auto-approve",
    );
    // Bad integer value.
    let output = run(&["history", "--limit", "abc"], &home);
    assert_exit_2(&output, "history --limit abc");
    // Bad choice value.
    assert_exit_2(
        &run(&["auto-approve", "bogus"], &home),
        "auto-approve bogus",
    );
    // Unexpected extra argument to a no-argument command.
    assert_exit_2(&run(&["status", "extra"], &home), "status extra");
    assert_exit_2(&run(&["chat", "extra"], &home), "chat extra");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn bare_auth_and_bare_fs_exit_2() {
    // Not-logged-in group: argparse `required=True` in the original;
    // dispatch must reject these before connecting.
    let home = fresh_home("bare");
    assert_exit_2(&run(&["auth"], &home), "bare auth");
    assert_exit_2(&run(&["fs"], &home), "bare fs");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn approve_without_id_and_without_all_exit_2() {
    // Not-logged-in group: the id-or---all check runs before connecting.
    let home = fresh_home("approve");
    let output = run(&["approve"], &home);
    assert_exit_2(&output, "approve without id and without --all");
    assert!(
        stderr(&output).contains("give an approval ID or --all (not both)"),
        "stderr: {}",
        stderr(&output)
    );
    // Both id and --all is the same error.
    let output = run(&["approve", "id-1", "--all"], &home);
    assert_exit_2(&output, "approve id-1 --all");
    assert!(
        stderr(&output).contains("give an approval ID or --all (not both)"),
        "stderr: {}",
        stderr(&output)
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn fs_rm_without_yes_exit_2_before_connecting() {
    // Not-logged-in group: the --yes guard runs before connecting.
    let home = fresh_home("fs-rm");
    let output = run(&["fs", "rm", "some/path.txt"], &home);
    assert_exit_2(&output, "fs rm without --yes");
    assert!(
        stderr(&output).contains("refusing to delete without --yes"),
        "stderr: {}",
        stderr(&output)
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn raw_validation_errors_exit_2_before_connecting() {
    // Not-logged-in group: raw input validation runs before connecting.
    let home = fresh_home("raw");
    let output = run(&["raw", "method.khong-ton-tai"], &home);
    assert_exit_2(&output, "raw unknown method");
    assert!(
        stderr(&output).contains("unknown method 'method.khong-ton-tai'"),
        "stderr: {}",
        stderr(&output)
    );
    let output = run(&["raw", "chat.history", "--body", "{sai"], &home);
    assert_exit_2(&output, "raw invalid --body JSON");
    let output = run(&["raw", "chat.history", "--param", "sai"], &home);
    assert_exit_2(&output, "raw --param without =");
    std::fs::remove_dir_all(&home).unwrap();
}

// ---------------------------------------------------------------------------
// Not logged in: gateway commands fail with auth error, exit 2
// ---------------------------------------------------------------------------

#[test]
fn not_logged_in_status_and_other_gateway_commands_exit_2_with_auth_error() {
    let home = fresh_home("not-logged-in");
    // No cookies file exists under this HOME, so every gateway command
    // must fail before any network use.
    for args in [&["status"][..], &["threads"][..], &["wake"][..]] {
        let output = run(args, &home);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: expected exit 2\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("auth error:"),
            "{args:?}: stderr must start with an auth error, got: {}",
            stderr(&output)
        );
    }
    std::fs::remove_dir_all(&home).unwrap();
}
