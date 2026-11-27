use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use od_core::{
    BoxFuture, Category, CreateTaskRequest, DownloadOptions, DownloadSpec, Error, FetcherRegistry,
    FileTaskStore, MemoryTaskStore, ResolveRequest, ResourceInterceptor, RetryPolicy, TaskEvent,
    TaskManager, TaskStatus, download,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

fn make_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn serve_dir(dir: &Path) -> String {
    let app = Router::new().fallback_service(ServeDir::new(dir));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

struct AliasInterceptor {
    target_url: String,
    token: String,
}

impl ResourceInterceptor for AliasInterceptor {
    fn name(&self) -> &str {
        "alias-ext"
    }

    fn rewrite<'a>(
        &'a self,
        req: &'a ResolveRequest,
    ) -> BoxFuture<'a, Result<Option<ResolveRequest>, Error>> {
        Box::pin(async move {
            if req.url.starts_with("alias://") {
                let mut rewritten = req.clone();
                rewritten.url = self.target_url.clone();
                rewritten
                    .headers
                    .push(("x-ext-token".into(), self.token.clone()));
                return Ok(Some(rewritten));
            }
            Ok(None)
        })
    }
}

#[tokio::test]
async fn two_phase_resolve_and_task_manager_download() {
    let payload = make_bytes(2 * 1024 * 1024, 0x1234_5678);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("archive.zip"), &payload).unwrap();
    let base = serve_dir(srv.path()).await;

    let registry = FetcherRegistry::with_defaults(reqwest::Client::new());
    let store = Arc::new(MemoryTaskStore);
    let mgr = TaskManager::new(registry, store, 2).unwrap();
    let mut rx = mgr.subscribe();

    let url = format!("{base}/archive.zip");
    let (_, resource) = mgr.resolve(&ResolveRequest::new(&url)).await.unwrap();
    assert_eq!(resource.name, "archive.zip");
    assert_eq!(resource.total_size, Some(payload.len() as u64));
    assert!(resource.range_supported);
    assert_eq!(resource.meta.category, Category::Compressed);
    assert_eq!(resource.files.len(), 1);

    let out = tempfile::tempdir().unwrap();
    let mut create = CreateTaskRequest::new(url, out.path());
    create.resource = Some(resource);
    let rec = mgr.create(create).await.unwrap();

    // Wait for completion via broadcast events.
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    let completed_path = loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for task completion"),
            ev = rx.recv() => {
                if let Ok(TaskEvent::StatusChanged { id, status: TaskStatus::Completed, output_path }) = ev
                    && id == rec.id
                {
                    break output_path;
                }
            }
        }
    };

    let path = completed_path.expect("completed event must include output_path");
    assert_eq!(std::fs::read(&path).unwrap(), payload);

    let final_rec = mgr.get(rec.id).await.unwrap();
    assert_eq!(final_rec.status, TaskStatus::Completed);
    assert_eq!(final_rec.progress.downloaded, payload.len() as u64);

    // Removing with delete_files cleans up the downloaded file.
    mgr.remove(rec.id, true).await.unwrap();
    assert!(mgr.get(rec.id).await.is_none());
    assert!(!path.exists());
}

