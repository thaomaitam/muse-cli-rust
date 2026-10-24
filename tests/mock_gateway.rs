//! Mock-gateway integration tests.
//!
//! The fake responder in this file is written HERE, in `tests/`, from
//! `snow` (Noise XX responder) + `prost` (the envelope structs). It
//! does NOT use any `#[cfg(test)]` helper from `gateway.rs` (those are
//! private to that module anyway). The transport is an in-memory
//! channel pair injected via `Gateway::connect_with_transport`, so no
//! live muse.ai call happens and no real cookie is used.
//!
//! Wire shape the responder implements (gateway spec §(b)):
//! client -> NoiseTransportFrame{ServiceRequest{ServiceFrame{Request}}}
//! server -> NoiseTransportFrame{ServiceResponse{ServiceFrame{Response}}}
//! The responder echoes the request's `stream_id` back, as a real
//! server would, and can first emit a decoy frame for another stream
//! id to prove unary `request` discards foreign-stream frames.

use muse_cli::config::Config;
use muse_cli::errors::CliError;
use muse_cli::gateway::{Gateway, Transport};
use muse_cli::proto::{
    ApplicationResponse, BodyChunk, Header, NoiseTransportFrame, ServiceFrame, ServiceFrameKind,
    ServiceRequest, ServiceResponse,
};
use muse_cli::wait::{WaitOutcomeKind, Waiter};
use prost::Message as _;
use serde_json::json;
use snow::{HandshakeState, TransportState};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

// ---------------------------------------------------------------------------
// In-memory transport (client side of the channel pair)
// ---------------------------------------------------------------------------

struct ChanTransport {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
}

impl Transport for ChanTransport {
    fn send_bytes(&mut self, data: &[u8]) -> Result<(), CliError> {
        self.tx
            .send(data.to_vec())
            .map_err(|_| CliError::io("mock channel closed".to_string()))
    }

    fn recv_bytes(&mut self) -> Result<Vec<u8>, CliError> {
        self.rx
            .recv()
            .map_err(|_| CliError::io("mock channel closed".to_string()))
    }
}

fn test_config() -> Config {
    Config {
        config_dir: PathBuf::from("/nonexistent-mock-config"),
        cookies_file: PathBuf::from("/nonexistent/cookies.txt"),
        vm_id: Some("vm-FAKE".to_string()),
        node_id: "0123456789abcdef".to_string(),
        auto_approve: None,
        subscription_action_id: None,
    }
}

// ---------------------------------------------------------------------------
// Fake responder (snow responder + prost envelopes, written here)
// ---------------------------------------------------------------------------

/// One scripted unary/subscription response, in request order.
struct ScriptedResponse {
    status: i32,
    body: Vec<u8>,
    end_body: bool,
}

fn encode_response(
    ts: &mut TransportState,
    stream_id: i64,
    status: i32,
    body: &[u8],
    end_body: bool,
) -> Vec<u8> {
    let frame = ServiceFrame {
        stream_id: Some(stream_id),
        kind: Some(ServiceFrameKind::Response(ApplicationResponse {
            status: Some(status),
            headers: vec![Header {
                key: Some("content-type".to_string()),
                value: Some("application/json".to_string()),
            }],
            body: Some(body.to_vec()),
            end_body: Some(end_body),
        })),
    };
    let service_response = ServiceResponse {
        payload: Some(frame.encode_to_vec()),
    };
    let transport_frame = NoiseTransportFrame {
        chunk_id: Some(11),
        chunk_index: Some(0),
        total_chunks: Some(1),
        payload: Some(service_response.encode_to_vec()),
    };
    let plaintext = transport_frame.encode_to_vec();
    let mut buf = vec![0u8; plaintext.len() + 32];
    let len = ts.write_message(&plaintext, &mut buf).unwrap();
    buf.truncate(len);
    buf
}

fn decode_request(ts: &mut TransportState, ciphertext: &[u8]) -> (i64, String, String) {
    let mut buf = vec![0u8; ciphertext.len()];
    let len = ts.read_message(ciphertext, &mut buf).unwrap();
    buf.truncate(len);
    let transport_frame = NoiseTransportFrame::decode(buf.as_slice()).unwrap();
    let service_request =
        ServiceRequest::decode(transport_frame.payload.unwrap_or_default().as_slice()).unwrap();
    let frame =
        ServiceFrame::decode(service_request.payload.unwrap_or_default().as_slice()).unwrap();
    let stream_id = frame.stream_id.unwrap_or(0);
    match frame.kind {
        Some(ServiceFrameKind::Request(req)) => (
            stream_id,
            req.verb.unwrap_or_default(),
            req.path.unwrap_or_default(),
        ),
        other => panic!("expected an ApplicationRequest, got {other:?}"),
    }
}

fn noise_responder() -> HandshakeState {
    let params: snow::params::NoiseParams = "Noise_XX_25519_AESGCM_SHA256".parse().unwrap();
    snow::Builder::new(params)
        .local_private_key(&[0x42u8; 32])
        .build_responder()
        .unwrap()
}

