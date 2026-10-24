//! Protobuf envelope types (writer B).
//!
//! Field numbers are EXACTLY the gateway spec §(b.2), decoded from
//! `assets/desc0.bin` (`noise_transport.proto`, package
//! `ingress_rev_proxy`) and `assets/desc1.bin` (`noise_envelope.proto`,
//! package `hatch.noise`). Proto3 `optional` fields map to `Option<T>`.
//! The cross-check test below decodes both descriptor files with
//! `prost_types::FileDescriptorProto` and asserts every field number in
//! this module matches the descriptors, so a drift fails the test suite
//! instead of silently breaking the wire format.

/// Transport frame wrapping one (possibly chunked) service payload.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NoiseTransportFrame {
    #[prost(int64, optional, tag = "1")]
    pub chunk_id: Option<i64>,
    #[prost(uint32, optional, tag = "2")]
    pub chunk_index: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub total_chunks: Option<u32>,
    #[prost(bytes, optional, tag = "4")]
    pub payload: Option<Vec<u8>>,
}

/// Service addressed by a frame's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ServiceType {
    Daemon = 0,
    Sentinel = 1,
    Vault = 2,
    Authd = 3,
}

/// Outbound envelope: which service should receive the inner frame bytes.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ServiceRequest {
    #[prost(enumeration = "ServiceType", optional, tag = "1")]
    pub service: Option<i32>,
    #[prost(bytes, optional, tag = "2")]
    pub payload: Option<Vec<u8>>,
}

/// Inbound envelope: inner frame bytes.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ServiceResponse {
    #[prost(bytes, optional, tag = "1")]
    pub payload: Option<Vec<u8>>,
}

/// One of the four kinds a `ServiceFrame` can carry.
#[derive(Clone, PartialEq, ::prost::Oneof)]
pub enum ServiceFrameKind {
    #[prost(message, tag = "2")]
    Request(ApplicationRequest),
    #[prost(message, tag = "3")]
    Response(ApplicationResponse),
    #[prost(message, tag = "4")]
    BodyChunk(BodyChunk),
    #[prost(message, tag = "5")]
    Reset(Reset),
}

/// A single stream frame: request, response, body chunk, or reset.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ServiceFrame {
    #[prost(int64, optional, tag = "1")]
    pub stream_id: Option<i64>,
    #[prost(oneof = "ServiceFrameKind", tags = "2, 3, 4, 5")]
    pub kind: Option<ServiceFrameKind>,
}

/// One HTTP-like header.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Header {
    #[prost(string, optional, tag = "1")]
    pub key: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub value: Option<String>,
}

/// Outbound application request. `end_body` is always `true` on send
/// (the Python client never sends chunked requests).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ApplicationRequest {
    #[prost(string, optional, tag = "1")]
    pub verb: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
    #[prost(message, repeated, tag = "3")]
    pub headers: Vec<Header>,
    #[prost(bytes, optional, tag = "4")]
    pub body: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "5")]
    pub end_body: Option<bool>,
}

/// Inbound application response head.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ApplicationResponse {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
    #[prost(message, repeated, tag = "2")]
    pub headers: Vec<Header>,
    #[prost(bytes, optional, tag = "3")]
    pub body: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "4")]
    pub end_body: Option<bool>,
}

/// Continuation body for a response.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BodyChunk {
    #[prost(bytes, optional, tag = "1")]
    pub data: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "2")]
    pub end_body: Option<bool>,
}

/// Stream reset reason codes (nested enum `Reset.Code` in the proto).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ResetCode {
    CodeUnspecified = 0,
    Cancelled = 1,
    Timeout = 2,
    ProtocolError = 3,
    RefusedStream = 4,
    InternalError = 5,
    ServiceUnavailable = 6,
}

