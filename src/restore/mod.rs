pub mod position;

use crate::ipc::{self, HyprClient, HyprWorkspaceRef, SessionSnapshot};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::thread;
use std::time::{Duration, Instant};

/// Orchestrates the restoration of a session in three phases:
///
/// 1. **Reconcile**: match every saved window against the windows that are
///    *already running* and move those to their saved workspace/position.
/// 2. **Launch**: spawn the saved windows that had no running match, then a
///    single global poll positions them as they appear.
/// 3. **Regroup**: rebuild every tabbed window group, whether its members were
///    already open or freshly launched.
///
/// Phases 1 and 2 also record a map from each saved window's address to the
/// real address it ended up as, which phase 3 needs to drive grouping.
pub fn restore_session(snapshot: &SessionSnapshot) -> Result<(), Box<dyn Error>> {
    let current_state = ipc::capture_state()?;
    let mut available_clients = current_state.clients;

    let baseline_addresses: HashSet<String> = available_clients
        .iter()
        .map(|c| c.address.clone())
        .collect();

    let original_workspace_id = ipc::get_active_workspace()
        .map(|ws| ws.id)
        .unwrap_or(1);

    // saved (old) address -> current real address of the restored window.
    let mut addr_map: HashMap<String, String> = HashMap::new();

    // ---- PHASE 1: reconcile already-open windows ----
    println!("Phase 1: relocating already-open windows...");
    let mut missing: Vec<&HyprClient> = Vec::new();
    for saved in &snapshot.clients {
        if let Some(index) = available_clients
            .iter()
            .position(|c| launched_window_matches(c, saved))
        {
            let current = available_clients.remove(index);
            println!(
                "   ↪ Moving {} → {}",
                current.class,
                workspace_label(&saved.workspace)
            );
            position::restore_window_position(&current, saved)?;
            addr_map.insert(saved.address.clone(), current.address);
        } else {
            missing.push(saved);
        }
    }

    // ---- PHASE 2: launch the windows that aren't running ----
    if missing.is_empty() {
        println!("Phase 2: nothing to launch — every saved window was already open.");
    } else {
        println!("Phase 2: launching {} missing window(s)...", missing.len());
        for saved in &missing {
            launch_app(saved);
        }
        position_launched(
            &missing,
            &baseline_addresses,
            &mut addr_map,
            Duration::from_secs(15),
        )?;
    }

    // ---- PHASE 3: rebuild tabbed window groups ----
    restore_groups(snapshot, &addr_map);

    // Return to the workspace we started on (best effort).
    let _ = ipc::focus_workspace(original_workspace_id);

    Ok(())
}

/// Human-readable label for a workspace (uses the name for special workspaces).
fn workspace_label(ws: &HyprWorkspaceRef) -> String {
    if ws.id < 0 {
        format!("workspace {}", ws.name)
    } else {
        format!("workspace {}", ws.id)
    }
}

/// The target string accepted by `movetoworkspacesilent` / `[workspace ...]`.
/// Special workspaces (negative id) must be addressed by name, e.g. `special:magic`.
fn workspace_target(ws: &HyprWorkspaceRef) -> String {
    if ws.id < 0 {
        ws.name.clone()
    } else {
        ws.id.to_string()
    }
}

/// Best command line to relaunch a saved window.
///
/// Order of preference:
/// 1. Chromium/Edge PWAs: reconstruct `--profile-directory=<p> --app-id=<id>`
///    from the app id encoded in the window class. The captured cmdline only
///    holds the base browser binary, so without this a PWA reopens as a plain
///    browser window.
/// 2. The captured argv (`command`), required for Electron apps & normal apps.
/// 3. The kernel exe path.
/// 4. A class-based heuristic as a last resort.
fn resolve_launch(saved: &HyprClient) -> Vec<String> {
    if let Some((app_id, profile)) = parse_pwa_class(&saved.class) {
        if let Some(base) = browser_binary(saved) {
            return vec![
                base,
                format!("--profile-directory={}", profile),
                format!("--app-id={}", app_id),
            ];
        }
    }
    // Flatpak apps: their captured command/exe_path point inside the sandbox and
    // aren't runnable from the host — launch via `flatpak run <app-id>`.
    if let Some(app_id) = &saved.flatpak {
        if !app_id.is_empty() {
            return vec!["flatpak".to_string(), "run".to_string(), app_id.clone()];
        }
    }
    if let Some(cmd) = &saved.command {
        if !cmd.is_empty() {
            return cmd.clone();
        }
    }
    if let Some(path) = &saved.exe_path {
        if !path.is_empty() {
            return vec![path.clone()];
        }
    }
    let raw_name = if !saved.initial_class.is_empty() {
        &saved.initial_class
    } else {
        &saved.class
    };
    vec![resolve_command(raw_name)]
}

