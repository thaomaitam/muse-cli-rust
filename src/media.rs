//! Pure media helpers (writer A: media.rs).
//!
//! Only the pure parts live here: [`media_path`] and
//! [`event_resources`] (plus [`local_name`], a pure filename helper
//! writer C's download code needs). Download orchestration — HTTP
//! calls, `.part` files, retries — belongs to writer C in
//! `commands.rs` and is deliberately NOT in this file.
//!
//! Both functions mirror `gateway.py:media_path` and
//! `cli.py:event_resources` exactly, including their quirks:
//! - dict priority: `variants.original` > `path` > `url` (Python `or`
//!   chain: empty strings and other falsy values fall through);
//! - `sandbox://` prefix stripped, `http(s)://` URLs reduced to their
//!   path, a leading `media/raw/` stripped once, percent-decoding
//!   then percent-encoding (safe `/`) on the way out;
//! - empty result or any `..` path segment -> `Value` error;
//! - `event_resources` merges by path (first `kind` wins, later
//!   non-null `mime`/`byte_len`/`label`/`variants` overwrite), skips
//!   `missing` items and items with neither a `sandbox://` path nor a
//!   `/media/raw/` path, and derives `kind` from a `data.*` key with
//!   trailing `s` characters stripped (`videos` -> `video`).
//!
//! Known serde difference (inherent to the CONTRACT's
//! `serde_json::Value` interface, not a logic choice): JSON object key
//! order is sorted in Rust, while Python preserves server insertion
//! order. Within one list, order is preserved. So `event_resources`
//! iterates `payload.data` keys in sorted order, and output objects
//! serialise with sorted keys. Semantically identical JSON.

use crate::errors::CliError;
use std::path::{Path, PathBuf};

/// Quote a string the way Python's `repr()` does for a plain string
/// (single quotes, backslash/quote/control escapes). Used in the
/// `bad sandbox path` error message.
fn python_repr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// Percent-decode like Python `urllib.parse.unquote`: `%XX` hex bytes
/// become bytes (invalid `%` sequences stay literal, invalid UTF-8
/// becomes U+FFFD), `+` is NOT converted to a space.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = &input[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode like Python `urllib.parse.quote(rel, safe="/")`:
/// ASCII letters, digits, `_.-~` and `/` pass through; every other
/// byte becomes `%XX` (uppercase hex).
fn percent_encode(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len());
    for &b in rel.as_bytes() {
        if b.is_ascii_alphanumeric()
            || b == b'_'
            || b == b'.'
            || b == b'-'
            || b == b'~'
            || b == b'/'
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// First truthy candidate for a resource dict, Python `or`-chain style:
/// non-empty strings, non-zero numbers, and `true` count; empty
/// strings, zero, `false`, and null fall through. Rendered
/// Python-style (`True`/`False` for booleans).
fn truthy_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => {
            if n.as_f64().unwrap_or(0.0) != 0.0 {
                Some(n.to_string())
            } else {
                None
            }
        }
        serde_json::Value::Bool(true) => Some("True".to_string()),
        _ => None,
    }
}

