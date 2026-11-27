use axum::Router;
use axum::body::Bytes;
use axum::extract::Query;
use axum::routing::{get, post};
use od_core::{
    CancellationToken, SpeedTestConfig, SpeedTestEvent, SpeedTestPhase, run_speedtest,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(serde::Deserialize)]
struct DownQuery {
    #[serde(default)]
    bytes: usize,
}

async fn spawn_mock_speed_server() -> String {
    let app = Router::new()
        .route(
            "/__down",
            get(|Query(q): Query<DownQuery>| async move {
                vec![0xABu8; q.bytes.min(2 * 1024 * 1024)]
            }),
        )
        .route(
            "/__up",
            post(|body: Bytes| async move { format!("received {}", body.len()) }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn speedtest_measures_ping_download_and_upload_against_local_server() {
    let base = spawn_mock_speed_server().await;
    let cfg = SpeedTestConfig {
        download_url: format!("{base}/__down"),
        upload_url: format!("{base}/__up"),
        ping_samples: 4,
        download_streams: 2,
        download_bytes: 1024 * 1024,
        upload_bytes: 256 * 1024,
        include_upload: true,
        phase_budget_ms: 3000,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let report = run_speedtest(
        &reqwest::Client::new(),
        &cfg,
        CancellationToken::new(),
        Some(tx),
    )
    .await
    .unwrap();

    assert!(report.online);
    assert!(report.ping_ms >= 0.0);
    assert!(report.jitter_ms >= 0.0);
    assert!(report.download_mbps > 0.0);
    assert!(report.download_bps > 0);
    assert!(report.upload_mbps.unwrap_or(0.0) > 0.0);
    assert!(!report.samples_mbps.is_empty());

    let mut saw_done = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, SpeedTestEvent::PhaseChanged { phase: SpeedTestPhase::Done }) {
            saw_done = true;
        }
    }
    assert!(saw_done);
}

#[tokio::test]
async fn speedtest_honours_cancellation() {
    let base = spawn_mock_speed_server().await;
    let cfg = SpeedTestConfig {
        download_url: format!("{base}/__down"),
        upload_url: format!("{base}/__up"),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = run_speedtest(&reqwest::Client::new(), &cfg, cancel, None)
        .await
        .unwrap_err();
    assert!(matches!(err, od_core::Error::Cancelled));
}
