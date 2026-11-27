/// Loopback port where `od-app` accepts `POST /add` requests from the
/// browser native-messaging host.
pub const APP_PORT: u16 = 19799;

mod auth;
pub mod autostart;
mod category;
pub mod config;
pub mod conflicts;
mod dash;
pub mod detective;
mod engine;
pub mod extractors;
pub mod fetcher;
mod hls;
mod journal;
mod probe;
pub mod retry;
pub mod smart;
pub mod speedtest;
mod stream;
pub mod task;
pub mod verify;

pub use auth::{auth_token, auth_token_path};
pub use category::Category;
pub use config::{
    AppConfig, AppearanceSettings, DownloadSettings, SecuritySettings, StartupSettings, ThemeMode,
};
pub use conflicts::{
    DetectedDownloader, KNOWN_DOWNLOADERS, KnownDownloader, scan_downloaders, start_downloader,
    stop_downloader,
};
pub use detective::{DetectedUrl, NinjaAction, NinjaDetective, NinjaMode, NinjaSettings};
pub use engine::{
    DownloadOptions, DownloadOutcome, DownloadSpec, Event, FinishReason, Scheduler, download, run,
};
pub use fetcher::{
    BoxFuture, FetchContext, Fetcher, FetcherRegistry, FileEntry, HttpFetcher, ResolveRequest,
    Resource, ResourceInterceptor, ResourceMeta,
};
pub use probe::{
    ProbeInfo, StreamKind, filename_from_url, parse_content_disposition, probe, probe_with_headers,
    sanitize_filename,
};
pub use retry::RetryPolicy;
pub use smart::{
    SmartPlan, SmartQueue, SmartTaskCandidate, TrafficBandit, TrafficTier, decide_initial_plan,
};
pub use speedtest::{
    ConnectionQuality, SpeedTestConfig, SpeedTestEvent, SpeedTestPhase, SpeedTestReport,
    run_speedtest,
};
pub use verify::{Sha256, sha256_file, verify_file_sha256};
pub use task::{
    CreateTaskRequest, FileTaskStore, MemoryTaskStore, TaskEvent, TaskId, TaskManager,
    TaskPriority, TaskProgress, TaskRecord, TaskStatus, TaskStore, export_tasks_csv,
};
pub use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status error: {0}")]
    HttpStatus(u16),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server refused range requests (status {0})")]
    RangeRefused(u16),
    #[error("journal error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cancelled")]
    Cancelled,
    #[error("worker task failed: {0}")]
    Worker(String),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("unsupported protocol: {0}")]
    UnsupportedProtocol(String),
    #[error("task not found: {0}")]
    TaskNotFound(u64),
    #[error("invalid task state: {0}")]
    InvalidState(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
}
