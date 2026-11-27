use reqwest::header::HeaderMap;

use crate::Error;

/// What the URL actually points at — a plain file or a streaming manifest
/// that has to be expanded into segments first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    File,
    /// `.m3u8` playlist (HLS).
    Hls,
    /// `.mpd` manifest (DASH).
    Dash,
}

impl StreamKind {
    /// Content-Type wins; URL extension is the fallback.
    pub fn sniff(url: &str, content_type: Option<&str>) -> Self {
        if let Some(ct) = content_type
            .and_then(|s| s.split(';').next())
            .map(str::trim)
        {
            match ct.to_ascii_lowercase().as_str() {
                "application/vnd.apple.mpegurl"
                | "application/x-mpegurl"
                | "application/mpegurl"
                | "audio/mpegurl"
                | "audio/x-mpegurl" => return Self::Hls,
                "application/dash+xml" => return Self::Dash,
                _ => {}
            }
        }
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if path.ends_with(".m3u8") {
            Self::Hls
        } else if path.ends_with(".mpd") {
            Self::Dash
        } else {
            Self::File
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProbeInfo {
    /// Total size in bytes, if the server disclosed it.
    pub size: Option<u64>,
    /// Whether the server honours `Range` requests.
    pub accepts_ranges: bool,
    /// Sanitized filename from `Content-Disposition`, if present.
    pub filename: Option<String>,
    /// `ETag` header — used to detect when a resumable file changed upstream.
    pub etag: Option<String>,
    /// `Content-Type` — fallback signal for category detection when the
    /// filename has no extension.
    pub content_type: Option<String>,
    /// File vs HLS/DASH manifest.
    pub kind: StreamKind,
}

pub async fn probe(client: &reqwest::Client, url: &str) -> Result<ProbeInfo, Error> {
    probe_with_headers(client, url, &HeaderMap::new()).await
}

/// Probe a URL with a 1-byte ranged GET. One request tells us range support,
/// total size and the suggested filename; the body is never read.
///
/// Falls back to a `HEAD` request when a server returns `416 Range Not
/// Satisfiable` (common for 0-byte files or servers that reject `bytes=0-0`).
pub async fn probe_with_headers(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<ProbeInfo, Error> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await?;

    let resp = if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        client
            .head(url)
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?
    } else {
        resp.error_for_status()?
    };

    let accepts_ranges = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let resp_headers = resp.headers();

    // `Content-Range: bytes 0-0/12345` gives the total for ranged responses;
    // `Content-Length` is the fallback for servers that ignored the range.
    let size = resp_headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|t| t.parse::<u64>().ok())
        .or_else(|| {
            resp_headers
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        });

    let filename = resp_headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_disposition)
        .and_then(|s| sanitize_filename(&s));

    let etag = resp_headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let content_type = resp_headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let kind = StreamKind::sniff(url, content_type.as_deref());

    Ok(ProbeInfo {
        size,
        accepts_ranges,
        filename,
        etag,
        content_type,
        kind,
    })
}

/// Extract a filename from an HTTP `Content-Disposition` header, supporting
/// both RFC 5987 (`filename*=UTF-8''...`) and classic `filename="..."` /
/// `filename=...`. RFC 5987 `filename*` takes precedence per RFC 6266 §4.3.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for param in header.split(';').skip(1) {
        let Some((raw_key, raw_val)) = param.split_once('=') else {
            continue;
        };
        let key = raw_key.trim().to_ascii_lowercase();
        let val = raw_val.trim();
        if key == "filename*" {
            // Format: charset'language'percent-encoded-value
            if let Some((_, encoded)) = val.rsplit_once('\'') {
                let decoded = percent_decode(encoded.trim_matches('"'));
                if !decoded.is_empty() {
                    extended = Some(decoded);
                }
            }
        } else if key == "filename" {
            let unquoted = val
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(val)
                .replace("\\\"", "\"");
            if !unquoted.is_empty() {
                plain = Some(unquoted);
            }
        }
    }

    extended.or(plain)
}

/// Sanitize a remote-supplied filename so it is safe on Windows, macOS, and Linux:
/// strips directory components (preventing `../` traversal), replaces illegal
/// characters, and avoids Windows reserved device names (`CON`, `NUL`, `COM1`…).
pub fn sanitize_filename(raw: &str) -> Option<String> {
    // Take only the final path segment if a server sent slashes.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }

    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }

    // Windows forbids trailing dots or spaces in filenames.
    let cleaned = out.trim().trim_end_matches(['.', ' ']);
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '_') {
        return None;
    }

    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or(cleaned)
        .to_ascii_uppercase();
    let is_reserved = matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );

    if is_reserved {
        Some(format!("_{cleaned}"))
    } else {
        Some(cleaned.to_string())
    }
}

/// Derive a safe fallback filename from a URL path.
pub fn filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.rsplit('/')
        .next()
        .map(percent_decode)
        .and_then(|s| sanitize_filename(&s))
        .unwrap_or_else(|| "download.bin".into())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc5987_and_plain_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".into())
        );
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"fallback.txt\"; filename*=UTF-8''my%20file%20%281%29.zip"
            ),
            Some("my file (1).zip".into())
        );
        assert_eq!(
            parse_content_disposition("inline; filename=plain.tar.gz"),
            Some("plain.tar.gz".into())
        );
    }

    #[test]
    fn sanitizes_traversal_illegal_chars_and_reserved_names() {
        assert_eq!(
            sanitize_filename("../../etc/passwd"),
            Some("passwd".into())
        );
        assert_eq!(
            sanitize_filename("a:b*c?d.mp4"),
            Some("a_b_c_d.mp4".into())
        );
        assert_eq!(sanitize_filename("CON.txt"), Some("_CON.txt".into()));
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("file.zip... "), Some("file.zip".into()));
    }

    #[test]
    fn filename_from_url_decodes_and_falls_back() {
        assert_eq!(
            filename_from_url("https://example.com/files/hello%20world.zip?token=123"),
            "hello world.zip"
        );
        assert_eq!(filename_from_url("https://example.com/"), "download.bin");
    }
}
