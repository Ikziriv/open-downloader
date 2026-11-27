//! File-type grouping: every download lands in a category subfolder of the
//! destination directory, the way IDM sorts into Compressed/Documents/
//! Video/etc. Detection is filename-extension first, Content-Type as the
//! fallback for extension-less URLs, and stream manifests always Video.

use crate::StreamKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Video,
    Audio,
    Documents,
    Images,
    Compressed,
    Programs,
    /// Anything unrecognized — stays in the base folder, no move.
    Other,
}

impl Category {
    /// Subfolder name under the destination directory.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Documents => "Documents",
            Self::Images => "Images",
            Self::Compressed => "Compressed",
            Self::Programs => "Programs",
            Self::Other => "Other",
        }
    }

    /// Category by file extension (case-insensitive).
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
            | "3gp" | "ts" | "m2ts" | "vob" => Self::Video,
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "oga" | "m4a" | "wma" | "opus" | "mid"
            | "midi" => Self::Audio,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "csv" | "rtf"
            | "odt" | "ods" | "odp" | "epub" | "mobi" | "md" => Self::Documents,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tif" | "tiff"
            | "avif" | "heic" => Self::Images,
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "tgz" | "iso" | "cab"
            | "jar" => Self::Compressed,
            "exe" | "msi" | "dmg" | "apk" | "deb" | "rpm" | "pkg" | "appimage" | "bat"
            | "ps1" | "sh" => Self::Programs,
            _ => Self::Other,
        }
    }

    /// Category by HTTP Content-Type — the fallback for extension-less
    /// names like `/download?id=123`.
    pub fn from_content_type(ct: &str) -> Self {
        let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        match ct.split_once('/').map(|(major, _)| major) {
            Some("video") => return Self::Video,
            Some("audio") => return Self::Audio,
            Some("image") => return Self::Images,
            _ => {}
        }
        match ct.as_str() {
            "application/pdf"
            | "application/msword"
            | "application/rtf"
            | "text/plain"
            | "text/csv" => Self::Documents,
            c if c.starts_with("application/vnd.openxmlformats-officedocument") => {
                Self::Documents
            }
            "application/zip"
            | "application/x-rar-compressed"
            | "application/vnd.rar"
            | "application/x-7z-compressed"
            | "application/gzip"
            | "application/x-tar"
            | "application/x-bzip2"
            | "application/x-xz" => Self::Compressed,
            "application/x-msdownload"
            | "application/x-msi"
            | "application/vnd.microsoft.portable-executable"
            | "application/x-apple-diskimage" => Self::Programs,
            _ => Self::Other,
        }
    }

    /// Best guess for a download: filename extension → stream kind →
    /// Content-Type → Other. The kind beats Content-Type because manifests
    /// are often served as `audio/mpegurl` — a playlist is always Video.
    pub fn detect(file_name: &str, kind: StreamKind, content_type: Option<&str>) -> Self {
        if let Some(ext) = file_name.rsplit('.').next().filter(|e| {
            *e != file_name && !e.is_empty() && e.len() <= 8
        }) {
            let cat = Self::from_extension(ext);
            if cat != Self::Other {
                return cat;
            }
        }
        if kind != StreamKind::File {
            return Self::Video;
        }
        if let Some(ct) = content_type {
            return Self::from_content_type(ct);
        }
        Self::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_folders() {
        assert_eq!(Category::from_extension("MP4"), Category::Video);
        assert_eq!(Category::from_extension("docx"), Category::Documents);
        assert_eq!(Category::from_extension("ZIP"), Category::Compressed);
        assert_eq!(Category::from_extension("nope"), Category::Other);
    }

    #[test]
    fn content_type_is_the_fallback() {
        assert_eq!(
            Category::from_content_type("application/pdf"),
            Category::Documents
        );
        assert_eq!(
            Category::from_content_type("video/webm; codecs=vp9"),
            Category::Video
        );
        assert_eq!(
            Category::from_content_type("application/octet-stream"),
            Category::Other
        );
    }

    #[test]
    fn detect_prefers_extension_then_type_then_stream() {
        assert_eq!(
            Category::detect("a.zip", StreamKind::File, Some("video/mp4")),
            Category::Compressed
        );
        assert_eq!(
            Category::detect("download", StreamKind::File, Some("application/pdf")),
            Category::Documents
        );
        assert_eq!(
            Category::detect("master.m3u8", StreamKind::Hls, None),
            Category::Video
        );
        assert_eq!(
            Category::detect("file.bin", StreamKind::File, None),
            Category::Other
        );
    }
}
