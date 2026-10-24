//! Pure `Waiter` integration tests (no network).
//!
//! These exercise the public `wait.rs` state machine through its real
//! event shapes: replay-before-running, subagent filtering, approval
//! before task status, handled-approval-once, and the
//! replies/needs-input helpers. `run_wait` itself is NOT tested here:
//! it needs two live gateway connections (subscribe + a fresh history
//! connection via a factory), which is mock-gateway follow-up work for
//! the coordinator — the subscribe half is covered in mock_gateway.rs
//! and the feed half is covered here.

use muse_cli::wait::{
    clean_text, describe_approval, needs_input_reason, open_widgets, question_of, replies_after,
    user_ack, ApprovalItem, WaitOutcomeKind, Waiter,
};
use serde_json::json;

fn noop_approver() -> impl FnMut(&[ApprovalItem]) -> (Vec<serde_json::Value>, Vec<serde_json::Value>)
{
    |_: &[ApprovalItem]| (Vec::new(), Vec::new())
}

fn task_event(status: &str, agent_id: &str, root_agent_id: &str) -> serde_json::Value {
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

fn approval_event(id: &str, options: &[&str]) -> serde_json::Value {
    json!({
        "event": "approvals.snapshot",
        "payload": {
            "pending_approvals": [{
                "approval_id": id,
                "display": {"summary_title": "Network access"},
                "decision_options": options.iter().map(|o| json!({"kind": o})).collect::<Vec<_>>(),
            }]
        }
    })
}

#[test]
fn completed_before_running_is_ignored_with_require_running() {
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
    assert!(result.approvals.is_empty());
}

#[test]
fn running_then_completed_is_done_and_failed_is_exit_2() {
    let mut waiter = Waiter::new(None, None, false);
    let mut approver = noop_approver();
    assert!(waiter
        .feed(&task_event("running", "root", "root"), &mut approver)
        .is_none());
    let result = waiter
        .feed(&task_event("completed", "root", "root"), &mut approver)
        .unwrap();
    assert_eq!(result.kind, WaitOutcomeKind::Done);
    assert_eq!(result.exit_code, 0);

    let mut waiter = Waiter::new(None, None, false);
    let result = waiter
        .feed(&task_event("failed", "root", "root"), &mut approver)
        .unwrap();
    assert_eq!(result.kind, WaitOutcomeKind::Failed);
    assert_eq!(result.exit_code, 2);
}

#[test]
fn subagent_completed_is_ignored() {
    let mut waiter = Waiter::new(None, None, false);
    let mut approver = noop_approver();
    // agent_id != root_agent_id: only the root agent ends a turn.
    assert!(waiter
        .feed(&task_event("completed", "sub", "root"), &mut approver)
        .is_none());
    assert!(waiter
        .feed(&task_event("running", "root", "root"), &mut approver)
        .is_none());
    assert!(waiter
        .feed(&task_event("completed", "root", "root"), &mut approver)
        .is_some());
}

#[test]
fn approval_is_checked_before_task_status_in_the_same_event() {
    let mut waiter = Waiter::new(None, None, false);
    let mut approver = noop_approver();
    // An approvals event that also carries a terminal-looking payload
    // must stop with NeedsApproval (exit 4), not Done.
    let mut event = approval_event("ap1", &["allow_once"]);
    event["payload"]["status"] = json!("completed");
    let result = waiter.feed(&event, &mut approver).unwrap();
    assert_eq!(result.kind, WaitOutcomeKind::NeedsApproval);
    assert_eq!(result.exit_code, 4);
    assert_eq!(result.approvals.len(), 1);
    assert_eq!(result.approvals[0]["approval_id"], "ap1");
}

#[test]
fn handled_approval_does_not_stop_the_wait_twice() {
    let mut waiter = Waiter::new(None, None, false);
    let event = approval_event("ap1", &["allow_always"]);
    let mut approver =
        |items: &[ApprovalItem]| -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
            assert_eq!(items.len(), 1);
            (vec![json!({"approval_id": items[0].id})], Vec::new())
        };
    assert!(waiter.feed(&event, &mut approver).is_none());
    // Second identical snapshot: nothing new is offered, and the
    // already-handled approval does not stop the wait.
    assert!(waiter.feed(&event, &mut approver).is_none());
    assert_eq!(waiter.auto_approved().len(), 1);
}

#[test]
fn approval_without_forever_option_and_unhandled_approver_stops_wait() {
    let mut waiter = Waiter::new(None, None, false);
    let mut approver = noop_approver();
    let event = approval_event("ap9", &["allow_once"]);
    let result = waiter.feed(&event, &mut approver).unwrap();
    assert_eq!(result.kind, WaitOutcomeKind::NeedsApproval);
    assert_eq!(result.exit_code, 4);
    // The offered item reports has_forever_option == false.
    let mut seen: Vec<bool> = Vec::new();
    let mut waiter2 = Waiter::new(None, None, false);
    let mut spy = |items: &[ApprovalItem]| -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        seen.extend(items.iter().map(|i| i.has_forever_option));
        (Vec::new(), Vec::new())
    };
    let _ = waiter2.feed(&event, &mut spy);
    assert_eq!(seen, vec![false]);
}

#[test]
fn thread_scope_filters_other_thread_events() {
    let mut waiter = Waiter::new(Some("thread-1".to_string()), None, false);
    let mut approver = noop_approver();
    let other = json!({
        "event": "task.status",
        "payload": {"status": "completed", "session_id": "thread-2", "agent_id": "root", "root_agent_id": "root"}
    });
    assert!(waiter.feed(&other, &mut approver).is_none());
    let same = json!({
        "event": "task.status",
        "payload": {"status": "completed", "session_id": "thread-1", "agent_id": "root", "root_agent_id": "root"}
    });
    assert!(waiter.feed(&same, &mut approver).is_some());
}

#[test]
fn replies_after_needs_input_question_and_user_ack() {
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

    assert_eq!(clean_text("a [[hatch_widget:w1]] b"), "a  b");
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
    assert_eq!(
        question_of("First. Is this ok? Second. Or that? Done."),
        "Is this ok? Or that?"
    );

    let user_events = vec![
        json!({"seq": 2, "event_name": "message.user", "payload": {"display_text": "hello"}}),
        json!({"seq": 4, "event_name": "message.user", "payload": {"display_text": "hello world"}}),
    ];
    assert_eq!(user_ack(&user_events, 2, Some("hello world")), Some(4));
    assert_eq!(user_ack(&user_events, 5, Some("hello")), None);
}

#[test]
fn describe_approval_and_open_widgets() {
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

    let unanswered = json!({
        "payload": {"widgets": [{"widget_id": "w1", "kind": "q", "display_text": "pick?", "state": {}}]}
    });
    assert_eq!(open_widgets(&unanswered).len(), 1);
    let answered = json!({
        "payload": {"widgets": [{"widget_id": "w1", "kind": "q", "display_text": "pick?", "state": {"value": "yes"}}]}
    });
    assert!(open_widgets(&answered).is_empty());
}