/// Parse a Chromium/Edge PWA window class into `(app_id, profile_directory)`.
///
/// PWAs get a class like `msedge-<app_id>-<profile>` (e.g.
/// `msedge-kldaona...-Default`). Under XWayland the browser sets the class
/// with an extra underscore: `msedge-_<app_id>-<profile>`. Both forms are
/// accepted. The app id is the 32-char Chromium id (a–p), which is exactly
/// what `--app-id=` expects. Returns `None` for non-PWA classes.
fn parse_pwa_class(class: &str) -> Option<(String, String)> {
    let (_, after) = class.split_once('-')?; // "[_]<app_id>-<profile>"
    let after = after.strip_prefix('_').unwrap_or(after);
    let (app_id, profile) = after.split_once('-')?;
    let is_chromium_app_id =
        app_id.len() == 32 && app_id.bytes().all(|b| (b'a'..=b'p').contains(&b));
    if is_chromium_app_id && !profile.is_empty() {
        Some((app_id.to_string(), profile.to_string()))
    } else {
        None
    }
}

/// Key used to compare window classes. Lowercase; for a Chromium/Edge PWA the
/// key is `pwa:<app_id>:<profile>`, so the Wayland form (`msedge-<id>-Default`)
/// and the XWayland form (`msedge-_<id>-Default`) of the same app match.
fn class_key(class: &str) -> String {
    match parse_pwa_class(class) {
        Some((app_id, profile)) => format!("pwa:{}:{}", app_id, profile),
        None => class.to_lowercase(),
    }
}

/// The browser binary to relaunch a PWA with: the captured argv[0], else the
/// kernel exe path.
///
/// Sessions saved before the cmdline fix can hold the whole Chromium command
/// line as one element; only its first token is the binary.
fn browser_binary(saved: &HyprClient) -> Option<String> {
    if let Some(first) = saved.command.as_ref().and_then(|c| c.first()) {
        if let Some(bin) = first.split_whitespace().next() {
            return Some(bin.to_string());
        }
    }
    saved
        .exe_path
        .as_ref()
        .filter(|p| !p.is_empty())
        .cloned()
}

/// Class-based command heuristic, used only when no real command is recorded.
fn resolve_command(class: &str) -> String {
    let lower = class.to_lowercase();
    match lower.as_str() {
        "brave-browser" => "brave".to_string(),
        "code" => "code".to_string(), // VS Code often has class "Code"
        "google-chrome" => "google-chrome-stable".to_string(),
        "com.mitchellh.ghostty" => "ghostty".to_string(),
        _ => lower,
    }
}

/// Fire-and-forget launch of a missing window onto its saved workspace.
fn launch_app(saved: &HyprClient) {
    let argv = resolve_launch(saved);
    let cmd_str = argv.join(" ");
    println!("   ⤷ Launching: {}", cmd_str);

    let _ = std::process::Command::new("notify-send")
        .arg("Restoring Session")
        .arg(format!("Launching {}...", saved.class))
        .spawn();

    let _ = ipc::exec_on_workspace(&cmd_str, &workspace_target(&saved.workspace));
}

