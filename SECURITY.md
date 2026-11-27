# Security Policy

## Supported Versions

| Version | Supported |
|---|---|
| `0.2.x` | Yes |
| `< 0.2.0` | No (upgrade to `0.2.0+`) |

---

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

If you discover a security issue in Open Downloader (engine, local API/MCP server, native messaging host, browser extension, or installation scripts):

1. Open a **Private Security Advisory** on GitHub (`Security` → `Advisories` → `Report a vulnerability`), or contact the maintainers privately.
2. Include as much detail as possible:
   - Affected component (`od-core`, `od-api`, `od-cli`, `od-app`, `od-nmh`, `extension/`, or `scripts/`)
   - Reproduction steps or a minimal proof-of-concept
   - Potential impact and any suggested remediation

### Response Timeline
- **Acknowledgment:** Within **48 hours**
- **Initial triage & severity assessment:** Within **5 business days**
- **Fix & coordinated disclosure:** Target **14 days** for critical/high issues

---

## Security Architecture & Boundaries

Open Downloader is designed around defense-in-depth for local desktop and daemon execution:

1. **Loopback-Only Control Plane:** `od-api` and `od-app` bind exclusively to `127.0.0.1:19799` by default and require a per-user random 128-bit token (`auth.token`) via `x-od-token` or `Authorization: Bearer` on all mutating/inspection endpoints (`/api/v1/*`, `/mcp`, `/add`).
2. **Filename & Path-Traversal Defense:** Remote `Content-Disposition` (RFC 5987 / RFC 6266) and URL filenames are passed through `od_core::sanitize_filename()`, stripping directory separators (`/`, `\`), blocking `..` traversal, replacing OS-illegal characters, and guarding Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`).
3. **Transport & Payload Integrity:** HTTPS uses `rustls-tls` with certificate verification enabled by default (`config.security.verify_tls = true`), and optional post-download streaming **SHA-256** verification (`expected_sha256` / `od verify`) protects against corrupted or tampered transfers.
4. **Privilege-Free OS Integration:** Auto-startup (Windows Task Scheduler, macOS LaunchAgent, Linux `systemd --user`) and process coexistence (`od-core::conflicts`) run strictly in current-user space with no elevation, kernel drivers, or DLL injection.
5. **Memory Safety:** Core networking, manifest parsing, and hashing crates are written in 100% safe Rust.
