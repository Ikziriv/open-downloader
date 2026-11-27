//! Cross-platform detection and graceful coexistence management for other
//! download managers (IDM, Gopeed, FDM, NeatDM, Motrix, JDownloader 2, aria2,
//! qBittorrent, XDM).
//!
//! Uses standard, privilege-free OS user-space tools (`tasklist`/`taskkill` on
//! Windows, `ps`/`kill` on Linux & macOS). Never kills third-party apps
//! automatically — stopping is always an explicit, graceful user action.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::Error;

/// Static descriptor of a known third-party download manager.
#[derive(Debug, Clone, Copy)]
pub struct KnownDownloader {
    pub id: &'static str,
    pub name: &'static str,
    /// Lowercase process image names (with or without `.exe`) to match.
    pub process_names: &'static [&'static str],
}

/// Curated registry of common competing download managers.
pub const KNOWN_DOWNLOADERS: &[KnownDownloader] = &[
    KnownDownloader {
        id: "idm",
        name: "Internet Download Manager (IDM)",
        process_names: &["idman.exe", "idman"],
    },
    KnownDownloader {
        id: "gopeed",
        name: "Gopeed",
        process_names: &["gopeed.exe", "gopeed"],
    },
    KnownDownloader {
        id: "fdm",
        name: "Free Download Manager",
        process_names: &["fdm.exe", "fdm"],
    },
    KnownDownloader {
        id: "neatdm",
        name: "Neat Download Manager",
        process_names: &["neatdm.exe", "neatdownloadmanager"],
    },
    KnownDownloader {
        id: "motrix",
        name: "Motrix",
        process_names: &["motrix.exe", "motrix"],
    },
    KnownDownloader {
        id: "jdownloader",
        name: "JDownloader 2",
        process_names: &["jdownloader2.exe", "jdownloader2"],
    },
    KnownDownloader {
        id: "aria2",
        name: "aria2",
        process_names: &["aria2c.exe", "aria2c"],
    },
    KnownDownloader {
        id: "qbittorrent",
        name: "qBittorrent",
        process_names: &["qbittorrent.exe", "qbittorrent"],
    },
    KnownDownloader {
        id: "xdm",
        name: "Xtreme Download Manager",
        process_names: &["xdm.exe", "xdm-app", "xdm"],
    },
];

/// Live status of a known download manager on the current machine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectedDownloader {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub pids: Vec<u32>,
}

/// Scan running processes and return all known download managers with their live
/// running status and PIDs (running apps sorted first).
pub fn scan_downloaders() -> Vec<DetectedDownloader> {
    let procs = list_processes();
    match_known_processes(&procs)
}

/// Gracefully stop all running processes of a known downloader by `id`.
/// Returns the number of PIDs signaled.
pub fn stop_downloader(id: &str, force: bool) -> Result<usize, Error> {
    let list = scan_downloaders();
    let Some(app) = list.into_iter().find(|d| d.id == id) else {
        return Err(Error::InvalidState(format!("unknown downloader id: {id}")));
    };
    if app.pids.is_empty() {
        return Ok(0);
    }

    let mut stopped = 0usize;
    for pid in app.pids {
        if stop_pid(pid, force)? {
            stopped += 1;
        }
    }
    Ok(stopped)
}

/// Launch a downloader executable (used to turn a stopped app back on).
pub fn start_downloader(executable: &Path) -> Result<(), Error> {
    Command::new(executable).spawn()?;
    Ok(())
}

/// Match raw `(pid, process_name)` pairs against [`KNOWN_DOWNLOADERS`].
pub fn match_known_processes(procs: &[(u32, String)]) -> Vec<DetectedDownloader> {
    let mut pids_by_id: HashMap<&'static str, Vec<u32>> = HashMap::new();
    for (pid, name) in procs {
        let lower = name.trim().to_ascii_lowercase();
        let base = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
        for known in KNOWN_DOWNLOADERS {
            if known.process_names.contains(&base) {
                pids_by_id.entry(known.id).or_default().push(*pid);
            }
        }
    }

    let mut out: Vec<DetectedDownloader> = KNOWN_DOWNLOADERS
        .iter()
        .map(|k| {
            let mut pids = pids_by_id.remove(k.id).unwrap_or_default();
            pids.sort_unstable();
            pids.dedup();
            DetectedDownloader {
                id: k.id.to_string(),
                name: k.name.to_string(),
                running: !pids.is_empty(),
                pids,
            }
        })
        .collect();

    // Running downloaders first, then alphabetical by name.
    out.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Parse Windows `tasklist /FO CSV /NH` output into `(pid, image_name)` pairs.
pub fn parse_tasklist_csv(output: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        let mut cols = line.split("\",\"");
        let Some(raw_name) = cols.next() else {
            continue;
        };
        let Some(raw_pid) = cols.next() else {
            continue;
        };
        let name = raw_name.trim_matches('"').trim();
        let pid_str = raw_pid.trim_matches('"').trim();
        if let Ok(pid) = pid_str.parse::<u32>()
            && !name.is_empty()
        {
            out.push((pid, name.to_string()));
        }
    }
    out
}

/// Parse Unix `ps -eo pid=,comm=` output into `(pid, command)` pairs.
pub fn parse_ps_output(output: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let Some(pid_str) = parts.next() else {
            continue;
        };
        let Some(comm) = parts.next() else {
            continue;
        };
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let name = comm.trim();
            if !name.is_empty() {
                out.push((pid, name.to_string()));
            }
        }
    }
    out
}

fn list_processes() -> Vec<(u32, String)> {
    #[cfg(target_os = "windows")]
    {
        let Ok(out) = Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
        else {
            return Vec::new();
        };
        parse_tasklist_csv(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let Ok(out) = Command::new("ps").args(["-eo", "pid=,comm="]).output() else {
            return Vec::new();
        };
        parse_ps_output(&String::from_utf8_lossy(&out.stdout))
    }
}

fn stop_pid(pid: u32, force: bool) -> Result<bool, Error> {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string()]);
        if force {
            cmd.arg("/F");
        }
        let status = cmd.status()?;
        Ok(status.success())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let sig = if force { "-KILL" } else { "-TERM" };
        let status = Command::new("kill").args([sig, &pid.to_string()]).status()?;
        Ok(status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_tasklist_and_matches_downloaders() {
        let sample = "\"IDMan.exe\",\"4120\",\"Console\",\"1\",\"42,000 K\"\n\
                      \"chrome.exe\",\"9001\",\"Console\",\"1\",\"120,000 K\"\n\
                      \"aria2c.exe\",\"5100\",\"Console\",\"1\",\"15,000 K\"\n";
        let procs = parse_tasklist_csv(sample);
        let found = match_known_processes(&procs);
        let running: Vec<&str> = found
            .iter()
            .filter(|d| d.running)
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(running.len(), 2);
        assert!(running.contains(&"idm"));
        assert!(running.contains(&"aria2"));
    }

    #[test]
    fn parses_unix_ps_output_and_matches_downloaders() {
        let sample = "  1204 gopeed\n  2291 /usr/bin/qbittorrent\n  9999 bash\n";
        let procs = parse_ps_output(sample);
        let found = match_known_processes(&procs);
        let running: Vec<&str> = found
            .iter()
            .filter(|d| d.running)
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(running.len(), 2);
        assert!(running.contains(&"gopeed"));
        assert!(running.contains(&"qbittorrent"));
    }
}