/// Map a sandbox path / media URL / resource dict to a
/// `/media/raw/...` path (CONTRACT; gateway.rs re-exports this).
///
/// Input is a JSON value: an object uses `variants.original` >
/// `path` > `url`; a string is used as-is; other scalars are
/// stringified Python-style. Empty result or a `..` segment ->
/// `CliError::Value("bad sandbox path: …")`.
pub fn media_path(ref_: &serde_json::Value) -> Result<String, CliError> {
    let raw: String = match ref_ {
        serde_json::Value::Object(map) => {
            let variants_original = match map.get("variants") {
                Some(serde_json::Value::Object(variants)) => {
                    variants.get("original").and_then(truthy_scalar)
                }
                _ => None,
            };
            let found = variants_original
                .or_else(|| map.get("path").and_then(truthy_scalar))
                .or_else(|| map.get("url").and_then(truthy_scalar));
            match found {
                Some(s) => s,
                None => {
                    return Err(CliError::value("resource has no path or variants.original"));
                }
            }
        }
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(true) => "True".to_string(),
        serde_json::Value::Bool(false) => "False".to_string(),
        serde_json::Value::Null => "None".to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(_) => {
            // An array stringifies only loosely (like Python's str()
            // of a list); it will fail the path check below unless it
            // happens to be a valid path.
            serde_json::to_string(ref_).unwrap_or_default()
        }
    };
    let rel = if let Some(rest) = raw.strip_prefix("sandbox://") {
        rest.to_string()
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        // urllib.parse.urlsplit(...).path: after "://host", up to the
        // first '?' or '#'; no '/' at all means an empty path.
        let after_scheme = match raw.find("://") {
            Some(idx) => &raw[idx + 3..],
            None => raw.as_str(),
        };
        match after_scheme.find('/') {
            Some(idx) => {
                let path = &after_scheme[idx..];
                let end = path.find(['?', '#']).unwrap_or(path.len());
                path[..end].to_string()
            }
            None => String::new(),
        }
    } else {
        raw.clone()
    };
    let mut rel = rel.trim_start_matches('/').to_string();
    if let Some(rest) = rel.strip_prefix("media/raw/") {
        rel = rest.to_string();
    }
    let rel = percent_decode(&rel);
    if rel.is_empty() || rel.split('/').any(|part| part == "..") {
        return Err(CliError::value(format!(
            "bad sandbox path: {}",
            python_repr(&raw)
        )));
    }
    Ok(format!("/media/raw/{}", percent_encode(&rel)))
}

/// Local filename for a downloaded resource, mirroring
/// `cli.py:_local_name`: basename of the path (query stripped,
/// trailing slashes stripped), or `"download"` when that is empty,
/// joined onto `directory`.
pub fn local_name(path: &str, directory: &Path) -> PathBuf {
    let before_query = path.split('?').next().unwrap_or(path);
    let trimmed = before_query.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or("");
    let name = if name.is_empty() { "download" } else { name };
    directory.join(name)
}

