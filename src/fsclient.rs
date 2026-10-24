//! System Files client (writer C: fsclient.rs).
//!
//! Mirrors Python `fs.py` over the gateway: path normalisation,
//! 4 MiB read/write chunking, folder get/put, and the editor helper.
//!
//! Two deliberate differences from the Python original, both settled
//! by the coordinator:
//! - `edit_in_editor` runs `$VISUAL`/`$EDITOR`/`vi` with a DIRECT
//!   `exec` (no shell). The editor string is split on ASCII
//!   whitespace: the first word is the program, the rest are its
//!   arguments, and the temporary file path is appended last. An
//!   editor value such as `code --wait` therefore still works, but
//!   shell quoting, pipes and redirects do not.
//! - There is no `base64` crate in `Cargo.toml` (and writer C may not
//!   edit it), so base64 encode/decode are implemented here as pure
//!   functions and unit-tested.
//!
//! Path-safety note: `norm` follows Python `fs.norm` exactly,
//! including its quirk — `posixpath.normpath("/" + p)` collapses
//! `..` at the root, so a path like `../secret` normalises to
//! `secret` and the post-normalisation `..` check never fires for
//! inputs that go through normpath. The check is kept verbatim so
//! the code matches the original line for line. Separately, `get`
//! refuses (with a `Value` error) when a server-supplied entry name
//! would make a local path escape the download root.

use crate::commands::python_repr;
use crate::errors::CliError;
use crate::gateway::Gateway;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `fs.read` / `fs.write` window, mirroring `fs.py:CHUNK` /
/// `WRITE_CHUNK` (4 MiB).
pub const CHUNK: usize = 4 * 1024 * 1024;

/// Normalise a System Files path, mirroring `fs.py:norm`.
///
/// Strip, `\` -> `/`, strip a `sandbox://` prefix, `normpath` onto
/// the root, `""` for the root. See the module doc for the `..`
/// quirk: the error below is unreachable through normpath alone
/// and is kept to match the original's code shape.
pub fn norm(path: &str) -> Result<String, CliError> {
    let mut p = path.trim().replace('\\', "/");
    if let Some(rest) = p.strip_prefix("sandbox://") {
        p = rest.to_string();
    }
    let mut stack: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            _ => stack.push(part),
        }
    }
    let mut out = stack.join("/");
    if out == "." {
        out.clear();
    }
    if out.split('/').any(|part| part == "..") {
        return Err(CliError::value(format!(
            "path traversal not allowed: {}",
            python_repr(path)
        )));
    }
    Ok(out)
}

/// Human size, mirroring `fs.py:fmt_size`: `None` -> `-`, under
/// 1024 -> `NB`, otherwise one decimal with `K`/`M`/`G` (stops at
/// `G`). The input is the JSON `size` value (`null`/missing -> `-`).
pub fn fmt_size(value: &Value) -> String {
    let mut n = match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => return "-".to_string(),
    };
    for unit in ["B", "K", "M", "G"] {
        if n < 1024.0 || unit == "G" {
            if unit == "B" {
                return format!("{n:.0}{unit}");
            }
            return format!("{n:.1}{unit}");
        }
        n /= 1024.0;
    }
    "-".to_string()
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode (with `=` padding), mirroring Python
/// `base64.b64encode(...).decode()`.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some((byte - b'A') as u32),
        b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
        b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 decode, mirroring Python `base64.b64decode` with
