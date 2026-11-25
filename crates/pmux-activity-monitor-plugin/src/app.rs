// SPDX-License-Identifier: MIT
//! Pure monitor state. The native host supplies pages, input and clocks.

use lunatui_core::{ClusterTable, Grid, Rect};
use lunatui_image::ImageSink;
use lunatui_runtime::RenderMode;
use lunatui_term::Event;
use lunatui_widgets::graphite::{BundledText, UiFont};
use lunatui_widgets::RenderCx;

use crate::model::Host;
use crate::ui::{self, Clock, Palette, Ui};

/// Graphite text uses the compact bundled fonts, which parse well within one
/// call's CPU budget in the plug-in sandbox.
const GRAPHITE_FONT: UiFont = UiFont::InterCompact;

pub struct Monitor {
    pub hosts: Vec<Host>,
    pub ui: Ui,
    pub palette: Palette,
    pub now_ms: u64,
    /// Whether a frame has been drawn. The first frame is always cells.
    pub(crate) opened: bool,
    /// Whether the Graphite fonts are parsed.
    fonts_loaded: bool,
}

impl Monitor {
    pub fn new(labels: Vec<String>, palette: Palette, now_ms: u64) -> Self {
        Self {
            hosts: labels.into_iter().map(Host::new).collect(),
            ui: Ui::default(),
            palette,
            now_ms,
            opened: false,
            fonts_loaded: false,
        }
    }

    pub fn animating(&self, mono_ms: u64) -> bool {
        ui::animating(&self.hosts, &self.ui, mono_ms)
    }

    pub fn event(&mut self, event: &Event) -> bool {
        self.ui.event(event, &mut self.hosts, self.now_ms)
    }

    /// Graphite whenever the host shows pixels, the viewer has not turned
    /// them off with `p` and the fonts are loaded. Cells otherwise.
    pub fn mode(&self, pixels: bool) -> RenderMode {
        if pixels && self.ui.pixels && self.fonts_loaded {
            RenderMode::Graphite
        } else {
            RenderMode::Cell
        }
    }

    /// Called before each frame. The monitor opens in cells at once, and the
    /// Graphite fonts are parsed in a later call, so no one call both opens
    /// the monitor and parses fonts. Fonts are never parsed while pixels are
    /// off or not shown.
    pub fn load_fonts(&mut self, pixels: bool) {
        if self.opened && self.fonts_pending(pixels) {
            BundledText::shared(GRAPHITE_FONT).load();
            self.fonts_loaded = true;
        }
    }

    /// Whether Graphite waits on fonts that are not parsed yet.
    pub fn fonts_pending(&self, pixels: bool) -> bool {
        pixels && self.ui.pixels && !self.fonts_loaded
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        grid: &mut Grid,
        clusters: &mut ClusterTable,
        sink: Option<&mut dyn ImageSink>,
        area: Rect,
        mono_ms: u64,
        pixels: bool,
        mode: RenderMode,
    ) {
        // Time passes for every host before each frame is drawn, even when no
        // poll brought anything new: an open turn can become stuck.
        for host in &mut self.hosts {
            host.advance(self.now_ms, mono_ms);
        }
        let theme = self.palette.theme();
        let clock = Clock {
            wall_ms: self.now_ms,
            mono_ms,
        };
        let mut cx = RenderCx::new(grid, clusters, &theme);
        if let Some(sink) = sink {
            cx = cx.image_sink(sink).render_mode(mode).ui_font(GRAPHITE_FONT);
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
            RenderMode::Cell,
        );
        (grid, clusters)
    }
}
