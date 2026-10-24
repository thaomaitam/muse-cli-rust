//! Wait state machine (writer B): pure `Waiter` + `run_wait` orchestration.
//!
//! Faithful to Python `wait.py` + `cli.py:run_wait`/`make_auto_approver`:
//! - Approval events are checked BEFORE task status in the same event.
//! - Each approval id is offered to the approver exactly once; approvals
//!   the approver handled do not stop the wait later.
//! - `require_running` ignores terminal statuses until a non-terminal
//!   status has been seen (the server replays a stale `completed` on
//!   subscribe).
//! - Only the root agent's task status ends a turn (a subagent's
//!   `completed` is ignored).
//! - `TimeoutError` from the gateway layer (exit 4, a `CliError`) is NOT
//!   the same as a `WaitResult` timeout (exit 124): the first is an
//!   error return, the second a normal `WaitResult`.
//! - Send's exit 5 (no delivery ack) belongs to the send command, NOT
//!   to this module.
//!
//! DELIBERATE ADDITION TO CONTRACT: `WaitResult` gains an `approvals`
//! field (empty except when the outcome is `NeedsApproval`). Python's
//! `run_wait` result includes `approvals` for that outcome, and without
//! this field writer C could not print the needs-approval JSON from a
//! `run_wait` return value at all. Adding a field breaks no reader of
//! the struct; only `wait.rs` constructs it.
//!
//! Approver contract (for `Waiter::feed`): the closure receives the
//! newly-offered approval items and returns `(approved_rows,
//! error_rows)`. An approval counts as handled when its id appears in
//! `approved_rows`, either as a plain string id or as an object with an
//! `approval_id` field (the shape `run_wait`'s own approver produces:
//! `{approval_id, title, kind, scope, decision: "allow_always"}`).
//! `run_wait`'s approver only ever decides `allow_always` and never
//! falls back to once/session/today.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::errors::CliError;
use crate::gateway::Gateway;

/// One pending approval, as offered to the approver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalItem {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub scope: String,
    /// True when the approval offers `allow_always` (or lists no options
    /// at all, which Python treats as accepting it).
    pub has_forever_option: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcomeKind {
    Done,
    Failed,
    NeedsInput,
    NeedsApproval,
    Timeout,
}

/// The decided result of a wait, mirroring Python `run_wait`'s result
/// dict (`outcome`, `status`, `after_seq`, `final_text`, `replies`,
/// plus `approvals` / `auto_approved` / `auto_approve_errors` /
/// `widgets` / `needs_input_reason` / `question` when they apply).
/// `run_wait` fills every field from data it already has: `status`
/// is the last root-agent task status the `Waiter` saw (Python's
/// `w.status`, `None` when no task status was ever seen, e.g. a pure
/// timeout), `replies`/`final_text` come from the history re-read,
/// `widgets` is the last reply's open widgets, and
/// `needs_input_reason` is `"widget"` when the last reply has widgets,
/// else the reason for its text (only when `kind == NeedsInput`).
/// `feed` alone cannot know the re-read fields, so it leaves
/// `final_text`/`question`/`replies`/`widgets`/`needs_input_reason`
/// empty and fills `status`/`after_seq` from its own state.
#[derive(Debug, Clone)]
pub struct WaitResult {
    pub kind: WaitOutcomeKind,
    pub exit_code: i32,
    /// Last root-agent task status seen, or `None` if none was seen.
    pub status: Option<String>,
    /// The resolved `after_seq` baseline (the caller's value, or the
    /// max seq of `history limit=1` when the caller passed `None`).
    pub after_seq: i64,
    pub final_text: Option<String>,
    pub question: Option<String>,
    /// Assistant replies after `after_seq`, oldest first.
    pub replies: Vec<Value>,
    /// Open widgets of the last reply, or empty.
    pub widgets: Vec<Value>,
    /// Why the final reply needs a human, only for `NeedsInput`.
    pub needs_input_reason: Option<String>,
    pub auto_approved: Vec<Value>,
    pub auto_approve_errors: Vec<Value>,
    /// Full described approvals, only when `kind == NeedsApproval`
    /// (see the module doc note about this CONTRACT addition).
    pub approvals: Vec<Value>,
}

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 2;
pub const EXIT_HUMAN: i32 = 4;
pub const EXIT_TIMEOUT: i32 = 124;

const DONE: [&str; 7] = [
    "completed",
    "complete",
    "idle",
    "done",
    "finished",
    "succeeded",
    "success",
];
const FAILED: [&str; 8] = [
    "failed",
    "failure",
    "error",
    "errored",
    "cancelled",
    "canceled",
    "aborted",
    "timed_out",
];

