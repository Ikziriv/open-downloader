use std::io::Write;
use std::path::Path;

use axum::Router;
use od_core::{DownloadOptions, DownloadSpec, download, probe};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

const SIZE: usize = 5 * 1024 * 1024;
// DownloadOptions::default() picks max(size/32, 512KB); for 5MB that's 512KB.
const CHUNK: usize = 512 * 1024;

fn make_bytes(len: usize) -> Vec<u8> {
    let mut x = 0x9e3779b9u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn serve_files(dir: &Path) -> String {
    let app = Router::new().fallback_service(ServeDir::new(dir));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

fn events() -> mpsc::UnboundedSender<od_core::Event> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tx
}

fn spec(url: String, dest: &Path) -> DownloadSpec {
    DownloadSpec {
        id: 0,
        url,
        dest: dest.to_path_buf(),
        headers: vec![],
    }
}

#[tokio::test]
async fn downloads_ranged_file_and_cleans_up() {
    let data = make_bytes(SIZE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("file.bin"), &data).unwrap();
    let base = serve_files(srv.path()).await;

    let client = reqwest::Client::new();
    let info = probe(&client, &format!("{base}/file.bin"))
        .await
        .unwrap();
    assert!(info.accepts_ranges);
    assert_eq!(info.size, Some(SIZE as u64));

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("file.bin");
    let res = download(
        &client,
        spec(format!("{base}/file.bin"), &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert!(!res.resumed);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!out.path().join("file.bin.part").exists());
    assert!(!out.path().join("file.bin.odj").exists());
}

#[tokio::test]
async fn resumes_completed_chunks_from_journal() {
    let data = make_bytes(SIZE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("file.bin"), &data).unwrap();
    let base = serve_files(srv.path()).await;
    let url = format!("{base}/file.bin");

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("file.bin");

    // Simulate a crashed run: .part preallocated, first 4 chunks already
    // written (they're contiguous → a plain prefix write), journal recorded.
    let done_chunks = [0u64, 1, 2, 3];
    let part = out.path().join("file.bin.part");
    let mut f = std::fs::File::create(&part).unwrap();
    f.set_len(SIZE as u64).unwrap();
    f.write_all(&data[..done_chunks.len() * CHUNK]).unwrap();
    drop(f);

    let journal = serde_json::json!({
        "url": url,
        "size": SIZE as u64,
        "etag": null,
        "chunk_size": CHUNK as u64,
        "completed": done_chunks,
    });
    std::fs::write(out.path().join("file.bin.odj"), journal.to_string()).unwrap();

    let res = download(
        &reqwest::Client::new(),
        spec(url, &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert!(res.resumed);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!part.exists());
    assert!(!out.path().join("file.bin.odj").exists());
}

#[tokio::test]
async fn stale_journal_starts_over() {
    let data = make_bytes(SIZE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("file.bin"), &data).unwrap();
    let base = serve_files(srv.path()).await;
    let url = format!("{base}/file.bin");

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("file.bin");

    // Journal for a *different* URL must not be trusted.
    let part = out.path().join("file.bin.part");
    std::fs::File::create(&part)
        .unwrap()
        .set_len(SIZE as u64)
        .unwrap();
    let journal = serde_json::json!({
        "url": "http://127.0.0.1:1/other.bin",
        "size": SIZE as u64,
        "etag": null,
        "chunk_size": CHUNK as u64,
        "completed": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    });
    std::fs::write(out.path().join("file.bin.odj"), journal.to_string()).unwrap();

    let res = download(
        &reqwest::Client::new(),
        spec(url, &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert!(!res.resumed);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn cancelled_download_resumes_later() {
    let data = make_bytes(SIZE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("file.bin"), &data).unwrap();
    let base = serve_files(srv.path()).await;
    let url = format!("{base}/file.bin");

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("file.bin");
    let client = reqwest::Client::new();

    // Cancel before the first byte: workers must report Cancelled and the
    // journal must still exist so a later run can resume.
    let cancel = od_core::CancellationToken::new();
    cancel.cancel();
    let err = download(
        &client,
        spec(url.clone(), &dest),
        &DownloadOptions {
            cancel: Some(cancel),
            ..Default::default()
        },
        events(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, od_core::Error::Cancelled));
    assert!(out.path().join("file.bin.odj").exists());

    let res = download(
        &client,
        spec(url, &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();
    assert!(res.resumed);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn forwards_extra_headers_on_every_request() {
    // Server requires a cookie on every request (probe AND range fetches).
    let data = make_bytes(SIZE);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("file.bin"), &data).unwrap();

    let app = Router::new()
        .fallback_service(ServeDir::new(srv.path()))
        .layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                if req.headers().get("cookie").and_then(|v| v.to_str().ok())
                    == Some("session=abc")
                {
                    Ok(next.run(req).await)
                } else {
                    Err(axum::http::StatusCode::UNAUTHORIZED)
                }
            },
        ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/file.bin");

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("file.bin");
    let client = reqwest::Client::new();

    // Without the cookie the probe itself gets a 401.
    let no_headers = spec(url.clone(), &dest);
    assert!(
        download(&client, no_headers, &DownloadOptions::default(), events())
            .await
            .is_err()
    );

    let mut with_headers = spec(url, &dest);
    with_headers.headers = vec![("cookie".into(), "session=abc".into())];
    let res = download(&client, with_headers, &DownloadOptions::default(), events())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&res.path).unwrap(), data);
}

#[tokio::test]
async fn falls_back_to_single_stream_without_ranges() {
    let data = make_bytes(1024 * 1024);
    let app = Router::new().route(
        "/blob",
        axum::routing::get(move || {
            let data = data.clone();
            async move { axum::body::Bytes::from(data) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("blob.bin");
    let res = download(
        &reqwest::Client::new(),
        spec(format!("http://127.0.0.1:{port}/blob"), &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert!(!res.resumed);
    assert_eq!(std::fs::read(&dest).unwrap(), make_bytes(1024 * 1024));
}

#[tokio::test]
async fn organize_sorts_into_category_folders() {
    let srv = tempfile::tempdir().unwrap();
    let data = make_bytes(300_000);
    std::fs::write(srv.path().join("clip.mp4"), &data).unwrap();
    std::fs::write(srv.path().join("mystery.bin"), &data).unwrap();
    std::fs::write(
        srv.path().join("v.m3u8"),
        "#EXTM3U\n#EXTINF:8.0,\nclip.mp4\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;
    let client = reqwest::Client::new();
    let out = tempfile::tempdir().unwrap();
    let opts = DownloadOptions {
        organize: true,
        ..Default::default()
    };

    // Known extension → Video/
    let res = download(&client, spec(format!("{base}/clip.mp4"), &out.path().join("clip.mp4")), &opts, events())
        .await
        .unwrap();
    assert_eq!(res.path, out.path().join("Video/clip.mp4"));

    // Unknown type stays in the base folder
    let res = download(&client, spec(format!("{base}/mystery.bin"), &out.path().join("mystery.bin")), &opts, events())
        .await
        .unwrap();
    assert_eq!(res.path, out.path().join("mystery.bin"));

    // HLS manifest → Video/, and the .m3u8 name becomes .ts
    let res = download(&client, spec(format!("{base}/v.m3u8"), &out.path().join("v.m3u8")), &opts, events())
        .await
        .unwrap();
    assert_eq!(res.path, out.path().join("Video/v.ts"));
    assert_eq!(std::fs::read(&res.path).unwrap(), data);
}

#[tokio::test]
async fn organize_uses_content_type_when_no_extension() {
    let data = make_bytes(200_000);
    let app = Router::new().route(
        "/get",
        axum::routing::get(move || {
            let data = data.clone();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/pdf")],
                    axum::body::Bytes::from(data),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("http://127.0.0.1:{port}/get"), &out.path().join("get")),
        &DownloadOptions {
            organize: true,
            ..Default::default()
        },
        events(),
    )
    .await
    .unwrap();
    assert_eq!(res.path, out.path().join("Documents/get"));
}
