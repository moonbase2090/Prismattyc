//! One name creates or renames a session and its mailbox.

use super::*;

#[derive(Clone, Debug)]
enum Target {
    Pane { axis: Option<prismattyc_mux::Axis> },
    Layout { count: usize, quadrants: bool },
    Add { space: String },
    Space { space: String },
    Rename { pane: PaneId, session: String },
}

pub(super) struct Prompt {
    target: Target,
    action: Option<keybind::Action>,
    pub(super) buffer: String,
    selected: bool,
    button: usize,
    anchor: PaneId,
    space: Option<String>,
    error: Option<String>,
}

fn suggested(space: &str) -> String {
    let output = std::process::Command::new(pmux_bin())
        .args(["session", "suggest", "--space", space])
        .stdin(std::process::Stdio::null())
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if prismattyc_mux::mailbox::AgentId::new(name.clone()).is_ok() {
                return name;
            }
        }
    }
    let names = attach_log::session_names()
        .into_values()
        .collect::<Vec<_>>();
    prismattyc_mux::session_name::suggest(space, &names)
}

fn begin(host: &mut HostState, target: Target, name: String) {
    host.space_rail.leave();
    host.tab_rename = None;
    host.palette = None;
    host.context_menu = None;
    host.context_menu_target = None;
    host.splash = None;
    host.preedit = Preedit::default();
    let action = match &target {
        Target::Pane { axis: None } => Some(keybind::Action::NewTab),
        Target::Pane {
            axis: Some(prismattyc_mux::Axis::Horizontal),
        } => Some(keybind::Action::SplitRight),
        Target::Pane {
            axis: Some(prismattyc_mux::Axis::Vertical),
        } => Some(keybind::Action::SplitDown),
        Target::Layout { count, .. } => Some(keybind::Action::Layout(*count as u8)),
        Target::Rename { .. } => Some(keybind::Action::RenamePane),
        Target::Add { .. } | Target::Space { .. } => None,
    };
    host.session_prompt = Some(Prompt {
        target,
        action,
        buffer: name,
        selected: true,
        button: 0,
        anchor: host.mux.focused_id(),
        space: host.space_rail.current.clone(),
        error: None,
    });
    host.palette_layout = None;
    host.dirty = true;
    host.window.request_redraw();
}

fn allows_blank(prompt: &Prompt) -> bool {
    matches!(prompt.target, Target::Pane { .. } | Target::Layout { .. })
}

fn button_count(prompt: &Prompt) -> usize {
    if allows_blank(prompt) {
        3
    } else {
        2
    }
}

fn begin_creation(host: &mut HostState, target: Target, name: String) {
    let mode = config::load(&config::config_path())
        .unwrap_or_default()
        .session_naming
        .unwrap_or_default();
    begin(host, target, name);
    if mode == "auto" || mode == "blank" {
        finish_choice(host, true, mode == "blank");
        if let Some(prompt) = host.session_prompt.take() {
            if let Some(error) = prompt.error {
                rail_toast(host, &format!("Could not create terminal: {error}"));
            }
        }
    }
}

pub(super) fn create_pane(host: &mut HostState, axis: Option<prismattyc_mux::Axis>) {
    if config::load(&config::config_path())
        .unwrap_or_default()
        .session_naming
        .as_deref()
        == Some("blank")
    {
        direct(host, axis, true);
        return;
    }
    let name = suggested(host.space_rail.current.as_deref().unwrap_or("session"));
    begin_creation(host, Target::Pane { axis }, name);
}

pub(super) fn direct(host: &mut HostState, axis: Option<prismattyc_mux::Axis>, blank: bool) {
    let name = if blank {
        String::new()
    } else {
        suggested(host.space_rail.current.as_deref().unwrap_or("session"))
    };
    begin(host, Target::Pane { axis }, name);
    finish_choice(host, true, blank);
}

pub(super) fn create_layout(host: &mut HostState, count: usize, quadrants: bool) {
    let name = suggested(host.space_rail.current.as_deref().unwrap_or("session"));
    begin_creation(host, Target::Layout { count, quadrants }, name);
}

pub(super) fn add_to_space(host: &mut HostState, space: String) {
    let name = suggested(&space);
    begin(host, Target::Add { space }, name);
}

pub(super) fn create_space(host: &mut HostState, space: String) {
    let name = suggested(&space);
    begin(host, Target::Space { space }, name);
}

