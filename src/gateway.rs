//! Gateway client (writer B): HTTPS auth + WebSocket/Noise transport +
//! protobuf envelopes + unary/subscription calls.
//!
//! Faithful to Python `gateway.py` unless noted in `DEVIATIONS` comments
//! below. In particular:
//! - Lock discipline: ONE lock guards the Noise state AND the transport
//!   (send and receive both go through it), because Noise decrypt has
//!   state and two threads decrypting concurrently corrupt it. The
//!   subscription reader thread holds that lock only for one `_read_frame`
//!   at a time; the main thread never holds it while waiting on the
//!   record channel, so there is no deadlock.
//! - Unary `request` DISCARDS frames of other streams (it does not buffer
//!   them for another consumer). That is why auto-approval and the
//!   history re-read in `wait.rs` use a separate connection.
//! - `connect` order follows the Python code, not the shorthand order in
//!   CONTRACT.md: when `cfg.vm_id` is absent, `fetch_session_info` runs
//!   FIRST (it needs only cookies), then `fetch_access_token`, then
//!   `fetch_hatch_token`. When `cfg.vm_id` is present, the session call
//!   is skipped entirely.
//! - `connect_with_transport` SKIPS the HTTPS auth calls and goes straight
//!   to the Noise handshake over the injected transport. This is the only
//!   reading under which its documented purpose ("tests use an in-memory
//!   pair") is possible without live muse.ai calls; see its doc comment.
//! - TLS verification is ON for the WebSocket (tungstenite/rustls verifies
//!   by default; there is no verify-off option in this code path).
//! - Chrome TLS fingerprint impersonation (`curl_cffi impersonate="chrome"`
//!   in Python) is NOT reproduced by reqwest/tungstenite. Live parity is
//!   therefore unproven, not claimed (see the gateway spec §5 risk 1).

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use prost::Message as _;
use rand::{rngs::OsRng, RngCore};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

use crate::config::Config;
use crate::errors::CliError;
use crate::noise::NoiseSession;
use crate::proto::{
    ApplicationRequest, Header, NoiseTransportFrame, ServiceFrame, ServiceFrameKind,
    ServiceRequest, ServiceResponse,
};
use crate::routes::{lookup, Route};

/// Re-exported from `media.rs` (writer A owns the logic). `gateway.rs`
/// must not duplicate it. NOTE: at the time writer B wrote this file,
/// `src/media.rs` did not exist yet in the crate, so a whole-crate
/// `cargo check` fails until writer A lands it; the /tmp self-check in
/// writer B's report uses a stub `media.rs` that implements exactly the
/// CONTRACT signature, and every error in that check belonging to this
/// file was fixed.
pub use crate::media::media_path;

pub const GATEWAY_HOST: &str = "hatch.metaaivm.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36";
pub const SUBSCRIPTION_ACTION_NAME: &str = "fetchSubscriptionAction";
pub const SUBSCRIPTION_ACTION_ID: &str = "4029ac5c4bd03136487d91ebde5de39c64b35229b4";

/// Transport abstraction: one side of a byte stream (WebSocket in
/// production, an in-memory pair in tests).
pub trait Transport: Send {
    fn send_bytes(&mut self, data: &[u8]) -> Result<(), CliError>;
    fn recv_bytes(&mut self) -> Result<Vec<u8>, CliError>;
    /// Best-effort close. The default is a no-op; the WebSocket transport
    /// sends a Close frame. A subscription reader blocked in `recv_bytes`
    /// is not woken by this (same daemon-thread caveat as Python).
    fn close(&mut self) {}
}

/// Egress-proxy support for the WebSocket transport.
///
/// Why this exists: the Python original connects its WebSocket through
/// curl_cffi, which honours the process proxy environment, while
/// tungstenite connects directly. On a host whose only outbound route is
/// an HTTP egress proxy (the case on this VM: `https_proxy` pointing at
/// a local proxy, direct TCP to port 443 answered by a middlebox), the
/// direct tungstenite handshake fails with `InvalidContentType` because
/// the response it parses is not a WebSocket upgrade at all. Routing
/// through the proxy with an HTTP CONNECT tunnel, then letting
/// tungstenite do its normal TLS + WebSocket handshake over the tunnel,
/// reproduces the Python behaviour. On a host with no proxy configured
/// this code is inert: `connect` takes the direct path unchanged.
///
/// Security: proxy credentials, when present in the proxy URL userinfo,
/// are used only for the CONNECT request's `Proxy-Authorization` header
/// and are never logged, never put in an error message, and never sent
/// to the gateway host itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyConfig {
    host: String,
    port: u16,
    auth_header: Option<String>,
}

fn no_proxy_matches(host: &str, no_proxy: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    for raw in no_proxy.split(',') {
        let entry = raw.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return true;
        }
        // Strip an optional :port suffix (host itself has no port here).
        let entry = entry.split(':').next().unwrap_or(&entry);
        if let Some(suffix) = entry.strip_prefix('.') {
            if host == suffix || host.ends_with(&format!(".{suffix}")) {
                return true;
            }
        } else if host == entry || host.ends_with(&format!(".{entry}")) {
            return true;
        }
    }
    false
}

fn proxy_for_host(host: &str) -> Option<ProxyConfig> {
    for var in ["NO_PROXY", "no_proxy"] {
        if let Ok(value) = std::env::var(var) {
            if no_proxy_matches(host, &value) {
                return None;
            }
        }
    }
    for var in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        let Ok(raw) = std::env::var(var) else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(parsed) = url::Url::parse(raw) else { continue };
        // Only plain-HTTP proxies are supported: the tunnel itself is
        // plain TCP to the proxy, TLS happens inside it. An https://
        // proxy URL would need a second TLS layer and is out of scope.
        if parsed.scheme() != "http" {
            continue;
        }
        let Some(proxy_host) = parsed.host_str() else { continue };
        let port = parsed.port_or_known_default().unwrap_or(80);
        let auth_header = if parsed.username().is_empty() {
            None
        } else {
            use base64::Engine as _;
            let creds = format!("{}:{}", parsed.username(), parsed.password().unwrap_or(""));
            Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(creds.as_bytes())
            ))
        };
        return Some(ProxyConfig {
            host: proxy_host.to_string(),
            port,
            auth_header,
        });
    }
    None
}

fn build_connect_request(host: &str, port: u16, auth_header: Option<&str>) -> String {
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(auth) = auth_header {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    req
}

fn read_connect_response(stream: &mut TcpStream) -> Result<(), CliError> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| CliError::io(format!("proxy CONNECT read failed: {e}")))?;
        if n == 0 {
            return Err(CliError::io("proxy CONNECT closed".to_string()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(CliError::io("proxy CONNECT response too large".to_string()));
        }
    }
    let first_line = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if first_line.contains(" 200 ") || first_line.ends_with(" 200") {
        Ok(())
    } else {
        // Only the status line is reported; the body may be a block page
        // and is never needed for diagnosis here.
        Err(CliError::io(format!("proxy CONNECT failed: {first_line}")))
    }
}

/// WebSocket transport over TLS (tungstenite + rustls, verify ON).
struct WsTransport {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WsTransport {
    fn connect(url: &str) -> Result<Self, CliError> {
        // DEVIATION (minor): Python passes timeout=20 to the WS connect;
        // tungstenite's `connect` takes no timeout, so no connect timeout
        // is enforced here. The handshake and later reads are blocking,
        // exactly like Python's.
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));
        if let Some(host) = host {
            if let Some(proxy) = proxy_for_host(&host) {
                return Self::connect_via_proxy(url, &proxy);
            }
        }
        let (ws, _response) = tungstenite::connect(url)
            .map_err(|e| CliError::io(format!("websocket connect failed: {e}")))?;
        Ok(Self { ws })
    }

    fn connect_via_proxy(url: &str, proxy: &ProxyConfig) -> Result<Self, CliError> {
        use std::io::Write as _;
        let target = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .ok_or_else(|| CliError::value("invalid websocket url".to_string()))?;
        let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .map_err(|e| CliError::io(format!("proxy connect failed: {e}")))?;
        let req = build_connect_request(&target, 443, proxy.auth_header.as_deref());
        stream
            .write_all(req.as_bytes())
            .map_err(|e| CliError::io(format!("proxy CONNECT write failed: {e}")))?;
        read_connect_response(&mut stream)?;
        // Let tungstenite do TLS + the WebSocket handshake over the
        // tunnel, with the same Chrome UA / Origin the Python client
        // presents. NOTE: `client_tls` with a hand-built request does
        // NOT fill in the WebSocket handshake headers (unlike the
        // high-level `connect`, which builds the request itself), and
        // its handshake code fails if `Sec-WebSocket-Key` (or Host /
        // Connection / Upgrade / Sec-WebSocket-Version) is missing.
        // So all five required headers are set here explicitly; the
        // key is 16 fresh random bytes, base64-encoded, per RFC 6455.
        // The request URI (which carries the auth token as a query
        // parameter) is never put in an error message by us.
        let mut key_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut key_bytes);
        let ws_key = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(key_bytes)
        };
        let request = tungstenite::http::Request::builder()
            .uri(url)
            .header("Host", target.as_str())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", ws_key)
            .header("User-Agent", USER_AGENT)
            .header("Origin", "https://muse.ai")
            .body(())
            .map_err(|_| CliError::value("invalid websocket request".to_string()))?;
        let (ws, _response) = tungstenite::client_tls(request, stream)
            .map_err(|e| CliError::io(format!("websocket connect failed (proxy): {e}")))?;
        Ok(Self { ws })
    }
}

