//! Service plugins.
//!
//! A plugin is a directory `plugins/<name>/` containing a `plugin.toml` manifest
//! and the scripts for the backends it supports:
//!
//! * `webview.html` — page loaded into the native WebView backend.
//! * `frame.js` — optional; injected into the subframes of that WebView.
//! * `chrome.js` — adapter evaluated in Chrome pages (Chrome backend).
//!
//! Every adapter defines `window.__tvctl.run(action)` returning a status object.
//! Built-in plugins are embedded in the binary; external plugins are read from
//! `$TVCTL_PLUGIN_DIR` or `${XDG_CONFIG_HOME:-~/.config/tvctl/plugins` and override
//! built-ins with the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};
use url::Url;

static BUILTIN: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/plugins");

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Webview,
    Chrome,
}

impl BackendKind {
    /// Files a plugin must provide for this backend.
    fn required_file(self) -> &'static str {
        match self {
            BackendKind::Webview => "webview.html",
            BackendKind::Chrome => "chrome.js",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Hosts handled by this plugin; subdomains match too.
    pub domains: Vec<String>,
    /// Supported backends in order of preference.
    pub backends: Vec<BackendKind>,
    /// Player actions this plugin supports (`status` is always supported).
    pub actions: Vec<String>,
    /// Actions from `actions` that a specific backend lacks.
    #[serde(default)]
    pub unsupported_actions: BTreeMap<BackendKind, Vec<String>>,
    /// Example URLs (shown in `tvctl --help` and `tvctl plugins`); each must be
    /// handled by this plugin.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Notes for agents and users: limits, accepted values, login hints.
    #[serde(default)]
    pub notes: Vec<String>,
    pub auth: Option<Auth>,
}

/// Every player action a plugin can declare.
pub const ACTIONS: &[&str] = &[
    "play",
    "pause",
    "toggle",
    "next",
    "previous",
    "seek",
    "subtitles",
    "audio",
    "volume",
    "speed",
    "quality",
];

impl Manifest {
    /// Whether `action` is available for this plugin on `backend`.
    pub fn supports(&self, action: &str, backend: BackendKind) -> bool {
        action == "status"
            || (self.actions.iter().any(|a| a == action)
                && !self
                    .unsupported_actions
                    .get(&backend)
                    .is_some_and(|u| u.iter().any(|a| a == action)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Playback needs a login (otherwise a login is optional, e.g. for no ads).
    #[serde(default)]
    pub required: bool,
    pub login_url: String,
    pub cookie_url: String,
    pub logged_in_cookies: Vec<String>,
    pub prefer_backend_when_logged_in: Option<BackendKind>,
    #[serde(default)]
    pub login_mode: LoginMode,
    /// Name of the site described by the fields above, when `sites` lists more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Further sites with their own logins (e.g. regional stores), by name.
    /// `auth login --site <name>` signs in to one; the service counts as logged
    /// in when any site is.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sites: BTreeMap<String, AuthSite>,
}

/// The login of one site of a service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSite {
    pub login_url: String,
    pub cookie_url: String,
    pub logged_in_cookies: Vec<String>,
}

impl Auth {
    /// Name of the main site (the top-level fields).
    pub fn main_site(&self) -> &str {
        self.site.as_deref().unwrap_or("default")
    }

    /// Every site, the main one first.
    pub fn all_sites(&self) -> Vec<(String, AuthSite)> {
        let main = AuthSite {
            login_url: self.login_url.clone(),
            cookie_url: self.cookie_url.clone(),
            logged_in_cookies: self.logged_in_cookies.clone(),
        };
        std::iter::once((self.main_site().to_string(), main))
            .chain(self.sites.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect()
    }

    /// The named site, or the main one when `name` is None.
    pub fn site(&self, name: Option<&str>) -> Result<AuthSite> {
        let all = self.all_sites();
        let want = name.unwrap_or(self.main_site());
        match all.iter().find(|(k, _)| k == want) {
            Some((_, s)) => Ok(s.clone()),
            None => bail!(
                "unknown site {want:?} (sites: {})",
                all.iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// How `auth login` opens the login page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMode {
    /// A plain Chrome (no DevTools connection) on the profile; finishes when the
    /// user quits Chrome. Needed for sites such as Google that refuse sign-in in
    /// automated browsers.
    #[default]
    Plain,
    /// A window of the controlled Chrome; finishes when that window closes.
    Controlled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    #[serde(flatten)]
    pub manifest: Manifest,
    /// `builtin` or the directory it was loaded from.
    pub source: String,
    #[serde(skip)]
    files: BTreeMap<String, String>,
}

impl Plugin {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// What `tvctl plugins` shows: what an agent needs to use the service,
    /// without internals such as login cookies.
    pub fn summary(&self) -> serde_json::Value {
        let m = &self.manifest;
        let login = m.auth.as_ref().map(|a| {
            let sites: Vec<String> = a.all_sites().into_iter().map(|(k, _)| k).collect();
            let commands: Vec<String> = if sites.len() > 1 {
                std::iter::once(format!("tvctl auth login {}", m.name))
                    .chain(
                        a.sites
                            .keys()
                            .map(|s| format!("tvctl auth login {} --site {s}", m.name)),
                    )
                    .collect()
            } else {
                vec![format!("tvctl auth login {}", m.name)]
            };
            let mut v = serde_json::json!({
                "required": a.required,
                "commands": commands,
            });
            if sites.len() > 1 {
                v["sites"] = serde_json::json!(sites);
            }
            if let Some(b) = a.prefer_backend_when_logged_in {
                v["backend_when_logged_in"] = serde_json::json!(b);
            }
            v
        });
        serde_json::json!({
            "name": m.name,
            "description": m.description,
            "domains": m.domains,
            "examples": m.examples,
            "backends": m.backends,
            "actions": m.actions,
            "unsupported_actions": m.unsupported_actions,
            "login": login,
            "notes": m.notes,
            "source": self.source,
        })
    }

    pub fn file(&self, name: &str) -> Option<&str> {
        self.files.get(name).map(String::as_str)
    }

    fn handles_host(&self, host: &str) -> bool {
        self.manifest
            .domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    pub(crate) fn from_files(files: BTreeMap<String, String>, source: String) -> Result<Plugin> {
        let manifest_src = files.get("plugin.toml").context("missing plugin.toml")?;
        let manifest: Manifest = toml::from_str(manifest_src).context("parsing plugin.toml")?;
        if manifest.domains.is_empty() {
            bail!("plugin {}: `domains` is empty", manifest.name);
        }
        if manifest.backends.is_empty() {
            bail!("plugin {}: `backends` is empty", manifest.name);
        }
        for a in manifest
            .actions
            .iter()
            .chain(manifest.unsupported_actions.values().flatten())
        {
            if !ACTIONS.contains(&a.as_str()) {
                bail!(
                    "plugin {}: unknown action {a:?} (known: {})",
                    manifest.name,
                    ACTIONS.join(", ")
                );
            }
        }
        for e in &manifest.examples {
            let host = parse_url(e)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            let handled = manifest
                .domains
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")));
            if !handled {
                bail!(
                    "plugin {}: example {e:?} is not on one of its domains",
                    manifest.name
                );
            }
        }
        if let Some(a) = &manifest.auth
            && a.sites.contains_key(a.main_site())
        {
            bail!(
                "plugin {}: site {:?} is both the main site and in `sites`",
                manifest.name,
                a.main_site()
            );
        }
        for b in &manifest.backends {
            if !files.contains_key(b.required_file()) {
                bail!(
                    "plugin {}: backend {b:?} requires {}",
                    manifest.name,
                    b.required_file()
                );
            }
        }
        Ok(Plugin {
            manifest,
            source,
            files,
        })
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    plugins: Vec<Plugin>,
}

impl Registry {
    /// Loads built-in plugins, then external ones (which override by name).
    pub fn load() -> Result<Registry> {
        Registry::load_with(external_dir())
    }

    fn load_with(external: Option<PathBuf>) -> Result<Registry> {
        let mut registry = Registry::default();
        for dir in BUILTIN.dirs() {
            let files = dir
                .files()
                .filter_map(|f| {
                    let name = f.path().file_name()?.to_str()?.to_string();
                    Some((name, f.contents_utf8()?.to_string()))
                })
                .collect();
            let plugin = Plugin::from_files(files, "builtin".into())
                .with_context(|| format!("built-in plugin {}", dir.path().display()))?;
            registry.insert(plugin);
        }
        if let Some(root) = external.filter(|d| d.is_dir()) {
            for entry in std::fs::read_dir(&root)? {
                let path = entry?.path();
                if path.join("plugin.toml").is_file() {
                    let plugin =
                        load_dir(&path).with_context(|| format!("plugin {}", path.display()))?;
                    registry.insert(plugin);
                }
            }
        }
        Ok(registry)
    }

    fn insert(&mut self, plugin: Plugin) {
        self.plugins.retain(|p| p.name() != plugin.name());
        self.plugins.push(plugin);
        self.plugins.sort_by(|a, b| a.name().cmp(b.name()));
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.name() == name)
    }

    /// Finds the plugin for a URL. Returns the normalized URL too.
    pub fn for_url(&self, input: &str) -> Result<(&Plugin, Url)> {
        let url = parse_url(input)?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let plugin = self.plugins.iter().find(|p| p.handles_host(&host));
        match plugin {
            Some(p) => Ok((p, url)),
            None => {
                let names: Vec<&str> = self.plugins.iter().map(|p| p.name()).collect();
                bail!(
                    "no plugin handles {host} (installed plugins: {})",
                    names.join(", ")
                )
            }
        }
    }

    /// `tvctl plugins NAME`: one plugin's summary.
    pub fn describe(&self, name: &str) -> Result<serde_json::Value> {
        match self.get(name) {
            Some(p) => Ok(p.summary()),
            None => {
                let names: Vec<&str> = self.plugins.iter().map(|p| p.name()).collect();
                bail!("no plugin named {name:?} (plugins: {})", names.join(", "))
            }
        }
    }

    /// The "Services" section of `tvctl --help`.
    pub fn help_section(&self) -> String {
        let mut s = String::from(
            "Services (installed plugins). `tvctl plugins <name>` shows a service's URLs,\n\
             supported commands, login and notes; read it before using a service:\n",
        );
        for p in &self.plugins {
            let m = &p.manifest;
            let login = match &m.auth {
                Some(a) if a.required => "login required",
                Some(_) => "login optional",
                None => "no login",
            };
            s.push_str(&format!("  {:<12} {} ({login})\n", m.name, m.description));
            if let Some(e) = m.examples.first() {
                s.push_str(&format!("  {:<12} e.g. tvctl open '{e}'\n", ""));
            }
        }
        s.trim_end().to_string()
    }
}

fn parse_url(input: &str) -> Result<Url> {
    let input = input.trim();
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url = Url::parse(&with_scheme).with_context(|| format!("invalid URL {input:?}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("not a web URL: {input}");
    }
    Ok(url)
}

pub fn external_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TVCTL_PLUGIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("tvctl").join("plugins"))
}

fn load_dir(dir: &Path) -> Result<Plugin> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file()
            && let (Some(name), Ok(content)) = (
                path.file_name().and_then(|n| n.to_str()),
                std::fs::read_to_string(&path),
            )
        {
            files.insert(name.to_string(), content);
        }
    }
    Plugin::from_files(files, dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_load_and_route() {
        let r = Registry::load().unwrap();
        let yt = r.get("youtube").expect("youtube plugin");
        assert!(yt.file("chrome.js").is_some());
        for u in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(r.for_url(u).unwrap().0.name(), "youtube", "{u}");
        }
        for u in [
            "https://www.amazon.com/gp/video/detail/B0GHXVBQLM",
            "amazon.com/dp/B0GHXVBQLM",
            "https://www.amazon.co.jp/-/en/gp/video/detail/B09R26WLZ5",
            "https://www.primevideo.com/detail/0HO80FV4Z40LP5NE2D9K7AQ0KD",
        ] {
            assert_eq!(r.for_url(u).unwrap().0.name(), "primevideo", "{u}");
        }
        for u in [
            "https://www.netflix.com/watch/81716297?trackId=264090261",
            "netflix.com/title/70264888",
        ] {
            assert_eq!(r.for_url(u).unwrap().0.name(), "netflix", "{u}");
        }
        for u in [
            "https://www.crunchyroll.com/watch/G0DUND0K2/the-journeys-end",
            "crunchyroll.com/ja/series/GG5H5XQX4",
        ] {
            assert_eq!(r.for_url(u).unwrap().0.name(), "crunchyroll", "{u}");
        }
        assert!(r.for_url("https://example.com/").is_err());
        assert!(r.for_url("https://notyoutube.com/").is_err());
        assert!(r.for_url("file:///etc/passwd").is_err());
    }

    fn write_plugin(root: &Path, name: &str, manifest: &str, files: &[&str]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        for f in files {
            std::fs::write(dir.join(f), "// test").unwrap();
        }
    }

    #[test]
    fn every_builtin_is_valid() {
        let r = Registry::load_with(None).unwrap();
        assert!(!r.plugins().is_empty());
        for p in r.plugins() {
            assert_eq!(p.source, "builtin");
            for b in &p.manifest.backends {
                assert!(
                    p.file(b.required_file()).is_some(),
                    "{} lacks {}",
                    p.name(),
                    b.required_file()
                );
            }
            // Agents learn services from `tvctl --help` / `tvctl plugins`.
            assert!(
                !p.manifest.examples.is_empty(),
                "{} has no examples",
                p.name()
            );
            assert!(!p.manifest.notes.is_empty(), "{} has no notes", p.name());
            for e in &p.manifest.examples {
                assert_eq!(r.for_url(e).unwrap().0.name(), p.name(), "{e}");
            }
            if let Some(a) = &p.manifest.auth {
                for (site, s) in a.all_sites() {
                    Url::parse(&s.login_url).unwrap();
                    Url::parse(&s.cookie_url).unwrap();
                    assert!(!s.logged_in_cookies.is_empty(), "{} {site}", p.name());
                }
                if let Some(b) = a.prefer_backend_when_logged_in {
                    assert!(p.manifest.backends.contains(&b), "{}", p.name());
                }
            }
        }
    }

    #[test]
    fn external_plugins_add_and_override() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "youtube",
            "name = \"youtube\"\ndomains = [\"youtube.com\"]\nbackends = [\"chrome\"]\nactions = [\"play\"]\n",
            &["chrome.js"],
        );
        write_plugin(
            dir.path(),
            "example",
            "name = \"example\"\ndomains = [\"example.com\"]\nbackends = [\"webview\"]\nactions = [\"play\"]\n",
            &["webview.html"],
        );
        std::fs::create_dir_all(dir.path().join("not-a-plugin")).unwrap();
        let r = Registry::load_with(Some(dir.path().to_path_buf())).unwrap();
        let yt = r.get("youtube").unwrap();
        assert_eq!(yt.manifest.backends, vec![BackendKind::Chrome]);
        assert_eq!(yt.source, dir.path().join("youtube").display().to_string());
        assert_eq!(
            r.plugins().iter().filter(|p| p.name() == "youtube").count(),
            1
        );
        assert_eq!(
            r.for_url("https://sub.example.com/v").unwrap().0.name(),
            "example"
        );
        assert!(r.get("not-a-plugin").is_none());
    }

    #[test]
    fn broken_external_plugin_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), "bad", "name = \"bad\"\n", &[]);
        let e = Registry::load_with(Some(dir.path().to_path_buf())).unwrap_err();
        assert!(format!("{e:#}").contains("bad"), "{e:#}");
    }

