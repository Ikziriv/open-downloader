//! Lightweight Model Context Protocol (MCP) JSON-RPC 2.0 endpoint (`/mcp`),
//! allowing AI agents to resolve URLs, queue downloads, and control tasks.

use axum::Json;
use axum::extract::State;
use od_core::{CancellationToken, ResolveRequest, SpeedTestConfig, TaskId, run_speedtest};
use serde_json::{Value, json};

use crate::{ApiState, CreateTaskPayload, create_task_from_payload};

#[derive(Debug, serde::Deserialize)]
pub(crate) struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

pub(crate) async fn handle_mcp(
    State(state): State<ApiState>,
    Json(req): Json<RpcRequest>,
) -> Json<Value> {
    let id = req.id.clone();
    let resp = match req.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "open-downloader",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "tools/list" => ok(id, json!({ "tools": tool_definitions() })),
        "tools/call" => handle_tool_call(&state, id, req.params).await,
        other => err(id, -32601, format!("method not found: {other}")),
    };
    Json(resp)
}

async fn handle_tool_call(state: &ApiState, id: Value, params: Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return err(id, -32602, "missing tool name".into());
    };
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let outcome: Result<Value, String> = match name {
        "resolve_url" => match serde_json::from_value::<ResolveRequest>(args) {
            Ok(req) => state
                .manager
                .resolve(&req)
                .await
                .map(|(r, res)| json!({ "request": r, "resource": res }))
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("invalid arguments: {e}")),
        },
        "create_download" => match serde_json::from_value::<CreateTaskPayload>(args) {
            Ok(payload) => create_task_from_payload(state, payload)
                .await
                .map(|rec| serde_json::to_value(rec).unwrap_or(Value::Null))
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("invalid arguments: {e}")),
        },
        "list_downloads" => {
            let list = state.manager.list().await;
            Ok(serde_json::to_value(list).unwrap_or(Value::Null))
        }
        "pause_download" => match parse_task_id(&args) {
            Ok(tid) => state
                .manager
                .pause(tid)
                .await
                .map(|()| json!({ "paused": tid }))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
        "resume_download" => match parse_task_id(&args) {
            Ok(tid) => state
                .manager
                .resume(tid)
                .await
                .map(|()| json!({ "resumed": tid }))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
        "remove_download" => match parse_task_id(&args) {
            Ok(tid) => {
                let delete_files = args
                    .get("delete_files")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                state
                    .manager
                    .remove(tid, delete_files)
                    .await
                    .map(|()| json!({ "removed": tid, "delete_files": delete_files }))
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e),
        },
        "run_speedtest" => {
            let cfg: SpeedTestConfig = serde_json::from_value(args).unwrap_or_default();
            let client = reqwest::Client::new();
            run_speedtest(&client, &cfg, CancellationToken::new(), None)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown tool: {other}")),
    };

    match outcome {
        Ok(data) => ok(
            id,
            json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&data).unwrap_or_default()
                }],
                "isError": false
            }),
        ),
        Err(msg) => ok(
            id,
            json!({
                "content": [{ "type": "text", "text": msg }],
                "isError": true
            }),
        ),
    }
}

fn parse_task_id(args: &Value) -> Result<TaskId, String> {
    args.get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `id` argument".into())
}

fn ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn err(id: Value, code: i32, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "resolve_url",
            "description": "Inspect a download URL and return its detected filename, size, category, and files without downloading.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "create_download",
            "description": "Create and enqueue a new download task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "filename": { "type": "string" },
                    "dest": { "type": "string" },
                    "connections": { "type": "integer" },
                    "rate_limit": { "type": "integer" },
                    "auto_start": { "type": "boolean" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "list_downloads",
            "description": "List all download tasks with their status, progress, speed, and ETA.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "pause_download",
            "description": "Pause an active download task while preserving resume state.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "resume_download",
            "description": "Resume a paused or failed download task.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "remove_download",
            "description": "Remove a download task and optionally delete its files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "delete_files": { "type": "boolean" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "run_speedtest",
            "description": "Measure Internet connection ping, jitter, download Mbps, upload Mbps, quality grade, and recommended parallel connections.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_upload": { "type": "boolean" }
                }
            }
        }
    ])
}
