use crate::ipc::{self, HyprClient};
use std::error::Error;

/// Restores the position and workspace of a single window
pub fn restore_window_position(
    current_client: &HyprClient,
    saved_client: &HyprClient,
) -> Result<(), Box<dyn Error>> {
    // Move to workspace. Special workspaces (negative id) must be addressed by
    // name (e.g. `special:magic`); numeric ids are used for regular workspaces.
    if current_client.workspace.id != saved_client.workspace.id {
        let target = if saved_client.workspace.id < 0 {
            saved_client.workspace.name.clone()
        } else {
            saved_client.workspace.id.to_string()
        };
        ipc::move_window_to_workspace_target(&current_client.address, &target)?;
    }

    // Move to position & Resize
    if saved_client.floating {
        if !current_client.floating {
            ipc::toggle_floating(&current_client.address)?;
        }
        ipc::move_window_pixel(
            &current_client.address,
            saved_client.at[0],
            saved_client.at[1],
        )?;
        ipc::resize_window_pixel(
            &current_client.address,
            saved_client.size[0],
            saved_client.size[1],
        )?;
    } else {
        // Saved as tiled
        if current_client.floating {
            ipc::toggle_floating(&current_client.address)?;
        }
        // For tiled windows, we can't easily force pixel positions without floating them.
        // We just move them to the workspace for now.
    }

    Ok(())
}
