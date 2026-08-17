// Foreground-process detection for the mirror streamer.
//
// herdr strips the mouse-mode DECSET from the frames the plugin observes, so the
// streamer can't tell whether the remote pane's app wants the mouse. As a proxy,
// query the remote pane's foreground process (`herdr pane process-info`) and
// classify it. This is a heuristic stand-in until herdr exposes the pane's
// mouse-reporting state through the API.
//
// "Is it a TUI?" turned out to be the wrong question. A plain shell at a prompt
// never enables mouse reporting — but neither does an agent CLI, which is
// full-screen and so answered "TUI" and got the mouse grabbed on its behalf.
// Holding the grab is precisely what stops herdr doing native text selection, so
// in a mirror of an agent host — where nearly every pane is an agent — selection
// was dead everywhere and the forwarded clicks went to a program that had never
// asked for them. The question that matters is "does this process want the
// mouse?", which is answered `false` for two different reasons.

use std::process::Stdio;

use tokio::process::Command;

use crate::pane::sh_quote;
use crate::remote::SSH_COMMON_OPTS;

/// Interactive shells: at a prompt these don't enable mouse reporting, so mouse
/// events over them should stay local rather than being forwarded to the pty.
const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "ksh93", "mksh", "ash", "tcsh",
    "csh", "nu", "elvish", "xonsh", "osh", "ysh", "oil", "ion", "murex", "ngs",
    "pwsh", "powershell", "cmd",
];

/// Agent CLIs: full-screen, but they do not turn mouse reporting on. Same
/// conclusion as a shell — keep the mouse local — reached for a different reason,
/// so it is a separate list. Kept lowercase and extension-free; `basename`
/// normalizes the incoming name before matching.
const MOUSE_BLIND_TUIS: &[&str] = &[
    "claude", "codex", "gemini", "cursor-agent", "opencode", "aider", "goose",
    "crush", "grok", "qwen", "kimi", "amp", "droid", "pi", "antigravity",
    "hermes",
];

/// Strip a login-shell dash (`-bash`), any leading path, and a Windows `.exe`
/// suffix, then lowercase — the form both lists are written in.
fn basename(name: &str) -> String {
    let base = name.trim_start_matches('-').rsplit(['/', '\\']).next().unwrap_or(name);
    base.trim_end_matches(".exe").to_ascii_lowercase()
}

/// Is `name` one of the known interactive shells?
pub fn is_shell(name: &str) -> bool {
    SHELLS.contains(&basename(name).as_str())
}

/// Does this foreground process enable mouse reporting? `extra` is the per-host
/// `mouse_local_apps` escape hatch: new agent CLIs appear faster than this list
/// is updated, and the cost of a miss is a pane you cannot select text in.
pub fn wants_mouse(name: &str, extra: &[String]) -> bool {
    let n = basename(name);
    !SHELLS.contains(&n.as_str())
        && !MOUSE_BLIND_TUIS.contains(&n.as_str())
        && !extra.iter().any(|e| basename(e) == n)
}

/// What the remote foreground implies for local input handling. Two answers, not
/// one, because they answer different questions: `is_shell` picks the cursor-key
/// encoding, `wants_mouse` decides the mouse grab. An agent CLI is *not* a shell
/// — it sets DECCKM, so arrows must stay in application mode — and still doesn't
/// want the mouse. Collapsing them is the bug this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fg {
    pub is_shell: bool,
    pub wants_mouse: bool,
}

/// Classify a `pane process-info` JSON response. `None` = indeterminate
/// (empty/unparseable), so the caller keeps its last known value.
pub fn classify(json: &str, extra: &[String]) -> Option<Fg> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let fg = v.get("result")?.get("process_info")?.get("foreground_processes")?.as_array()?;
    // An agent CLI anywhere in the chain wins over the leaf. A working
    // agent's leaf is whatever tool it just spawned (`node`, `python3`,
    // `rg`, a shell — changing every few seconds), but those children are
    // batch commands, not TUIs, and the AGENT keeps reading the pty the
    // whole time. Classifying the leaf made the pane flip to "mouse-wanting
    // TUI" exactly while the agent worked, so the grab was held and the
    // wheel forwarded — straight into the fullscreen agent's conversation
    // view (and native selection died for the duration of every tool call).
    let agent = fg.iter().filter_map(|p| p.get("name").and_then(|n| n.as_str())).any(|name| {
        let n = basename(name);
        MOUSE_BLIND_TUIS.contains(&n.as_str()) || extra.iter().any(|e| basename(e) == n)
    });
    if agent {
        return Some(Fg { is_shell: false, wants_mouse: false });
    }
    // otherwise the last foreground process is the actually-running leaf, so
    // `sudo vim` classifies on `vim`, not `sudo`
    let name = fg.last()?.get("name")?.as_str()?;
    Some(Fg { is_shell: is_shell(name), wants_mouse: wants_mouse(name, extra) })
}

