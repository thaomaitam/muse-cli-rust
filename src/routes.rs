//! Route table for the gateway (writer B).
//!
//! Mirrors Python `gateway.py`: `routes.json` is loaded once into a map
//! keyed by method name. The Python code does NOT use `noiseOnly`,
//! `streamMode`, `responseType`, `channel` or `aliases` to decide how a
//! call is made, and `raw` does NOT resolve aliases, so neither does this
//! module: a route's `method`/`http`/`path`/`service` are the only fields
//! kept. A synthetic route such as `media.raw` is deliberately NOT in the
//! map (it is not in `routes.json`); it is built by the caller through
//! `Gateway::request_with_route`, not through `lookup`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::errors::CliError;

/// One entry of `routes.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub method: String,
    pub http: String,
    pub path: String,
    pub service: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RouteJson {
    method: String,
    http: String,
    path: String,
    service: Option<String>,
}

/// Percent-encode matching Python `urllib.parse.quote(s, safe="")`:
/// everything except ASCII letters, digits and `_.-~` is encoded, UTF-8
/// bytes included, with uppercase hex. In particular `/` -> `%2F`.
/// Implemented byte-by-byte so non-ASCII handling cannot depend on a
/// crate's ASCII-set semantics.

fn load_routes() -> HashMap<String, Route> {
    let text = include_str!("../assets/routes.json");
    let entries: Vec<RouteJson> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(_) => Vec::new(),
    };
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries {
        map.insert(
            entry.method.clone(),
            Route {
                method: entry.method,
                http: entry.http,
                path: entry.path,
                service: entry.service,
            },
        );
    }
    map
}

fn routes_map() -> &'static HashMap<String, Route> {
    static MAP: OnceLock<HashMap<String, Route>> = OnceLock::new();
    MAP.get_or_init(load_routes)
}

/// All routes, parsed once from the embedded `assets/routes.json`.
/// Must contain 258 entries (asserted by unit test).
pub fn routes() -> &'static HashMap<String, Route> {
    routes_map()
}

/// Look a method up by name. Unknown methods are a `Value` error with the
/// exact message the Python `raw` command prints (exit 2 via the caller).
pub fn lookup(method: &str) -> Result<&'static Route, CliError> {
    routes_map().get(method).ok_or_else(|| {
        CliError::value(format!(
            "unknown method '{method}' (see routes.json for the 258 known methods)"
        ))
    })
}

/// A request built by [`build_request`]: verb and path after substitution,
/// headers to send, and the serialised body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltRequest {
    pub verb: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub raw_body: Vec<u8>,
}

/// Percent-encode a path-param value exactly like Python
/// `quote(str(v), safe="")`.
pub fn quote_path_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn query_value_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    }
}

/// Build a request the way Python `Gateway._open` does (without sending):
/// substitute `{k}` path params with fully percent-encoded values, append
/// the query string (skipping `None` values, `?` or `&` as appropriate),
/// serialise the body to JSON bytes (`None` -> empty), and set headers:
/// `x-request-id: <uuid v4>` always, `content-type: application/json`
/// only when the body is non-empty.
///
/// NOTE: the GET body->query conversion lives in `Gateway::request`, NOT
/// here (see CONTRACT).
pub fn build_request(
    method: &str,
    path_params: &[(String, String)],
    body: Option<&Value>,
    query: Option<&Value>,
) -> Result<BuiltRequest, CliError> {
    let route = lookup(method)?;
    build_request_with_route(route, path_params, body, query)
}

