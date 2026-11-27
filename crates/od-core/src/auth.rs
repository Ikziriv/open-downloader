use std::path::PathBuf;

/// Path of the token shared between `od-app` (which serves `/add`) and
/// `od-nmh` (which calls it). The file lives in the user profile, so it only
/// proves the caller is a process running as this user — enough to keep
/// random local tools and other users from queueing downloads.
pub fn auth_token_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("OpenDownloader").join("auth.token")
}

/// Read the shared token, creating a random one on first use.
pub fn auth_token() -> std::io::Result<String> {
    let path = auth_token_path();
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, &token)?;
    Ok(token)
}
