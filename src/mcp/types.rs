//! JSON-RPC 2.0 types for the MCP stdio transport.
//!
//! These types implement a minimal subset of the Model Context Protocol:
//! - `initialize` / `notifications/initialized`
//! - `tools/list`
//! - `tools/call`
//!
//! Newline-delimited JSON (one JSON value per line) over stdin/stdout.
//! Logging goes to stderr exclusively so stdout stays clean for JSON-RPC.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 core ────────────────────────────────────────

/// A JSON-RPC 2.0 request from the client.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response sent to the client.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    /// JSON-RPC 2.0 requires an `id` on every response — `null` for
    /// requests where the id could not be determined (parse errors).
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    /// Build a successful response.
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response.
    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Standard JSON-RPC 2.0 error codes (public API — some may be unused internally).
#[allow(dead_code)]
pub const PARSE_ERROR: i32 = -32700;
#[allow(dead_code)]
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// ── MCP protocol types ───────────────────────────────────────

/// Server information sent in the `initialize` response.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Capabilities advertised by the server in the `initialize` response.
#[derive(Debug, Serialize)]
pub struct McpCapabilities {
    pub tools: Value, // empty object `{}` means tools are supported
}

/// The `initialize` response body.
#[derive(Debug, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    pub capabilities: McpCapabilities,
}

/// Definition of a single tool, returned by `tools/list`.
#[derive(Debug, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// The `tools/list` response body.
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDefinition>,
}

/// A piece of content in a `tools/call` result.
#[derive(Debug, Serialize)]
pub struct ToolContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// The `tools/call` response body.
#[derive(Debug, Serialize)]
pub struct ToolCallResult {
    pub content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "is_false")]
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// Helper for `skip_serializing_if` on `is_error`.
/// Returns `true` when the bool is `false`, so the field is omitted.
fn is_false(b: &bool) -> bool {
    !*b
}

impl ToolCallResult {
    pub fn ok(text: String) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text".into(),
                text,
            }],
            is_error: false,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text".into(),
                text: message.into(),
            }],
            is_error: true,
        }
    }
}