#[tokio::test]
async fn interceptor_rewrites_custom_scheme_and_injects_headers() {
    let payload = b"hello from intercepted extension resource".to_vec();
    let body = payload.clone();
    let app = Router::new().route(
        "/secret.bin",
        get(move |headers: HeaderMap| {
            let body = body.clone();
            async move {
                if headers.get("x-ext-token").and_then(|v| v.to_str().ok()) != Some("secret-42") {
                    return (StatusCode::FORBIDDEN, Vec::new()).into_response();
                }
                (
                    StatusCode::OK,
                    [("content-disposition", "attachment; filename*=UTF-8''resolved%20file.bin")],
                    body,
                )
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut registry = FetcherRegistry::with_defaults(reqwest::Client::new());
    registry.add_interceptor(Arc::new(AliasInterceptor {
        target_url: format!("http://127.0.0.1:{port}/secret.bin"),
        token: "secret-42".into(),
    }));

    let mgr = TaskManager::new(registry, Arc::new(MemoryTaskStore), 2).unwrap();
    let (rewritten, res) = mgr
        .resolve(&ResolveRequest::new("alias://my-video"))
        .await
        .unwrap();
    assert_eq!(res.name, "resolved file.bin");
    assert_eq!(rewritten.headers.len(), 1);

    let out = tempfile::tempdir().unwrap();
    let mut rx = mgr.subscribe();
    let rec = mgr
        .create(CreateTaskRequest::new("alias://my-video", out.path()))
        .await
        .unwrap();

    while let Ok(ev) = rx.recv().await {
        if let TaskEvent::StatusChanged {
            id,
            status: TaskStatus::Completed,
            output_path: Some(path),
        } = ev
            && id == rec.id
        {
            assert_eq!(std::fs::read(path).unwrap(), payload);
            return;
        }
    }
    panic!("event channel closed before completion");
}

#[tokio::test]
async fn transient_503_errors_are_retried_automatically() {
    let payload = make_bytes(128 * 1024, 0x9999);
    let hits = Arc::new(AtomicU32::new(0));
    let hits_clone = Arc::clone(&hits);
    let body = payload.clone();

    let app = Router::new().route(
        "/flaky.bin",
        get(move || {
            let hits = Arc::clone(&hits_clone);
            let body = body.clone();
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                // First request is probe (0); fail download attempts 1 and 2 with 503, succeed on 3.
                if n == 1 || n == 2 {
                    return (StatusCode::SERVICE_UNAVAILABLE, Vec::new()).into_response();
                }
                (StatusCode::OK, body).into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("flaky.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let opts = DownloadOptions {
        retry: RetryPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        ..Default::default()
    };

    let outcome = download(
        &reqwest::Client::new(),
        DownloadSpec {
            id: 1,
            url: format!("http://127.0.0.1:{port}/flaky.bin"),
            dest: dest.clone(),
            headers: vec![],
        },
        &opts,
        tx,
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&outcome.path).unwrap(), payload);
    assert!(hits.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn file_task_store_persists_and_recovers_paused_tasks() {
    let payload = make_bytes(256 * 1024, 0xCAFE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("doc.pdf"), &payload).unwrap();
    let base = serve_dir(srv.path()).await;

    let state_dir = tempfile::tempdir().unwrap();
    let store_path = state_dir.path().join("tasks.json");
    let out = tempfile::tempdir().unwrap();

    // Create a paused task and persist it.
    let task_id = {
        let reg = FetcherRegistry::with_defaults(reqwest::Client::new());
        let store = Arc::new(FileTaskStore::new(&store_path));
        let mgr = TaskManager::new(reg, store, 2).unwrap();
        let mut req = CreateTaskRequest::new(format!("{base}/doc.pdf"), out.path());
        req.auto_start = false;
        let rec = mgr.create(req).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Paused);
        rec.id
    };

    // Re-open TaskManager from the same store file and resume the task.
    let reg = FetcherRegistry::with_defaults(reqwest::Client::new());
    let store = Arc::new(FileTaskStore::new(&store_path));
    let mgr = TaskManager::new(reg, store, 2).unwrap();
    let restored = mgr.get(task_id).await.expect("task must be restored");
    assert_eq!(restored.status, TaskStatus::Paused);

    let mut rx = mgr.subscribe();
    mgr.resume(task_id).await.unwrap();
    while let Ok(ev) = rx.recv().await {
        if let TaskEvent::StatusChanged {
            id,
            status: TaskStatus::Completed,
            output_path: Some(path),
        } = ev
            && id == task_id
        {
            assert_eq!(std::fs::read(path).unwrap(), payload);
            return;
        }
    }
    panic!("resumed task did not complete");
}

#[tokio::test]
async fn unsupported_protocol_returns_typed_error() {
    let reg = FetcherRegistry::with_defaults(reqwest::Client::new());
    let err = reg
        .resolve(&ResolveRequest::new("magnet:?xt=urn:btih:0123456789abcdef"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::UnsupportedProtocol(ref s) if s == "magnet"));
}

#[tokio::test]
async fn mirror_failover_recovers_when_primary_returns_503() {
    let payload = make_bytes(96 * 1024, 0xFACE);
    let body = payload.clone();
    let app = Router::new()
        .route("/primary.bin", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
        .route(
            "/mirror.bin",
            get(move || {
                let body = body.clone();
                async move { (StatusCode::OK, body) }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut hasher = od_core::Sha256::new();
    hasher.update(&payload);
    let expected_sha = hasher.finalize_hex();

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("mirror-test.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let opts = DownloadOptions {
        mirrors: vec![format!("http://127.0.0.1:{port}/mirror.bin")],
        expected_sha256: Some(expected_sha),
        retry: RetryPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        ..Default::default()
    };

    let outcome = download(
        &reqwest::Client::new(),
        DownloadSpec {
            id: 7,
            url: format!("http://127.0.0.1:{port}/primary.bin"),
            dest: dest.clone(),
            headers: vec![],
        },
        &opts,
        tx,
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&outcome.path).unwrap(), payload);
}
