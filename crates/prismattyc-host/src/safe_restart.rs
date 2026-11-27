// SPDX-License-Identifier: MIT
//! App-menu Restart. Shows what `pmux restart --plan` reports, then runs a
//! plain `pmux restart`, which defers anything that would end a running
//! session. This path never asks for `--stop-sessions`.
use super::*;
use serde_json::Value;

/// The read-only impact preview.
const PLAN_ARGS: [&str; 3] = ["restart", "--plan", "--json"];
/// The restart itself. Deliberately free of `--stop-sessions`: live sessions
/// keep their daemon and the result says the daemon was left alone.
const RESTART_ARGS: [&str; 2] = ["restart", "--json"];

/// What a restart would touch, as reported by `pmux restart --plan --json`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    sessions: Vec<String>,
    hosts: usize,
    mcp_adapters: usize,
    daemon_deferred: bool,
}

fn parse_plan(json: &str) -> Result<Plan> {
    let value: Value = serde_json::from_str(json).context("read the restart preview")?;
    // A preview that says sessions would stop is not the safe path. Refuse
    // instead of presenting a Restart button for it.
    anyhow::ensure!(
        value["stops_sessions"] == false,
        "The restart preview says sessions would be stopped. Nothing was restarted."
    );
    let count = |key: &str| value[key].as_array().map_or(0, Vec::len);
    let sessions: Vec<(u64, String)> =
        serde_json::from_value(value["sessions"].clone()).context("read running sessions")?;
    Ok(Plan {
        sessions: sessions.into_iter().map(|(_, name)| name).collect(),
        hosts: count("host_pids"),
        mcp_adapters: count("mcp_supervisor_pids"),
        daemon_deferred: value["daemon_deferred"] == true,
    })
}

pub(super) fn preview_text(plan: &Plan) -> String {
    let mut text = String::from("Restart Prismattyc components that can restart safely.");
    if plan.sessions.is_empty() {
        text.push_str("\nNo sessions are running. The daemon will restart too.");
    } else {
        let noun = if plan.sessions.len() == 1 {
            "session is"
        } else {
            "sessions are"
        };
        text.push_str(&format!(
            "\n{} {noun} running and will keep running. The daemon is left alone.",
            plan.sessions.len()
        ));
        for name in &plan.sessions {
            text.push_str(&format!("\n  {name}"));
        }
    }
    text.push_str(&format!(
        "\nApp windows to restart: {}. MCP adapters to restart: {}.",
        plan.hosts, plan.mcp_adapters
    ));
    text.push_str(
        "\nA window that owns blank terminals or an exact pane view stays open and reports why.",
    );
    text.push_str("\nA session configuration snapshot is saved first.");
    text
}

pub(super) fn start_plan(proxy: winit::event_loop::EventLoopProxy<UserAction>) {
    std::thread::spawn(move || {
        let result = update_flow::run(&find_mux_bin(), &PLAN_ARGS)
            .and_then(|json| parse_plan(&json))
            .map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserAction::SafeRestartPlanned(result));
    });
}

pub(super) fn start_restart(proxy: winit::event_loop::EventLoopProxy<UserAction>) {
    update_flow::withdraw_idle_restart();
    std::thread::spawn(move || {
        let result = restart().map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserAction::SafeRestartFinished(result));
    });
}

fn restart() -> Result<String> {
    let pmux = find_mux_bin();
    // Save before touching anything. If the snapshot cannot be written, stop:
    // restart has nothing to recover from.
    let snapshot = update_flow::snapshot_path("restart")?;
    update_flow::run(
        &pmux,
        &["session-config", "save", &snapshot.to_string_lossy()],
    )
    .context("The session configuration could not be saved. Nothing was restarted.")?;
    let report = update_flow::run(&pmux, &RESTART_ARGS).context("The restart failed")?;
    Ok(format!(
        "{}\nSession configuration saved at {}.",
        space_panel::maintenance_result(&report),
        snapshot.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"{"status":"planned","host_pids":[41],"mcp_supervisor_pids":[52,53],"daemon":true,"sessions":[[1,"work"],[2,"build"]],"daemon_deferred":true,"stops_sessions":false}"#;

    #[test]
    fn restart_never_asks_to_stop_sessions() {
        for args in [&PLAN_ARGS[..], &RESTART_ARGS[..]] {
            assert!(!args.contains(&"--stop-sessions"), "{args:?}");
            assert!(!args.contains(&"--consented-config"), "{args:?}");
        }
        assert!(PLAN_ARGS.contains(&"--plan"));
        assert!(!RESTART_ARGS.contains(&"--plan"));
    }

    #[test]
    fn plan_lists_sessions_and_components() {
        let plan = parse_plan(PLAN).unwrap();
        assert_eq!(
            plan,
            Plan {
                sessions: vec!["work".into(), "build".into()],
                hosts: 1,
                mcp_adapters: 2,
                daemon_deferred: true,
            }
        );
        let text = preview_text(&plan);
        assert!(text.contains("2 sessions are running and will keep running"));
        assert!(text.contains("\n  work\n  build\n"));
        assert!(text.contains("App windows to restart: 1. MCP adapters to restart: 2."));
        assert!(text.contains("snapshot is saved first"));
    }

    #[test]
    fn plan_without_sessions_says_the_daemon_restarts() {
        let plan = parse_plan(
            r#"{"host_pids":[],"mcp_supervisor_pids":[],"sessions":[],"daemon_deferred":false,"stops_sessions":false}"#,
        )
        .unwrap();
        let text = preview_text(&plan);
        assert!(text.contains("No sessions are running. The daemon will restart too."));
        assert!(!text.contains("left alone"));
    }

    #[test]
    fn a_preview_that_stops_sessions_is_refused() {
        let unsafe_plan = PLAN.replace("\"stops_sessions\":false", "\"stops_sessions\":true");
        let error = parse_plan(&unsafe_plan).unwrap_err().to_string();
        assert!(error.contains("would be stopped"), "{error}");
        // A preview with no verdict is not proof of safety either.
        let missing = PLAN.replace(",\"stops_sessions\":false", "");
        assert!(parse_plan(&missing).is_err());
    }

    #[test]
    fn malformed_previews_are_errors_not_empty_plans() {
        assert!(parse_plan("not json").is_err());
        let bad_sessions = PLAN.replace("[[1,\"work\"],[2,\"build\"]]", "\"none\"");
        assert!(parse_plan(&bad_sessions).is_err());
    }
}
