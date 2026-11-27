//! Background "Ninja Detective" URL discovery engine supporting **Global** and
//! **Focused** modes with deduplication and safe Grabber Queue / Auto-Download actions.
//!
//! Designed to consume text from the OS clipboard or URLs sniffed by the browser
//! extension without invasive system hooks or antivirus-triggering injection.

use std::collections::{HashSet, VecDeque};

use crate::extractors::{
    rewrite_gdrive_url, rewrite_github_blob_url, rewrite_huggingface_url,
};
use crate::probe::{StreamKind, filename_from_url};

/// Operating scope for background URL detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NinjaMode {
    /// Completely dormant.
    Off,
    /// Inspects all copied/sniffed downloadable URLs (minus `excluded_domains`).
    #[default]
    Global,
    /// Only triggers on user-selected `focused_domains` or `focused_extensions`.
    Focused,
}

impl NinjaMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Global => "Global Ninja",
            Self::Focused => "Focused Ninja",
        }
    }
}

/// Action taken when Ninja Detective discovers a matching URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NinjaAction {
    /// Safe default: places URLs in a 1-click Grabber inbox for user confirmation.
    #[default]
    GrabberQueue,
    /// Immediately resolves and starts downloading matching URLs in the background.
    AutoDownload,
}

/// Persistent Ninja Detective settings stored in `AppConfig::ninja`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NinjaSettings {
    pub mode: NinjaMode,
    pub action: NinjaAction,
    pub watch_clipboard: bool,
    pub watch_browser: bool,
    /// Domains targeted in [`NinjaMode::Focused`] (e.g. `huggingface.co`, `github.com`).
    pub focused_domains: Vec<String>,
    /// File extensions targeted in [`NinjaMode::Focused`] (lowercase, no leading dot).
    pub focused_extensions: Vec<String>,
    /// Domains always ignored in both Global and Focused modes.
    pub excluded_domains: Vec<String>,
}

impl Default for NinjaSettings {
    fn default() -> Self {
        Self {
            mode: NinjaMode::Global,
            action: NinjaAction::GrabberQueue,
            watch_clipboard: true,
            watch_browser: true,
            focused_domains: vec![
                "huggingface.co".into(),
                "github.com".into(),
                "drive.google.com".into(),
            ],
            focused_extensions: vec![
                "zip".into(),
                "7z".into(),
                "rar".into(),
                "tar".into(),
                "gz".into(),
                "iso".into(),
                "exe".into(),
                "msi".into(),
                "dmg".into(),
                "mp4".into(),
                "mkv".into(),
                "webm".into(),
                "mp3".into(),
                "flac".into(),
                "pdf".into(),
                "m3u8".into(),
                "mpd".into(),
                "safetensors".into(),
                "gguf".into(),
            ],
            excluded_domains: vec!["localhost".into(), "127.0.0.1".into()],
        }
    }
}

/// A URL discovered by Ninja Detective.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectedUrl {
    pub url: String,
    pub filename: String,
    /// Source sensor (`"clipboard"` or `"browser"`).
    pub source: String,
    /// Why this URL matched (e.g. `"extension:.zip"`, `"domain:huggingface.co"`, `"stream:hls"`).
    pub reason: String,
}

