//! Pointer drag and collapse for the Spaces sidebar (issue #174).
//!
//! The pane grid is refit on the reflow clock and again on release, not on
//! every pixel. The width and the collapsed flag are written when the gesture
//! commits.
use super::*;

fn blocked(host: &HostState) -> bool {
    host.spacing.layout != config::LayoutMode::Sidebar
        || host.restore_prompt.is_some()
        || host.context_menu.is_some()
        || host.palette.is_some()
        || host.space_picker.is_some()
        || host.session_prompt.is_some()
        || host.space_panel.is_some()
}

pub(super) fn column(host: &HostState) -> Option<(f32, f32, f32, f32)> {
    if host.spacing.layout != config::LayoutMode::Sidebar {
        return None;
    }
    let geom = host.mux.geom();
    let size = host.window.inner_size();
    let height = size.height as f32;
    if geom.sidebar_px > 0 {
        Some((0.0, geom.sidebar_px as f32, 0.0, height))
    } else if geom.rail_px > 0 {
        let width = geom.rail_px as f32;
        Some((size.width as f32 - width, width, 0.0, height))
    } else {
        None
    }
}

pub(super) fn grip_hot(host: &HostState) -> bool {
    if blocked(host) {
        return false;
    }
    let Some((x, y)) = host.pointer_px else {
        return false;
    };
    let Some((column_x, column_w, column_y, column_h)) = column(host) else {
        return false;
    };
    sidebar_width::hits_grip(
        sidebar_width::dock_for_rail(host.spacing.space_rail),
        column_x,
        column_w,
        column_y,
        column_h,
        x as f32,
        y as f32,
    )
}

fn chrome_of(host: &HostState) -> sidebar_width::Chrome {
    sidebar_width::Chrome {
        expanded_px: host.spacing.sidebar_width_px as f32,
        collapsed: host.spacing.sidebar_collapsed,
        dock: sidebar_width::dock_for_rail(host.spacing.space_rail),
    }
}

fn apply(host: &mut HostState, chrome: sidebar_width::Chrome, why: &str) {
    let width = chrome.expanded_px.round().clamp(
        sidebar_width::MIN_DESIGN_PX,
        sidebar_width::MAX_STORED_PX as f32,
    ) as u32;
    host.spacing.sidebar_width_px = width;
    host.spacing.sidebar_collapsed = chrome.collapsed;
    App::refit_geom(host, host.window.inner_size(), Some(why));
    host.dirty = true;
}

pub(super) fn save(host: &mut HostState) {
    let path = config::config_path();
    let (width, collapsed) = sidebar_width::persisted(chrome_of(host));
    if let Err(error) = config::save_preference(&path, "sidebar_width_px", toml_edit::value(width))
    {
        rail_toast(host, &format!("Could not save sidebar width: {error}"));
    }
    if let Err(error) =
        config::save_preference(&path, "sidebar_collapsed", toml_edit::value(collapsed))
    {
        rail_toast(host, &format!("Could not save sidebar collapse: {error}"));
    }
}

pub(super) fn toggle(host: &mut HostState) {
    if host.spacing.layout != config::LayoutMode::Sidebar {
        return;
    }
    apply(host, chrome_of(host).toggle(), "sidebar collapse");
    save(host);
    host.window.request_redraw();
}

pub(super) fn motion(host: &mut HostState, x: f64, y: f64) -> bool {
    if host.sidebar_drag.is_none() {
        return false;
    }
    host.pointer_px = Some((x, y));
    let started = host.sidebar_drag_at.unwrap_or_else(Instant::now);
    let now = started.elapsed().as_millis();
    let event = host
        .sidebar_drag
        .as_mut()
        .expect("drag")
        .pointer(x as f32, now);
    if event == sidebar_width::DragEvent::Reflow {
        let chrome = host.sidebar_drag.expect("drag").chrome();
        apply(host, chrome, "sidebar width");
    }
    host.window.request_redraw();
    true
}

pub(super) fn button(host: &mut HostState, state: ElementState, button: MouseButton) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    if state == ElementState::Pressed && grip_hot(host) {
        let now = Instant::now();
        if host.sidebar_grip_at.is_some_and(|previous| {
            now.saturating_duration_since(previous).as_millis() <= sidebar_width::DOUBLE_CLICK_MS
        }) {
            host.sidebar_grip_at = None;
            host.sidebar_drag = None;
            host.sidebar_drag_at = None;
            apply(host, chrome_of(host).reset_default(), "sidebar width reset");
            save(host);
            host.left_button_down = false;
            return true;
        }
        let window = host.window.inner_size().width as f32;
        let scale = host.mux.geom().chrome.scale_milli;
        host.sidebar_grip_at = Some(now);
        host.sidebar_drag = Some(sidebar_width::Drag::start(
            chrome_of(host),
            scale,
            window,
            0,
        ));
        host.sidebar_drag_at = Some(now);
        host.left_button_down = false;
        host.dirty = true;
        return true;
    }
    if state == ElementState::Released {
        if let Some(drag) = host.sidebar_drag.take() {
            let (chrome, _) = drag.release();
            host.sidebar_drag_at = None;
            apply(host, chrome, "sidebar width");
            save(host);
            sync_chrome_hover(host);
            return true;
        }
    }
    false
}
