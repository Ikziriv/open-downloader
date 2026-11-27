//! Cross-platform user-level auto-startup configuration (Windows, macOS, Linux).
//!
//! Uses each OS's native, privilege-free (current-user) mechanism:
//! - **Windows**: Registry `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!   for GUI apps, or Task Scheduler XML (`LogonTrigger` + hidden window +
//!   restart-on-failure) for the headless `od serve` daemon.
//! - **macOS**: LaunchAgent plist in `~/Library/LaunchAgents/com.opendownloader.daemon.plist`
//!   with `RunAtLoad` and crash-only `KeepAlive`.
//! - **Linux**: `systemd --user` unit in `~/.config/systemd/user/open-downloader.service`
//!   (with `Restart=on-failure`, `NoNewPrivileges=true`) and XDG `.desktop` entry
//!   in `~/.config/autostart/open-downloader.desktop`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

pub const SERVICE_LABEL: &str = "com.opendownloader.daemon";
pub const WINDOWS_TASK_NAME: &str = "OpenDownloaderDaemon";
pub const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const WINDOWS_RUN_VALUE: &str = "OpenDownloader";

/// Configuration for registering an auto-start entry.
#[derive(Debug, Clone)]
pub struct AutoStartConfig {
    /// Absolute path to the executable (`od` or `od-app`).
    pub executable: PathBuf,
    /// CLI arguments passed on launch (e.g. `["serve"]` for the headless daemon).
    pub args: Vec<String>,
    /// Optional working directory.
    pub working_dir: Option<PathBuf>,
    /// Optional log directory (used by macOS LaunchAgent).
    pub log_dir: Option<PathBuf>,
}

impl AutoStartConfig {
    /// Default configuration for starting the current `od` binary in headless daemon mode.
    pub fn daemon(executable: impl Into<PathBuf>) -> Self {
        let executable = executable.into();
        let working_dir = executable.parent().map(Path::to_path_buf);
        Self {
            executable,
            args: vec!["serve".into()],
            working_dir,
            log_dir: None,
        }
    }
}

/// Current auto-startup registration status.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AutoStartStatus {
    pub platform: &'static str,
    pub mechanism: &'static str,
    pub enabled: bool,
    pub target: String,
}

/// Render a macOS LaunchAgent `.plist` XML document.
pub fn render_launchd_plist(cfg: &AutoStartConfig) -> String {
    let mut program_args = format!(
        "        <string>{}</string>\n",
        xml_escape(&cfg.executable.display().to_string())
    );
    for arg in &cfg.args {
        program_args.push_str(&format!("        <string>{}</string>\n", xml_escape(arg)));
    }
    let log_dir = cfg
        .log_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .display()
        .to_string();
    let stdout_path = xml_escape(&format!("{log_dir}/open-downloader.out.log"));
    let stderr_path = xml_escape(&format!("{log_dir}/open-downloader.err.log"));

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{stdout_path}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_path}</string>
</dict>
</plist>
"#
    )
}

/// Render a Linux `systemd --user` service unit.
pub fn render_systemd_unit(cfg: &AutoStartConfig) -> String {
    let mut exec = shell_quote(&cfg.executable.display().to_string());
    for arg in &cfg.args {
        exec.push(' ');
        exec.push_str(&shell_quote(arg));
    }
    let workdir = cfg
        .working_dir
        .as_ref()
        .map(|p| format!("WorkingDirectory={}\n", p.display()))
        .unwrap_or_default();

    format!(
        r#"[Unit]
Description=Open Downloader Headless Daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec}
{workdir}Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
"#
    )
}

/// Render a Linux XDG `.desktop` autostart entry.
pub fn render_xdg_desktop(cfg: &AutoStartConfig) -> String {
    let mut exec = shell_quote(&cfg.executable.display().to_string());
    for arg in &cfg.args {
        exec.push(' ');
        exec.push_str(&shell_quote(arg));
    }
    format!(
        r#"[Desktop Entry]
Type=Application
Version=1.0
Name=Open Downloader
Comment=High-speed multi-protocol download daemon
Exec={exec}
Terminal=false
Hidden=false
X-GNOME-Autostart-enabled=true
Categories=Network;FileTransfer;
"#
    )
}

