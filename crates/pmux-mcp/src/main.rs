//! `pmux-mcp` — stdio MCP adapter over the pmux Mail* socket.
//!
//! Each tool call is one handshake-then-drop connection: RegisterClient,
//! MailHello, one mailbox op, one reply. JSON-RPC lives only on this
//! process's stdio toward the MCP host.
//!
//! Tool names are `pmux_*` (mail and Spaces tools). Identity: `--as <agent>`,
//! else `$PMUX_AGENT`. No other default.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use prismattyc_mux::mailbox::AgentId;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;

mod activity;
mod mail;
mod supervisor;

use mail::{format_data, mux_socket, MailOp};

const USAGE: &str = "pmux-mcp — MCP stdio adapter over the pmux Mail* socket

usage: pmux-mcp [-V|--version] [-h|--help] [--as <agent>] [--supervise]
                [--activity-hosts FILE]
       pmux-mcp --activity-only [--activity-hosts FILE] [--supervise]

identity: --as <agent>, else $PMUX_AGENT. No default.
--activity-only serves only the read-only pmux_activity_* tools and needs no
identity: no mail, Space or pane tools are offered.
--activity-hosts FILE lists the hosts the activity tools may read (TOML; see
docs/activity-connector.md). Default: this machine's socket, labeled local.
Each tool call reconnects: MailHello, one op, drop. Watch is not a tool here.

-V, --version  print version and exit
-h, --help     print this help and exit
--supervise keeps the host-facing stdio transport open and restarts an
inner adapter if it exits. Configure MCP hosts to launch this mode when
they do not respawn failed stdio servers themselves.";

struct Config {
    /// `None` only with `--activity-only`.
    agent: Option<AgentId>,
    supervise: bool,
    activity_hosts: Option<PathBuf>,
    /// Arguments that recreate this configuration in a supervised child.
    child_args: Vec<String>,
}