impl Transport for WsTransport {
    fn send_bytes(&mut self, data: &[u8]) -> Result<(), CliError> {
        self.ws
            .send(Message::Binary(data.to_vec().into()))
            .map_err(|e| CliError::io(format!("websocket send failed: {e}")))?;
        Ok(())
    }

    fn recv_bytes(&mut self) -> Result<Vec<u8>, CliError> {
        match self.ws.read() {
            Ok(Message::Binary(bytes)) => Ok(bytes.to_vec()),
            Ok(Message::Close(_)) => Err(CliError::io("websocket closed".to_string())),
            Ok(_) => Err(CliError::io("unexpected websocket frame".to_string())),
            Err(e) => Err(CliError::io(format!("websocket recv failed: {e}"))),
        }
    }

    fn close(&mut self) {
        let _ = self.ws.close(None);
    }
}

struct GatewayInner {
    transport: Box<dyn Transport>,
    noise: NoiseSession,
    partial: HashMap<i64, HashMap<u32, Vec<u8>>>,
}

/// One authenticated connection to the personal VM gateway.
pub struct Gateway {
    inner: Arc<Mutex<GatewayInner>>,
    stream: Mutex<i64>,
    vm_id: String,
}

fn lock_inner(inner: &Arc<Mutex<GatewayInner>>) -> Result<MutexGuard<'_, GatewayInner>, CliError> {
    inner
        .lock()
        .map_err(|_| CliError::io("gateway lock poisoned".to_string()))
}

fn lock_stream(stream: &Mutex<i64>) -> Result<MutexGuard<'_, i64>, CliError> {
    stream
        .lock()
        .map_err(|_| CliError::io("gateway stream lock poisoned".to_string()))
}

fn service_id(service: Option<&str>) -> i32 {
    match service {
        Some("daemon") => 0,
        Some("sentinel") => 1,
        Some("vault") => 2,
        Some("authd") => 3,
        _ => 0,
    }
}

fn random_chunk_id() -> i64 {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    i64::from_le_bytes(bytes)
}

fn io_error(message: impl Into<String>) -> CliError {
    CliError::io(message.into())
}

// ---------------------------------------------------------------------------
// HTTPS helpers (muse.ai, NOT through the Noise tunnel)
// ---------------------------------------------------------------------------

/// Header list for muse.ai HTTPS calls, in Python `_hatch_headers` order.
/// `Authorization: Bearer …` is present only when an access token is given.
pub fn hatch_headers(cookies: &str, access_token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("Referer".to_string(), "https://muse.ai/".to_string()),
        ("Origin".to_string(), "https://muse.ai".to_string()),
        ("Sec-Fetch-Site".to_string(), "same-origin".to_string()),
        ("Sec-Fetch-Mode".to_string(), "cors".to_string()),
        ("Sec-Fetch-Dest".to_string(), "empty".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Cookie".to_string(), cookies.to_string()),
    ];
    if let Some(token) = access_token {
        if !token.is_empty() {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }
    headers
}

struct HttpResponse {
    status: u16,
    body: String,
    content_type: String,
}

fn http_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    timeout_secs: u64,
) -> Result<HttpResponse, CliError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| CliError::value(format!("invalid header name: {key}")))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| CliError::value(format!("invalid header value for {key}")))?;
        map.insert(name, header_value);
    }
    let reqwest_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| CliError::value(format!("invalid HTTP method: {method}")))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| io_error(format!("http client failed: {e}")))?;
    let mut request = client.request(reqwest_method, url).headers(map);
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request
        .send()
        .map_err(|e| io_error(format!("http request failed: {e}")))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .bytes()
        .map_err(|e| io_error(format!("http read failed: {e}")))?;
    Ok(HttpResponse {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        content_type,
    })
}

fn first_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

fn parse_json_body(text: &str) -> Result<Value, CliError> {
    serde_json::from_str(text)
        .map_err(|_| CliError::io("invalid JSON in HTTP response".to_string()))
}

/// POST https://muse.ai/api/auth/check (cookies, empty body) -> access_token.
/// Used by: `wake` (and by `Gateway::connect`).
pub fn fetch_access_token(cookies: &str) -> Result<String, CliError> {
    let response = http_request(
        "POST",
        "https://muse.ai/api/auth/check",
        &hatch_headers(cookies, None),
        Some(Vec::new()),
        20,
    )?;
    if response.status != 200 {
        return Err(CliError::auth(format!(
            "auth/check -> {} {} (cookies expired? re-export)",
            response.status,
            first_chars(&response.body, 120)
        )));
    }
    match parse_json_body(&response.body) {
        Ok(Value::Object(map)) => match map.get("access_token").and_then(|v| v.as_str()) {
            Some(token) => Ok(token.to_string()),
            None => Err(CliError::auth(
                "auth/check returned JSON without access_token".to_string(),
            )),
        },
        _ => Err(CliError::auth("auth/check returned non-JSON".to_string())),
    }
}

/// POST https://muse.ai/api/hatch/token -> hatch token.
/// Used by: `Gateway::connect` only (no CLI command calls it directly).
pub fn fetch_hatch_token(
    cookies: &str,
    access_token: &str,
    vm_id: &str,
) -> Result<String, CliError> {
    let body = serde_json::json!({
        "vmAddress": format!("wss://{vm_id}.metaaivm.com/"),
        "vmName": vm_id,
    });
    let response = http_request(
        "POST",
        "https://muse.ai/api/hatch/token",
        &hatch_headers(cookies, Some(access_token)),
        Some(serde_json::to_vec(&body).map_err(|e| io_error(format!("json error: {e}")))?),
        20,
    )?;
    if response.status != 200 {
        return Err(CliError::auth(format!(
            "hatch/token -> {} {}",
            response.status,
            first_chars(&response.body, 160)
        )));
    }
    match parse_json_body(&response.body) {
        Ok(Value::Object(map)) => match map.get("token").and_then(|v| v.as_str()) {
            Some(token) => Ok(token.to_string()),
            None => Err(CliError::auth(
                "hatch/token returned JSON without token".to_string(),
            )),
        },
        _ => Err(CliError::auth("hatch/token returned non-JSON".to_string())),
    }
}

/// GET https://muse.ai/api/session -> session JSON (contains `vm_id`).
/// A 200 response WITHOUT `vm_id` means the VM is unavailable: that is a
/// `Gateway(-1)` error, NOT an auth error (Python distinguishes them).
/// Used by: `wake` (when no vm_id is configured) and `Gateway::connect`.
pub fn fetch_session_info(cookies: &str) -> Result<Value, CliError> {
    let response = http_request(
        "GET",
        "https://muse.ai/api/session",
        &hatch_headers(cookies, None),
        None,
        20,
    )?;
    if response.status != 200 {
        return Err(CliError::auth(format!(
            "api/session -> {} (cookies expired? re-export)",
            response.status
        )));
    }
    let info = match serde_json::from_str::<Value>(&response.body) {
        Ok(value) => value,
        Err(_) => {
            return Err(CliError::auth(format!(
                "api/session returned non-JSON ({:?})",
                first_chars(&response.body, 80)
            )));
        }
    };
    match &info {
        Value::Object(map) if map.contains_key("vm_id") => Ok(info),
        _ => Err(CliError::gateway(
            -1,
            format!("VM unavailable ({info}); retry shortly or `MUSE_VM_ID=<id> muse-cli wake`"),
        )),
    }
}

