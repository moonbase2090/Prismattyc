//! A fixed side rail width. Pointer dragging changes geometry immediately.
use super::*;
pub(super) fn at_edge(host: &HostState) -> bool {
    if host.restore_prompt.is_some()
        || host.context_menu.is_some()
        || host.palette.is_some()
        || host.space_picker.is_some()
        || host.session_prompt.is_some()
        || host.space_panel.is_some()
    {
        return false;
    }
    let Some((x, y)) = host.pointer_px else {
        return false;
    };
    let geom = host.mux.geom();
    let edge = match geom.rail_side {
        space_rail::RailSide::Left => geom.rail_px as f64,
        space_rail::RailSide::Right => host.window.inner_size().width as f64 - geom.rail_px as f64,
        _ => return false,
    };
    y >= geom.top_chrome_px as f64 && (x - edge).abs() <= 5.0
}
pub(super) fn motion(host: &mut HostState, x: f64, y: f64) -> bool {
    if !host.rail_resizing {
        return false;
    }
    host.pointer_px = Some((x, y));
    let width = host.window.inner_size().width as f64;
    let px = if host.spacing.space_rail == space_rail::RailSide::Right {
        width - x
    } else {
        x
    };
    let max = ((width / host.font.cell_w as f64) as usize / 2).clamp(8, 60);
    let cols = ((px / host.font.cell_w as f64).round() as usize).clamp(8, max);
    if cols != host.spacing.space_rail_width_cols {
        host.spacing.space_rail_width_cols = cols;
        App::refit_geom(host, host.window.inner_size(), Some("rail width"));
        host.dirty = true;
    }
    host.window.request_redraw();
    true
}
pub(super) fn button(host: &mut HostState, state: ElementState, button: MouseButton) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    if state == ElementState::Pressed && at_edge(host) {
        host.rail_resizing = true;
        host.left_button_down = false;
        return true;
    }
    if state == ElementState::Released && host.rail_resizing {
        host.rail_resizing = false;
        if let Err(error) = config::save_preference(
            &config::config_path(),
            "space_rail_width_cols",
            toml_edit::value(host.spacing.space_rail_width_cols as i64),
        ) {
            rail_toast(host, &format!("Could not save rail width: {error}"));
        }
        sync_chrome_hover(host);
        return true;
    }
    false
}
