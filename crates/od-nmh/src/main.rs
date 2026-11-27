//! Native messaging host for the Open Downloader browser extension.
//!
//! Chrome/Edge talk to this process over stdio: each message is a
//! little-endian u32 length followed by that many bytes of JSON, and the
//! reply uses the same framing. A download request is forwarded to the
//! running app (`POST http://127.0.0.1:APP_PORT/add`) authenticated with the
//! shared token; when the app isn't up we fall back to spawning the `od`
//! CLI next to this binary.

use std::io::{Read, Write};
use std::process::Command;
use std::time::Duration;

const MAX_MSG: usize = 1024 * 1024;

fn read_msg() -> Option<serde_json::Value> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut len = [0u8; 4];
    stdin.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_MSG {
        return None;
    }
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_msg(v: &serde_json::Value) -> std::io::Result<()> {
    let buf = serde_json::to_vec(v)?;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&(buf.len() as u32).to_le_bytes())?;
    stdout.write_all(&buf)?;
    stdout.flush()
}

fn forward_to_app(msg: &serde_json::Value) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client
        .post(format!("http://127.0.0.1:{}/add", od_core::APP_PORT))
        .json(&serde_json::json!({
            "url": msg["url"],
            "filename": msg["filename"],
            "cookie": msg["cookie"],
            "referer": msg["referer"],
        }));
    if let Ok(token) = std::fs::read_to_string(od_core::auth_token_path()) {
        req = req.header("x-od-token", token.trim());
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("app returned {}", resp.status()))
    }
}

fn spawn_cli(url: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let cli = exe.with_file_name(if cfg!(windows) { "od.exe" } else { "od" });
    Command::new(cli).arg(url).spawn().map(|_| ())
}

fn main() {
    while let Some(msg) = read_msg() {
        let resp = match (msg["type"].as_str(), msg["url"].as_str()) {
            (Some("add"), Some(url)) if url.starts_with("http") => {
                match forward_to_app(&msg).or_else(|_| spawn_cli(url)) {
                    Ok(()) => serde_json::json!({ "ok": true }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            (Some("ping"), _) => {
                serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })
            }
            _ => serde_json::json!({ "ok": false, "error": "bad message" }),
        };
        if write_msg(&resp).is_err() {
            break;
        }
    }
}
