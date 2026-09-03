use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::process::Command;
use std::sync::OnceLock;

// --- Data Models (matching hyprctl -j output) ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HyprWorkspaceRef {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HyprActiveWorkspace {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HyprClient {
    pub address: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: HyprWorkspaceRef,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub initial_title: String,
    pub floating: bool,
    pub pinned: bool,
    pub monitor: i64,
    pub fullscreen: i32, // 0: none, 1: maximized, 2: fullscreen
    pub xwayland: bool,
    pub pid: i32,

    /// Full argv-style command used to launch the application.
    /// This is required to correctly restore PWAs, Electron apps,
    /// and browser-based app runtimes.
    #[serde(default)]
    pub command: Option<Vec<String>>,

    /// Fallback executable path from /proc/<pid>/exe.
    /// Used only if command is unavailable.
    #[serde(default)]
    pub exe_path: Option<String>,

    /// Addresses of all windows in this window's group (tabbed group),
    /// including itself. Empty when the window is not grouped. The order
    /// reflects the tab order. Reported directly by `hyprctl clients`.
    #[serde(default)]
    pub grouped: Vec<String>,

    /// Flatpak application id (e.g. `com.spotify.Client`) when the window
    /// belongs to a Flatpak sandbox. Such apps must be relaunched with
    /// `flatpak run <id>`; their `command`/`exe_path` point inside the sandbox
    /// and aren't executable from the host.
    #[serde(default)]
    pub flatpak: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HyprWorkspace {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    pub windows: i32,
    pub hasfullscreen: bool,
    pub lastwindow: String,
    pub lastwindowtitle: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HyprMonitor {
    pub id: i64,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_rate: f32,
    pub x: i32,
    pub y: i32,
    pub active_workspace: HyprWorkspaceRef,
}

// --- Helper Struct for the full snapshot ---

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub clients: Vec<HyprClient>,
    pub workspaces: Vec<HyprWorkspace>,
    pub monitors: Vec<HyprMonitor>,
}

// --- Implementation ---

/// Execute a hyprctl command and return the output as a string
fn run_hyprctl(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("hyprctl")
        .arg("-j")
        .args(args)
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "hyprctl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Get all open windows (clients)
fn get_clients() -> Result<Vec<HyprClient>, Box<dyn Error>> {
    let json = run_hyprctl(&["clients"])?;
    Ok(serde_json::from_str(&json)?)
}

/// Get all active workspaces
fn get_workspaces() -> Result<Vec<HyprWorkspace>, Box<dyn Error>> {
    let json = run_hyprctl(&["workspaces"])?;
    Ok(serde_json::from_str(&json)?)
}

/// Get all connected monitors
fn get_monitors() -> Result<Vec<HyprMonitor>, Box<dyn Error>> {
    let json = run_hyprctl(&["monitors"])?;
    Ok(serde_json::from_str(&json)?)
}

/// Get the active workspace for the currently focused monitor
pub fn get_active_workspace() -> Result<HyprActiveWorkspace, Box<dyn Error>> {
    let json = run_hyprctl(&["activeworkspace"])?;
    Ok(serde_json::from_str(&json)?)
}

/// Extract a Flatpak app id from a cgroup line such as
/// `0::/user.slice/.../app-flatpak-com.spotify.Client-816252360.scope`.
/// Returns e.g. `com.spotify.Client`, or `None` if not a Flatpak scope.
fn parse_flatpak_app_id(cgroup: &str) -> Option<String> {
    const MARKER: &str = "app-flatpak-";
    let start = cgroup.find(MARKER)? + MARKER.len();
    let rest = &cgroup[start..];
    let end = rest.find(".scope")?;
    let token = &rest[..end]; // e.g. "com.spotify.Client-816252360"
    match token.rsplit_once('-') {
        // Strip the trailing "-<instance-number>" if present.
        Some((id, num)) if !id.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => {
            Some(id.to_string())
        }
        _ => Some(token.to_string()),
    }
}

/// Split a `/proc/<pid>/cmdline` buffer into argv.
///
/// Normal processes separate the arguments with NUL bytes. Chromium-based
/// browsers (Edge, Chrome, Electron) rewrite their argv area at startup and
/// leave one string with the arguments joined by spaces. In that case split on
/// whitespace, so that `args[0]` is the binary and not the whole command line.
fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if args.len() == 1 && args[0].contains(char::is_whitespace) {
        return args[0].split_whitespace().map(str::to_owned).collect();
    }
    args
}

