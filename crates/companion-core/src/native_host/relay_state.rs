//! Relay lifecycle decisions, independent of readers, sockets, and credentials.
use super::NativeReconnectBackoff;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pairing,
    Connected,
    Reconnecting,
    Enrolling,
    Done,
}

pub(super) enum RelayEvent {
    Connected,
    EndpointChanged,
    Disconnected { has_credential: bool },
    AuthRefused { can_pair: bool },
    Paired,
    EnrollmentFinalized { keep_relay: bool },
    NativeClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelayAction {
    Continue,
    ReconnectAfter(Duration),
    FinalizeEnrollment,
    Stop,
    FailWebSocket,
    FailAuthentication,
    FailProtocol,
}

pub(super) struct RelayState {
    phase: Phase,
    enrollment_pending: bool,
    backoff: NativeReconnectBackoff,
}

impl RelayState {
    pub(super) fn new(enrollment_pending: bool) -> Self {
        Self {
            phase: Phase::Pairing,
            enrollment_pending,
            backoff: NativeReconnectBackoff::default(),
        }
    }

    pub(super) fn step(&mut self, event: RelayEvent) -> RelayAction {
        if self.phase == Phase::Done {
            return RelayAction::Stop;
        }
        match event {
            RelayEvent::Connected => {
                self.phase = Phase::Connected;
                self.backoff.reset();
                RelayAction::Continue
            }
            RelayEvent::EndpointChanged => {
                self.phase = Phase::Reconnecting;
                self.backoff.reset();
                RelayAction::ReconnectAfter(Duration::ZERO)
            }
            RelayEvent::Disconnected {
                has_credential: true,
            } => {
                self.phase = Phase::Reconnecting;
                RelayAction::ReconnectAfter(self.backoff.next_delay())
            }
            RelayEvent::Disconnected {
                has_credential: false,
            } => {
                self.phase = Phase::Done;
                RelayAction::FailWebSocket
            }
            RelayEvent::AuthRefused { can_pair: true } => {
                self.phase = Phase::Pairing;
                RelayAction::ReconnectAfter(Duration::ZERO)
            }
            RelayEvent::AuthRefused { can_pair: false } => {
                self.phase = Phase::Done;
                RelayAction::FailAuthentication
            }
            RelayEvent::Paired if self.phase == Phase::Connected => {
                if self.enrollment_pending {
                    self.phase = Phase::Enrolling;
                    RelayAction::FinalizeEnrollment
                } else {
                    RelayAction::Continue
                }
            }
            RelayEvent::EnrollmentFinalized { keep_relay } if self.phase == Phase::Enrolling => {
                self.enrollment_pending = false;
                self.phase = if keep_relay {
                    Phase::Connected
                } else {
                    Phase::Done
                };
                if keep_relay {
                    RelayAction::Continue
                } else {
                    RelayAction::Stop
                }
            }
            RelayEvent::NativeClosed => {
                self.phase = Phase::Done;
                RelayAction::Stop
            }
            _ => {
                self.phase = Phase::Done;
                RelayAction::FailProtocol
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_delays_reset_only_after_connection_or_endpoint_change() {
        let mut state = RelayState::new(false);
        for millis in [100, 200, 400] {
            assert_eq!(
                state.step(RelayEvent::Disconnected {
                    has_credential: true
                }),
                RelayAction::ReconnectAfter(Duration::from_millis(millis))
            );
        }
        assert_eq!(
            state.step(RelayEvent::EndpointChanged),
            RelayAction::ReconnectAfter(Duration::ZERO)
        );
        assert_eq!(
            state.step(RelayEvent::Disconnected {
                has_credential: true
            }),
            RelayAction::ReconnectAfter(Duration::from_millis(100))
        );
        state.step(RelayEvent::Connected);
        assert_eq!(
            state.step(RelayEvent::Disconnected {
                has_credential: true
            }),
            RelayAction::ReconnectAfter(Duration::from_millis(100))
        );
    }
    #[test]
    fn enrollment_finalizes_once_and_release_is_terminal() {
        for keep_relay in [false, true] {
            let mut state = RelayState::new(true);
            state.step(RelayEvent::Connected);
            assert_eq!(
                state.step(RelayEvent::Paired),
                RelayAction::FinalizeEnrollment
            );
            assert_eq!(
                state.step(RelayEvent::EnrollmentFinalized { keep_relay }),
                if keep_relay {
                    RelayAction::Continue
                } else {
                    RelayAction::Stop
                }
            );
            assert_eq!(
                state.step(RelayEvent::Paired),
                if keep_relay {
                    RelayAction::Continue
                } else {
                    RelayAction::Stop
                }
            );
        }
    }
    #[test]
    fn uncredentialed_loss_and_out_of_order_enrollment_fail_closed() {
        let mut state = RelayState::new(false);
        assert_eq!(
            state.step(RelayEvent::Disconnected {
                has_credential: false
            }),
            RelayAction::FailWebSocket
        );
        assert_eq!(state.step(RelayEvent::Connected), RelayAction::Stop);
        let mut state = RelayState::new(true);
        assert_eq!(
            state.step(RelayEvent::EnrollmentFinalized { keep_relay: true }),
            RelayAction::FailProtocol
        );
    }
}