/// `validate=False` (characters outside the alphabet are ignored).
/// A dangling single trailing character is an error, like Python's
/// `binascii.Error` (a `ValueError` subclass -> exit 2).
pub fn base64_decode(text: &str) -> Result<Vec<u8>, CliError> {
    let mut cleaned: Vec<u8> = Vec::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if b64_value(byte).is_some() || byte == b'=' {
            cleaned.push(byte);
        }
    }
    if cleaned.len() % 4 == 1 {
        return Err(CliError::value("invalid base64 data".to_string()));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        if chunk.len() < 4 {
            // Final partial group (no padding): 2 chars -> 1 byte,
            // 3 chars -> 2 bytes.
            let mut vals = [0u32; 4];
            for (i, &byte) in chunk.iter().enumerate() {
                vals[i] = b64_value(byte).unwrap_or(0);
            }
            let triple = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
            out.push((triple >> 16) as u8);
            if chunk.len() > 2 {
                out.push((triple >> 8) as u8);
            }
            continue;
        }
        let mut vals = [0u32; 4];
        let mut pad = 0;
        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'=' {
                pad += 1;
            } else {
                vals[i] = b64_value(byte).unwrap_or(0);
            }
        }
        let triple = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((triple >> 16) as u8);
        if pad < 2 {
            out.push((triple >> 8) as u8);
        }
        if pad < 1 {
            out.push(triple as u8);
        }
    }
    Ok(out)
}

