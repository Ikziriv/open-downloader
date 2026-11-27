//! Unified, persistent application configuration (`config.json`) shared by
//! `od-cli`, `od-api`, and `od-app`.
//!
//! Covers four domains:
//! - **Downloads & Library**: default folder, IDM-style category subfolders,
//!   per-category custom folder overrides, parallel connections, rate limit, retries.
//! - **Startup**: OS-native auto-startup toggle and auto-resume on launch.
//! - **Security & Network**: API token enforcement, filename sanitization,
//!   TLS verification, optional proxy URL.
//! - **Appearance**: theme mode (`system` / `dark` / `light`), compact rows, sparkline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::detective::NinjaSettings;
use crate::{Category, DownloadOptions, Error, RetryPolicy, auth_token_path};

/// UI theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Dark,
    Light,
}

/// Download engine & IDM-style library organization settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DownloadSettings {
    /// Base destination directory for all downloads.
    pub download_dir: PathBuf,
    /// Automatically sort files into `Video/`, `Audio/`, `Documents/`, etc.
    pub organize_by_category: bool,
    /// Optional per-category custom directory overrides (e.g. `"Video" -> "D:\\Movies"`).
    pub category_dirs: BTreeMap<String, PathBuf>,
    /// Maximum files downloading concurrently.
    pub max_concurrent_downloads: usize,
    /// Parallel HTTP connections per file (`1..=32`).
    pub connections_per_download: usize,
    /// Optional global speed cap in bytes/sec (`None` = unlimited).
    pub rate_limit_bps: Option<u64>,
    /// Retry attempts on transient network / 5xx errors (`1..=10`).
    pub retry_attempts: u32,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            download_dir: default_downloads_dir(),
            organize_by_category: true,
            category_dirs: BTreeMap::new(),
            max_concurrent_downloads: 3,
            connections_per_download: 8,
            rate_limit_bps: None,
            retry_attempts: 4,
        }
    }
}

/// Startup & background daemon behavior.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StartupSettings {
    /// Launch Open Downloader automatically when the user logs in.
    pub auto_start: bool,
    /// Automatically resume paused/interrupted tasks on startup.
    pub auto_resume_on_start: bool,
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self {
            auto_start: false,
            auto_resume_on_start: true,
        }
    }
}

/// Security & network transport controls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// Require `x-od-token` / `Bearer` authentication on local API & MCP routes.
    pub require_api_token: bool,
    /// Strip path traversal (`../`) and OS-illegal characters from remote filenames.
    pub sanitize_filenames: bool,
    /// Verify HTTPS TLS certificates (disable only for local testing).
    pub verify_tls: bool,
    /// Optional HTTP/HTTPS/SOCKS5 proxy URL (e.g. `http://127.0.0.1:7890`).
    pub proxy_url: Option<String>,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            require_api_token: true,
            sanitize_filenames: true,
            verify_tls: true,
            proxy_url: None,
        }
    }
}

/// Visual & interface preferences.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemeMode,
    pub compact_rows: bool,
    pub show_sparkline: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            compact_rows: false,
            show_sparkline: true,
        }
    }
}

/// Complete Open Downloader application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub downloads: DownloadSettings,
    pub startup: StartupSettings,
    pub security: SecuritySettings,
    pub appearance: AppearanceSettings,
    pub ninja: NinjaSettings,
}

impl AppConfig {
    /// Default location of `config.json` (next to `auth.token` and `tasks.json`).
    pub fn default_path() -> PathBuf {
        auth_token_path().with_file_name("config.json")
    }

    /// Load configuration from `default_path()`, falling back to defaults if
    /// missing or malformed.
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// Load configuration from an explicit path, sanitizing numeric bounds.
    pub fn load_from(path: &Path) -> Self {
        let mut cfg: Self = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        cfg.normalize();
        cfg
    }

    /// Clamp settings to safe operational ranges.
    pub fn normalize(&mut self) {
        self.downloads.max_concurrent_downloads =
            self.downloads.max_concurrent_downloads.clamp(1, 16);
        self.downloads.connections_per_download =
            self.downloads.connections_per_download.clamp(1, 32);
        self.downloads.retry_attempts = self.downloads.retry_attempts.clamp(1, 10);
        if let Some(ref mut proxy) = self.security.proxy_url
            && proxy.trim().is_empty()
        {
            self.security.proxy_url = None;
        }
    }

    /// Atomically persist configuration to `default_path()`.
    pub fn save(&self) -> Result<(), Error> {
        self.save_to(&Self::default_path())
    }

    /// Atomically persist configuration to an explicit path via write-tmp-then-rename.
    pub fn save_to(&self, path: &Path) -> Result<(), Error> {
        let mut copy = self.clone();
        copy.normalize();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let bytes = serde_json::to_vec_pretty(&copy)?;
        std::fs::write(&tmp, bytes)?;
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    }

    /// Resolve the destination directory for a given [`Category`], honouring
    /// IDM-style per-category folder overrides first, then `download_dir/<Category>`,
    /// or `download_dir` when `organize_by_category` is off.
    pub fn output_dir_for(&self, cat: Category) -> PathBuf {
        if let Some(custom) = self.downloads.category_dirs.get(cat.dir_name())
            && !custom.as_os_str().is_empty()
        {
            return custom.clone();
        }
        if self.downloads.organize_by_category && cat != Category::Other {
            self.downloads.download_dir.join(cat.dir_name())
        } else {
            self.downloads.download_dir.clone()
        }
    }

    /// Build a [`DownloadOptions`] template from this configuration.
    pub fn to_download_options(&self) -> DownloadOptions {
        DownloadOptions {
            connections: self.downloads.connections_per_download,
            rate_limit: self.downloads.rate_limit_bps,
            organize: self.downloads.organize_by_category,
            retry: RetryPolicy {
                max_attempts: self.downloads.retry_attempts,
                ..RetryPolicy::default()
            },
            ..DownloadOptions::default()
        }
    }
}

fn default_downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_and_clamps_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = AppConfig::default();
        cfg.downloads.connections_per_download = 999;
        cfg.downloads.max_concurrent_downloads = 0;
        cfg.downloads
            .category_dirs
            .insert("Video".into(), PathBuf::from("/media/movies"));
        cfg.appearance.theme = ThemeMode::Dark;
        cfg.security.proxy_url = Some("   ".into());
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.downloads.connections_per_download, 32);
        assert_eq!(loaded.downloads.max_concurrent_downloads, 1);
        assert_eq!(loaded.appearance.theme, ThemeMode::Dark);
        assert_eq!(loaded.security.proxy_url, None);
        assert_eq!(
            loaded.output_dir_for(Category::Video),
            PathBuf::from("/media/movies")
        );
        assert_eq!(
            loaded.output_dir_for(Category::Audio),
            loaded.downloads.download_dir.join("Audio")
        );
    }
}
