# Technical Specification Document (TSD) — Open Downloader

| Field | Value |
|---|---|
| **Version** | `v0.2.0` |
| **Language / Toolchain** | Rust `1.85+` (Edition `2024`, Resolver `3`) |
| **Async Runtime** | `tokio` (multi-threaded) + `reqwest` (`rustls-tls`) + `axum 0.8` |

---

## 1. Workspace Crate Topology

| Crate | Type | Responsibility | Default Member |
|---|---|---|---|
| `od-core` | Library | Download engine, HLS/DASH, `Fetcher` & `ResourceInterceptor`, `TaskManager`, `AppConfig`, `Sha256`, `speedtest`, `autostart`, `auth`, `category`. | Yes |
| `od-api` | Library | Axum HTTP control plane: REST API v1, SSE `/api/v1/events`, MCP `/mcp`, legacy `/add`, and embedded Web Dashboard (`GET /`). | Yes |
| `od-cli` | Binary (`od`) | CLI entrypoint: direct downloads, `resolve`, `serve`, `speedtest`, `batch`, `verify`, `config`, `doctor`, `status`, `autostart`. | Yes |
| `od-nmh` | Binary (`od-nmh`) | Chrome/Edge/Firefox Native Messaging stdio bridge forwarding browser captures to `127.0.0.1:19799/add` (or `od` fallback). | Optional |
| `od-app` | Binary (`od-app`) | Native GPUI desktop application (Windows) with Obsidian Cobalt / Porcelain Slate UI, live sparkline, Speed Test, and Settings. | Optional |

---

## 2. HTTP / REST / SSE / MCP Wire Contracts (`od-api`)

Default bind address: `127.0.0.1:19799` (`od_core::APP_PORT`).

### Authentication
All endpoints except `GET /` and `GET /api/v1/health` require either:
- `x-od-token: <token>`
- `Authorization: Bearer <token>`

The 128-bit hex token is auto-generated via `getrandom` on first run at `%LOCALAPPDATA%\OpenDownloader\auth.token` (Windows) or `$XDG_DATA_HOME/open-downloader/auth.token` / temp fallback.

### REST Endpoints

| Method | Path | Request Body / Query | Response |
|---|---|---|---|
| `GET` | `/` | — | `200 text/html` — Embedded Web Control Center |
| `GET` | `/api/v1/health` | — | `200 JSON` `{ ok, version, protocols }` |
| `POST` | `/api/v1/resolve` | `{ url, headers?, extra? }` | `200 JSON` `{ request, resource }` |
| `GET` | `/api/v1/tasks` | — | `200 JSON` `TaskRecord[]` |
| `POST` | `/api/v1/tasks` | `{ url, dest?, filename?, headers?, connections?, rate_limit?, organize?, expected_sha256?, auto_start?, resource? }` | `201 JSON` `TaskRecord` |
| `POST` | `/api/v1/tasks/batch` | `{ urls?, text?, dest?, auto_start? }` | `200 JSON` `{ created: TaskRecord[], errors: [url, msg][] }` |
| `GET` | `/api/v1/tasks/{id}` | — | `200 JSON` `TaskRecord` / `404` |
| `POST` | `/api/v1/tasks/{id}/pause` | — | `204 No Content` |
| `POST` | `/api/v1/tasks/{id}/resume` | — | `204 No Content` |
| `DELETE` | `/api/v1/tasks/{id}?delete_files=bool` | — | `204 No Content` |
| `GET` | `/api/v1/events` | — | `200 text/event-stream` (`TaskEvent` JSON frames, 15s keep-alive) |
| `GET` | `/api/v1/config` | — | `200 JSON` `{ config: AppConfig, autostart: AutoStartStatus? }` |
| `PUT` | `/api/v1/config` | `AppConfig` JSON | `200 JSON` `{ config: AppConfig, autostart: AutoStartStatus? }` |
| `POST` | `/api/v1/speedtest` | Optional `SpeedTestConfig` JSON | `200 JSON` `SpeedTestReport` |
| `POST` | `/add` | `{ url, filename?, cookie?, referer? }` | `200 JSON` `TaskRecord` (legacy `od-nmh` compatibility) |

### MCP Endpoint (`POST /mcp`)
Implements JSON-RPC 2.0 (`protocolVersion: "2024-11-05"`):
- `initialize` → server info & tool capability declaration
- `tools/list` → 7 tool schemas
- `tools/call` → `resolve_url`, `create_download`, `list_downloads`, `pause_download`, `resume_download`, `remove_download`, `run_speedtest`

---

## 3. On-Disk Persistence Schemas

All files use **atomic write-tmp-then-rename** to prevent corruption on power loss or process kill:

| File | Location | Purpose |
|---|---|---|
| `config.json` | App data dir | Unified `AppConfig` (`downloads`, `startup`, `security`, `appearance`). |
| `tasks.json` | App data dir | `FileTaskStore` array of `TaskRecord`s; active tasks recover as `Paused` on restart. |
| `auth.token` | App data dir | 32-char hex shared secret for local IPC authentication. |
| `<file>.part` + `<file>.odj` | Destination dir | Preallocated sparse partial payload + JSON chunk completion journal (`url`, `size`, `etag`, `chunk_size`, `completed: BTreeSet<u64>`). |
| `<file>.segments/` | Destination dir | Ordered `NNNNNN` segment files for resumable HLS/DASH downloads. |

---

## 4. Cross-Platform Auto-Startup & Packaging Specification

| Platform | Service Artifact | Install Path | Manager Command |
|---|---|---|---|
| **Windows** | `deploy/windows/open-downloader-task.xml` | Task Scheduler `OpenDownloaderDaemon` (fallback: `HKCU\...\Run\OpenDownloader`) | `schtasks /Create /TN OpenDownloaderDaemon /XML ... /F` |
| **macOS** | `deploy/macos/com.opendownloader.daemon.plist` | `~/Library/LaunchAgents/com.opendownloader.daemon.plist` | `launchctl bootstrap gui/$(id -u) <plist>` |
| **Linux** | `deploy/linux/open-downloader.service` (+ `.desktop` fallback) | `~/.config/systemd/user/open-downloader.service` | `systemctl --user enable --now open-downloader.service` |

### CI/CD Release Matrix (`.github/workflows/release.yml`)
- **Verify job:** Runs `cargo clippy --all-targets -- -D warnings` and `cargo test` on `ubuntu-latest`, `macos-latest`, and `windows-latest`.
- **Release job (on `v*` tags):** Builds release binaries (`opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`) and emits `.zip`/`.tar.gz` archives + `.sha256` digests for `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-apple-darwin`.