/// Single global poll loop: as launched windows appear, match each against a
/// still-pending saved window, position it, and record the saved→current
/// address mapping. Each new window is matched at most once.
fn position_launched(
    missing: &[&HyprClient],
    baseline_addresses: &HashSet<String>,
    addr_map: &mut HashMap<String, String>,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let mut pending: Vec<&HyprClient> = missing.to_vec();
    let mut used: HashSet<String> = addr_map.values().cloned().collect();
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();

    while !pending.is_empty() && start.elapsed() < timeout {
        let state = ipc::capture_state()?;
        for client in &state.clients {
            if baseline_addresses.contains(&client.address) || used.contains(&client.address) {
                continue;
            }
            if let Some(pos) = pending
                .iter()
                .position(|saved| launched_window_matches(client, saved))
            {
                let saved = pending.remove(pos);
                println!("   ✓ Positioned {}", client.class);
                position::restore_window_position(client, saved)?;
                addr_map.insert(saved.address.clone(), client.address.clone());
                used.insert(client.address.clone());
            }
        }
        if pending.is_empty() {
            break;
        }
        thread::sleep(poll_interval);
    }

    for saved in &pending {
        eprintln!(
            "   ⚠️ Gave up waiting for {} to appear (it may still open later)",
            saved.class
        );
    }

    Ok(())
}

// ---- Phase 3: window groups ----

/// Detect every tabbed group in the snapshot. A group is the set of windows
/// sharing the same `grouped` member list; members are returned in tab order.
fn plan_groups(clients: &[HyprClient]) -> Vec<Vec<&HyprClient>> {
    let by_addr: HashMap<&str, &HyprClient> =
        clients.iter().map(|c| (c.address.as_str(), c)).collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut groups: Vec<Vec<&HyprClient>> = Vec::new();

    for client in clients {
        if client.grouped.len() < 2 {
            continue;
        }
        let mut key_parts = client.grouped.clone();
        key_parts.sort();
        let key = key_parts.join(",");
        if !seen.insert(key) {
            continue;
        }
        let members: Vec<&HyprClient> = client
            .grouped
            .iter()
            .filter_map(|addr| by_addr.get(addr.as_str()).copied())
            .collect();
        if members.len() >= 2 {
            groups.push(members);
        }
    }
    groups
}

/// Rebuild every saved group from the windows that now exist. Uses
/// `group:group_on_movetoworkspace` so that even already-open windows can be
/// merged into a group (the lua build has no `moveintogroup`). The option is
/// enabled for the duration and restored afterwards.
fn restore_groups(snapshot: &SessionSnapshot, addr_map: &HashMap<String, String>) {
    let groups = plan_groups(&snapshot.clients);
    if groups.is_empty() {
        return;
    }

    println!("Phase 3: rebuilding {} window group(s)...", groups.len());

    let previous = ipc::get_group_on_movetoworkspace();
    if let Err(e) = ipc::set_group_on_movetoworkspace(true) {
        eprintln!(
            "   ⚠️ Could not enable group_on_movetoworkspace ({}); skipping groups",
            e
        );
        return;
    }

    // Temp workspaces used to assemble groups away from their destination so
    // multiple groups headed for the same (special) workspace don't merge.
    let mut temp_ws = 90111;
    for group in &groups {
        let members: Vec<String> = group
            .iter()
            .filter_map(|c| addr_map.get(&c.address).cloned())
            .collect();
        let label = group[0].class.clone();
        if members.len() < 2 {
            eprintln!("   ⚠️ Group ({}) has fewer than 2 live members; skipping", label);
            continue;
        }
        let dest = workspace_target(&group[0].workspace);

        // Leave already-correct groups untouched: if the live windows already
        // form exactly this group, don't dissolve and rebuild it.
        if let Some(current_ws) = existing_group_ws(&members) {
            if current_ws == dest {
                println!(
                    "   = Group ({}) already intact on {}; leaving as-is",
                    label,
                    workspace_label(&group[0].workspace)
                );
                continue;
            }
            // Intact but on the wrong workspace: relocate the whole group
            // (moving one tab drags all of it) without dissolving it. OFF so it
            // doesn't merge into another group already at the destination.
            println!(
                "   ↪ Group ({}) intact; moving to {}",
                label,
                workspace_label(&group[0].workspace)
            );
            let _ = ipc::set_group_on_movetoworkspace(false);
            let _ = ipc::focus_window(&members[0]);
            let _ = ipc::move_window_to_workspace_target(&members[0], &dest);
            thread::sleep(Duration::from_millis(300));
            let _ = ipc::set_group_on_movetoworkspace(true);
            continue;
        }

        println!(
            "   ⊞ Grouping {} window(s) ({}) on {}",
            members.len(),
            label,
            workspace_label(&group[0].workspace)
        );
        if let Err(e) = rebuild_group(&members, temp_ws, &dest) {
            eprintln!("   ⚠️ Failed to rebuild group ({}): {}", label, e);
        }
        temp_ws += 2;
    }

    let _ = ipc::set_group_on_movetoworkspace(previous);
}

