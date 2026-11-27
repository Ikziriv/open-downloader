//! Gopeed-inspired two-phase download abstraction (`Resolve → Download`).
//!
//! Every protocol (HTTP/HTTPS, HLS, DASH, and future BitTorrent, Magnet, FTP,
//! SFTP, or extension-resolved resources) implements [`Fetcher`]. Callers first
//! call [`FetcherRegistry::resolve`] to inspect a URL and preview its files,
//! size, and metadata, then call [`FetcherRegistry::download`] to transfer the
//! selected files.
//!
//! Extension runtimes (e.g. QuickJS/WASM extractors) plug into
//! [`ResourceInterceptor`] so they can rewrite URLs, attach auth headers, or
//! synthesize multi-file [`Resource`] manifests without modifying core code.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::header::HeaderMap;
use tokio::sync::mpsc;

use crate::engine::download_with_probe;
use crate::probe::{ProbeInfo, StreamKind, filename_from_url, probe_with_headers};
use crate::retry::{RetryPolicy, retry_async};
use crate::{CancellationToken, Category, DownloadOptions, DownloadOutcome, DownloadSpec, Error, Event};

/// Boxed future type keeping [`Fetcher`] and [`ResourceInterceptor`] dyn-compatible
/// so heterogeneous protocol implementations can live in [`FetcherRegistry`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Input to the `Resolve` phase.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolveRequest {
    pub url: String,
    /// Extra request headers (cookies, referer, authorization, custom user-agent).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Protocol-specific or extension-specific parameters.
    #[serde(default)]
    pub extra: serde_json::Value,
}

impl ResolveRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            extra: serde_json::Value::Null,
        }
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Lowercase URL scheme (`http`, `https`, `magnet`, `ftp`, …).
    pub fn scheme(&self) -> Option<String> {
        self.url
            .split_once(':')
            .map(|(s, _)| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
    }

    pub(crate) fn header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                k.parse::<reqwest::header::HeaderName>(),
                v.parse::<reqwest::header::HeaderValue>(),
            ) {
                map.append(name, value);
            }
        }
        map
    }
}

/// One downloadable file inside a [`Resource`].
/// Single-file HTTP downloads have 1 entry; torrents, playlists, and multi-track
/// streams expose N entries that the user can selectively toggle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub index: usize,
    pub name: String,
    /// Relative path inside the destination folder (empty for single-file downloads).
    #[serde(default)]
    pub path: String,
    pub size: Option<u64>,
    #[serde(default = "default_selected")]
    pub selected: bool,
}

fn default_selected() -> bool {
    true
}

/// Protocol and content metadata produced during `Resolve`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResourceMeta {
    /// Name of the protocol/fetcher that resolved this resource (e.g. `"http"`).
    pub protocol: String,
    pub kind: StreamKind,
    pub category: Category,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    /// Optional resolved direct URL (may differ from the input URL after extension resolution).
    pub resolved_url: String,
    /// Protocol- or extension-specific metadata (e.g. torrent info_hash, HLS variants).
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Result of resolving a URL before starting the download.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Resource {
    pub name: String,
    pub total_size: Option<u64>,
    pub range_supported: bool,
    pub files: Vec<FileEntry>,
    pub meta: ResourceMeta,
}

/// Execution context passed to [`Fetcher::download`].
pub struct FetchContext {
    pub id: u64,
    pub request: ResolveRequest,
    pub resource: Resource,
    /// Target directory or explicit output path (when `explicit_path` is true).
    pub dest: PathBuf,
    /// When `true`, `dest` is treated as the exact file path; when `false`,
    /// `dest` is treated as the output directory and `resource.name` is joined.
    pub explicit_path: bool,
    pub options: DownloadOptions,
    pub events: mpsc::UnboundedSender<Event>,
}