fn exit_code_for(kind: WaitOutcomeKind) -> i32 {
    match kind {
        WaitOutcomeKind::Done => EXIT_OK,
        WaitOutcomeKind::Failed => EXIT_FAILED,
        WaitOutcomeKind::NeedsInput | WaitOutcomeKind::NeedsApproval => EXIT_HUMAN,
        WaitOutcomeKind::Timeout => EXIT_TIMEOUT,
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// First non-empty string among the given values, else `""`.
fn first_string(values: &[Option<&Value>]) -> String {
    for value in values {
        if let Some(Value::String(text)) = value {
            if !text.is_empty() {
                return text.clone();
            }
        }
    }
    String::new()
}

fn event_string_field(event: &Value, key: &str) -> Option<String> {
    match event.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// Python `event_name`: `event` or `event_name` or `type`, first truthy.
pub fn event_name(event: &Value) -> Option<String> {
    event_string_field(event, "event")
        .or_else(|| event_string_field(event, "event_name"))
        .or_else(|| event_string_field(event, "type"))
}

fn payload_of(event: &Value) -> &serde_json::Map<String, Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    match event.get("payload") {
        Some(Value::Object(map)) => map,
        _ => EMPTY.get_or_init(serde_json::Map::new),
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python `in_scope`: with a thread, only events whose
/// `payload.session_id` is absent/`null` or equal to the thread; without
/// a thread, only events whose payload is not a thread event.
pub fn in_scope(event: &Value, thread: Option<&str>) -> bool {
    let payload = payload_of(event);
    match thread {
        Some(thread) => match payload.get("session_id") {
            None | Some(Value::Null) => true,
            Some(Value::String(session)) => session == thread,
            Some(_) => false,
        },
        None => !payload.get("is_thread").is_some_and(json_truthy),
    }
}

/// Lower-cased status of the root agent's task, or `None`.
pub fn task_status(event: &Value, thread: Option<&str>) -> Option<String> {
    if event_name(event).as_deref() != Some("task.status") || !in_scope(event, thread) {
        return None;
    }
    let payload = payload_of(event);
    let root = payload
        .get("root_agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let agent = payload
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !root.is_empty() && !agent.is_empty() && agent != root {
        return None;
    }
    match payload.get("status") {
        Some(Value::String(status)) if !status.is_empty() => Some(status.to_lowercase()),
        None | Some(Value::Null) => None,
        Some(other) => {
            let text = value_to_string(other);
            if text.is_empty() {
                None
            } else {
                Some(text.to_lowercase())
            }
        }
    }
}

/// Pending approvals for events whose name starts with `"approvals"`,
/// from `payload.pending_approvals` (fallback `payload.pending`).
/// A non-list value, or an empty list, yields `None` (Python's `if pend`
/// treats an empty list as absent).
pub fn pending_approvals(event: &Value) -> Option<Vec<Value>> {
    let name = event_name(event).unwrap_or_default();
    if !name.starts_with("approvals") {
        return None;
    }
    let payload = payload_of(event);
    let pend = payload
        .get("pending_approvals")
        .or_else(|| payload.get("pending"))?;
    match pend {
        Value::Array(items) if !items.is_empty() => Some(items.clone()),
        _ => None,
    }
}

/// Describe one approval exactly like Python `describe_approval`, as a
/// JSON object. The `options` field is a list of decision `kind`s, or
/// `null` when the approval lists none.
pub fn describe_approval(approval: &Value) -> Value {
    let display = approval.get("display").and_then(|v| v.as_object());
    let question = display
        .and_then(|d| d.get("permission_question"))
        .and_then(|v| v.as_object());
    let mut rows: HashMap<String, String> = HashMap::new();
    if let Some(detail_rows) = display
        .and_then(|d| d.get("detail_rows"))
        .and_then(|v| v.as_array())
    {
        for row in detail_rows {
            if let (Some(label), Some(value)) = (
                row.get("label").and_then(|v| v.as_str()),
                row.get("value").and_then(|v| v.as_str()),
            ) {
                rows.insert(label.to_string(), value.to_string());
            }
        }
    }
    let question_text = question
        .and_then(|q| q.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace("{assistant}", "Muse");
    let options: Option<Vec<Value>> = approval
        .get("decision_options")
        .and_then(|v| v.as_array())
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    o.get("kind")
                        .and_then(|v| v.as_str())
                        .map(|s| Value::String(s.to_string()))
                })
                .collect()
        })
        .filter(|opts: &Vec<Value>| !opts.is_empty());
    let kind = question
        .and_then(|q| q.get("kind"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            approval
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let scope = question
        .and_then(|q| q.get("scope"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| rows.get("Host").cloned());
    let why = display
        .and_then(|d| d.get("short_explanation"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            display
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            approval
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    serde_json::json!({
        "approval_id": approval.get("approval_id").cloned().unwrap_or(Value::Null),
        "title": display
            .and_then(|d| d.get("summary_title"))
            .or_else(|| display.and_then(|d| d.get("display_name")))
            .cloned()
            .unwrap_or(Value::Null),
        "question": if question_text.is_empty() { Value::Null } else { Value::String(question_text) },
        "kind": kind.map(Value::String).unwrap_or(Value::Null),
        "scope": scope.map(Value::String).unwrap_or(Value::Null),
        "why": why.map(Value::String).unwrap_or(Value::Null),
        "expires_at_ms": approval.get("request_expires_at_ms").cloned().unwrap_or(Value::Null),
        "options": options.map(Value::Array).unwrap_or(Value::Null),
    })
}

/// True when the approval can be decided with `allow_always`: it offers
/// that option, or it lists no options at all (older servers are assumed
/// to accept it).
pub fn offers_forever(described: &Value) -> bool {
    match described.get("options") {
        None | Some(Value::Null) => true,
        Some(Value::Array(options)) => options.iter().any(|o| o.as_str() == Some("allow_always")),
        Some(_) => true,
    }
}

fn approval_item(described: &Value) -> ApprovalItem {
    ApprovalItem {
        id: described
            .get("approval_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        title: described
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        kind: described
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        scope: described
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        has_forever_option: offers_forever(described),
    }
}

/// Remove every `[[hatch_widget:<id>]]` marker, then trim.
pub fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[hatch_widget:") {
        out.push_str(&rest[..start]);
        match rest[start..].find("]]") {
            Some(end) => {
                rest = &rest[start + end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Unanswered hatch widgets in a history event.
pub fn open_widgets(event: &Value) -> Vec<Value> {
    let payload = payload_of(event);
    let mut out = Vec::new();
    let Some(widgets) = payload.get("widgets").and_then(|v| v.as_array()) else {
        return out;
    };
    for widget in widgets {
        let state = widget.get("state").and_then(|v| v.as_object());
        // Answered when any of these keys holds a value outside
        // (null, "", [], {}). Note: `false` and `0` count as answered,
        // exactly like Python's `not in (None, "", [], {})` test.
        let answered = [
            "value",
            "selected",
            "selection",
            "answer",
            "submitted",
            "response",
        ]
        .iter()
        .any(|key| match state.and_then(|s| s.get(*key)) {
            None | Some(Value::Null) => false,
            Some(Value::String(text)) => !text.is_empty(),
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::Object(map)) => !map.is_empty(),
            Some(_) => true,
        });
        if !answered {
            out.push(serde_json::json!({
                "widget_id": widget.get("widget_id").cloned().unwrap_or(Value::Null),
                "kind": widget.get("kind").cloned().unwrap_or(Value::Null),
                "text": widget.get("display_text").or_else(|| widget.get("fallback_text")).cloned().unwrap_or(Value::Null),
                "options": widget.get("data").and_then(|d| d.get("options")).cloned().unwrap_or(Value::Null),
            }));
        }
    }
    out
}

/// Complete assistant messages with `seq > after_seq`, oldest first.
/// Uses ONLY the `event_name` key (Python `replies_after` does not use
/// the `event`/`type` fallbacks) and does NOT filter on
/// `reply_to_message_id` (unlike the older `is_reply` logic).
pub fn replies_after(events: &[Value], after_seq: i64) -> Vec<Value> {
    let mut sorted: Vec<&Value> = events.iter().collect();
    sorted.sort_by_key(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0));
    let mut out = Vec::new();
    for event in sorted {
        let seq = event.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
        if seq <= after_seq {
            continue;
        }
        if event.get("event_name").and_then(|v| v.as_str()) != Some("message.assistant") {
            continue;
        }
        let payload = payload_of(event);
        let text = first_string(&[
            payload.get("display_text"),
            payload.get("content"),
            event.get("display_text"),
        ]);
        if text.is_empty() {
            continue;
        }
        let mut reply = serde_json::json!({
            "seq": seq,
            "message_id": payload.get("message_id").or_else(|| event.get("message_id")).cloned().unwrap_or(Value::Null),
            "text": clean_text(&text),
        });
        let widgets = open_widgets(event);
        if !widgets.is_empty() {
            reply
                .as_object_mut()
                .map(|map| map.insert("widgets".to_string(), Value::Array(widgets)));
        }
        out.push(reply);
    }
    out
}

const WAIT_PHRASES: [&str; 34] = [
    "chờ boss",
    "chờ bạn",
    "tôi chờ",
    "mình chờ",
    "đợi boss",
    "đợi bạn",
    "boss muốn",
    "bạn muốn",
    "boss có muốn",
    "bạn có muốn",
    "trước khi tiếp tục",
    "trước khi làm tiếp",
    "boss xác nhận",
    "bạn xác nhận",
    "cần boss",
    "cần bạn",
    "boss quyết",
    "bạn quyết",
    "hay không?",
    "được không?",
    "nhé?",
    "should i",
    "do you want",
    "would you like",
    "waiting for your",
    "let me know if",
    "let me know whether",
    "please confirm",
    "can you confirm",
    "before i continue",
    "before i proceed",
    "shall i",
    "which do you prefer",
    "your call",
];

/// Why the final reply is waiting on a human, or `None`. Check order:
/// gate/policy question (but `none` does not count), safety check,
/// waiting phrase, trailing question mark.
pub fn needs_input_reason(text: &str) -> Option<String> {
    let cleaned = clean_text(text);
    if cleaned.is_empty() {
        return None;
    }
    let low = cleaned.to_lowercase();
    if gate_question_present(&low) {
        return Some("gate/policy question".to_string());
    }
    if safety_trigger_present(&low) {
        return Some("stopped by a safety check".to_string());
    }
    for phrase in WAIT_PHRASES {
        if low.contains(phrase) {
            return Some(format!("waiting phrase: '{phrase}'"));
        }
    }
    let tail = cleaned
        .trim_end_matches([' ', '\n', '*', '_', '`', ')', '"', '\''])
        .lines()
        .last()
        .unwrap_or("")
        .trim_end();
    if tail.ends_with('?') || tail.ends_with('？') {
        return Some("ends with a question".to_string());
    }
    None
}

fn gate_question_present(low: &str) -> bool {
    // (gate|policy)[ _-]?questions?\s*[:=]\s*(?!none\b)\S, case-insensitive
    // (input is already lower-cased).
    let mut search_from = 0;
    while search_from < low.len() {
        let gate = low[search_from..].find("gate");
        let policy = low[search_from..].find("policy");
        let Some(offset) = (match (gate, policy) {
            (Some(g), Some(p)) => Some(g.min(p)),
            (Some(g), None) => Some(g),
            (None, Some(p)) => Some(p),
            (None, None) => None,
        }) else {
            return false;
        };
        let mut index = search_from
            + offset
            + if low[search_from + offset..].starts_with("gate") {
                4
            } else {
                6
            };
        if index < low.len() && matches!(low.as_bytes()[index], b' ' | b'_' | b'-') {
            index += 1;
        }
        if !low[index..].starts_with("question") {
            search_from = index.max(search_from + 1);
            continue;
        }
        index += 8;
        if low[index..].starts_with('s') {
            index += 1;
        }
        while index < low.len() && low.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= low.len() || !matches!(low.as_bytes()[index], b':' | b'=') {
            search_from = index.max(search_from + 1);
            continue;
        }
        index += 1;
        while index < low.len() && low.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= low.len() {
            return false;
        }
        // Negative lookahead: the next word must not be exactly "none".
        let rest = &low[index..];
        let word_end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if &rest[..word_end] == "none" {
            search_from = index + word_end.max(1);
            continue;
        }
        return true;
    }
    false
}

fn safety_trigger_present(low: &str) -> bool {
    if low.contains("safety check")
        || low.contains("kiểm tra an toàn")
        || low.contains("cơ chế an toàn")
        || low.contains("refuse to run")
        || low.contains("refuse to do")
        || low.contains("refuse to continue")
        || low.contains("refused to run")
        || low.contains("refused to do")
        || low.contains("refused to continue")
        || low.contains("từ chối chạy")
        || low.contains("từ chối làm")
    {
        return true;
    }
    // tripped .{0,30}safety: "safety" must start within 30 chars after
    // "tripped " (a 36-char window covers start positions 0..=30).
    let mut search_from = 0;
    while let Some(offset) = low[search_from..].find("tripped ") {
        let after = &low[search_from + offset + 8..];
        if after
            .chars()
            .take(36)
            .collect::<String>()
            .contains("safety")
        {
            return true;
        }
        search_from += offset + 8;
    }
    false
}

/// The question sentence(s) for a needs-input reply: up to the last two
/// sentences containing `?`, else the last line of the cleaned text.
pub fn question_of(text: &str) -> String {
    let cleaned = clean_text(text);
    if cleaned.is_empty() {
        return String::new();
    }
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        if matches!(c, '.' | '!' | '?' | '。') {
            // Split after the terminator when followed by whitespace.
            let mut whitespace = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_whitespace() {
                    whitespace.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            if !whitespace.is_empty() {
                sentences.push(current.trim().to_string());
                current = String::new();
            }
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }
    let questions: Vec<&String> = sentences.iter().filter(|s| s.contains('?')).collect();
    if !questions.is_empty() {
        let start = questions.len().saturating_sub(2);
        return questions[start..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ");
    }
    cleaned.lines().last().unwrap_or("").to_string()
}

/// `seq` of the first `message.user` event after `baseline` whose text
/// contains (or is contained in) `text`, or `None`.
pub fn user_ack(events: &[Value], baseline: i64, text: Option<&str>) -> Option<i64> {
    let mut sorted: Vec<&Value> = events.iter().collect();
    sorted.sort_by_key(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0));
    for event in sorted {
        let seq = event.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
        if seq <= baseline {
            continue;
        }
        if event.get("event_name").and_then(|v| v.as_str()) != Some("message.user") {
            continue;
        }
        let payload = payload_of(event);
        let event_text = first_string(&[
            payload.get("display_text"),
            payload.get("content"),
            event.get("display_text"),
        ]);
        // Python: text.strip() in t or t.strip() in text.
        if let Some(want) = text {
            let want = want.trim();
            let event_text = event_text.trim();
            if event_text.contains(want) || want.contains(event_text) {
                return Some(seq);
            }
        } else {
            return Some(seq);
        }
    }
    None
}

/// Pure event consumer: feed live subscribe events, get a decision.
pub struct Waiter {
    thread: Option<String>,
    after_seq: Option<i64>,
    require_running: bool,
    seen_running: bool,
    status: Option<String>,
    offered: HashSet<String>,
    handled: HashSet<String>,
    auto_approved: Vec<Value>,
    auto_approve_errors: Vec<Value>,
}

impl Waiter {
    pub fn new(thread: Option<String>, after_seq: Option<i64>, require_running: bool) -> Self {
        Self {
            thread,
            after_seq,
            require_running,
            seen_running: false,
            status: None,
            offered: HashSet::new(),
            handled: HashSet::new(),
            auto_approved: Vec::new(),
            auto_approve_errors: Vec::new(),
        }
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn after_seq(&self) -> Option<i64> {
        self.after_seq
    }

    pub fn auto_approved(&self) -> &[Value] {
        &self.auto_approved
    }

    pub fn auto_approve_errors(&self) -> &[Value] {
        &self.auto_approve_errors
    }

    /// Feed one event. Returns `Some(result)` exactly when the wait is
    /// decided; `None` means keep waiting. Approval is checked before
    /// task status, as in Python.
    pub fn feed(
        &mut self,
        event: &Value,
        approver: &mut dyn FnMut(&[ApprovalItem]) -> (Vec<Value>, Vec<Value>),
    ) -> Option<WaitResult> {
        if !event.is_object() {
            return None;
        }
        if let Some(pending) = pending_approvals(event) {
            let described: Vec<Value> = pending.iter().map(describe_approval).collect();
            let new: Vec<ApprovalItem> = described
                .iter()
                .filter(|d| {
                    !self
                        .offered
                        .contains(d.get("approval_id").and_then(|v| v.as_str()).unwrap_or(""))
                })
                .map(approval_item)
                .collect();
            if !new.is_empty() {
                for item in &new {
                    self.offered.insert(item.id.clone());
                }
                let (approved, errors) = approver(&new);
                for row in &approved {
                    if let Some(id) = approved_row_id(row) {
                        self.handled.insert(id);
                    }
                }
                self.auto_approved.extend(approved);
                self.auto_approve_errors.extend(errors);
            }
            let left: Vec<Value> = described
                .iter()
                .filter(|d| {
                    !self
                        .handled
                        .contains(d.get("approval_id").and_then(|v| v.as_str()).unwrap_or(""))
                })
                .cloned()
                .collect();
            if left.is_empty() {
                return None;
            }
            return Some(WaitResult {
                kind: WaitOutcomeKind::NeedsApproval,
                exit_code: EXIT_HUMAN,
                status: self.status.clone(),
                after_seq: self.after_seq.unwrap_or(0),
                final_text: None,
                question: None,
                replies: Vec::new(),
                widgets: Vec::new(),
                needs_input_reason: None,
                auto_approved: self.auto_approved.clone(),
                auto_approve_errors: self.auto_approve_errors.clone(),
                approvals: left,
            });
        }
        if let Some(status) = task_status(event, self.thread.as_deref()) {
            self.status = Some(status.clone());
            if DONE.contains(&status.as_str()) || FAILED.contains(&status.as_str()) {
                if self.require_running && !self.seen_running {
                    return None;
                }
                let kind = if DONE.contains(&status.as_str()) {
                    WaitOutcomeKind::Done
                } else {
                    WaitOutcomeKind::Failed
                };
                return Some(WaitResult {
                    kind,
                    exit_code: exit_code_for(kind),
                    status: self.status.clone(),
                    after_seq: self.after_seq.unwrap_or(0),
                    final_text: None,
                    question: None,
                    replies: Vec::new(),
                    widgets: Vec::new(),
                    needs_input_reason: None,
                    auto_approved: self.auto_approved.clone(),
                    auto_approve_errors: self.auto_approve_errors.clone(),
                    approvals: Vec::new(),
                });
            }
            self.seen_running = true;
        }
        None
    }
}

fn approved_row_id(row: &Value) -> Option<String> {
    match row {
        Value::String(id) => Some(id.clone()),
        Value::Object(_) => row
            .get("approval_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn cli_error_message(err: &CliError) -> String {
    match err {
        CliError::Auth(message)
        | CliError::Value(message)
        | CliError::Timeout(message)
        | CliError::Io(message) => message.clone(),
        CliError::Gateway { status, payload } => {
            format!("gateway status={status} payload={payload:?}")
        }
    }
}

fn history_events(
    gw: &mut Gateway,
    limit: i64,
    thread: Option<&str>,
) -> Result<Vec<Value>, CliError> {
    let mut body = serde_json::json!({"limit": limit});
    if let Some(thread) = thread {
        body.as_object_mut()
            .map(|map| map.insert("session_id".to_string(), Value::String(thread.to_string())));
    }
    let value = gw.call_json(
        "chat.history",
        &[],
        Some(&body),
        None,
        Duration::from_secs(30),
    )?;
    match value {
        Value::Object(map) => match map.get("chat_events") {
            Some(Value::Array(events)) => Ok(events.clone()),
            _ => Ok(Vec::new()),
        },
        _ => Ok(Vec::new()),
    }
}

/// Subscribe until the turn ends, a human is needed, or the timeout, then
/// re-read the final reply from history.
///
/// `auto_approve` gates the internal approver: when true, newly offered
/// approvals are decided with `allow_always` ONLY (never once/session/
/// today) on a gateway obtained from `approver_gateway_factory`, and the
/// wait continues; approvals without a forever option, or a failed
/// decide, are recorded in `auto_approve_errors` and still end the wait
/// with `NeedsApproval`.
///
/// IMPORTANT for writer C: `approver_gateway_factory` is ALSO used for
/// the history re-read, which Python does on a fresh connection (the
/// subscribe reader thread still owns the first one, and reusing it
/// would let the lingering reader steal response frames). The factory
/// must therefore be a working connect closure even when `auto_approve`
/// is false. Fix round 2 removed the old deviation (factory failure
/// yielding empty replies): like Python, where the re-read connect
/// sits outside the `try` and only the `call_json` errors are
/// excepted, a factory failure here propagates, while only
/// Gateway/Timeout errors from the history call fall back to empty
/// replies.
pub fn run_wait(
    gw: &mut Gateway,
    thread: Option<String>,
    after_seq: Option<i64>,
    timeout: Duration,
    require_running: bool,
    auto_approve: bool,
    approver_gateway_factory: &mut dyn FnMut() -> Result<Gateway, CliError>,
) -> Result<WaitResult, CliError> {
    let mut after_seq = after_seq;
    if after_seq.is_none() {
        let events = history_events(gw, 1, thread.as_deref())?;
        after_seq = Some(
            events
                .iter()
                .map(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0))
                .max()
                .unwrap_or(0),
        );
    }
    let after_seq = after_seq.unwrap_or(0);
    let mut waiter = Waiter::new(thread.clone(), Some(after_seq), require_running);
    let mut decided: Option<WaitResult> = None;
    let mut on_record = |record: &str| {
        let Ok(event) = serde_json::from_str::<Value>(record) else {
            return false;
        };
        if auto_approve {
            let mut approver = |items: &[ApprovalItem]| -> (Vec<Value>, Vec<Value>) {
                let mut approved: Vec<Value> = Vec::new();
                let mut errors: Vec<Value> = Vec::new();
                let mut gateway = match approver_gateway_factory() {
                    Ok(gateway) => gateway,
                    Err(err) => {
                        for item in items {
                            errors.push(serde_json::json!({
                                "approval_id": item.id,
                                "error": cli_error_message(&err),
                            }));
                        }
                        return (approved, errors);
                    }
                };
                for item in items {
                    let row = serde_json::json!({
                        "approval_id": item.id,
                        "title": item.title,
                        "kind": item.kind,
                        "scope": item.scope,
                        "decision": "allow_always",
                    });
                    if !item.has_forever_option {
                        errors.push(serde_json::json!({
                            "approval_id": item.id,
                            "title": item.title,
                            "kind": item.kind,
                            "scope": item.scope,
                            "decision": "allow_always",
                            "error": "no forever option",
                        }));
                        eprintln!(
                            "auto-approve SKIPPED {}: Muse offers no forever option",
                            item.id
                        );
                        continue;
                    }
                    let path_params = vec![("approval_id".to_string(), item.id.clone())];
                    let body = serde_json::json!({"decision": "allow_always"});
                    match gateway.call_json(
                        "egress.approval.decide",
                        &path_params,
                        Some(&body),
                        None,
                        Duration::from_secs(30),
                    ) {
                        Ok(_) => {
                            approved.push(row);
                            eprintln!(
                                "auto-approved {} (allow_always): {} [{} {}]",
                                item.id, item.title, item.kind, item.scope
                            );
                        }
                        Err(err) => {
                            let mut error_row = row;
                            error_row.as_object_mut().map(|map| {
                                map.insert(
                                    "error".to_string(),
                                    Value::String(cli_error_message(&err)),
                                )
                            });
                            errors.push(error_row);
                            eprintln!(
                                "auto-approve FAILED {}: {}",
                                item.id,
                                cli_error_message(&err)
                            );
                        }
                    }
                }
                gateway.close();
                (approved, errors)
            };
            if let Some(result) = waiter.feed(&event, &mut approver) {
                decided = Some(result);
                return true;
            }
            false
        } else {
            let mut noop =
                |_: &[ApprovalItem]| -> (Vec<Value>, Vec<Value>) { (Vec::new(), Vec::new()) };
            if let Some(result) = waiter.feed(&event, &mut noop) {
                decided = Some(result);
                return true;
            }
            false
        }
    };
    let mut subscribe_body = serde_json::json!({"capabilities": ["chat_cancel", "delta_stream"]});
    if let Some(thread) = &thread {
        subscribe_body
            .as_object_mut()
            .map(|map| map.insert("session_id".to_string(), Value::String(thread.clone())));
    }
    gw.subscribe_records(
        "chat.subscribe",
        &[],
        Some(&subscribe_body),
        None,
        timeout,
        timeout,
        1_000_000_000,
        &mut on_record,
    )?;

    // Fresh connection for the history re-read (see the doc
    // comment). Python puts this connect OUTSIDE the try: a
    // connect failure propagates, while only `call_json`
    // (Gateway/Timeout) errors are swallowed into empty replies.
    let mut fresh = approver_gateway_factory()?;
    let replies = match history_events(&mut fresh, 30, thread.as_deref()) {
        Ok(events) => replies_after(&events, after_seq),
        Err(CliError::Gateway { .. } | CliError::Timeout(_)) => Vec::new(),
        Err(e) => {
            fresh.close();
            return Err(e);
        }
    };
    fresh.close();
    let last = replies.last().cloned();
    let last_text = last
        .as_ref()
        .and_then(|r| r.get("text"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let last_has_widgets = last
        .as_ref()
        .and_then(|r| r.get("widgets"))
        .is_some_and(|w| w.as_array().is_some_and(|a| !a.is_empty()));
    let feed_kind = decided
        .as_ref()
        .map(|r| r.kind)
        .unwrap_or(WaitOutcomeKind::Timeout);
    let kind = match feed_kind {
        WaitOutcomeKind::Done => {
            let needs_input =
                last_has_widgets || last_text.as_deref().and_then(needs_input_reason).is_some();
            if needs_input {
                WaitOutcomeKind::NeedsInput
            } else {
                WaitOutcomeKind::Done
            }
        }
        other => other,
    };
    let question = if kind == WaitOutcomeKind::NeedsInput {
        last_text.as_deref().map(question_of)
    } else {
        None
    };
    // Python's `needs_input_reason`: "widget" when the last reply has
    // widgets, else the reason for its text. Only for NeedsInput.
    let needs_input_reason = if kind == WaitOutcomeKind::NeedsInput {
        if last_has_widgets {
            Some("widget".to_string())
        } else {
            last_text.as_deref().and_then(needs_input_reason)
        }
    } else {
        None
    };
    let widgets = last
        .as_ref()
        .and_then(|r| r.get("widgets"))
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    let approvals = if kind == WaitOutcomeKind::NeedsApproval {
        decided
            .as_ref()
            .map(|r| r.approvals.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(WaitResult {
        kind,
        exit_code: exit_code_for(kind),
        // Last task status the Waiter saw (Python's `w.status`); `None`
        // when no task status arrived at all, e.g. a pure timeout.
        status: waiter.status().map(|s| s.to_string()),
        after_seq,
        final_text: last_text,
        question,
        replies,
        widgets,
        needs_input_reason,
        auto_approved: waiter.auto_approved.clone(),
        auto_approve_errors: waiter.auto_approve_errors.clone(),
        approvals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn noop_approver() -> impl FnMut(&[ApprovalItem]) -> (Vec<Value>, Vec<Value>) {
        |_: &[ApprovalItem]| (Vec::new(), Vec::new())
    }

    fn task_event(status: &str, agent_id: &str, root_agent_id: &str) -> Value {
        json!({
            "event": "task.status",
            "payload": {
                "status": status,
                "agent_id": agent_id,
                "root_agent_id": root_agent_id,
                "session_id": null,
                "is_thread": false
            }
        })
    }

    #[test]
    fn replay_completed_before_running_is_ignored_with_require_running() {
        let mut waiter = Waiter::new(None, None, true);
        let mut approver = noop_approver();
        assert!(waiter
            .feed(&task_event("completed", "root", "root"), &mut approver)
            .is_none());
        assert!(waiter
            .feed(&task_event("running", "root", "root"), &mut approver)
            .is_none());
        let result = waiter
            .feed(&task_event("completed", "root", "root"), &mut approver)
            .unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::Done);
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn feed_result_carries_status_and_after_seq() {
        let mut waiter = Waiter::new(None, Some(42), false);
        let mut approver = noop_approver();
        assert!(waiter
            .feed(&task_event("running", "root", "root"), &mut approver)
            .is_none());
        let result = waiter
            .feed(&task_event("completed", "root", "root"), &mut approver)
            .unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::Done);
        assert_eq!(result.status.as_deref(), Some("completed"));
        assert_eq!(result.after_seq, 42);
        // feed alone has no history re-read, so these stay empty.
        assert!(result.replies.is_empty());
        assert!(result.widgets.is_empty());
        assert_eq!(result.needs_input_reason, None);
    }

    #[test]
    fn running_then_completed_is_done_and_subagent_completed_is_ignored() {
        let mut waiter = Waiter::new(None, None, false);
        let mut approver = noop_approver();
        assert!(waiter
            .feed(&task_event("completed", "sub", "root"), &mut approver)
            .is_none());
        assert!(waiter
            .feed(&task_event("running", "root", "root"), &mut approver)
            .is_none());
        let result = waiter
            .feed(&task_event("failed", "root", "root"), &mut approver)
            .unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::Failed);
        assert_eq!(result.exit_code, 2);
    }

    #[test]
    fn approval_is_checked_before_task_status_in_the_same_event() {
        let mut waiter = Waiter::new(None, None, false);
        let mut approver = noop_approver();
        let event = json!({
            "event": "approvals.snapshot",
            "payload": {
                "pending_approvals": [{
                    "approval_id": "ap1",
                    "display": {"summary_title": "Network access"},
                    "decision_options": [{"kind": "allow_once"}]
                }],
                "status": "completed"
            }
        });
        // The event name is approvals.*, so the approval branch wins even
        // though a task status could be read from the same payload shape.
        let result = waiter.feed(&event, &mut approver).unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::NeedsApproval);
        assert_eq!(result.exit_code, 4);
        assert_eq!(result.approvals.len(), 1);
    }

    #[test]
    fn handled_approval_does_not_stop_the_wait_twice() {
        let mut waiter = Waiter::new(None, None, false);
        let event = || {
            json!({
                "event": "approvals.snapshot",
                "payload": {
                    "pending_approvals": [{
                        "approval_id": "ap1",
                        "display": {"summary_title": "T"},
                        "decision_options": [{"kind": "allow_always"}]
                    }]
                }
            })
        };
        let mut approver = |items: &[ApprovalItem]| -> (Vec<Value>, Vec<Value>) {
            assert_eq!(items.len(), 1);
            (vec![json!({"approval_id": items[0].id})], Vec::new())
        };
        assert!(waiter.feed(&event(), &mut approver).is_none());
        // Second identical snapshot: nothing new is offered, and the
        // already-handled approval does not stop the wait.
        assert!(waiter.feed(&event(), &mut approver).is_none());
    }

    #[test]
    fn approval_without_forever_option_and_unhandled_approver_stops_wait() {
        let mut waiter = Waiter::new(None, None, false);
        let event = json!({
            "event": "approvals.snapshot",
            "payload": {
                "pending_approvals": [{
                    "approval_id": "ap9",
                    "display": {"summary_title": "T"},
                    "decision_options": [{"kind": "allow_once"}]
                }]
            }
        });
        let mut approver =
            |_: &[ApprovalItem]| -> (Vec<Value>, Vec<Value>) { (Vec::new(), Vec::new()) };
        let result = waiter.feed(&event, &mut approver).unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::NeedsApproval);
    }

    #[test]
    fn scope_filters_thread_events() {
        let mut waiter = Waiter::new(Some("thread-1".to_string()), None, false);
        let mut approver = noop_approver();
        let other_thread = json!({
            "event": "task.status",
            "payload": {"status": "completed", "session_id": "thread-2", "agent_id": "root", "root_agent_id": "root"}
        });
        assert!(waiter.feed(&other_thread, &mut approver).is_none());
        let same_thread = json!({
            "event": "task.status",
            "payload": {"status": "completed", "session_id": "thread-1", "agent_id": "root", "root_agent_id": "root"}
        });
        assert!(waiter.feed(&same_thread, &mut approver).is_some());
    }

    #[test]
    fn replies_after_filters_and_cleans() {
        let events = vec![
            json!({"seq": 5, "event_name": "message.assistant", "payload": {"display_text": "old [[hatch_widget:w1]] reply"}}),
            json!({"seq": 3, "event_name": "message.assistant", "payload": {"display_text": "earlier"}}),
            json!({"seq": 7, "event_name": "message.user", "payload": {"display_text": "user"}}),
            json!({"seq": 9, "event_name": "message.assistant", "payload": {"content": "newest", "widgets": [{"widget_id": "w2", "kind": "q", "display_text": "pick?", "state": {}}]}}),
        ];
        let replies = replies_after(&events, 5);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["seq"], 9);
        assert_eq!(replies[0]["text"], "newest");
        assert!(replies[0].get("widgets").is_some());
    }

    #[test]
    fn needs_input_reason_cases() {
        assert_eq!(needs_input_reason(""), None);
        assert_eq!(needs_input_reason("gate_questions: none"), None);
        assert_eq!(
            needs_input_reason("gate_questions: something"),
            Some("gate/policy question".to_string())
        );
        assert_eq!(
            needs_input_reason("stopped by a safety check here"),
            Some("stopped by a safety check".to_string())
        );
        assert_eq!(
            needs_input_reason("Boss có muốn tiếp tục không?"),
            Some("waiting phrase: 'boss có muốn'".to_string())
        );
        assert_eq!(
            needs_input_reason("All done.\nShould we proceed?"),
            Some("ends with a question".to_string())
        );
        assert_eq!(needs_input_reason("All done, no question."), None);
    }

    #[test]
    fn question_of_takes_last_two_question_sentences() {
        assert_eq!(
            question_of("First. Is this ok? Second. Or that? Done."),
            "Is this ok? Or that?"
        );
        assert_eq!(question_of("No question here."), "No question here.");
    }

    #[test]
    fn user_ack_finds_user_message_after_baseline() {
        let events = vec![
            json!({"seq": 2, "event_name": "message.user", "payload": {"display_text": "hello"}}),
            json!({"seq": 4, "event_name": "message.user", "payload": {"display_text": "hello world"}}),
        ];
        assert_eq!(user_ack(&events, 2, Some("hello world")), Some(4));
        assert_eq!(user_ack(&events, 5, Some("hello")), None);
    }

    #[test]
    fn describe_approval_maps_fields() {
        let approval = json!({
            "approval_id": "ap1",
            "display": {
                "summary_title": "Network",
                "permission_question": {"text": "Allow {assistant}?", "kind": "network_access", "scope": "example.com"},
                "detail_rows": [{"label": "Host", "value": "h.example"}],
                "short_explanation": "needed"
            },
            "decision_options": [{"kind": "allow_once"}, {"kind": "allow_always"}],
            "request_expires_at_ms": 123
        });
        let described = describe_approval(&approval);
        assert_eq!(described["question"], "Allow Muse?");
        assert_eq!(described["kind"], "network_access");
        assert_eq!(described["scope"], "example.com");
        assert!(offers_forever(&described));
        let item = approval_item(&described);
        assert!(item.has_forever_option);
    }
}