pub(super) fn retry_space(host: &mut HostState, space: String, name: String, error: String) {
    begin(host, Target::Space { space }, name);
    host.session_prompt.as_mut().unwrap().error = Some(error);
}

pub(super) fn rename(host: &mut HostState, pane: PaneId, action: keybind::Action) -> bool {
    let Some(session) = host.mux.attach_session_of(pane).map(str::to_string) else {
        return false;
    };
    let name = attach_log::session_names()
        .get(&session)
        .cloned()
        .or_else(|| host.mux.attach_name_of(pane).map(str::to_string))
        .unwrap_or_else(|| session.clone());
    begin(host, Target::Rename { pane, session }, name);
    host.session_prompt.as_mut().unwrap().action = Some(action);
    true
}

fn run(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(pmux_bin())
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn spawn_standalone(
    host: &mut HostState,
    axis: Option<prismattyc_mux::Axis>,
    name: &str,
) -> Result<()> {
    run(&["new", "--no-attach", name])?;
    let snapshot =
        attach_log::live_snapshot().context("session created; daemon snapshot unavailable")?;
    let session = snapshot
        .sessions
        .iter()
        .find(|s| s.name == name)
        .context("created session not found")?;
    let id = session.id.to_string();
    let args = vec!["attach".into(), "--session-id".into(), id.clone()];
    let program = find_mux_bin().to_string_lossy().into_owned();
    let pane = if let Some(axis) = axis {
        host.mux.split_focused(&program, &args, axis, 0.5)?
    } else {
        host.mux.new_tab(&program, &args)?;
        host.mux.focused_id()
    };
    host.mux.mark_attach_session(pane, id.clone(), name.into());
    host.attach_pane_sessions.insert(pane, id);
    if axis.is_none() {
        host.mux.rename_window(host.mux.active_window(), name)?;
    }
    Ok(())
}

fn apply(host: &mut HostState, target: &Target, name: &str) -> Result<()> {
    prismattyc_mux::mailbox::AgentId::new(name.to_string())?;
    match target {
        Target::Pane { axis } => {
            if host.space_rail.current.is_some() {
                spawn_owned_space_pane(host, *axis, Some(name))?;
            } else {
                spawn_standalone(host, *axis, name)?;
            }
        }
        Target::Layout { count, quadrants } => {
            let axis = Some(prismattyc_mux::Axis::Horizontal);
            if host.space_rail.current.is_some() {
                spawn_owned_space_pane(host, axis, Some(name))?;
            } else {
                spawn_standalone(host, axis, name)?;
            }
            if host.mux.active_pane_count() >= *count {
                if *quadrants {
                    host.mux.ensure_even_quadrants("/bin/sh", &[])?;
                } else {
                    host.mux.ensure_even_columns("/bin/sh", &[], *count)?;
                }
            }
        }
        Target::Add { space } => {
            run(&["space", "add", space, "--name", name])?;
            host.last_space_refresh = None;
        }
        Target::Space { space } => {
            host.space_opens.enqueue_named(space, name.into());
            advance_space_opens(host);
        }
        Target::Rename { pane, session } => {
            if host.mux.attach_session_of(*pane) != Some(session.as_str()) {
                bail!("this pane has changed sessions; close and reopen the naming popup");
            }
            run(&["session", "name", name, "--session", session])?;
            host.mux
                .mark_attach_session(*pane, session.clone(), name.into());
            let index = focused_tab_index(host, *pane);
            if host.mux.tab_panes()[index].1.len() == 1 {
                if let Some(window) = host.mux.window_at_tab(index) {
                    host.mux.rename_window(window, name)?;
                }
            }
            host.last_space_refresh = None;
        }
    }
    finish_arrangement(host);
    Ok(())
}

fn finish_arrangement(host: &mut HostState) {
    mark_layout_dirty(host);
    App::refit_geom(host, host.window.inner_size(), Some("new terminal"));
    host.app_mouse_button = None;
    host.last_app_mouse_cell = None;
    host.window
        .set_title(&window_title(&host.mux, show_tab_strip(host)));
}

fn apply_blank(host: &mut HostState, target: &Target) -> Result<()> {
    let axis = match target {
        Target::Pane { axis } => *axis,
        Target::Layout { .. } => Some(prismattyc_mux::Axis::Horizontal),
        _ => bail!("blank terminals are available for new panes and tabs"),
    };
    let command = prismattyc_mux::platform::default_shell_command();
    let shell = command[0].clone();
    let args = command[1..].to_vec();
    let pane = if let Some(axis) = axis {
        host.mux.split_focused(&shell, &args, axis, 0.5)?
    } else {
        host.mux.new_tab(&shell, &args)?;
        host.mux
            .rename_window(host.mux.active_window(), "Terminal")?;
        host.mux.focused_id()
    };
    host.mux.retain_local_terminal(pane);
    if let Target::Layout { count, quadrants } = target {
        if host.mux.active_pane_count() >= *count {
            if *quadrants {
                host.mux.ensure_even_quadrants(&shell, &args)?;
            } else {
                host.mux.ensure_even_columns(&shell, &args, *count)?;
            }
        }
    }
    finish_arrangement(host);
    Ok(())
}

pub(super) fn activate(host: &mut HostState, index: usize) {
    let Some(prompt) = &host.session_prompt else {
        return;
    };
    if index >= button_count(prompt) {
        return;
    }
    let blank = allows_blank(prompt) && index == 1;
    finish_choice(host, index == 0 || blank, blank);
}

pub(super) fn finish(host: &mut HostState, accept: bool) {
    finish_choice(host, accept, false);
}

fn finish_choice(host: &mut HostState, accept: bool, blank: bool) {
    let Some(mut prompt) = host.session_prompt.take() else {
        return;
    };
    if accept {
        let anchored = !matches!(prompt.target, Target::Pane { .. } | Target::Layout { .. })
            || (host.mux.focused_id() == prompt.anchor && host.space_rail.current == prompt.space);
        let result = if anchored {
            if blank {
                apply_blank(host, &prompt.target)
            } else {
                apply(host, &prompt.target, prompt.buffer.trim())
            }
        } else {
            Err(anyhow::anyhow!(
                "the active pane changed; close and reopen the naming popup"
            ))
        };
        match result {
            Ok(()) => {
                if let Target::Layout { count, quadrants } = prompt.target {
                    if host.mux.active_pane_count() < count {
                        create_layout(host, count, quadrants);
                    }
                }
                if host.session_prompt.is_none() {
                    if let Some(action) = prompt.action {
                        observe_host_action(host, action, true);
                    }
                }
            }
            Err(error) => {
                prompt.error = Some(error.to_string());
                host.session_prompt = Some(prompt);
            }
        }
    }
    host.palette_layout = None;
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn dispatch_key(host: &mut HostState, key: &Key, repeat: bool) {
    if repeat && matches!(key, Key::Named(NamedKey::Enter | NamedKey::Escape)) {
        return;
    }
    if matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("v"))
        && (host.modifiers.control_key() || host.modifiers.super_key())
    {
        if host.clipboard.is_none() {
            host.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(text) = host.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
            let prompt = host.session_prompt.as_mut().unwrap();
            for ch in text.trim().chars() {
                apply_rename_stroke(
                    &mut prompt.buffer,
                    &mut prompt.selected,
                    RenameStroke::Insert(ch),
                );
            }
            prompt.error = None;
        }
        host.dirty = true;
        host.window.request_redraw();
        return;
    }
    match key {
        Key::Named(NamedKey::Enter) => {
            let index = host.session_prompt.as_ref().map_or(0, |p| p.button);
            activate(host, index);
        }
        Key::Named(NamedKey::Tab | NamedKey::ArrowUp | NamedKey::ArrowDown) => {
            if let Some(prompt) = host.session_prompt.as_mut() {
                let count = button_count(prompt);
                let backwards = matches!(key, Key::Named(NamedKey::ArrowUp))
                    || (matches!(key, Key::Named(NamedKey::Tab)) && host.modifiers.shift_key());
                prompt.button = (prompt.button + if backwards { count - 1 } else { 1 }) % count;
            }
        }
        Key::Named(NamedKey::Escape) => finish(host, false),
        _ => {
            if let Some(prompt) = host.session_prompt.as_mut() {
                if let Key::Character(text) = key {
                    if (host.modifiers.control_key() || host.modifiers.super_key())
                        && text.eq_ignore_ascii_case("a")
                    {
                        prompt.selected = true;
                    } else if !host.modifiers.control_key() && !host.modifiers.super_key() {
                        for ch in text.chars() {
                            apply_rename_stroke(
                                &mut prompt.buffer,
                                &mut prompt.selected,
                                RenameStroke::Insert(ch),
                            );
                        }
                    }
                } else if let Some(stroke) = rename_stroke_from_logical(key) {
                    apply_rename_stroke(&mut prompt.buffer, &mut prompt.selected, stroke);
                }
                prompt.error = None;
            }
        }
    }
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn handle_pointer(host: &mut HostState, event: &WindowEvent) -> bool {
    if host.session_prompt.is_none() {
        return false;
    }
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            host.pointer_px = Some((position.x, position.y));
            host.cursor_cell = None;
            if let Some(row) = host
                .palette_layout
                .as_ref()
                .and_then(|layout| palette_hit(layout, position.x as usize, position.y as usize))
            {
                if row < button_count(host.session_prompt.as_ref().unwrap()) {
                    host.session_prompt.as_mut().unwrap().button = row;
                }
            }
        }
        WindowEvent::CursorLeft { .. } => host.pointer_px = None,
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } => {
            if let Some((x, y)) = host.pointer_px {
                if let Some(row) = host
                    .palette_layout
                    .as_ref()
                    .and_then(|layout| palette_hit(layout, x as usize, y as usize))
                {
                    if row < button_count(host.session_prompt.as_ref().unwrap()) {
                        activate(host, row);
                        host.suppress_left_release = true;
                    }
                }
            }
        }
        WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
            let prompt = host.session_prompt.as_mut().unwrap();
            for ch in text.chars() {
                apply_rename_stroke(
                    &mut prompt.buffer,
                    &mut prompt.selected,
                    RenameStroke::Insert(ch),
                );
            }
            prompt.error = None;
        }
        WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } | WindowEvent::Ime(_) => {}
        _ => return false,
    }
    host.dirty = true;
    host.window.request_redraw();
    true
}