    #[test]
    fn missing_external_dir_is_fine() {
        let r = Registry::load_with(Some(PathBuf::from("/nonexistent/tvctl-plugins"))).unwrap();
        assert!(r.get("youtube").is_some());
    }

    #[test]
    fn url_normalization() {
        assert_eq!(
            parse_url("youtu.be/x").unwrap().as_str(),
            "https://youtu.be/x"
        );
        assert_eq!(
            parse_url(" http://a.com/b ").unwrap().as_str(),
            "http://a.com/b"
        );
        assert!(parse_url("ftp://a.com/").is_err());
        assert!(parse_url("").is_err());
    }

    #[test]
    fn manifest_validation() {
        let parse = |toml: &str, files: &[&str]| {
            let mut m: BTreeMap<String, String> = files
                .iter()
                .map(|f| (f.to_string(), String::new()))
                .collect();
            m.insert("plugin.toml".into(), toml.into());
            Plugin::from_files(m, "t".into())
        };
        assert!(
            parse(
                "name = \"x\"\ndomains = []\nbackends = [\"chrome\"]\nactions = [\"play\"]",
                &["chrome.js"]
            )
            .is_err()
        );
        assert!(
            parse(
                "name = \"x\"\ndomains = [\"x.com\"]\nbackends = []\nactions = [\"play\"]",
                &[]
            )
            .is_err()
        );
        assert!(
            parse(
                "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"vlc\"]\nactions = [\"play\"]",
                &[]
            )
            .is_err()
        );
        assert!(
            parse(
                "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"chrome\"]\nactions = [\"play\"]\ncolor = 1",
                &["chrome.js"]
            )
            .is_err()
        );
        assert!(
            parse(
                "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"webview\"]\nactions = [\"play\"]",
                &["chrome.js"]
            )
            .is_err()
        );
        let auth = "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"webview\", \"chrome\"]\nactions = [\"play\"]\n[auth]\nlogin_url = \"https://x.com/login\"\ncookie_url = \"https://x.com\"\nlogged_in_cookies = [\"sid\"]\nprefer_backend_when_logged_in = \"chrome\"\n";
        let p = parse(auth, &["webview.html", "chrome.js"]).unwrap();
        assert_eq!(
            p.manifest.auth.unwrap().prefer_backend_when_logged_in,
            Some(BackendKind::Chrome)
        );
    }