/// GET https://muse.ai/api/hatch/invite (read-only).
/// Used by: `invite` (and `invite --create` re-reads after creating).
pub fn fetch_invite(cookies: &str) -> Result<Value, CliError> {
    let response = http_request(
        "GET",
        "https://muse.ai/api/hatch/invite",
        &hatch_headers(cookies, None),
        None,
        20,
    )?;
    if response.status == 401 || response.status == 403 {
        return Err(CliError::auth(format!(
            "api/hatch/invite -> {} (cookies expired? re-export)",
            response.status
        )));
    }
    if response.status != 200 {
        return Err(CliError::gateway(
            response.status as i64,
            first_chars(&response.body, 200),
        ));
    }
    let parsed: Value = match serde_json::from_str::<Value>(&response.body) {
        Ok(value) => value,
        Err(_) => {
            return Err(CliError::gateway(
                -1,
                format!(
                    "api/hatch/invite returned non-JSON ({:?})",
                    first_chars(&response.body, 80)
                ),
            ));
        }
    };
    match parsed {
        Value::Object(_) => Ok(parsed),
        other => Err(CliError::gateway(
            -1,
            format!("api/hatch/invite returned {}", json_type_name(&other)),
        )),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// POST https://muse.ai/api/hatch/invite with an empty body (WRITES: this
/// is what opening Invite friends in the app does). Only for
/// `invite --create`, and only when GET returned `code: null`.
pub fn create_invite(cookies: &str) -> Result<Value, CliError> {
    let response = http_request(
        "POST",
        "https://muse.ai/api/hatch/invite",
        &hatch_headers(cookies, None),
        Some(Vec::new()),
        30,
    )?;
    if response.status == 401 || response.status == 403 {
        return Err(CliError::auth(format!(
            "api/hatch/invite POST -> {} (cookies expired? re-export)",
            response.status
        )));
    }
    if response.status != 200 {
        return Err(CliError::gateway(
            response.status as i64,
            first_chars(&response.body, 200),
        ));
    }
    parse_json_body(&response.body).map_err(|_| {
        CliError::gateway(
            -1,
            format!(
                "api/hatch/invite POST returned non-JSON ({:?})",
                first_chars(&response.body, 80)
            ),
        )
    })
}

/// POST https://muse.ai/api/hatch/invite-code/redeem (WRITES, one-shot).
/// The code is stripped and upper-cased, then validated against
/// `^[A-Z0-9]{6,16}$` BEFORE any request is sent (a bad code is a `Value`
/// error, exit 2, with no request). Returns `(http_status, body)`; a
/// non-JSON body becomes `{success: false, reason: "unknown", detail}`.
/// Used by: `redeem` only. No gateway (Noise) call is involved.
pub fn redeem_invite_code(cookies: &str, code: &str) -> Result<(u16, Value), CliError> {
    let code = code.trim().to_uppercase();
    let valid = (6..=16).contains(&code.len())
        && code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !valid {
        return Err(CliError::value(format!("not an invite code: {code:?}")));
    }
    let body = serde_json::json!({"code": code, "supportsRedemptionStatus": true});
    let response = http_request(
        "POST",
        "https://muse.ai/api/hatch/invite-code/redeem",
        &hatch_headers(cookies, None),
        Some(serde_json::to_vec(&body).map_err(|e| io_error(format!("json error: {e}")))?),
        30,
    )?;
    if response.status == 401 || response.status == 403 {
        return Err(CliError::auth(format!(
            "invite-code/redeem -> {} (cookies expired? re-export)",
            response.status
        )));
    }
    match serde_json::from_str::<Value>(&response.body) {
        Ok(value) => Ok((response.status, value)),
        Err(_) => Ok((
            response.status,
            serde_json::json!({
                "success": false,
                "reason": "unknown",
                "detail": first_chars(&response.body, 200),
            }),
        )),
    }
}

/// POST https://muse.ai/api/hatch/vm/wake. Mirrors Python `cmd_wake`:
/// the access token is fetched first, and `vm_id` comes from the argument
/// or, when absent, from `fetch_session_info`. Returns
/// `{"status": <http_code>, "body": <json or null>}`.
/// DEVIATION (minor): Python would raise on a non-empty non-JSON body
/// (`resp.json()`); here that case yields `body: null` instead of an
/// exception, so `wake` still prints a usable JSON object.
/// Used by: `wake` only. No gateway (Noise) call is involved.
pub fn wake_vm(cookies: &str, vm_id: Option<&str>) -> Result<Value, CliError> {
    let access_token = fetch_access_token(cookies)?;
    let vm_id = match vm_id {
        Some(id) => id.to_string(),
        None => {
            let info = fetch_session_info(cookies)?;
            info.get("vm_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    CliError::gateway(-1, "VM unavailable (no vm_id in session info)".to_string())
                })?
        }
    };
    let body = serde_json::json!({"vm_id": vm_id, "retry_count": 0});
    let response = http_request(
        "POST",
        "https://muse.ai/api/hatch/vm/wake",
        &hatch_headers(cookies, Some(&access_token)),
        Some(serde_json::to_vec(&body).map_err(|e| io_error(format!("json error: {e}")))?),
        15,
    )?;
    let parsed = if response.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&response.body).unwrap_or(Value::Null)
    };
    Ok(serde_json::json!({"status": response.status, "body": parsed}))
}

/// Parse a Next.js server-action (RSC) `text/x-component` body, exactly
/// like Python `parse_rsc_result`: keep only lines whose value (after the
/// first `:`) starts with `{` or `[` and parses as JSON; if row `"0"`
/// references `"$@<n>"`, return row `"<n>"`, else row `"1"` (falling back
/// to row `"0"`).
pub fn parse_rsc_result(text: &str) -> Option<Value> {
    let mut rows: HashMap<String, Value> = HashMap::new();
    for line in text.lines() {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let (key, val) = (&line[..colon], &line[colon + 1..]);
        if val.starts_with('{') || val.starts_with('[') {
            if let Ok(value) = serde_json::from_str::<Value>(val) {
                rows.insert(key.to_string(), value);
            }
        }
    }
    let head = rows.get("0").cloned();
    if let Some(Value::Object(head_map)) = &head {
        if let Some(Value::String(reference)) = head_map.get("a") {
            if let Some(target) = reference.strip_prefix("$@") {
                return rows.get(target).cloned();
            }
        }
    }
    rows.get("1").cloned().or(head)
}

fn call_action(cookies: &str, action_id: &str, args: &Value) -> Result<Option<Value>, CliError> {
    let mut headers = hatch_headers(cookies, None);
    for (key, value) in headers.iter_mut() {
        if key == "Accept" {
            *value = "text/x-component".to_string();
        } else if key == "Content-Type" {
            *value = "text/plain;charset=UTF-8".to_string();
        }
    }
    headers.push(("Next-Action".to_string(), action_id.to_string()));
    let response = http_request(
        "POST",
        "https://muse.ai/settings",
        &headers,
        Some(serde_json::to_vec(args).map_err(|e| io_error(format!("json error: {e}")))?),
        30,
    )?;
    if response.status == 401 || response.status == 403 {
        return Err(CliError::auth(format!(
            "server action -> {} (cookies expired? re-export)",
            response.status
        )));
    }
    if response.status != 200 || !response.content_type.contains("x-component") {
        return Ok(None);
    }
    Ok(parse_rsc_result(&response.body))
}

fn chunk_path_candidates(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find(prefix) {
        let after = &rest[index + prefix.len()..];
        let mut end = 0;
        for (i, c) in after.char_indices() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        let candidate = &after[..end];
        if candidate.ends_with(".js") {
            out.push(format!("{prefix}{candidate}"));
        }
        rest = &after[end.max(1)..];
    }
    out
}

fn find_action_in_js(text: &str, name: &str) -> Option<String> {
    let marker = "createServerReference)(";
    let mut rest = text;
    while let Some(index) = rest.find(marker) {
        let after = &rest[index + marker.len()..];
        // Expect: "<hex>",<up to 120 chars without ')'>"<name>"
        let after = after.strip_prefix('"')?;
        let mut hex_end = 0;
        for (i, c) in after.char_indices() {
            if c.is_ascii_hexdigit() {
                hex_end = i + c.len_utf8();
            } else {
                break;
            }
        }
        let hex = &after[..hex_end];
        let tail = &after[hex_end..];
        if hex.len() >= 20 && tail.starts_with("\",") {
            let window = &tail[2..];
            let window = match window.find(')') {
                Some(position) if position < 120 => &window[..position],
                Some(_) => &window[..120.min(window.len())],
                None => &window[..120.min(window.len())],
            };
            if window.contains(&format!("\"{name}\"")) {
                return Some(hex.to_string());
            }
        }
        rest = &after[hex_end.max(1)..];
    }
    None
}

