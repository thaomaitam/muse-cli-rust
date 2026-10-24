# Gateway protocol

Implementation: `src/gateway.rs`, `src/noise.rs`, `src/proto.rs`,
`src/routes.rs`, `src/config.rs`. The Python reference is
`src/muse_cli/gateway.py` (read-only) and `docs/PROTOCOL.md` in the
original repo. Where this document and the code disagree, the code wins.

## Layering

```text
HTTPS (muse.ai, reqwest blocking, NOT through the tunnel)
  -> WebSocket wss://hatch.metaaivm.com/v1/noise?vm_id=…&auth_token=…
       (tungstenite + rustls, TLS verification ON)
  -> Noise XX handshake (snow, 3 messages, empty payloads)
  -> NoiseTransportFrame (desc0) carrying ServiceRequest/ServiceResponse
  -> ServiceFrame (desc1): request / response / body_chunk / reset
  -> ApplicationRequest / ApplicationResponse (HTTP-like verb/path/headers/body)
```

## Step 1: HTTPS auth (muse.ai)

All HTTPS calls use the same header order (`hatch_headers`,
`src/gateway.rs`): `User-Agent` (Chrome 153 UA string, see below),
`Referer: https://muse.ai/`, `Origin: https://muse.ai`,
`Sec-Fetch-Site: same-origin`, `Sec-Fetch-Mode: cors`,
`Sec-Fetch-Dest: empty`, `Accept: application/json`,
`Content-Type: application/json`, `Cookie: <cookies>`, plus
`Authorization: Bearer <access_token>` only when a token is given.
Each call uses a 20 s timeout.

Order in `Gateway::connect` (`src/gateway.rs`):

1. Load cookies (`config::load_cookies`). Empty file -> Auth error.
2. If `config.vm_id` is absent: `GET https://muse.ai/api/session`
   (cookies only) -> `vm_id`. A 200 without `vm_id` means the VM is
   unavailable: that is a `Gateway(-1)` error, NOT an auth error.
   If `vm_id` is present, the session call is skipped entirely.
3. `POST https://muse.ai/api/auth/check` (cookies, empty body) ->
   `access_token`. Non-200 or JSON without `access_token` -> Auth error.
4. `POST https://muse.ai/api/hatch/token` with `Authorization: Bearer`
   and body `{"vmAddress": "wss://<vm_id>.metaaivm.com/", "vmName": <vm_id>}`
   -> hatch `token`. Non-200 or JSON without `token` -> Auth error.
5. WebSocket URL:
   `wss://hatch.metaaivm.com/v1/noise?vm_id=<vm_id>&auth_token=<hatch_token>`
   (the token is percent-encoded as a path param; it is never put in
   an error message).

Other direct HTTPS calls (not through the tunnel): `wake`
(`POST /api/hatch/vm/wake`), `invite` read (`GET /api/hatch/invite`),
`invite --create` (`POST /api/hatch/invite`, empty body),
`redeem` (`POST /api/hatch/invite-code/redeem`), `usage` (fetches
`/settings` HTML and scans JS bundles for `fetchSubscriptionAction`
— see `docs/parity.md` for the `subscription_action_id` fallback chain).
`Gateway::connect_with_transport` skips all HTTPS auth and goes straight
to the Noise handshake over the injected transport; it exists for the
in-memory mock-gateway tests only.

## Step 2: WebSocket + Noise

- Host: `hatch.metaaivm.com` (`GATEWAY_HOST`).
- Client role: Noise initiator, pattern
  `Noise_XX_25519_AESGCM_SHA256` (`src/noise.rs`), fresh random 32-byte
  static key per connection, empty payloads for all three handshake
  messages.
- WebSocket handshake headers on the proxy path (and the equivalent
  defaults on the direct path): `Host`, `Connection: Upgrade`,
  `Upgrade: websocket`, `Sec-WebSocket-Version: 13`,
  `Sec-WebSocket-Key` (16 fresh random bytes, base64), `User-Agent`
  (`Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36
  (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36`),
  `Origin: https://muse.ai`.
