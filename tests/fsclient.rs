//! Pure `fsclient` integration tests (no network).
//!
//! Added after `fsclient.rs` landed mid-task (writer C). Covers the
//! pure API: `norm`, the 4 MiB `CHUNK` constant, `fmt_size`, base64
//! round-trip, and `apply_replace`. The chunk *call count* of
//! `FsClient::read_file` / `write_file` (e.g. a 10 MiB file -> 3 calls
//! with overwrite/append flags) is NOT covered here: those methods
//! need a `Gateway`, and serving them needs a body-aware mock
//! responder (offset/limit windows) that is follow-up work for the
//! coordinator — see the final report.
//!
//! NOTE on `..`: `norm` collapses `..` like `posixpath.normpath`
//! (`../secret` -> `secret`, `..` -> root), so the post-norm `..`
//! check in the code never fires for such input. That matches the
//! module's own documented quirk and its unit tests; it is NOT
//! treated here as a traversal rejection.

use muse_cli::errors::CliError;
use muse_cli::fsclient::{apply_replace, base64_decode, base64_encode, fmt_size, norm, CHUNK};
use serde_json::json;

#[test]
fn fs_norm_root_prefix_backslash_and_dot_segments() {
    assert_eq!(norm("").unwrap(), "");
    assert_eq!(norm("/").unwrap(), "");
    assert_eq!(norm("/config").unwrap(), "config");
    assert_eq!(
        norm("sandbox://workspace/a.txt").unwrap(),
        "workspace/a.txt"
    );
    assert_eq!(norm("a\\b\\c.txt").unwrap(), "a/b/c.txt");
    assert_eq!(norm("a/./b/../c").unwrap(), "a/c");
    assert_eq!(norm("  /x/y/ ").unwrap(), "x/y");
}

#[test]
fn fs_norm_dotdot_collapses_like_posixpath_not_an_error() {
    // Documented quirk (see the header note): traversal segments are
    // collapsed, not rejected, once normalised.
    assert_eq!(norm("../secret").unwrap(), "secret");
    assert_eq!(norm("..").unwrap(), "");
    assert_eq!(norm("a/../../b").unwrap(), "b");
}

#[test]
fn fs_chunk_constant_is_4_mib() {
    assert_eq!(CHUNK, 4 * 1024 * 1024);
}

#[test]
fn fs_fmt_size_thresholds() {
    assert_eq!(fmt_size(&json!(null)), "-");
    assert_eq!(fmt_size(&json!(0)), "0B");
    assert_eq!(fmt_size(&json!(1023)), "1023B");
    assert_eq!(fmt_size(&json!(1024)), "1.0K");
    assert_eq!(fmt_size(&json!(1536)), "1.5K");
    assert_eq!(fmt_size(&json!(1048576)), "1.0M");
    assert_eq!(fmt_size(&json!(1073741824)), "1.0G");
}

#[test]
fn fs_base64_known_vectors_and_roundtrip() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    for data in [
        b"".as_slice(),
        b"a",
        b"hello world",
        b"\x00\xff\xfe\x01\x02",
    ] {
        assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
    }
    assert!(base64_decode("Z").is_err());
}

#[test]
fn fs_apply_replace_replaces_all_and_errors_when_absent() {
    assert_eq!(apply_replace("aaa", "a", "b").unwrap(), "bbb");
    let err = apply_replace("hello", "z", "y").unwrap_err();
    match err {
        CliError::Value(msg) => {
            assert!(msg.contains("--replace: text not found:"));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
