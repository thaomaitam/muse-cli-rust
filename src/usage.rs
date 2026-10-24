//! Pure usage / invite / subscription helpers (writer A: usage.rs).
//!
//! Mirrors `cli.py:parse_tokens_left`, `summarize_usage`,
//! `_human_tokens`, `summarize_invite`, and
//! (RSC parsing and the `find_action_id` pattern live in gateway.rs.)
//! No network calls live here.
//!
//! NOTE FOR WRITER C — the network part that is NOT here:
//! `fetch_subscription(cookies, action_id)` must, in order,
//!   1. POST `https://muse.ai/settings` with header
//!      `Next-Action: <action_id>` and JSON body
//!      `[{"includeAgreement": false}]`, `Accept: text/x-component`,
//!      trying first the action id saved in config
//!      (`subscription_action_id`), then the default
//!      `SUBSCRIPTION_ACTION_ID` below;
//!   2. if both return a non-RSC (or non-200, non-`text/x-component`)
//!      response, scan the web app's JS bundles for the action id
//!      (see the pattern in gateway.rs) and retry;
//!   3. require the parsed result to be an object with
//!      `success == true` and a `subscription` object, else raise
//!      `Gateway(-1, "<action> failed: …")`;
//!   4. return `(subscription_object, action_id_used)`; the caller
//!      persists `action_id_used` into config when it changed.
//! Inputs writer C needs: the cookie header string from
//! `config::load_cookies`, and `config.subscription_action_id`.
//! Auth failures (401/403) map to `CliError::Auth`; anything else
//! non-200/non-RSC is a stale-id signal, not an error, until the
//! bundle scan also fails.
//!
//! Local-time note: Python formats `resets_at` with the system local
//! timezone. Rust std has no local-offset API and this crate has no
//! chrono dependency (writer A may not change `Cargo.toml`), so
//! [`format_local_timestamp`] computes the offset from TZif data
//! (`/etc/localtime`, or `/usr/share/zoneinfo/<TZ>` when `TZ` names a
//! zone) with a POSIX-style `TZ` fallback, defaulting to UTC when
//! nothing parses. The offset lookup is best-effort; the pure
//! formatting with an explicit offset is fully tested.

/// Default RSC server-action id for `fetchSubscriptionAction`
/// (gateway.py). It changes when the web app is redeployed, which is
/// why the bundle scan exists.
pub const SUBSCRIPTION_ACTION_ID: &str = "4029ac5c4bd03136487d91ebde5de39c64b35229b4";

/// RSC server-action name for the subscription fetch.
pub const SUBSCRIPTION_ACTION_NAME: &str = "fetchSubscriptionAction";

const SCALE_K: f64 = 1e3;
const SCALE_M: f64 = 1e6;
const SCALE_B: f64 = 1e9;
const SCALE_T: f64 = 1e12;

/// Round half to even (Python `round()` for a float that lands exactly
/// on `.5`), returning an integer. Rust's `f64::round` rounds half
/// away from zero, which differs on exact halves.
fn round_half_even(x: f64) -> i64 {
    let floor = x.floor();
    let frac = x - floor;
    if frac < 0.5 {
        floor as i64
    } else if frac > 0.5 {
        (floor + 1.0) as i64
    } else if (floor as i64) % 2 == 0 {
        floor as i64
    } else {
        (floor + 1.0) as i64
    }
}