/// MCP stdio server. One agent identity for the process lifetime, or none in
/// activity-only mode, which serves only the read-only activity tools.
#[derive(Clone)]
struct PmuxMcp {
    agent: Option<AgentId>,
    socket: PathBuf,
    activity: std::sync::Arc<activity::Hosts>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// Recipient: agent id (`operator-id`) or alias. Never a seat (`0@1`) or `cell:` prefix.
    to: String,
    /// One-line summary.
    summary: String,
    /// Letter body. Empty string if omitted.
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IdsArgs {
    /// Letter ids from a previous `pmux_claim`.
    ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AliasArgs {
    /// Shorthand bound to this process's agent.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BroadcastArgs {
    /// One-line summary.
    summary: String,
    /// Letter body. Empty string if omitted.
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpaceName {
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpaceRoleArgs {
    name: String,
    session: String,
    role: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpaceLinkArgs {
    name: String,
    label: String,
    target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpaceAttentionArgs {
    name: String,
    session: String,
    pane: u64,
    revision: u64,
    /// "resolve" or "snooze" (10 minutes). Does not claim or commit mail.
    action: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TemplateArgs {
    template: String,
    space: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TemplateCreateArgs {
    template: String,
    space: String,
    /// Explicitly execute the commands shown in the preview. Defaults to shells only.
    #[serde(default)]
    launch: bool,
}

/// The `pmux` next to this binary, else the one on PATH.
fn pmux_program() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .map(|p| p.join(prismattyc_mux::platform::executable_name("pmux")))
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("pmux"))
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct ActivityHostArgs {
    /// Host label from pmux_activity_hosts. Optional when one host is configured.
    #[serde(default)]
    host: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActivityRecentArgs {
    /// Seat (agent id) whose retained events to return.
    seat: String,
    /// Keep only the newest N events (1-256). Default: all retained.
    #[serde(default)]
    limit: Option<u16>,
    /// Host label from pmux_activity_hosts. Optional when one host is configured.
    #[serde(default)]
    host: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActivityPollArgs {
    /// `next_cursor` from the previous poll on the same host (EPOCH:SEQ).
    /// Omit to start at the oldest retained event.
    #[serde(default)]
    cursor: Option<String>,
    /// Return only this seat's events. The cursor still advances past others.
    #[serde(default)]
    seat: Option<String>,
    /// Page size, 1-256. Default 256.
    #[serde(default)]
    limit: Option<u16>,
    /// Host label from pmux_activity_hosts. Optional when one host is configured.
    #[serde(default)]
    host: Option<String>,
}

fn activity_result(value: &serde_json::Value, ok: bool) -> CallToolResult {
    let text = vec![ContentBlock::text(value.to_string())];
    if ok {
        CallToolResult::success(text)
    } else {
        CallToolResult::error(text)
    }
}

/// Check tool-supplied values here, before they reach a command line. A
/// command endpoint hands its arguments to a remote shell, so only seats,
/// cursors and limits in their validated forms may pass.
fn activity_args(
    verb: &str,
    seat: Option<String>,
    cursor: Option<String>,
    limit: Option<u16>,
) -> Result<Vec<String>, activity::ActivityError> {
    let invalid = |message: String| activity::ActivityError {
        code: "invalid_argument",
        message,
        host: None,
        retry_after_ms: None,
    };
    let mut argv = vec![verb.to_owned()];
    if let Some(seat) = seat {
        let seat = AgentId::new(seat).map_err(|e| invalid(format!("seat: {e}")))?;
        if verb == "recent" {
            argv.push(seat.to_string());
        } else {
            argv.extend(["--seat".into(), seat.to_string()]);
        }
    }
    if let Some(cursor) = cursor {
        let cursor = prismattyc_mux::activity_query::parse_cursor(&cursor)
            .map_err(|e| invalid(e.to_string()))?;
        argv.extend([
            "--cursor".into(),
            prismattyc_mux::activity_query::format_cursor(&cursor),
        ]);
    }
    if let Some(limit) = limit {
        if !(1..=256).contains(&limit) {
            return Err(invalid("limit must be 1 to 256".into()));
        }
        argv.extend(["--limit".into(), limit.to_string()]);
    }
    Ok(argv)
}

#[tool_router(router = activity_router)]
impl PmuxMcp {
    /// Validate, pick the host, apply its rate limit, and run the query.
    async fn activity_query(
        &self,
        host: Option<String>,
        args: Result<Vec<String>, activity::ActivityError>,
    ) -> Result<CallToolResult, McpError> {
        let args = match args {
            Ok(args) => args,
            Err(error) => return Ok(activity_result(&error.to_json(), false)),
        };
        let selected = match self.activity.select(host.as_deref()) {
            Ok(selected) => selected,
            Err(error) => return Ok(activity_result(&error.to_json(), false)),
        };
        if let Err(error) = self
            .activity
            .admit(&selected.label, std::time::Instant::now())
        {
            return Ok(activity_result(&error.to_json(), false));
        }
        match activity::run(&pmux_program(), selected, &args).await {
            Ok((ok, text)) => {
                let content = vec![ContentBlock::text(text)];
                Ok(if ok {
                    CallToolResult::success(content)
                } else {
                    CallToolResult::error(content)
                })
            }
            Err(error) => Ok(activity_result(&error.to_json(), false)),
        }
    }

    #[tool(
        description = "List the hosts this server can read activity from: each label and whether it is local or reached by a command. Pass a label as host to the other pmux_activity_* tools."
    )]
    async fn pmux_activity_hosts(&self) -> Result<CallToolResult, McpError> {
        Ok(activity_result(&self.activity.summary(), true))
    }

    #[tool(
        description = "List seats seen in a host's retained activity feed with each seat's latest event kind and time. Seeing a seat does not mean its process is alive. Read-only; the host's pmuxd must run with --experimental-activity."
    )]
    async fn pmux_activity_seats(
        &self,
        Parameters(args): Parameters<ActivityHostArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.activity_query(args.host, activity_args("seats", None, None, None))
            .await
    }

    #[tool(
        description = "Return one seat's retained activity events on a host (metadata only: kind, source, time), oldest first. Read-only."
    )]
    async fn pmux_activity_recent(
        &self,
        Parameters(args): Parameters<ActivityRecentArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.activity_query(
            args.host,
            activity_args("recent", Some(args.seat), None, args.limit),
        )
        .await
    }

    #[tool(
        description = "Read one bounded page of a host's activity after a cursor and return it with next_cursor. Call again with next_cursor to follow the feed; keep one cursor per host. A non-null page.reset means continuity was lost: discard cached seat state. Read-only."
    )]
    async fn pmux_activity_poll(
        &self,
        Parameters(args): Parameters<ActivityPollArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.activity_query(
            args.host,
            activity_args("poll", args.seat, args.cursor, args.limit),
        )
        .await
    }
}

#[tool_router]
impl PmuxMcp {
    async fn space_command(&self, args: Vec<String>) -> Result<CallToolResult, McpError> {
        let mut command = tokio::process::Command::new(pmux_program());
        command
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await {
            Ok(Ok(output)) if output.status.success() => Ok(CallToolResult::success(vec![ContentBlock::text(String::from_utf8_lossy(&output.stdout))])),
            Ok(Ok(output)) => Ok(CallToolResult::error(vec![ContentBlock::text(String::from_utf8_lossy(&output.stderr))])),
            Ok(Err(error)) => Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())])),
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text("Space operation timed out. Inspect its sessions before retrying; work may have started.")])),
        }
    }

    #[tool(
        description = "Inspect a Space: session names, roles, live state, explicit attention requests, separate letter counts, and context links. Does not launch work or claim mail."
    )]
    async fn pmux_space_details(
        &self,
        Parameters(args): Parameters<SpaceName>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "details".into(),
            args.name,
            "--json".into(),
        ])
        .await
    }

    #[tool(
        description = "Read retained Space open results. These are historical operation results; use pmux_space_details for current liveness."
    )]
    async fn pmux_space_result(
        &self,
        Parameters(args): Parameters<SpaceName>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "result".into(),
            args.name,
            "--json".into(),
        ])
        .await
    }

    #[tool(description = "Set the displayed role of a session in its owning Space.")]
    async fn pmux_space_role(
        &self,
        Parameters(args): Parameters<SpaceRoleArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "role".into(),
            args.name,
            args.session,
            args.role,
        ])
        .await
    }

    #[tool(
        description = "Add a labeled context link to a Space. The target must be an HTTP(S) URL or absolute local path. Does not open it or modify external task state."
    )]
    async fn pmux_space_link(
        &self,
        Parameters(args): Parameters<SpaceLinkArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "link".into(),
            args.name,
            args.label,
            args.target,
        ])
        .await
    }

    #[tool(
        description = "Resolve or snooze an exact attention request returned by Space details. Rechecks ownership and request revision. Leaves mail delivery unchanged."
    )]
    async fn pmux_space_attention(
        &self,
        Parameters(args): Parameters<SpaceAttentionArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !matches!(args.action.as_str(), "resolve" | "snooze") {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "action must be resolve or snooze",
            )]));
        }
        self.space_command(vec![
            "space".into(),
            "attention".into(),
            args.action,
            args.name,
            args.session,
            args.pane.to_string(),
            args.revision.to_string(),
        ])
        .await
    }

    #[tool(
        description = "Save a reusable team template from a Space definition. Does not create or stop sessions and does not execute commands."
    )]
    async fn pmux_template_save(
        &self,
        Parameters(args): Parameters<TemplateArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "template".into(),
            "save".into(),
            args.template,
            "--space".into(),
            args.space,
        ])
        .await
    }

    #[tool(description = "List saved team templates without launching work.")]
    async fn pmux_template_list(&self) -> Result<CallToolResult, McpError> {
        self.space_command(vec!["space".into(), "template".into(), "ls".into()])
            .await
    }

    #[tool(
        description = "Preview the sessions, directories, programs, commands, and conflicts for a new team. Has no process or saved-Space side effects."
    )]
    async fn pmux_template_preview(
        &self,
        Parameters(args): Parameters<TemplateArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.space_command(vec![
            "space".into(),
            "template".into(),
            "preview".into(),
            args.template,
            args.space,
            "--json".into(),
        ])
        .await
    }

    #[tool(
        description = "Create an independent Space from a previewed template. Requires explicit user intent. launch=false creates shells only; launch=true executes the previewed commands. Retry never blindly repeats a launch. Does not open a desktop view."
    )]
    async fn pmux_template_create(
        &self,
        Parameters(args): Parameters<TemplateCreateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut command = vec![
            "space".into(),
            "template".into(),
            "create".into(),
            args.template,
            args.space,
            "--prepare-only".into(),
        ];
        if args.launch {
            command.push("--launch".into());
        }
        self.space_command(command).await
    }
    /// Full server: mail, Space and activity tools for `agent`.
    fn new(agent: AgentId, socket: PathBuf, activity: activity::Hosts) -> Self {
        let mut tool_router = Self::tool_router();
        tool_router.merge(Self::activity_router());
        Self {
            agent: Some(agent),
            socket,
            activity: std::sync::Arc::new(activity),
            tool_router,
        }
    }

    /// Read-only server: only the activity tools, no agent identity.
    fn activity_only(socket: PathBuf, activity: activity::Hosts) -> Self {
        Self {
            agent: None,
            socket,
            activity: std::sync::Arc::new(activity),
            tool_router: Self::activity_router(),
        }
    }

    fn agent(&self) -> Result<&AgentId, McpError> {
        self.agent
            .as_ref()
            .ok_or_else(|| McpError::invalid_request("this server runs with --activity-only", None))
    }

    async fn run_op(&self, op: MailOp) -> Result<CallToolResult, McpError> {
        let agent = self.agent()?.clone();
        let socket = self.socket.clone();
        match tokio::task::spawn_blocking(move || mail::exchange(&socket, &agent, op)).await {
            Ok(Ok(data)) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_data(&data),
            )])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
            Err(e) => Err(McpError::internal_error(format!("join: {e}"), None)),
        }
    }

    #[tool(
        description = "Get the pmux collaboration tutorial: identity, live presence, stored mail, intentional pane writes, authorized shell commands, and Space cleanup."
    )]
    async fn pmux_tutorial(&self) -> Result<CallToolResult, McpError> {
        let who_result = self.run_op(MailOp::Who).await;
        let presence = match &who_result {
            Ok(result) => result
                .content
                .first()
                .and_then(|c| match c {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "(could not reach daemon)".to_string()),
            Err(_) => "(could not reach daemon)".to_string(),
        };

        let inbox_result = self.run_op(MailOp::Inbox).await;
        let inbox = match &inbox_result {
            Ok(result) => result
                .content
                .first()
                .and_then(|c| match c {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            Err(_) => String::new(),
        };

        let tutorial = render_tutorial(self.agent()?, presence.trim(), inbox.trim());

        Ok(CallToolResult::success(vec![ContentBlock::text(tutorial)]))
    }

    #[tool(
        description = "Deliver a letter. to is an agent id (operator-id) or alias. Never a seat (0@1) or cell: prefix. Omitting body sends an empty body."
    )]
    async fn pmux_send(
        &self,
        Parameters(args): Parameters<SendArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Send {
            to: args.to,
            summary: args.summary,
            body: args.body,
        })
        .await
    }

    #[tool(
        description = "Claim this agent's uncommitted letters. Open letters become held; already-held letters are re-listed. Commit or release next."
    )]
    async fn pmux_claim(&self) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Claim).await
    }

    #[tool(description = "Commit held letters by id. They are gone for good.")]
    async fn pmux_commit(
        &self,
        Parameters(args): Parameters<IdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.ids.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "commit needs at least one letter id",
            )]));
        }
        self.run_op(MailOp::Commit { ids: args.ids }).await
    }

    #[tool(description = "Return held letters to open by id, making them claimable again.")]
    async fn pmux_release(
        &self,
        Parameters(args): Parameters<IdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.ids.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "release needs at least one letter id",
            )]));
        }
        self.run_op(MailOp::Release { ids: args.ids }).await
    }

    #[tool(description = "Peek at mailbox depth without claiming. Returns open and held counts.")]
    async fn pmux_inbox(&self) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Inbox).await
    }

    #[tool(description = "Bind a shorthand alias to this process's agent.")]
    async fn pmux_alias(
        &self,
        Parameters(args): Parameters<AliasArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Alias { name: args.name }).await
    }

    #[tool(description = "List live agent-bound sessions (presence), including this process.")]
    async fn pmux_who(&self) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Who).await
    }

    #[tool(
        description = "Send one letter to every bound agent except this agent. Single transaction."
    )]
    async fn pmux_broadcast(
        &self,
        Parameters(args): Parameters<BroadcastArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_op(MailOp::Broadcast {
            summary: args.summary,
            body: args.body,
        })
        .await
    }
}

