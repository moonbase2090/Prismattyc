//! Title-row label and unfocused-pane notice (PT-190).
//!
//! Pure decisions. Window, mux, and paint side effects stay in the caller.

use crate::config::PaneTitlesMode;

/// Label and handle tint for one tab chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TitleRowDecision<'a> {
    pub label: &'a str,
    pub notice_handle: Option<usize>,
}

/// An unfocused handle whose title changed between snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TitleNotice {
    pub tab: usize,
    pub handle: usize,
    pub title: String,
}

/// Hover, then a live notice, then the focused pane title (when the mode
/// allows it), then the tab name.
pub(crate) fn title_row_decision<'a>(
    mode: PaneTitlesMode,
    handles: usize,
    tab_title: &'a str,
    pane_title: Option<&'a str>,
    handle_titles: &'a [String],
    hover_handle: Option<usize>,
    notice: Option<(usize, &'a str)>,
) -> TitleRowDecision<'a> {
    let notice_handle = notice.map(|(handle, _)| handle);
    if let Some(handle) = hover_handle {
        if let Some(text) = handle_titles
            .get(handle)
            .map(String::as_str)
            .filter(|text| !text.is_empty())
        {
            return TitleRowDecision {
                label: text,
                notice_handle,
            };
        }
    }
    if let Some((_, text)) = notice {
        if !text.is_empty() {
            return TitleRowDecision {
                label: text,
                notice_handle,
            };
        }
    }
    let show_focused = match mode {
        PaneTitlesMode::Focused => true,
        PaneTitlesMode::Hover => handles == 0,
    };
    if show_focused {
        if let Some(text) = pane_title.filter(|text| !text.is_empty()) {
            return TitleRowDecision {
                label: text,
                notice_handle,
            };
        }
    }
    TitleRowDecision {
        label: tab_title,
        notice_handle,
    }
}

/// First snapshot is silent. Later diffs fire on an unfocused handle whose
/// title text changed. The last matching handle in tab order wins.
pub(crate) fn title_notice_from_diff(
    previous: &[Vec<String>],
    current_titles: &[Vec<String>],
    focused: &[Option<usize>],
) -> Option<TitleNotice> {
    if previous.is_empty() {
        return None;
    }
    let mut found = None;
    for (tab, titles) in current_titles.iter().enumerate() {
        let Some(prior) = previous.get(tab) else {
            continue;
        };
        let focused_handle = focused.get(tab).copied().flatten();
        for (handle, title) in titles.iter().enumerate() {
            if title.is_empty() || focused_handle == Some(handle) {
                continue;
            }
            let Some(old) = prior.get(handle) else {
                continue;
            };
            if old != title {
                found = Some(TitleNotice {
                    tab,
                    handle,
                    title: title.clone(),
                });
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn hover_wins_then_notice_then_focused_then_tab_name() {
        let handles = titles(&["pane 1", "Waiting for you"]);
        let hover = title_row_decision(
            PaneTitlesMode::Focused,
            2,
            "main",
            Some("focused"),
            &handles,
            Some(1),
            Some((0, "old notice")),
        );
        assert_eq!(
            hover,
            TitleRowDecision {
                label: "Waiting for you",
                notice_handle: Some(0),
            }
        );
        let notice = title_row_decision(
            PaneTitlesMode::Focused,
            2,
            "main",
            Some("focused"),
            &handles,
            None,
            Some((1, "Waiting for you")),
        );
        assert_eq!(
            notice,
            TitleRowDecision {
                label: "Waiting for you",
                notice_handle: Some(1),
            }
        );
        let focused = title_row_decision(
            PaneTitlesMode::Focused,
            2,
            "main",
            Some("focused"),
            &handles,
            None,
            None,
        );
        assert_eq!(
            focused,
            TitleRowDecision {
                label: "focused",
                notice_handle: None,
            }
        );
        let tab = title_row_decision(
            PaneTitlesMode::Focused,
            2,
            "main",
            None,
            &handles,
            None,
            None,
        );
        assert_eq!(
            tab,
            TitleRowDecision {
                label: "main",
                notice_handle: None,
            }
        );
    }

    #[test]
    fn hover_mode_keeps_single_pane_title_and_hides_multi_pane_focus() {
        let solo = title_row_decision(
            PaneTitlesMode::Hover,
            0,
            "main",
            Some("build"),
            &[],
            None,
            None,
        );
        assert_eq!(solo.label, "build");
        let handle_titles = titles(&["a", "b"]);
        let multi = title_row_decision(
            PaneTitlesMode::Hover,
            2,
            "main",
            Some("focused"),
            &handle_titles,
            None,
            None,
        );
        assert_eq!(multi.label, "main");
    }

    #[test]
    fn first_snapshot_is_silent_and_unfocused_change_wins() {
        let previous = vec![titles(&["pane 1", "pane 2"])];
        let current = vec![titles(&["pane 1", "Waiting for you"])];
        let notice = title_notice_from_diff(&previous, &current, &[Some(0)]).expect("notice");
        assert_eq!(
            notice,
            TitleNotice {
                tab: 0,
                handle: 1,
                title: "Waiting for you".into(),
            }
        );
        assert_eq!(
            title_notice_from_diff(&[], &current, &[Some(0)]),
            None,
            "bootstrap snapshot stays silent"
        );
        let focused_only = vec![titles(&["now focused", "pane 2"])];
        assert_eq!(
            title_notice_from_diff(&previous, &focused_only, &[Some(0)]),
            None,
            "focused handle changes are not notices"
        );
    }
}