    #[test]
    fn actions_are_validated_and_queried() {
        let parse = |toml: &str| {
            let mut m = BTreeMap::new();
            m.insert("plugin.toml".to_string(), toml.to_string());
            m.insert("webview.html".to_string(), String::new());
            m.insert("chrome.js".to_string(), String::new());
            Plugin::from_files(m, "t".into())
        };
        let base = "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"webview\", \"chrome\"]\n";
        let e = parse(&format!("{base}actions = [\"fly\"]")).unwrap_err();
        assert!(format!("{e:#}").contains("unknown action"), "{e:#}");
        let e = parse(&format!(
            "{base}actions = [\"play\"]\n[unsupported_actions]\nwebview = [\"fly\"]"
        ))
        .unwrap_err();
        assert!(format!("{e:#}").contains("unknown action"), "{e:#}");
        assert!(parse(base).is_err(), "actions is required");
        let p = parse(&format!(
            "{base}actions = [\"play\", \"audio\"]\n[unsupported_actions]\nwebview = [\"audio\"]"
        ))
        .unwrap();
        let m = &p.manifest;
        assert!(m.supports("status", BackendKind::Webview));
        assert!(m.supports("play", BackendKind::Webview));
        assert!(m.supports("audio", BackendKind::Chrome));
        assert!(!m.supports("audio", BackendKind::Webview));
        assert!(!m.supports("seek", BackendKind::Chrome));
    }

