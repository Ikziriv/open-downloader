# Business Requirements Document (BRD) — Open Downloader

| Field | Value |
|---|---|
| **Project Name** | Open Downloader (`od`) |
| **Document Version** | `1.0` (aligned with release `v0.2.0`) |
| **License** | GPL-2.0-only |
| **Status** | Approved for Initial Public Release (`v0.2.0` Beta / MVP) |

---

## 1. Executive Summary

**Open Downloader** is a modern, open-source, high-speed download manager written entirely in **Rust** (Edition 2024). Inspired by the speed of **IDM**, the protocol/extension vision of **Gopeed**, and the scriptability of **aria2**, it delivers a native GPUI desktop interface, an embedded zero-dependency Web Control Center, a full-featured CLI, a browser capture extension, and an AI-agent-ready **Model Context Protocol (MCP)** server — all powered by a single memory-safe core engine.

## 2. Business Problem & Opportunity

1. **Fragmented Tooling:** Users currently juggle separate tools — IDM (Windows-only, closed-source, paid), aria2 (CLI-only, poor UX), yt-dlp/ffmpeg scripts (manual stream assembly), and browser downloaders (single-stream, fragile resume).
2. **Modern Resource Complexity:** Downloads today are no longer just static HTTP files — they include segmented HLS/DASH streams, AES-128 encrypted playlists, Hugging Face model blobs, Google Drive share links, and AI-automated workflows.
3. **Heavyweight UIs:** Electron-based download managers consume 200–400 MB of RAM idle; a Rust + GPUI desktop app and headless daemon consume a fraction of that while remaining cross-platform.

## 3. Target User Personas

| Persona | Primary Needs | Preferred Interface |
|---|---|---|
| **Power Desktop User** | Multi-connection speed, automatic IDM-style folder sorting, browser auto-capture, pre-download speed test, clean dark/light UI. | `od-app` (GPUI) + Browser Extension |
| **Developer / DevOps Engineer** | Scriptable CLI (`od`, `od batch`, `od verify`, `od resolve`), headless daemon on Linux/macOS/Docker, SHA-256 verification, zero external runtime dependencies. | `od-cli` + `od serve` + Web Dashboard |
| **AI Agent / Automation Builder** | Programmatic URL resolution, task lifecycle control, and network diagnostics over a standardized protocol. | `od-api` REST v1 + `POST /mcp` |

## 4. Business Objectives & Success Metrics (KPIs)

| Objective | Measurable KPI | Target (`v0.2.0`) |
|---|---|---|
| **Reliability** | Zero data corruption on pause/resume/crash recovery | 100% pass on ranged, journal, stream & SHA-256 test suites |
| **Performance** | Multi-connection saturation of user bandwidth | 4–32 parallel ranged connections + work-stealing chunks |
| **Cross-Platform Reach** | Native install & auto-startup across desktop OSes | Windows (Task Scheduler), macOS (LaunchAgent), Linux (`systemd --user`/XDG) |
| **Zero-Friction Integration** | Browser & AI tool ecosystem adoption | Chrome/Edge/Firefox MV3 extension + MCP JSON-RPC 2.0 server |
| **Engineering Quality** | Memory safety & zero-warning builds | `cargo clippy --all-targets -- -D warnings` clean; 50+ automated tests |

## 5. Scope Boundaries

### In Scope (`v0.2.0`)
- Multi-connection HTTP/HTTPS ranged downloads with crash-safe `.odj` resume journals and automatic transient-error retry.
- HLS (`.m3u8`, AES-128 + key rotation) and DASH (`.mpd`, `SegmentList` / `SegmentTemplate` / `SegmentTimeline` / `BaseURL`) VOD downloads.
- Two-phase `Resolve → Download` architecture (`FetcherRegistry`) with built-in smart extractors (Hugging Face, Google Drive, GitHub).
- Persistent `TaskManager`, unified `AppConfig` (`config.json`), streaming SHA-256 verification, and Speedtest.net-style connection diagnostics.
- `od-api` (REST v1 + SSE + MCP + Embedded Web Dashboard), `od-cli`, `od-app` (GPUI on Windows), `od-nmh`, MV3 Browser Extension, and OS-native install/autostart scripts.

### Out of Scope (Planned for `v0.3.0+`)
- BitTorrent / Magnet protocol (`od-proto-bt` via `librqbit`).
- Sandboxed QuickJS/WASM third-party extension marketplace (`od-ext`).
- Multi-track video+audio FFmpeg muxing and DRM-protected streams (Widevine/FairPlay/SAMPLE-AES).
- Signed OS installers (`.msi` / `.dmg` notarization / Homebrew tap / Winget manifest).

## 6. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Path traversal or illegal filenames from untrusted servers | High (Security) | Strict `sanitize_filename()` + RFC 5987 parser strips `/`, `\`, `..`, and Windows reserved device names (`CON`, `NUL`, etc.). |
| Unauthorized local processes queuing downloads | Medium (Security) | Loopback-only binding (`127.0.0.1:19799`) + per-user random `auth.token` enforced via `x-od-token` / `Bearer`. |
| Slow first compile of GPUI (`od-app`) | Medium (DX) | `od-app` is excluded from workspace `default-members`; CI and headless installs build `od-core`/`od-api`/`od-cli`/`od-nmh` in seconds. |
