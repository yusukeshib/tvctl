//! Service plugins.
//!
//! A plugin is a directory `plugins/<name>/` containing a `plugin.toml` manifest
//! and the scripts for the backends it supports:
//!
//! * `webview.html` — page loaded into the native WebView backend.
//! * `frame.js` — optional; injected into the subframes of that WebView.
//! * `chrome.js` — adapter evaluated in Chrome pages (Chrome backend).
//!
//! Every adapter defines `window.__play.run(action)` returning a status object.
//! Built-in plugins are embedded in the binary; external plugins are read from
//! `$PLAY_PLUGIN_DIR` or `${XDG_CONFIG_HOME:-~/.config}/play/plugins` and override
//! built-ins with the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};
use url::Url;

static BUILTIN: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/plugins");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
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
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub login_url: String,
    pub cookie_url: String,
    pub logged_in_cookies: Vec<String>,
    pub prefer_backend_when_logged_in: Option<BackendKind>,
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

    pub fn file(&self, name: &str) -> Option<&str> {
        self.files.get(name).map(String::as_str)
    }

    fn handles_host(&self, host: &str) -> bool {
        self.manifest
            .domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    fn from_files(files: BTreeMap<String, String>, source: String) -> Result<Plugin> {
        let manifest_src = files.get("plugin.toml").context("missing plugin.toml")?;
        let manifest: Manifest = toml::from_str(manifest_src).context("parsing plugin.toml")?;
        if manifest.domains.is_empty() {
            bail!("plugin {}: `domains` is empty", manifest.name);
        }
        if manifest.backends.is_empty() {
            bail!("plugin {}: `backends` is empty", manifest.name);
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
        if let Some(root) = external_dir().filter(|d| d.is_dir()) {
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
    if let Some(dir) = std::env::var_os("PLAY_PLUGIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("play").join("plugins"))
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
        assert!(yt.file("webview.html").is_some());
        for u in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(r.for_url(u).unwrap().0.name(), "youtube", "{u}");
        }
        assert!(r.for_url("https://example.com/").is_err());
        assert!(r.for_url("https://notyoutube.com/").is_err());
        assert!(r.for_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn manifest_validation() {
        let mut files = BTreeMap::new();
        files.insert(
            "plugin.toml".to_string(),
            "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"chrome\"]\n".to_string(),
        );
        assert!(Plugin::from_files(files.clone(), "t".into()).is_err());
        files.insert("chrome.js".to_string(), String::new());
        assert!(Plugin::from_files(files, "t".into()).is_ok());
    }
}
