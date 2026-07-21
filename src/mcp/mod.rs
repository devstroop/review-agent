mod tools;
mod types;

use crate::config::Settings;
use crate::engine::ReviewEngine;
use crate::error::Result;
use crate::mcp::tools::{
    handle_review_diff, handle_review_files, handle_review_pr, tool_definitions,
};
use crate::mcp::types::*;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

/// Maximum bytes to read per JSON-RPC line from stdin.
/// Prevents memory exhaustion from an attacker sending data without a newline.
/// We enforce this per-line by reading byte-by-byte into a `Vec<u8>` with a
/// maximum capacity — this avoids `take()` which would cap total session input.
const MAX_LINE_LENGTH: usize = 1 << 20; // 1 MiB

pub async fn run(settings: &Settings) -> Result<()> {
    eprintln!("[mcp] Starting review-agent MCP server");

    let engine = ReviewEngine::new(settings)?;
    let tool_defs = tool_definitions();
    let initialized = Arc::new(AtomicBool::new(false));

    let mut stdout = tokio::io::stdout();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::with_capacity(4096);

    loop {
        line.clear();

        // Read byte-by-byte up to the per-line limit to prevent OOM from
        // an attacker sending data without a newline.  The per-line bound
        // does NOT limit cumulative session input.
        //
        // We use read_until with a limit guard rather than take() because
        // take() caps total bytes across all reads, causing early shutdown.
        let mut buf = Vec::with_capacity(MAX_LINE_LENGTH.min(4096));
        let n_read = loop {
            let byte = {
                let mut b = [0u8; 1];
                match reader.read(&mut b).await {
                    Ok(0) => break 0,
                    Ok(_) => b[0],
                    Err(e) => {
                        eprintln!("[mcp] stdin read error: {e}");
                        return Err(crate::error::AgentError::Io(e));
                    }
                }
            };
            if buf.len() >= MAX_LINE_LENGTH {
                eprintln!(
                    "[mcp] line exceeds safety limit of {MAX_LINE_LENGTH} bytes -- disconnecting"
                );
                return Err(crate::error::AgentError::Io(std::io::Error::other(
                    "line too long",
                )));
            }
            buf.push(byte);
            if byte == b'\n' {
                break buf.len();
            }
        };

        if n_read == 0 {
            eprintln!("[mcp] stdin EOF -- shutting down");
            break;
        }

        match std::str::from_utf8(&buf) {
            Ok(s) => line.push_str(s.trim_end_matches(['\n', '\r'])),
            Err(_) => {
                eprintln!("[mcp] invalid UTF-8 on stdin -- disconnecting");
                return Err(crate::error::AgentError::Io(std::io::Error::other(
                    "invalid UTF-8 on stdin",
                )));
            }
        }

        let trimmed = line.trim().to_string();

        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&trimmed) {
            Ok(r) => r,
            Err(e) => {
                // JSON-RPC 2.0 §8.1: the server MUST reply with a parse
                // error for malformed JSON, using id = null.
                let resp = JsonRpcResponse::err(None, PARSE_ERROR, format!("{e}"));
                if write_line(&mut stdout, &resp).await.is_err() {
                    eprintln!("[mcp] stdout write failed -- shutting down");
                    return Err(crate::error::AgentError::Io(std::io::Error::other(
                        "stdout write failed",
                    )));
                }
                continue;
            }
        };

        let is_notification = request.id.is_none();
        let response = dispatch(&request, &engine, &tool_defs, &initialized).await;

        if is_notification {
            if let Err((code, ref msg)) = response {
                eprintln!(
                    "[mcp] notification failed -- method={}, code={}, error={msg}",
                    request.method, code
                );
            }
            continue;
        }

        let id = request.id.clone().unwrap();
        let response = match response {
            Ok(result_value) => JsonRpcResponse::ok(Some(id), result_value),
            Err((code, msg)) => JsonRpcResponse::err(Some(id), code, msg),
        };

        if write_line(&mut stdout, &response).await.is_err() {
            eprintln!("[mcp] stdout write failed -- shutting down");
            return Err(crate::error::AgentError::Io(std::io::Error::other(
                "stdout write failed",
            )));
        }
    }

    eprintln!("[mcp] Server shut down");
    Ok(())
}

