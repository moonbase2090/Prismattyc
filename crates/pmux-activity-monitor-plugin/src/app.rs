// SPDX-License-Identifier: MIT
//! Pure monitor state. The native host supplies pages, input and clocks.

use lunatui_core::{ClusterTable, Grid, Rect};
use lunatui_image::ImageSink;
use lunatui_term::Event;
use lunatui_widgets::RenderCx;

use crate::model::Host;
use crate::ui::{self, Clock, Palette, Ui};

pub struct Monitor {
    pub hosts: Vec<Host>,
    pub ui: Ui,
    pub palette: Palette,
    pub now_ms: u64,
}

impl Monitor {
    pub fn new(labels: Vec<String>, palette: Palette, now_ms: u64) -> Self {
        Self {
            hosts: labels.into_iter().map(Host::new).collect(),
            ui: Ui::default(),
            palette,
            now_ms,
        }
    }

    pub fn animating(&self, mono_ms: u64) -> bool {
        ui::animating(&self.hosts, &self.ui, mono_ms)
    }

    pub fn event(&mut self, event: &Event) -> bool {
        self.ui.event(event, &mut self.hosts, self.now_ms)
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
            wall_ms: self.now_ms,
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

    pub fn frame(&mut self, width: u16, height: u16, mono_ms: u64) -> (Grid, ClusterTable) {
        let mut grid = Grid::new(width, height);
        let mut clusters = ClusterTable::new();
        self.render(
            &mut grid,
            &mut clusters,
            None,
            Rect::new(0, 0, width, height),
            mono_ms,
            false,
        );
        (grid, clusters)
    }
}
