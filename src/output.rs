//! JSON output helpers (writer A: output.rs).
//!
//! Mirrors Python `out(obj)`: `json.dumps(obj, indent=2,
//! ensure_ascii=False)` to stdout. `serde_json::to_string_pretty` uses
//! indent 2 and does not escape non-ASCII, so it matches.
//!
//! This module must not decide exit codes; commands print and then
//! return their own exit code (or error) to `main.rs`.

/// Print a JSON value, pretty-printed with indent 2, plus a newline.
///
/// A `serde_json::Value` is always serialisable, so the `Err` branch is
/// unreachable in practice; it is handled by printing nothing rather
/// than panicking.
pub fn print_json(value: &serde_json::Value) {
    if let Ok(text) = serde_json::to_string_pretty(value) {
        println!("{text}");
    }
}

/// Print a string that is already serialised JSON, verbatim, plus a
/// newline.
///
/// The caller owns the serialisation (indentation, key order). Callers
/// that hold a parsed value should use [`print_json`] instead, so the
/// output is always indent-2 pretty.
pub fn print_json_str(s: &str) {
    println!("{s}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pretty_print_uses_indent_two() {
        let value = json!({"b": 1, "a": [1, 2]});
        let text = serde_json::to_string_pretty(&value).unwrap();
        assert!(text.contains("\n  \"a\"") || text.contains("\n  \"b\""));
        assert!(!text.contains('\t'));
    }

    #[test]
    fn pretty_print_keeps_non_ascii_unescaped() {
        // Python ensure_ascii=False: non-ASCII is printed as-is.
        let value = json!({"label": "còn 12 tỷ token"});
        let text = serde_json::to_string_pretty(&value).unwrap();
        assert!(text.contains("còn 12 tỷ token"));
        assert!(!text.contains("\\u"));
    }

    #[test]
    fn print_json_and_print_json_str_do_not_panic() {
        // These write to stdout; the test only proves they do not panic
        // (output capture is the harness's job, not ours).
        print_json(&json!({"ok": true}));
        print_json_str("{\"already\": \"serialised\"}");
    }
}
