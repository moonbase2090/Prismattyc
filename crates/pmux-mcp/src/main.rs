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

mod mail;
mod supervisor;

use mail::{format_data, mux_socket, MailOp};

const USAGE: &str = "pmux-mcp — MCP stdio adapter over the pmux Mail* socket

usage: pmux-mcp [-V|--version] [-h|--help] [--as <agent>] [--supervise]

identity: --as <agent>, else $PMUX_AGENT. No default.
Each tool call reconnects: MailHello, one op, drop. Watch is not a tool here.

-V, --version  print version and exit
-h, --help     print this help and exit
--supervise keeps the host-facing stdio transport open and restarts an
inner adapter if it exits. Configure MCP hosts to launch this mode when
they do not respawn failed stdio servers themselves.";

struct Config {
    agent: AgentId,
    supervise: bool,
}

/// MCP stdio server. One agent identity for the process lifetime.
#[derive(Clone)]
struct PmuxMcp {
    agent: AgentId,
    socket: PathBuf,
    #[allow(dead_code)]
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

#[tool_router]
impl PmuxMcp {
    async fn space_command(&self, args: Vec<String>) -> Result<CallToolResult, McpError> {
        let pmux = std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.parent()
                    .map(|p| p.join(prismattyc_mux::platform::executable_name("pmux")))
            })
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("pmux"));
        let mut command = tokio::process::Command::new(pmux);
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
    fn new(agent: AgentId, socket: PathBuf) -> Self {
        Self {
            agent,
            socket,
            tool_router: Self::tool_router(),
        }
    }

    async fn run_op(&self, op: MailOp) -> Result<CallToolResult, McpError> {
        let agent = self.agent.clone();
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

        let tutorial = render_tutorial(&self.agent, presence.trim(), inbox.trim());

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

#[tool_handler]
impl ServerHandler for PmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
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
                agent = self.agent
            ),
        )
    }
}

fn parse_config() -> Result<Config, String> {
    let mut args = env::args().skip(1).peekable();
    let mut agent: Option<String> = None;
    let mut supervise = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--as" => {
                let name = args
                    .next()
                    .ok_or_else(|| "--as needs a value".to_string())?;
                agent = Some(name);
            }
            "--supervise" => supervise = true,
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
    Ok(Config { agent, supervise })
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
        return match supervisor::run(&config.agent).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pmux-mcp supervisor: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let server = PmuxMcp::new(config.agent, mux_socket());
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

    #[test]
    fn tools_cover_the_mailbox_verbs() {
        let names: Vec<_> = PmuxMcp::tool_router()
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
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
        assert_eq!(names.len(), 18, "unexpected tool count: {names:?}");
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
}
