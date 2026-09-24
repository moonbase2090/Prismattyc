//! Session, window, pane, and layout management for Prismattyc (Phase 2 / 2A).
//!
//! Architecture: `docs/architecture.md`; mux domain contract: mux architecture.
//!
//! This crate owns mux topology and the optional long-lived server runtime.
//! Focus and active-tab selection remain **per-client** ([`ClientView`]); the
//! server runtime owns PTYs/emulators without becoming a second compositor.

pub mod attach_focus;
mod attach_scan;
pub mod attach_tabs;
mod attention;
pub mod component_restart;
pub mod config;
mod control;
mod domain;
mod geometry;
pub mod host_register;
pub mod host_render_status;
mod ids;
mod image_paste;
mod inject_submit;
mod layout;
mod layout_file;
mod live;
pub mod mailbox;
mod pane_log;
mod pane_log_persist;
pub mod procinfo;
pub mod release_update;
mod remote_size;
mod rich;
pub mod session_name;
pub mod space_team;
pub mod space_template;
pub mod supervisor;
pub mod team_attention;
pub mod update;
pub mod walkthrough;

pub use attach_scan::{
    attach_targets_session, classify_attach, parse_attach_client, scan_attach_clients,
    AttachClient, AttachKind,
};
pub use control::{
    classify_control_request_id, default_socket_path, diagnose_runtime_dir_miss,
    diagnose_runtime_dir_miss_from_env, live_pmux_sockets_in, next_stale_skip,
    parse_hive_cell_address, probe_socket_liveness, rle_style_runs, systemd_user_runtime_dir,
    ArrangementWire, AxisWire, ColorWire, ControlError, ControlErrorCode, ControlIdMatch,
    ControlPlane, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData,
    ControlServer, CursorShapeWire, Event, EventBatch, EventEnvelope, LayoutSnapshot,
    MailAttentionState, MailInjectDiagnostic, MailInjectOutcome, MailWake, MutationAck,
    OverlayKind, OverlayRun, PaneContent, PaneEvent, PaneGeometry, PaneInputLedger, PaneLogFrame,
    PaneOverlay, PaneSnapshot, PaneStyled, PaneWriteSubmit, RichInputKind, RichPointerPhase,
    RuntimeDirMiss, SessionSnapshot, Snapshot, SocketLiveness, SpawnSpec, StyleRun, WindowBounds,
    WindowSnapshot, WorkspaceInverseRun, WorkspaceStyleRun, CONTROL_STALE_SKIP_MAX,
    MAIL_ATTENTION_CELL, MAIL_INJECT_DIRTY_IDLE_MS, MAIL_INJECT_INPUT_IDLE_MS,
    MAIL_INJECT_NUDGE_DELAY_MS, MAIL_INJECT_PAYLOAD, PMUX_MAIL_NOTIFICATION, PROTOCOL_VERSION,
};
pub use image_paste::{
    clipboard_image_file_with, clipboard_image_to_png, clipboard_image_to_png_with,
    encode_rgba_png, expand_empty_bracketed_paste, expand_empty_paste, image_path_from_uri_list,
    is_empty_bracketed_or_blank, is_empty_bracketed_paste, parse_file_uri_list, paste_dir,
    paste_reference, write_paste_png, write_paste_png_in,
};
pub use inject_submit::{
    classify_cmdline, detect_inject_agent, inject_writes, pid_in_tree, InjectAgent, CURSOR_SUBMIT,
};

pub use config::{
    load_mux_section, merge_mux_section, prism_config_path, render_mux_section,
    resolve_attach_on_new, resolve_mux_target, resolve_remote_size,
    resolve_space_open_runs_commands, write_config_atomic, MuxKeyKind, MuxKeySpec, MuxSection,
    SpaceOpenRunsCommands, MUX_KEYS,
};
pub use domain::{ClientView, Domain, DomainError, Pane, Session, Window};
pub use geometry::{
    apply_arrangement, even_horizontal_row, even_two_row_grid, even_vertical_column,
    layout_to_rects, main_horizontal, main_vertical, subtree_min_cols, subtree_min_rows,
    suggested_focus_after_close, try_subtree_min_cols, try_subtree_min_rows, Arrangement, CellRect,
    GeometryError, DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS,
};
pub use host_register::{
    attach_pty_fallback, host_ack_path_from_socket, host_pane_nested, host_pid_path_from_socket,
    live_host_pid, register_host_pid, route_seat_to_host, should_host_route_seat, touch_host_ack,
    unregister_host_pid, wait_host_ack,
};
pub use ids::{ClientId, PaneId, SessionId, WindowId};
pub use layout::{Axis, PaneLayout, Split};
pub use layout_file::{
    clamp_ratio, from_sessions, from_snapshot, from_snapshot_with, layout_path, layouts_dir,
    list_layouts, list_spaces, load_layout, load_space, new_space_id, plan, remove_layout,
    remove_space, rename_space, save_layout, save_space, space_active_session, space_add_session,
    space_bind_agent, space_remove_session, space_sessions_in_tab_order, spaces_dir,
    stub_space_session, valid_space_id, validate_layout_name, CwdSource, LayoutListEntry, NodeRef,
    SavedLayout, SavedNode, SavedSpace, SavedSpaceSession, SavedSpaceTab, SavedWindow,
    SpaceListEntry, SplitOp, OWNED_SPACE_VERSION, SAVED_LAYOUT_VERSION, SAVED_SPACE_VERSION,
};
pub use live::{PMUX_SOCKET, PRISMATTYC_PANE_ID};
pub use pane_log::PaneFramePolicy;
pub use pane_log_persist::resolve_pane_log_path;
pub use remote_size::{
    disconnect_decision, record_host_chosen, remember_host_size, remote_size_chip, resize_decision,
    ClientRole, RemoteSizePolicy, SizeOwner, SizeOwnerKind, Viewport,
};
pub use update::run_update;

pub mod local_socket;

pub mod platform;
