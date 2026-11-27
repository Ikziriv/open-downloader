# Product Requirements Document (PRD) — Open Downloader

| Field | Value |
|---|---|
| **Product** | Open Downloader (`v0.2.0`) |
| **Target Platforms** | Windows 10/11 (GUI + CLI + Daemon), Linux & macOS (CLI + Daemon + Web Dashboard) |
| **References** | [BRD.md](BRD.md) · [TSD.md](TSD.md) · [SDD.md](SDD.md) |

---

## 1. Product Vision

Deliver a single, fast, trustworthy download platform where **any user or AI agent** can inspect a link, verify network capacity, download at maximum speed with automatic resume & integrity verification, and organize files cleanly — whether through a native desktop app, browser, terminal, or web dashboard.

---

## 2. Functional Requirements

| ID | Feature | Requirement Description | Surfaces |
|---|---|---|---|
| **FR-01** | **Ranged Multi-Connection Engine** | Split range-capable HTTP/HTTPS files into work-stealing chunks across configurable parallel connections (`1..=32`) with `.part` + `.odj` crash-safe resume. | Core, CLI, API, GUI |
| **FR-02** | **Streaming Manifests (HLS & DASH)** | Parse `.m3u8` (highest-bandwidth variant, AES-128 decryption & key rotation) and `.mpd` (`SegmentList`, `SegmentTemplate`, `SegmentTimeline`, `BaseURL`) with segment-level resume. | Core, CLI, API, GUI |
| **FR-03** | **Two-Phase `Resolve → Download`** | Inspect URLs prior to downloading to return filename, total size, range support, category, and file entries (`od resolve`, `POST /api/v1/resolve`, Web UI Inspect modal). | Core, CLI, API, Web |
| **FR-04** | **Smart Resource Extractors** | Automatically rewrite Hugging Face `/blob/` → `/resolve/` (with `HF_TOKEN`), Google Drive share links → direct download, and GitHub `/blob/` → `raw.githubusercontent.com`. | Core, CLI, API, Web |
| **FR-05** | **Resilient Retry & Backoff** | Automatically retry transient network errors and HTTP `408`/`425`/`429`/`5xx` per chunk/segment with cancellation-aware exponential backoff + jitter. | Core |
| **FR-06** | **SHA-256 Integrity Verification** | Stream-verify downloaded files against an expected SHA-256 digest before finalizing (`expected_sha256`, `od verify`). | Core, CLI, API |
| **FR-07** | **Batch Queue Import** | Accept newline-separated URL files/text (ignoring `#` comments and blanks) via `od batch <file>` and `POST /api/v1/tasks/batch`. | Core, CLI, API |
| **FR-08** | **IDM-Style Library & Config Center** | Persist `config.json` controlling default output folder, automatic category subfolders (`Video/`, `Audio/`, `Documents/`, `Images/`, `Compressed/`, `Programs/`), per-category folder overrides, concurrency, rate limit, security, and theme. | Core, CLI, API, GUI, Web |
| **FR-09** | **Internet Speed Test & Diagnostics** | Measure Ping (ms), Jitter (ms), multi-stream Download Mbps, optional Upload Mbps, quality grade, and recommended connections with live traffic curve. | Core, CLI, API, GUI, Web |
| **FR-10** | **Cross-Platform Auto-Startup** | Privilege-free user-level auto-start via Windows Task Scheduler, macOS LaunchAgent, and Linux `systemd --user` / XDG `.desktop` (`od autostart`, Settings, install scripts). | Core, CLI, GUI, Scripts |
| **FR-11** | **Browser Capture (MV3 Extension)** | Intercept Chrome/Edge/Firefox downloads and sniff page media (`.mp4`/`.m3u8`/`.mpd`), forwarding URL + cookies + referer via `od-nmh` to `POST /add`. | Extension, NMH, API, GUI |
| **FR-12** | **AI Agent Control (MCP)** | Expose `POST /mcp` JSON-RPC 2.0 server with 7 tools (`resolve_url`, `create_download`, `list_downloads`, `pause_download`, `resume_download`, `remove_download`, `run_speedtest`). | API |

---

## 3. Non-Functional Requirements

| ID | Category | Requirement |
|---|---|---|
| **NFR-01** | **Memory Safety** | 100% safe Rust in `od-core`, `od-api`, `od-cli`, `od-nmh`; zero `.unwrap()` on untrusted network/file paths. |
| **NFR-02** | **Async Responsiveness** | No blocking `std::fs` calls on Tokio hot paths; no `Mutex` held across `.await`; progress events throttled to ~50 ms. |
| **NFR-03** | **Accessibility & Visual Contrast** | All GPUI Dark (`Obsidian Cobalt`) and Light (`Porcelain Slate`) surfaces must pass automated WCAG contrast tests (`>= 4.5:1` body text, `>= 4.0:1` muted text). |
| **NFR-04** | **Security by Default** | Loopback-only binding (`127.0.0.1`), token auth enabled by default, RFC 5987 + path-traversal filename sanitization. |
| **NFR-05** | **Offline Testability** | Full test suite (`cargo test`) runs deterministically against local loopback Axum servers with zero external internet dependency. |
| **NFR-06** | **Portability** | Single-command install & clean uninstall on Windows (`install.ps1`), Linux, and macOS (`install.sh`). |

---

## 4. Product Roadmap

- **`v0.2.0` (Current — First Public Release Candidate):** Hardened core engine, `FetcherRegistry`, `TaskManager`, `od-api` (REST + SSE + MCP + Embedded Web UI), `od-cli` (`resolve`, `serve`, `speedtest`, `batch`, `verify`, `config`, `doctor`, `status`, `autostart`), upgraded `od-app` GPUI, and cross-platform install/autostart scripts.
- **`v0.3.0`:** BitTorrent & Magnet protocol (`od-proto-bt`), FTP/SFTP (`od-proto-ftp`), multi-track DASH/HLS FFmpeg muxing.
- **`v0.4.0`:** Sandboxed QuickJS (`rquickjs`) extension runtime (`od-ext`) with community extension installer.
- **`v1.0.0`:** Signed installers (Winget, Homebrew, AppImage/DEB), cross-platform GPUI builds (macOS/Linux), and full SQLite task history analytics.
