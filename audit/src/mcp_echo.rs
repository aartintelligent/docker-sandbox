//! `mcp-echo`: a minimal MCP server on standard input and output.
//!
//! It only exists so that `sbx mcp add/load/unload` can run against a local
//! server during the `coverage` suite. It speaks JSON-RPC 2.0, one message per
//! line, exposes a single `echo` tool and logs the methods it receives on
//! standard error.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

/// Protocol version announced when the client does not ask for one.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Serves requests read from standard input until it is closed.
///
/// # Errors
///
/// Fails if standard input or output cannot be used.
pub(crate) fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        eprintln!("[mcp-echo] {}", request["method"].as_str().unwrap_or("?"));
        if let Some(reply) = answer(&request) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// The reply to `request`, or `None` for a notification.
fn answer(request: &Value) -> Option<Value> {
    let id = request.get("id").filter(|id| !id.is_null())?.clone();
    let method = request["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": request["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "docker-sandbox-audit-echo", "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": [{
            "name": "echo",
            "description": "Returns the given text",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            },
        }]}),
        "tools/call" => json!({
            "content": [{
                "type": "text",
                "text": request["params"]["arguments"]["text"].as_str().unwrap_or_default(),
            }],
            "isError": false,
        }),
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_answered_and_notifications_ignored() {
        let reply =
            answer(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
                .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);

        let reply = answer(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "echo", "arguments": {"text": "hi"}}}))
        .unwrap();
        assert_eq!(reply["result"]["content"][0]["text"], "hi");

        assert!(
            answer(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none()
        );
        assert_eq!(
            answer(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap()["error"]["code"],
            -32601
        );
    }
}
