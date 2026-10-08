//! Complete render guard diagnostics. The legacy reason keeps its priority order.
use super::{App, Duration, Instant, TransientOverlayState};
use std::fmt;

#[derive(Debug, Clone, Copy)]
pub(super) enum Guard {
    Pending,
    Resize,
    AltScreen,
    Scrollback,
    ScrollOverflow,
    Backend,
    Osd,
    BellFlash,
    BackgroundImage,
    LayoutTransition,
    ChromeGeometry,
    PreviousOverlay,
    WorkspaceLayout,
    ExperimentalRich,
    StoredImages,
    FooterVisible,
    Splash,
    Palette,
    ThemePicker,
    SpacePicker,
    ContextMenu,
    FindActive,
    TabRename,
    Walkthrough,
    DragToast,
    ConfigPath,
    ConfigError,
    Preedit,
    BellToasts,
    TitleNotice,
    HoverTarget,
    RestorePrompt,
    SaveSpace,
    Transparency,
}

const GUARDS: &[(Guard, &str)] = &[
    (Guard::Pending, "pending"),
    (Guard::Resize, "resize"),
    (Guard::AltScreen, "alt-screen"),
    (Guard::Scrollback, "scrollback"),
    (Guard::ScrollOverflow, "scroll-overflow"),
    (Guard::Backend, "backend-no-partial"),
    (Guard::Osd, "osd"),
    (Guard::BellFlash, "bell-flash"),
    (Guard::BackgroundImage, "background-image"),
    (Guard::LayoutTransition, "layout-transition"),
    (Guard::ChromeGeometry, "chrome-geometry"),
    (Guard::PreviousOverlay, "previous-overlay"),
    (Guard::WorkspaceLayout, "workspace-layout"),
    (Guard::ExperimentalRich, "experimental-rich"),
    (Guard::StoredImages, "stored-images"),
    (Guard::FooterVisible, "footer-visible"),
    (Guard::Splash, "splash"),
    (Guard::Palette, "palette"),
    (Guard::ThemePicker, "theme-picker"),
    (Guard::SpacePicker, "space-picker"),
    (Guard::ContextMenu, "context-menu"),
    (Guard::FindActive, "find-active"),
    (Guard::TabRename, "tab-rename"),
    (Guard::Walkthrough, "walkthrough"),
    (Guard::DragToast, "drag-toast"),
    (Guard::ConfigPath, "config-path"),
    (Guard::ConfigError, "config-error"),
    (Guard::Preedit, "preedit"),
    (Guard::BellToasts, "bell-toasts"),
    (Guard::TitleNotice, "title-notice"),
    (Guard::HoverTarget, "hover-target"),
    (Guard::RestorePrompt, "restore-prompt"),
    (Guard::SaveSpace, "save-space"),
    (Guard::Transparency, "transparency"),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct GuardMask(u64);

impl GuardMask {
    pub(super) fn set(&mut self, guard: Guard, active: bool) {
        let bit = 1 << guard as u32;
        if active {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    pub(super) fn add(&mut self, guard: Guard, active: bool) {
        if active {
            self.set(guard, true);
        }
    }

    pub(super) fn contains(self, guard: Guard) -> bool {
        self.0 & (1 << guard as u32) != 0
    }

    fn names(self) -> Vec<&'static str> {
        GUARDS
            .iter()
            .filter_map(|(guard, name)| self.contains(*guard).then_some(*name))
            .collect()
    }
}

impl fmt::Display for GuardMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut separator = "";
        for (guard, name) in GUARDS {
            if self.contains(*guard) {
                write!(f, "{separator}{name}")?;
                separator = ",";
            }
        }
        if separator.is_empty() {
            f.write_str("-")?;
        }
        Ok(())
    }
}