/// Scan the web app's JS chunks for a server action's id.
/// DEVIATION (performance only): Python uses a 16-thread pool; this
/// fetches chunks sequentially. The result (first id found, or `None`
/// after 3000 files) is the same, only slower.
/// Used by: `usage` (via `fetch_subscription`) only.
pub fn find_action_id(cookies: &str, name: &str, page: &str) -> Result<Option<String>, CliError> {
    let headers = vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("Cookie".to_string(), cookies.to_string()),
    ];
    let html = http_request("GET", page, &headers, None, 30)?.body;
    let mut todo: std::collections::HashSet<String> =
        chunk_path_candidates(&html, "/_next/static/chunks/")
            .into_iter()
            .collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while !todo.is_empty() && seen.len() < 3000 {
        let batch: Vec<String> = todo.difference(&seen).cloned().collect();
        todo.clear();
        for path in &batch {
            seen.insert(path.clone());
        }
        for path in batch {
            let text = match http_request("GET", &format!("https://muse.ai{path}"), &[], None, 30) {
                Ok(response) => response.body,
                Err(_) => String::new(),
            };
            if let Some(id) = find_action_in_js(&text, name) {
                return Ok(Some(id));
            }
            for candidate in chunk_path_candidates(&text, "static/chunks/") {
                todo.insert(format!("/_next/{candidate}"));
            }
        }
    }
    Ok(None)
}

/// Python truthiness for a JSON value, as used by
/// `gateway.py:fetch_subscription`'s `not res.get("subscription")`
/// check: null, false, 0, `""`, `[]` and `{}` are all falsy.
/// Pure, so it is unit-testable without any HTTP call.
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