/// Stream reset.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Reset {
    #[prost(enumeration = "ResetCode", optional, tag = "1")]
    pub code: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use prost_types::FileDescriptorProto;
    use std::collections::HashMap;

    fn descriptor(bytes: &[u8]) -> FileDescriptorProto {
        FileDescriptorProto::decode(bytes).expect("descriptor must decode")
    }

    /// (message name, field name) -> field number, from a descriptor file.
    fn field_numbers(fd: &FileDescriptorProto) -> HashMap<(&str, &str), i32> {
        let mut out = HashMap::new();
        for message in &fd.message_type {
            for field in &message.field {
                out.insert(
                    (
                        message.name.as_deref().unwrap_or(""),
                        field.name.as_deref().unwrap_or(""),
                    ),
                    field.number.unwrap_or(-1),
                );
            }
            for nested in &message.nested_type {
                for field in &nested.field {
                    out.insert(
                        (
                            nested.name.as_deref().unwrap_or(""),
                            field.name.as_deref().unwrap_or(""),
                        ),
                        field.number.unwrap_or(-1),
                    );
                }
            }
        }
        out
    }

    fn enum_values(fd: &FileDescriptorProto) -> HashMap<(&str, &str), i32> {
        let mut out = HashMap::new();
        for e in &fd.enum_type {
            for value in &e.value {
                out.insert(
                    (
                        e.name.as_deref().unwrap_or(""),
                        value.name.as_deref().unwrap_or(""),
                    ),
                    value.number.unwrap_or(-1),
                );
            }
        }
        for message in &fd.message_type {
            for e in &message.enum_type {
                for value in &e.value {
                    out.insert(
                        (
                            e.name.as_deref().unwrap_or(""),
                            value.name.as_deref().unwrap_or(""),
                        ),
                        value.number.unwrap_or(-1),
                    );
                }
            }
        }
        out
    }

    #[test]
    fn descriptors_match_struct_field_numbers() {
        let desc0 = descriptor(include_bytes!("../assets/desc0.bin"));
        let desc1 = descriptor(include_bytes!("../assets/desc1.bin"));
        assert_eq!(desc0.name.as_deref(), Some("noise_transport.proto"));
        assert_eq!(desc1.name.as_deref(), Some("noise_envelope.proto"));

        let f0 = field_numbers(&desc0);
        assert_eq!(f0[&("NoiseTransportFrame", "chunk_id")], 1);
        assert_eq!(f0[&("NoiseTransportFrame", "chunk_index")], 2);
        assert_eq!(f0[&("NoiseTransportFrame", "total_chunks")], 3);
        assert_eq!(f0[&("NoiseTransportFrame", "payload")], 4);

        let f1 = field_numbers(&desc1);
        assert_eq!(f1[&("ServiceRequest", "service")], 1);
        assert_eq!(f1[&("ServiceRequest", "payload")], 2);
        assert_eq!(f1[&("ServiceResponse", "payload")], 1);
        assert_eq!(f1[&("ServiceFrame", "stream_id")], 1);
        assert_eq!(f1[&("ServiceFrame", "request")], 2);
        assert_eq!(f1[&("ServiceFrame", "response")], 3);
        assert_eq!(f1[&("ServiceFrame", "body_chunk")], 4);
        assert_eq!(f1[&("ServiceFrame", "reset")], 5);
        assert_eq!(f1[&("Header", "key")], 1);
        assert_eq!(f1[&("Header", "value")], 2);
        assert_eq!(f1[&("ApplicationRequest", "verb")], 1);
        assert_eq!(f1[&("ApplicationRequest", "path")], 2);
        assert_eq!(f1[&("ApplicationRequest", "headers")], 3);
        assert_eq!(f1[&("ApplicationRequest", "body")], 4);
        assert_eq!(f1[&("ApplicationRequest", "end_body")], 5);
        assert_eq!(f1[&("ApplicationResponse", "status")], 1);
        assert_eq!(f1[&("ApplicationResponse", "headers")], 2);
        assert_eq!(f1[&("ApplicationResponse", "body")], 3);
        assert_eq!(f1[&("ApplicationResponse", "end_body")], 4);
        assert_eq!(f1[&("BodyChunk", "data")], 1);
        assert_eq!(f1[&("BodyChunk", "end_body")], 2);
        assert_eq!(f1[&("Reset", "code")], 1);
        assert_eq!(f1[&("Reset", "reason")], 2);

        let enums = enum_values(&desc1);
        assert_eq!(enums[&("ServiceType", "SERVICE_DAEMON")], 0);
        assert_eq!(enums[&("ServiceType", "SERVICE_SENTINEL")], 1);
        assert_eq!(enums[&("ServiceType", "SERVICE_VAULT")], 2);
        assert_eq!(enums[&("ServiceType", "SERVICE_AUTHD")], 3);
        assert_eq!(enums[&("Code", "CODE_UNSPECIFIED")], 0);
        assert_eq!(enums[&("Code", "CANCELLED")], 1);
        assert_eq!(enums[&("Code", "TIMEOUT")], 2);
        assert_eq!(enums[&("Code", "PROTOCOL_ERROR")], 3);
        assert_eq!(enums[&("Code", "REFUSED_STREAM")], 4);
        assert_eq!(enums[&("Code", "INTERNAL_ERROR")], 5);
        assert_eq!(enums[&("Code", "SERVICE_UNAVAILABLE")], 6);

        // And the Rust enum discriminants agree with the descriptor values.
        assert_eq!(ServiceType::Daemon as i32, 0);
        assert_eq!(ServiceType::Sentinel as i32, 1);
        assert_eq!(ServiceType::Vault as i32, 2);
        assert_eq!(ServiceType::Authd as i32, 3);
        assert_eq!(ResetCode::CodeUnspecified as i32, 0);
        assert_eq!(ResetCode::ServiceUnavailable as i32, 6);
    }

    #[test]
    fn envelope_roundtrip_preserves_fields() {
        let frame = ServiceFrame {
            stream_id: Some(7),
            kind: Some(ServiceFrameKind::Request(ApplicationRequest {
                verb: Some("GET".to_string()),
                path: Some("/chat/history?limit=1".to_string()),
                headers: vec![Header {
                    key: Some("x-request-id".to_string()),
                    value: Some("abc".to_string()),
                }],
                body: Some(Vec::new()),
                end_body: Some(true),
            })),
        };
        let bytes = frame.encode_to_vec();
        let decoded = ServiceFrame::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, frame);

        let transport = NoiseTransportFrame {
            chunk_id: Some(-42),
            chunk_index: Some(0),
            total_chunks: Some(1),
            payload: Some(b"payload".to_vec()),
        };
        let bytes = transport.encode_to_vec();
        assert_eq!(
            NoiseTransportFrame::decode(bytes.as_slice()).unwrap(),
            transport
        );
    }
}
