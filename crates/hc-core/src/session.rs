//! Session state machine.
//!
//! The GUI and CLI are thin projections of this state. Transitions are validated by
//! [`SessionState::next`]; an invalid `(state, event)` pair returns `None` so callers
//! can treat illegal transitions as bugs rather than silently misbehaving.

/// Why a session entered the [`SessionState::Error`] state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorReason {
    /// An OS permission (e.g. Screen Recording) was not granted.
    PermissionDenied,
    /// The chosen device became unreachable.
    DeviceUnreachable,
    /// A protocol-level failure (handshake, stream).
    ProtocolFailed,
    /// The network changed underneath us (e.g. Wi-Fi switch).
    NetworkChanged,
}

/// High-level state of a single cast session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Nothing happening.
    Idle,
    /// Scanning the network for devices.
    Discovering,
    /// A device has been chosen.
    DeviceSelected,
    /// Negotiating capabilities/quality with the device.
    Negotiating,
    /// Establishing the transport session.
    Connecting,
    /// Actively streaming.
    Streaming,
    /// Streaming but degraded (reduced quality under congestion).
    Degraded,
    /// Lost connection; attempting to reconnect.
    Reconnecting,
    /// Tearing down.
    Stopping,
    /// Terminal error with a reason.
    Error(ErrorReason),
}

/// Events that drive [`SessionState`] transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Begin discovery.
    StartDiscovery,
    /// User picked a device.
    DeviceChosen,
    /// Begin capability negotiation.
    BeginNegotiate,
    /// Capabilities negotiated; ready to connect.
    CapsNegotiated,
    /// Transport session established and streaming started.
    Connected,
    /// Network congestion detected.
    Congested,
    /// Congestion cleared.
    Recovered,
    /// Connection lost mid-stream.
    ConnectionLost,
    /// User requested stop.
    Stop,
    /// Teardown complete.
    Stopped,
    /// A failure occurred.
    Failed(ErrorReason),
}

impl SessionState {
    /// Returns the next state for `event`, or `None` if the transition is invalid from
    /// the current state. `Stop` and `Failed` are accepted from any active state.
    #[must_use]
    pub fn next(&self, event: &Event) -> Option<SessionState> {
        use Event as E;
        use SessionState as S;

        let next = match (self, event) {
            (S::Idle, E::StartDiscovery) => S::Discovering,
            (S::Discovering, E::DeviceChosen) => S::DeviceSelected,
            (S::DeviceSelected, E::BeginNegotiate) => S::Negotiating,
            (S::Negotiating, E::CapsNegotiated) => S::Connecting,
            (S::Connecting | S::Reconnecting, E::Connected) => S::Streaming,
            (S::Streaming, E::Congested) => S::Degraded,
            (S::Degraded, E::Recovered) => S::Streaming,
            (S::Streaming | S::Degraded, E::ConnectionLost) => S::Reconnecting,
            (S::Stopping, E::Stopped) => S::Idle,
            // Stop and Failed are valid from any active (non-idle) state.
            (state, E::Stop) if !matches!(state, S::Idle) => S::Stopping,
            (_, E::Failed(reason)) => S::Error(reason.clone()),
            _ => return None,
        };
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(start: SessionState, events: &[Event]) -> Option<SessionState> {
        let mut state = start;
        for ev in events {
            state = state.next(ev)?;
        }
        Some(state)
    }

    #[test]
    fn happy_path_reaches_streaming() {
        let end = drive(
            SessionState::Idle,
            &[
                Event::StartDiscovery,
                Event::DeviceChosen,
                Event::BeginNegotiate,
                Event::CapsNegotiated,
                Event::Connected,
            ],
        );
        assert_eq!(end, Some(SessionState::Streaming));
    }

    #[test]
    fn congestion_round_trip() {
        assert_eq!(
            SessionState::Streaming.next(&Event::Congested),
            Some(SessionState::Degraded)
        );
        assert_eq!(
            SessionState::Degraded.next(&Event::Recovered),
            Some(SessionState::Streaming)
        );
    }

    #[test]
    fn reconnect_path() {
        assert_eq!(
            SessionState::Streaming.next(&Event::ConnectionLost),
            Some(SessionState::Reconnecting)
        );
        assert_eq!(
            SessionState::Reconnecting.next(&Event::Connected),
            Some(SessionState::Streaming)
        );
    }

    #[test]
    fn invalid_transition_is_none() {
        assert!(SessionState::Idle.next(&Event::Connected).is_none());
        assert!(SessionState::Idle.next(&Event::Stop).is_none());
    }

    #[test]
    fn failure_from_any_active_state() {
        assert_eq!(
            SessionState::Streaming.next(&Event::Failed(ErrorReason::DeviceUnreachable)),
            Some(SessionState::Error(ErrorReason::DeviceUnreachable))
        );
    }
}