- **Chrome TLS fingerprint is NOT reproduced.** Python uses
  `curl_cffi impersonate="chrome"`; `reqwest`/`tungstenite` send a
  rustls fingerprint instead. Live parity is therefore unproven, not
  claimed.
- **WebSocket connect timeout: none.** Python passes `timeout=20` to
  the WS connect; `tungstenite::connect` takes no timeout, so this
  build does not enforce one at connect time. Later reads are blocking,
  as in Python.

### Egress proxy (WebSocket only)

`src/gateway.rs` (`proxy_for_host`, `connect_via_proxy`) — added because
tungstenite connects directly while Python's `curl_cffi` honours the
proxy environment; on a host whose only outbound route is an HTTP egress
proxy, the direct handshake fails with `InvalidContentType`.

- `NO_PROXY`/`no_proxy` is checked first: exact host, suffix match, and
  `*` (entries may carry a `:port` suffix, which is stripped). A match
  means direct connection, no proxy.
- Otherwise the first usable proxy variable wins, in order:
  `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, `all_proxy`, `HTTP_PROXY`,
  `http_proxy`. Empty values are skipped; unparseable URLs are skipped.
- **Only plain-`http://` proxies are supported.** An `https://` proxy
  URL is skipped (a second TLS layer is out of scope); if no variable
  yields an `http://` proxy, the direct path is used.
- The tunnel is a plain-TCP connection to the proxy, then
  `CONNECT <gateway-host>:443 HTTP/1.1` with `Host:` and, only when the
  proxy URL has userinfo, `Proxy-Authorization: Basic <base64(user:pass)>`.
  The CONNECT response must be `200` (only the status line is reported
  on failure; the body is never needed). Responses over 8192 bytes are
  rejected.
- **Security:** proxy credentials from the URL userinfo are used only
  for the CONNECT header, are never logged, never put in an error
  message, and never sent to the gateway host. TLS + the WebSocket
  handshake then run over the tunnel via `tungstenite::client_tls`.
- On a host with no proxy configured, this code is inert: `connect`
  takes the direct path unchanged.
- Note: this proxy handling covers the WebSocket transport only.
  Whether `reqwest` (HTTPS) honours the same proxy environment is
  reqwest's default behaviour, not something this code configures —
  do not claim parity for the HTTPS path from this section.

## Step 3: Protobuf envelopes

Descriptors (data, not code): `assets/desc0.bin` =
`noise_transport.proto`, package `ingress_rev_proxy`;
`assets/desc1.bin` = `noise_envelope.proto`, package `hatch.noise`.
`src/proto.rs` mirrors them exactly, and the unit test
`descriptors_match_struct_field_numbers` decodes both descriptors with
`prost_types::FileDescriptorProto` and asserts every field number, so
drift fails the test suite instead of breaking the wire format silently.

| Message (file) | Fields (number: name: type) |
|---|---|
| `NoiseTransportFrame` (desc0) | 1: `chunk_id` int64 opt; 2: `chunk_index` uint32 opt; 3: `total_chunks` uint32 opt; 4: `payload` bytes opt |
| `ServiceRequest` (desc1) | 1: `service` enum opt; 2: `payload` bytes opt |
| `ServiceResponse` (desc1) | 1: `payload` bytes opt |
| `ServiceFrame` (desc1) | 1: `stream_id` int64 opt; oneof 2: `request`, 3: `response`, 4: `body_chunk`, 5: `reset` |
| `Header` (desc1) | 1: `key` string opt; 2: `value` string opt |
| `ApplicationRequest` (desc1) | 1: `verb` string opt; 2: `path` string opt; 3: `headers` repeated; 4: `body` bytes opt; 5: `end_body` bool opt |
| `ApplicationResponse` (desc1) | 1: `status` int32 opt; 2: `headers` repeated; 3: `body` bytes opt; 4: `end_body` bool opt |
| `BodyChunk` (desc1) | 1: `data` bytes opt; 2: `end_body` bool opt |
| `Reset` (desc1) | 1: `code` enum opt; 2: `reason` string opt |

