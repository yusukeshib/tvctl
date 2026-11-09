//! Persistent data: the data directory.

use std::path::PathBuf;

/// `$TVCTL_DATA_DIR`, else `~/Library/Application Support/tvctl` on macOS
/// and `${XDG_DATA_HOME:-~/.local/share}/tvctl` elsewhere.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TVCTL_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/tvctl")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("tvctl")
    }
}