/// Query the remote pane's foreground process over ssh and classify it. `None`
/// on any failure (ssh/network/parse) so the caller keeps its last known value.
pub async fn poll(
    ssh_target: &str,
    remote_bin: Option<&str>,
    session: Option<&str>,
    pane: &str,
    ctl_path: Option<&str>,
    container: Option<&crate::pane::ContainerArg>,
    extra_mouse_local: &[String],
) -> Option<Fg> {
    // same expression as the observe session (configured path or PATH auto)
    let bin = crate::config::remote_herdr_expr(remote_bin, session);
    let cmd = format!("exec {} pane process-info --pane {}", bin, sh_quote(pane));
    let mut sc = match container {
        Some(ct) => {
            // async resolve, not the blocking one: this runs on the pane's
            // single-threaded runtime and fires on every keystroke burst, so a
            // blocking `docker ps` would stall input and rendering (and hang
            // the pane outright if the Docker daemon wedges).
            //
            // No ControlMaster equivalent is needed — docker exec is local, so
            // there is no handshake to amortize.
            let ids = crate::docker::resolve(&ct.docker_bin, &ct.kind).await.ok()?;
            let id = ids.into_iter().next()?;
            let mut c = Command::new(&ct.docker_bin);
            // `sh -c` not `-lc`: match ssh's non-login remote shell
            c.args(["exec", &id, "sh", "-c", &cmd]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            // reuse the daemon's ControlMaster when given so the poll skips the
            // handshake; `-S` without `-M` uses an existing master or, if the socket
            // isn't there, connects directly — so this degrades gracefully
            if let Some(path) = ctl_path {
                c.arg("-S").arg(path);
            }
            c.args(SSH_COMMON_OPTS).arg(ssh_target).arg(cmd);
            c
        }
    };
    let out = sc
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    classify(&String::from_utf8_lossy(&out.stdout), extra_mouse_local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_recognized_including_login_and_path() {
        assert!(is_shell("zsh"));
        assert!(is_shell("bash"));
        assert!(is_shell("-bash")); // login shell
        assert!(is_shell("/usr/bin/fish")); // full path
        assert!(is_shell("pwsh.exe")); // windows
        assert!(!is_shell("vim"));
        assert!(!is_shell("htop"));
        assert!(!is_shell("nvim"));
        assert!(!is_shell("lazygit"));
    }

    fn fg(name: &str) -> String {
        format!(r#"{{"result":{{"process_info":{{"foreground_processes":[{{"name":"{name}"}}]}}}}}}"#)
    }

    #[test]
    fn classify_reads_leaf_foreground() {
        assert_eq!(classify(&fg("zsh"), &[]), Some(Fg { is_shell: true, wants_mouse: false }));
        assert_eq!(classify(&fg("vim"), &[]), Some(Fg { is_shell: false, wants_mouse: true }));
        // sudo wrapper: the leaf is the real program
        let sudo =
            r#"{"result":{"process_info":{"foreground_processes":[{"name":"sudo"},{"name":"vim"}]}}}"#;
        assert_eq!(classify(sudo, &[]), Some(Fg { is_shell: false, wants_mouse: true }));
    }

    #[test]
    fn a_working_agent_still_classifies_as_the_agent_not_its_tool() {
        // claude mid-tool-call: the leaf is the spawned command, but claude
        // keeps reading the pty — the pane must NOT flip to mouse-wanting
        for leaf in ["node", "python3", "rg", "zsh"] {
            let chain = format!(
                r#"{{"result":{{"process_info":{{"foreground_processes":[{{"name":"claude"}},{{"name":"sh"}},{{"name":"{leaf}"}}]}}}}}}"#
            );
            assert_eq!(
                classify(&chain, &[]),
                Some(Fg { is_shell: false, wants_mouse: false }),
                "claude running {leaf} must stay classified as the agent"
            );
        }
        // the escape-hatch list joins the chain scan too
        let chain = r#"{"result":{"process_info":{"foreground_processes":[{"name":"myagent"},{"name":"node"}]}}}"#;
        let extra = vec!["myagent".to_string()];
        assert_eq!(classify(chain, &extra), Some(Fg { is_shell: false, wants_mouse: false }));
    }

    #[test]
    fn classify_indeterminate_on_empty_or_garbage() {
        assert_eq!(classify(r#"{"result":{"process_info":{"foreground_processes":[]}}}"#, &[]), None);
        assert_eq!(classify("not json", &[]), None);
        assert_eq!(classify(r#"{"result":{}}"#, &[]), None);
    }

    #[test]
    fn an_agent_cli_is_not_a_shell_but_still_does_not_want_the_mouse() {
        // the whole point: these two answers must be allowed to disagree, or
        // application cursor keys break when the mouse is fixed (and vice versa)
        for name in ["claude", "codex", "gemini", "opencode", "cursor-agent"] {
            let c = classify(&fg(name), &[]).unwrap();
            assert!(!c.is_shell, "{name} must keep application cursor keys");
            assert!(!c.wants_mouse, "{name} must leave the mouse to herdr");
        }
    }

    #[test]
    fn a_real_mouse_aware_tui_still_gets_the_mouse() {
        for name in ["vim", "nvim", "htop", "lazygit", "emacs"] {
            assert!(wants_mouse(name, &[]), "{name} must still receive clicks");
        }
    }

    #[test]
    fn the_escape_hatch_normalizes_like_the_builtin_lists() {
        let extra = vec!["MyAgent.exe".to_string(), "/opt/bin/tool".to_string()];
        assert!(!wants_mouse("myagent", &extra));
        assert!(!wants_mouse("/usr/local/bin/MyAgent", &extra));
        assert!(!wants_mouse("tool", &extra));
        assert!(wants_mouse("unrelated", &extra));
    }
}
