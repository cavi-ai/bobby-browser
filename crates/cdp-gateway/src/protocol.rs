use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_IN_FLIGHT_REQUESTS: usize = 128;
pub const MAX_QUEUED_EVENTS: usize = 1024;
const MAX_REQUEST_ID: u64 = 9_007_199_254_740_991;
const MAX_METHOD_BYTES: usize = 256;

#[derive(Debug, Clone, Deserialize)]
pub struct CdpRequest {
    pub id: u64,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
    pub method: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl CdpRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            session_id: None,
            method: method.into(),
            params,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), CdpError> {
        if self.id == 0
            || self.id > MAX_REQUEST_ID
            || self.method.is_empty()
            || self.method.len() > MAX_METHOD_BYTES
        {
            return Err(CdpError::new(
                CdpErrorCode::InvalidRequest,
                "invalid CDP request",
            ));
        }
        if !self.params.is_object() {
            return Err(CdpError::new(
                CdpErrorCode::InvalidParams,
                "params must be an object",
            ));
        }
        if self
            .session_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            return Err(CdpError::new(
                CdpErrorCode::InvalidRequest,
                "invalid sessionId",
            ));
        }
        Ok(())
    }
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdpResponse {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sessionId")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<CdpError>,
}

impl CdpResponse {
    pub fn success(request: &CdpRequest, result: Value) -> Self {
        Self {
            id: request.id,
            session_id: request.session_id.clone(),
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(request: &CdpRequest, error: CdpError) -> Self {
        Self {
            id: request.id,
            session_id: request.session_id.clone(),
            result: None,
            error: Some(error),
        }
    }

    pub fn result(&self) -> Option<&Value> {
        self.result.as_ref()
    }

    pub fn error(&self) -> Option<&CdpError> {
        self.error.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdpError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl CdpError {
    pub fn new(code: CdpErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code as i32,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum CdpErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    RuntimeFailure = -32000,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sessionId")]
    pub session_id: Option<String>,
}

pub fn parse_frame(frame: &[u8]) -> Result<CdpRequest, CdpError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(CdpError::new(
            CdpErrorCode::InvalidRequest,
            "CDP frame exceeds 1 MiB",
        ));
    }
    let value: Value = serde_json::from_slice(frame)
        .map_err(|_| CdpError::new(CdpErrorCode::ParseError, "invalid JSON"))?;
    if !value.is_object() {
        return Err(CdpError::new(
            CdpErrorCode::InvalidRequest,
            "CDP request must be an object",
        ));
    }
    let request: CdpRequest = serde_json::from_value(value)
        .map_err(|_| CdpError::new(CdpErrorCode::InvalidRequest, "invalid CDP request"))?;
    request.validate()?;
    Ok(request)
}
