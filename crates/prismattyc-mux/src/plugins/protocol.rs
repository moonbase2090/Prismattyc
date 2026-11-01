// SPDX-License-Identifier: MIT

use pmux_plugin_api::host::pmux::view::frame;
use serde::{Deserialize, Serialize};

use super::{CatalogEntry, Installed, Manifest};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Discover {
        #[serde(default)]
        refresh: bool,
    },
    List,
    Inspect {
        id: String,
    },
    Admin {
        retry_id: String,
        action: AdminAction,
    },
    Invoke {
        id: String,
        contribution: String,
        arguments: String,
    },
    OpenView {
        id: String,
        contribution: String,
        viewport: frame::Viewport,
    },
    ViewEvent {
        handle: String,
        event: frame::Event,
    },
    CloseView {
        handle: String,
    },
    Exports {
        allowlist: Vec<String>,
    },
    CallExport {
        handle: String,
        arguments: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminAction {
    Install {
        id: String,
        version: Option<String>,
    },
    Enable {
        package: Installed,
        grants: Vec<String>,
    },
    Disable {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Catalog {
        packages: Vec<CatalogEntry>,
    },
    List {
        installed: Vec<Installed>,
        states: Vec<Status>,
    },
    Inspection {
        state: Status,
        manifest: Option<Manifest>,
    },
    Installed {
        package: Installed,
    },
    State {
        state: Status,
    },
    Tool {
        json: String,
        is_error: bool,
    },
    ViewOpened {
        handle: String,
    },
    /// Base64 of the compact typed frame, not terminal escape sequences.
    Frame {
        handle: String,
        content: String,
    },
    Idle,
    Exports {
        tools: Vec<ToolExport>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub selected: Option<Installed>,
    pub enabled: bool,
    pub grants: Vec<String>,
    pub observed: Observed,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observed {
    Stopped,
    Starting,
    Running,
    Failed,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExport {
    pub handle: String,
    pub plugin: String,
    pub contribution: String,
    pub name: String,
    pub input_schema: String,
    pub output_schema: String,
}
