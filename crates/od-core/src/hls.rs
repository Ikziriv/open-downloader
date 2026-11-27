//! HLS (`.m3u8`) support: master playlists pick the highest-bandwidth
//! variant, media playlists expand into segment URLs, and the shared
//! segment fetcher writes them in order. AES-128 (`EXT-X-KEY`) streams are
//! decrypted segment-by-segment, including mid-playlist key rotation.
//!
//! Limits, all reported as `Error::Stream` rather than silently producing a
//! broken file: live playlists (no `#EXT-X-ENDLIST`), `SAMPLE-AES`/DRM
//! encryption, and `#EXT-X-DISCONTINUITY` is tolerated by concatenation but
//! may produce a file some players handle poorly.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::header::HeaderMap;
use tokio::sync::mpsc;

use crate::engine::sidecar_paths;
use crate::stream::{Decrypt, concat_segments, fetch_segments, segments_dir};
use crate::{DownloadOptions, DownloadOutcome, DownloadSpec, Error, Event};

struct Playlist {
    /// Segments in play order — the `#EXT-X-MAP` init segment first when
    /// the variant is fMP4.
    segments: Vec<Seg>,
    is_fmp4: bool,
}

struct Seg {
    url: String,
    /// AES-128 parameters in effect for this segment (key rotation means
    /// earlier and later segments may differ).
    key: Option<KeyRef>,
}

#[derive(Clone)]
struct KeyRef {
    /// Absolute key URL.
    uri: String,
    /// Explicit IV; `None` = the segment's media sequence number.
    iv: Option<[u8; 16]>,
    /// Media sequence number — the default IV per RFC 8216.
    seq: u64,
}

fn join(base: &reqwest::Url, rel: &str) -> Result<String, Error> {
    Ok(base
        .join(rel)
        .map_err(|e| Error::Stream(format!("bad segment URL {rel:?}: {e}")))?
        .to_string())
}

/// Quoted attribute value: `KEY="..."`.
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let rest = tag.split_once(&format!("{key}=\""))?.1;
    rest.split('"').next()
}

/// Unquoted attribute value: `KEY=v` (stops at the next `,`).
fn attr_raw<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    tag.split_once(&format!("{key}="))?.1.split(',').next()
}

/// Unquoted-or-quoted numeric attribute: `KEY=123` or `KEY="123"`.
fn attr_u64(tag: &str, key: &str) -> Option<u64> {
    let rest = tag.split_once(&format!("{key}="))?.1;
    rest.trim_start_matches('"')
        .split([',', '"', ' '])
        .next()?
        .parse()
        .ok()
}

/// Variant URI of the highest-bandwidth stream in a master playlist.
fn pick_variant(text: &str) -> Option<String> {
    let mut best_bw = 0u64;
    let mut best = None;
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF") {
            let bw = attr_u64(rest, "BANDWIDTH").unwrap_or(0);
            if let Some(uri) = lines
                .next()
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                && bw >= best_bw
            {
                best_bw = bw;
                best = Some(uri.to_string());
            }
        }
    }
    best
}

fn hex16(s: &str) -> Option<[u8; 16]> {
    let s = s.trim().strip_prefix("0x").unwrap_or(s.trim());
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

type ParsedKey = (String, Option<[u8; 16]>);

/// Parse an `#EXT-X-KEY` tag into (method, uri, iv).
fn parse_key(rest: &str, base: &reqwest::Url) -> Result<Option<ParsedKey>, Error> {
    match attr_raw(rest, "METHOD") {
        Some("NONE") => Ok(None),
        Some("AES-128") => {
            let uri = attr(rest, "URI")
                .ok_or_else(|| Error::Stream("EXT-X-KEY without URI".into()))?;
            let iv = attr_raw(rest, "IV").and_then(hex16);
            Ok(Some((join(base, uri)?, iv)))
        }
        Some(m) => Err(Error::Stream(format!(
            "unsupported HLS encryption method {m:?} (only AES-128 works)"
        ))),
        None => Err(Error::Stream("EXT-X-KEY without METHOD".into())),
    }
}

/// Parse a *media* playlist into absolute segment URLs.
fn parse_media_playlist(playlist_url: &str, text: &str) -> Result<Playlist, Error> {
    let base = reqwest::Url::parse(playlist_url)
        .map_err(|e| Error::Stream(format!("bad playlist URL: {e}")))?;
    let mut segments = Vec::new();
    let mut is_fmp4 = false;
    let mut has_endlist = false;
    let mut media_seq = 0u64;
    let mut cur_key: Option<(String, Option<[u8; 16]>)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY") {
            cur_key = parse_key(rest, &base)?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE") {
            media_seq = rest.trim_start_matches(':').trim().parse().unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP") {
            if let Some(uri) = attr(rest, "URI") {
                is_fmp4 = true;
                segments.push(Seg {
                    url: join(&base, uri)?,
                    key: cur_key
                        .clone()
                        .map(|(uri, iv)| KeyRef { uri, iv, seq: media_seq }),
                });
            }
            continue;
        }
        if line.starts_with("#EXT-X-ENDLIST") {
            has_endlist = true;
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        segments.push(Seg {
            url: join(&base, line)?,
            key: cur_key
                .clone()
                .map(|(uri, iv)| KeyRef { uri, iv, seq: media_seq }),
        });
        media_seq += 1;
    }
    if !has_endlist {
        return Err(Error::Stream(
            "live HLS playlists aren't supported (no EXT-X-ENDLIST)".into(),
        ));
    }
    if segments.is_empty() {
        return Err(Error::Stream("HLS playlist had no segments".into()));
    }
    Ok(Playlist { segments, is_fmp4 })
}

