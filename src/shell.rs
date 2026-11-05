//! Runtime completions derived from the CLI, with read-only dynamic values.

use std::ffi::OsStr;
use std::io::Write;

use clap::{CommandFactory, ValueEnum};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::env::{Bash, EnvCompleter, Zsh};
use serde_json::Value;

use crate::{Cli, client, plugin};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
}

pub fn register(shell: Shell, out: &mut dyn Write) -> std::io::Result<()> {
    let completer: &dyn EnvCompleter = match shell {
        Shell::Zsh => &Zsh,
        Shell::Bash => &Bash,
    };
    completer.write_registration("TVCTL_COMPLETE", "tvctl", "tvctl", "tvctl", out)
}

pub fn command() -> clap::Command {
    enrich(Cli::command())
}

// Keep the command tree, option arities and enums in one place: the clap CLI.
fn enrich(cmd: clap::Command) -> clap::Command {
    let plugins = cmd.get_name() == "plugins";
    cmd.mut_args(|arg| match arg.get_id().as_str() {
        "id" => arg.add(ArgValueCompleter::new(players)),
        "service" => arg.add(ArgValueCompleter::new(auth_services)),
        "name" if plugins => arg.add(ArgValueCompleter::new(services)),
        "url" => arg.value_hint(clap::ValueHint::Url),
        _ => arg,
    })
    .mut_subcommands(enrich)
}

fn players(current: &OsStr) -> Vec<CompletionCandidate> {
    client::completion_list()
        .map(|value| player_candidates(&value, current))
        .unwrap_or_default()
}

fn player_candidates(value: &Value, current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    value["players"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|player| {
            let id = player["id"].as_str()?;
            if !id.starts_with(prefix) {
                return None;
            }
            let title = player["title"].as_str().unwrap_or_default();
            let service = player["service"].as_str().unwrap_or_default();
            let description = format!("{service} {title}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Some(CompletionCandidate::new(id).help(Some(description.into())))
        })
        .collect()
}

fn services(current: &OsStr) -> Vec<CompletionCandidate> {
    service_candidates(current, false)
}

fn auth_services(current: &OsStr) -> Vec<CompletionCandidate> {
    service_candidates(current, true)
}

fn service_candidates(current: &OsStr, auth_only: bool) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let Ok(registry) = plugin::Registry::load() else {
        return Vec::new();
    };
    registry
        .plugins()
        .iter()
        .filter(|p| p.name().starts_with(prefix) && (!auth_only || p.manifest.auth.is_some()))
        .map(|p| {
            CompletionCandidate::new(p.name()).help(Some(p.manifest.description.clone().into()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::engine::complete;
    use serde_json::json;

    fn candidates(words: &[&str]) -> Vec<String> {
        complete(
            &mut command(),
            words.iter().map(Into::into).collect(),
            words.len() - 1,
            None,
        )
        .unwrap()
        .iter()
        .map(|c| c.get_value().to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn command_tree_and_enums() {
        let root = candidates(&["tvctl", ""]);
        for cmd in Cli::command()
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
        {
            assert!(root.contains(&cmd.get_name().to_owned()), "{cmd:?}");
        }
        assert!(!root.contains(&"__serve".to_owned()));
        assert!(!root.contains(&"__eval".to_owned()));
        let shells = candidates(&["tvctl", "config", ""]);
        assert!(shells.contains(&"zsh".to_owned()));
        assert!(shells.contains(&"bash".to_owned()));
        assert_eq!(candidates(&["tvctl", "server", "st"]), ["status"]);
        assert_eq!(candidates(&["tvctl", "cast", "c"]), ["code"]);
        assert!(
            candidates(&["tvctl", "open", "https://youtu.be/x", "--"])
                .contains(&"--backend".to_owned())
        );
        assert_eq!(
            candidates(&["tvctl", "open", "--backend", "ch"]),
            ["chrome"]
        );
        assert_eq!(
            candidates(&["tvctl", "window", "1", "--layout", "bottom-r"]),
            ["bottom-right"]
        );
        assert_eq!(
            candidates(&["tvctl", "window", "--on-top=of"]),
            ["--on-top=off"]
        );
        assert_eq!(candidates(&["tvctl", "fullscreen", "1", "of"]), ["off"]);
    }

    #[test]
    fn player_ids_and_descriptions() {
        let value = json!({"players": [
            {"id": "1", "service": "youtube", "title": "A video\nwith a title"},
            {"id": "12", "service": "spotify", "title": "A song"},
            {"id": "2", "service": "netflix"},
            {"missing": "id"}
        ]});
        let matches = player_candidates(&value, OsStr::new("1"));
        let ids: Vec<_> = matches.iter().map(|c| c.get_value()).collect();
        assert_eq!(ids, ["1", "12"]);
        assert_eq!(
            matches[0].get_help().unwrap().to_string(),
            "youtube A video with a title"
        );
        assert!(player_candidates(&value, OsStr::new("9")).is_empty());
        assert!(player_candidates(&Value::Null, OsStr::new("")).is_empty());
    }

    #[test]
    fn installed_services() {
        assert_eq!(candidates(&["tvctl", "plugins", "net"]), ["netflix"]);
        assert_eq!(
            candidates(&["tvctl", "auth", "login", "pri"]),
            ["primevideo"]
        );
        assert_eq!(candidates(&["tvctl", "auth", "status", "you"]), ["youtube"]);
    }

    #[test]
    fn registration() {
        for shell in [Shell::Zsh, Shell::Bash] {
            let mut code = Vec::new();
            register(shell, &mut code).unwrap();
            let code = String::from_utf8(code).unwrap();
            assert!(code.contains("TVCTL_COMPLETE="));
            assert!(code.contains("tvctl"));
            assert!(!code.ends_with("null\n"));
        }
    }
}