/// Stateful, deduplicating URL detector.
pub struct NinjaDetective {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl Default for NinjaDetective {
    fn default() -> Self {
        Self::new(256)
    }
}

impl NinjaDetective {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(16),
        }
    }

    /// Inspect raw text (e.g. clipboard content) and return newly matched URLs.
    pub fn inspect_text(
        &mut self,
        text: &str,
        source: &str,
        settings: &NinjaSettings,
    ) -> Vec<DetectedUrl> {
        if settings.mode == NinjaMode::Off {
            return Vec::new();
        }
        let mut found = Vec::new();
        for url in extract_candidate_urls(text) {
            if self.seen.contains(&url) {
                continue;
            }
            if let Some(reason) = match_reason(&url, settings) {
                self.remember(&url);
                found.push(DetectedUrl {
                    filename: filename_from_url(&url),
                    url,
                    source: source.to_string(),
                    reason,
                });
            }
        }
        found
    }

    /// Forget previously seen URLs (e.g. when the user resets the Grabber queue).
    pub fn clear_seen(&mut self) {
        self.seen.clear();
        self.order.clear();
    }

    fn remember(&mut self, url: &str) {
        if self.seen.insert(url.to_string()) {
            self.order.push_back(url.to_string());
            while self.order.len() > self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

/// Extract clean `http://` and `https://` URLs from arbitrary text.
pub fn extract_candidate_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| {
            matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';')
        });
        let trimmed = trimmed.trim_end_matches('.');
        if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
            && reqwest::Url::parse(trimmed).is_ok()
            && !out.iter().any(|u| u == trimmed)
        {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Evaluate whether `url` matches the current [`NinjaSettings`], returning the
/// human-readable match reason if it should be captured.
pub fn match_reason(url: &str, settings: &NinjaSettings) -> Option<String> {
    if settings.mode == NinjaMode::Off {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();

    // Always respect excluded domains.
    if settings.excluded_domains.iter().any(|ex| {
        let ex = ex.trim().to_ascii_lowercase();
        !ex.is_empty() && (host == ex || host.ends_with(&format!(".{ex}")))
    }) {
        return None;
    }

    // Check streaming manifests first.
    match StreamKind::sniff(url, None) {
        StreamKind::Hls => return Some("stream:hls".into()),
        StreamKind::Dash => return Some("stream:dash".into()),
        StreamKind::File => {}
    }

    // Check built-in smart extractors (Hugging Face, Google Drive, GitHub).
    let is_smart = rewrite_huggingface_url(url).is_some()
        || rewrite_gdrive_url(url).is_some()
        || rewrite_github_blob_url(url).is_some();

    let ext = url_extension(&parsed);

    match settings.mode {
        NinjaMode::Off => None,
        NinjaMode::Focused => {
            if let Some(d) = settings.focused_domains.iter().find(|d| {
                let d = d.trim().to_ascii_lowercase();
                !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
            }) {
                return Some(format!("domain:{d}"));
            }
            if let Some(e) = &ext
                && settings
                    .focused_extensions
                    .iter()
                    .any(|fe| fe.trim().eq_ignore_ascii_case(e))
            {
                return Some(format!("ext:.{e}"));
            }
            None
        }
        NinjaMode::Global => {
            if is_smart {
                return Some(format!("extractor:{host}"));
            }
            if let Some(e) = &ext
                && settings
                    .focused_extensions
                    .iter()
                    .any(|fe| fe.trim().eq_ignore_ascii_case(e))
            {
                return Some(format!("ext:.{e}"));
            }
            None
        }
    }
}

fn url_extension(parsed: &reqwest::Url) -> Option<String> {
    let seg = parsed.path_segments()?.next_back()?;
    let (_, ext) = seg.rsplit_once('.')?;
    if !ext.is_empty() && ext.len() <= 12 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext.to_ascii_lowercase())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_and_focused_modes_filter_and_deduplicate() {
        let mut ninja = NinjaDetective::default();
        let mut cfg = NinjaSettings::default();

        // Global catches .zip, .m3u8, and smart extractors, ignores plain web pages & localhost.
        let text = "Check https://example.com/app.zip and https://example.com/about and \
                    http://127.0.0.1:8080/local.zip and https://cdn.site/live/master.m3u8";
        let hits = ninja.inspect_text(text, "clipboard", &cfg);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].filename, "app.zip");
        assert_eq!(hits[1].reason, "stream:hls");

        // Re-inspecting the same text produces 0 duplicates.
        assert!(ninja.inspect_text(text, "clipboard", &cfg).is_empty());

        // Focused mode restricts to configured domains/extensions.
        ninja.clear_seen();
        cfg.mode = NinjaMode::Focused;
        cfg.focused_domains = vec!["huggingface.co".into()];
        cfg.focused_extensions = vec!["safetensors".into()];
        let hits2 = ninja.inspect_text(
            "https://example.com/app.zip https://huggingface.co/org/model/blob/main/w.safetensors",
            "clipboard",
            &cfg,
        );
        assert_eq!(hits2.len(), 1);
        assert!(hits2[0].url.contains("huggingface.co"));
    }
}