/// Default IV: the segment's media sequence number as a 16-byte big-endian
/// integer (RFC 8216 §5.2).
fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn aes128_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, Error> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    let mut buf = data.to_vec();
    let pt = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| Error::Stream(format!("corrupt AES-128 segment: {e}")))?;
    Ok(pt.to_vec())
}

async fn get_text(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<String, Error> {
    Ok(client
        .get(url)
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Download an HLS stream to `spec.dest` (extension swapped to `.ts`, or
/// `.mp4` for fMP4 variants, when it was auto-derived as `.m3u8`).
pub(crate) async fn download(
    client: &reqwest::Client,
    spec: &DownloadSpec,
    headers: &HeaderMap,
    opts: &DownloadOptions,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<DownloadOutcome, Error> {
    let text = get_text(client, &spec.url, headers).await?;
    let (media_url, text) = if text.contains("#EXT-X-STREAM-INF") {
        let variant = pick_variant(&text)
            .ok_or_else(|| Error::Stream("master playlist had no playable variant".into()))?;
        let base = reqwest::Url::parse(&spec.url)
            .map_err(|e| Error::Stream(format!("bad playlist URL: {e}")))?;
        let media_url = join(&base, &variant)?;
        (media_url.clone(), get_text(client, &media_url, headers).await?)
    } else {
        (spec.url.clone(), text)
    };

    let pl = parse_media_playlist(&media_url, &text)?;
    let ext = if pl.is_fmp4 { "mp4" } else { "ts" };
    let dest: PathBuf = if spec.dest.extension().is_some_and(|e| e == "m3u8") {
        spec.dest.with_extension(ext)
    } else {
        spec.dest.clone()
    };

    // Fetch each distinct key once, then hand the fetcher a decrypt hook.
    let key_uris: BTreeSet<&str> = pl
        .segments
        .iter()
        .filter_map(|s| s.key.as_ref().map(|k| k.uri.as_str()))
        .collect();
    let decrypt: Option<Decrypt> = if key_uris.is_empty() {
        None
    } else {
        let mut keys = HashMap::new();
        for uri in key_uris {
            let bytes = client
                .get(uri)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let key: [u8; 16] = bytes
                .as_ref()
                .try_into()
                .map_err(|_| Error::Stream(format!("HLS key {uri} isn't 16 bytes")))?;
            keys.insert(uri.to_string(), key);
        }
        let plan: Vec<Option<([u8; 16], [u8; 16])>> = pl
            .segments
            .iter()
            .map(|s| {
                s.key.as_ref().and_then(|k| {
                    keys.get(&k.uri).map(|key| (*key, k.iv.unwrap_or_else(|| seq_iv(k.seq))))
                })
            })
            .collect();
        Some(Arc::new(move |i: usize, data: &[u8]| {
            match plan.get(i).copied().flatten() {
                Some((k, iv)) => aes128_decrypt(&k, &iv, data),
                None => Ok(data.to_vec()),
            }
        }))
    };

    let urls: Vec<String> = pl.segments.iter().map(|s| s.url.clone()).collect();
    let (part, _) = sidecar_paths(&dest);
    let seg_dir = segments_dir(&dest);
    let paths = fetch_segments(
        client, &urls, headers, &seg_dir, opts, events, spec.id, decrypt,
    )
    .await?;
    let bytes = concat_segments(&paths, &part).await?;
    let _ = tokio::fs::remove_dir_all(&seg_dir).await;
    let _ = events.send(Event::Progress {
        id: spec.id,
        downloaded: bytes,
        total: Some(bytes),
    });
    // Windows rename fails when the target exists — remove it first.
    let _ = tokio::fs::remove_file(&dest).await;
    tokio::fs::rename(&part, &dest).await?;
    Ok(DownloadOutcome {
        path: dest,
        resumed: false,
    })
}
