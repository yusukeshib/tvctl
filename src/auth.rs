//! Persistent data: the data directory and remembered login state per service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// `$MEDIACTL_DATA_DIR`, else `~/Library/Application Support/mediactl` on macOS
/// and `${XDG_DATA_HOME:-~/.local/share}/mediactl` elsewhere.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MEDIACTL_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/mediactl")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("mediactl")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub logins: BTreeMap<String, Login>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Login {
    pub logged_in: bool,
    /// Unix time of the last check.
    pub checked_at: u64,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

fn load_from(dir: &Path) -> State {
    std::fs::read_to_string(state_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn record_in(dir: &Path, service: &str, logged_in: bool) {
    let mut state = load_from(dir);
    let checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    state.logins.insert(
        service.to_string(),
        Login {
            logged_in,
            checked_at,
        },
    );
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(state_path(dir), json);
    }
}

/// Last recorded login state of a service (false if never checked).
pub fn is_logged_in(service: &str) -> bool {
    load_from(&data_dir())
        .logins
        .get(service)
        .is_some_and(|l| l.logged_in)
}

pub fn record(service: &str, logged_in: bool) {
    record_in(&data_dir(), service, logged_in);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_loads_login_state() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("nested");
        assert!(load_from(&data).logins.is_empty());
        record_in(&data, "youtube", true);
        record_in(&data, "netflix", false);
        let s = load_from(&data);
        assert!(s.logins["youtube"].logged_in);
        assert!(!s.logins["netflix"].logged_in);
        assert!(s.logins["youtube"].checked_at > 0);
        record_in(&data, "youtube", false);
        assert!(!load_from(&data).logins["youtube"].logged_in);
    }

    #[test]
    fn corrupt_state_is_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "{not json").unwrap();
        assert!(load_from(dir.path()).logins.is_empty());
        record_in(dir.path(), "youtube", true);
        assert!(load_from(dir.path()).logins["youtube"].logged_in);
    }
}