/// Assemble one group in a clean temp workspace, then move it whole to its
/// destination. Each step polls Hyprland's state instead of sleeping blindly.
fn rebuild_group(members: &[String], temp_ws: i32, dest: &str) -> Result<(), Box<dyn Error>> {
    let staging = (temp_ws + 1).to_string();
    let temp = temp_ws.to_string();
    let lead = &members[0];

    // 1. Move the leader to the temp workspace, dissolve any prior group, and
    //    make it a fresh group of one.
    ipc::move_window_to_workspace_target(lead, &temp)?;
    wait_until(|| client_ws(lead).as_deref() == Some(&temp), Duration::from_secs(3));
    dissolve_group(lead)?;
    ipc::focus_window(lead)?;
    if grouped_count(lead) == 0 {
        ipc::toggle_group()?;
        wait_until(|| grouped_count(lead) >= 1, Duration::from_secs(2));
    }

    // 2. Bring each remaining member into the group via a staging hop so the
    //    move actually crosses workspaces (which is what triggers the merge).
    for member in &members[1..] {
        dissolve_group(member)?;
        ipc::move_window_to_workspace_target(member, &staging)?;
        wait_until(
            || client_ws(member).as_deref() == Some(&staging),
            Duration::from_secs(3),
        );
        ipc::focus_window(lead)?; // group is the active window in temp
        let before = grouped_count(lead);
        ipc::move_window_to_workspace_target(member, &temp)?;
        wait_until(|| grouped_count(lead) > before, Duration::from_secs(3));
    }

    // 3. Move the whole group to its destination WITHOUT merging into any group
    //    already there (so two groups can share one special workspace).
    ipc::set_group_on_movetoworkspace(false)?;
    ipc::focus_window(lead)?;
    ipc::move_window_to_workspace_target(lead, dest)?;
    thread::sleep(Duration::from_millis(300));
    ipc::set_group_on_movetoworkspace(true)?; // re-enable for the next group

    Ok(())
}

/// Dissolve the group a window currently belongs to, if any (leaves all its
/// former members as standalone windows).
fn dissolve_group(address: &str) -> Result<(), Box<dyn Error>> {
    if grouped_count(address) > 1 {
        ipc::focus_window(address)?;
        ipc::toggle_group()?;
        wait_until(|| grouped_count(address) <= 1, Duration::from_secs(2));
    }
    Ok(())
}

/// If the live windows for `members` already form *exactly* this group (same
/// member set, no more, no fewer), returns the workspace target they're on;
/// otherwise `None`. Used to skip rebuilding groups that are already correct.
fn existing_group_ws(members: &[String]) -> Option<String> {
    let state = ipc::capture_state().ok()?;
    let target: HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
    let lead = state.clients.iter().find(|c| c.address == members[0])?;
    let current: HashSet<&str> = lead.grouped.iter().map(|s| s.as_str()).collect();
    if current == target {
        Some(workspace_target(&lead.workspace))
    } else {
        None
    }
}

/// Number of windows in the group the given address belongs to (0 if ungrouped).
fn grouped_count(address: &str) -> usize {
    ipc::capture_state()
        .ok()
        .and_then(|s| {
            s.clients
                .into_iter()
                .find(|c| c.address == address)
                .map(|c| c.grouped.len())
        })
        .unwrap_or(0)
}

/// The workspace target string the given window is currently on, if found.
fn client_ws(address: &str) -> Option<String> {
    ipc::capture_state().ok().and_then(|s| {
        s.clients
            .into_iter()
            .find(|c| c.address == address)
            .map(|c| workspace_target(&c.workspace))
    })
}

