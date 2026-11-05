//! Blackout: player pages turn black after playback has been paused or ended for
//! a while (`TVCTL_BLACKOUT_SECS`, default 60; 0 turns it off). The page script
//! does the work; the backends install it in every player page.

use std::sync::OnceLock;
use std::time::Duration;

const SCRIPT: &str = include_str!("blackout.js");

/// Default wait before a paused or ended player turns black.
const DEFAULT_DELAY: Duration = Duration::from_secs(60);

/// JavaScript that tells the page script about tvctl activity (a player
/// command), which removes the blackout and restarts the wait.
pub const WAKE_SCRIPT: &str = "if (window.__tvctlBlackout) window.__tvctlBlackout.wake();";

/// JavaScript expression: whether the page is black now.
pub const SHOWN_EXPR: &str = "!!(window.__tvctlBlackout && window.__tvctlBlackout.shown)";

/// Wait before blackout: `$TVCTL_BLACKOUT_SECS` or 60 seconds; zero is off.
fn delay() -> Duration {
    parse_delay(std::env::var("TVCTL_BLACKOUT_SECS").ok().as_deref())
}

fn parse_delay(v: Option<&str>) -> Duration {
    v.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .map(|s| Duration::from_secs_f64(s.max(0.0)))
        .unwrap_or(DEFAULT_DELAY)
}

fn script_for(delay: Duration) -> String {
    SCRIPT.replace("__TVCTL_BLACKOUT_MS__", &delay.as_millis().to_string())
}

/// The page script, with the server's delay.
pub fn page_script() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| script_for(delay()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_from_env_value() {
        assert_eq!(parse_delay(None), DEFAULT_DELAY);
        assert_eq!(parse_delay(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_delay(Some("1.5")), Duration::from_millis(1500));
        assert_eq!(parse_delay(Some("0")), Duration::ZERO);
        assert_eq!(parse_delay(Some("-3")), Duration::ZERO);
        assert_eq!(parse_delay(Some("soon")), DEFAULT_DELAY);
        assert_eq!(parse_delay(Some("inf")), DEFAULT_DELAY);
    }

    #[test]
    fn script_gets_the_delay() {
        let s = script_for(Duration::from_secs(60));
        assert!(s.contains("const DELAY_MS = 60000;"));
        assert!(!s.contains("__TVCTL_BLACKOUT_MS__"));
        assert!(script_for(Duration::ZERO).contains("const DELAY_MS = 0;"));
    }
}