/// Spawn the responder thread. It performs the 3-message Noise XX
/// handshake (empty payloads), then answers requests in script order.
/// With `decoy_first`, the first answer is preceded by a response frame
/// for stream 999, which the client must discard. Captured
/// (verb, path) pairs are recorded when `capture` is given. After the
/// script is exhausted the thread exits (dropping its channel ends).
#[allow(clippy::type_complexity)]
fn spawn_fake_gateway(
    script: Vec<ScriptedResponse>,
    decoy_first: bool,
    capture: Option<Arc<Mutex<Vec<(String, String)>>>>,
) -> (ChanTransport, JoinHandle<()>) {
    let (client_tx, responder_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (responder_tx, client_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let handle = std::thread::spawn(move || {
        let mut hs = noise_responder();
        // Message 1: initiator -> responder.
        let msg1 = responder_rx.recv().unwrap();
        let mut buf = vec![0u8; 65535];
        hs.read_message(&msg1, &mut buf).unwrap();
        // Message 2: responder -> initiator.
        let mut buf2 = vec![0u8; 65535];
        let len = hs.write_message(b"", &mut buf2).unwrap();
        responder_tx.send(buf2[..len].to_vec()).unwrap();
        // Message 3: initiator -> responder; handshake completes here.
        let msg3 = responder_rx.recv().unwrap();
        let mut buf = vec![0u8; 65535];
        hs.read_message(&msg3, &mut buf).unwrap();
        assert!(hs.is_handshake_finished());
        let mut ts = hs.into_transport_mode().unwrap();

        let mut index = 0usize;
        let mut first = true;
        while index < script.len() {
            let Ok(ciphertext) = responder_rx.recv() else {
                break;
            };
            let (stream_id, verb, path) = decode_request(&mut ts, &ciphertext);
            if let Some(capture) = &capture {
                capture.lock().unwrap().push((verb, path));
            }
            if decoy_first && first {
                // Foreign-stream frame: the client must discard it and
                // still complete the real request.
                let decoy = encode_response(&mut ts, 999, 200, b"ignored", true);
                responder_tx.send(decoy).unwrap();
            }
            first = false;
            let answer = &script[index];
            index += 1;
            let bytes = encode_response(
                &mut ts,
                stream_id,
                answer.status,
                &answer.body,
                answer.end_body,
            );
            if responder_tx.send(bytes).is_err() {
                break;
            }
        }
        // Script exhausted: stay alive (blocked on recv) instead of
        // dropping the channel. If this thread dropped its sender, the
        // gateway's subscription reader would read a channel-closed
        // error and turn a successful subscribe into an Err. The
        // thread exits when the client side drops (end of test), or
        // when the test process exits.
        while responder_rx.recv().is_ok() {}
    });
    (
        ChanTransport {
            tx: client_tx,
            rx: client_rx,
        },
        handle,
    )
}

fn connect(script: Vec<ScriptedResponse>, decoy_first: bool) -> (Gateway, JoinHandle<()>) {
    let (transport, handle) = spawn_fake_gateway(script, decoy_first, None);
    let gateway = Gateway::connect_with_transport(&test_config(), Box::new(transport)).unwrap();
    (gateway, handle)
}

// ---------------------------------------------------------------------------
// Unary call_json: success (+ decoy discard + stream counter), status error
// ---------------------------------------------------------------------------

#[test]
fn unary_call_json_success_then_status_error() {
    let script = vec![
        ScriptedResponse {
            status: 200,
            body: br#"{"result": {"hello": "world"}}"#.to_vec(),
            end_body: true,
        },
        ScriptedResponse {
            status: 500,
            body: b"boom".to_vec(),
            end_body: true,
        },
    ];
    let (mut gateway, _handle) = connect(script, true);
    // Stream ids start at 1 and increase monotonically.
    assert_eq!(gateway.stream_counter(), 1);
    let value = gateway
        .call_json(
            "chat.history",
            &[],
            Some(&json!({"limit": 1})),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(value, json!({"hello": "world"}));
    assert_eq!(gateway.stream_counter(), 2);
    // Second call on the same connection: a non-2xx status is a
    // Gateway error carrying the status and the payload head.
    let err = gateway
        .call_json(
            "chat.history",
            &[],
            Some(&json!({"limit": 1})),
            None,
            Duration::from_secs(10),
        )
        .unwrap_err();
    match err {
        CliError::Gateway { status, payload } => {
            assert_eq!(status, 500);
            assert!(payload.contains("boom"), "payload: {payload}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert_eq!(gateway.stream_counter(), 3);
}

#[test]
fn call_json_variants_empty_non_json_ok_false_result_and_whole_dict() {
    let script = vec![
        ScriptedResponse {
            status: 200,
            body: vec![],
            end_body: true,
        },
        ScriptedResponse {
            status: 200,
            body: b"not-json{".to_vec(),
            end_body: true,
        },
        ScriptedResponse {
            status: 200,
            body: br#"{"ok": false, "error": "denied"}"#.to_vec(),
            end_body: true,
        },
        ScriptedResponse {
            status: 200,
            body: br#"{"result": 42}"#.to_vec(),
            end_body: true,
        },
        ScriptedResponse {
            status: 200,
            body: br#"{"hello": "world"}"#.to_vec(),
            end_body: true,
        },
    ];
    let (mut gateway, _handle) = connect(script, false);
    // Empty payload -> {}.
    assert_eq!(
        gateway
            .call_json("chat.history", &[], None, None, Duration::from_secs(10))
            .unwrap(),
        json!({})
    );
    // Invalid JSON -> Gateway(-1, "non-JSON response: …").
    let err = gateway
        .call_json("chat.history", &[], None, None, Duration::from_secs(10))
        .unwrap_err();
    match err {
        CliError::Gateway { status, payload } => {
            assert_eq!(status, -1);
            assert!(payload.contains("non-JSON response"), "payload: {payload}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // Dict with ok == false -> Gateway(-1, "api error: …").
    let err = gateway
        .call_json("chat.history", &[], None, None, Duration::from_secs(10))
        .unwrap_err();
    match err {
        CliError::Gateway { status, payload } => {
            assert_eq!(status, -1);
            assert_eq!(payload, "api error: denied");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // Dict with "result" -> that value.
    assert_eq!(
        gateway
            .call_json("chat.history", &[], None, None, Duration::from_secs(10))
            .unwrap(),
        json!(42)
    );
    // Dict without "result" -> the whole dict.
    assert_eq!(
        gateway
            .call_json("chat.history", &[], None, None, Duration::from_secs(10))
            .unwrap(),
        json!({"hello": "world"})
    );
}

#[test]
fn download_bytes_uses_the_synthetic_media_raw_route() {
    let capture: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let script = vec![ScriptedResponse {
        status: 200,
        body: b"FAKE-BYTES".to_vec(),
        end_body: true,
    }];
    let (transport, _handle) = spawn_fake_gateway(script, false, Some(Arc::clone(&capture)));
    let mut gateway = Gateway::connect_with_transport(&test_config(), Box::new(transport)).unwrap();
    let bytes = gateway
        .download_bytes(&json!("sandbox://workspace/a.png"), Duration::from_secs(10))
        .unwrap();
    assert_eq!(bytes, b"FAKE-BYTES");
    let captured = capture.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "GET");
    assert_eq!(captured[0].1, "/media/raw/workspace/a.png");
}

// ---------------------------------------------------------------------------
// Subscribe: running -> completed, fed through Waiter
// ---------------------------------------------------------------------------

#[test]
fn subscribe_records_running_then_completed_feeds_waiter_to_done() {
    let running = json!({
        "event": "task.status",
        "payload": {"status": "running", "agent_id": "root", "root_agent_id": "root"}
    });
    let completed = json!({
        "event": "task.status",
        "payload": {"status": "completed", "agent_id": "root", "root_agent_id": "root"}
    });
    let body = format!("{}\n{}\n", running, completed).into_bytes();
    let script = vec![ScriptedResponse {
        status: 200,
        body,
        end_body: false,
    }];
    let (mut gateway, _handle) = connect(script, false);
    let mut seen: Vec<String> = Vec::new();
    let records = gateway
        .subscribe_records(
            "chat.subscribe",
            &[],
            Some(&json!({"capabilities": ["chat_cancel", "delta_stream"]})),
            None,
            Duration::from_secs(2),
            Duration::from_secs(20),
            100,
            &mut |record: &str| {
                seen.push(record.to_string());
                false
            },
        )
        .unwrap();
    assert_eq!(records.len(), 2, "records: {records:?}");
    assert_eq!(records, seen);
    // The same records, fed to the pure Waiter, decide the wait:
    // running keeps waiting, completed ends it with Done / exit 0.
    let mut waiter = Waiter::new(None, None, false);
    let mut noop = |_: &[muse_cli::wait::ApprovalItem]| (Vec::new(), Vec::new());
    let first: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
    assert!(waiter.feed(&first, &mut noop).is_none());
    let second: serde_json::Value = serde_json::from_str(&records[1]).unwrap();
    let result = waiter.feed(&second, &mut noop).unwrap();
    assert_eq!(result.kind, WaitOutcomeKind::Done);
    assert_eq!(result.exit_code, 0);
}

#[test]
fn subscribe_records_on_record_halt_stops_early() {
    let lines = ["{\"n\": 1}", "{\"n\": 2}", "{\"n\": 3}"].join("\n") + "\n";
    let script = vec![ScriptedResponse {
        status: 200,
        body: lines.into_bytes(),
        end_body: false,
    }];
    let (mut gateway, _handle) = connect(script, false);
    let records = gateway
        .subscribe_records(
            "chat.subscribe",
            &[],
            None,
            None,
            Duration::from_secs(2),
            Duration::from_secs(20),
            100,
            &mut |record: &str| record.contains("\"n\": 2"),
        )
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(records[1].contains("\"n\": 2"));
}

// Keep BodyChunk imported (the responder could split bodies into chunks;
// the single-frame path above is what the unary tests exercise, and
// multi-chunk reassembly is covered by join_chunks in library_pure).
#[allow(dead_code)]
fn _uses_body_chunk(chunk: BodyChunk) -> Option<Vec<u8>> {
    chunk.data
}
