//! Shell integration without opening windows or starting the real server.
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn cli(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tvctl"));
    cmd.env("TVCTL_SOCKET", dir.join("tvctl.sock"))
        .env("TVCTL_PLUGIN_DIR", dir.join("plugins"))
        .env_remove("TVCTL_COMPLETE");
    cmd
}

fn completion(dir: &std::path::Path, shell: &str, words: &[&str]) -> Output {
    cli(dir)
        .env("TVCTL_COMPLETE", shell)
        .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
        .env("_CLAP_COMPLETE_COMP_TYPE", "9")
        .env("_CLAP_COMPLETE_SPACE", "false")
        .args(["--"])
        .args(words)
        .output()
        .unwrap()
}

fn text(out: Output) -> String {
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn temp() -> tempfile::TempDir {
    // Unix socket paths have a short limit, including on macOS.
    tempfile::tempdir_in("/tmp").unwrap()
}

#[test]
fn config_is_shell_code_and_rejects_invalid_shells() {
    let dir = temp();
    for shell in ["bash", "zsh"] {
        let code = text(cli(dir.path()).args(["config", shell]).output().unwrap());
        assert!(code.contains("TVCTL_COMPLETE="));
        assert!(!code.ends_with("null\n"));
    }
    for args in [vec!["config"], vec!["config", "fish"]] {
        let out = cli(dir.path()).args(args).output().unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }
    assert!(!dir.path().join("tvctl.sock").exists());
}

#[test]
fn completion_without_a_server_is_silent_and_does_not_start_one() {
    let dir = temp();
    for shell in ["bash", "zsh"] {
        assert!(text(completion(dir.path(), shell, &["tvctl", "pause", "1"])).is_empty());
        assert!(
            text(completion(dir.path(), shell, &["tvctl", "plugins", "net"])).contains("netflix")
        );
        assert!(!dir.path().join("tvctl.sock").exists());
    }
}

#[test]
fn screenshot_paths_with_spaces_are_completed() {
    let dir = temp();
    std::fs::write(dir.path().join("my screenshot.png"), b"").unwrap();
    let out = cli(dir.path())
        .current_dir(dir.path())
        .env("TVCTL_COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .env("_CLAP_COMPLETE_SPACE", "false")
        .args(["--", "tvctl", "screenshot", "my"])
        .output()
        .unwrap();
    assert_eq!(text(out).trim(), "my screenshot.png");
}

/// Commands take no player id, so completion never contacts the server.
#[test]
fn completion_does_not_contact_the_server() {
    let dir = temp();
    let listener = UnixListener::bind(dir.path().join("tvctl.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    for words in [
        &["tvctl", "pause", ""][..],
        &["tvctl", "window", "--layout", "left", ""][..],
    ] {
        completion(dir.path(), "bash", words);
        completion(dir.path(), "zsh", words);
    }
    assert!(
        listener.accept().is_err(),
        "completion connected to the server"
    );
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn generated_code_registers_in_real_shells() {
    let dir = temp();
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_tvctl"));
    let path = std::env::join_paths(
        std::iter::once(bin.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    for (shell, script) in [
        (
            "bash",
            r#"
            eval "$(tvctl config bash)"
            COMP_WORDS=(tvctl config z)
            COMP_CWORD=2
            COMP_TYPE=9
            _clap_complete_tvctl tvctl z config
            [[ ${COMPREPLY[0]} == zsh* ]]
        "#,
        ),
        (
            "zsh",
            r#"
            autoload -Uz compinit
            compinit -D
            eval "$(tvctl config zsh)"
            [[ $_comps[tvctl] == _clap_dynamic_completer_tvctl ]]
            # Capture what the registered completer offers to zsh.
            _describe() { local array=$3; print -rl -- "${(@P)array}"; }
            words=(tvctl config z)
            CURRENT=3
            [[ $(_clap_dynamic_completer_tvctl) == zsh ]]
        "#,
        ),
    ] {
        let out = Command::new(shell)
            .args(["-f", "-c", script])
            .env("PATH", &path)
            .env("TVCTL_SOCKET", dir.path().join("tvctl.sock"))
            .env("TVCTL_PLUGIN_DIR", dir.path().join("plugins"))
            .env_remove("TVCTL_COMPLETE")
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell}: {out:?}");
        assert!(out.stderr.is_empty(), "{shell}: {out:?}");
    }
}