/// Parse a "tokens left" label.
///
/// `'0% used (3B tokens left)'` -> `Some(3_000_000_000)`;
/// `'Đã dùng 4% (còn 12 tỷ token)'` -> `Some(12_000_000_000)`.
/// Units: `K/M/B/T` (case-insensitive) and `nghìn/triệu/tỷ`.
/// Comma rule, verbatim from the original: if the number contains a
/// `.` or its comma count is not exactly 1, commas are dropped;
/// otherwise the single comma is a decimal point. An unparseable
/// number after a `token(s)` match returns `None` (Python's
/// `ValueError` path), it does not keep searching.
pub fn parse_tokens_left(label: &str) -> Option<i64> {
    if label.is_empty() {
        return None;
    }
    let chars: Vec<char> = label.to_lowercase().chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let num_start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',') {
            i += 1;
        }
        let num_str: String = chars[num_start..i].iter().collect();
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        // Unit candidates, most specific first. Like the original
        // regex, a unit that is really the start of "tokens" must not
        // be consumed: for "42 tokens", the 't' is the token word,
        // not a Tera unit. So try with-unit first, then without.
        let mut attempts: Vec<(f64, usize)> = Vec::new();
        let mut unit_end = j;
        let mut unit_scale = 1.0f64;
        let mut found_unit = false;
        for (unit, scale) in [("nghìn", SCALE_K), ("triệu", SCALE_M), ("tỷ", SCALE_B)] {
            let unit_chars: Vec<char> = unit.chars().collect();
            if j + unit_chars.len() <= chars.len()
                && chars[j..j + unit_chars.len()] == unit_chars[..]
            {
                unit_scale = scale;
                unit_end = j + unit_chars.len();
                found_unit = true;
                break;
            }
        }
        if !found_unit && j < chars.len() {
            match chars[j] {
                'k' => {
                    unit_scale = SCALE_K;
                    unit_end = j + 1;
                    found_unit = true;
                }
                'm' => {
                    unit_scale = SCALE_M;
                    unit_end = j + 1;
                    found_unit = true;
                }
                'b' => {
                    unit_scale = SCALE_B;
                    unit_end = j + 1;
                    found_unit = true;
                }
                't' => {
                    unit_scale = SCALE_T;
                    unit_end = j + 1;
                    found_unit = true;
                }
                _ => {}
            }
        }
        if found_unit {
            attempts.push((unit_scale, unit_end));
        }
        attempts.push((1.0, j));
        let token: Vec<char> = "token".chars().collect();
        let mut matched: Option<f64> = None;
        for (scale, start) in attempts {
            let mut k = start;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            if k + token.len() <= chars.len() && chars[k..k + token.len()] == token[..] {
                matched = Some(scale);
                break;
            }
        }
        let scale = match matched {
            Some(scale) => scale,
            None => {
                // Not a tokens-left match at this number; try the next digit.
                i = num_start + 1;
                continue;
            }
        };
        let comma_count = num_str.matches(',').count();
        let normalised = if num_str.contains('.') || comma_count != 1 {
            num_str.replace(',', "")
        } else {
            num_str.replace(',', ".")
        };
        let number: f64 = match normalised.parse() {
            Ok(n) => n,
            Err(_) => return None,
        };
        return Some(round_half_even(number * scale));
    }
    None
}

/// Human token count, mirroring `cli.py:_human_tokens`:
/// `1500 -> "1.5K"`, `1000000 -> "1M"`, `999 -> "999"`.
pub fn human_tokens(n: i64) -> String {
    let magnitude = (n as i128).abs();
    for (div, unit) in [
        (1_000_000_000i128, "B"),
        (1_000_000i128, "M"),
        (1_000i128, "K"),
    ] {
        if magnitude >= div {
            let formatted = format!("{:.1}", n as f64 / div as f64);
            let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
            return format!("{trimmed}{unit}");
        }
    }
    n.to_string()
}

/// JSON number as i64 when it is an integer, else as f64 truncated
/// (Python `time.localtime` accepts floats; truncation matches the
/// usual integer-second timestamps).
fn json_to_seconds(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                n.as_f64().map(|f| f as i64)
            }
        }
        _ => None,
    }
}