/// Extract the subscription dict from a server-action result, or
/// `None` when the result is not a success with a truthy
/// subscription. Python (`gateway.py:fetch_subscription`) rejects
/// when `not res.get("subscription")`, so a present-but-null (or
/// otherwise falsy) subscription is a failure, not a success with
/// a null subscription (fix round 2, TECHNICAL F04).
fn subscription_from_result(result: &Value) -> Option<Value> {
    match result {
        Value::Object(map)
            if map
                .get("success")
                .is_some_and(|v| v.as_bool().unwrap_or(false)) =>
        {
            match map.get("subscription") {
                Some(sub) if json_truthy(sub) => Some(sub.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Plan + usage via the Next.js server action `fetchSubscriptionAction`.
/// Tries the given action id, then the built-in default, then a bundle scan.
/// Returns `(subscription_dict, action_id_used)`.
/// Used by: `usage` only. No gateway (Noise) call is involved.
pub fn fetch_subscription(
    cookies: &str,
    action_id: Option<&str>,
    include_agreement: bool,
) -> Result<(Value, String), CliError> {
    let args = serde_json::json!([{"includeAgreement": include_agreement}]);
    let mut tried: Vec<String> = Vec::new();
    let mut result: Option<Value> = None;
    let mut used_id = String::new();
    for candidate in [action_id, Some(SUBSCRIPTION_ACTION_ID)] {
        let Some(candidate) = candidate else {
            continue;
        };
        if tried.iter().any(|t| t == candidate) {
            continue;
        }
        tried.push(candidate.to_string());
        let res = call_action(cookies, candidate, &args)?;
        if res.is_some() {
            result = res;
            used_id = candidate.to_string();
            break;
        }
    }
    if result.is_none() {
        let found = find_action_id(
            cookies,
            SUBSCRIPTION_ACTION_NAME,
            "https://muse.ai/settings",
        )?;
        let Some(found) = found else {
            return Err(CliError::gateway(
                -1,
                format!("could not find {SUBSCRIPTION_ACTION_NAME} in the web app bundle"),
            ));
        };
        result = call_action(cookies, &found, &args)?;
        used_id = found;
    }
    let subscription = result.as_ref().and_then(subscription_from_result);
    match subscription {
        Some(subscription) => Ok((subscription, used_id)),
        None => {
            let error_text = match &result {
                Some(Value::Object(map)) => match map.get("error") {
                    Some(error) => error.to_string(),
                    None => "null".to_string(),
                },
                Some(other) => other.to_string(),
                None => "None".to_string(),
            };
            Err(CliError::gateway(
                -1,
                format!("{SUBSCRIPTION_ACTION_NAME} failed: {error_text}"),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Chunk reassembly
// ---------------------------------------------------------------------------

/// Feed one `NoiseTransportFrame`; return the full payload once complete.
/// `total <= 1` (including 0 and unset) returns the payload immediately.
/// Otherwise parts are collected by `chunk_index` (a duplicate index
/// overwrites) and joined sorted by index once `total` parts are present.
/// A missing index therefore never completes: the caller times out at the
/// layer above; there is no reassembly-local timeout (Python parity).
pub fn join_chunks(
    partial: &mut HashMap<i64, HashMap<u32, Vec<u8>>>,
    frame: &NoiseTransportFrame,
) -> Option<Vec<u8>> {
    let total = frame.total_chunks.unwrap_or(1);
    if total <= 1 {
        return Some(frame.payload.clone().unwrap_or_default());
    }
    let chunk_id = frame.chunk_id.unwrap_or(0);
    let chunk_index = frame.chunk_index.unwrap_or(0);
    let parts = partial.entry(chunk_id).or_default();
    parts.insert(chunk_index, frame.payload.clone().unwrap_or_default());
    if (parts.len() as u32) < total {
        return None;
    }
    let parts = partial.remove(&chunk_id).unwrap_or_default();
    let mut indices: Vec<u32> = parts.keys().copied().collect();
    indices.sort_unstable();
    let mut out = Vec::new();
    for index in indices {
        if let Some(bytes) = parts.get(&index) {
            out.extend_from_slice(bytes);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

impl Gateway {
    fn from_transport(vm_id: String, mut transport: Box<dyn Transport>) -> Result<Self, CliError> {
        let mut noise = NoiseSession::new_initiator()?;
        transport.send_bytes(&noise.write_handshake_message()?)?;
        let msg2 = transport.recv_bytes()?;
        noise.read_handshake_message(&msg2)?;
        transport.send_bytes(&noise.write_handshake_message()?)?;
        if !noise.handshake_finished() {
            return Err(io_error("noise handshake did not finish".to_string()));
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(GatewayInner {
                transport,
                noise,
                partial: HashMap::new(),
            })),
            stream: Mutex::new(1),
            vm_id,
        })
    }

    /// Full connect: HTTPS auth (session only when `vm_id` is absent,
    /// then auth/check, then hatch/token) + WebSocket + Noise handshake.
    /// TLS verification is ON.
    pub fn connect(cfg: &Config) -> Result<Self, CliError> {
        let cookies = crate::config::load_cookies(&cfg.cookies_file)?;
        if cookies.trim().is_empty() {
            return Err(CliError::auth(format!(
                "cookies file {} is empty; run `muse-cli auth export`",
                cfg.cookies_file.display()
            )));
        }
        let vm_id = match &cfg.vm_id {
            Some(id) => id.clone(),
            None => {
                let info = fetch_session_info(&cookies)?;
                info.get("vm_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        CliError::gateway(
                            -1,
                            "VM unavailable (no vm_id in session info)".to_string(),
                        )
                    })?
            }
        };
        let access_token = fetch_access_token(&cookies)?;
        let hatch_token = fetch_hatch_token(&cookies, &access_token, &vm_id)?;
        let url = format!(
            "wss://{GATEWAY_HOST}/v1/noise?vm_id={vm_id}&auth_token={}",
            crate::routes::quote_path_param(&hatch_token)
        );
        let transport = WsTransport::connect(&url)?;
        Self::from_transport(vm_id, Box::new(transport))
    }

    /// Same Noise handshake, but over an injected transport and WITHOUT
    /// the HTTPS auth calls. The injected transport is what tests use
    /// (an in-memory pair): performing live muse.ai auth here would make
    /// the documented test use impossible, so skipping it is deliberate
    /// and is the one intentional reading of "same, but transport
    /// injected". `vm_id` comes from the config, or is empty when absent.
    pub fn connect_with_transport(
        cfg: &Config,
        transport: Box<dyn Transport>,
    ) -> Result<Self, CliError> {
        let vm_id = cfg.vm_id.clone().unwrap_or_default();
        Self::from_transport(vm_id, transport)
    }

    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    /// Next stream id to be allocated (starts at 1, monotonic, shared by
    /// unary and subscription streams on this connection).
    pub fn stream_counter(&self) -> i64 {
        lock_stream(&self.stream).map(|guard| *guard).unwrap_or(1)
    }

    /// Best-effort close of the transport. If the subscription reader
    /// thread currently holds the transport lock (blocked in recv), the
    /// close is skipped rather than deadlocking.
    pub fn close(&mut self) {
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.transport.close();
        }
    }

    fn allocate_stream(&self) -> Result<i64, CliError> {
        let mut guard = lock_stream(&self.stream)?;
        let sid = *guard;
        *guard += 1;
        Ok(sid)
    }

    /// Send one application request envelope (3 layers: Noise transport
    /// frame -> ServiceRequest -> ServiceFrame) WITHOUT splitting
    /// (`total_chunks=1`, random 8-byte little-endian signed `chunk_id`).
    /// Returns the allocated stream id.
    fn send_application_request(
        &self,
        verb: &str,
        path: &str,
        headers: &[(String, String)],
        raw_body: &[u8],
        service: i32,
    ) -> Result<i64, CliError> {
        let sid = self.allocate_stream()?;
        let request = ApplicationRequest {
            verb: Some(verb.to_string()),
            path: Some(path.to_string()),
            headers: headers
                .iter()
                .map(|(key, value)| Header {
                    key: Some(key.clone()),
                    value: Some(value.clone()),
                })
                .collect(),
            body: Some(raw_body.to_vec()),
            end_body: Some(true),
        };
        let frame = ServiceFrame {
            stream_id: Some(sid),
            kind: Some(ServiceFrameKind::Request(request)),
        };
        let frame_bytes = frame.encode_to_vec();
        let outer = ServiceRequest {
            service: Some(service),
            payload: Some(frame_bytes),
        };
        let transport_frame = NoiseTransportFrame {
            chunk_id: Some(random_chunk_id()),
            chunk_index: Some(0),
            total_chunks: Some(1),
            payload: Some(outer.encode_to_vec()),
        };
        let mut inner = lock_inner(&self.inner)?;
        let ciphertext = inner.noise.encrypt(&transport_frame.encode_to_vec())?;
        inner.transport.send_bytes(&ciphertext)?;
        Ok(sid)
    }

    fn open_with_route(
        &self,
        route: &Route,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
    ) -> Result<i64, CliError> {
        let built = crate::routes::build_request_with_route(route, path_params, body, query)?;
        self.send_application_request(
            &built.verb,
            &built.path,
            &built.headers,
            &built.raw_body,
            service_id(route.service.as_deref()),
        )
    }

    /// Open a stream without waiting for its response (Python `send`
    /// uses this for `chat.stream`: the stream's own response is ignored
    /// and delivery is confirmed by polling history instead).
    pub fn open_stream(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
    ) -> Result<i64, CliError> {
        let route = lookup(method)?;
        self.open_with_route(route, path_params, body, query)
    }

    /// Read one complete `ServiceFrame`, joining transport chunks as
    /// needed. The whole recv/decrypt/parse loop runs under the single
    /// Noise+transport lock.
    fn read_frame(&self) -> Result<ServiceFrame, CliError> {
        read_frame_from(&self.inner)
    }

    /// Wait for the response of stream `sid` until `timeout`.
    /// Frames of other streams are discarded, not buffered. A `reset`
    /// raises `Gateway(-1)`. If no response head arrived before the
    /// deadline, this is a `Timeout`. QUIRK (kept for parity): if a
    /// response head arrived but `end_body` never did before the
    /// deadline, the accumulated payload is returned WITHOUT an error.
    fn wait_response(
        &self,
        sid: i64,
        method_label: &str,
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>), CliError> {
        let deadline = Instant::now() + timeout;
        let mut status: Option<i32> = None;
        let mut seen_response = false;
        let mut chunks: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            let frame = self.read_frame()?;
            if frame.stream_id != Some(sid) {
                continue;
            }
            match frame.kind {
                Some(ServiceFrameKind::Response(response)) => {
                    seen_response = true;
                    status = Some(response.status.unwrap_or(0));
                    if let Some(body) = response.body {
                        chunks.extend_from_slice(&body);
                    }
                    if response.end_body.unwrap_or(false) {
                        break;
                    }
                }
                Some(ServiceFrameKind::BodyChunk(chunk)) => {
                    if let Some(data) = chunk.data {
                        chunks.extend_from_slice(&data);
                    }
                    if chunk.end_body.unwrap_or(false) {
                        break;
                    }
                }
                Some(ServiceFrameKind::Reset(reset)) => {
                    return Err(CliError::gateway(
                        -1,
                        format!(
                            "stream reset {}: {}",
                            reset.code.unwrap_or(0),
                            reset.reason.unwrap_or_default()
                        ),
                    ));
                }
                _ => {}
            }
        }
        if !seen_response {
            return Err(CliError::timeout(format!("no response for {method_label}")));
        }
        let status = status.unwrap_or(0);
        if !(200..300).contains(&status) {
            let head = &chunks[..chunks.len().min(500)];
            return Err(CliError::gateway(
                status as i64,
                String::from_utf8_lossy(head).into_owned(),
            ));
        }
        Ok((status, chunks))
    }

    /// Unary request/response. GET + body + no query => the body becomes
    /// the query (gateway spec §(b.5)); GET with both body and query
    /// keeps the body as-is.
    pub fn request(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>), CliError> {
        let route = lookup(method)?;
        let (effective_body, effective_query) =
            if route.http == "GET" && body.is_some() && query.is_none() {
                (None, body)
            } else {
                (body, query)
            };
        let sid = self.open_with_route(route, path_params, effective_body, effective_query)?;
        self.wait_response(sid, method, timeout)
    }

    /// Unary request over a synthetic route (not in `routes.json`), e.g.
    /// `media.raw`: GET `/media/raw/<path>`, service daemon (0).
    /// `body` is raw bytes (not JSON). The GET body->query conversion is
    /// NOT applied here: synthetic routes are used with no body
    /// (download), and Python's conversion expects a JSON-object body.
    pub fn request_with_route(
        &mut self,
        verb: &str,
        path: &str,
        service: i32,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>), CliError> {
        let raw_body = body.unwrap_or(&[]);
        let mut headers = vec![("x-request-id".to_string(), Uuid::new_v4().to_string())];
        if !raw_body.is_empty() {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        let sid = self.send_application_request(verb, path, &headers, raw_body, service)?;
        self.wait_response(sid, "media.raw", timeout)
    }

    /// `request` + JSON unwrap: empty payload -> `{}`; invalid JSON ->
    /// `Gateway(-1, "non-JSON response: …")`; dict with `ok == false` ->
    /// `Gateway(-1, "api error: …")`; dict with `result` -> that value,
    /// else the whole dict; non-dict JSON -> as-is.
    pub fn call_json(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, CliError> {
        let (_, payload) = self.request(method, path_params, body, query, timeout)?;
        if payload.is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        let value: Value = match serde_json::from_slice(&payload) {
            Ok(value) => value,
            Err(_) => {
                let head = &payload[..payload.len().min(200)];
                return Err(CliError::gateway(
                    -1,
                    format!("non-JSON response: {:?}", String::from_utf8_lossy(head)),
                ));
            }
        };
        if let Value::Object(map) = &value {
            if map.get("ok").is_some_and(|v| v.as_bool() == Some(false)) {
                let error = match map.get("error") {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => "None".to_string(),
                };
                return Err(CliError::gateway(-1, format!("api error: {error}")));
            }
            if let Some(result) = map.get("result") {
                return Ok(result.clone());
            }
        }
        Ok(value)
    }

    /// `request`, returning the raw payload bytes (status errors already
    /// raised by `request`).
    pub fn call_bytes(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
        timeout: Duration,
    ) -> Result<Vec<u8>, CliError> {
        let (_, payload) = self.request(method, path_params, body, query, timeout)?;
        Ok(payload)
    }

    /// Download a sandbox/media resource through the Noise tunnel via
    /// the synthetic `media.raw` route (Python `Gateway.download_file`).
    pub fn download_bytes(&mut self, ref_: &Value, timeout: Duration) -> Result<Vec<u8>, CliError> {
        let path = media_path(ref_)?;
        let (_, bytes) = self.request_with_route("GET", &path, 0, None, timeout)?;
        Ok(bytes)
    }

    /// Subscribe to an NDJSON event stream (Python `subscribe_raw`).
    /// A reader thread pushes `ServiceFrame`s into a channel; the main
    /// loop polls it every 2s. The idle timer is reset ONLY by frames of
    /// our own stream (other-stream frames are discarded without
    /// resetting it). Records are newline-split lines with non-blank
    /// content; `on_record` returning `true` halts immediately (the
    /// record was already appended). A `reset` breaks without error, as
    /// does `end_body` on an empty buffer. After the loop, a trailing
    /// non-blank buffer becomes a final record (`on_record` is called,
    /// its result ignored). Stops on: halt, overall timeout, idle
    /// timeout, `max_records`, `end_body`/`reset`.
    ///
    /// The reader thread is left detached when this returns (it may be
    /// blocked in `recv_bytes`): the Python reader is a daemon thread
    /// with the same property. Do NOT reuse this `Gateway` for another
    /// call afterwards; `wait.rs` therefore re-reads history on a fresh
    /// connection (see its `run_wait`).
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_records(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
        idle_timeout: Duration,
        overall_timeout: Duration,
        max_records: usize,
        on_record: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Vec<String>, CliError> {
        let route = lookup(method)?;
        let sid = self.open_with_route(route, path_params, body, query)?;
        let (tx, rx) = std::sync::mpsc::channel::<Result<ServiceFrame, CliError>>();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_inner = Arc::clone(&self.inner);
        let reader_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !reader_stop.load(Ordering::SeqCst) {
                match read_frame_from(&reader_inner) {
                    Ok(frame) => {
                        if tx.send(Ok(frame)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });

        let overall_deadline = Instant::now() + overall_timeout;
        let mut idle_deadline = Instant::now() + idle_timeout;
        let mut buf: Vec<u8> = Vec::new();
        let mut records: Vec<String> = Vec::new();
        let mut halted = false;
        let mut result: Result<Vec<String>, CliError> = Ok(Vec::new());
        'outer: while !halted && Instant::now() < overall_deadline && records.len() < max_records {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(frame)) => {
                    if frame.stream_id != Some(sid) {
                        continue;
                    }
                    idle_deadline = Instant::now() + idle_timeout;
                    let mut ended = false;
                    match frame.kind {
                        Some(ServiceFrameKind::Response(response)) => {
                            if let Some(body) = response.body {
                                buf.extend_from_slice(&body);
                            }
                            if response.end_body.unwrap_or(false) && buf.is_empty() {
                                ended = true;
                            }
                        }
                        Some(ServiceFrameKind::BodyChunk(chunk)) => {
                            if let Some(data) = chunk.data {
                                buf.extend_from_slice(&data);
                            }
                            if chunk.end_body.unwrap_or(false) {
                                ended = true;
                            }
                        }
                        Some(ServiceFrameKind::Reset(_)) => {
                            ended = true;
                        }
                        _ => {}
                    }
                    if ended {
                        break 'outer;
                    }
                    while let Some(position) = buf.iter().position(|&b| b == b'\n') {
                        let mut line: Vec<u8> = buf.drain(..=position).collect();
                        line.pop();
                        if line.iter().all(|b| b.is_ascii_whitespace()) {
                            continue;
                        }
                        let record = String::from_utf8_lossy(&line).into_owned();
                        records.push(record.clone());
                        if on_record(&record) {
                            halted = true;
                            break;
                        }
                        if records.len() >= max_records {
                            break;
                        }
                    }
                }
                Ok(Err(err)) => {
                    result = Err(err);
                    break 'outer;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() > idle_deadline {
                        break 'outer;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break 'outer;
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        if result.is_err() {
            return result;
        }
        if !buf.iter().all(|b| b.is_ascii_whitespace()) && !buf.is_empty() && !halted {
            let record = String::from_utf8_lossy(&buf).into_owned();
            records.push(record.clone());
            on_record(&record);
        }
        Ok(records)
    }

    /// `subscribe_records` + JSON parsing per line, silently skipping
    /// lines that are not valid JSON (Python `subscribe_json`).
    pub fn subscribe_json(
        &mut self,
        method: &str,
        path_params: &[(String, String)],
        body: Option<&Value>,
        query: Option<&Value>,
        idle_timeout: Duration,
        overall_timeout: Duration,
        max_records: usize,
    ) -> Result<Vec<Value>, CliError> {
        let mut noop = |_: &str| false;
        let records = self.subscribe_records(
            method,
            path_params,
            body,
            query,
            idle_timeout,
            overall_timeout,
            max_records,
            &mut noop,
        )?;
        let mut out = Vec::new();
        for record in &records {
            if let Ok(value) = serde_json::from_str::<Value>(record) {
                out.push(value);
            }
        }
        Ok(out)
    }
}

fn read_frame_from(inner: &Arc<Mutex<GatewayInner>>) -> Result<ServiceFrame, CliError> {
    let payload = {
        let mut guard = lock_inner(inner)?;
        loop {
            let data = guard.transport.recv_bytes()?;
            let plaintext = guard.noise.decrypt(&data)?;
            let frame = NoiseTransportFrame::decode(plaintext.as_slice())
                .map_err(|e| io_error(format!("invalid NoiseTransportFrame: {e}")))?;
            if let Some(payload) = join_chunks(&mut guard.partial, &frame) {
                break payload;
            }
        }
    };
    let service_response = ServiceResponse::decode(payload.as_slice())
        .map_err(|e| io_error(format!("invalid ServiceResponse: {e}")))?;
    let service_frame =
        ServiceFrame::decode(service_response.payload.unwrap_or_default().as_slice())
            .map_err(|e| io_error(format!("invalid ServiceFrame: {e}")))?;
    Ok(service_frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::NoiseSession as Noise;
    use crate::proto::{ApplicationResponse, BodyChunk, Reset};
    use std::sync::mpsc::{Receiver, Sender};

    // -- join_chunks ------------------------------------------------------

    fn frame(chunk_id: i64, index: u32, total: u32, payload: &[u8]) -> NoiseTransportFrame {
        NoiseTransportFrame {
            chunk_id: Some(chunk_id),
            chunk_index: Some(index),
            total_chunks: Some(total),
            payload: Some(payload.to_vec()),
        }
    }

    #[test]
    fn join_chunks_single_returns_immediately() {
        let mut partial = HashMap::new();
        assert_eq!(
            join_chunks(&mut partial, &frame(1, 0, 1, b"abc")),
            Some(b"abc".to_vec())
        );
        assert!(partial.is_empty());
    }

    #[test]
    fn join_chunks_in_order_and_reverse_order() {
        let mut partial = HashMap::new();
        assert_eq!(join_chunks(&mut partial, &frame(9, 0, 3, b"a")), None);
        assert_eq!(join_chunks(&mut partial, &frame(9, 1, 3, b"b")), None);
        assert_eq!(
            join_chunks(&mut partial, &frame(9, 2, 3, b"c")),
            Some(b"abc".to_vec())
        );
        assert!(partial.is_empty());

        let mut partial = HashMap::new();
        assert_eq!(join_chunks(&mut partial, &frame(9, 2, 3, b"c")), None);
        assert_eq!(join_chunks(&mut partial, &frame(9, 0, 3, b"a")), None);
        assert_eq!(
            join_chunks(&mut partial, &frame(9, 1, 3, b"b")),
            Some(b"abc".to_vec())
        );
    }

    #[test]
    fn join_chunks_missing_chunk_never_completes() {
        let mut partial = HashMap::new();
        assert_eq!(join_chunks(&mut partial, &frame(9, 0, 3, b"a")), None);
        assert_eq!(join_chunks(&mut partial, &frame(9, 2, 3, b"c")), None);
        assert_eq!(partial.len(), 1);
    }

    #[test]
    fn join_chunks_duplicate_index_overwrites_and_does_not_count_twice() {
        let mut partial = HashMap::new();
        assert_eq!(join_chunks(&mut partial, &frame(9, 0, 2, b"a")), None);
        assert_eq!(join_chunks(&mut partial, &frame(9, 0, 2, b"A")), None);
        assert_eq!(
            join_chunks(&mut partial, &frame(9, 1, 2, b"b")),
            Some(b"Ab".to_vec())
        );
    }

    // -- in-memory transport pair + fake server ---------------------------

    struct PairTransport {
        tx: Sender<Vec<u8>>,
        rx: Receiver<Vec<u8>>,
    }

    impl Transport for PairTransport {
        fn send_bytes(&mut self, data: &[u8]) -> Result<(), CliError> {
            self.tx
                .send(data.to_vec())
                .map_err(|_| io_error("test transport closed".to_string()))
        }

        fn recv_bytes(&mut self) -> Result<Vec<u8>, CliError> {
            self.rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| io_error("test transport timeout".to_string()))
        }
    }

    fn make_pair() -> (PairTransport, PairTransport) {
        let (a_tx, a_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (b_tx, b_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        (
            PairTransport { tx: b_tx, rx: a_rx },
            PairTransport { tx: a_tx, rx: b_rx },
        )
    }

    struct FakeServer {
        transport: PairTransport,
        noise: Noise,
    }

    impl FakeServer {
        fn handshake(mut transport: PairTransport) -> Self {
            let mut noise = Noise::new_responder().unwrap();
            let msg1 = transport.recv_bytes().unwrap();
            noise.read_handshake_message(&msg1).unwrap();
            transport
                .send_bytes(&noise.write_handshake_message().unwrap())
                .unwrap();
            let msg3 = transport.recv_bytes().unwrap();
            noise.read_handshake_message(&msg3).unwrap();
            assert!(noise.handshake_finished());
            Self { transport, noise }
        }

        fn recv_frame(&mut self) -> ServiceFrame {
            let data = self.transport.recv_bytes().unwrap();
            let plaintext = self.noise.decrypt(&data).unwrap();
            let transport_frame = NoiseTransportFrame::decode(plaintext.as_slice()).unwrap();
            let request =
                ServiceRequest::decode(transport_frame.payload.unwrap().as_slice()).unwrap();
            ServiceFrame::decode(request.payload.unwrap().as_slice()).unwrap()
        }

        fn send_frame(&mut self, frame: ServiceFrame) {
            let response = ServiceResponse {
                payload: Some(frame.encode_to_vec()),
            };
            let transport_frame = NoiseTransportFrame {
                chunk_id: Some(1234),
                chunk_index: Some(0),
                total_chunks: Some(1),
                payload: Some(response.encode_to_vec()),
            };
            let ciphertext = self
                .noise
                .encrypt(&transport_frame.encode_to_vec())
                .unwrap();
            self.transport.send_bytes(&ciphertext).unwrap();
        }

        fn respond(&mut self, stream_id: i64, status: i32, body: &[u8], end_body: bool) {
            self.send_frame(ServiceFrame {
                stream_id: Some(stream_id),
                kind: Some(ServiceFrameKind::Response(ApplicationResponse {
                    status: Some(status),
                    headers: Vec::new(),
                    body: Some(body.to_vec()),
                    end_body: Some(end_body),
                })),
            });
        }
    }

    fn test_config() -> Config {
        Config {
            config_dir: std::path::PathBuf::from("/tmp/fake-config"),
            cookies_file: std::path::PathBuf::from("/tmp/fake-cookies.txt"),
            vm_id: Some("fake-vm".to_string()),
            node_id: "0123456789abcdef".to_string(),
            auto_approve: None,
            subscription_action_id: None,
        }
    }

    fn connect_pair() -> (Gateway, FakeServer) {
        let (client_transport, server_transport) = make_pair();
        let server_handle = std::thread::spawn(move || FakeServer::handshake(server_transport));
        let gateway = Gateway::connect_with_transport(&test_config(), Box::new(client_transport))
            .expect("in-memory connect must succeed");
        let server = server_handle.join().expect("server handshake thread");
        (gateway, server)
    }

    #[test]
    fn loopback_stream_counter_and_unary_call_json() {
        let (mut gateway, mut server) = connect_pair();
        assert_eq!(gateway.stream_counter(), 1);
        let handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            assert_eq!(request.stream_id, Some(1));
            let ServiceFrameKind::Request(app) = request.kind.unwrap() else {
                panic!("expected a request frame");
            };
            // GET + body became a query string (spec §(b.5)).
            assert_eq!(app.verb.as_deref(), Some("GET"));
            assert_eq!(app.path.as_deref(), Some("/chat/history?limit=1"));
            server.respond(1, 200, b"{\"ok\":true,\"result\":{\"hello\":1}}", true);
        });
        let value = gateway
            .call_json(
                "chat.history",
                &[],
                Some(&serde_json::json!({"limit": 1})),
                None,
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(value, serde_json::json!({"hello": 1}));
        assert_eq!(gateway.stream_counter(), 2);
        handle.join().unwrap();
    }

    #[test]
    fn loopback_other_stream_frames_are_discarded() {
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.respond(99, 200, b"wrong stream", true);
            server.respond(sid, 200, b"{\"ok\":true}", true);
        });
        let value = gateway
            .call_json("sessions.list", &[], None, None, Duration::from_secs(10))
            .unwrap();
        // Dict without "result" is returned whole (the wrong-stream frame
        // above was discarded, not mistaken for the response).
        assert_eq!(value, serde_json::json!({"ok": true}));
        handle.join().unwrap();
    }

    #[test]
    fn loopback_reset_becomes_gateway_minus_one() {
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.send_frame(ServiceFrame {
                stream_id: Some(sid),
                kind: Some(ServiceFrameKind::Reset(Reset {
                    code: Some(2),
                    reason: Some("too slow".to_string()),
                })),
            });
        });
        let err = gateway
            .request("sessions.list", &[], None, None, Duration::from_secs(10))
            .unwrap_err();
        match err {
            CliError::Gateway { status, payload } => {
                assert_eq!(status, -1);
                assert_eq!(payload, "stream reset 2: too slow");
            }
            other => panic!("expected Gateway error, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn loopback_status_error_truncates_payload() {
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.respond(sid, 404, b"not found here", true);
        });
        let err = gateway
            .request("sessions.list", &[], None, None, Duration::from_secs(10))
            .unwrap_err();
        match err {
            CliError::Gateway { status, payload } => {
                assert_eq!(status, 404);
                assert_eq!(payload, "not found here");
            }
            other => panic!("expected Gateway error, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn call_json_variants() {
        // Empty payload -> {}; non-JSON -> Gateway(-1); ok:false -> api error.
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            for body in [
                "".as_bytes(),
                b"not json".as_slice(),
                b"{\"ok\":false,\"error\":\"boom\"}".as_slice(),
                b"[1,2]".as_slice(),
                b"{\"ok\":true}".as_slice(),
            ] {
                let request = server.recv_frame();
                let sid = request.stream_id.unwrap();
                server.respond(sid, 200, body, true);
            }
        });
        let timeout = Duration::from_secs(10);
        assert_eq!(
            gateway
                .call_json("sessions.list", &[], None, None, timeout)
                .unwrap(),
            serde_json::json!({})
        );
        assert!(matches!(
            gateway
                .call_json("sessions.list", &[], None, None, timeout)
                .unwrap_err(),
            CliError::Gateway { status: -1, .. }
        ));
        let err = gateway
            .call_json("sessions.list", &[], None, None, timeout)
            .unwrap_err();
        match err {
            CliError::Gateway { status, payload } => {
                assert_eq!(status, -1);
                assert_eq!(payload, "api error: boom");
            }
            other => panic!("expected Gateway error, got {other:?}"),
        }
        assert_eq!(
            gateway
                .call_json("sessions.list", &[], None, None, timeout)
                .unwrap(),
            serde_json::json!([1, 2])
        );
        // Dict without "result" is returned whole.
        assert_eq!(
            gateway
                .call_json("sessions.list", &[], None, None, timeout)
                .unwrap(),
            serde_json::json!({"ok": true})
        );
        handle.join().unwrap();
    }

    #[test]
    fn loopback_subscribe_records_and_halt() {
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.respond(sid, 200, b"first\nsecond\n", false);
            server.send_frame(ServiceFrame {
                stream_id: Some(sid),
                kind: Some(ServiceFrameKind::BodyChunk(BodyChunk {
                    data: Some(b"third\n".to_vec()),
                    end_body: Some(false),
                })),
            });
        });
        let mut seen: Vec<String> = Vec::new();
        let mut on_record = |record: &str| {
            seen.push(record.to_string());
            record == "second"
        };
        let records = gateway
            .subscribe_records(
                "chat.subscribe",
                &[],
                Some(&serde_json::json!({"capabilities": ["chat_cancel", "delta_stream"]})),
                None,
                Duration::from_secs(10),
                Duration::from_secs(10),
                100,
                &mut on_record,
            )
            .unwrap();
        assert_eq!(records, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);
        handle.join().unwrap();
    }

    #[test]
    fn subscribe_idle_timeout_without_frames() {
        let (mut gateway, mut server) = connect_pair();
        // Server consumes the open frame, then never sends anything.
        let handle = std::thread::spawn(move || {
            let _request = server.recv_frame();
            std::thread::sleep(Duration::from_secs(5));
        });
        let mut noop = |_: &str| false;
        let records = gateway
            .subscribe_records(
                "chat.subscribe",
                &[],
                None,
                None,
                Duration::from_secs(1),
                Duration::from_secs(30),
                100,
                &mut noop,
            )
            .unwrap();
        assert!(records.is_empty());
        handle.join().unwrap();
    }

    #[test]
    fn service_id_defaults_to_daemon() {
        assert_eq!(service_id(None), 0);
        assert_eq!(service_id(Some("daemon")), 0);
        assert_eq!(service_id(Some("sentinel")), 1);
        assert_eq!(service_id(Some("vault")), 2);
        assert_eq!(service_id(Some("authd")), 3);
        assert_eq!(service_id(Some("unknown-service")), 0);
    }

    #[test]
    fn loopback_no_response_before_deadline_is_timeout() {
        let (mut gateway, mut server) = connect_pair();
        let handle = std::thread::spawn(move || {
            let _request = server.recv_frame();
            // A frame for another stream does not count as a response;
            // the next frame arrives only after the client's deadline.
            server.respond(99, 200, b"wrong stream", true);
            std::thread::sleep(Duration::from_secs(2));
            server.respond(99, 200, b"wrong stream", true);
        });
        let err = gateway
            .request("sessions.list", &[], None, None, Duration::from_secs(1))
            .unwrap_err();
        match err {
            CliError::Timeout(message) => {
                assert_eq!(message, "no response for sessions.list");
            }
            other => panic!("expected Timeout error, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn run_wait_done_via_subscribe_then_history_reread() {
        use crate::wait::{run_wait, WaitOutcomeKind};
        let (mut gateway, mut server) = connect_pair();
        // Server 1: the subscribe stream. Running, then completed.
        let sub_handle = std::thread::spawn(move || {
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.respond(
                sid,
                200,
                b"{\"event\":\"task.status\",\"payload\":{\"status\":\"running\",\"agent_id\":\"root\",\"root_agent_id\":\"root\"}}\n",
                false,
            );
            server.send_frame(ServiceFrame {
                stream_id: Some(sid),
                kind: Some(ServiceFrameKind::BodyChunk(BodyChunk {
                    data: Some(
                        b"{\"event\":\"task.status\",\"payload\":{\"status\":\"completed\",\"agent_id\":\"root\",\"root_agent_id\":\"root\"}}\n"
                            .to_vec(),
                    ),
                    end_body: Some(false),
                })),
            });
        });
        // Server 2: the fresh connection for the history re-read.
        let (hist_client, hist_server) = make_pair();
        let hist_handle = std::thread::spawn(move || {
            let mut server = FakeServer::handshake(hist_server);
            let request = server.recv_frame();
            assert!(request.kind.is_some());
            let sid = request.stream_id.unwrap();
            server.respond(
                sid,
                200,
                b"{\"ok\":true,\"result\":{\"chat_events\":[{\"seq\":11,\"event_name\":\"message.assistant\",\"payload\":{\"display_text\":\"final answer\"}}]}}",
                true,
            );
        });
        let mut hist_transport = Some(hist_client);
        let mut factory = move || -> Result<Gateway, CliError> {
            match hist_transport.take() {
                Some(transport) => {
                    Gateway::connect_with_transport(&test_config(), Box::new(transport))
                }
                None => Err(CliError::io("factory exhausted".to_string())),
            }
        };
        let result = run_wait(
            &mut gateway,
            None,
            Some(10),
            Duration::from_secs(30),
            false,
            false,
            &mut factory,
        )
        .unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::Done);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.final_text.as_deref(), Some("final answer"));
        sub_handle.join().unwrap();
        hist_handle.join().unwrap();
    }

    #[test]
    fn run_wait_timeout_is_124_not_4() {
        use crate::wait::{run_wait, WaitOutcomeKind};
        let (mut gateway, mut server) = connect_pair();
        let sub_handle = std::thread::spawn(move || {
            let _request = server.recv_frame();
            std::thread::sleep(Duration::from_secs(5));
        });
        let (hist_client, hist_server) = make_pair();
        let hist_handle = std::thread::spawn(move || {
            let mut server = FakeServer::handshake(hist_server);
            let request = server.recv_frame();
            let sid = request.stream_id.unwrap();
            server.respond(
                sid,
                200,
                b"{\"ok\":true,\"result\":{\"chat_events\":[]}}",
                true,
            );
        });
        let mut hist_transport = Some(hist_client);
        let mut factory = move || -> Result<Gateway, CliError> {
            match hist_transport.take() {
                Some(transport) => {
                    Gateway::connect_with_transport(&test_config(), Box::new(transport))
                }
                None => Err(CliError::io("factory exhausted".to_string())),
            }
        };
        let result = run_wait(
            &mut gateway,
            None,
            Some(10),
            Duration::from_secs(1),
            false,
            false,
            &mut factory,
        )
        .unwrap();
        assert_eq!(result.kind, WaitOutcomeKind::Timeout);
        assert_eq!(result.exit_code, 124);
        assert_eq!(result.final_text, None);
        sub_handle.join().unwrap();
        hist_handle.join().unwrap();
    }

    #[test]
    fn hatch_headers_shape() {
        let headers = hatch_headers("hatch_sess=FAKE", Some("tok"));
        assert_eq!(headers[0].1, USER_AGENT);
        assert_eq!(headers.last().unwrap().1, "Bearer tok");
        let without = hatch_headers("hatch_sess=FAKE", None);
        assert!(!without.iter().any(|(k, _)| k == "Authorization"));
    }

    #[test]
    fn rsc_parse_reference_and_fallback() {
        let text = "0:{\"a\":\"$@1\"}\n1:{\"success\":true,\"subscription\":{\"tier\":\"pro\"}}\n2:not json";
        let value = parse_rsc_result(text).unwrap();
        assert_eq!(value["success"], true);
        let fallback = parse_rsc_result("0:{\"x\":1}\n1:{\"y\":2}").unwrap();
        assert_eq!(fallback["y"], 2);
    }

    #[test]
    fn action_id_regex_finds_hex_id() {
        let js = "x = createServerReference)(\"4029ac5c4bd03136487d91ebde5de39c64b35229b4\", foo, \"fetchSubscriptionAction\");";
        assert_eq!(
            find_action_in_js(js, "fetchSubscriptionAction").as_deref(),
            Some("4029ac5c4bd03136487d91ebde5de39c64b35229b4")
        );
        assert_eq!(
            find_action_in_js("nothing here", "fetchSubscriptionAction"),
            None
        );
    }

    #[test]
    fn subscription_from_result_rejects_null_and_falsy_subscription() {
        // Present-but-null used to count as success (key-exists
        // check); Python's `not res.get("subscription")` rejects it.
        let null_sub = serde_json::json!({"success": true, "subscription": null});
        assert_eq!(subscription_from_result(&null_sub), None);
        let empty_sub = serde_json::json!({"success": true, "subscription": {}});
        assert_eq!(subscription_from_result(&empty_sub), None);
        let missing_sub = serde_json::json!({"success": true});
        assert_eq!(subscription_from_result(&missing_sub), None);
        let failed = serde_json::json!({"success": false, "subscription": {"tier": "pro"}});
        assert_eq!(subscription_from_result(&failed), None);
        let ok = serde_json::json!({"success": true, "subscription": {"tier": "pro"}});
        assert_eq!(
            subscription_from_result(&ok),
            Some(serde_json::json!({"tier": "pro"}))
        );
    }

    #[test]
    fn redeem_code_validation_rejects_before_any_request() {
        // Too short / bad characters must fail without touching the network.
        assert!(redeem_invite_code("hatch_sess=FAKE", "abc").is_err());
        assert!(redeem_invite_code("hatch_sess=FAKE", "ABC-123").is_err());
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn no_proxy_matches_exact_suffix_and_wildcard() {
        assert!(no_proxy_matches("hatch.metaaivm.com", "localhost, hatch.metaaivm.com"));
        assert!(no_proxy_matches("a.hatch.metaaivm.com", ".metaaivm.com"));
        assert!(!no_proxy_matches("example.com", ".metaaivm.com"));
        assert!(no_proxy_matches("anything.example", "*"));
        assert!(!no_proxy_matches("example.com", ""));
    }

    #[test]
    fn build_connect_request_includes_auth_only_when_present() {
        let with_auth = build_connect_request("hatch.metaaivm.com", 443, Some("Basic dXNlcjpwYXNz"));
        assert!(with_auth.starts_with("CONNECT hatch.metaaivm.com:443 HTTP/1.1\r\n"));
        assert!(with_auth.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
        let without_auth = build_connect_request("hatch.metaaivm.com", 443, None);
        assert!(!without_auth.contains("Proxy-Authorization"));
    }
}
