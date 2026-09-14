//! Serializable local-shell layout recipes. No command or process state is saved.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Recipe {
    tabs: Vec<(String, Node)>,
    selected: usize,
    focused: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Node {
    Leaf {
        key: u64,
        session: Option<String>,
        cwd: Option<PathBuf>,
        title: Option<String>,
    },
    Split {
        horizontal: bool,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}
impl Node {
    fn size(&self, depth: usize) -> Option<usize> {
        if depth > 16 {
            return None;
        }
        match self {
            Self::Leaf { .. } => Some(1),
            Self::Split {
                ratio,
                first,
                second,
                ..
            } if ratio.is_finite() && *ratio > 0.0 && *ratio < 1.0 => {
                Some(first.size(depth + 1)? + second.size(depth + 1)?)
            }
            _ => None,
        }
    }
}
impl MuxRuntime {
    pub(crate) fn local_recipe(&self) -> Option<Recipe> {
        if !self.panes.values().any(|pane| pane.keep_local) {
            return None;
        }
        fn node(mux: &MuxRuntime, layout: &PaneLayout) -> Option<Node> {
            match layout {
                PaneLayout::Leaf(id) => {
                    let pane = mux.panes.get(id)?;
                    let session = pane
                        .attach_name
                        .clone()
                        .or_else(|| pane.attach_session.clone());
                    if session.is_none() && !pane.keep_local {
                        return None;
                    }
                    Some(Node::Leaf {
                        key: id.get(),
                        session,
                        cwd: pane.keep_local.then(|| pane.cwd_for_split()).flatten(),
                        title: pane.title_pinned.then(|| pane.title.clone()).flatten(),
                    })
                }
                PaneLayout::Split(split) => {
                    match (node(mux, &split.first), node(mux, &split.second)) {
                        (Some(first), Some(second)) => Some(Node::Split {
                            horizontal: split.axis == Axis::Horizontal,
                            ratio: split.ratio,
                            first: Box::new(first),
                            second: Box::new(second),
                        }),
                        (first, second) => first.or(second),
                    }
                }
            }
        }
        Some(Recipe {
            tabs: self
                .window_ids()
                .iter()
                .filter_map(|id| {
                    let win = self.domain.window(*id)?;
                    Some((win.title.clone(), node(self, &win.layout)?))
                })
                .collect(),
            selected: self.selected_tab_index(),
            focused: self.focused_id().get(),
        })
    }

    pub(crate) fn restore_local_recipe(&mut self, recipe: &Recipe) -> Result<()> {
        anyhow::ensure!(
            recipe.tabs.len() <= 64
                && recipe
                    .tabs
                    .iter()
                    .map(|(_, node)| node.size(0))
                    .collect::<Option<Vec<_>>>()
                    .is_some_and(|sizes| sizes.iter().sum::<usize>() <= 64),
            "blank terminal layout is too large or invalid"
        );
        let mut domain = Domain::new();
        let session = domain.create_session("host")?;
        let client = domain.mint_client()?;
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut staged = Vec::new();
        let mut used = std::collections::HashSet::new();
        let mut focused = None;
        let mut selected = None;
        // Stage new shells and geometry before replacing the live view.
        struct Builder<'a> {
            mux: &'a MuxRuntime,
            domain: &'a mut Domain,
            staged: &'a mut Vec<(PaneId, Option<PaneId>, Option<PaneRuntime>)>,
            used: &'a mut std::collections::HashSet<PaneId>,
            shell: &'a str,
            focused_key: u64,
            focused: &'a mut Option<PaneId>,
        }
        impl Builder<'_> {
            fn node(&mut self, node: &Node) -> Result<Option<PaneLayout>> {
                match node {
                    Node::Leaf {
                        key,
                        session,
                        cwd,
                        title,
                    } => {
                        let source = if let Some(session) = session {
                            let found = self
                                .mux
                                .panes
                                .iter()
                                .find(|(id, pane)| {
                                    !self.used.contains(id)
                                        && (pane.attach_name.as_deref() == Some(session)
                                            || pane.attach_session.as_deref() == Some(session))
                                })
                                .map(|(id, _)| *id);
                            let Some(id) = found else {
                                return Ok(None);
                            };
                            self.used.insert(id);
                            Some(id)
                        } else {
                            None
                        };
                        let pane = self.domain.alloc_pane(title.as_deref().unwrap_or(""))?;
                        let runtime = if source.is_none() {
                            let cwd = cwd
                                .as_ref()
                                .filter(|path| path.is_absolute() && path.is_dir())
                                .cloned()
                                .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
                            let mut runtime = PaneRuntime::spawn(
                                pane,
                                self.shell,
                                &["-l".into()],
                                self.mux.cols.max(1),
                                self.mux.rows.max(1),
                                cwd.as_deref(),
                                self.mux.experimental_rich,
                                self.mux.wake.clone(),
                                self.mux.geom.cell_w,
                                self.mux.geom.cell_h,
                                self.mux.space_id.as_deref(),
                            )?;
                            runtime.keep_local = true;
                            runtime.title = title.clone();
                            runtime.title_pinned = title.is_some();
                            Some(runtime)
                        } else {
                            None
                        };
                        self.staged.push((pane, source, runtime));
                        if *key == self.focused_key {
                            *self.focused = Some(pane);
                        }
                        Ok(Some(PaneLayout::leaf(pane)))
                    }
                    Node::Split {
                        horizontal,
                        ratio,
                        first,
                        second,
                    } => {
                        let first = self.node(first)?;
                        let second = self.node(second)?;
                        Ok(match (first, second) {
                            (Some(first), Some(second)) => {
                                Some(PaneLayout::Split(prismattyc_mux::Split {
                                    axis: if *horizontal {
                                        Axis::Horizontal
                                    } else {
                                        Axis::Vertical
                                    },
                                    ratio: *ratio,
                                    first: Box::new(first),
                                    second: Box::new(second),
                                }))
                            }
                            (first, second) => first.or(second),
                        })
                    }
                }
            }
        }
        for (index, (title, node)) in recipe.tabs.iter().enumerate() {
            let layout = Builder {
                mux: self,
                domain: &mut domain,
                staged: &mut staged,
                used: &mut used,
                shell: &shell,
                focused_key: recipe.focused,
                focused: &mut focused,
            }
            .node(node)?;
            if let Some(layout) = layout {
                let (window, placeholder) = domain.create_window(session, title)?;
                domain.set_layout(window, layout)?;
                domain.free_pane(placeholder)?;
                if index == recipe.selected {
                    selected = Some(window);
                }
            }
        }
        // Keep newly added sessions that were not part of the saved recipe.
        for (id, pane) in &self.panes {
            if used.contains(id) || pane.attach_session.is_none() {
                continue;
            }
            let (window, new) =
                domain.create_window(session, pane.attach_name.as_deref().unwrap_or("Session"))?;
            let _ = window;
            staged.push((new, Some(*id), None));
        }
        let mut view = ClientView::attach_session(&domain, client, session);
        if let Some(window) = selected {
            view.window = Some(window);
        }
        if let Some(pane) = focused {
            if let Some(window) = domain.pane_owner(pane) {
                view.window = Some(window);
                view.set_focused_pane(window, pane);
            }
        }
        let window = view.window.context("restored view has no tabs")?;
        let rects = rects_for(&domain, window, self.cols, self.rows, self.geom, None)?;
        let mut panes = HashMap::new();
        for (id, source, runtime) in staged {
            panes.insert(
                id,
                match runtime {
                    Some(runtime) => runtime,
                    None => self
                        .panes
                        .remove(&source.unwrap())
                        .expect("staged existing pane"),
                },
            );
        }
        self.domain = domain;
        self.view = view;
        self.panes = panes;
        self.zoomed = None;
        self.last_window = None;
        self.last_pane.clear();
        self.git_info = Default::default();
        self.apply_rects(rects, self.geom)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn move_local_preserves_live_process_and_scrollback_even_from_last_pane() {
        let mut source = MuxRuntime::spawn("/bin/cat", &[], 80, 24).unwrap();
        let pane = source.focused_id();
        source.retain_local_terminal(pane);
        source.focused_mut().emulator.feed(b"kept scrollback\r\n");
        let pid = source.focused().child_pid();
        let mut target = source.empty_view().unwrap();
        let moved = source.transfer_local(&mut target, pane).unwrap();
        assert_eq!(target.pane(moved).unwrap().child_pid(), pid);
        let screen = target.pane(moved).unwrap().emulator.screen();
        assert!(screen
            .extract_text(screen.viewport_range().unwrap())
            .contains("kept scrollback"));
        assert!(target.is_retained_local_terminal(moved));
        assert!(!source.is_retained_local_terminal(source.focused_id()));
        assert!(source.is_placeholder(source.focused_id()));
        assert_eq!(target.pane_count(), 1);
        assert!(target.local_recipe().is_some());
    }
    #[test]
    fn restore_blank_recipe_creates_fresh_processes_and_keeps_split_ratios() {
        let mut source = MuxRuntime::spawn("/bin/cat", &[], 100, 40).unwrap();
        source.retain_local_terminal(source.focused_id());
        let second = source
            .split_focused("/bin/cat", &[], Axis::Horizontal, 0.3)
            .unwrap();
        source.retain_local_terminal(second);
        let old_pids: Vec<_> = source
            .local_terminal_rows()
            .iter()
            .map(|row| row.3)
            .collect();
        let recipe = source.local_recipe().unwrap();
        let json = serde_json::to_string(&recipe).unwrap();
        assert!(!json.contains("command"));
        let mut restored = source.empty_view().unwrap();
        restored
            .restore_local_recipe(&serde_json::from_str(&json).unwrap())
            .unwrap();
        assert_eq!(restored.pane_count(), 2);
        assert!(restored.tab_layouts()[0].1.contains("ratio: 0.3"));
        assert!(restored
            .local_terminal_rows()
            .iter()
            .all(|row| !old_pids.contains(&row.3)));
        assert!(restored.is_retained_local_terminal(restored.focused_id()));
    }
}
