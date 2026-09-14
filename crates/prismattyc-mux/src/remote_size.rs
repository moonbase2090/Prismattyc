//! Pane-size arbitration for shared attach (PT-200).
//!
//! The most recently active client sets the pane size (tmux `window-size
//! latest`). `--fit` always wins. On a remote detach, restore the last
//! host geometry so the desktop window is not left small.

use serde::{Deserialize, Serialize};

/// `[mux] remote_size` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteSizePolicy {
    /// Last active client wins (default).
    #[default]
    Latest,
    /// Only the host (or `--fit`) sets the size.
    Host,
}

/// Who reported a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRole {
    Host,
    Remote,
}

/// Who last set the pane size (PT-202). Additive on snapshots and log frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeOwnerKind {
    Host,
    Remote,
}

/// Control-plane client that currently owns the window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeOwner {
    pub client_id: u64,
    pub kind: SizeOwnerKind,
}

/// One client's reported window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub cols: u32,
    pub rows: u32,
    pub cell_width_px: Option<u32>,
    pub cell_height_px: Option<u32>,
}

/// Choose the window size after a `Resize`.
///
/// `None` means leave the current bounds unchanged. A report that
/// matches the applied cells is a no-op, including `--fit`.
pub fn resize_decision(
    policy: RemoteSizePolicy,
    role: ClientRole,
    size: Viewport,
    fit: bool,
    host: Option<Viewport>,
    current: Option<(u32, u32)>,
) -> Option<Viewport> {
    let chosen = if fit {
        Some(size)
    } else {
        match policy {
            RemoteSizePolicy::Latest => Some(size),
            RemoteSizePolicy::Host => {
                if role == ClientRole::Host {
                    Some(size)
                } else {
                    host
                }
            }
        }
    };
    let view = chosen?;
    if current.is_some_and(|cur| cur == (view.cols, view.rows)) {
        None
    } else {
        Some(view)
    }
}

/// Choose the window size after a remote client disconnects.
///
/// Prefer the last host geometry so the desktop is not left at the
/// remote terminal size.
pub fn disconnect_decision(
    host: Option<Viewport>,
    remaining: Option<Viewport>,
) -> Option<Viewport> {
    host.or(remaining)
}

/// Show a `remote WxH` chip when someone other than this client owns the size.
///
/// The server is the source of truth (PT-202). Do not infer from replica vs
/// last host request: an echo of a clamped size would stick the chip.
pub fn remote_size_chip(
    owner: Option<SizeOwner>,
    self_client_id: Option<u64>,
    replica: (u32, u32),
) -> Option<(u32, u32)> {
    let owner = owner?;
    let other = match self_client_id {
        Some(id) => owner.client_id != id,
        None => owner.kind == SizeOwnerKind::Remote,
    };
    other.then_some(replica)
}

/// Keep the last host-chosen cells when a later request only echoes the
/// applied replica. The chip compares applied vs this value.
pub fn remember_host_size(
    previous: Option<(u32, u32)>,
    requested: (u32, u32),
    replica: (u32, u32),
) -> Option<(u32, u32)> {
    if previous.is_some() && requested == replica && previous != Some(requested) {
        previous
    } else {
        Some(requested)
    }
}