/// Extract the downloadable files referenced by one history event
/// (mirrors `cli.py:event_resources`).
///
/// Sources, in order: `payload.data.<list>[]`, `event.resources`,
/// `payload.resources`, then `sandbox://…` links found in
/// `payload.fallback_text`, `payload.data.fallback_text`,
/// `payload.display_text`, and `payload.content` (regex
/// `sandbox://[^\s)\]"'>]+`). Results are deduplicated by path.
pub fn event_resources(ev: &serde_json::Value) -> Vec<serde_json::Value> {
    let payload = match ev.get("payload") {
        Some(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    };
    // Ordered by first-seen path (Vec + linear scan keeps insertion
    // order without an extra dependency).
    let mut found: Vec<serde_json::Value> = Vec::new();

    let mut add = |item: &serde_json::Value, kind: &str| {
        let item_obj: Option<serde_json::Map<String, serde_json::Value>> = match item {
            serde_json::Value::String(s) => {
                let mut map = serde_json::Map::new();
                map.insert("path".to_string(), serde_json::Value::String(s.clone()));
                Some(map)
            }
            serde_json::Value::Object(map) => Some(map.clone()),
            _ => None,
        };
        let item_obj = match item_obj {
            Some(map) => map,
            None => return,
        };
        let path = item_obj.get("path").and_then(truthy_scalar).or_else(|| {
            match item_obj.get("variants") {
                Some(serde_json::Value::Object(variants)) => {
                    variants.get("original").and_then(truthy_scalar)
                }
                _ => None,
            }
        });
        let path = match path {
            Some(p) => p,
            None => return,
        };
        if !(path.starts_with("sandbox://") || path.contains("/media/raw/")) {
            return;
        }
        if item_obj.get("missing").is_some_and(|v| match v {
            serde_json::Value::Null => false,
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            serde_json::Value::Object(o) => !o.is_empty(),
        }) {
            return;
        }
        // Merge into the existing row for this path, or create it.
        let pos = found
            .iter()
            .position(|r| r.get("path").and_then(|p| p.as_str()) == Some(path.as_str()));
        let pos = match pos {
            Some(pos) => pos,
            None => {
                let mut map = serde_json::Map::new();
                map.insert("path".to_string(), serde_json::Value::String(path.clone()));
                map.insert(
                    "kind".to_string(),
                    serde_json::Value::String(kind.to_string()),
                );
                found.push(serde_json::Value::Object(map));
                found.len() - 1
            }
        };
        let row = match found.get_mut(pos) {
            Some(row) => row,
            None => return,
        };
        if let Some(obj) = row.as_object_mut() {
            for key in ["mime", "byte_len", "label", "variants"] {
                if let Some(value) = item_obj.get(key) {
                    if !matches!(value, serde_json::Value::Null) {
                        obj.insert(key.to_string(), value.clone());
                    }
                }
            }
        }
    };

    let data = match payload.and_then(|p| p.get("data")) {
        Some(serde_json::Value::Object(map)) => Some(map.clone()),
        _ => None,
    };
    if let Some(data) = &data {
        for (key, value) in data {
            if let serde_json::Value::Array(items) = value {
                // Python key.rstrip("s"): strips ALL trailing 's's.
                let kind = key.trim_end_matches('s');
                for item in items {
                    add(item, kind);
                }
            }
        }
    }
    for src in [
        ev.get("resources"),
        payload.and_then(|p| p.get("resources")),
    ] {
        if let Some(serde_json::Value::Array(items)) = src {
            for item in items {
                add(item, "resource");
            }
        }
    }
    let texts = [
        payload.and_then(|p| p.get("fallback_text")),
        data.as_ref().and_then(|d| d.get("fallback_text")),
        payload.and_then(|p| p.get("display_text")),
        payload.and_then(|p| p.get("content")),
    ];
    for text in texts.into_iter().flatten() {
        if let serde_json::Value::String(text) = text {
            for link in sandbox_links(text) {
                let item = serde_json::Value::String(link);
                add(&item, "link");
            }
        }
    }
    found
}

/// All `sandbox://…` links in a text, Python regex
/// `sandbox://[^\s)\]"'>]+` (scan, no regex crate needed).
fn sandbox_links(text: &str) -> Vec<String> {
    const STOP: &[char] = &[' ', '\t', '\n', '\r', ')', ']', '"', '\'', '>'];
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("sandbox://") {
        let after = &rest[idx..];
        let end = after
            .find(|c: char| STOP.contains(&c))
            .unwrap_or(after.len());
        links.push(after[..end].to_string());
        rest = &after[end..];
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn media_path_sandbox_prefix_is_stripped() {
        assert_eq!(
            media_path(&json!("sandbox://workspace/photo.png")).unwrap(),
            "/media/raw/workspace/photo.png"
        );
    }

    #[test]
    fn media_path_dict_prefers_variants_original_over_path_over_url() {
        let with_all = json!({
            "variants": {"original": "https://vm.example.com/media/raw/orig.png"},
            "path": "sandbox://workspace/path.png",
            "url": "https://vm.example.com/media/raw/url.png",
        });
        assert_eq!(media_path(&with_all).unwrap(), "/media/raw/orig.png");
        let with_path = json!({
            "path": "sandbox://workspace/path.png",
            "url": "https://vm.example.com/media/raw/url.png",
        });
        assert_eq!(
            media_path(&with_path).unwrap(),
            "/media/raw/workspace/path.png"
        );
        let with_url = json!({"url": "https://vm.example.com/media/raw/url.png"});
        assert_eq!(media_path(&with_url).unwrap(), "/media/raw/url.png");
    }

    #[test]
    fn media_path_url_uses_path_only_and_strips_media_raw_prefix() {
        assert_eq!(
            media_path(&json!("https://vm.example.com/media/raw/a/b.png?x=1#frag")).unwrap(),
            "/media/raw/a/b.png"
        );
    }

    #[test]
    fn media_path_bare_path_and_media_raw_prefix() {
        assert_eq!(
            media_path(&json!("workspace/a.png")).unwrap(),
            "/media/raw/workspace/a.png"
        );
        assert_eq!(
            media_path(&json!("media/raw/a.png")).unwrap(),
            "/media/raw/a.png"
        );
        assert_eq!(
            media_path(&json!("/media/raw/a.png")).unwrap(),
            "/media/raw/a.png"
        );
    }

    #[test]
    fn media_path_unquotes_then_requotes() {
        assert_eq!(
            media_path(&json!("sandbox://workspace/my%20photo.png")).unwrap(),
            "/media/raw/workspace/my%20photo.png"
        );
        assert_eq!(
            media_path(&json!("sandbox://workspace/a+b.png")).unwrap(),
            "/media/raw/workspace/a%2Bb.png"
        );
    }

    #[test]
    fn media_path_dotdot_is_a_value_error() {
        let err = media_path(&json!("sandbox://workspace/../secret.txt")).unwrap_err();
        match err {
            CliError::Value(msg) => {
                assert!(msg.starts_with("bad sandbox path: "));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn media_path_empty_is_a_value_error() {
        assert!(matches!(
            media_path(&json!("")).unwrap_err(),
            CliError::Value(_)
        ));
        assert!(matches!(
            media_path(&json!("sandbox://")).unwrap_err(),
            CliError::Value(_)
        ));
        assert!(matches!(
            media_path(&json!({})).unwrap_err(),
            CliError::Value(_)
        ));
    }

    #[test]
    fn media_path_dict_without_path_is_the_documented_error() {
        let err = media_path(&json!({"mime": "image/png"})).unwrap_err();
        match err {
            CliError::Value(msg) => {
                assert_eq!(msg, "resource has no path or variants.original");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn local_name_strips_query_and_falls_back_to_download() {
        assert_eq!(
            local_name("sandbox://workspace/photo.png?x=1", Path::new("/tmp/out")),
            Path::new("/tmp/out/photo.png")
        );
        // Empty basename -> "download" (Python: `or "download"`).
        assert_eq!(local_name("", Path::new("out")), Path::new("out/download"));
        // Faithful to Python's basename: "sandbox://" alone trims to
        // "sandbox:", which has no '/' and is kept as the name.
        assert_eq!(
            local_name("sandbox://", Path::new("out")),
            Path::new("out/sandbox:")
        );
        assert_eq!(
            local_name("https://h/media/raw/a/b.png", Path::new(".")),
            Path::new("./b.png")
        );
    }

    #[test]
    fn event_resources_from_data_lists_with_kind_derivation() {
        let ev = json!({
            "payload": {
                "data": {
                    "videos": [{"path": "sandbox://workspace/v.mp4", "mime": "video/mp4", "byte_len": 42}],
                    "images": [{"variants": {"original": "https://vm/media/raw/i.png"}}],
                }
            }
        });
        let rows = event_resources(&ev);
        assert_eq!(rows.len(), 2);
        let video = rows.iter().find(|r| r["kind"] == "video").unwrap();
        assert_eq!(video["path"], "sandbox://workspace/v.mp4");
        assert_eq!(video["mime"], "video/mp4");
        assert_eq!(video["byte_len"], 42);
        let image = rows.iter().find(|r| r["kind"] == "image").unwrap();
        assert_eq!(image["path"], "https://vm/media/raw/i.png");
    }

    #[test]
    fn event_resources_skips_missing_and_non_sandbox_paths() {
        let ev = json!({
            "payload": {
                "data": {
                    "files": [
                        {"path": "sandbox://workspace/gone.png", "missing": true},
                        {"path": "/tmp/plain.png"},
                        {"path": "sandbox://workspace/kept.png"},
                    ]
                }
            }
        });
        let rows = event_resources(&ev);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"], "sandbox://workspace/kept.png");
    }

    #[test]
    fn event_resources_finds_sandbox_links_in_text() {
        let ev = json!({
            "payload": {
                "display_text": "see sandbox://workspace/a.png and (sandbox://workspace/b.png) done"
            }
        });
        let rows = event_resources(&ev);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["kind"] == "link"));
    }

    #[test]
    fn event_resources_merges_same_path_first_kind_wins() {
        let ev = json!({
            "payload": {
                "data": {"images": [{"path": "sandbox://workspace/a.png", "mime": "image/png"}]},
                "resources": [{"path": "sandbox://workspace/a.png", "label": "A"}],
            }
        });
        let rows = event_resources(&ev);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "image");
        assert_eq!(rows[0]["mime"], "image/png");
        assert_eq!(rows[0]["label"], "A");
    }
}