/// JSON number as f64 (for topup arithmetic); non-numbers count as 0.
fn json_to_f64(value: &serde_json::Value) -> f64 {
    match value {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Civil date from days since the Unix epoch (Howard Hinnant's
/// algorithm), returning `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a Unix timestamp with an explicit UTC offset, Python
/// `%Y-%m-%dT%H:%M:%S%z` style (`2026-01-01T07:00:00+0700`).
/// The `%z` part drops sub-minute offset seconds, like Python.
pub fn format_timestamp_with_offset(secs: i64, offset_secs: i64) -> String {
    let local = secs + offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let total_minutes = offset_secs.abs() / 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{sign}{:02}{:02}",
        total_minutes / 60,
        total_minutes % 60
    )
}

/// Read a big-endian u32 / i32 at an offset, or `None` if out of range.
fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be_i32(data: &[u8], at: usize) -> Option<i32> {
    be_u32(data, at).map(|v| v as i32)
}

fn be_i64(data: &[u8], at: usize) -> Option<i64> {
    data.get(at..at + 8)
        .map(|b| i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Timestamp-aware TZif offset selection: the last transition at or
/// before `timestamp` wins; before the first transition, the first
/// non-DST type is used (glibc behaviour).
fn tzif_offset_at(pairs: &[(i64, i64)], types: &[(i32, bool)], timestamp: i64) -> i64 {
    let mut current: Option<i64> = None;
    for (when, offset) in pairs {
        if *when <= timestamp {
            current = Some(*offset);
        } else {
            break;
        }
    }
    match current {
        Some(offset) => offset,
        None => types
            .iter()
            .find(|(_, isdst)| !*isdst)
            .map(|(gmtoff, _)| *gmtoff as i64)
            .unwrap_or_else(|| types.first().map(|(g, _)| *g as i64).unwrap_or(0)),
    }
}

/// Parse a POSIX-style TZ string (`EST5`, `UTC-7`, `<+07>-7`) into a
/// UTC offset. POSIX offsets are the time added to local time to get
/// UTC, so the UTC offset is their negation. DST rules are ignored
/// (a zone file is always preferred when available).
fn posix_tz_offset(tz: &str) -> Option<i64> {
    let rest = if let Some(stripped) = tz.strip_prefix('<') {
        let end = stripped.find('>')?;
        &stripped[end + 1..]
    } else {
        let end = tz
            .find(|c: char| c.is_ascii_digit() || c == '+' || c == '-')
            .unwrap_or(tz.len());
        &tz[end..]
    };
    if rest.is_empty() {
        return None;
    }
    let (sign, digits) = match rest.strip_prefix('-') {
        Some(d) => (1i64, d),
        None => match rest.strip_prefix('+') {
            Some(d) => (-1i64, d),
            None => (-1i64, rest),
        },
    };
    let mut parts = digits.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().map_or(Ok(0), |p| p.parse()).ok()?;
    let seconds: i64 = parts.next().map_or(Ok(0), |p| p.parse()).ok()?;
    if hours > 24 || minutes >= 60 || seconds >= 60 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

/// UTC offset (seconds) for a timestamp, given an explicit TZ value
/// (`None` = unset). Zone names are read from
/// `/usr/share/zoneinfo/<name>` (path-traversal-safe: `..` components
/// are rejected); unset TZ falls back to `/etc/localtime`; a POSIX
/// offset is the last resort. Unparseable input -> 0 (UTC).
pub fn local_offset_for_tz(tz: Option<&str>, timestamp: i64) -> i64 {
    if let Some(name) = tz {
        let name = name.strip_prefix(':').unwrap_or(name);
        if !name.is_empty() {
            let safe = !name.split('/').any(|part| part == "..");
            if safe {
                let path = format!("/usr/share/zoneinfo/{name}");
                if let Ok(data) = std::fs::read(&path) {
                    if let Some(offset) = tzif_offset_with_timestamp(&data, timestamp) {
                        return offset;
                    }
                }
            }
            if let Some(offset) = posix_tz_offset(name) {
                return offset;
            }
            if name == "UTC" || name == "UTC0" || name == "GMT" || name == "GMT0" {
                return 0;
            }
            return 0;
        }
    }
    if let Ok(data) = std::fs::read("/etc/localtime") {
        if let Some(offset) = tzif_offset_with_timestamp(&data, timestamp) {
            return offset;
        }
    }
    0
}

/// Parse a TZif file and return the UTC offset (seconds) in effect at
/// `timestamp`, or `None` if the file does not parse. Supports v1
/// (32-bit transitions) and v2+ (64-bit transitions). Before the first
/// transition, the first non-DST type is used (glibc behaviour for
/// pre-transition times).
fn tzif_offset_with_timestamp(data: &[u8], timestamp: i64) -> Option<i64> {
    // Re-implement the v1/v2 walk returning pairs + types, so the
    // timestamp selection is shared and testable.
    if data.len() < 44 || &data[0..4] != b"TZif" {
        return None;
    }
    let version = data[4];
    let isutcnt = be_u32(data, 20)? as usize;
    let isstdcnt = be_u32(data, 24)? as usize;
    let leapcnt = be_u32(data, 28)? as usize;
    let timecnt = be_u32(data, 32)? as usize;
    let typecnt = be_u32(data, 36)? as usize;
    let charcnt = be_u32(data, 40)? as usize;
    // v1 data block: transitions, type indices, ttinfo, abbreviations,
    // leap records, standard/wall indicators, UT indicators.
    let v1_size = timecnt * 4 + timecnt + typecnt * 6 + charcnt + leapcnt * 8 + isstdcnt + isutcnt;
    if version == b'2' || version == b'3' || version == b'4' {
        let base = 44 + v1_size;
        if data.len() < base + 44 || &data[base..base + 4] != b"TZif" {
            return None;
        }
        let timecnt2 = be_u32(data, base + 32)? as usize;
        let typecnt2 = be_u32(data, base + 36)? as usize;
        let mut at = base + 44;
        let mut transitions = Vec::with_capacity(timecnt2);
        for _ in 0..timecnt2 {
            transitions.push(be_i64(data, at)?);
            at += 8;
        }
        let mut indices = Vec::with_capacity(timecnt2);
        for _ in 0..timecnt2 {
            indices.push(*data.get(at)? as usize);
            at += 1;
        }
        let mut types = Vec::with_capacity(typecnt2);
        for _ in 0..typecnt2 {
            let gmtoff = be_i32(data, at)?;
            let isdst = data.get(at + 4).copied().unwrap_or(0) != 0;
            types.push((gmtoff, isdst));
            at += 6;
        }
        let pairs: Vec<(i64, i64)> = transitions
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let idx = indices.get(i).copied().unwrap_or(0);
                (t, types.get(idx).map(|tt| tt.0 as i64).unwrap_or(0))
            })
            .collect();
        Some(tzif_offset_at(&pairs, &types, timestamp))
    } else {
        let mut at = 44;
        let mut transitions = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            transitions.push(be_i32(data, at)? as i64);
            at += 4;
        }
        let mut indices = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            indices.push(*data.get(at)? as usize);
            at += 1;
        }
        let mut types = Vec::with_capacity(typecnt);
        for _ in 0..typecnt {
            let gmtoff = be_i32(data, at)?;
            let isdst = data.get(at + 4).copied().unwrap_or(0) != 0;
            types.push((gmtoff, isdst));
            at += 6;
        }
        let pairs: Vec<(i64, i64)> = transitions
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let idx = indices.get(i).copied().unwrap_or(0);
                (t, types.get(idx).map(|tt| tt.0 as i64).unwrap_or(0))
            })
            .collect();
        Some(tzif_offset_at(&pairs, &types, timestamp))
    }
}