pub(super) fn record_overlay_guards(mask: &mut GuardMask, state: TransientOverlayState) {
    mask.set(Guard::FooterVisible, state.footer_visible);
    mask.set(Guard::Splash, state.splash);
    mask.set(Guard::RestorePrompt, state.restore_prompt);
    mask.set(Guard::Palette, state.palette);
    mask.set(Guard::ThemePicker, state.theme_picker);
    mask.set(Guard::SpacePicker, state.space_picker);
    mask.set(Guard::ContextMenu, state.context_menu);
    mask.set(Guard::FindActive, state.find_active);
    mask.set(Guard::TabRename, state.tab_rename);
    mask.set(Guard::Walkthrough, state.walkthrough);
    mask.set(Guard::DragToast, state.drag_toast);
    mask.set(Guard::ConfigPath, state.config_path);
    mask.set(Guard::ConfigError, state.config_error);
    mask.set(Guard::Preedit, state.preedit);
    mask.set(Guard::BellToasts, state.bell_toasts);
    mask.set(Guard::TitleNotice, state.title_notice);
    mask.set(Guard::HoverTarget, state.hover_target);
    mask.set(Guard::SaveSpace, state.save_space);
    mask.set(Guard::Transparency, state.transparency);
}

/// Published `bell-toasts` follows the live chip list, not only the last raster.
pub(super) fn sync_live_bell_toast_guard(guards: &mut GuardMask, live: bool) {
    guards.set(Guard::BellToasts, live);
}

fn bell_toast_guard_is_stale(guards: GuardMask, live: bool) -> bool {
    guards.contains(Guard::BellToasts) != live
}

fn should_publish_render_status(
    last: Option<Instant>,
    now: Instant,
    toast_guard_stale: bool,
) -> bool {
    toast_guard_stale || render_status_due(last, now)
}

fn present_timing_json(present: Option<crate::present_timing::PresentTiming>) -> serde_json::Value {
    let Some(present) = present else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "backend": present.backend,
        "write_us": present.write_us,
        "commit_us": present.commit_us,
        "dirty_tiles": present.dirty_tiles,
        "changed_tiles": present.changed_tiles,
        "write_bytes": present.write_bytes,
        "busy_surface_stalls": present.busy_surface_stalls,
    })
}