Enums: `ServiceType` Daemon=0, Sentinel=1, Vault=2, Authd=3
(route `service` strings map the same way; unknown/missing -> 0/Daemon).
`Reset.Code`: Unspecified=0, Cancelled=1, Timeout=2, ProtocolError=3,
RefusedStream=4, InternalError=5, ServiceUnavailable=6.

Wire rules (`src/gateway.rs`):

- Outbound: one `ApplicationRequest` per call, `end_body` always `true`
  (Python never sends chunked requests), wrapped in one
  `NoiseTransportFrame` with `total_chunks=1` and a random 8-byte
  little-endian signed `chunk_id` — no splitting.
- Stream ids start at 1 and increase monotonically, shared by unary and
  subscription streams on one connection.
- Inbound transport frames are reassembled by `chunk_id` and sorted by
  `chunk_index` before decoding (defensive: whether the server ever
  sends out-of-order chunks is **not verified live**).
- One lock guards Noise state and transport together, because Noise
  decrypt has state and two concurrent decrypts corrupt it.

## Step 4: Calling a route

Routes: `assets/routes.json`, 258 entries, each with `method`, `http`,
`path`, `service`. `routes::lookup(method)` returns the route or the
error `unknown method '<method>' (see routes.json for the 258 known methods)`.
`build_request` substitutes `{param}` path params (percent-encoded),
appends query params, and sets headers. `Gateway::request` sends the
envelope and reads until `end_body`.

`call_json` semantics (the normal path for most commands):

1. Empty payload -> `{}`.
2. Non-JSON payload -> `Gateway(-1, "non-JSON response: <first 200 bytes>")`.
3. JSON object with `ok == false` -> `Gateway(-1, "api error: <error>")`.
4. JSON object with a `result` field -> that value (the whole object
   is NOT returned).
5. Otherwise -> the whole JSON value.

`call_bytes` returns the raw payload (used by `media.raw` downloads).
`download_bytes` resolves a media reference via `media::media_path`
and calls the synthetic `media.raw` route (`GET <resolved path>`,
service 0) through the tunnel.

Unary `request` quirks, kept from Python:

- Frames of other streams are **discarded, not buffered**. That is why
  auto-approval and the history re-read in `wait.rs` use a separate
  connection.
- A `Reset` frame -> `Gateway(-1)` (the reset reason is surfaced).
- Status outside 200..300 -> `Gateway` with the payload truncated to
  500 bytes.
- No response by the deadline -> `Timeout`.
- If a response head arrived but `end_body` never arrives before the
  deadline, the accumulated partial payload is returned **without an
  error** (Python quirk, kept deliberately).

`subscribe` (used by `wait`/`watch`/`chat`/`send --wait`):

- A reader thread pushes `ServiceFrame`s into a channel; the main loop
  polls every 2 s.
- The idle timer is reset **only** by frames of the subscribed stream;
  other-stream frames are discarded without resetting it.
- Records are non-blank lines after newline-splitting; `on_record`
  returning `true` halts immediately (the record was already appended).
- A trailing non-blank buffer becomes a final record. A `reset`, or
  `end_body` on an empty buffer, stops the loop without error.
- The subscription reader thread is a daemon-style thread: `close()`
  does not wake a thread blocked in `recv_bytes` (same caveat as Python).

## What is NOT in this document (because it is not verified)

- The full body/response schema of the routes the Python code never
  calls: `routes.json` gives method/http/path/service only. There
  are 231 such routes (258 total minus the 27 that appear as
  literals in the original and Rust code, counted 2026-10-24).
- Whether the server actually splits responses into multiple
  `NoiseTransportFrame`s, and whether it ever sends chunks out of order.
- The exact extra WebSocket headers `curl_cffi` sends beyond
  `impersonate` + `verify`, and the Chrome TLS fingerprint.
- The complete shape of every `chat.subscribe` event beyond what
  `wait.rs` and the mock tests cover. See `docs/live-verification-checklist.md`.