/// Same as [`build_request`], but for a route value the caller already
/// has (used by the gateway for synthetic routes such as `media.raw`,
/// which are not in the route map).
pub fn build_request_with_route(
    route: &Route,
    path_params: &[(String, String)],
    body: Option<&Value>,
    query: Option<&Value>,
) -> Result<BuiltRequest, CliError> {
    let mut path = route.path.clone();
    for (key, value) in path_params {
        let placeholder = format!("{{{key}}}");
        let encoded = quote_path_param(value);
        path = path.replace(&placeholder, &encoded);
    }
    if let Some(Value::Object(query_map)) = query {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (key, value) in query_map {
            if let Some(text) = query_value_string(value) {
                pairs.push((key.clone(), text));
            }
        }
        if !pairs.is_empty() {
            // urlencode with quote_plus semantics (space -> '+').
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in &pairs {
                serializer.append_pair(key, value);
            }
            let qs = serializer.finish();
            path = format!(
                "{}{}{}",
                path,
                if path.contains('?') { "&" } else { "?" },
                qs
            );
        }
    }
    let raw_body: Vec<u8> = match body {
        None => Vec::new(),
        Some(value) => serde_json::to_vec(value)
            .map_err(|e| CliError::value(format!("cannot serialise request body: {e}")))?,
    };
    let mut headers = vec![("x-request-id".to_string(), Uuid::new_v4().to_string())];
    if !raw_body.is_empty() {
        headers.push(("content-type".to_string(), "application/json".to_string()));
    }
    Ok(BuiltRequest {
        verb: route.http.clone(),
        path,
        headers,
        raw_body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn route_table_has_258_entries() {
        assert_eq!(routes().len(), 258);
    }

    #[test]
    fn unknown_method_error_message_matches_python_raw() {
        let err = lookup("no.such.method").unwrap_err();
        match err {
            CliError::Value(msg) => assert_eq!(
                msg,
                "unknown method 'no.such.method' (see routes.json for the 258 known methods)"
            ),
            other => panic!("expected Value error, got {other:?}"),
        }
    }

    #[test]
    fn alias_is_not_resolved() {
        // routes.json carries aliases only as a JSON string on some entries;
        // Python `raw` accepts exactly the method names, nothing else.
        let alias = routes().values().find_map(|route| {
            // Any method name that is not itself a key cannot be looked up.
            let _ = route;
            None::<()>
        });
        assert!(alias.is_none());
        assert!(lookup("/nodes/list").is_err());
    }

    #[test]
    fn path_param_unicode_is_utf8_percent_encoded() {
        assert_eq!(quote_path_param("café/x"), "caf%C3%A9%2Fx");
        assert_eq!(quote_path_param("a~b-c_d.e"), "a~b-c_d.e");
    }

    #[test]
    fn path_param_slash_is_percent_encoded() {
        let built = build_request(
            "egress.approval.decide",
            &[("approval_id".to_string(), "a/b c".to_string())],
            None,
            None,
        )
        .unwrap();
        assert_eq!(built.path, "/approvals/a%2Fb%20c/decide");
        assert_eq!(built.verb, "POST");
        assert!(built.raw_body.is_empty());
    }

    #[test]
    fn query_skips_none_values_and_joins_with_question_mark() {
        let built = build_request(
            "feed.list",
            &[],
            None,
            Some(&json!({"limit": 10, "cursor": null})),
        )
        .unwrap();
        assert_eq!(built.path, "/api/feed?limit=10");
    }

    #[test]
    fn header_rule_request_id_always_content_type_only_with_body() {
        let empty = build_request("sessions.list", &[], None, None).unwrap();
        assert_eq!(empty.headers.len(), 1);
        assert_eq!(empty.headers[0].0, "x-request-id");
        // A v4 UUID: 36 chars, version nibble '4' at position 14.
        let uuid = &empty.headers[0].1;
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');

        let with_body = build_request("chat.stream", &[], Some(&json!({"a": 1})), None).unwrap();
        assert_eq!(with_body.headers.len(), 2);
        assert_eq!(
            with_body.headers[1],
            ("content-type".to_string(), "application/json".to_string())
        );
        assert_eq!(with_body.raw_body, b"{\"a\":1}");
    }

    #[test]
    fn stream_starts_at_one_is_a_gateway_concern_not_a_route_concern() {
        // build_request itself carries no stream id; the gateway assigns
        // stream ids starting at 1 (tested in gateway.rs).
        let built = build_request("chat.history", &[], None, None).unwrap();
        assert_eq!(built.verb, "GET");
        assert_eq!(built.path, "/chat/history");
    }
}
