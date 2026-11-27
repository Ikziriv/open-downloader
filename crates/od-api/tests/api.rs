use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use od_api::{ApiState, router};
use od_core::{FetcherRegistry, MemoryTaskStore, TaskManager, TaskRecord, TaskStatus};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

async fn serve_files(dir: &Path) -> String {
    let app = Router::new().fallback_service(ServeDir::new(dir));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

async fn spawn_api(default_dir: &Path, token: Option<&str>) -> String {
    let reg = FetcherRegistry::with_defaults(reqwest::Client::new());
    let mgr = TaskManager::new(reg, Arc::new(MemoryTaskStore), 2).unwrap();
    let mut state = ApiState::new(mgr, default_dir);
    if let Some(t) = token {
        state = state.with_token(t);
    }
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn auth_protects_api_while_health_is_public() {
    let out = tempfile::tempdir().unwrap();
    let api = spawn_api(out.path(), Some("top-secret")).await;
    let client = reqwest::Client::new();

    // Embedded Web Dashboard UI is served at GET /.
    let ui = client.get(format!("{api}/")).send().await.unwrap();
    assert_eq!(ui.status(), 200);
    assert!(ui.text().await.unwrap().contains("Open Downloader"));

    // Health endpoint is open.
    let health = client
        .get(format!("{api}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);

    // Missing token -> 401.
    let unauth = client
        .get(format!("{api}/api/v1/tasks"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);

    // Valid x-od-token -> 200.
    let ok = client
        .get(format!("{api}/api/v1/tasks"))
        .header("x-od-token", "top-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // Config endpoint roundtrip (GET -> PUT -> GET).
    let cfg_resp: Value = client
        .get(format!("{api}/api/v1/config"))
        .header("x-od-token", "top-secret")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut cfg = cfg_resp["config"].clone();
    cfg["downloads"]["connections_per_download"] = json!(12);
    let updated: Value = client
        .put(format!("{api}/api/v1/config"))
        .header("x-od-token", "top-secret")
        .json(&cfg)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["config"]["downloads"]["connections_per_download"], 12);
}

#[tokio::test]
async fn rest_resolve_create_poll_and_delete_roundtrip() {
    let payload = b"open-downloader-phase2-api-payload".repeat(1024);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("sample.bin"), &payload).unwrap();
    let files_url = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let api = spawn_api(out.path(), Some("tok")).await;
    let client = reqwest::Client::new();

    // 1. Resolve
    let resolved: Value = client
        .post(format!("{api}/api/v1/resolve"))
        .header("x-od-token", "tok")
        .json(&json!({ "url": format!("{files_url}/sample.bin") }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["resource"]["name"], "sample.bin");
    assert_eq!(resolved["resource"]["total_size"], payload.len() as u64);

    // 2. Create task
    let created: TaskRecord = client
        .post(format!("{api}/api/v1/tasks"))
        .header("x-od-token", "tok")
        .json(&json!({
            "url": format!("{files_url}/sample.bin"),
            "resource": resolved["resource"]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    // 3. Poll until Completed
    let mut final_rec = created.clone();
    for _ in 0..50 {
        final_rec = client
            .get(format!("{api}/api/v1/tasks/{}", created.id))
            .header("x-od-token", "tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if final_rec.status == TaskStatus::Completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(final_rec.status, TaskStatus::Completed);
    let output_path = final_rec.output_path.expect("output_path set");
    assert_eq!(std::fs::read(&output_path).unwrap(), payload);

    // 4. Delete with delete_files=true
    let del = client
        .delete(format!("{api}/api/v1/tasks/{}?delete_files=true", created.id))
        .header("x-od-token", "tok")
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    assert!(!output_path.exists());
}

#[tokio::test]
async fn legacy_add_and_mcp_tools_work_end_to_end() {
    let payload = b"mcp-and-legacy-add-test".to_vec();
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("hello.txt"), &payload).unwrap();
    let files_url = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let api = spawn_api(out.path(), None).await;
    let client = reqwest::Client::new();

    // Legacy POST /add (browser native messaging host compatibility)
    let add_resp = client
        .post(format!("{api}/add"))
        .json(&json!({ "url": format!("{files_url}/hello.txt") }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_resp.status(), 200);

    // MCP initialize
    let init: Value = client
        .post(format!("{api}/mcp"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "open-downloader");

    // MCP tools/list
    let tools: Value = client
        .post(format!("{api}/mcp"))
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tools["result"]["tools"].as_array().unwrap().len() >= 6);

    // MCP tools/call -> resolve_url
    let resolved: Value = client
        .post(format!("{api}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "resolve_url",
                "arguments": { "url": format!("{files_url}/hello.txt") }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["result"]["isError"], false);
    let text = resolved["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hello.txt"));

    // Batch import endpoint (POST /api/v1/tasks/batch)
    let batch: Value = client
        .post(format!("{api}/api/v1/tasks/batch"))
        .json(&json!({
            "text": format!("# sample batch\n{files_url}/hello.txt\n"),
            "auto_start": false
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(batch["created"].as_array().unwrap().len(), 1);
    assert!(batch["errors"].as_array().unwrap().is_empty());

    // CSV export endpoint
    let csv = client
        .get(format!("{api}/api/v1/tasks/export?format=csv"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(csv.starts_with("id,name,url,status,priority,downloaded,total,output_path"));
    assert!(csv.contains("hello.txt"));
}
