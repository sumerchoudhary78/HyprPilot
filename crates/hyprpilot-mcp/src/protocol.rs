//! JSON-RPC 2.0 envelope + the small subset of MCP types we need.
//!
//! MCP itself is JSON-RPC 2.0 over a stream transport (stdio here). The
//! method names and result shapes are dictated by the MCP spec; we model
//! only what `initialize`, `tools/list`, and `tools/call` require.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- JSON-RPC 2.0 envelope --------------------------------------------------

/// A JSON-RPC 2.0 request or notification. `id` is absent on notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC 2.0 response. Exactly one of `result` / `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(rename = "jsonrpc")]
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn err(id: Value, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: code.code(),
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn err_with_data(
        id: Value,
        code: ErrorCode,
        message: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: code.code(),
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 standard error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    /// MCP / application-level: tool execution failed.
    ToolExecution,
    /// MCP / application-level: tool not permitted by active profile.
    CapabilityDenied,
}

impl ErrorCode {
    pub fn code(self) -> i32 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            // -32000 to -32099 are reserved for implementation-defined errors.
            ErrorCode::ToolExecution => -32000,
            ErrorCode::CapabilityDenied => -32001,
        }
    }
}

// ---- MCP shapes -------------------------------------------------------------

/// MCP protocol version this server speaks. The spec uses date-stamped
/// versions; this is the latest stable as of HyprPilot v0.2.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default, rename = "clientInfo")]
    pub client_info: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Empty object signals "tools are supported". A `listChanged: true`
    /// member is reserved for future use.
    pub tools: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<Tool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(default, rename = "isError", skip_serializing_if = "is_false")]
    pub is_error: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One piece of structured output from a tool. We emit text and image
/// blocks; the MCP spec also defines resource blocks which we don't use.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        /// Base64-encoded image bytes.
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into() }
    }

    /// Build an `Image` content block from raw bytes. The bytes are
    /// base64-encoded (standard alphabet, with padding) per the MCP spec.
    pub fn image(bytes: &[u8], mime: impl Into<String>) -> Self {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Content::Image { data, mime_type: mime.into() }
    }
}