/// Lexically normalise a path (`.` removed, `..` resolved against
/// earlier components, never touching the filesystem). Used only
/// for the download-root containment check in `get`.
fn lexical_normal(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Refuse a local path whose fully-resolved form escapes the
/// canonicalized download `root` (fix round 2, SAFETY F2). The
/// lexical check in `get` cannot see symlinks: a server-supplied
/// name, or a planted link inside the download tree, could
/// otherwise redirect a write outside the root. This resolves the
/// longest existing prefix with `canonicalize` (which follows
/// every symlink component) and appends the not-yet-existing
/// suffix lexically, then requires the result to stay under `root`.
fn ensure_within_root(root_canonical: &Path, local: &Path) -> Result<(), CliError> {
    let mut current = local.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(resolved) => {
                let mut full = resolved;
                for part in suffix.iter().rev() {
                    full.push(part);
                }
                let full = lexical_normal(&full);
                if full.starts_with(root_canonical) {
                    return Ok(());
                }
                return Err(CliError::value(format!(
                    "unsafe local path (escapes download root): {}",
                    python_repr(&local.to_string_lossy())
                )));
            }
            Err(_) => match current.file_name().map(|n| n.to_os_string()) {
                Some(name) => {
                    suffix.push(name);
                    if !current.pop() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    // No existing prefix at all (unreachable when the root was just
    // created): fall back to the lexical containment check.
    if lexical_normal(local).starts_with(&lexical_normal(root_canonical)) {
        Ok(())
    } else {
        Err(CliError::value(format!(
            "unsafe local path (escapes download root): {}",
            python_repr(&local.to_string_lossy())
        )))
    }
}

/// Apply one `--replace OLD NEW` pair, mirroring `fs.py:apply_replace`
/// (replace ALL occurrences; `OLD` absent -> `Value` error, exit 2).
pub fn apply_replace(text: &str, old: &str, new: &str) -> Result<String, CliError> {
    if !text.contains(old) {
        return Err(CliError::value(format!(
            "--replace: text not found: {}",
            python_repr(old)
        )));
    }
    Ok(text.replace(old, new))
}

/// Open `data` in an editor and return the file's contents afterwards,
/// mirroring `fs.py:edit_in_editor` except for the direct exec (see
/// the module doc). The temporary file is created with mode 600
/// and always removed, including when the initial write fails
/// (fix round 2, SAFETY F3 — the old early `?` on the write
/// abandoned the temp file). The editor's exit status is ignored,
/// like Python's `subprocess.call` return value.
pub fn edit_in_editor(data: &[u8], suffix: &str) -> Result<Vec<u8>, CliError> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let mut words = editor.split_ascii_whitespace();
    let program = words.next().unwrap_or("vi");
    let args: Vec<&str> = words.collect();
    let tmp = unique_temp_path(suffix)?;
    if let Err(e) = std::fs::write(&tmp, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::from(e));
    }
    let result = (|| -> Result<Vec<u8>, CliError> {
        let _status = std::process::Command::new(program)
            .args(&args)
            .arg(&tmp)
            .status()?;
        let after = std::fs::read(&tmp)?;
        Ok(after)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn unique_temp_path(suffix: &str) -> Result<PathBuf, CliError> {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100u32 {
        let candidate = dir.join(format!("muse-cli-edit-{pid}-{nanos}-{attempt}{suffix}"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        // Mode 600 from creation (fix round 2, SAFETY F3); the
        // `create_new` refusal of a planted file/symlink is kept.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(file) => {
                drop(file);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CliError::from(e)),
        }
    }
    Err(CliError::io(
        "could not allocate a temporary file for fs edit".to_string(),
    ))
}

/// System Files over a gateway connection, mirroring `fs.py:FS`.
pub struct FsClient<'a> {
    gw: &'a mut Gateway,
}

impl<'a> FsClient<'a> {
    pub fn new(gw: &'a mut Gateway) -> Self {
        Self { gw }
    }

    fn call(&mut self, method: &str, body: Value, timeout_secs: u64) -> Result<Value, CliError> {
        self.gw.call_json(
            method,
            &[],
            Some(&body),
            None,
            Duration::from_secs(timeout_secs),
        )
    }

    /// `fs.list` entries (default `""` = root).
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<Value>, CliError> {
        let value = self.call("fs.list", serde_json::json!({"path": norm(path)?}), 60)?;
        match value {
            Value::Object(map) => match map.get("entries") {
                Some(Value::Array(entries)) => Ok(entries.clone()),
                _ => Ok(Vec::new()),
            },
            _ => Ok(Vec::new()),
        }
    }

    /// `fs.stat` (whole result object).
    pub fn stat(&mut self, path: &str) -> Result<Value, CliError> {
        self.call("fs.stat", serde_json::json!({"path": norm(path)?}), 60)
    }

    /// One `fs.read` window: `(bytes, eof)`.
    pub fn read_chunk(
        &mut self,
        path: &str,
        offset: i64,
        length: i64,
    ) -> Result<(Vec<u8>, bool), CliError> {
        let value = self.call(
            "fs.read",
            serde_json::json!({"path": norm(path)?, "offset": offset, "len": length}),
            120,
        )?;
        let encoded = value
            .get("data_base64")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let eof = value.get("eof").and_then(|v| v.as_bool()).unwrap_or(true);
        Ok((base64_decode(encoded)?, eof))
    }

    /// Whole file, in 4 MiB windows, honouring `max_bytes`.
    pub fn read_file(&mut self, path: &str, max_bytes: Option<i64>) -> Result<Vec<u8>, CliError> {
        let mut out: Vec<u8> = Vec::new();
        let mut off: i64 = 0;
        loop {
            let n = match max_bytes {
                None => CHUNK as i64,
                Some(max) => std::cmp::min(CHUNK as i64, max - off),
            };
            if n <= 0 {
                break;
            }
            let (data, eof) = self.read_chunk(path, off, n)?;
            let len = data.len() as i64;
            out.extend_from_slice(&data);
            off += len;
            if eof || data.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// `fs.search` entries (`limit` sent only when truthy).
    pub fn search(&mut self, contains: &str, limit: Option<i64>) -> Result<Vec<Value>, CliError> {
        let mut body = serde_json::json!({"name": {"contains": contains}});
        if let Some(limit) = limit {
            if limit != 0 {
                body.as_object_mut()
                    .map(|map| map.insert("limit".to_string(), Value::from(limit)));
            }
        }
        let value = self.call("fs.search", body, 60)?;
        match value {
            Value::Object(map) => match map.get("entries") {
                Some(Value::Array(entries)) => Ok(entries.clone()),
                _ => Ok(Vec::new()),
            },
            _ => Ok(Vec::new()),
        }
    }

    fn walk_into(
        &mut self,
        base: &str,
        depth: Option<i64>,
        level: i64,
        out: &mut Vec<(String, Value, i64)>,
    ) -> Result<(), CliError> {
        for entry in self.list_dir(base)? {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let rel = if base.is_empty() {
                name.clone()
            } else {
                format!("{base}/{name}")
            };
            let is_dir = entry.get("type").and_then(|v| v.as_str()) == Some("directory");
            out.push((rel.clone(), entry, level));
            if is_dir && depth.is_none_or(|d| level + 1 < d) {
                self.walk_into(&rel, depth, level + 1, out)?;
            }
        }
        Ok(())
    }

    /// Depth-first `(relpath, entry, level)` walk, mirroring
    /// `fs.py:walk` (root's children are level 0; recursion only
    /// while `level + 1 < depth`).
    pub fn walk(
        &mut self,
        path: &str,
        depth: Option<i64>,
    ) -> Result<Vec<(String, Value, i64)>, CliError> {
        let base = norm(path)?;
        let mut out = Vec::new();
        self.walk_into(&base, depth, 0, &mut out)?;
        Ok(out)
    }

    /// Write `data` in 4 MiB chunks: the first chunk uses `overwrite`
    /// as given, later chunks use `overwrite=true, append=true`.
    /// Returns `{"path": "/<normed>", "bytes_written": N}`.
    pub fn write_file(
        &mut self,
        path: &str,
        data: &[u8],
        overwrite: bool,
    ) -> Result<Value, CliError> {
        let p = norm(path)?;
        let mut off = 0usize;
        let mut first = true;
        while first || off < data.len() {
            let end = std::cmp::min(off + CHUNK, data.len());
            let part = &data[off..end];
            self.call(
                "fs.write",
                serde_json::json!({
                    "path": p,
                    "overwrite": if first { overwrite } else { true },
                    "append": !first,
                    "create_parent": true,
                    "data_base64": base64_encode(part),
                }),
                120,
            )?;
            off = end;
            first = false;
        }
        Ok(serde_json::json!({"path": format!("/{p}"), "bytes_written": data.len()}))
    }

    /// `fs.mkdir` (always recursive, like the CLI).
    pub fn mkdir(&mut self, path: &str) -> Result<Value, CliError> {
        self.call(
            "fs.mkdir",
            serde_json::json!({"path": norm(path)?, "recursive": true}),
            60,
        )
    }

    /// `fs.rename` (`create_parent` always true).
    pub fn rename(&mut self, src: &str, dst: &str, overwrite: bool) -> Result<Value, CliError> {
        self.call(
            "fs.rename",
            serde_json::json!({
                "source": norm(src)?,
                "destination": norm(dst)?,
                "overwrite": overwrite,
                "create_parent": true,
            }),
            60,
        )
    }

    /// `fs.delete`.
    pub fn delete(&mut self, path: &str, recursive: bool) -> Result<Value, CliError> {
        self.call(
            "fs.delete",
            serde_json::json!({"path": norm(path)?, "recursive": recursive}),
            60,
        )
    }

    /// Download a file (or a folder with `recursive`) into `dest`.
    /// Returns the local file paths (files only, not directories).
    /// A folder without `recursive` is a `Value` error, like Python.
    pub fn get(
        &mut self,
        path: &str,
        dest: &Path,
        recursive: bool,
    ) -> Result<Vec<String>, CliError> {
        let p = norm(path)?;
        let st = self.stat(&p)?;
        let name = match Path::new(&p).file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => "system-files".to_string(),
        };
        let kind = st.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "dir" || kind == "directory" {
            if !recursive {
                return Err(CliError::value(format!("{path} is a folder; use -r")));
            }
            let root = dest.join(&name);
            std::fs::create_dir_all(&root)?;
            let root_normal = lexical_normal(&root);
            let root_canonical = std::fs::canonicalize(&root)?;
            let mut got = Vec::new();
            for (rel, entry, _) in self.walk(&p, None)? {
                let suffix = rel.strip_prefix(&p).unwrap_or(&rel).trim_start_matches('/');
                let local = suffix
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .fold(root.clone(), |acc, part| acc.join(part));
                // Server-supplied entry names must not escape the root.
                if !lexical_normal(&local).starts_with(&root_normal) {
                    return Err(CliError::value(format!(
                        "unsafe path in server listing: {}",
                        python_repr(&rel)
                    )));
                }
                if entry.get("type").and_then(|v| v.as_str()) == Some("directory") {
                    ensure_within_root(&root_canonical, &local)?;
                    std::fs::create_dir_all(&local)?;
                } else {
                    if let Some(parent) = local.parent() {
                        ensure_within_root(&root_canonical, parent)?;
                        std::fs::create_dir_all(parent)?;
                    }
                    ensure_within_root(&root_canonical, &local)?;
                    let data = self.read_file(&rel, None)?;
                    std::fs::write(&local, &data)?;
                    got.push(local.to_string_lossy().into_owned());
                }
            }
            return Ok(got);
        }
        let local = if dest.is_dir() {
            dest.join(&name)
        } else {
            dest.to_path_buf()
        };
        if dest.is_dir() {
            // Server-supplied basename joined onto a caller-chosen
            // directory: the resolved target must stay inside it.
            // (An explicitly named output file is the caller's own
            // path and is written as given.)
            let root_canonical = std::fs::canonicalize(dest)?;
            ensure_within_root(&root_canonical, &local)?;
        }
        let data = self.read_file(&p, None)?;
        std::fs::write(&local, &data)?;
        Ok(vec![local.to_string_lossy().into_owned()])
    }

    /// Upload a local file (or a folder with `recursive`) to
    /// `remote`. Returns one `write_file` result per file.
    pub fn put(
        &mut self,
        local: &Path,
        remote: &str,
        recursive: bool,
        overwrite: bool,
    ) -> Result<Vec<Value>, CliError> {
        let r = norm(remote)?;
        if local.is_dir() {
            if !recursive {
                return Err(CliError::value(format!(
                    "{} is a folder; use -r",
                    local.to_string_lossy()
                )));
            }
            let mut done = Vec::new();
            self.put_dir(local, &r, overwrite, &mut done)?;
            return Ok(done);
        }
        let data = std::fs::read(local)?;
        Ok(vec![self.write_file(&r, &data, overwrite)?])
    }

    fn put_dir(
        &mut self,
        local_dir: &Path,
        remote_dir: &str,
        overwrite: bool,
        done: &mut Vec<Value>,
    ) -> Result<(), CliError> {
        self.mkdir(remote_dir)?;
        let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
        let entries = std::fs::read_dir(local_dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let remote_child = if remote_dir.is_empty() {
                file_name.clone()
            } else {
                format!("{remote_dir}/{file_name}")
            };
            if file_type.is_dir() {
                subdirs.push((path, remote_child));
            } else if file_type.is_symlink() {
                // Like Python's os.walk (which yields a symlinked dir
                // in neither its descend list nor its file list): a
                // symlinked dir is skipped entirely — no remote
                // mkdir, no descent (fix round 2, TECHNICAL F11).
                // A symlinked file IS in os.walk's file list, so it
                // is uploaded by content (the read follows the link).
                if !path.is_dir() {
                    let data = std::fs::read(&path)?;
                    done.push(self.write_file(&remote_child, &data, overwrite)?);
                }
            } else {
                let data = std::fs::read(&path)?;
                done.push(self.write_file(&remote_child, &data, overwrite)?);
            }
        }
        for (subdir, remote_child) in subdirs {
            self.put_dir(&subdir, &remote_child, overwrite, done)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_collapses_backslash_prefix_and_dot_segments() {
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
    fn norm_root_dotdot_collapses_like_posixpath() {
        // Python posixpath.normpath("/../secret") == "/secret": the
        // post-norm ".." check in fs.py never fires for such input.
        assert_eq!(norm("../secret").unwrap(), "secret");
        assert_eq!(norm("..").unwrap(), "");
    }

    #[test]
    fn fmt_size_thresholds_match_python() {
        assert_eq!(fmt_size(&Value::Null), "-");
        assert_eq!(fmt_size(&serde_json::json!(0)), "0B");
        assert_eq!(fmt_size(&serde_json::json!(1023)), "1023B");
        assert_eq!(fmt_size(&serde_json::json!(1024)), "1.0K");
        assert_eq!(fmt_size(&serde_json::json!(1536)), "1.5K");
        assert_eq!(fmt_size(&serde_json::json!(1048576)), "1.0M");
        assert_eq!(fmt_size(&serde_json::json!(1073741824)), "1.0G");
        assert_eq!(fmt_size(&serde_json::json!(1099511627776i64)), "1024.0G");
    }

    #[test]
    fn base64_roundtrip_and_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
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
        // Python validate=False ignores stray characters.
        assert_eq!(base64_decode("Zm9v\n").unwrap(), b"foo");
        assert!(base64_decode("Z").is_err());
    }

    #[test]
    fn apply_replace_replaces_all_and_errors_when_absent() {
        assert_eq!(apply_replace("aaa", "a", "b").unwrap(), "bbb".to_string());
        let err = apply_replace("hello", "z", "y").unwrap_err();
        match err {
            CliError::Value(msg) => assert!(msg.contains("--replace: text not found: 'z'")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn lexical_normal_resolves_dotdot() {
        assert_eq!(
            lexical_normal(Path::new("root/a/../b")),
            PathBuf::from("root/b")
        );
        assert_eq!(
            lexical_normal(Path::new("root/../../evil")),
            PathBuf::from("evil")
        );
        assert!(!lexical_normal(Path::new("root/../../evil"))
            .starts_with(&lexical_normal(Path::new("root"))));
    }

    fn containment_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "muse-cli-rust-containment-{tag}-{pid}-{n}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(unix)]
    fn ensure_within_root_refuses_symlink_escape_and_allows_inside_link() {
        use std::os::unix::fs::symlink;
        let base = containment_dir("root");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let root_canonical = std::fs::canonicalize(&root).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // Plain nested path: allowed.
        assert!(ensure_within_root(&root_canonical, &root.join("a/b.txt")).is_ok());
        // Link inside the root pointing outside: refused, both as a
        // directory prefix and as the final file itself.
        symlink(&outside, &root.join("escape")).unwrap();
        assert!(ensure_within_root(&root_canonical, &root.join("escape/evil.txt")).is_err());
        std::fs::write(&outside.join("planted.txt"), "x").unwrap();
        symlink(&outside.join("planted.txt"), &root.join("file-link")).unwrap();
        assert!(ensure_within_root(&root_canonical, &root.join("file-link")).is_err());
        // Link inside the root pointing back inside the root (via
        // an absolute path under the canonical root): allowed.
        symlink(&root_canonical, &root.join("back")).unwrap();
        assert!(ensure_within_root(&root_canonical, &root.join("back/a.txt")).is_ok());
        // Lexical escape: refused.
        assert!(ensure_within_root(&root_canonical, &root.join("../outside/x")).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    // Both edit tests touch shared process-global state: the /tmp
    // `muse-cli-edit-*` namespace (scanned by the failure test) and
    // the VISUAL env var. Rust runs tests in one process in
    // parallel, so without this lock the failure test can snapshot
    // /tmp while the 600 test's temp file still exists and flake
    // (observed: before=0, after=1, ~1 in 10 lib runs). Production
    // code is unchanged; this only serialises the two tests.
    static EDIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[cfg(unix)]
    fn edit_temp_file_is_created_at_600() {
        let _guard = EDIT_TEST_LOCK.lock().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_temp_path(".txt").unwrap();
        assert_eq!(
            std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn edit_in_editor_failure_leaves_no_temp_file() {
        let _guard = EDIT_TEST_LOCK.lock().unwrap();
        // A nonexistent editor program fails the spawn; the temp
        // file must still be removed (no muse-cli-edit-* leftovers).
        fn leftovers() -> Vec<PathBuf> {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("muse-cli-edit-"))
                })
                .collect()
        }
        let before = leftovers();
        let old_visual = std::env::var("VISUAL").ok();
        std::env::set_var("VISUAL", "/nonexistent-editor-muse-cli-test");
        let result = edit_in_editor(b"hello", ".txt");
        match old_visual {
            Some(value) => std::env::set_var("VISUAL", value),
            None => std::env::remove_var("VISUAL"),
        }
        assert!(result.is_err());
        let after = leftovers();
        assert_eq!(before.len(), after.len());
        assert!(after.iter().all(|p| before.contains(p)));
    }
}