    #[test]
    fn builtin_actions() {
        let r = Registry::load_with(None).unwrap();
        let yt = &r.get("youtube").unwrap().manifest;
        for a in ACTIONS {
            assert!(yt.supports(a, BackendKind::Chrome), "youtube chrome {a}");
        }
        let sp = &r.get("spotify").unwrap().manifest;
        for a in [
            "play", "pause", "toggle", "next", "previous", "seek", "volume",
        ] {
            assert!(sp.supports(a, BackendKind::Chrome), "spotify {a}");
        }
        for a in ["subtitles", "audio", "speed", "quality"] {
            assert!(!sp.supports(a, BackendKind::Chrome), "spotify {a}");
        }
        let pv = &r.get("primevideo").unwrap().manifest;
        assert_eq!(pv.backends, [BackendKind::Chrome]);
        for a in ACTIONS {
            assert_eq!(
                pv.supports(a, BackendKind::Chrome),
                *a != "previous",
                "primevideo {a}"
            );
        }
        let nf = &r.get("netflix").unwrap().manifest;
        assert_eq!(nf.backends, [BackendKind::Chrome]);
        for a in ACTIONS {
            let want = !matches!(*a, "previous" | "quality");
            assert_eq!(nf.supports(a, BackendKind::Chrome), want, "netflix {a}");
        }
        let cr = &r.get("crunchyroll").unwrap().manifest;
        assert_eq!(cr.backends, [BackendKind::Chrome]);
        for a in ACTIONS {
            assert_eq!(
                cr.supports(a, BackendKind::Chrome),
                *a != "previous",
                "crunchyroll {a}"
            );
        }
    }

