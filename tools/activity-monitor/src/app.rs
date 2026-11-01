// SPDX-License-Identifier: MIT

//! Joins the feed, the model and the view.

use std::time::{SystemTime, UNIX_EPOCH};

use lunatui_core::{ClusterTable, Grid, Rect};
use lunatui_image::ImageSink;
use lunatui_term::Event;
use lunatui_widgets::RenderCx;

use crate::feed::{Feed, Update};
use crate::model::Host;
use crate::ui::{self, Clock, Palette, Ui};

pub struct Monitor {
    pub hosts: Vec<Host>,
    pub ui: Ui,
    pub palette: Palette,
    pub feed: Option<Feed>,
    /// Fixed wall clock for snapshots and tests; `None` reads the system clock.
    pub fixed_now: Option<u64>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

impl Monitor {
    pub fn new(labels: Vec<String>, palette: Palette, feed: Option<Feed>) -> Self {
        Self {
            hosts: labels.into_iter().map(Host::new).collect(),
            ui: Ui::default(),
            palette,
            feed,
            fixed_now: None,
        }
    }

    fn wall(&self) -> u64 {
        self.fixed_now.unwrap_or_else(now_ms)
    }

    /// Applies queued feed updates. Returns true when anything arrived.
    pub fn drain(&mut self, mono_ms: u64) -> bool {
        let Some(feed) = self.feed.as_ref() else {
            return false;
        };
        let wall = self.fixed_now.unwrap_or_else(now_ms);
        let mut any = false;
        while let Ok(update) = feed.updates.try_recv() {
            any = true;
            let Some(host) = self.hosts.get_mut(update.host) else {
                continue;
            };
            match update.update {
                Update::Page(page) => host.apply(page, mono_ms),
                Update::Disconnected => host.disconnect(wall),
                Update::Disabled => host.link = crate::model::Link::Disabled,
                Update::Unsupported => host.link = crate::model::Link::Unsupported,
            }
        }
        any
    }

    pub fn animating(&self, mono_ms: u64) -> bool {
        ui::animating(&self.hosts, &self.ui, mono_ms)
    }

    /// Applies one input event. Returns true to quit.
    pub fn event(&mut self, event: &Event) -> bool {
        let now = self.wall();
        self.ui.event(event, &self.hosts, now)
    }

    pub fn render(
        &mut self,
        grid: &mut Grid,
        clusters: &mut ClusterTable,
        sink: Option<&mut dyn ImageSink>,
        area: Rect,
        mono_ms: u64,
        pixels: bool,
    ) {
        let theme = self.palette.theme();
        let clock = Clock {
            wall_ms: self.wall(),
            mono_ms,
        };
        let mut cx = RenderCx::new(grid, clusters, &theme);
        if let Some(sink) = sink {
            cx = cx.image_sink(sink);
        }
        ui::render(
            &mut cx,
            area,
            &self.hosts,
            &mut self.ui,
            &self.palette,
            clock,
            pixels,
        );
    }

    /// Renders one cell-only frame at the given size.
    pub fn frame(&mut self, width: u16, height: u16, mono_ms: u64) -> (Grid, ClusterTable) {
        let area = Rect::new(0, 0, width, height);
        let mut grid = Grid::new(width, height);
        let mut clusters = ClusterTable::new();
        self.render(&mut grid, &mut clusters, None, area, mono_ms, false);
        (grid, clusters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::Source;
    use crate::model::tests::{page_in, record_run};
    use crate::model::Link;
    use std::time::{Duration, Instant};

    #[test]
    fn the_wall_clock_is_unix_milliseconds() {
        // 2023-11-14 in Unix milliseconds; any current clock is later.
        assert!(now_ms() > 1_700_000_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn drain_applies_queued_pages_and_outages_and_reports_whether_anything_arrived() {
        let line = serde_json::json!({
            "type": "activity", "seat_filter": null,
            "page": {
                "epoch": "e", "oldest_sequence": 1, "current_sequence": 1, "has_more": false, "reset": null,
                "cursor": {"epoch": "e", "sequence": 1},
                "records": [{"sequence": 1, "received_at_ms": 1, "event": {
                    "schema_version": 1, "seat": "worker-a", "run_id": "run-1",
                    "event_id": "event-1", "source": "hooks", "kind": "waiting"}}]
            }
        });
        let script = format!("printf '%s\\n' '{line}'");
        let feed = Feed::start(vec![Source::Command(vec![
            "/bin/sh".into(),
            "-c".into(),
            script,
        ])]);
        let mut monitor = Monitor::new(vec!["remote".into()], Palette::DARK, Some(feed));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(monitor.hosts[0].link, Link::Retrying { .. }) {
            monitor.drain(0);
            assert!(
                Instant::now() < deadline,
                "no outage after the command exited"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            monitor.hosts[0].seats.len(),
            1,
            "the page was applied before the exit"
        );
        assert!(
            !monitor.drain(0),
            "nothing new is queued right after the exit"
        );
        let mut idle = Monitor::new(vec!["mac".into()], Palette::DARK, None);
        assert!(!idle.drain(0));
        assert!(idle.hosts[0].seats.is_empty());
    }

    #[test]
    fn a_state_change_animates_until_the_fade_ends() -> anyhow::Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, None);
        monitor.hosts[0].apply(
            page_in(
                "e",
                vec![record_run("worker-a", 1, "working", 1, "r")?],
                None,
            )?,
            0,
        );
        assert!(!monitor.animating(0), "a first state is not a change");
        monitor.hosts[0].apply(
            page_in(
                "e",
                vec![record_run("worker-a", 2, "waiting", 2, "r")?],
                None,
            )?,
            100,
        );
        assert!(monitor.animating(100 + ui::FADE_MS - 1));
        assert!(!monitor.animating(100 + ui::FADE_MS));
        monitor.ui.motion = false;
        assert!(!monitor.animating(150));
        Ok(())
    }
}
