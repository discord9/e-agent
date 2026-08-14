pub mod agent;
pub mod codex;
pub mod codex_auth;
pub mod config;
pub mod delegate;
pub mod markdown;
pub mod mcp;
pub mod model;
pub mod output_receipt;
pub mod roles;
pub mod runner;
pub mod server;
pub mod session;
pub mod session_factory;
#[cfg(feature = "greptime")]
pub mod session_greptime;
#[cfg(feature = "sqlite")]
pub mod session_sqlite;
pub mod session_store;
pub mod tools;
pub mod tui;
pub mod workspace;

use std::path::{Path, PathBuf};

/// Resolve the user's home directory. Priority: `$HOME` on every platform
/// (Git Bash sets it on Windows too), then — only on Windows, where `HOME`
/// is typically absent under cmd/PowerShell — `$USERPROFILE`, then
/// `$APPDATA`. On non-Windows platforms this is exactly `$HOME`, preserving
/// the previous inline behavior byte for byte.
pub fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let home = home
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .or_else(|| std::env::var_os("APPDATA").filter(|h| !h.is_empty()));
    home.map(PathBuf::from)
}

/// Canonicalize a path, then strip the Windows `\\?\` verbatim prefix so
/// the result compares equal to ordinary (non-prefixed) paths. On
/// non-Windows platforms this is exactly `std::fs::canonicalize`.
pub fn canonicalize_path(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    Ok(strip_verbatim_prefix(&path))
}

/// Strip the Windows `\\?\` verbatim prefix that `std::fs::canonicalize`
/// prepends to every result (needed for >260-char paths). Comparisons and
/// `strip_prefix` between canonicalized and ordinary paths fail while one
/// side carries the prefix; stripping both sides first restores equality.
/// No-op on non-Windows and on paths without the prefix.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(not(windows))]
    {
        let _ = path;
        path.to_path_buf()
    }
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            // Verbatim UNC form: `\\?\UNC\server\share` → `\\server\share`.
            PathBuf::from(format!(r"\\{rest}"))
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            path.to_path_buf()
        }
    }
}