/// Capture the entire current state of Hyprland
pub fn capture_state() -> Result<SessionSnapshot, Box<dyn Error>> {
    let mut clients = get_clients()?;

    for client in &mut clients {
        // Flatpak app id from the process cgroup, if sandboxed.
        if let Ok(cgroup) = fs::read_to_string(format!("/proc/{}/cgroup", client.pid)) {
            client.flatpak = parse_flatpak_app_id(&cgroup);
        }

        let cmdline_path = format!("/proc/{}/cmdline", client.pid);
        let exe_path = format!("/proc/{}/exe", client.pid);

        // Prefer full argv from /proc/<pid>/cmdline
        if let Ok(bytes) = fs::read(&cmdline_path) {
            let args = parse_cmdline(&bytes);

            if !args.is_empty() {
                client.command = Some(args);
                continue;
            }
        }

        // Fallback: kernel-reported executable path
        if let Ok(path) = fs::read_link(&exe_path) {
            client.exe_path = Some(path.to_string_lossy().into_owned());
        }
    }

    Ok(SessionSnapshot {
        clients,
        workspaces: get_workspaces()?,
        monitors: get_monitors()?,
    })
}

// --- Dispatch Commands (Actions) ---

/// Cached detection of the `hyprland-lua` plugin parser.
static LUA_PARSER: OnceLock<bool> = OnceLock::new();

/// Detect whether the `hyprland-lua` plugin parser is active.
///
/// Under that parser, `hyprctl dispatch <native>` is evaluated as a *Lua
/// expression* (`return hl.dispatch(<native>)`) and silently fails — so window
/// moves/resizes never happen. When detected we emit `hl.dsp.*` Lua calls
/// instead of native dispatchers.
///
/// Probe: `hl.dsp.no_op()` returns "ok" only when the Lua parser interprets it;
/// a stock Hyprland reports an invalid dispatcher.
fn lua_parser() -> bool {
    *LUA_PARSER.get_or_init(|| {
        Command::new("hyprctl")
            .args(["dispatch", "hl.dsp.no_op()"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok")
            .unwrap_or(false)
    })
}

/// Render a workspace selector as a Lua value: a bare number for numeric ids,
/// or a quoted string for names like `special:magic`.
fn lua_workspace(target: &str) -> String {
    if target.parse::<i32>().is_ok() {
        target.to_string()
    } else {
        format!("\"{}\"", target)
    }
}

/// Run a single, already-formatted `hyprctl dispatch <arg>`.
///
/// The whole command is passed as ONE argument (never split on whitespace) so
/// Lua expressions like `hl.dsp.window.move({ ... })` survive intact. Under the
/// Lua parser, errors are reported on stdout with exit code 0, so we also scan
/// stdout for an `error:` marker.
fn dispatch_arg(arg: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new("hyprctl").arg("dispatch").arg(arg).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || stdout.trim_start().starts_with("error:") {
        return Err(format!(
            "Dispatch failed: {}{}",
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

/// Execute a raw hyprctl dispatch command (native syntax).
/// Kept for completeness; prefer the semantic helpers below.
#[allow(dead_code)]
pub fn dispatch(command: &str) -> Result<(), Box<dyn Error>> {
    dispatch_arg(command)
}

/// Move a specific window to a workspace.
/// `target` is a workspace selector: a numeric id (e.g. "3") or a name
/// (e.g. "special:magic" for special workspaces).
pub fn move_window_to_workspace_target(address: &str, target: &str) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!(
            "hl.dsp.window.move({{ workspace = {}, window = \"address:{}\" }})",
            lua_workspace(target),
            address
        )
    } else {
        format!("movetoworkspacesilent {},address:{}", target, address)
    };
    dispatch_arg(&arg)
}

/// Toggle the floating state of a specific window.
pub fn toggle_floating(address: &str) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!(
            "hl.dsp.window.float({{ action = \"toggle\", window = \"address:{}\" }})",
            address
        )
    } else {
        format!("togglefloating address:{}", address)
    };
    dispatch_arg(&arg)
}

/// Move a window to an exact pixel coordinate (used for floating windows).
pub fn move_window_pixel(address: &str, x: i32, y: i32) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!(
            "hl.dsp.window.move({{ x = {}, y = {}, window = \"address:{}\" }})",
            x, y, address
        )
    } else {
        format!("movewindowpixel exact {} {},address:{}", x, y, address)
    };
    dispatch_arg(&arg)
}