/// Render a Windows Task Scheduler XML definition (runs silently on user logon,
/// restarts on failure, works on battery power, requires no UAC elevation).
pub fn render_windows_task_xml(cfg: &AutoStartConfig) -> String {
    let cmd = xml_escape(&cfg.executable.display().to_string());
    let args = xml_escape(&cfg.args.join(" "));
    let workdir = cfg
        .working_dir
        .as_ref()
        .map(|p| {
            format!(
                "\n      <WorkingDirectory>{}</WorkingDirectory>",
                xml_escape(&p.display().to_string())
            )
        })
        .unwrap_or_default();
    let args_tag = if args.is_empty() {
        String::new()
    } else {
        format!("\n      <Arguments>{args}</Arguments>")
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Open Downloader background daemon (REST API, SSE, and MCP)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>{args_tag}{workdir}
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Enable auto-startup on the current operating system.
pub fn enable(cfg: &AutoStartConfig) -> Result<AutoStartStatus, Error> {
    #[cfg(target_os = "windows")]
    {
        enable_windows(cfg)
    }
    #[cfg(target_os = "macos")]
    {
        enable_macos(cfg)
    }
    #[cfg(target_os = "linux")]
    {
        enable_linux(cfg)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = cfg;
        Err(Error::UnsupportedProtocol("autostart on this OS".into()))
    }
}

/// Disable auto-startup on the current operating system.
pub fn disable() -> Result<AutoStartStatus, Error> {
    #[cfg(target_os = "windows")]
    {
        disable_windows()
    }
    #[cfg(target_os = "macos")]
    {
        disable_macos()
    }
    #[cfg(target_os = "linux")]
    {
        disable_linux()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err(Error::UnsupportedProtocol("autostart on this OS".into()))
    }
}

/// Inspect whether auto-startup is currently enabled on this machine.
pub fn status() -> Result<AutoStartStatus, Error> {
    #[cfg(target_os = "windows")]
    {
        status_windows()
    }
    #[cfg(target_os = "macos")]
    {
        status_macos()
    }
    #[cfg(target_os = "linux")]
    {
        status_linux()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err(Error::UnsupportedProtocol("autostart on this OS".into()))
    }
}

// ---------------------------------------------------------------------------
// Windows implementation (Task Scheduler with HKCU\Run fallback)

#[cfg(target_os = "windows")]
fn enable_windows(cfg: &AutoStartConfig) -> Result<AutoStartStatus, Error> {
    let xml = render_windows_task_xml(cfg);
    let tmp = std::env::temp_dir().join("od-autostart-task.xml");
    // Write UTF-16LE with BOM as expected by `schtasks /Create /XML`.
    let mut utf16 = Vec::with_capacity(xml.len() * 2 + 2);
    utf16.extend_from_slice(&[0xFF, 0xFE]);
    for unit in xml.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&tmp, &utf16)?;

    let out = Command::new("schtasks")
        .args(["/Create", "/TN", WINDOWS_TASK_NAME, "/XML"])
        .arg(&tmp)
        .arg("/F")
        .output();
    let _ = std::fs::remove_file(&tmp);

    match out {
        Ok(o) if o.status.success() => Ok(AutoStartStatus {
            platform: "windows",
            mechanism: "task-scheduler",
            enabled: true,
            target: WINDOWS_TASK_NAME.into(),
        }),
        _ => {
            // Fallback to HKCU\...\Run when Task Scheduler is restricted by policy.
            let mut cmd = format!("\"{}\"", cfg.executable.display());
            for a in &cfg.args {
                cmd.push(' ');
                cmd.push_str(a);
            }
            let status = Command::new("reg")
                .args([
                    "add",
                    WINDOWS_RUN_KEY,
                    "/v",
                    WINDOWS_RUN_VALUE,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &cmd,
                    "/f",
                ])
                .status()?;
            if !status.success() {
                return Err(Error::Io(std::io::Error::other(
                    "failed to register Windows autostart",
                )));
            }
            Ok(AutoStartStatus {
                platform: "windows",
                mechanism: "hkcu-run",
                enabled: true,
                target: format!("{WINDOWS_RUN_KEY}\\{WINDOWS_RUN_VALUE}"),
            })
        }
    }
}

#[cfg(target_os = "windows")]
fn disable_windows() -> Result<AutoStartStatus, Error> {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"])
        .output();
    let _ = Command::new("reg")
        .args(["delete", WINDOWS_RUN_KEY, "/v", WINDOWS_RUN_VALUE, "/f"])
        .output();
    Ok(AutoStartStatus {
        platform: "windows",
        mechanism: "task-scheduler",
        enabled: false,
        target: WINDOWS_TASK_NAME.into(),
    })
}

#[cfg(target_os = "windows")]
fn status_windows() -> Result<AutoStartStatus, Error> {
    let task_ok = Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if task_ok {
        return Ok(AutoStartStatus {
            platform: "windows",
            mechanism: "task-scheduler",
            enabled: true,
            target: WINDOWS_TASK_NAME.into(),
        });
    }
    let reg_ok = Command::new("reg")
        .args(["query", WINDOWS_RUN_KEY, "/v", WINDOWS_RUN_VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    Ok(AutoStartStatus {
        platform: "windows",
        mechanism: if reg_ok { "hkcu-run" } else { "task-scheduler" },
        enabled: reg_ok,
        target: if reg_ok {
            format!("{WINDOWS_RUN_KEY}\\{WINDOWS_RUN_VALUE}")
        } else {
            WINDOWS_TASK_NAME.into()
        },
    })
}

// ---------------------------------------------------------------------------
// macOS implementation (LaunchAgent)

#[cfg(target_os = "macos")]
fn macos_plist_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn enable_macos(cfg: &AutoStartConfig) -> Result<AutoStartStatus, Error> {
    let mut cfg = cfg.clone();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let log_dir = cfg
        .log_dir
        .clone()
        .unwrap_or_else(|| home.join("Library/Logs/OpenDownloader"));
    std::fs::create_dir_all(&log_dir)?;
    cfg.log_dir = Some(log_dir);

    let plist_path = macos_plist_path();
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist_path, render_launchd_plist(&cfg))?;

    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .output();
    let _ = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .output();

    Ok(AutoStartStatus {
        platform: "macos",
        mechanism: "launchd",
        enabled: true,
        target: plist_path.display().to_string(),
    })
}

#[cfg(target_os = "macos")]
fn disable_macos() -> Result<AutoStartStatus, Error> {
    let plist_path = macos_plist_path();
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .output();
    let _ = std::fs::remove_file(&plist_path);
    Ok(AutoStartStatus {
        platform: "macos",
        mechanism: "launchd",
        enabled: false,
        target: plist_path.display().to_string(),
    })
}

#[cfg(target_os = "macos")]
fn status_macos() -> Result<AutoStartStatus, Error> {
    let plist_path = macos_plist_path();
    Ok(AutoStartStatus {
        platform: "macos",
        mechanism: "launchd",
        enabled: plist_path.exists(),
        target: plist_path.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Linux implementation (systemd --user with XDG autostart fallback)

#[cfg(target_os = "linux")]
fn linux_systemd_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("systemd/user/open-downloader.service")
}

#[cfg(target_os = "linux")]
fn linux_xdg_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("autostart/open-downloader.desktop")
}

#[cfg(target_os = "linux")]
fn enable_linux(cfg: &AutoStartConfig) -> Result<AutoStartStatus, Error> {
    let unit_path = linux_systemd_path();
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&unit_path, render_systemd_unit(cfg))?;

    let systemd_ok = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .is_ok_and(|s| s.success())
        && Command::new("systemctl")
            .args(["--user", "enable", "--now", "open-downloader.service"])
            .status()
            .is_ok_and(|s| s.success());

    if systemd_ok {
        return Ok(AutoStartStatus {
            platform: "linux",
            mechanism: "systemd-user",
            enabled: true,
            target: unit_path.display().to_string(),
        });
    }

    let desktop_path = linux_xdg_path();
    if let Some(parent) = desktop_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&desktop_path, render_xdg_desktop(cfg))?;
    Ok(AutoStartStatus {
        platform: "linux",
        mechanism: "xdg-autostart",
        enabled: true,
        target: desktop_path.display().to_string(),
    })
}

#[cfg(target_os = "linux")]
fn disable_linux() -> Result<AutoStartStatus, Error> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "open-downloader.service"])
        .output();
    let unit_path = linux_systemd_path();
    let _ = std::fs::remove_file(&unit_path);
    let _ = std::fs::remove_file(linux_xdg_path());
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok(AutoStartStatus {
        platform: "linux",
        mechanism: "systemd-user",
        enabled: false,
        target: unit_path.display().to_string(),
    })
}