/// Format a Unix timestamp in the system local timezone, Python
/// `time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(secs))` style.
pub fn format_local_timestamp(secs: i64) -> String {
    let tz = std::env::var("TZ").ok();
    let offset = local_offset_for_tz(tz.as_deref(), secs);
    format_timestamp_with_offset(secs, offset)
}

/// Flatten a `fetchSubscriptionAction` result into the numbers that
/// matter (mirrors `cli.py:summarize_usage`).
///
/// `topupBalance`/`topupTotal` are NOT tokens: they feed only the
/// percent, while the token count is parsed from the label. `resets_at`
/// is the local `%Y-%m-%dT%H:%M:%S%z` string, or `null` when the
/// server gives no (or zero) reset time.
pub fn summarize_usage(sub: &serde_json::Value) -> serde_json::Value {
    let get = |key: &str| sub.get(key).cloned().unwrap_or(serde_json::Value::Null);
    let tier = sub.get("tier").cloned().unwrap_or(serde_json::Value::Null);
    let usage = sub.get("usage").cloned().unwrap_or(serde_json::Value::Null);
    let tier_get = |key: &str| tier.get(key).cloned().unwrap_or(serde_json::Value::Null);
    let usage_get = |key: &str| usage.get(key).cloned().unwrap_or(serde_json::Value::Null);

    let resets_at = match sub.get("usage").and_then(|u| u.get("resetsAt")) {
        Some(value) => match json_to_seconds(value) {
            Some(secs) if secs != 0 => serde_json::Value::String(format_local_timestamp(secs)),
            _ => serde_json::Value::Null,
        },
        None => serde_json::Value::Null,
    };

    let total = json_to_f64(sub.get("topupTotal").unwrap_or(&serde_json::Value::Null));
    let balance = json_to_f64(sub.get("topupBalance").unwrap_or(&serde_json::Value::Null));
    let value_label = sub.get("topupRowValueLabel");
    let bonus = match value_label {
        Some(serde_json::Value::String(label)) if total > 0.0 && !label.trim().is_empty() => {
            let used = total - balance.clamp(0.0, total);
            let used_percent = round_half_even(100.0 * used / total);
            serde_json::json!({
                "label": sub.get("topupRowLabel").and_then(|v| v.as_str()).unwrap_or("Additional tokens"),
                "value_label": label,
                "used_percent": used_percent,
                "remaining_tokens": parse_tokens_left(label),
                "balance_units": sub.get("topupBalance").cloned().unwrap_or(serde_json::Value::Null),
                "total_units": sub.get("topupTotal").cloned().unwrap_or(serde_json::Value::Null),
            })
        }
        _ => serde_json::Value::Null,
    };

    serde_json::json!({
        "plan": tier_get("name"),
        "tier": tier_get("tierCode"),
        "paid": tier_get("isPaid"),
        "label": get("usageRowLabel"),
        "value_label": get("usageRowValueLabel"),
        "state": usage_get("state"),
        "used_percent": usage_get("percentUsed"),
        "quota_status": usage_get("quotaStatus"),
        "resets_at": resets_at,
        "status": get("statusSubtitle"),
        "bonus_tokens": bonus,
        "upgrade": get("entryPointCtaLabel"),
    })
}