/// Resize a window to exact dimensions.
pub fn resize_window_pixel(address: &str, width: i32, height: i32) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!(
            "hl.dsp.window.resize({{ x = {}, y = {}, window = \"address:{}\" }})",
            width, height, address
        )
    } else {
        format!("resizewindowpixel exact {} {},address:{}", width, height, address)
    };
    dispatch_arg(&arg)
}

/// Focus a specific window by address.
pub fn focus_window(address: &str) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!("hl.dsp.focus({{ window = \"address:{}\" }})", address)
    } else {
        format!("focuswindow address:{}", address)
    };
    dispatch_arg(&arg)
}

/// Toggle the group state of the focused window (creates or dissolves a group).
/// With `group:auto_group` enabled (Hyprland's default), windows launched while
/// a group is focused are added to it — this is how restore rebuilds groups.
pub fn toggle_group() -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        "hl.dsp.group.toggle()".to_string()
    } else {
        "togglegroup".to_string()
    };
    dispatch_arg(&arg)
}

/// Read the current value of `group:group_on_movetoworkspace`.
/// Returns false if it can't be determined.
pub fn get_group_on_movetoworkspace() -> bool {
    run_hyprctl(&["getoption", "group:group_on_movetoworkspace"])
        .ok()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .and_then(|v| v.get("int").and_then(|i| i.as_i64()))
        .map(|i| i != 0)
        .unwrap_or(false)
}

/// Set `group:group_on_movetoworkspace`. When enabled, moving a window into a
/// workspace that holds a group merges it into that group — the only way to add
/// an already-open window to a group under the lua build (no `moveintogroup`).
pub fn set_group_on_movetoworkspace(enabled: bool) -> Result<(), Box<dyn Error>> {
    if lua_parser() {
        let arg = format!(
            "hl.config({{ group = {{ group_on_movetoworkspace = {} }} }}) or hl.dsp.no_op()",
            enabled
        );
        dispatch_arg(&arg)
    } else {
        let output = Command::new("hyprctl")
            .arg("keyword")
            .arg("group:group_on_movetoworkspace")
            .arg(if enabled { "true" } else { "false" })
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "keyword failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }
}

/// Switch the focused monitor to a workspace (by numeric id).
pub fn focus_workspace(id: i32) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        format!("hl.dsp.focus({{ workspace = {} }})", id)
    } else {
        format!("workspace {}", id)
    };
    dispatch_arg(&arg)
}

/// Launch a command onto a target workspace.
/// `target` is a workspace selector (numeric id or name). Under the Lua parser
/// the workspace rule doesn't reliably place the window, so the caller's
/// post-launch poll is relied upon to reposition it.
pub fn exec_on_workspace(cmd: &str, target: &str) -> Result<(), Box<dyn Error>> {
    let arg = if lua_parser() {
        let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
        format!("hl.dsp.exec_cmd(\"{}\")", escaped)
    } else {
        format!("exec [workspace {} silent] {}", target, cmd)
    };
    dispatch_arg(&arg)
}

#[cfg(test)]
mod tests {
    use super::parse_cmdline;

    #[test]
    fn cmdline_nul_separated() {
        let args = parse_cmdline(b"/usr/bin/foo\0--flag\0a b\0");
        assert_eq!(args, vec!["/usr/bin/foo", "--flag", "a b"]);
    }

    #[test]
    fn cmdline_rewritten_by_chromium() {
        let args = parse_cmdline(
            b"/opt/microsoft/msedge/msedge --profile-directory=Default --app-id=kldaonaeondlgcjdncdbnamchihmoalb",
        );
        assert_eq!(
            args,
            vec![
                "/opt/microsoft/msedge/msedge",
                "--profile-directory=Default",
                "--app-id=kldaonaeondlgcjdncdbnamchihmoalb",
            ]
        );
    }

    #[test]
    fn cmdline_single_arg_without_spaces() {
        assert_eq!(parse_cmdline(b"/usr/bin/foo\0"), vec!["/usr/bin/foo"]);
    }
}
