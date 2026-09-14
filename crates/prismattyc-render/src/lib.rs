//! Classic cell-grid rendering and future swappable backends for Prismattyc.

use std::collections::BTreeMap;
use std::io::{self, Write};

use prismattyc_core::{
    char_display_width, for_each_display_scalar, CellRange, Color, Screen, Style,
};
use prismattyc_protocol::{
    resolve_workspace_rows, validate_workspace_snapshot, CollectionSnapshot, StatusSnapshot,
    StatusTone, StatusVisual, TreeNode, TreeNodeKind, WorkspaceSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceRect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub rows: u16,
    pub cols: u16,
    pub lines: Vec<String>,
    pub placements: BTreeMap<u32, WorkspaceRect>,
    /// Inverse runs in dock cell coordinates. Only the selected node region.
    pub selected_runs: Vec<SelectedCellRun>,
    /// Semantic theme runs for static status primitives.
    pub status_runs: Vec<StatusCellRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedCellRun {
    pub row: u16,
    pub col: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCellRun {
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub tone: StatusTone,
}

/// Paint one display-width row onto `screen`. Combining tails keep the
/// cursor after their base so [`Screen::put_char`] attaches to that glyph.
pub fn paint_display_row(
    screen: &mut Screen,
    row: usize,
    line: &str,
    style_at: impl Fn(usize) -> Style,
) {
    for_each_display_scalar(line, |col, ch, width| {
        if width > 0 {
            screen.set_cursor_position(row, col);
            screen.set_style(style_at(col));
        }
        screen.put_char(ch);
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLayoutError {
    InvalidTree,
    Unsatisfiable,
}

/// Resolve a bounded keyed workspace tree into clipped cells. `Ok(None)` is
/// the frozen too-small fallback and never mutates the classic grid.
pub fn layout_workspace(
    snapshot: &WorkspaceSnapshot,
    pane_rows: u16,
    pane_cols: u16,
) -> Result<Option<WorkspaceLayout>, WorkspaceLayoutError> {
    layout_workspace_with_status(snapshot, None, pane_rows, pane_cols)
}

pub fn layout_workspace_with_status(
    snapshot: &WorkspaceSnapshot,
    status: Option<&StatusSnapshot>,
    pane_rows: u16,
    pane_cols: u16,
) -> Result<Option<WorkspaceLayout>, WorkspaceLayoutError> {
    validate_workspace_snapshot(snapshot).map_err(|_| WorkspaceLayoutError::InvalidTree)?;
    let Some(rows) = resolve_workspace_rows(snapshot.rows, pane_rows, pane_cols) else {
        return Ok(None);
    };
    let mut canvas = vec![vec![" ".to_string(); usize::from(pane_cols)]; usize::from(rows)];
    let mut children = BTreeMap::<u32, Vec<&TreeNode>>::new();
    for node in &snapshot.nodes {
        children.entry(node.parent).or_default().push(node);
    }
    let roots = children.get(&0).cloned().unwrap_or_default();
    let [root] = roots.as_slice() else {
        return Err(WorkspaceLayoutError::InvalidTree);
    };
    let mut placements = BTreeMap::new();
    layout_node(
        root,
        WorkspaceRect {
            row: 0,
            col: 0,
            rows,
            cols: pane_cols,
        },
        pane_cols,
        &children,
        &mut placements,
        &mut canvas,
    )?;
    let status_runs = status
        .filter(|layer| {
            layer.surface_generation == snapshot.surface_generation
                && layer.scene_rev == snapshot.scene_rev
                && prismattyc_protocol::validate_status_snapshot(layer).is_ok()
        })
        .map(|layer| apply_status_layer(snapshot, layer, &placements, &mut canvas))
        .unwrap_or_default();
    let lines = canvas.into_iter().map(join_canvas_row).collect();
    let selected_runs = selected_runs_for(snapshot, &placements);
    Ok(Some(WorkspaceLayout {
        rows,
        cols: pane_cols,
        lines,
        placements,
        selected_runs,
        status_runs,
    }))
}

fn apply_status_layer(
    snapshot: &WorkspaceSnapshot,
    status: &StatusSnapshot,
    placements: &BTreeMap<u32, WorkspaceRect>,
    canvas: &mut [Vec<String>],
) -> Vec<StatusCellRun> {
    let nodes: BTreeMap<_, _> = snapshot.nodes.iter().map(|node| (node.id, node)).collect();
    let mut runs = Vec::new();
    for item in &status.items {
        let Some(node) = nodes.get(&item.node_id).copied() else {
            continue;
        };
        let Some(rect) = placements.get(&item.node_id).copied() else {
            continue;
        };
        if node.kind != TreeNodeKind::Text || rect.rows == 0 || rect.cols == 0 {
            continue;
        }
        let text = format_status_text(&node.text, &item.visual, rect.cols);
        clear_rect(canvas, rect);
        paint_text(canvas, rect, &text);
        mark_status_runs(&mut runs, rect, &text, item.tone);
    }
    runs
}

fn format_status_text(label: &str, visual: &StatusVisual, cols: u16) -> String {
    let label = label.lines().next().unwrap_or("").trim();
    match visual {
        StatusVisual::Badge => {
            let decorated = format!("[{label}]");
            if display_width(&decorated) <= usize::from(cols) {
                decorated
            } else {
                label.to_string()
            }
        }
        StatusVisual::Meter {
            current,
            total: Some(total),
        } => {
            let prefix = format!("{current}/{total} {label}");
            let available = usize::from(cols).saturating_sub(display_width(&prefix) + 3);
            if available < 4 {
                return prefix;
            }
            let width = available.min(12);
            let filled = usize::try_from(
                current
                    .saturating_mul(width as u64)
                    .checked_div(*total)
                    .unwrap_or(0),
            )
            .unwrap_or(width)
            .min(width);
            format!(
                "{prefix} [{}{}]",
                "#".repeat(filled),
                "-".repeat(width - filled)
            )
        }
        StatusVisual::Meter { total: None, .. } => {
            let available = usize::from(cols).saturating_sub(display_width(label) + 3);
            if available < 4 {
                return label.to_string();
            }
            let width = available.min(12);
            let pattern = (0..width)
                .map(|index| if index % 2 == 0 { '=' } else { ' ' })
                .collect::<String>();
            format!("{label} [{pattern}]")
        }
        StatusVisual::Sparkline { samples } => {
            let available = usize::from(cols).saturating_sub(display_width(label) + 1);
            if available == 0 {
                return label.to_string();
            }
            let samples = &samples[samples.len().saturating_sub(available.min(16))..];
            let min = samples.iter().copied().min().unwrap_or(0);
            let max = samples.iter().copied().max().unwrap_or(min);
            let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
            let graph = samples
                .iter()
                .map(|value| {
                    let level = if max == min {
                        3
                    } else {
                        usize::from(value.saturating_sub(min)) * (blocks.len() - 1)
                            / usize::from(max - min)
                    };
                    blocks[level]
                })
                .collect::<String>();
            format!("{label} {graph}")
        }
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn clear_rect(canvas: &mut [Vec<String>], rect: WorkspaceRect) {
    let bottom = usize::from(rect.row.saturating_add(rect.rows)).min(canvas.len());
    for row in canvas.iter_mut().take(bottom).skip(usize::from(rect.row)) {
        let right = usize::from(rect.col.saturating_add(rect.cols)).min(row.len());
        for cell in row.iter_mut().take(right).skip(usize::from(rect.col)) {
            *cell = " ".to_string();
        }
    }
}

fn mark_status_runs(
    runs: &mut Vec<StatusCellRun>,
    rect: WorkspaceRect,
    text: &str,
    tone: StatusTone,
) {
    let mut row = 0u16;
    let mut col = 0u16;
    for ch in text.chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            continue;
        }
        let width = u16::try_from(char_display_width(ch)).unwrap_or(0);
        if width == 0 {
            continue;
        }
        if col.saturating_add(width) > rect.cols && col > 0 {
            row = row.saturating_add(1);
            col = 0;
        }
        if row >= rect.rows {
            break;
        }
        let absolute_row = rect.row.saturating_add(row);
        let absolute_col = rect.col.saturating_add(col);
        if let Some(last) = runs.last_mut() {
            if last.tone == tone
                && last.row == absolute_row
                && last.col.saturating_add(last.cols) == absolute_col
            {
                last.cols = last.cols.saturating_add(width);
                col = col.saturating_add(width);
                continue;
            }
        }
        runs.push(StatusCellRun {
            row: absolute_row,
            col: absolute_col,
            cols: width,
            tone,
        });
        col = col.saturating_add(width);
    }
}

fn selected_runs_for(
    snapshot: &WorkspaceSnapshot,
    placements: &BTreeMap<u32, WorkspaceRect>,
) -> Vec<SelectedCellRun> {
    let mut selected = Vec::new();
    for node in &snapshot.nodes {
        if node.kind != TreeNodeKind::Text {
            continue;
        }
        let Some(rect) = placements.get(&node.id) else {
            continue;
        };
        mark_selected_runs(&mut selected, *rect, &node.text);
    }
    selected
}

fn push_selected_cells(selected: &mut Vec<SelectedCellRun>, row: u16, col: u16, cols: u16) {
    if cols == 0 {
        return;
    }
    if let Some(last) = selected.last_mut() {
        if last.row == row && last.col.saturating_add(last.cols) == col {
            last.cols = last.cols.saturating_add(cols);
            return;
        }
    }
    selected.push(SelectedCellRun { row, col, cols });
}

fn mark_selected_runs(selected: &mut Vec<SelectedCellRun>, rect: WorkspaceRect, text: &str) {
    let mut row = 0u16;
    let mut col = 0u16;
    let mut line_selected = false;
    let mut at_line_start = true;
    for ch in text.chars() {
        if at_line_start {
            line_selected = ch == '>';
            at_line_start = false;
        }
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            at_line_start = true;
            line_selected = false;
            continue;
        }
        let width = u16::try_from(char_display_width(ch)).unwrap_or(0);
        if width == 0 {
            continue;
        }
        if col.saturating_add(width) > rect.cols && col > 0 {
            row = row.saturating_add(1);
            col = 0;
        }
        if line_selected {
            push_selected_cells(
                selected,
                rect.row.saturating_add(row),
                rect.col.saturating_add(col),
                width,
            );
        }
        col = col.saturating_add(width);
    }
}

/// Map a physical row inside wrapped `text` onto its logical newline index.
pub fn logical_index_at_wrapped_row(text: &str, cols: usize, row: usize) -> Option<usize> {
    if cols == 0 {
        return None;
    }
    let mut physical = 0usize;
    for (index, line) in text.split('\n').enumerate() {
        let rows = wrapped_line_rows(line, cols);
        if row >= physical && row < physical.saturating_add(rows) {
            return Some(index);
        }
        physical = physical.saturating_add(rows);
    }
    None
}

fn wrapped_line_rows(line: &str, cols: usize) -> usize {
    if line.is_empty() {
        return 1;
    }
    let mut rows = 1;
    let mut col = 0usize;
    for ch in line.chars() {
        let width = char_display_width(ch);
        if width == 0 {
            continue;
        }
        if col.saturating_add(width) > cols && col > 0 {
            rows += 1;
            col = 0;
        }
        col = col.saturating_add(width);
    }
    rows
}

/// Paint a cached collection tail into unused dock rows.
///
/// Live host/mux paint does not call this. The workspace tree already
/// carries the selected follow/pause/filter window. Overlaying the raw
/// cache tail overwrites that window.
pub fn overlay_collection_cache(
    layout: &mut WorkspaceLayout,
    collections: &BTreeMap<String, CollectionSnapshot>,
) {
    if layout.lines.len() < 2 || collections.is_empty() {
        return;
    }
    let cols = usize::from(layout.cols.max(1));
    let mut extras = Vec::new();
    for (id, snapshot) in collections {
        extras.push(fit_cache_line(
            &format!("cache:{id} rev={}", snapshot.rev),
            cols,
        ));
        let start = snapshot.items.len().saturating_sub(4);
        for item in snapshot.items.iter().skip(start) {
            extras.push(fit_cache_line(&item.text, cols));
        }
    }
    let last = layout.lines.len() - 1;
    let dest = last.saturating_sub(extras.len());
    for (index, extra) in extras.into_iter().enumerate() {
        let row = dest + index;
        if row < last {
            layout.lines[row] = extra;
        }
    }
}

fn fit_cache_line(text: &str, cols: usize) -> String {
    let mut line = String::new();
    let mut width = 0usize;
    let mut last_was_base = false;
    for ch in text.chars() {
        let glyph = char_display_width(ch);
        if glyph == 0 {
            if last_was_base {
                line.push(ch);
            }
            continue;
        }
        if width.saturating_add(glyph) > cols {
            break;
        }
        line.push(ch);
        width = width.saturating_add(glyph);
        last_was_base = true;
    }
    line.extend(std::iter::repeat_n(' ', cols.saturating_sub(width)));
    line
}

fn visible(node: &TreeNode, pane_cols: u16) -> bool {
    pane_cols >= node.show_min_cols && (node.show_max_cols == 0 || pane_cols <= node.show_max_cols)
}

#[allow(clippy::too_many_arguments)]
fn layout_node(
    node: &TreeNode,
    rect: WorkspaceRect,
    pane_cols: u16,
    children: &BTreeMap<u32, Vec<&TreeNode>>,
    placements: &mut BTreeMap<u32, WorkspaceRect>,
    canvas: &mut [Vec<String>],
) -> Result<(), WorkspaceLayoutError> {
    if !visible(node, pane_cols) || rect.rows == 0 || rect.cols == 0 {
        return Ok(());
    }
    placements.insert(node.id, rect);
    let visible_children: Vec<_> = children
        .get(&node.id)
        .into_iter()
        .flatten()
        .copied()
        .filter(|child| visible(child, pane_cols))
        .collect();
    match node.kind {
        TreeNodeKind::Text => paint_text(canvas, rect, &node.text),
        TreeNodeKind::Spacer => {}
        TreeNodeKind::Stack => {
            for child in visible_children {
                layout_node(child, rect, pane_cols, children, placements, canvas)?;
            }
        }
        TreeNodeKind::Border => {
            paint_border(canvas, rect, &node.text);
            if let Some(child) = visible_children.first() {
                let inner = WorkspaceRect {
                    row: rect.row.saturating_add(1),
                    col: rect.col.saturating_add(1),
                    rows: rect.rows.saturating_sub(2),
                    cols: rect.cols.saturating_sub(2),
                };
                layout_node(child, inner, pane_cols, children, placements, canvas)?;
            }
        }
        TreeNodeKind::Row | TreeNodeKind::Column => {
            let total = if node.kind == TreeNodeKind::Row {
                rect.cols
            } else {
                rect.rows
            };
            let sizes = allocate_axis(&visible_children, total)?;
            let mut cursor = 0u16;
            for (child, size) in visible_children.into_iter().zip(sizes) {
                let child_rect = if node.kind == TreeNodeKind::Row {
                    WorkspaceRect {
                        row: rect.row,
                        col: rect.col.saturating_add(cursor),
                        rows: rect.rows,
                        cols: size,
                    }
                } else {
                    WorkspaceRect {
                        row: rect.row.saturating_add(cursor),
                        col: rect.col,
                        rows: size,
                        cols: rect.cols,
                    }
                };
                cursor = cursor.saturating_add(size);
                layout_node(child, child_rect, pane_cols, children, placements, canvas)?;
            }
        }
    }
    Ok(())
}

fn allocate_axis(nodes: &[&TreeNode], total: u16) -> Result<Vec<u16>, WorkspaceLayoutError> {
    if nodes.is_empty() {
        return Ok(Vec::new());
    }
    let minimum: u32 = nodes.iter().map(|node| u32::from(node.min)).sum();
    if minimum > u32::from(total) {
        return Err(WorkspaceLayoutError::Unsatisfiable);
    }
    let mut sizes: Vec<u16> = nodes.iter().map(|node| node.min).collect();
    let mut remaining = total.saturating_sub(u16::try_from(minimum).unwrap_or(total));
    for (node, size) in nodes.iter().zip(&mut sizes) {
        let want = node.preferred.saturating_sub(*size).min(remaining);
        *size = size.saturating_add(want);
        remaining = remaining.saturating_sub(want);
    }
    while remaining > 0 {
        let fill_total: u32 = nodes.iter().map(|node| u32::from(node.fill)).sum();
        if fill_total == 0 {
            if let Some(last) = sizes.last_mut() {
                *last = last.saturating_add(remaining);
            }
            break;
        }
        let before = remaining;
        for (node, size) in nodes.iter().zip(&mut sizes) {
            if remaining == 0 || node.fill == 0 {
                continue;
            }
            let share = ((u32::from(before) * u32::from(node.fill)) / fill_total)
                .max(1)
                .min(u32::from(remaining));
            let share = u16::try_from(share).unwrap_or(remaining);
            *size = size.saturating_add(share);
            remaining = remaining.saturating_sub(share);
        }
    }
    Ok(sizes)
}

fn paint_text(canvas: &mut [Vec<String>], rect: WorkspaceRect, text: &str) {
    let mut row = usize::from(rect.row);
    let mut col = usize::from(rect.col);
    let max_row = row.saturating_add(usize::from(rect.rows)).min(canvas.len());
    let origin = usize::from(rect.col);
    let max_col = origin.saturating_add(usize::from(rect.cols));
    let mut last_cell: Option<(usize, usize)> = None;
    for ch in text.chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = origin;
            last_cell = None;
            continue;
        }
        let width = char_display_width(ch);
        if width == 0 {
            if let Some((cell_row, cell_col)) = last_cell {
                if cell_row < canvas.len() && cell_col < canvas[cell_row].len() {
                    canvas[cell_row][cell_col].push(ch);
                }
            }
            continue;
        }
        if col.saturating_add(width) > max_col && col > origin {
            row = row.saturating_add(1);
            col = origin;
            last_cell = None;
        }
        if row >= max_row {
            break;
        }
        if col < canvas[row].len() {
            write_canvas_cell(canvas, row, col, ch, width);
            last_cell = Some((row, col));
        }
        col = col.saturating_add(width);
    }
}

fn write_canvas_cell(canvas: &mut [Vec<String>], row: usize, col: usize, ch: char, width: usize) {
    canvas[row][col] = ch.to_string();
    for extra in 1..width {
        let next = col.saturating_add(extra);
        if next < canvas[row].len() {
            canvas[row][next].clear();
        }
    }
}

fn join_canvas_row(row: Vec<String>) -> String {
    row.into_iter().filter(|cell| !cell.is_empty()).collect()
}

fn paint_border(canvas: &mut [Vec<String>], rect: WorkspaceRect, title: &str) {
    if canvas.is_empty() || rect.rows == 0 || rect.cols == 0 {
        return;
    }
    let top = usize::from(rect.row);
    let left = usize::from(rect.col);
    let bottom = top
        .saturating_add(usize::from(rect.rows).saturating_sub(1))
        .min(canvas.len().saturating_sub(1));
    let right = left.saturating_add(usize::from(rect.cols).saturating_sub(1));
    for row in top..=bottom {
        if row >= canvas.len() {
            break;
        }
        for col in left..=right.min(canvas[row].len().saturating_sub(1)) {
            let edge = row == top || row == bottom || col == left || col == right;
            if edge {
                canvas[row][col] = if (row == top || row == bottom) && (col == left || col == right)
                {
                    "+".to_string()
                } else if row == top || row == bottom {
                    "-".to_string()
                } else {
                    "|".to_string()
                };
            }
        }
    }
    if rect.cols > 4 {
        let max_col = left
            .saturating_add(2)
            .saturating_add(usize::from(rect.cols.saturating_sub(4)));
        let mut col = left.saturating_add(2);
        let mut last_cell: Option<usize> = None;
        for ch in title.chars() {
            let width = char_display_width(ch);
            if width == 0 {
                if let Some(cell_col) = last_cell {
                    canvas[top][cell_col].push(ch);
                }
                continue;
            }
            if col.saturating_add(width) > max_col {
                break;
            }
            if top < canvas.len() && col < canvas[top].len() {
                write_canvas_cell(canvas, top, col, ch, width);
                last_cell = Some(col);
            }
            col = col.saturating_add(width);
        }
    }
}

/// Optional per-run style for a cell-rect / viewport overlay (protocol 0.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OverlayRun {
    pub text: String,
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A non-caret overlay painted above the classic grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRectOverlay {
    /// Viewport row; may be negative after scroll translation (clipped).
    pub row: i32,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
    pub text: String,
    /// When non-empty, these runs replace `text` as the source of cells.
    pub runs: Vec<OverlayRun>,
}

impl CellRectOverlay {
    pub fn source_text(&self) -> String {
        if self.runs.is_empty() {
            self.text.clone()
        } else {
            self.runs.iter().map(|run| run.text.as_str()).collect()
        }
    }

    pub fn owns(&self, row: usize, column: usize) -> bool {
        let row = i32::try_from(row).unwrap_or(i32::MAX);
        let height = i32::try_from(self.rows).unwrap_or(0);
        row >= self.row
            && row < self.row.saturating_add(height)
            && column >= self.col
            && column < self.col.saturating_add(self.cols)
    }

    /// Style for a cell inside this overlay. `None` means the 0B inverse paint.
    pub fn run_style_at(&self, row: usize, column: usize) -> Option<Style> {
        if self.runs.is_empty() || !self.owns(row, column) {
            return None;
        }
        let rel_row = (i32::try_from(row).unwrap_or(0) - self.row).max(0) as usize;
        let rel_col = column.saturating_sub(self.col);
        let index = rel_row.saturating_mul(self.cols).saturating_add(rel_col);
        let mut walked = 0usize;
        for run in &self.runs {
            let len = run.text.chars().count();
            if index < walked.saturating_add(len) {
                return Some(Style {
                    bold: run.bold,
                    italic: run.italic,
                    underline: run.underline,
                    inverse: run.inverse,
                    foreground: run.fg.map(Color::Indexed).unwrap_or(Color::Default),
                    background: run.bg.map(Color::Indexed).unwrap_or(Color::Default),
                    ..Style::default()
                });
            }
            walked = walked.saturating_add(len);
        }
        None
    }
}

/// Deterministic renderer used by tests and non-interactive consumers.
pub struct PlainTextRenderer;

impl PlainTextRenderer {
    /// Classic fast path: stream characters from the grid with no extra buffers.
    pub fn render(screen: &Screen) -> String {
        let mut output = String::new();
        for row in 0..screen.rows() {
            if row > 0 {
                output.push('\n');
            }
            for cell in screen.view_row(row).expect("row is inside screen bounds") {
                cell.write_grapheme_into(&mut output);
            }
        }
        output
    }

    /// Compose overlays above the classic grid. Empty overlays use the classic path.
    pub fn render_with_overlays(screen: &Screen, overlays: &[CellRectOverlay]) -> String {
        if overlays.is_empty() {
            return Self::render(screen);
        }

        let mut rows: Vec<Vec<char>> = (0..screen.rows())
            .map(|row| {
                screen
                    .row(row)
                    .expect("row is inside screen bounds")
                    .iter()
                    .map(|cell| {
                        if cell.wide_cont {
                            // Keep column alignment for overlay paint; blank cont.
                            ' '
                        } else {
                            cell.character
                        }
                    })
                    .collect()
            })
            .collect();

        for overlay in overlays {
            paint_overlay_chars(&mut rows, screen.columns(), overlay);
        }

        let mut output = String::new();
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.extend(row.iter());
        }
        output
    }
}

/// Full-grid ANSI renderer for the interactive classic path.
pub struct AnsiRenderer<W> {
    writer: W,
}

impl<W: Write> AnsiRenderer<W> {
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Classic fast path: paint the grid with no overlay/selection composition.
    ///
    /// Empty-rich / no-selection calls never pay per-cell overlay or selection checks.
    pub fn render(&mut self, screen: &Screen) -> io::Result<()> {
        self.render_scrolled(screen, 0, true, None)
    }

    /// Paint the viewport from history: `scroll_offset` rows above the live bottom
    ///. Offset 0 is the live grid. When `place_cursor` is false (host is
    /// scrolled up), the caret is parked at (1,1) after paint — caller should keep
    /// the host caret hidden. Optional `selection` is viewport-coordinate chrome
    /// (including history view).
    pub fn render_scrolled(
        &mut self,
        screen: &Screen,
        scroll_offset: usize,
        place_cursor: bool,
        selection: Option<CellRange>,
    ) -> io::Result<()> {
        self.writer.write_all(b"\x1b[H")?;
        let mut active_style = None;
        for row_index in 0..screen.rows() {
            if row_index > 0 {
                write!(self.writer, "\x1b[{};1H", row_index + 1)?;
            }
            for col in 0..screen.columns() {
                let cell = screen.view_cell(scroll_offset, row_index, col);
                // ADR-0004: continuation half is not painted; the lead wide glyph
                // already advanced the outer host by two columns.
                if cell.wide_cont {
                    continue;
                }
                let mut style = cell.style;
                if selection.is_some_and(|r| {
                    // Host stores absolute history rows (scrollback-inclusive select).
                    screen.selection_covers_abs_at_view(scroll_offset, r, row_index, col)
                }) {
                    style.inverse = !style.inverse;
                }
                if active_style != Some(style) {
                    write_style(&mut self.writer, style)?;
                    active_style = Some(style);
                }
                // Base + combining; sanitize only the base scalar for C0 safety.
                write!(self.writer, "{}", sanitize_paint_char(cell.character))?;
                for &mark in cell.combining_marks() {
                    write!(self.writer, "{mark}")?;
                }
            }
        }
        if place_cursor && scroll_offset == 0 {
            let cursor = screen.cursor();
            write!(
                self.writer,
                "\x1b[0m\x1b[{};{}H",
                cursor.row + 1,
                cursor.column + 1
            )?;
        } else {
            // Scrolled away from live bottom: no live caret in the history view.
            write!(self.writer, "\x1b[0m\x1b[1;1H")?;
        }
        self.writer.flush()
    }

    pub fn render_with_overlays(
        &mut self,
        screen: &Screen,
        overlays: &[CellRectOverlay],
    ) -> io::Result<()> {
        self.render_composed(screen, overlays, None)
    }

    /// Paint grid + optional overlays + selection highlight (z-order: grid →
    /// cell-rect → selection inverse → caret). Selection never mutates storage.
    ///
    /// Empty overlays and no selection use the classic `render` fast path.
    pub fn render_composed(
        &mut self,
        screen: &Screen,
        overlays: &[CellRectOverlay],
        selection: Option<CellRange>,
    ) -> io::Result<()> {
        if overlays.is_empty() && selection.is_none() {
            return self.render(screen);
        }

        // Overlay path builds a plain-text grid once, then indexes by column.
        // Avoid `chars.nth(col)` per cell (O(n²) and fragile on `\n`).
        let composed_chars: Option<Vec<Vec<char>>> = if overlays.is_empty() {
            None
        } else {
            let composed = PlainTextRenderer::render_with_overlays(screen, overlays);
            Some(
                composed
                    .split('\n')
                    .map(|line| line.chars().collect())
                    .collect(),
            )
        };

        self.writer.write_all(b"\x1b[H")?;
        let mut active_style = None;
        for row_index in 0..screen.rows() {
            if row_index > 0 {
                write!(self.writer, "\x1b[{};1H", row_index + 1)?;
            }
            for column in 0..screen.columns() {
                let cell = screen.view_cell(0, row_index, column);
                // ADR-0004: skip continuation half (lead wide glyph spans two cols).
                if cell.wide_cont && composed_chars.is_none() {
                    continue;
                }
                let character = if let Some(rows) = composed_chars.as_ref() {
                    rows.get(row_index)
                        .and_then(|row| row.get(column).copied())
                        .unwrap_or(' ')
                } else {
                    cell.character
                };
                let mut style = cell.style;
                if let Some(run_style) = overlay_run_style(overlays, row_index, column) {
                    style = run_style;
                } else if overlay_owns(overlays, row_index, column) {
                    style.inverse = true;
                }
                if selection
                    .is_some_and(|r| screen.selection_covers_abs_at_view(0, r, row_index, column))
                {
                    style.inverse = !style.inverse;
                }
                if active_style != Some(style) {
                    write_style(&mut self.writer, style)?;
                    active_style = Some(style);
                }
                write!(self.writer, "{}", sanitize_paint_char(character))?;
                if composed_chars.is_none() {
                    for &mark in cell.combining_marks() {
                        write!(self.writer, "{mark}")?;
                    }
                }
            }
        }
        let cursor = screen.cursor();
        write!(
            self.writer,
            "\x1b[0m\x1b[{};{}H",
            cursor.row + 1,
            cursor.column + 1
        )?;
        self.writer.flush()
    }
}

/// Defense in depth: never emit raw C0/C1 control scalars from the
/// host paint path (would confuse the outer terminal). Keep tab/LF/CR as space
/// so geometry stays one cell wide; printable and other Unicode pass through.
fn sanitize_paint_char(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' => ' ',
        c if c.is_control() => ' ',
        c => c,
    }
}

