# Contributing to Open Downloader

Thank you for your interest in contributing to **Open Downloader**! We welcome bug reports, feature proposals, protocol fetchers, smart extractors, UI polish, and documentation improvements.

Please review our [Code of Conduct](CODE_OF_CONDUCT.md) before participating.

---

## 1. Development Environment

### Prerequisites
- **Rust `1.85+`** (Edition `2024`, installed via [rustup.rs](https://rustup.rs))
- **Node.js `18+`** *(optional — for browser extension unit tests)*

### Quick Verification
```bash
# Build core workspace (od-core, od-api, od-cli)
cargo build

# Run all core, API, and CLI tests (local loopback servers, no internet required)
cargo test

# Run strict lints (must pass with zero warnings)
cargo clippy --all-targets -- -D warnings

# Run GPUI desktop tests (Windows)
cargo test -p od-app

# Run browser extension tests
node extension/test/extension.test.mjs
```

---

## 2. Project Architecture

| Crate / Directory | Responsibility |
|---|---|
| `crates/od-core` | Download engine, HLS/DASH, `FetcherRegistry`, `TaskManager`, `TrafficBandit` & `SmartQueue`, `AppConfig`, `Sha256`, `speedtest`, `detective`, `conflicts`, `autostart`. |
| `crates/od-api` | Axum REST API v1, SSE `/api/v1/events`, MCP `/mcp`, and embedded Web Control Center (`ui.html`). |
| `crates/od-cli` | `od` CLI (`resolve`, `serve`, `speedtest`, `batch`, `verify`, `config`, `doctor`, `status`, `conflict`, `autostart`). |
| `crates/od-app` | Native GPUI desktop application (excluded from `default-members` to keep CI/CLI builds fast). |
| `crates/od-nmh` | Native Messaging Host bridging Chrome/Edge/Firefox to `od-api` / `od-app`. |
| `extension/` | Manifest V3 browser extension (Chrome/Edge + Firefox). |
| `deploy/` & `scripts/` | Cross-platform startup templates & Windows/Linux/macOS install/uninstall scripts. |
| `docs/` | [BRD](docs/BRD.md), [PRD](docs/PRD.md), [TSD](docs/TSD.md), [SDD](docs/SDD.md), and [LOCAL_PREVIEW.md](docs/LOCAL_PREVIEW.md). |

---

## 3. Coding Standards (Rust 2024)

1. **No panics on data paths:** Never use `.unwrap()` or `.expect()` on network responses, file I/O, or untrusted user input. Return typed `od_core::Error` variants.
2. **Async hygiene:** Never call blocking `std::fs` on Tokio hot paths (`spawn_blocking` or `tokio::fs` only), and never hold a `Mutex`/`RwLock` across an `.await` point.
3. **Security first:** Sanitize all remote filenames (`sanitize_filename`), keep local IPC loopback-only and token-gated, and avoid `unsafe` unless strictly justified with a `// SAFETY:` comment.
4. **Offline testability:** Every new feature or bug fix should include deterministic unit or local-Axum integration tests that pass with zero external internet access.
5. **Formatting & lints:** Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before opening a PR.

---

## 4. Commit & Pull Request Workflow

1. Fork the repository and create a topic branch:
   ```bash
   git checkout -b feat/my-improvement
   ```
2. Follow [Conventional Commits](https://www.conventionalcommits.org/):
   - `feat(core): add SFTP protocol fetcher`
   - `fix(probe): handle RFC 5987 uppercase charset`
   - `docs(prd): update v0.3 roadmap`
   - `test(api): cover batch import error reporting`
3. Open a Pull Request describing:
   - **What changed** and **why**
   - **How it was tested** (`cargo test`, `cargo clippy`, screenshots for UI changes)
   - Any breaking API or config schema changes

---

## 5. Contribution Licensing

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be licensed under the repository's licensing terms (see `LICENSE-MIT`, `LICENSE-APACHE`, and `LICENSE`) without any additional terms or conditions.