#[cfg(target_os = "linux")]
fn status_linux() -> Result<AutoStartStatus, Error> {
    let unit_path = linux_systemd_path();
    if unit_path.exists() {
        return Ok(AutoStartStatus {
            platform: "linux",
            mechanism: "systemd-user",
            enabled: true,
            target: unit_path.display().to_string(),
        });
    }
    let desktop_path = linux_xdg_path();
    Ok(AutoStartStatus {
        platform: "linux",
        mechanism: if desktop_path.exists() {
            "xdg-autostart"
        } else {
            "systemd-user"
        },
        enabled: desktop_path.exists(),
        target: if desktop_path.exists() {
            desktop_path.display().to_string()
        } else {
            unit_path.display().to_string()
        },
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_cross_platform_configs() {
        let cfg = AutoStartConfig {
            executable: PathBuf::from("/usr/local/bin/od"),
            args: vec!["serve".into(), "--concurrency".into(), "4".into()],
            working_dir: Some(PathBuf::from("/usr/local/bin")),
            log_dir: Some(PathBuf::from("/tmp/od-logs")),
        };

        let plist = render_launchd_plist(&cfg);
        assert!(plist.contains(SERVICE_LABEL));
        assert!(plist.contains("<string>/usr/local/bin/od</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));

        let systemd = render_systemd_unit(&cfg);
        assert!(systemd.contains("ExecStart=/usr/local/bin/od serve --concurrency 4"));
        assert!(systemd.contains("Restart=on-failure"));
        assert!(systemd.contains("NoNewPrivileges=true"));

        let desktop = render_xdg_desktop(&cfg);
        assert!(desktop.contains("Exec=/usr/local/bin/od serve --concurrency 4"));
        assert!(desktop.contains("X-GNOME-Autostart-enabled=true"));

        let win_xml = render_windows_task_xml(&cfg);
        assert!(win_xml.contains("<LogonTrigger>"));
        assert!(win_xml.contains("<Hidden>true</Hidden>"));
        assert!(win_xml.contains("<Arguments>serve --concurrency 4</Arguments>"));
    }
}