impl FetchContext {
    /// Compute the effective destination file path for single-file protocols.
    pub fn target_path(&self) -> PathBuf {
        if self.explicit_path {
            self.dest.clone()
        } else {
            self.dest.join(&self.resource.name)
        }
    }
}

/// Protocol backend trait. Implement this to add new download protocols
/// (BitTorrent, Magnet, FTP/SFTP, S3, etc.) without touching core scheduling.
pub trait Fetcher: Send + Sync {
    /// Unique identifier for this fetcher (e.g. `"http"`, `"bt"`, `"ftp"`).
    fn name(&self) -> &'static str;

    /// URL schemes handled by this fetcher (lowercase, e.g. `&["http", "https"]`).
    fn schemes(&self) -> &'static [&'static str];

    /// Inspect a URL and return its [`Resource`] metadata without transferring payload.
    fn resolve<'a>(&'a self, req: &'a ResolveRequest) -> BoxFuture<'a, Result<Resource, Error>>;

    /// Execute the download for a previously resolved [`Resource`].
    fn download<'a>(&'a self, ctx: FetchContext) -> BoxFuture<'a, Result<DownloadOutcome, Error>>;
}

/// Optional hook invoked before protocol resolution — the extension seam for
/// custom site extractors (QuickJS/WASM) to rewrite URLs, inject auth headers,
/// or return a synthesized [`Resource`].
pub trait ResourceInterceptor: Send + Sync {
    fn name(&self) -> &str;

    /// Return `Some(ResolveRequest)` to rewrite the request before protocol resolution,
    /// or `None` to pass through untouched.
    fn rewrite<'a>(
        &'a self,
        req: &'a ResolveRequest,
    ) -> BoxFuture<'a, Result<Option<ResolveRequest>, Error>>;
}

/// Registry dispatching [`ResolveRequest`]s and downloads to registered [`Fetcher`]s.
#[derive(Clone, Default)]
pub struct FetcherRegistry {
    by_scheme: HashMap<String, Arc<dyn Fetcher>>,
    by_name: HashMap<String, Arc<dyn Fetcher>>,
    interceptors: Vec<Arc<dyn ResourceInterceptor>>,
}

impl FetcherRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry pre-populated with the built-in [`HttpFetcher`]
    /// (covering HTTP/HTTPS files, HLS, and DASH) and built-in smart extractors
    /// (Hugging Face, Google Drive, GitHub).
    pub fn with_defaults(client: reqwest::Client) -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(HttpFetcher::new(client)));
        crate::extractors::register_builtin_extractors(&mut reg);
        reg
    }

    /// Register a protocol [`Fetcher`] for all of its declared schemes.
    pub fn register(&mut self, fetcher: Arc<dyn Fetcher>) {
        self.by_name
            .insert(fetcher.name().to_string(), Arc::clone(&fetcher));
        for &scheme in fetcher.schemes() {
            self.by_scheme
                .insert(scheme.to_ascii_lowercase(), Arc::clone(&fetcher));
        }
    }

    /// Add a pre-resolve [`ResourceInterceptor`] (e.g. an extension hook).
    pub fn add_interceptor(&mut self, interceptor: Arc<dyn ResourceInterceptor>) {
        self.interceptors.push(interceptor);
    }

    /// List registered protocol names.
    pub fn protocols(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Look up the [`Fetcher`] for a URL scheme.
    pub fn fetcher_for_scheme(&self, scheme: &str) -> Option<Arc<dyn Fetcher>> {
        self.by_scheme.get(&scheme.to_ascii_lowercase()).cloned()
    }

    /// Look up a [`Fetcher`] by its registered name.
    pub fn fetcher_by_name(&self, name: &str) -> Option<Arc<dyn Fetcher>> {
        self.by_name.get(name).cloned()
    }

    /// Run interceptors and resolve `req` via the matching protocol [`Fetcher`].
    /// Returns both the (possibly rewritten) request and the resolved [`Resource`].
    pub async fn resolve(&self, req: &ResolveRequest) -> Result<(ResolveRequest, Resource), Error> {
        let mut effective = req.clone();
        for interceptor in &self.interceptors {
            if let Some(updated) = interceptor.rewrite(&effective).await? {
                effective = updated;
            }
        }

        let scheme = effective
            .scheme()
            .ok_or_else(|| Error::UnsupportedProtocol(effective.url.clone()))?;
        let fetcher = self
            .fetcher_for_scheme(&scheme)
            .ok_or(Error::UnsupportedProtocol(scheme))?;

        let resource = fetcher.resolve(&effective).await?;
        Ok((effective, resource))
    }

    /// Execute a download using the fetcher recorded in `ctx.resource.meta.protocol`
    /// (falling back to URL scheme dispatch).
    pub async fn download(&self, ctx: FetchContext) -> Result<DownloadOutcome, Error> {
        let fetcher = self
            .fetcher_by_name(&ctx.resource.meta.protocol)
            .or_else(|| {
                ctx.request
                    .scheme()
                    .and_then(|s| self.fetcher_for_scheme(&s))
            })
            .ok_or_else(|| {
                Error::UnsupportedProtocol(ctx.resource.meta.protocol.clone())
            })?;
        fetcher.download(ctx).await
    }
}

