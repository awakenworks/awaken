//! A2A wire-version negotiation shared by discovery and request routing.

use axum::http::HeaderMap;

use crate::state::PushProtocolVersion;

pub(crate) const A2A_VERSION_HEADER: &str = "A2A-Version";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolVersion {
    V03,
    V1,
}

impl ProtocolVersion {
    pub(crate) fn push_version(self) -> PushProtocolVersion {
        match self {
            Self::V03 => PushProtocolVersion::V03,
            Self::V1 => PushProtocolVersion::V1,
        }
    }
}

pub(crate) fn negotiate_version(headers: &HeaderMap) -> Result<ProtocolVersion, String> {
    match headers
        .get(A2A_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        None | Some("") | Some("0.3") | Some("0.3.0") => Ok(ProtocolVersion::V03),
        Some("1.0") | Some("1.0.0") => Ok(ProtocolVersion::V1),
        Some(version) => Err(format!("unsupported A2A protocol version: {version}")),
    }
}