fn overlay_owns(overlays: &[CellRectOverlay], row: usize, column: usize) -> bool {
    overlays.iter().any(|overlay| overlay.owns(row, column))
}

fn overlay_run_style(overlays: &[CellRectOverlay], row: usize, column: usize) -> Option<Style> {
    let mut found = None;
    for overlay in overlays {
        if overlay.owns(row, column) {
            if let Some(style) = overlay.run_style_at(row, column) {
                found = Some(style);
            }
        }
    }
    found
}

fn paint_overlay_chars(rows: &mut [Vec<char>], columns: usize, overlay: &CellRectOverlay) {
    if overlay.rows == 0 || overlay.cols == 0 || rows.is_empty() || columns == 0 {
        return;
    }
    let source = overlay.source_text();
    let mut chars = source.chars();
    // Skip characters that belong to rows scrolled above the viewport.
    // Full-row skip keeps the iterator aligned to row-major source offsets.
    if overlay.row < 0 {
        let skipped_rows = overlay.row.unsigned_abs() as usize;
        let skip = skipped_rows.saturating_mul(overlay.cols);
        for _ in 0..skip {
            let _ = chars.next();
        }
    }
    for row_offset in 0..overlay.rows {
        let absolute = overlay
            .row
            .saturating_add(i32::try_from(row_offset).unwrap_or(0));
        if absolute < 0 {
            // Already consumed via the negative-row skip above.
            continue;
        }
        let row = absolute as usize;
        if row >= rows.len() {
            break;
        }
        // Always advance one source cell per logical col so horizontal clip
        // preserves row-major geometry (clipped cells still consume text).
        for col_offset in 0..overlay.cols {
            let ch = chars.next().unwrap_or(' ');
            let column = overlay.col.saturating_add(col_offset);
            if column < columns && column < rows[row].len() {
                rows[row][column] = ch;
            }
        }
    }
}

