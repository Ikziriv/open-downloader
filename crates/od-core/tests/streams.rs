use std::path::Path;

use axum::Router;
use od_core::{DownloadOptions, DownloadSpec, download};
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
async fn hls_master_picks_highest_bandwidth_variant() {
    let segs: Vec<Vec<u8>> = (0..3).map(|i| make_bytes(100_000 + i * 7, 0xabc0 + i as u64)).collect();
    let srv = tempfile::tempdir().unwrap();
    for (i, s) in segs.iter().enumerate() {
        std::fs::write(srv.path().join(format!("h{i}.ts")), s).unwrap();
    }
    std::fs::write(srv.path().join("l0.ts"), b"low").unwrap();
    std::fs::write(
        srv.path().join("master.m3u8"),
        "#EXTM3U\n\
         #EXT-X-STREAM-INF:BANDWIDTH=4000000\nhi.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=800000\nlo.m3u8\n",
    )
    .unwrap();
    std::fs::write(
        srv.path().join("hi.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n\
         #EXTINF:8.0,\nh0.ts\n#EXTINF:8.0,\nh1.ts\n#EXTINF:8.0,\nh2.ts\n\
         #EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        srv.path().join("lo.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXTINF:8.0,\nl0.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/master.m3u8"), &out.path().join("movie.m3u8")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    // .m3u8 dest is rewritten to .ts and contains the hi variant's bytes.
    assert_eq!(res.path, out.path().join("movie.ts"));
    let want: Vec<u8> = segs.concat();
    assert_eq!(std::fs::read(&res.path).unwrap(), want);
    assert!(!out.path().join("movie.ts.segments").exists());
}

#[tokio::test]
async fn hls_live_playlist_errors_clearly() {
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("s0.ts"), b"x").unwrap();
    std::fs::write(
        srv.path().join("live.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\ns0.ts\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let err = download(
        &reqwest::Client::new(),
        spec(format!("{base}/live.m3u8"), &out.path().join("live.m3u8")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("live"), "{err}");
}

fn aes128_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    let mut buf = data.to_vec();
    buf.resize(data.len() + 16, 0);
    let n = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .unwrap()
        .len();
    buf.truncate(n);
    buf
}

#[tokio::test]
async fn hls_aes128_decrypts_segments() {
    let key = [0x42u8; 16];
    let iv = [0x07u8; 16];
    let plain: Vec<Vec<u8>> = (0..2).map(|i| make_bytes(60_000 + i, 0x77 + i as u64)).collect();
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("key.bin"), key).unwrap();
    for (i, p) in plain.iter().enumerate() {
        std::fs::write(srv.path().join(format!("e{i}.ts")), aes128_encrypt(&key, &iv, p)).unwrap();
    }
    let iv_hex: String = iv.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(
        srv.path().join("enc.m3u8"),
        format!(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{iv_hex}\n\
             #EXTINF:8.0,\ne0.ts\n#EXTINF:8.0,\ne1.ts\n#EXT-X-ENDLIST\n"
        ),
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/enc.m3u8"), &out.path().join("enc.m3u8")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();
    let want: Vec<u8> = plain.concat();
    assert_eq!(std::fs::read(&res.path).unwrap(), want);
}

#[tokio::test]
async fn hls_aes128_uses_media_sequence_as_default_iv() {
    let key = [0x11u8; 16];
    // EXT-X-MEDIA-SEQUENCE:5 → first segment's IV is 5 big-endian.
    let seq = 5u64;
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    let plain = make_bytes(40_000, 0x55);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("k.bin"), key).unwrap();
    std::fs::write(srv.path().join("s0.ts"), aes128_encrypt(&key, &iv, &plain)).unwrap();
    std::fs::write(
        srv.path().join("seq.m3u8"),
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n\
         #EXTINF:8.0,\ns0.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/seq.m3u8"), &out.path().join("seq.m3u8")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&res.path).unwrap(), plain);
}

#[tokio::test]
async fn hls_sample_aes_errors_clearly() {
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("s0.ts"), b"x").unwrap();
    std::fs::write(
        srv.path().join("drm.m3u8"),
        "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k.bin\"\n\
         #EXTINF:8.0,\ns0.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let err = download(
        &reqwest::Client::new(),
        spec(format!("{base}/drm.m3u8"), &out.path().join("drm.m3u8")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("SAMPLE-AES"), "{err}");
}

#[tokio::test]
async fn hls_resumes_from_segment_dir() {
    let segs: Vec<Vec<u8>> = (0..3).map(|i| make_bytes(50_000, 0xf00 + i as u64)).collect();
    let srv = tempfile::tempdir().unwrap();
    for (i, s) in segs.iter().enumerate() {
        std::fs::write(srv.path().join(format!("s{i}.ts")), s).unwrap();
    }
    std::fs::write(
        srv.path().join("v.m3u8"),
        "#EXTM3U\n#EXTINF:8.0,\ns0.ts\n#EXTINF:8.0,\ns1.ts\n#EXTINF:8.0,\ns2.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("v.ts");
    // Simulate an interrupted run: segment 0 finished, segment 1 half-written.
    let seg_dir = out.path().join("v.ts.segments");
    std::fs::create_dir_all(&seg_dir).unwrap();
    std::fs::write(seg_dir.join("000000"), &segs[0]).unwrap();
    std::fs::write(seg_dir.join("000001.tmp"), &segs[1][..1000]).unwrap();

    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/v.m3u8"), &dest),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    // Segment 0 was reused, truncated 000001.tmp was re-fetched whole.
    let want: Vec<u8> = segs.concat();
    assert_eq!(std::fs::read(&res.path).unwrap(), want);
}

#[tokio::test]
async fn dash_segment_list_concatenates_init_and_media() {
    let init = make_bytes(600, 0x1);
    let m0 = make_bytes(80_000, 0x2);
    let m1 = make_bytes(70_000, 0x3);
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("init.mp4"), &init).unwrap();
    std::fs::write(srv.path().join("m0.m4s"), &m0).unwrap();
    std::fs::write(srv.path().join("m1.m4s"), &m1).unwrap();
    std::fs::write(
        srv.path().join("movie.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="1" bandwidth="900000">
        <SegmentList>
          <Initialization sourceURL="init.mp4"/>
          <SegmentURL media="m0.m4s"/>
          <SegmentURL media="m1.m4s"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/movie.mpd"), &out.path().join("movie.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert_eq!(res.path, out.path().join("movie.mp4"));
    let mut want = init.clone();
    want.extend_from_slice(&m0);
    want.extend_from_slice(&m1);
    assert_eq!(std::fs::read(&res.path).unwrap(), want);
}

#[tokio::test]
async fn dash_baseurl_representation_downloads_whole_file() {
    let media = make_bytes(120_000, 0x9);
    let srv = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(srv.path().join("v")).unwrap();
    std::fs::write(srv.path().join("v/full.mp4"), &media).unwrap();
    std::fs::write(
        srv.path().join("one.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="1" bandwidth="500000">
        <BaseURL>v/full.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/one.mpd"), &out.path().join("one.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&res.path).unwrap(), media);
}

#[tokio::test]
async fn dash_dynamic_mpd_errors_clearly() {
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(
        srv.path().join("live.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic">
  <Period><AdaptationSet><Representation id="1" bandwidth="1"/></AdaptationSet></Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let err = download(
        &reqwest::Client::new(),
        spec(format!("{base}/live.mpd"), &out.path().join("live.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("live"), "{err}");
}

#[tokio::test]
async fn dash_segment_template_expands_duration_and_number_format() {
    let init = make_bytes(500, 0xa);
    let segs: Vec<Vec<u8>> = (1..=3).map(|i| make_bytes(30_000 + i, 0xb0 + i as u64)).collect();
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(srv.path().join("init-r42.mp4"), &init).unwrap();
    for (i, s) in segs.iter().enumerate() {
        std::fs::write(srv.path().join(format!("seg-{:03}.m4s", i + 1)), s).unwrap();
    }
    // PT3S at timescale=1000, duration=1000 → segments 1..3 (inclusive).
    std::fs::write(
        srv.path().join("t.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT3S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate media="seg-$Number%03d$.m4s" initialization="init-$RepresentationID$.mp4"
                       timescale="1000" duration="1000" startNumber="1"/>
      <Representation id="r42" bandwidth="2000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/t.mpd"), &out.path().join("t.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();

    assert_eq!(res.path, out.path().join("t.mp4"));
    let mut want = init;
    want.extend(segs.concat());
    assert_eq!(std::fs::read(&res.path).unwrap(), want);
}

#[tokio::test]
async fn dash_segment_timeline_expands_time_template() {
    // Timeline: S(t=0,d=1000,r=1) S(t=2500,d=500) → times 0,1000,2500.
    let parts: Vec<Vec<u8>> = [0u64, 1000, 2500]
        .iter()
        .map(|t| make_bytes(20_000, t + 1))
        .collect();
    let srv = tempfile::tempdir().unwrap();
    for (i, p) in parts.iter().enumerate() {
        std::fs::write(srv.path().join(format!("c{i}.m4s")), p).unwrap();
    }
    std::fs::write(
        srv.path().join("tl.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000000">
        <SegmentTemplate media="c$Number$.m4s" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S t="0" d="1000" r="1"/>
            <S t="2500" d="500"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let res = download(
        &reqwest::Client::new(),
        spec(format!("{base}/tl.mpd"), &out.path().join("tl.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&res.path).unwrap(), parts.concat());
}

#[tokio::test]
async fn dash_template_without_duration_errors() {
    let srv = tempfile::tempdir().unwrap();
    std::fs::write(
        srv.path().join("bad.mpd"),
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static">
  <Period><AdaptationSet>
    <Representation id="v" bandwidth="1">
      <SegmentTemplate media="s$Number$.m4s" timescale="1000"/>
    </Representation>
  </AdaptationSet></Period>
</MPD>"#,
    )
    .unwrap();
    let base = serve_files(srv.path()).await;

    let out = tempfile::tempdir().unwrap();
    let err = download(
        &reqwest::Client::new(),
        spec(format!("{base}/bad.mpd"), &out.path().join("bad.mpd")),
        &DownloadOptions::default(),
        events(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("SegmentTimeline"), "{err}");
}