fn render_tutorial(agent: &AgentId, presence: &str, inbox: &str) -> String {
    format!(
        "# PMUX collaboration tutorial\n\n## Your identity\n\nThe adapter is configured as **{agent}**. \
         Use `pmux whoami --json` to verify your live pane binding.\n\n\
         ## Who is online now\n\n{presence}\n\n## Your mailbox\n\n{inbox}\n\n{}",
        include_str!("../tutorial.md")
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PmuxMcp {
    fn get_info(&self) -> ServerInfo {
        let info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        let Some(agent) = &self.agent else {
            return info.with_instructions(
                "Read-only pmux activity feed. Call pmux_activity_hosts for host labels, \
                 pmux_activity_seats for an overview, then pmux_activity_poll with each \
                 host's next_cursor to follow it. Keep one cursor per host; a page.reset \
                 means continuity was lost. Results are metadata only. Calls to one host \
                 are rate-limited; on rate_limited, wait retry_after_ms.",
            );
        };
        info.with_instructions(
            format!(
                "You are {agent}. pmux mail is the in-mux agent mailbox. \
                 Call pmux_tutorial first for mail, direct pane input, and Space workflows. \
                 Quick reference: pmux_who (presence), \
                 pmux_send (deliver a letter), \
                 pmux_claim then pmux_commit (read and acknowledge mail), \
                 pmux_inbox (peek at depth), \
                 pmux_broadcast (fan-out to all). \
                 send.to is an agent id (operator-id) or alias, never a seat (0@1) or cell: prefix. \
                 Direct terminal input uses the pmux pane-write CLI; pmux_send stores mail. Each mail tool call reconnects to pmuxd — this is normal.",
                agent = agent
            ),
        )
    }
}

fn parse_config() -> Result<Config, String> {
    let mut args = env::args().skip(1).peekable();
    let mut agent: Option<String> = None;
    let mut supervise = false;
    let mut activity_only = false;
    let mut activity_hosts = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--as" => {
                let name = args
                    .next()
                    .ok_or_else(|| "--as needs a value".to_string())?;
                agent = Some(name);
            }
            "--supervise" => supervise = true,
            "--activity-only" => activity_only = true,
            "--activity-hosts" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--activity-hosts needs a file".to_string())?;
                activity_hosts = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{}", prismattyc_core::bin_version("pmux-mcp"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    let mut child_args = Vec::new();
    if let Some(path) = &activity_hosts {
        child_args.extend(["--activity-hosts".into(), path.display().to_string()]);
    }
    if activity_only {
        if agent.is_some() {
            return Err(format!("--activity-only takes no --as identity\n\n{USAGE}"));
        }
        child_args.push("--activity-only".into());
        return Ok(Config {
            agent: None,
            supervise,
            activity_hosts,
            child_args,
        });
    }
    let name = match agent {
        Some(name) => name,
        None => {
            let mut found = None;
            if let Ok(value) = env::var("PMUX_AGENT") {
                if !value.is_empty() {
                    found = Some(value);
                }
            }
            found.ok_or_else(|| {
                format!("no identity: pass --as <agent> or set PMUX_AGENT\n\n{USAGE}")
            })?
        }
    };
    let agent = AgentId::new(name).map_err(|e| e.to_string())?;
    child_args.extend(["--as".into(), agent.to_string()]);
    Ok(Config {
        agent: Some(agent),
        supervise,
        activity_hosts,
        child_args,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = prismattyc_mux::release_update::forward_installed("pmux-mcp") {
        eprintln!("pmux-mcp: {error:#}");
        return ExitCode::FAILURE;
    }
    let config = match parse_config() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("pmux-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.supervise {
        return match supervisor::run(&config.child_args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pmux-mcp supervisor: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let socket = mux_socket();
    let hosts = match &config.activity_hosts {
        Some(path) => match activity::Hosts::load(path) {
            Ok(hosts) => hosts,
            Err(e) => {
                eprintln!("pmux-mcp: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => activity::Hosts::local(socket.clone()),
    };
    let server = match config.agent {
        Some(agent) => PmuxMcp::new(agent, socket, hosts),
        None => PmuxMcp::activity_only(socket, hosts),
    };
    match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => match running.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pmux-mcp: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("pmux-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_mcp() -> PmuxMcp {
        let agent = AgentId::new("mutant-kill-agent").expect("agent id");
        // Nonexistent socket: tools that reach pmux must not look like success.
        let socket = PathBuf::from("/tmp/pmux-mcp-mutant-kill-missing.sock");
        PmuxMcp::new(agent, socket.clone(), activity::Hosts::local(socket))
    }

    fn tool_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn activity_only_serves_just_the_read_only_activity_tools() {
        let socket = PathBuf::from("/tmp/pmux-mcp-activity-only-missing.sock");
        let server = PmuxMcp::activity_only(socket.clone(), activity::Hosts::local(socket));
        let mut names: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "pmux_activity_hosts",
                "pmux_activity_poll",
                "pmux_activity_recent",
                "pmux_activity_seats"
            ]
        );
        assert!(server.agent.is_none());
    }

    #[test]
    fn activity_arguments_are_validated_before_any_command_line() {
        let ok = activity_args(
            "poll",
            Some("worker-a".into()),
            Some("ab:3".into()),
            Some(5),
        )
        .expect("valid arguments");
        assert_eq!(
            ok,
            ["poll", "--seat", "worker-a", "--cursor", "ab:3", "--limit", "5"]
        );
        let recent = activity_args("recent", Some("worker-a".into()), None, None).expect("valid");
        assert_eq!(recent, ["recent", "worker-a"]);
        for (seat, cursor, limit) in [
            (Some("a;rm -rf ~"), None, None),
            (Some("a b"), None, None),
            (Some("$(id)"), None, None),
            (None, Some("ab:1;id"), None),
            (None, Some("`id`:1"), None),
            (None, None, Some(0)),
            (None, None, Some(257)),
        ] {
            let result = activity_args(
                "poll",
                seat.map(str::to_owned),
                cursor.map(str::to_owned),
                limit,
            );
            let error = result.expect_err("must be rejected before reaching a shell");
            assert_eq!(error.code, "invalid_argument");
        }
    }

    #[test]
    fn tools_cover_the_mailbox_verbs() {
        let names: Vec<_> = test_mcp()
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for want in [
            "pmux_send",
            "pmux_claim",
            "pmux_commit",
            "pmux_release",
            "pmux_inbox",
            "pmux_alias",
            "pmux_who",
            "pmux_broadcast",
            "pmux_tutorial",
            "pmux_space_details",
            "pmux_space_result",
            "pmux_space_role",
            "pmux_space_link",
            "pmux_space_attention",
            "pmux_template_save",
            "pmux_template_list",
            "pmux_template_preview",
            "pmux_template_create",
            "pmux_activity_hosts",
            "pmux_activity_seats",
            "pmux_activity_recent",
            "pmux_activity_poll",
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
        assert_eq!(names.len(), 22, "unexpected tool count: {names:?}");
    }

    #[test]
    fn send_schema_rejects_seat_addressing_in_copy() {
        let schema = schemars::schema_for!(SendArgs);
        let text = serde_json::to_string(&schema).unwrap();
        assert!(
            text.contains("Never a seat") || text.contains("never a seat"),
            "schema must drop N@G seat copy: {text}"
        );
        assert!(
            !text.contains("live seat (`0@1`)"),
            "old seat wording must not remain: {text}"
        );
    }

    #[tokio::test]
    async fn pmux_space_attention_rejects_unknown_action() {
        let mcp = test_mcp();
        let result = mcp
            .pmux_space_attention(Parameters(SpaceAttentionArgs {
                name: "demo".into(),
                session: "s".into(),
                pane: 1,
                revision: 1,
                action: "ignore".into(),
            }))
            .await
            .expect("tool result");
        assert_eq!(result.is_error, Some(true));
        let text = tool_text(&result);
        assert!(
            text.contains("action must be resolve or snooze"),
            "unknown action must fail closed: {text}"
        );
    }

    #[tokio::test]
    async fn pmux_space_attention_resolve_is_not_the_action_error() {
        // Deleting `!` in the action guard would treat "resolve" as invalid.
        let mcp = test_mcp();
        let result = mcp
            .pmux_space_attention(Parameters(SpaceAttentionArgs {
                name: "demo".into(),
                session: "s".into(),
                pane: 1,
                revision: 1,
                action: "resolve".into(),
            }))
            .await
            .expect("tool result");
        let text = tool_text(&result);
        assert!(
            !text.contains("action must be resolve or snooze"),
            "resolve must reach the Space command path: {text}"
        );
        assert_ne!(
            result,
            CallToolResult::default(),
            "resolve must not collapse to an empty default result"
        );
        assert!(
            result.is_error == Some(true) || !result.content.is_empty(),
            "missing daemon must still produce a tool-level error payload"
        );
    }

    #[tokio::test]
    async fn pmux_space_link_invokes_space_command_not_empty_default() {
        let mcp = test_mcp();
        let result = mcp
            .pmux_space_link(Parameters(SpaceLinkArgs {
                name: "demo".into(),
                label: "docs".into(),
                target: "https://example.com/x".into(),
            }))
            .await
            .expect("tool result");
        assert_ne!(
            result,
            CallToolResult::default(),
            "link must not short-circuit to Ok(Default::default())"
        );
        assert_eq!(result.is_error, Some(true));
        assert!(
            !tool_text(&result).is_empty(),
            "failed link must report why the Space command did not run"
        );
    }
}