fn write_style(writer: &mut impl Write, style: Style) -> io::Result<()> {
    writer.write_all(b"\x1b[0m")?;
    if style.bold {
        writer.write_all(b"\x1b[1m")?;
    }
    if style.italic {
        writer.write_all(b"\x1b[3m")?;
    }
    if style.underline {
        writer.write_all(b"\x1b[4m")?;
    }
    if style.inverse {
        writer.write_all(b"\x1b[7m")?;
    }
    write_color(writer, style.foreground, true)?;
    write_color(writer, style.background, false)
}

fn write_color(writer: &mut impl Write, color: Color, foreground: bool) -> io::Result<()> {
    match color {
        Color::Default => Ok(()),
        Color::Ansi(index) => {
            let code = match (foreground, index < 8) {
                (true, true) => 30 + index,
                (true, false) => 90 + index - 8,
                (false, true) => 40 + index,
                (false, false) => 100 + index - 8,
            };
            write!(writer, "\x1b[{code}m")
        }
        Color::Indexed(index) => {
            let ground = if foreground { 38 } else { 48 };
            write!(writer, "\x1b[{ground};5;{index}m")
        }
        Color::Rgb { r, g, b } => {
            let ground = if foreground { 38 } else { 48 };
            write!(writer, "\x1b[{ground};2;{r};{g};{b}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_fixture() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: prismattyc_protocol::WorkspaceRows {
                min: 5,
                preferred: 7,
                max: 9,
            },
            nodes: vec![
                TreeNode {
                    id: 10,
                    parent: 0,
                    kind: TreeNodeKind::Column,
                    min: 5,
                    preferred: 7,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: String::new(),
                },
                TreeNode {
                    id: 20,
                    parent: 10,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 0,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "Tasks: check test lint".into(),
                },
                TreeNode {
                    id: 30,
                    parent: 10,
                    kind: TreeNodeKind::Border,
                    min: 3,
                    preferred: 5,
                    fill: 1,
                    show_min_cols: 60,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "Detail".into(),
                },
                TreeNode {
                    id: 31,
                    parent: 30,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 1,
                    show_min_cols: 60,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "cargo check --workspace --locked".into(),
                },
                TreeNode {
                    id: 40,
                    parent: 10,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 0,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "read-only | no task started".into(),
                },
            ],
        }
    }

    #[test]
    fn workspace_layout_adapts_without_changing_logical_ids() {
        let snapshot = workspace_fixture();
        let wide = layout_workspace(&snapshot, 20, 80).unwrap().unwrap();
        let narrow = layout_workspace(&snapshot, 20, 50).unwrap().unwrap();
        let minimum = layout_workspace(&snapshot, 13, 40).unwrap().unwrap();

        assert_eq!(wide.rows, 7);
        assert!(wide.placements.contains_key(&30));
        assert!(wide.lines.join("\n").contains("cargo check"));
        assert!(!narrow.placements.contains_key(&30));
        assert_eq!(narrow.placements.get(&20).unwrap().row, 0);
        assert_eq!(minimum.rows, 5);
        assert_eq!(minimum.placements.get(&20).unwrap().row, 0);
        assert!(minimum
            .lines
            .iter()
            .all(|line| prismattyc_core::line_display_width(line) == 40));
    }

    #[test]
    fn status_layer_is_static_themed_and_narrow_text_first() {
        let snapshot = workspace_fixture();
        let status = StatusSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rev: 1,
            items: vec![
                prismattyc_protocol::StatusItem {
                    node_id: 20,
                    tone: StatusTone::Success,
                    visual: StatusVisual::Meter {
                        current: 2,
                        total: Some(3),
                    },
                },
                prismattyc_protocol::StatusItem {
                    node_id: 40,
                    tone: StatusTone::Info,
                    visual: StatusVisual::Sparkline {
                        samples: vec![2, 0, 2],
                    },
                },
            ],
        };
        let wide = layout_workspace_with_status(&snapshot, Some(&status), 20, 80)
            .unwrap()
            .unwrap();
        assert!(wide.lines[0].contains("2/3 Tasks"));
        assert!(wide.lines.join("\n").contains("▁"));
        assert!(wide
            .status_runs
            .iter()
            .any(|run| run.tone == StatusTone::Success));

        let narrow = layout_workspace_with_status(&snapshot, Some(&status), 13, 40)
            .unwrap()
            .unwrap();
        assert!(narrow.lines[0].starts_with("2/3 Tasks"));
        assert!(narrow
            .lines
            .iter()
            .all(|line| prismattyc_core::line_display_width(line) == 40));
    }

    #[test]
    fn indeterminate_meter_has_no_time_or_animation_phase() {
        let first = format_status_text(
            "check running",
            &StatusVisual::Meter {
                current: 0,
                total: None,
            },
            40,
        );
        let second = format_status_text(
            "check running",
            &StatusVisual::Meter {
                current: 0,
                total: None,
            },
            40,
        );
        assert_eq!(first, second);
        assert_eq!(first, "check running [= = = = = = ]");
    }

    #[test]
    fn narrow_badge_keeps_authoritative_label_before_decoration() {
        assert_eq!(
            format_status_text("ready", &StatusVisual::Badge, 7),
            "[ready]"
        );
        assert_eq!(
            format_status_text("ready", &StatusVisual::Badge, 5),
            "ready"
        );
        assert_eq!(format_status_text("警告", &StatusVisual::Badge, 4), "警告");
    }

    #[test]
    fn wrapped_continuation_stays_on_the_same_logical_line() {
        let text = format!("> {} crates/first.rs:1:1\n  second", "wrap".repeat(20));
        assert_eq!(logical_index_at_wrapped_row(&text, 20, 0), Some(0));
        assert_eq!(logical_index_at_wrapped_row(&text, 20, 1), Some(0));
        let last = text
            .split('\n')
            .next()
            .unwrap()
            .chars()
            .count()
            .div_ceil(20);
        assert_eq!(logical_index_at_wrapped_row(&text, 20, last), Some(1));
    }

    #[test]
    fn wide_and_combining_glyphs_use_cell_width() {
        assert_eq!(logical_index_at_wrapped_row("界界\nnext", 2, 0), Some(0));
        assert_eq!(logical_index_at_wrapped_row("界界\nnext", 2, 1), Some(0));
        assert_eq!(logical_index_at_wrapped_row("界界\nnext", 2, 2), Some(1));
        assert_eq!(
            logical_index_at_wrapped_row("e\u{0301}e\u{0301}\nnext", 1, 1),
            Some(0)
        );
    }

    #[test]
    fn selected_runs_stay_inside_the_marked_node() {
        let snapshot = WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: prismattyc_protocol::WorkspaceRows {
                min: 5,
                preferred: 5,
                max: 8,
            },
            nodes: vec![
                TreeNode {
                    id: 1,
                    parent: 0,
                    kind: TreeNodeKind::Row,
                    min: 3,
                    preferred: 3,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: String::new(),
                },
                TreeNode {
                    id: 2,
                    parent: 1,
                    kind: TreeNodeKind::Text,
                    min: 3,
                    preferred: 3,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "Tasks".into(),
                },
                TreeNode {
                    id: 3,
                    parent: 1,
                    kind: TreeNodeKind::Text,
                    min: 3,
                    preferred: 3,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "> error crates/x.rs:1:1".into(),
                },
            ],
        };
        let layout = layout_workspace(&snapshot, 20, 80).unwrap().unwrap();
        let right = *layout.placements.get(&3).unwrap();
        assert!(!layout.selected_runs.is_empty());
        for run in &layout.selected_runs {
            assert!(run.col >= right.col, "{run:?} left of {right:?}");
            assert!(
                run.col.saturating_add(run.cols) <= right.col.saturating_add(right.cols),
                "{run:?} wider than {right:?}"
            );
        }
    }

    #[test]
    fn workspace_layout_clips_chrome_and_rejects_unsatisfiable_tree() {
        let mut snapshot = workspace_fixture();
        snapshot.nodes[2].text = "detail title that is much wider than its pane".into();
        let layout = layout_workspace(&snapshot, 20, 60).unwrap().unwrap();
        assert!(layout
            .lines
            .iter()
            .all(|line| prismattyc_core::line_display_width(line) == 60));

        snapshot.nodes[1].min = 4;
        snapshot.nodes[1].preferred = 4;
        snapshot.nodes[2].min = 4;
        snapshot.nodes[2].preferred = 4;
        snapshot.nodes[4].min = 4;
        snapshot.nodes[4].preferred = 4;
        assert_eq!(
            layout_workspace(&snapshot, 13, 60),
            Err(WorkspaceLayoutError::Unsatisfiable)
        );
    }

    #[test]
    fn wide_workspace_rows_keep_host_screen_aligned() {
        let mut snapshot = workspace_fixture();
        snapshot.nodes[1].text = "e\u{301} 警告e\u{301} crates/wide.rs:1:1".into();
        let layout = layout_workspace(&snapshot, 20, 80).unwrap().unwrap();
        assert!(layout.lines.iter().any(|line| line.contains("警告")));
        assert!(layout.lines.iter().any(|line| line.contains('\u{0301}')));
        let mut screen = Screen::new(usize::from(layout.cols), usize::from(layout.rows), 0);
        for (row, line) in layout.lines.iter().enumerate() {
            assert_eq!(
                prismattyc_core::line_display_width(line),
                usize::from(layout.cols),
                "workspace row must occupy exactly the declared cell width: {line:?}"
            );
            paint_display_row(&mut screen, row, line, |_| Style::default());
        }
        let painted: Vec<String> = (0..usize::from(layout.rows))
            .map(|row| {
                let mut line = String::new();
                if let Some(cells) = screen.view_row(row) {
                    for cell in cells {
                        if cell.wide_cont {
                            continue;
                        }
                        cell.write_grapheme_into(&mut line);
                    }
                }
                line
            })
            .collect();
        let footer = painted
            .iter()
            .find(|line| line.contains("read-only"))
            .expect("footer row survived");
        assert!(
            !footer.contains("告") && !footer.contains("警"),
            "wide glyphs must not wrap into later workspace rows: {footer:?}"
        );
        assert!(painted.iter().any(|line| line.contains("警告")));
        let marked = painted
            .iter()
            .find(|line| line.contains("警告"))
            .expect("wide row");
        assert!(
            marked.contains("e\u{301} 警告e\u{301}"),
            "combining tail must stay on its base at col 0 and after a width-2 glyph: {marked:?}"
        );
        assert!(
            !marked.contains("\u{301}e"),
            "mark must not attach to the previous cell: {marked:?}"
        );
    }

    #[test]
    fn workspace_layout_returns_none_below_frozen_geometry() {
        let snapshot = workspace_fixture();
        assert_eq!(layout_workspace(&snapshot, 12, 80).unwrap(), None);
        assert_eq!(layout_workspace(&snapshot, 20, 39).unwrap(), None);
    }

    #[test]
    fn overlay_collection_cache_paints_reattach_window() {
        use prismattyc_protocol::{CollectionItem, CollectionSnapshot};
        let snapshot = workspace_fixture();
        let mut layout = layout_workspace(&snapshot, 24, 80).unwrap().unwrap();
        let mut collections = BTreeMap::new();
        collections.insert(
            "diag".into(),
            CollectionSnapshot {
                surface_generation: 1,
                collection_id: "diag".into(),
                rev: 3,
                items: vec![CollectionItem {
                    id: 1,
                    replaceable: false,
                    text: "check failed".into(),
                }],
            },
        );
        overlay_collection_cache(&mut layout, &collections);
        let joined = layout.lines.join("\n");
        assert!(joined.contains("cache:diag rev=3"));
        assert!(joined.contains("check failed"));
    }

    #[test]
    fn plain_text_preserves_grid_shape() {
        let mut screen = Screen::new(3, 2, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        assert_eq!(PlainTextRenderer::render(&screen), "abc\nd  ");
    }

    #[test]
    fn sanitize_paint_char_maps_c0_to_space() {
        assert_eq!(sanitize_paint_char('\u{0007}'), ' '); // BEL
        assert_eq!(sanitize_paint_char('\n'), ' ');
        assert_eq!(sanitize_paint_char('\t'), ' ');
        assert_eq!(sanitize_paint_char('A'), 'A');
        assert_eq!(sanitize_paint_char('中'), '中');
    }

    #[test]
    fn ansi_render_does_not_emit_raw_bel_from_cell() {
        let mut screen = Screen::new(2, 1, 0);
        // Grid can hold a control scalar (defense path if feed ever stores one).
        screen.put_char('\u{0007}');
        let mut out = Vec::new();
        AnsiRenderer::new(&mut out).render(&screen).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(
            !s.contains('\u{0007}'),
            "host paint must not emit raw BEL: {s:?}"
        );
    }

    #[test]
    fn empty_overlays_match_classic_render() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        assert_eq!(
            PlainTextRenderer::render(&screen),
            PlainTextRenderer::render_with_overlays(&screen, &[])
        );
    }

    #[test]
    fn overlay_paints_above_grid_without_mutating_screen() {
        let mut screen = Screen::new(10, 1, 0);
        for character in "..........".chars() {
            screen.put_char(character);
        }
        let overlays = [CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 9,
            text: "status:ok".into(),
            runs: Vec::new(),
        }];
        assert_eq!(
            PlainTextRenderer::render_with_overlays(&screen, &overlays),
            "status:ok."
        );
        assert_eq!(PlainTextRenderer::render(&screen), "..........");
    }

    #[test]
    fn overlay_clips_when_partially_scrolled_above_viewport() {
        let mut screen = Screen::new(4, 2, 0);
        for character in "........".chars() {
            screen.put_char(character);
        }
        let overlays = [CellRectOverlay {
            row: -1,
            col: 0,
            rows: 2,
            cols: 4,
            text: "ABCD1234".into(),
            runs: Vec::new(),
        }];
        // First overlay row is above the viewport; second row paints "1234".
        assert_eq!(
            PlainTextRenderer::render_with_overlays(&screen, &overlays),
            "1234\n...."
        );
    }

    #[test]
    fn right_edge_multi_row_clip_preserves_row_major_source_offsets() {
        // 2x3 source ABCDEF at col=3 on a 4-wide screen: only col 3 is visible,
        // so row0 paints A and row1 paints D (not B).
        let mut screen = Screen::new(4, 2, 0);
        for character in "........".chars() {
            screen.put_char(character);
        }
        let overlays = [CellRectOverlay {
            row: 0,
            col: 3,
            rows: 2,
            cols: 3,
            text: "ABCDEF".into(),
            runs: Vec::new(),
        }];
        assert_eq!(
            PlainTextRenderer::render_with_overlays(&screen, &overlays),
            "...A\n...D"
        );
    }

    #[test]
    fn render_composed_styled_run_emits_indexed_sgr() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "....".chars() {
            screen.put_char(character);
        }
        let overlays = [CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 2,
            text: String::new(),
            runs: vec![OverlayRun {
                text: "AB".into(),
                fg: Some(1),
                bg: Some(4),
                bold: true,
                italic: false,
                underline: true,
                inverse: false,
            }],
        }];
        let mut out = Vec::new();
        AnsiRenderer::new(&mut out)
            .render_composed(&screen, &overlays, None)
            .unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\u{1b}[1m"), "bold: {s:?}");
        assert!(s.contains("\u{1b}[4m"), "underline: {s:?}");
        assert!(s.contains("\u{1b}[38;5;1m"), "fg indexed: {s:?}");
        assert!(s.contains("\u{1b}[48;5;4m"), "bg indexed: {s:?}");
        assert!(s.contains('A'));
    }

    #[test]
    fn ansi_renderer_emits_style_and_restores_cursor() {
        let mut screen = Screen::new(2, 1, 0);
        screen.set_style(Style {
            bold: true,
            foreground: Color::Ansi(1),
            ..Style::default()
        });
        screen.put_char('X');
        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes).render(&screen).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("\x1b[1m\x1b[31mX"));
        assert!(rendered.ends_with("\x1b[0m\x1b[1;2H"));
    }

    #[test]
    fn ansi_renderer_emits_indexed_and_rgb() {
        let mut screen = Screen::new(2, 1, 0);
        screen.set_style(Style {
            foreground: Color::Indexed(196),
            background: Color::Rgb { r: 1, g: 2, b: 3 },
            ..Style::default()
        });
        screen.put_char('Z');
        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes).render(&screen).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("\x1b[38;5;196m"), "indexed: {rendered:?}");
        assert!(rendered.contains("\x1b[48;2;1;2;3m"), "rgb: {rendered:?}");
    }

    // / F6: `render_composed` must emit inverse (CSI 7) for selected cells.
    #[test]
    fn render_composed_selection_emits_inverse_sgr() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        let selection = CellRange {
            start_row: 0,
            start_col: 1,
            end_row: 0,
            end_col: 2,
        };

        let mut without = Vec::new();
        AnsiRenderer::new(&mut without)
            .render_composed(&screen, &[], None)
            .unwrap();
        let without = String::from_utf8(without).unwrap();

        let mut with = Vec::new();
        AnsiRenderer::new(&mut with)
            .render_composed(&screen, &[], Some(selection))
            .unwrap();
        let with = String::from_utf8(with).unwrap();

        assert!(
            !without.contains("\x1b[7m"),
            "no selection must not emit inverse: {without:?}"
        );
        // write_style path: reset + CSI 7 for the selected span "bc".
        assert!(
            with.contains("\x1b[7m"),
            "selection must emit CSI 7 inverse via write_style: {with:?}"
        );
        assert!(
            with.contains("\x1b[7mbc") || with.contains("\x1b[0m\x1b[7mbc"),
            "selected cells must paint under inverse: {with:?}"
        );
        assert_ne!(without, with, "selection must change the wire stream");
    }

    /// selection inverse XOR-toggles cells that already carry inverse SGR.
    #[test]
    fn render_composed_selection_xors_existing_inverse() {
        let mut screen = Screen::new(3, 1, 0);
        screen.set_style(Style {
            inverse: true,
            ..Style::default()
        });
        screen.put_char('A');
        screen.set_style(Style::default());
        screen.put_char('B');
        screen.put_char('C');

        let selection = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1, // A (already inverse) and B (plain)
        };

        let mut without = Vec::new();
        AnsiRenderer::new(&mut without)
            .render_composed(&screen, &[], None)
            .unwrap();
        let without = String::from_utf8(without).unwrap();

        let mut with = Vec::new();
        AnsiRenderer::new(&mut with)
            .render_composed(&screen, &[], Some(selection))
            .unwrap();
        let with = String::from_utf8(with).unwrap();

        assert_ne!(
            without, with,
            "XOR selection must differ from unselected paint"
        );
        // Storage inverse on A → CSI 7 without selection.
        assert!(
            without.contains("\x1b[7mA"),
            "pre-styled inverse A on wire: {without:?}"
        );
        // Selected A: inverse XOR → not inverse (no CSI 7 immediately before A).
        assert!(
            !with.contains("\x1b[7mA"),
            "selection must XOR-clear inverse on already-inverse A: {with:?}"
        );
        // Selected B: plain XOR → inverse on wire.
        assert!(
            with.contains("\x1b[7mB"),
            "selection must inverse plain B: {with:?}"
        );
    }

    // / F6: multi-row visual trim — pure trailing spaces get no inverse paint.
    #[test]
    fn render_composed_multi_row_selection_skips_trailing_space_inverse() {
        let mut screen = Screen::new(8, 2, 0);
        for character in "ab".chars() {
            screen.put_char(character);
        }
        screen.carriage_return();
        screen.line_feed();
        for character in "cd".chars() {
            screen.put_char(character);
        }
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 7,
        };

        // Geometry includes blanks; visual paint (selection_covers_cell) does not.
        assert!(range.contains(0, 5));
        assert!(!screen.selection_covers_cell(range, 0, 5));
        assert!(screen.selection_covers_cell(range, 0, 0));
        assert!(screen.selection_covers_cell(range, 0, 1));
        assert!(!screen.selection_covers_cell(range, 1, 4));

        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes)
            .render_composed(&screen, &[], Some(range))
            .unwrap();
        let rendered = String::from_utf8(bytes).unwrap();

        assert!(
            rendered.contains("\x1b[7m"),
            "covered cells still get inverse: {rendered:?}"
        );
        // Correct trim: inverse only "ab", then SGR reset before trailing spaces.
        // Wrong (stream contains): inverse stays through "ab      ".
        assert!(
            rendered.contains("\x1b[7mab\x1b[0m"),
            "trailing spaces must drop inverse after covered text: {rendered:?}"
        );
        assert!(
            !rendered.contains("\x1b[7mab "),
            "must not keep CSI 7 active over pure trailing spaces: {rendered:?}"
        );
        assert!(
            rendered.contains("\x1b[7mcd\x1b[0m"),
            "row 1 covered text inverse + reset: {rendered:?}"
        );
        assert!(
            !rendered.contains("\x1b[7mcd "),
            "row 1 trailing spaces must not stay inverse: {rendered:?}"
        );
    }
}
