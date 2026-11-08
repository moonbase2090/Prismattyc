// SPDX-License-Identifier: MIT

use lunatui_core::{Color, Rect, Style};
use lunatui_widgets::canvas::Points;
use lunatui_widgets::{
    BarSet, Canvas, Marker, RenderCx, Sparkline, SparklineBar, Widget, NINE_LEVELS,
};
use pmux_plugin_api::monitor::GraphStyle;

use crate::model::{Bin, SeatState};
use crate::ui::Palette;

const ASCII: BarSet = BarSet {
    empty: " ",
    one_eighth: ".",
    one_quarter: "_",
    three_eighths: ":",
    half: "-",
    five_eighths: "=",
    three_quarters: "+",
    seven_eighths: "*",
    full: "#",
};

pub(crate) struct Lane<'a> {
    pub bins: &'a [Bin],
    pub style: GraphStyle,
    pub pixels: bool,
    pub background: Color,
    pub palette: &'a Palette,
    pub ink: Option<Color>,
}

fn value(bin: &Bin) -> u64 {
    let base = u64::from(!matches!(
        bin.state,
        None | Some(SeatState::Stopped | SeatState::Unknown)
    ));
    (base + u64::from(bin.events) * 2).min(7)
}

impl Lane<'_> {
    fn color(&self, state: SeatState) -> Color {
        self.ink.unwrap_or_else(|| self.palette.state(state))
    }

    pub fn render(&self, area: Rect, cx: &mut RenderCx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let samples = if self.style == GraphStyle::Braille {
            2
        } else {
            1
        };
        // Pixel lanes retain one value per cell. Braille cells retain both
        // half-cell samples, so nearby events no longer merge into one column.
        if self.style != GraphStyle::Braille || self.pixels {
            let bars: Vec<_> = self
                .bins
                .chunks(samples)
                .map(|pair| {
                    let Some(state) = pair.iter().rev().find_map(|bin| bin.state) else {
                        return SparklineBar::absent();
                    };
                    let bin = Bin {
                        events: pair.iter().map(|bin| bin.events).sum(),
                        state: Some(state),
                    };
                    SparklineBar::value(value(&bin))
                        .style(Style::new().fg(self.color(state)).bg(self.background))
                })
                .collect();
            Sparkline::new(&bars)
                .max(8)
                .style(Style::new().bg(self.background))
                .absent_value_symbol(" ")
                .absent_value_style(Style::new().bg(self.background))
                .bar_set(if self.style == GraphStyle::Ascii && !self.pixels {
                    ASCII
                } else {
                    NINE_LEVELS
                })
                .pixels(self.pixels)
                .render(area, cx);
        }
        if self.style == GraphStyle::Block {
            return;
        }
        if self.style == GraphStyle::Ascii {
            // Keep the chosen cells behind a pixel upload as well, including
            // when a native presenter declines that upload after capture.
            if self.pixels {
                Self {
                    pixels: false,
                    ..*self
                }
                .render(area, cx);
            }
            return;
        }
        let width = usize::from(area.width) * 2;
        let height = u64::from(area.height) * 4;
        Canvas::new(|canvas| {
            for (x, bin) in self.bins.iter().take(width).enumerate() {
                let Some(state) = bin.state else { continue };
                let dots = (value(bin) * height).div_ceil(8);
                let points: Vec<_> = (0..dots).map(|y| (x as f64, y as f64)).collect();
                canvas.draw(&Points {
                    coords: &points,
                    style: Style::new().fg(self.color(state)).bg(self.background),
                });
            }
        })
        .x_bounds([0.0, (width - 1) as f64])
        .y_bounds([0.0, (height - 1) as f64])
        .marker(Marker::Braille)
        .background(Style::new().bg(self.background))
        .pixels(false)
        .render(area, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunatui_core::{ClusterTable, Grid};

    fn draw(style: GraphStyle, bins: &[Bin], columns: u16) -> (String, Grid) {
        let palette = Palette::DARK;
        let theme = palette.theme();
        let mut grid = Grid::new(columns, 1);
        let mut clusters = ClusterTable::new();
        Lane {
            bins,
            style,
            pixels: false,
            background: Color::Default,
            palette: &palette,
            ink: None,
        }
        .render(
            Rect::new(0, 0, columns, 1),
            &mut RenderCx::new(&mut grid, &mut clusters, &theme),
        );
        (grid.display_text(&clusters), grid)
    }

    #[test]
    fn default_braille_keeps_two_distinct_samples_in_each_cell() {
        let bins = [
            Bin {
                events: 1,
                state: Some(SeatState::Working),
            },
            Bin {
                events: 2,
                state: Some(SeatState::Failed),
            },
            Bin {
                events: 0,
                state: None,
            },
            Bin {
                events: 0,
                state: Some(SeatState::Waiting),
            },
        ];
        let (text, grid) = draw(GraphStyle::default(), &bins, 2);
        assert_eq!(text, "⣴⢀\n");
        assert_eq!(grid.get(0, 0).unwrap().style().fg, Palette::DARK.failed);
        assert_eq!(grid.get(1, 0).unwrap().style().fg, Palette::DARK.waiting);
    }

    #[test]
    fn block_and_ascii_keep_their_explicit_glyphs() {
        let bins: Vec<_> = (0..4)
            .map(|events| Bin {
                events,
                state: Some(SeatState::Working),
            })
            .collect();
        assert_eq!(draw(GraphStyle::Block, &bins, 4).0, "▁▃▅▇\n");
        assert_eq!(draw(GraphStyle::Ascii, &bins, 4).0, ".:=*\n");
        assert_eq!(
            draw(GraphStyle::Braille.for_terminal(true), &bins, 4).0,
            ".:=*\n"
        );
    }
}
