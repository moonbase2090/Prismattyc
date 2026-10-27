//! CLI access to the host's local tab namespace.
use super::*;

fn generation() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION.get_or_init(|| {
        format!(
            "{}-{}",
            std::process::id(),
            prismattyc_mux::host_render_status::unix_ms()
        )
    })
}

pub(super) fn poll(app: &mut App) {
    static LAST_POLL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = prismattyc_mux::host_render_status::unix_ms();
    let last = LAST_POLL_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 500
        || LAST_POLL_MS
            .compare_exchange(
                last,
                now,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
    {
        return;
    }
    let Some(socket) = host_mux_socket() else {
        return;
    };
    let pid = std::process::id();
    let views = app
        .windows
        .values()
        .map(|host| {
            let tabs = host
                .mux
                .tab_infos()
                .into_iter()
                .filter_map(|tab| {
                    Some(prismattyc_mux::host_tabs::Tab {
                        token: tab.window?.get(),
                        title: tab.title,
                        fixed_name: tab.fixed_name,
                    })
                })
                .collect();
            prismattyc_mux::host_tabs::View {
                token: host.mux.instance(),
                tabs,
            }
        })
        .collect();
    let manifest = prismattyc_mux::host_tabs::Manifest {
        pid,
        generation: generation().to_string(),
        views,
    };
    if let Err(error) = prismattyc_mux::host_tabs::write_manifest(&socket, &manifest) {
        eprintln!("prismattyc-host: could not publish host tab manifest: {error}");
        return;
    }
    let Some(request) = prismattyc_mux::host_tabs::take_request(&socket, pid) else {
        return;
    };
    let result = (|| -> Result<()> {
        anyhow::ensure!(
            request.generation == manifest.generation,
            "host tab view expired; run pmux tab list again"
        );
        let host = app
            .windows
            .values_mut()
            .find(|host| host.mux.instance() == request.view)
            .context("host tab view no longer exists; run pmux tab list again")?;
        let window = host
            .mux
            .tab_window_by_token(request.tab)
            .context("host tab no longer exists; run pmux tab list again")?;
        host.mux
            .set_fixed_tab_name(window, request.name.as_deref())?;
        mark_layout_dirty(host);
        host.dirty = true;
        host.window.request_redraw();
        Ok(())
    })();
    let (ok, detail) = match result {
        Ok(()) => (true, "tab name updated".to_string()),
        Err(error) => (false, format!("{error:#}")),
    };
    if let Err(error) = prismattyc_mux::host_tabs::respond(&socket, &request, ok, detail) {
        eprintln!("prismattyc-host: could not answer host tab request: {error}");
    }
}

pub(super) fn cleanup() {
    if let Some(socket) = host_mux_socket() {
        prismattyc_mux::host_tabs::remove_manifest(&socket, std::process::id());
    }
}
