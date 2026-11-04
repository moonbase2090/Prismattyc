// SPDX-License-Identifier: MIT
//! Submit menu operations to the Space worker and apply their completion.
use super::*;
use space_client::{CommandJob, Operation};

pub(super) fn delete(host: &mut HostState, name: String) {
    if let Err(error) = submit(host, Operation::Delete { name }) {
        rail_error_toast(host, &format!(" Could not delete: {error} "));
    }
}

pub(super) fn submit(host: &mut HostState, operation: Operation) -> Result<(), String> {
    if host.space_command_pending {
        return Err("another Space operation is still running".into());
    }
    let job = CommandJob {
        pmux: pmux_bin(),
        dir: spaces_dir(),
        operation,
        current: host.space_rail.current.clone(),
        owner: host.mux.space_id.clone(),
        epoch: host.space_epoch,
    };
    if !host.space_client.submit_command(job) {
        return Err("the Space worker is unavailable".into());
    }
    host.space_command_pending = true;
    host.dirty = true;
    Ok(())
}

pub(super) fn complete(
    host: &mut HostState,
    job: CommandJob,
    result: Result<(), String>,
    directory: space_rail::Directory,
    resolved: Option<(String, prismattyc_mux::SavedSpace)>,
) {
    host.space_command_pending = false;
    host.last_space_refresh = None;
    host.space_poll_loaded = None;
    if host.space_rail.apply_directory(directory) {
        rail_changed(host);
    }
    let same_visit = job.epoch == host.space_epoch && job.owner == host.mux.space_id;
    if let Err(error) = result {
        let label = match job.operation {
            Operation::Rename { .. } => "rename",
            Operation::Delete { .. } => "delete",
            Operation::Undo { .. } => "undo",
            Operation::PaneTitle { .. } => {
                if same_visit {
                    observe_host_action(host, keybind::Action::RenamePane, false);
                }
                "rename pane"
            }
        };
        rail_error_toast(host, &format!(" Could not {label}: {error} "));
        return;
    }
    match job.operation {
        Operation::Rename { new, .. } => {
            if same_visit {
                if let Some((name, _)) = resolved {
                    set_current_space(host, Some(name));
                }
                if host.space_rail.keyboard {
                    host.space_rail.focus =
                        host.space_rail.names.iter().position(|name| name == &new);
                }
            }
            rail_toast(host, &format!(" Renamed space to {new} "));
        }
        Operation::Delete { name } => {
            if same_visit {
                if let Some(picker) = host
                    .space_picker
                    .as_mut()
                    .filter(|picker| picker.kind == SpacePickerKind::Delete)
                {
                    picker.status = Some(format!("deleted {name}"));
                }
            }
            rail_toast(host, &format!(" Deleted space {name} "));
        }
        Operation::Undo { path } => {
            if same_visit && host.space_polish.undo.as_ref() == Some(&path) {
                host.space_polish.undo = None;
                host.observed_space_sessions.clear();
            }
            rail_toast(host, " Membership restored; sessions kept running ");
        }
        Operation::PaneTitle { .. } => {
            if same_visit {
                observe_host_action(host, keybind::Action::RenamePane, true);
            }
        }
    }
    host.dirty = true;
}