impl App {
    pub(super) fn publish_render_status(&mut self) {
        let Some((pid_path, pid)) = self.registered_host.as_ref() else {
            return;
        };
        let now = Instant::now();
        // Linger ends in drain_pty. pump() then publishes before paint, so
        // last_raster.guards can still name `bell-toasts` after the chips
        // are gone. Emit as soon as the bit disagrees with the live list.
        let toast_guard_stale = self.windows.values().any(|host| {
            bell_toast_guard_is_stale(host.render_frame.guards, !host.bell_toasts.is_empty())
        });
        if !should_publish_render_status(self.last_render_status, now, toast_guard_stale) {
            return;
        }
        self.last_render_status = Some(now);
        self.render_status_seq = self.render_status_seq.saturating_add(1);
        for host in self.windows.values_mut() {
            sync_live_bell_toast_guard(&mut host.render_frame.guards, !host.bell_toasts.is_empty());
        }
        let mut attach_stream_count = 0usize;
        let mut attach_queue_budget_bytes = 0usize;
        let mut attach_queue_bytes = 0usize;
        let mut attach_queue_high_water_bytes = 0usize;
        let mut attach_reader_blocked_ms = 0u64;
        let pump = self.pump_timing.json();
        let windows: Vec<_> = self.windows.iter().map(|(id, host)| {
            let pane_ids = host
                .mux
                .tab_panes()
                .into_iter()
                .flat_map(|(_, panes)| panes)
                .collect::<Vec<_>>();
            let panes: Vec<_> = pane_ids.into_iter().filter_map(|id| {
                host.mux.pane(id).map(|pane| {
                    let (batches, frames, superseded_frames, superseded_bytes) =
                        pane.attach_policy_stats();
                    let queue_stats = pane.attach_queue_stats();
                    let queue = queue_stats.unwrap_or_default();
                    if queue_stats.is_some() {
                        attach_stream_count = attach_stream_count.saturating_add(1);
                        attach_queue_budget_bytes =
                            attach_queue_budget_bytes.saturating_add(queue.budget_bytes);
                        attach_queue_bytes = attach_queue_bytes.saturating_add(queue.queued_bytes);
                        attach_queue_high_water_bytes =
                            attach_queue_high_water_bytes.saturating_add(queue.high_water_bytes);
                        attach_reader_blocked_ms =
                            attach_reader_blocked_ms.saturating_add(queue.reader_blocked_ms);
                    }
                    serde_json::json!({
                        "pane_id": id.to_string(),
                        "remote_pane_id": host.mux.remote_pane_id(id),
                        "local_child_pid": pane.child_pid(),
                        "alt_active": pane.emulator.screen().alt_active(),
                        "view_scroll": pane.view_scroll,
                        "stored_images": pane.emulator.images().len(),
                        "workspace_layout": pane.workspace_layout().is_some(),
                        "experimental_rich": pane.experimental_rich(),
                        "attach_policy": {
                            "batches": batches,
                            "frames": frames,
                            "superseded_frames": superseded_frames,
                            "superseded_bytes": superseded_bytes,
                            "coalesced_frames": superseded_frames,
                            "coalesced_bytes": superseded_bytes,
                            "dropped_frames": 0,
                            "dropped_bytes": 0,
                            "reader_queue_budget_bytes": queue.budget_bytes,
                            "reader_queue_bytes": queue.queued_bytes,
                            "reader_queue_high_water_bytes": queue.high_water_bytes,
                            "reader_blocked_ms": queue.reader_blocked_ms,
                            "notice": pane.attach_policy_notice(),
                        },
                    })
                })
            }).collect();
            let size = host.window.inner_size();
            let chips: Vec<_> = host.space_rail.layout(host.mux.geom(), size.width as usize, size.height as usize, host.spacing.space_rail_pane_names)
                .into_iter().flat_map(|layout| (0..=host.space_rail.names.len()).filter_map(move |index| {
                    layout.chip_bounds(index, host.space_rail.names.len()).map(|(x,y,w,h)| serde_json::json!({
                        "name": host.space_rail.names.get(index), "x":x,"y":y,"width":w,"height":h
                    }))
                })).collect();
            let sidebar_spaces: Vec<_> = host.sidebar_rows.iter().filter_map(|(slot, row)| {
                if row.kind != crate::sidebar::RowKind::Space {
                    return None;
                }
                let name = &host.sidebar_tree.spaces.get(row.space)?.name;
                Some(serde_json::json!({
                    "name": name, "x": slot.x, "y": slot.y,
                    "width": slot.w, "height": slot.h
                }))
            }).collect();
            let frame = host.render_frame;
            let last_raster = serde_json::json!({
                "unix_ms": frame.raster_at_unix_ms,
                "present_succeeded": frame.present_succeeded,
                "raster_mode": if frame.full_repaint_reason.is_some() { "full" } else { "partial" },
                "full_repaint_reason": frame.full_repaint_reason.map(super::FullRepaintReason::as_str),
                "guard_mask": frame.guards.0,
                "guards": frame.guards.names(),
                "cells_painted": frame.cells_painted,
                "rows_scrolled_as_blit": frame.rows_scrolled_as_blit,
                "timing": {
                    "parse_us": frame.timing.last_parse_us,
                    "damage_us": frame.timing.damage_us,
                    "raster_us": frame.timing.raster_us,
                    "present_us": frame.timing.present_us,
                },
                "present": present_timing_json(frame.present),
            });
            let mut status = serde_json::json!({
                "window_id": format!("{id:?}"),
                "focused": host.window_focused,
                "occluded": host.window_occluded,
                "pane_count": host.mux.pane_count(),
                "tab_count": host.mux.tab_count(),
                "tab_layouts": host.mux.tab_layouts(),
                "tab_git_labels": host.mux.tab_git_labels(),
                "terminal_switcher": host.terminal_targets.as_ref().map(|entries| entries.iter().map(|entry| &entry.label).collect::<Vec<_>>()),
                "space_rail_width_cols": host.spacing.space_rail_width_cols,
                "space_rail_px": host.mux.geom().rail_px,
                "focused_pane": host.mux.focused_id().get(),
                "visible_pane_slots": host.mux.panes_and_rects().map(|(id, _, rect)| (id.get(), host.mux.geom().pane_slot_px(rect))).collect::<Vec<_>>(),
                "session_prompt_open": host.session_prompt.is_some(),
                "local_terminal_panes": host.mux.tab_panes().iter().flat_map(|(_, panes)| panes.iter()).filter(|pane| host.mux.is_retained_local_terminal(**pane)).map(|pane| pane.get()).collect::<Vec<_>>(),
                "space": host.space_rail.current,
                "space_save_status": host.space_rail.save_status,
                "space_rail_position": host.spacing.space_rail.as_str(),
                "space_undo_available": host.space_polish.undo.is_some(),
                "space_picker_open": host.space_picker.is_some(),
                "space_open_pending": host.space_opens.busy(),
                "last_space_open": host.last_space_open,
                "space_chips": chips,
                "space_order": host.space_rail.names,
                "tab_strip_y": host.mux.geom().tab_strip_y(),
                "tab_strip_height": host.mux.geom().top_chrome_px,
                "space_details": crate::space_panel::status(host),
                "context_menu": host.context_menu.as_ref().map(|menu| serde_json::json!({
                    "selected": menu.selected,
                    "confirmation": menu.confirm,
                    "target": host.context_menu_target.map(|target| match target {
                        super::ContextMenuTarget::SpaceChip(_) => "space",
                        super::ContextMenuTarget::Pane(_) => "pane",
                        super::ContextMenuTarget::RailSpace(_) => "rail-space",
                        super::ContextMenuTarget::RailSession(_) => "rail-session",
                        super::ContextMenuTarget::RailSessionSolo(_) => "rail-session-solo",
                        super::ContextMenuTarget::RailPane(_) => "rail-pane",
                    }),
                })),
                "space_session_names": host.space_rail.live_pane_names,
                "space_attention_counts": host.space_rail.attention_counts,
                "selected_tab": host.mux.selected_tab_index(),
                "focused_session": host.attach_pane_sessions.get(&host.mux.focused_id()),
                "current_panes": panes,
            });
            if let Some(status) = status.as_object_mut() {
                status.insert("pump".into(), pump.clone());
                status.insert("last_raster".into(), last_raster);
                status.insert(
                    "sidebar_space_rows".into(),
                    serde_json::Value::Array(sidebar_spaces),
                );
                status.insert(
                    "space_reorder_drag_active".into(),
                    serde_json::Value::Bool(
                        host.space_reorder_drag
                            .as_ref()
                            .is_some_and(|drag| drag.active),
                    ),
                );
            }
            status
        }).collect();
        let status = serde_json::json!({
            "schema_version": 1,
            "host_pid": pid,
            "host_version": env!("CARGO_PKG_VERSION"),
            "host_git_hash": prismattyc_core::git_hash(),
            "status_seq": self.render_status_seq,
            "sampled_at_unix_ms": prismattyc_mux::host_render_status::unix_ms(),
            "attach_queue": {
                "stream_count": attach_stream_count,
                "reader_queue_budget_bytes": attach_queue_budget_bytes,
                "reader_queue_bytes": attach_queue_bytes,
                "reader_queue_high_water_bytes": attach_queue_high_water_bytes,
                "reader_blocked_ms": attach_reader_blocked_ms,
            },
            "windows": windows,
        });
        if let Err(error) = self
            .file_writer
            .handle()
            .render_status(pid_path.clone(), *pid, status)
        {
            eprintln!("prismattyc-host: could not publish render status: {error}");
        }
    }
}