/// Built-in HTTP/HTTPS fetcher supporting ranged multi-connection files,
/// HLS (`.m3u8`), and DASH (`.mpd`).
#[derive(Clone)]
pub struct HttpFetcher {
    client: reqwest::Client,
    resolve_retry: RetryPolicy,
}

impl HttpFetcher {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            resolve_retry: RetryPolicy::default(),
        }
    }

    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.resolve_retry = policy;
        self
    }
}

impl Fetcher for HttpFetcher {
    fn name(&self) -> &'static str {
        "http"
    }

    fn schemes(&self) -> &'static [&'static str] {
        &["http", "https"]
    }

    fn resolve<'a>(&'a self, req: &'a ResolveRequest) -> BoxFuture<'a, Result<Resource, Error>> {
        Box::pin(async move {
            let headers = req.header_map();
            let cancel = CancellationToken::new();
            let info = retry_async(&self.resolve_retry, &cancel, |_| {
                probe_with_headers(&self.client, &req.url, &headers)
            })
            .await?;

            let name = info
                .filename
                .clone()
                .unwrap_or_else(|| filename_from_url(&req.url));
            let category = Category::detect(&name, info.kind, info.content_type.as_deref());

            Ok(Resource {
                name: name.clone(),
                total_size: info.size,
                range_supported: info.accepts_ranges,
                files: vec![FileEntry {
                    index: 0,
                    name,
                    path: String::new(),
                    size: info.size,
                    selected: true,
                }],
                meta: ResourceMeta {
                    protocol: self.name().to_string(),
                    kind: info.kind,
                    category,
                    etag: info.etag,
                    content_type: info.content_type,
                    resolved_url: req.url.clone(),
                    extra: serde_json::Value::Null,
                },
            })
        })
    }

    fn download<'a>(&'a self, ctx: FetchContext) -> BoxFuture<'a, Result<DownloadOutcome, Error>> {
        Box::pin(async move {
            let headers = ctx.request.header_map();
            let dest = ctx.target_path();
            let spec = DownloadSpec {
                id: ctx.id,
                url: ctx.resource.meta.resolved_url.clone(),
                dest,
                headers: ctx.request.headers.clone(),
            };
            let info = ProbeInfo {
                size: ctx.resource.total_size,
                accepts_ranges: ctx.resource.range_supported,
                filename: Some(ctx.resource.name.clone()),
                etag: ctx.resource.meta.etag.clone(),
                content_type: ctx.resource.meta.content_type.clone(),
                kind: ctx.resource.meta.kind,
            };
            download_with_probe(&self.client, spec, &headers, info, &ctx.options, ctx.events).await
        })
    }
}