/// Flatten a GET `/api/hatch/invite` object (mirrors
/// `cli.py:summarize_invite`). `uses_remaining` falls back to
/// `max(0, max_uses - use_count)` when the server does not give it.
pub fn summarize_invite(d: &serde_json::Value) -> serde_json::Value {
    let get = |key: &str| d.get(key).cloned().unwrap_or(serde_json::Value::Null);
    let reward = match d.get("reward") {
        Some(serde_json::Value::Object(reward)) => serde_json::json!({
            "title": reward.get("title").cloned().unwrap_or(serde_json::Value::Null),
            "body": reward.get("body").cloned().unwrap_or(serde_json::Value::Null),
        }),
        _ => serde_json::Value::Null,
    };
    let remaining = match d.get("uses_remaining") {
        Some(value) if !matches!(value, serde_json::Value::Null) => value.clone(),
        _ => match (
            d.get("max_uses").and_then(|v| v.as_i64()),
            d.get("use_count").and_then(|v| v.as_i64()),
        ) {
            (Some(max), Some(used)) => serde_json::json!(std::cmp::max(max - used, 0)),
            _ => serde_json::Value::Null,
        },
    };
    serde_json::json!({
        "state": get("invite_state"),
        "code": get("code"),
        "link": get("invite_link"),
        "max_uses": get("max_uses"),
        "used": get("use_count"),
        "uses_remaining": remaining,
        "reward": reward,
        "offer": get("subtitle"),
        "share_text": get("share_text"),
        "redeemed_someone_elses_code": get("has_redeemed_invite_code"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tokens_left_docstring_examples() {
        assert_eq!(
            parse_tokens_left("0% used (3B tokens left)"),
            Some(3_000_000_000)
        );
        assert_eq!(
            parse_tokens_left("Đã dùng 4% (còn 12 tỷ token)"),
            Some(12_000_000_000)
        );
    }

    #[test]
    fn parse_tokens_left_units_and_scales() {
        assert_eq!(parse_tokens_left("5K tokens"), Some(5_000));
        assert_eq!(parse_tokens_left("2.5M tokens left"), Some(2_500_000));
        assert_eq!(parse_tokens_left("1T tokens"), Some(1_000_000_000_000));
        assert_eq!(parse_tokens_left("3 nghìn token"), Some(3_000));
        assert_eq!(parse_tokens_left("7 triệu token"), Some(7_000_000));
        assert_eq!(parse_tokens_left("42 tokens"), Some(42));
    }

    #[test]
    fn parse_tokens_left_comma_rule() {
        // One comma, no dot -> decimal point.
        assert_eq!(parse_tokens_left("1,5M tokens"), Some(1_500_000));
        // A dot present -> commas are thousands separators.
        assert_eq!(parse_tokens_left("1,000.5K tokens"), Some(1_000_500));
        // Two commas -> thousands separators.
        assert_eq!(parse_tokens_left("1,000,000 tokens"), Some(1_000_000));
    }

    #[test]
    fn parse_tokens_left_ignores_numbers_without_token() {
        assert_eq!(parse_tokens_left("0% used"), None);
        assert_eq!(parse_tokens_left(""), None);
        assert_eq!(parse_tokens_left("no numbers here"), None);
        // A percent number first, then the real label: the first
        // number fails the token check, the second matches.
        assert_eq!(
            parse_tokens_left("Đã dùng 4% (còn 12 tỷ token)"),
            Some(12_000_000_000)
        );
    }

    #[test]
    fn parse_tokens_left_case_insensitive() {
        assert_eq!(parse_tokens_left("3b TOKENS LEFT"), Some(3_000_000_000));
    }

    #[test]
    fn human_tokens_examples() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1000), "1K");
        assert_eq!(human_tokens(1500), "1.5K");
        assert_eq!(human_tokens(1_000_000), "1M");
        assert_eq!(human_tokens(2_500_000_000), "2.5B");
        assert_eq!(human_tokens(-1500), "-1.5K");
    }

    #[test]
    fn format_timestamp_with_offset_examples() {
        assert_eq!(
            format_timestamp_with_offset(0, 0),
            "1970-01-01T00:00:00+0000"
        );
        assert_eq!(
            format_timestamp_with_offset(0, 7 * 3600),
            "1970-01-01T07:00:00+0700"
        );
        assert_eq!(
            format_timestamp_with_offset(1_767_000_000, -5 * 3600),
            "2025-12-29T04:20:00-0500"
        );
    }

    #[test]
    fn tzif_offset_for_known_zones() {
        // Ho Chi Minh is UTC+7 year-round (no DST).
        let offset = local_offset_for_tz(Some("Asia/Ho_Chi_Minh"), 1_767_000_000);
        assert_eq!(offset, 7 * 3600);
        let utc = local_offset_for_tz(Some("UTC"), 1_767_000_000);
        assert_eq!(utc, 0);
    }

    #[test]
    fn posix_tz_offset_sign_convention() {
        // POSIX: the number is added to local time to get UTC.
        assert_eq!(posix_tz_offset("EST5"), Some(-5 * 3600));
        assert_eq!(posix_tz_offset("UTC-7"), Some(7 * 3600));
        assert_eq!(posix_tz_offset("<+07>-7"), Some(7 * 3600));
        assert_eq!(posix_tz_offset("XYZ"), None);
    }

    #[test]
    fn summarize_usage_full_shape() {
        let sub = json!({
            "tier": {"name": "Pro", "tierCode": "PRO", "isPaid": true},
            "usage": {"state": "ACTIVE", "percentUsed": 40, "resetsAt": 0, "quotaStatus": "SUFFICIENT"},
            "usageRowLabel": "Weekly tokens",
            "usageRowValueLabel": "60% left",
            "statusSubtitle": "All good",
            "topupTotal": 100,
            "topupBalance": 25,
            "topupRowLabel": "Bonus",
            "topupRowValueLabel": "3B tokens left",
            "entryPointCtaLabel": "Upgrade",
        });
        let out = summarize_usage(&sub);
        assert_eq!(out["plan"], "Pro");
        assert_eq!(out["tier"], "PRO");
        assert_eq!(out["paid"], true);
        assert_eq!(out["label"], "Weekly tokens");
        assert_eq!(out["resets_at"], serde_json::Value::Null);
        let bonus = &out["bonus_tokens"];
        assert_eq!(bonus["label"], "Bonus");
        assert_eq!(bonus["used_percent"], 75);
        assert_eq!(bonus["remaining_tokens"], 3_000_000_000i64);
        assert_eq!(bonus["balance_units"], 25);
        assert_eq!(bonus["total_units"], 100);
    }

    #[test]
    fn summarize_usage_no_bonus_when_total_zero_or_label_blank() {
        let sub = json!({"tier": {}, "usage": {}, "topupTotal": 0, "topupRowValueLabel": "3B tokens left"});
        assert_eq!(
            summarize_usage(&sub)["bonus_tokens"],
            serde_json::Value::Null
        );
        let sub = json!({"tier": {}, "usage": {}, "topupTotal": 10, "topupRowValueLabel": "  "});
        assert_eq!(
            summarize_usage(&sub)["bonus_tokens"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn summarize_invite_computes_remaining_with_floor_zero() {
        let d = json!({
            "invite_state": "ELIGIBLE", "code": "FAKE12", "invite_link": "https://muse.ai/i/FAKE12",
            "max_uses": 5, "use_count": 7, "reward": {"title": "T", "body": "B"},
            "subtitle": "Offer", "share_text": "Share", "has_redeemed_invite_code": false,
        });
        let out = summarize_invite(&d);
        assert_eq!(out["code"], "FAKE12");
        assert_eq!(out["uses_remaining"], 0);
        assert_eq!(out["reward"]["title"], "T");
        assert_eq!(out["used"], 7);
        // Server-provided remaining wins over the computed one.
        let d = json!({"max_uses": 5, "use_count": 1, "uses_remaining": 9});
        assert_eq!(summarize_invite(&d)["uses_remaining"], 9);
    }


}
