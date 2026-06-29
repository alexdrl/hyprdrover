pub mod position;

use crate::ipc::{self, HyprClient, HyprWorkspaceRef, SessionSnapshot};
use std::collections::HashSet;
use std::error::Error;
use std::time::{Duration, Instant};

/// Orchestrates the restoration of a session in two distinct phases:
///
/// 1. **Reconcile**: match every saved window against the windows that are
///    *already running* and move those to their saved workspace/position.
///    Nothing is launched here.
/// 2. **Launch**: only the saved windows that had no running match are spawned
///    (each exactly once), then positioned by a single global poll loop.
///
/// Splitting the work this way avoids the duplicate-launch / per-window timeout
/// problems of interleaving "move" and "launch" for every window.
pub fn restore_session(snapshot: &SessionSnapshot) -> Result<(), Box<dyn Error>> {
    let current_state = ipc::capture_state()?;
    let mut available_clients = current_state.clients;

    // Baseline addresses to identify newly spawned windows after launching.
    let baseline_addresses: HashSet<String> = available_clients
        .iter()
        .map(|c| c.address.clone())
        .collect();

    // Preserve the currently active workspace so restore doesn't leave you elsewhere.
    let original_workspace_id = ipc::get_active_workspace()
        .map(|ws| ws.id)
        .unwrap_or(1);

    let mut restored_addresses: HashSet<String> = HashSet::new();

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
            restored_addresses.insert(current.address);
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
            &mut restored_addresses,
            Duration::from_secs(15),
        )?;
    }

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
/// PWAs get a class like `msedge-_<app_id>-<profile>` (e.g.
/// `msedge-_kldaona...-Default`). The app id is the 32-char Chromium id (a–p),
/// which is exactly what `--app-id=` expects. Returns `None` for non-PWA classes.
fn parse_pwa_class(class: &str) -> Option<(String, String)> {
    let after = class.split_once("-_")?.1; // "<app_id>-<profile>"
    let (app_id, profile) = after.split_once('-')?;
    let is_chromium_app_id =
        app_id.len() == 32 && app_id.bytes().all(|b| (b'a'..=b'p').contains(&b));
    if is_chromium_app_id && !profile.is_empty() {
        Some((app_id.to_string(), profile.to_string()))
    } else {
        None
    }
}

/// The browser binary to relaunch a PWA with: the captured argv[0], else the
/// kernel exe path.
fn browser_binary(saved: &HyprClient) -> Option<String> {
    if let Some(first) = saved.command.as_ref().and_then(|c| c.first()) {
        if !first.is_empty() {
            return Some(first.clone());
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
/// still-pending saved window and position it. Each new window is positioned at
/// most once, so duplicate spawns can't happen here.
fn position_launched(
    missing: &[&HyprClient],
    baseline_addresses: &HashSet<String>,
    restored_addresses: &mut HashSet<String>,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let mut pending: Vec<&HyprClient> = missing.to_vec();
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();

    while !pending.is_empty() && start.elapsed() < timeout {
        let state = ipc::capture_state()?;
        for client in &state.clients {
            if baseline_addresses.contains(&client.address)
                || restored_addresses.contains(&client.address)
            {
                continue;
            }
            if let Some(pos) = pending
                .iter()
                .position(|saved| launched_window_matches(client, saved))
            {
                let saved = pending.remove(pos);
                println!("   ✓ Positioned {}", client.class);
                position::restore_window_position(client, saved)?;
                restored_addresses.insert(client.address.clone());
            }
        }
        if pending.is_empty() {
            break;
        }
        std::thread::sleep(poll_interval);
    }

    for saved in &pending {
        eprintln!(
            "   ⚠️ Gave up waiting for {} to appear (it may still open later)",
            saved.class
        );
    }

    Ok(())
}

fn launched_window_matches(current: &HyprClient, saved: &HyprClient) -> bool {
    if let (Some(current_path), Some(saved_path)) = (&current.exe_path, &saved.exe_path) {
        if current_path == saved_path {
            return true;
        }
    }

    let saved_class = saved.class.to_lowercase();
    let saved_initial_class = saved.initial_class.to_lowercase();
    let current_class = current.class.to_lowercase();
    let current_initial_class = current.initial_class.to_lowercase();

    (!saved_class.is_empty()
        && (current_class == saved_class || current_initial_class == saved_class))
        || (!saved_initial_class.is_empty()
            && (current_class == saved_initial_class
                || current_initial_class == saved_initial_class))
}