async fn dispatch(
    request: &JsonRpcRequest,
    engine: &ReviewEngine,
    tool_defs: &[(String, String, Value)],
    initialized: &AtomicBool,
) -> std::result::Result<Value, (i32, String)> {
    match request.method.as_str() {
        "initialize" => handle_initialize(request.params.as_ref()),
        "notifications/initialized" => {
            initialized.store(true, Ordering::Release);
            Ok(serde_json::Value::Null)
        }
        "tools/list" | "tools/call" => {
            if !initialized.load(Ordering::Acquire) {
                return Err((INVALID_REQUEST, "Server not yet initialized -- send initialize + notifications/initialized first".into()));
            }
            if request.method == "tools/list" {
                handle_tools_list(tool_defs)
            } else {
                handle_tools_call(request.params.as_ref(), engine).await
            }
        }
        "ping" => Ok(serde_json::Value::Null),
        _ => Err((
            METHOD_NOT_FOUND,
            format!("unknown method: {}", request.method),
        )),
    }
}

async fn write_line(
    stdout: &mut tokio::io::Stdout,
    resp: &JsonRpcResponse,
) -> std::result::Result<(), std::io::Error> {
    let json = serde_json::to_string(resp)
        .map_err(|e| std::io::Error::other(format!("Response serialization failed: {e}")))?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

fn handle_initialize(params: Option<&Value>) -> std::result::Result<Value, (i32, String)> {
    // Validate client protocol version if provided.
    if let Some(client_version) = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
    {
        // Accept any 2024-xx-xx version (the spec uses 2024-11-05).
        // This is forward-compatible with minor revisions of the
        // 2024 line.  Clients on older or newer lines will be
        // rejected with a clear error.
        if !client_version.starts_with("2024-") {
            return Err((
                INVALID_REQUEST,
                format!(
                    "Unsupported protocol version '{client_version}': review-agent supports 2024-xx-xx"
                ),
            ));
        }
    }

    let value = serde_json::to_value(InitializeResult {
        protocol_version: "2024-11-05".into(),
        server_info: ServerInfo {
            name: "review-agent".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        capabilities: McpCapabilities {
            tools: serde_json::json!({}),
        },
    })
    .map_err(|e| (INTERNAL_ERROR, format!("Serialization failed: {e}")))?;
    Ok(value)
}

fn handle_tools_list(
    tool_defs: &[(String, String, Value)],
) -> std::result::Result<Value, (i32, String)> {
    let tools: Vec<ToolDefinition> = tool_defs
        .iter()
        .map(|(name, description, input_schema)| ToolDefinition {
            name: name.clone(),
            description: description.clone(),
            input_schema: input_schema.clone(),
        })
        .collect();
    let value = serde_json::to_value(ToolsListResult { tools })
        .map_err(|e| (INTERNAL_ERROR, format!("Serialization failed: {e}")))?;
    Ok(value)
}

async fn handle_tools_call(
    params: Option<&Value>,
    engine: &ReviewEngine,
) -> std::result::Result<Value, (i32, String)> {
    let params = params.ok_or((
        INVALID_PARAMS,
        "missing params: expected object with 'name' and 'arguments'".into(),
    ))?;

    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, "missing 'name' in params".into()))?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let result: std::result::Result<Value, String> = match name {
        "review_pr" => handle_review_pr(engine, arguments).await,
        "review_diff" => handle_review_diff(engine, arguments).await,
        "review_files" => handle_review_files(engine, arguments).await,
        _ => return Err((METHOD_NOT_FOUND, format!("unknown tool: {name}"))),
    };

    match result {
        Ok(value) => {
            let text = serde_json::to_string(&value)
                .unwrap_or_else(|e| format!("failed to serialize tool result: {e}"));
            let tr = ToolCallResult::ok(text);
            serde_json::to_value(&tr).map_err(|e| (INTERNAL_ERROR, format!("{e}")))
        }
        Err(msg) => {
            let tr = ToolCallResult::err(&msg);
            serde_json::to_value(&tr).map_err(|e| (INTERNAL_ERROR, format!("{e}")))
        }
    }
}