    #[test]
    fn summaries_and_help() {
        let r = Registry::load_with(None).unwrap();
        let pv = r.describe("primevideo").unwrap();
        assert_eq!(pv["login"]["required"], true);
        assert_eq!(
            pv["login"]["commands"],
            serde_json::json!([
                "tvctl auth login primevideo",
                "tvctl auth login primevideo --site jp"
            ])
        );
        assert_eq!(pv["login"]["sites"], serde_json::json!(["us", "jp"]));
        // Login internals are not part of the summary.
        assert!(!pv.to_string().contains("logged_in_cookies"));
        assert!(pv["notes"].as_array().unwrap().len() > 1);
        let yt = r.describe("youtube").unwrap();
        assert_eq!(yt["login"]["required"], false);
        assert!(yt["login"].get("backend_when_logged_in").is_none());
        assert!(yt["login"].get("sites").is_none());
        let e = r.describe("hulu").unwrap_err().to_string();
        assert!(
            e.contains("no plugin named \"hulu\" (plugins: crunchyroll, netflix, primevideo, spotify, youtube)"),
            "{e}"
        );
        let help = r.help_section();
        assert!(
            help.contains("youtube") && help.contains("(login optional)"),
            "{help}"
        );
        assert!(
            help.contains("spotify") && help.contains("(login required)"),
            "{help}"
        );
        // An example must be on one of the plugin's domains.
        let mut files = BTreeMap::new();
        files.insert("chrome.js".to_string(), String::new());
        files.insert(
            "plugin.toml".to_string(),
            "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"chrome\"]\nactions = []\n\
             examples = [\"https://y.com/v\"]\n"
                .to_string(),
        );
        let e = Plugin::from_files(files, "t".into())
            .unwrap_err()
            .to_string();
        assert!(e.contains("not on one of its domains"), "{e}");
    }