fn render_status_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_reports_simultaneous_guards_and_clears_independently() {
        let mut mask = GuardMask::default();
        assert_eq!(mask.to_string(), "-");
        mask.set(Guard::AltScreen, true);
        mask.set(Guard::Backend, true);
        mask.set(Guard::LayoutTransition, true);
        mask.set(Guard::ChromeGeometry, true);
        mask.set(Guard::AltScreen, true);
        assert!(mask.contains(Guard::AltScreen));
        mask.add(Guard::Scrollback, true);
        assert!(mask.contains(Guard::Scrollback));
        mask.set(Guard::Scrollback, false);
        mask.add(Guard::Backend, false);
        assert_eq!(
            mask.to_string(),
            "alt-screen,backend-no-partial,layout-transition,chrome-geometry"
        );
        mask.set(Guard::AltScreen, false);
        assert_eq!(
            mask.names(),
            ["backend-no-partial", "layout-transition", "chrome-geometry"]
        );
    }

    #[test]
    fn render_status_throttle_has_strict_one_second_boundary() {
        let now = Instant::now();
        assert!(render_status_due(None, now));
        assert!(!render_status_due(
            Some(now - Duration::from_millis(999)),
            now
        ));
        assert!(render_status_due(Some(now - Duration::from_secs(1)), now));
        assert!(render_status_due(
            Some(now - Duration::from_millis(1001)),
            now
        ));
    }

    #[test]
    fn osd_guard_has_a_named_entry() {
        let mut mask = GuardMask::default();
        mask.set(Guard::Osd, true);
        assert_eq!(mask.to_string(), "osd");
    }

    #[test]
    fn guards_table_covers_every_variant() {
        const ALL: &[Guard] = &[
            Guard::Pending,
            Guard::Resize,
            Guard::AltScreen,
            Guard::Scrollback,
            Guard::ScrollOverflow,
            Guard::Backend,
            Guard::Osd,
            Guard::BellFlash,
            Guard::BackgroundImage,
            Guard::LayoutTransition,
            Guard::ChromeGeometry,
            Guard::PreviousOverlay,
            Guard::WorkspaceLayout,
            Guard::ExperimentalRich,
            Guard::StoredImages,
            Guard::FooterVisible,
            Guard::Splash,
            Guard::Palette,
            Guard::ThemePicker,
            Guard::SpacePicker,
            Guard::ContextMenu,
            Guard::FindActive,
            Guard::TabRename,
            Guard::Walkthrough,
            Guard::DragToast,
            Guard::ConfigPath,
            Guard::ConfigError,
            Guard::Preedit,
            Guard::BellToasts,
            Guard::TitleNotice,
            Guard::HoverTarget,
            Guard::RestorePrompt,
            Guard::SaveSpace,
            Guard::Transparency,
        ];
        assert_eq!(GUARDS.len(), ALL.len());
        for guard in ALL {
            assert!(GUARDS
                .iter()
                .any(|(listed, _)| *listed as u32 == *guard as u32));
        }
    }

    #[test]
    fn bell_flash_guard_has_a_named_entry() {
        let mut mask = GuardMask::default();
        mask.set(Guard::BellFlash, true);
        assert_eq!(mask.to_string(), "bell-flash");
    }
    #[test]
    fn each_overlay_has_an_independent_named_guard() {
        let cases = [
            (
                TransientOverlayState {
                    footer_visible: true,
                    ..TransientOverlayState::default()
                },
                "footer-visible",
            ),
            (
                TransientOverlayState {
                    restore_prompt: true,
                    ..TransientOverlayState::default()
                },
                "restore-prompt",
            ),
            (
                TransientOverlayState {
                    splash: true,
                    ..TransientOverlayState::default()
                },
                "splash",
            ),
            (
                TransientOverlayState {
                    palette: true,
                    ..TransientOverlayState::default()
                },
                "palette",
            ),
            (
                TransientOverlayState {
                    theme_picker: true,
                    ..TransientOverlayState::default()
                },
                "theme-picker",
            ),
            (
                TransientOverlayState {
                    space_picker: true,
                    ..TransientOverlayState::default()
                },
                "space-picker",
            ),
            (
                TransientOverlayState {
                    context_menu: true,
                    ..TransientOverlayState::default()
                },
                "context-menu",
            ),
            (
                TransientOverlayState {
                    find_active: true,
                    ..TransientOverlayState::default()
                },
                "find-active",
            ),
            (
                TransientOverlayState {
                    tab_rename: true,
                    ..TransientOverlayState::default()
                },
                "tab-rename",
            ),
            (
                TransientOverlayState {
                    walkthrough: true,
                    ..TransientOverlayState::default()
                },
                "walkthrough",
            ),
            (
                TransientOverlayState {
                    drag_toast: true,
                    ..TransientOverlayState::default()
                },
                "drag-toast",
            ),
            (
                TransientOverlayState {
                    config_path: true,
                    ..TransientOverlayState::default()
                },
                "config-path",
            ),
            (
                TransientOverlayState {
                    config_error: true,
                    ..TransientOverlayState::default()
                },
                "config-error",
            ),
            (
                TransientOverlayState {
                    preedit: true,
                    ..TransientOverlayState::default()
                },
                "preedit",
            ),
            (
                TransientOverlayState {
                    bell_toasts: true,
                    ..TransientOverlayState::default()
                },
                "bell-toasts",
            ),
            (
                TransientOverlayState {
                    title_notice: true,
                    ..TransientOverlayState::default()
                },
                "title-notice",
            ),
            (
                TransientOverlayState {
                    hover_target: true,
                    ..TransientOverlayState::default()
                },
                "hover-target",
            ),
            (
                TransientOverlayState {
                    save_space: true,
                    ..TransientOverlayState::default()
                },
                "save-space",
            ),
            (
                TransientOverlayState {
                    transparency: true,
                    ..TransientOverlayState::default()
                },
                "transparency",
            ),
        ];
        for (state, name) in cases {
            let mut mask = GuardMask::default();
            record_overlay_guards(&mut mask, state);
            assert_eq!(mask.names(), [name]);
            record_overlay_guards(&mut mask, TransientOverlayState::default());
            assert_eq!(mask.to_string(), "-");
        }
    }

    #[test]
    fn published_bell_toasts_guard_follows_the_live_chip_list() {
        let mut mask = GuardMask::default();
        mask.set(Guard::BellToasts, true);
        mask.set(Guard::Palette, true);
        assert!(bell_toast_guard_is_stale(mask, false));
        assert!(!bell_toast_guard_is_stale(mask, true));
        sync_live_bell_toast_guard(&mut mask, false);
        assert_eq!(mask.names(), ["palette"]);
        assert!(!bell_toast_guard_is_stale(mask, false));
        sync_live_bell_toast_guard(&mut mask, true);
        assert_eq!(mask.names(), ["palette", "bell-toasts"]);
        sync_live_bell_toast_guard(&mut mask, false);
        assert!(
            !mask.contains(Guard::BellToasts),
            "an expired chip must leave last_raster.guards before the next raster"
        );
    }

    #[test]
    fn toast_guard_mismatch_publishes_before_the_one_second_throttle() {
        let now = Instant::now();
        let last = Some(now);
        assert!(!should_publish_render_status(last, now, false));
        assert!(
            should_publish_render_status(last, now, true),
            "a stale bell-toasts bit must not wait for the 1s heartbeat"
        );
        assert!(should_publish_render_status(None, now, false));
        assert!(should_publish_render_status(
            last,
            now + Duration::from_secs(1),
            false
        ));
    }

    #[test]
    fn present_status_schema_reports_write_commit_and_tile_counts() {
        let status = present_timing_json(Some(crate::present_timing::PresentTiming {
            backend: "tiles",
            write_us: 17,
            commit_us: 23,
            dirty_tiles: 11,
            changed_tiles: Some(4),
            write_bytes: 65_536,
            busy_surface_stalls: 0,
        }));
        assert_eq!(status["backend"], "tiles");
        assert_eq!(status["write_us"], 17);
        assert_eq!(status["commit_us"], 23);
        assert_eq!(status["dirty_tiles"], 11);
        assert_eq!(status["changed_tiles"], 4);
        assert_eq!(status["write_bytes"], 65_536);
        assert_eq!(status["busy_surface_stalls"], 0);
        assert!(present_timing_json(None).is_null());
    }
}
