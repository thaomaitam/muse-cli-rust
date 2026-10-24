pub struct NoiseTransportFrame {
    #[prost(int64, optional, tag = "1")]
    pub chunk_id: Option<i64>,
    #[prost(bytes, optional, tag = "4")]
    pub payload: Option<Vec<u8>>,
}

pub struct ServiceRequest {
    #[prost(enumeration = "ServiceType", optional, tag = "1")]
    pub service: Option<i32>,
}

pub enum ServiceType {
    Daemon = 0,
    Sentinel = 1,
}
