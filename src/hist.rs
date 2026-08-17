// Local history view for a mirror pane.
//
// A mirror pane paints into the alternate screen, so it has no local
// scrollback of its own, and the remote side offers nothing to scroll either:
// the session API's `terminal.scroll` is deliver-to-the-app semantics (for a
// no-mouse pane it types history-cycling arrow keys — measured), and a
// classic/inline agent renderer swallows re-materialized SGR wheels without
// moving anything. The pane's real history lives server-side, and `pane read
// --source recent` can hand it to us — herdr even collects the conversation
// text of idle alternate-screen agents into it — so a wheel-up fetches that
// over the existing ssh ControlMaster and renders it LOCALLY. Read-only, no
// control session, no agent lock: a glance-scroll never touches the remote.

use std::process::Stdio;

use tokio::process::Command;

use crate::pane::sh_quote;
use crate::remote::SSH_COMMON_OPTS;

/// How many history lines one fetch asks for. Enough that hitting the top
/// means the pane really is near its beginning, small enough that the ansi
/// payload stays a sub-second read over an existing ControlMaster.
pub const FETCH_LINES: usize = 2000;

/// A fetched snapshot being viewed, plus how far up into it we are.
pub struct HistView {
    /// ansi-styled lines, oldest first, trailing blanks trimmed
    pub lines: Vec<String>,
    /// lines scrolled up from the bottom; clamped at render time
    pub offset: usize,
}

/// Fetch the remote pane's recent history as ansi lines. `None` on any
/// failure — the caller shows "history unavailable" and stays live.
pub async fn fetch(
    ssh_target: &str,
    remote_bin: Option<&str>,
    session: Option<&str>,
    pane: &str,
    ctl_path: Option<&str>,
    container: Option<&crate::pane::ContainerArg>,
) -> Option<Vec<String>> {
    let bin = crate::config::remote_herdr_expr(remote_bin, session);
    let cmd = format!(
        "exec {} pane read {} --source recent --format ansi --lines {}",
        bin,
        sh_quote(pane),
        FETCH_LINES
    );
    let mut sc = match container {
        Some(ct) => {
            let ids = crate::docker::resolve(&ct.docker_bin, &ct.kind).await.ok()?;
            let id = ids.into_iter().next()?;
            let mut c = Command::new(&ct.docker_bin);
            c.args(["exec", &id, "sh", "-c", &cmd]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            if let Some(path) = ctl_path {
                c.arg("-S").arg(path);
            }
            c.args(SSH_COMMON_OPTS).arg(ssh_target).arg(cmd);
            c
        }
    };
    let out =
        sc.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let mut lines: Vec<String> =
        String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines)
}

/// Which slice of the buffer a viewport shows at a given offset. Pure so the
/// clamping logic is testable: `rows_avail` is the viewport minus the status
/// bar, the returned offset is the clamped one to store back.
pub fn window(total: usize, rows_avail: usize, offset: usize) -> (usize, usize, usize) {
    let max_off = total.saturating_sub(rows_avail);
    let off = offset.min(max_off);
    let end = total - off;
    let start = end.saturating_sub(rows_avail);
    (start, end, off)
}

/// Paint the view: absolute cursor addressing per row so a repaint needs no
/// scroll region, autowrap off (the caller restores `?7h` on exit) so a line
/// wider than the local pane truncates instead of pushing rows out of place,
/// all inside a synchronized-update block. Returns the clamped offset.
pub fn render(view: &HistView, _cols: usize, rows: usize) -> (String, usize) {
    let rows_avail = rows.saturating_sub(1).max(1);
    let (start, end, off) = window(view.lines.len(), rows_avail, view.offset);
    let mut out = String::with_capacity(4096);
    out.push_str("\x1b[?2026h\x1b[?7l\x1b[?25l");
    for row in 0..rows_avail {
        out.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
        if let Some(line) = view.lines.get(start + row).filter(|_| start + row < end) {
            out.push_str(line);
            out.push_str("\x1b[0m");
        }
    }
    let above = off;
    let top = if start == 0 { " · top" } else { "" };
    out.push_str(&format!(
        "\x1b[{};1H\x1b[2K\x1b[7m history · {above} below live{top} · wheel down / any key to return \x1b[0m",
        rows.max(1)
    ));
    out.push_str("\x1b[?2026l");
    (out, off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_clamps_offset_to_what_exists() {
        // 100 lines, 20 usable rows: deepest offset shows lines 0..20
        assert_eq!(window(100, 20, 0), (80, 100, 0));
        assert_eq!(window(100, 20, 30), (50, 70, 30));
        assert_eq!(window(100, 20, 80), (0, 20, 80));
        assert_eq!(window(100, 20, 5000), (0, 20, 80));
    }

    #[test]
    fn window_with_fewer_lines_than_viewport_pins_to_top() {
        assert_eq!(window(5, 20, 0), (0, 5, 0));
        assert_eq!(window(5, 20, 3), (0, 5, 0));
        assert_eq!(window(0, 20, 3), (0, 0, 0));
    }

    #[test]
    fn render_reports_clamped_offset_and_marks_top() {
        let view = HistView { lines: (0..10).map(|i| format!("l{i}")).collect(), offset: 999 };
        let (out, off) = render(&view, 80, 6);
        // 5 usable rows over 10 lines: max offset is 5
        assert_eq!(off, 5);
        assert!(out.contains(" · top"));
        assert!(out.contains("l0"));
        assert!(!out.contains("l9"));
        // autowrap off and synchronized update are part of the contract
        assert!(out.contains("\x1b[?7l"));
        assert!(out.starts_with("\x1b[?2026h"));
        assert!(out.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn render_at_bottom_shows_last_lines_without_top_marker() {
        let view = HistView { lines: (0..10).map(|i| format!("l{i}")).collect(), offset: 0 };
        let (out, off) = render(&view, 80, 6);
        assert_eq!(off, 0);
        assert!(out.contains("l9"));
        assert!(!out.contains(" · top"));
    }
}