/// Poll `cond` until it holds or the timeout elapses.
fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(150));
    }
}

fn launched_window_matches(current: &HyprClient, saved: &HyprClient) -> bool {
    if let (Some(current_path), Some(saved_path)) = (&current.exe_path, &saved.exe_path) {
        if current_path == saved_path {
            return true;
        }
    }

    let saved_class = class_key(&saved.class);
    let saved_initial_class = class_key(&saved.initial_class);
    let current_class = class_key(&current.class);
    let current_initial_class = class_key(&current.initial_class);

    (!saved_class.is_empty()
        && (current_class == saved_class || current_initial_class == saved_class))
        || (!saved_initial_class.is_empty()
            && (current_class == saved_initial_class
                || current_initial_class == saved_initial_class))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "kldaonaeondlgcjdncdbnamchihmoalb";

    fn client(class: &str, command: Option<Vec<&str>>) -> HyprClient {
        HyprClient {
            address: "0x1".into(),
            at: [0, 0],
            size: [1, 1],
            workspace: HyprWorkspaceRef { id: 1, name: "1".into() },
            class: class.into(),
            title: String::new(),
            initial_class: class.into(),
            initial_title: String::new(),
            floating: false,
            pinned: false,
            monitor: 0,
            fullscreen: 0,
            xwayland: false,
            pid: 1,
            command: command.map(|c| c.into_iter().map(str::to_owned).collect()),
            exe_path: None,
            grouped: vec![],
            flatpak: None,
        }
    }

    #[test]
    fn pwa_class_wayland_form() {
        assert_eq!(
            parse_pwa_class(&format!("msedge-{}-Default", APP)),
            Some((APP.to_string(), "Default".to_string()))
        );
    }

    #[test]
    fn pwa_class_xwayland_form() {
        assert_eq!(
            parse_pwa_class(&format!("msedge-_{}-Default", APP)),
            Some((APP.to_string(), "Default".to_string()))
        );
    }

    #[test]
    fn pwa_class_rejects_plain_browser_and_others() {
        assert_eq!(parse_pwa_class("msedge"), None);
        assert_eq!(parse_pwa_class("org.telegram.desktop"), None);
        assert_eq!(parse_pwa_class("com.mitchellh.ghostty"), None);
        assert_eq!(parse_pwa_class(&format!("msedge-{}-", APP)), None);
    }

    #[test]
    fn saved_xwayland_pwa_matches_live_wayland_pwa() {
        let saved = client(&format!("msedge-_{}-Default", APP), None);
        let live = client(&format!("msedge-{}-Default", APP), None);
        assert!(launched_window_matches(&live, &saved));
    }

    #[test]
    fn different_pwas_do_not_match() {
        let saved = client(&format!("msedge-_{}-Default", APP), None);
        let live = client("msedge-pkooggnaalmfkidjmlhoelhdllpphaga-Default", None);
        assert!(!launched_window_matches(&live, &saved));
    }

    #[test]
    fn pwa_launch_from_old_session_with_joined_cmdline() {
        let saved = client(
            &format!("msedge-_{}-Default", APP),
            Some(vec![
                "/opt/microsoft/msedge/msedge --profile-directory=Default --app-id=pkooggnaalmfkidjmlhoelhdllpphaga",
            ]),
        );
        assert_eq!(resolve_launch(&saved)[0], "/opt/microsoft/msedge/msedge");
        assert_eq!(resolve_launch(&saved).len(), 3);
    }

    #[test]
    fn pwa_launch_uses_binary_only() {
        let saved = client(
            &format!("msedge-_{}-Default", APP),
            Some(vec![
                "/opt/microsoft/msedge/msedge",
                "--profile-directory=Default",
                "--app-id=pkooggnaalmfkidjmlhoelhdllpphaga",
                "--app-url=https://outlook.office365.com/mail/",
            ]),
        );
        assert_eq!(
            resolve_launch(&saved),
            vec![
                "/opt/microsoft/msedge/msedge",
                "--profile-directory=Default",
                &format!("--app-id={}", APP),
            ]
        );
    }
}
