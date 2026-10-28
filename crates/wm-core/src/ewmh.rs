//! The backend-neutral half of the EWMH vocabulary: which
//! `_NET_WM_STATE` atom names which core state, and the guards every
//! backend puts in front of the untrusted numbers a client message
//! carries.
//!
//! Two legs of this desktop speak EWMH to X11 clients — the X11
//! session's own window manager and the XWayland arm of the compositor
//! — and before this module each kept its own copy, in step only by a
//! comment. They drifted: the X11 session never learned the pin and
//! attention states the XWayland arm gained, and went on advertising a
//! shade it did not accept. Nothing here needs x11rb; a backend interns
//! the names once and decodes against the result.

use crate::{NetState, NetStateAction, MAX_WORKSPACES};

/// The `_NET_WM_STATE` atoms a client may ask for, by name, and the
/// core state each one lands on. ABOVE and STICKY are the two halves of
/// the one pinned concept (see [`NetState::Pinned`]).
///
/// The whole vocabulary, not any one backend's policy: the X11 session
/// advertises and decodes every entry, while the XWayland arm leaves
/// out the ones smithay decodes for it or cannot reflect.
pub const REQUESTABLE_NET_WM_STATES: &[(&str, NetState)] = &[
    ("_NET_WM_STATE_FULLSCREEN", NetState::Fullscreen),
    ("_NET_WM_STATE_MAXIMIZED_HORZ", NetState::MaximizedHorz),
    ("_NET_WM_STATE_MAXIMIZED_VERT", NetState::MaximizedVert),
    ("_NET_WM_STATE_ABOVE", NetState::Pinned),
    ("_NET_WM_STATE_STICKY", NetState::Pinned),
    ("_NET_WM_STATE_DEMANDS_ATTENTION", NetState::DemandsAttention),
    ("_NET_WM_STATE_MODAL", NetState::Modal),
    ("_NET_WM_STATE_SHADED", NetState::Shaded),
];

/// `_NET_WM_DESKTOP`'s "on every desktop" value. On this desktop that
/// is pinned, the same state ABOVE and STICKY ask for.
pub const ALL_DESKTOPS: u32 = 0xFFFF_FFFF;

/// `_NET_WM_STATE`'s `data[0]`: 0 remove, 1 add, 2 toggle. Anything
/// else is a malformed message from an untrusted client.
pub const fn net_state_action(action: u32) -> Option<NetStateAction> {
    match action {
        0 => Some(NetStateAction::Remove),
        1 => Some(NetStateAction::Add),
        2 => Some(NetStateAction::Toggle),
        _ => None,
    }
}

/// Converts the untrusted CARDINAL carried by an EWMH desktop request
/// only when it names a workspace the core permits.
pub fn workspace_index_from_ewmh(desktop: u32) -> Option<usize> {
    let desktop = usize::try_from(desktop).ok()?;
    (desktop < MAX_WORKSPACES).then_some(desktop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewmh_workspace_indices_stop_at_the_core_ceiling() {
        assert_eq!(workspace_index_from_ewmh(0), Some(0));
        assert_eq!(workspace_index_from_ewmh((MAX_WORKSPACES - 1) as u32), Some(MAX_WORKSPACES - 1));
        assert_eq!(workspace_index_from_ewmh(MAX_WORKSPACES as u32), None);
        assert_eq!(workspace_index_from_ewmh(ALL_DESKTOPS), None, "all-desktops is a pin, never an index");
    }

    /// `data[0]` is untrusted: a client can put anything there, and
    /// only three values mean anything.
    #[test]
    fn a_net_wm_state_action_outside_the_spec_is_refused() {
        assert_eq!(net_state_action(0), Some(NetStateAction::Remove));
        assert_eq!(net_state_action(1), Some(NetStateAction::Add));
        assert_eq!(net_state_action(2), Some(NetStateAction::Toggle));
        assert_eq!(net_state_action(3), None);
        assert_eq!(net_state_action(u32::MAX), None);
    }

    /// A state with no name can never be asked for. The match is
    /// exhaustive on purpose: a new `NetState` does not compile here
    /// until someone decides what a client calls it.
    #[test]
    fn every_core_state_has_a_name_a_client_can_send() {
        let states = [
            NetState::Fullscreen,
            NetState::MaximizedHorz,
            NetState::MaximizedVert,
            NetState::Pinned,
            NetState::DemandsAttention,
            NetState::Modal,
            NetState::Shaded,
        ];
        for state in states {
            match state {
                NetState::Fullscreen
                | NetState::MaximizedHorz
                | NetState::MaximizedVert
                | NetState::Pinned
                | NetState::DemandsAttention
                | NetState::Modal
                | NetState::Shaded => {}
            }
            assert!(
                REQUESTABLE_NET_WM_STATES.iter().any(|&(_, named)| named == state),
                "{state:?} has no _NET_WM_STATE name"
            );
        }
    }

    #[test]
    fn the_state_names_are_distinct_net_wm_state_atoms() {
        for (index, &(name, _)) in REQUESTABLE_NET_WM_STATES.iter().enumerate() {
            assert!(name.starts_with("_NET_WM_STATE_"), "{name} is not a _NET_WM_STATE atom");
            assert!(
                REQUESTABLE_NET_WM_STATES[index + 1..].iter().all(|&(other, _)| other != name),
                "{name} is listed twice"
            );
        }
    }
}
