pub mod hypr_commands;

// Re-export the actual functions and structs we created
pub use hypr_commands::{
    capture_state, exec_on_workspace, focus_window, focus_workspace, get_active_workspace,
    get_group_on_movetoworkspace, move_window_pixel, move_window_to_workspace_target,
    resize_window_pixel, set_group_on_movetoworkspace, toggle_floating, toggle_group, HyprClient,
    HyprWorkspaceRef, SessionSnapshot,
};
