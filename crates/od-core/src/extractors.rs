//! Built-in Gopeed-style smart [`ResourceInterceptor`] extractors that turn
//! popular web/platform URLs into direct, high-speed download endpoints:
//! - **Hugging Face** web blob URLs (`/blob/<rev>/<file>`) → direct `/resolve/<rev>/<file>`
//!   (auto-attaching `HF_TOKEN` when set in the environment).
//! - **Google Drive** share URLs (`/file/d/<id>/view`) → direct `drive.usercontent.google.com`
//!   download URL.
//! - **GitHub** blob URLs (`github.com/<o>/<r>/blob/<ref>/<path>`) → `raw.githubusercontent.com`.

use std::sync::Arc;

use crate::Error;
use crate::fetcher::{BoxFuture, FetcherRegistry, ResolveRequest, ResourceInterceptor};

/// Register all built-in smart extractors on a [`FetcherRegistry`].
pub fn register_builtin_extractors(registry: &mut FetcherRegistry) {
    registry.add_interceptor(Arc::new(HuggingFaceExtractor));
    registry.add_interceptor(Arc::new(GoogleDriveExtractor));
    registry.add_interceptor(Arc::new(GitHubBlobExtractor));
}

/// Rewrites `https://huggingface.co/<org>/<repo>/blob/<rev>/<path>` into the
/// direct `/resolve/<rev>/<path>` download URL and attaches `HF_TOKEN` if present.
pub struct HuggingFaceExtractor;

impl ResourceInterceptor for HuggingFaceExtractor {
    fn name(&self) -> &str {
        "huggingface"
    }

    fn rewrite<'a>(
        &'a self,
        req: &'a ResolveRequest,
    ) -> BoxFuture<'a, Result<Option<ResolveRequest>, Error>> {
        Box::pin(async move {
            let Some(rewritten_url) = rewrite_huggingface_url(&req.url) else {
                return Ok(None);
            };
            let mut next = req.clone();
            next.url = rewritten_url;
            if let Ok(tok) = std::env::var("HF_TOKEN")
                && !tok.trim().is_empty()
                && !next
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            {
                next.headers
                    .push(("authorization".into(), format!("Bearer {}", tok.trim())));
            }
            Ok(Some(next))
        })
    }
}

/// Rewrites Google Drive share links (`https://drive.google.com/file/d/<ID>/view...`
/// or `https://drive.google.com/open?id=<ID>`) into direct download URLs.
pub struct GoogleDriveExtractor;

impl ResourceInterceptor for GoogleDriveExtractor {
    fn name(&self) -> &str {
        "google-drive"
    }

    fn rewrite<'a>(
        &'a self,
        req: &'a ResolveRequest,
    ) -> BoxFuture<'a, Result<Option<ResolveRequest>, Error>> {
        Box::pin(async move {
            let Some(rewritten_url) = rewrite_gdrive_url(&req.url) else {
                return Ok(None);
            };
            let mut next = req.clone();
            next.url = rewritten_url;
            Ok(Some(next))
        })
    }
}

/// Rewrites GitHub file web views (`https://github.com/<owner>/<repo>/blob/<ref>/<path>`)
/// into direct `https://raw.githubusercontent.com/<owner>/<repo>/<ref>/<path>` URLs.
pub struct GitHubBlobExtractor;

impl ResourceInterceptor for GitHubBlobExtractor {
    fn name(&self) -> &str {
        "github-blob"
    }

    fn rewrite<'a>(
        &'a self,
        req: &'a ResolveRequest,
    ) -> BoxFuture<'a, Result<Option<ResolveRequest>, Error>> {
        Box::pin(async move {
            let Some(rewritten_url) = rewrite_github_blob_url(&req.url) else {
                return Ok(None);
            };
            let mut next = req.clone();
            next.url = rewritten_url;
            Ok(Some(next))
        })
    }
}

pub fn rewrite_huggingface_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.host_str()? != "huggingface.co" {
        return None;
    }
    let path = parsed.path();
    if let Some(pos) = path.find("/blob/") {
        let mut new_url = parsed.clone();
        let replaced = format!("{}/resolve/{}", &path[..pos], &path[pos + 6..]);
        new_url.set_path(&replaced);
        return Some(new_url.to_string());
    }
    None
}

pub fn rewrite_gdrive_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.host_str()? != "drive.google.com" {
        return None;
    }
    // Pattern 1: /file/d/<ID>/view
    let segs: Vec<&str> = parsed.path_segments()?.collect();
    if segs.len() >= 3 && segs[0] == "file" && segs[1] == "d" && !segs[2].is_empty() {
        return Some(format!(
            "https://drive.usercontent.google.com/download?id={}&export=download&confirm=t",
            segs[2]
        ));
    }
    // Pattern 2: /open?id=<ID> or /uc?id=<ID>
    for (k, v) in parsed.query_pairs() {
        if k == "id" && !v.is_empty() {
            return Some(format!(
                "https://drive.usercontent.google.com/download?id={v}&export=download&confirm=t"
            ));
        }
    }
    None
}

pub fn rewrite_github_blob_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.host_str()? != "github.com" {
        return None;
    }
    let segs: Vec<&str> = parsed.path_segments()?.collect();
    if segs.len() >= 5 && segs[2] == "blob" {
        let owner = segs[0];
        let repo = segs[1];
        let rest = segs[3..].join("/");
        return Some(format!("https://raw.githubusercontent.com/{owner}/{repo}/{rest}"));
    }
    None
}

/// Parse a batch list of URLs (one per line), skipping blank lines and `#` comments.
pub fn parse_batch_urls(input: &str) -> Vec<String> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_huggingface_gdrive_and_github_urls() {
        assert_eq!(
            rewrite_huggingface_url(
                "https://huggingface.co/meta-llama/Llama-3/blob/main/model.safetensors"
            ),
            Some(
                "https://huggingface.co/meta-llama/Llama-3/resolve/main/model.safetensors".into()
            )
        );
        assert_eq!(
            rewrite_gdrive_url("https://drive.google.com/file/d/1AbCdEfGhIjK/view?usp=sharing"),
            Some(
                "https://drive.usercontent.google.com/download?id=1AbCdEfGhIjK&export=download&confirm=t"
                    .into()
            )
        );
        assert_eq!(
            rewrite_github_blob_url("https://github.com/rust-lang/rust/blob/master/README.md"),
            Some("https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into())
        );
    }

    #[test]
    fn parses_batch_url_lines() {
        let urls = parse_batch_urls("# comment\n\nhttps://a.com/1.zip\n  https://b.com/2.mp4  \n");
        assert_eq!(urls, vec!["https://a.com/1.zip", "https://b.com/2.mp4"]);
    }
}
