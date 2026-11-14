//! Whether this process can use the macOS login keychain. Chrome keeps the key
//! it encrypts cookies with there ("Chrome Safe Storage"); a Chrome that cannot
//! read it cannot use the saved cookies, and tvctl's logins are lost. Processes
//! outside the user's login session (for example over SSH) get
//! `errSecInteractionNotAllowed`.

use anyhow::Result;

/// Test hook: pretend the keychain cannot be used.
const UNAVAILABLE_ENV: &str = "TVCTL_TEST_KEYCHAIN_UNAVAILABLE";

/// Fails, with the reason, if this process cannot use the login keychain.
pub fn check() -> Result<()> {
    if std::env::var_os(UNAVAILABLE_ENV).is_some() {
        anyhow::bail!(message(&format!("{UNAVAILABLE_ENV} is set")));
    }
    platform::check()
}

fn message(detail: &str) -> String {
    format!(
        "this process cannot use the macOS login keychain ({detail}), so tvctl's Chrome could \
         not use its saved cookies and its logins would be lost. Run tvctl from a terminal on \
         this Mac's own screen"
    )
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;

    use anyhow::{Result, bail};

    /// `SecKeychainSettings` (Security framework).
    #[repr(C)]
    struct Settings {
        version: u32,
        lock_on_sleep: u8,
        use_lock_interval: u8,
        lock_interval: u32,
    }

    const SETTINGS_VERS1: u32 = 1;
    const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainCopySettings(keychain: *const c_void, settings: *mut Settings) -> i32;
    }

    /// Reads the default keychain's settings (as `security show-keychain-info`
    /// does), which fails when the keychain cannot be used by this process.
    pub fn check() -> Result<()> {
        let mut settings = Settings {
            version: SETTINGS_VERS1,
            lock_on_sleep: 0,
            use_lock_interval: 0,
            lock_interval: 0,
        };
        // SAFETY: a null keychain means the default keychain; `settings` is a
        // valid, initialized SecKeychainSettings with its version set.
        let status = unsafe { SecKeychainCopySettings(std::ptr::null(), &mut settings) };
        match status {
            0 => Ok(()),
            ERR_INTERACTION_NOT_ALLOWED => bail!(super::message("user interaction is not allowed")),
            s => bail!(super::message(&format!("Security framework error {s}"))),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn check() -> anyhow::Result<()> {
        Ok(())
    }
}