/// A host-tagged `Resize` updates the stored host-chosen size unless a
/// remote client is latest and the report only echoes the applied size.
pub fn record_host_chosen(
    reported: (u32, u32),
    applied: Option<(u32, u32)>,
    latest_is_remote: bool,
) -> bool {
    !latest_is_remote || applied != Some(reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(cols: u32, rows: u32) -> Viewport {
        Viewport {
            cols,
            rows,
            cell_width_px: None,
            cell_height_px: None,
        }
    }

    #[test]
    fn latest_applies_the_active_client() {
        let host = vp(80, 24);
        let remote = vp(40, 20);
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Latest,
                ClientRole::Remote,
                remote,
                false,
                Some(host),
                Some((host.cols, host.rows))
            ),
            Some(remote)
        );
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Latest,
                ClientRole::Host,
                host,
                false,
                Some(host),
                Some((remote.cols, remote.rows))
            ),
            Some(host)
        );
    }

    #[test]
    fn host_policy_ignores_remote_unless_fit() {
        let host = vp(80, 24);
        let remote = vp(40, 20);
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Host,
                ClientRole::Remote,
                remote,
                false,
                Some(host),
                Some((remote.cols, remote.rows))
            ),
            Some(host)
        );
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Host,
                ClientRole::Remote,
                remote,
                true,
                Some(host),
                Some((host.cols, host.rows))
            ),
            Some(remote)
        );
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Host,
                ClientRole::Host,
                host,
                false,
                Some(host),
                Some((remote.cols, remote.rows))
            ),
            Some(host)
        );
    }

    #[test]
    fn fit_wins_on_both_policies() {
        let small = vp(40, 12);
        for policy in [RemoteSizePolicy::Latest, RemoteSizePolicy::Host] {
            assert_eq!(
                resize_decision(
                    policy,
                    ClientRole::Remote,
                    small,
                    true,
                    Some(vp(80, 24)),
                    Some((80, 24)),
                ),
                Some(small)
            );
        }
    }

    #[test]
    fn disconnect_restores_host_before_remaining() {
        assert_eq!(
            disconnect_decision(Some(vp(80, 24)), Some(vp(40, 20))),
            Some(vp(80, 24))
        );
        assert_eq!(
            disconnect_decision(None, Some(vp(40, 20))),
            Some(vp(40, 20))
        );
        assert_eq!(disconnect_decision(None, None), None);
    }

    #[test]
    fn same_size_report_is_a_noop() {
        let remote = vp(40, 20);
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Latest,
                ClientRole::Remote,
                remote,
                false,
                Some(vp(80, 24)),
                Some((40, 20))
            ),
            None
        );
        assert_eq!(
            resize_decision(
                RemoteSizePolicy::Latest,
                ClientRole::Remote,
                remote,
                true,
                Some(vp(80, 24)),
                Some((40, 20))
            ),
            None,
            "fit of the applied size is still a no-op"
        );
    }

    fn remote(id: u64) -> SizeOwner {
        SizeOwner {
            client_id: id,
            kind: SizeOwnerKind::Remote,
        }
    }

    fn host(id: u64) -> SizeOwner {
        SizeOwner {
            client_id: id,
            kind: SizeOwnerKind::Host,
        }
    }

    #[test]
    fn remote_chip_only_when_another_client_owns_size() {
        assert_eq!(remote_size_chip(None, Some(1), (40, 20)), None);
        assert_eq!(
            remote_size_chip(Some(host(1)), Some(1), (40, 20)),
            None,
            "own host ownership never chips, even if replica differs"
        );
        assert_eq!(remote_size_chip(Some(host(1)), Some(1), (40, 20)), None);
        assert_eq!(
            remote_size_chip(Some(remote(2)), Some(1), (40, 20)),
            Some((40, 20))
        );
        assert_eq!(
            remote_size_chip(Some(remote(2)), None, (40, 20)),
            Some((40, 20))
        );
    }

    #[test]
    fn echo_request_never_produces_a_chip() {
        assert_eq!(
            remote_size_chip(Some(host(7)), Some(7), (99, 48)),
            None,
            "host echo of a clamped replica must not show remote WxH"
        );
    }

    #[test]
    fn remember_host_size_keeps_host_when_request_echoes_replica() {
        assert_eq!(remember_host_size(None, (80, 24), (80, 24)), Some((80, 24)));
        assert_eq!(
            remember_host_size(Some((80, 24)), (40, 20), (40, 20)),
            Some((80, 24)),
            "echo of the remote-applied size must not clobber host-chosen"
        );
        assert_eq!(
            remember_host_size(Some((80, 24)), (80, 24), (40, 20)),
            Some((80, 24))
        );
        assert_eq!(
            remember_host_size(Some((80, 24)), (100, 30), (40, 20)),
            Some((100, 30)),
            "a real host window change replaces host-chosen"
        );
    }

    #[test]
    fn record_host_chosen_rejects_echo_while_remote_is_latest() {
        assert!(record_host_chosen((80, 24), Some((80, 24)), false));
        assert!(record_host_chosen((80, 24), Some((40, 20)), true));
        assert!(
            !record_host_chosen((40, 20), Some((40, 20)), true),
            "host must not store the remote-applied size as host-chosen"
        );
    }

    #[test]
    fn chip_shows_while_remote_owns_and_clears_when_host_owns() {
        assert_eq!(
            remote_size_chip(Some(remote(2)), Some(1), (40, 20)),
            Some((40, 20))
        );
        assert_eq!(
            remote_size_chip(Some(host(1)), Some(1), (80, 24)),
            None,
            "disconnect restore that returns ownership to the host hides the chip"
        );
    }
}