    #[test]
    fn auth_sites() {
        let r = Registry::load_with(None).unwrap();
        let a = r.get("primevideo").unwrap().manifest.auth.as_ref().unwrap();
        let names: Vec<String> = a.all_sites().into_iter().map(|(k, _)| k).collect();
        assert_eq!(names, ["us", "jp"]);
        assert_eq!(a.site(None).unwrap().cookie_url, "https://www.amazon.com");
        assert_eq!(
            a.site(Some("jp")).unwrap().cookie_url,
            "https://www.amazon.co.jp"
        );
        let e = a.site(Some("uk")).unwrap_err().to_string();
        assert!(e.contains("unknown site \"uk\" (sites: us, jp)"), "{e}");
        // A single-site plugin has just the main site.
        let yt = r.get("youtube").unwrap().manifest.auth.as_ref().unwrap();
        assert_eq!(yt.all_sites().len(), 1);
        assert!(yt.site(None).is_ok());
        // The main site's name may not repeat in `sites`.
        let mut files = BTreeMap::new();
        files.insert("chrome.js".to_string(), String::new());
        files.insert(
            "plugin.toml".to_string(),
            "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"chrome\"]\nactions = []\n\
             [auth]\nsite = \"a\"\nlogin_url = \"https://x.com/l\"\ncookie_url = \"https://x.com\"\n\
             logged_in_cookies = [\"c\"]\n[auth.sites.a]\nlogin_url = \"https://x.com/l\"\n\
             cookie_url = \"https://x.com\"\nlogged_in_cookies = [\"c\"]\n"
                .to_string(),
        );
        let e = Plugin::from_files(files, "t".into())
            .unwrap_err()
            .to_string();
        assert!(e.contains("both the main site and in `sites`"), "{e}");
    }

    #[test]
    fn manifest_validation_minimal() {
        let mut files = BTreeMap::new();
        files.insert(
            "plugin.toml".to_string(),
            "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"chrome\"]\nactions = [\"play\"]\n"
                .to_string(),
        );
        assert!(Plugin::from_files(files.clone(), "t".into()).is_err());
        files.insert("chrome.js".to_string(), String::new());
        assert!(Plugin::from_files(files, "t".into()).is_ok());
    }
}