pub(super) fn paint(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    let Some(prompt) = &host.session_prompt else {
        return;
    };
    let renaming = matches!(prompt.target, Target::Rename { .. });
    let (header, action) = if renaming {
        ("Rename session", "Rename")
    } else {
        ("New session", "Create")
    };
    let subtitle = match &prompt.target {
        Target::Space { space } => format!("Space: {space} · Name its first session"),
        Target::Add { space } => format!("Space: {space} · One mailbox for this pane"),
        Target::Rename { .. } => "Pending mail stays. The old address forwards here.".into(),
        Target::Pane { .. } => "One name for the session and mailbox".into(),
        Target::Layout { count, .. } => format!(
            "Name the next session · {} of {count} panes",
            host.mux.active_pane_count() + 1
        ),
    };
    let hint = prompt
        .error
        .clone()
        .unwrap_or_else(|| format!("Agent ID: {}", prompt.buffer.trim()));
    let mut rows = vec![PaletteRow::plain(action.into(), hint, "Enter".into())];
    if allows_blank(prompt) {
        rows.push(PaletteRow::plain(
            "Blank terminal".into(),
            "Local shell; no session or mailbox".into(),
            String::new(),
        ));
    }
    rows.push(PaletteRow::plain(
        "Cancel".into(),
        "Keep the current arrangement".into(),
        "Esc".into(),
    ));
    let sections = [PaletteSection {
        header,
        subtitle: &subtitle,
        rows: &rows,
    }];
    let frame = PaletteFrame {
        query: Some(&prompt.buffer),
        chips: None,
        sections: &sections,
        selected: prompt.button,
        scroll: 0,
        detail: None,
        footer: "Type to replace · Tab choose · Enter accept · Esc cancel",
    };
    host.palette_layout = paint_palette_overlay(
        &host.font,
        &host.theme,
        focus_border_rgb(host.focus_border),
        &frame,
        host_overlay_surface(host),
        buffer,
        width,
        height,
    );
}

pub(super) fn accessibility(prompt: &Prompt) -> a11y::OverlayKind {
    a11y::OverlayKind::SessionPrompt {
        name: prompt.buffer.clone(),
        renaming: matches!(prompt.target, Target::Rename { .. }),
        allow_blank: allows_blank(prompt),
        selected: prompt.button,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(target_os = "linux")]
    fn real_window_session_naming() {
        crate::render_window_tests::session_naming_in_private_window();
    }
}
