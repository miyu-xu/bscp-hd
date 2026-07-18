use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Defined,
    Preparing,
    Launching,
    DisplayAttached,
    GuestBooting,
    AdbConnecting,
    Ready,
    Stopping,
    Stopped,
    Failed,
    Blocked,
}

impl InstanceState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed | Self::Blocked)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use InstanceState::{
            AdbConnecting, Blocked, Defined, DisplayAttached, Failed, GuestBooting, Launching,
            Preparing, Ready, Stopped, Stopping,
        };

        self == next
            || matches!(
                (self, next),
                (Defined | Stopped, Preparing)
                    | (Preparing, Launching)
                    | (Launching, DisplayAttached)
                    | (DisplayAttached, GuestBooting)
                    | (GuestBooting, AdbConnecting | Ready)
                    | (AdbConnecting, Ready)
                    | (
                        Preparing
                            | Launching
                            | DisplayAttached
                            | GuestBooting
                            | AdbConnecting
                            | Ready,
                        Stopping
                    )
                    | (Stopping, Stopped)
                    | (
                        Preparing
                            | Launching
                            | DisplayAttached
                            | GuestBooting
                            | AdbConnecting
                            | Ready
                            | Stopping,
                        Failed | Blocked
                    )
                    | (Failed | Blocked, Preparing | Stopped)
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub state: InstanceState,
    pub revision: u64,
    pub reason: Option<String>,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            state: InstanceState::Defined,
            revision: 0,
            reason: None,
        }
    }
}

impl StateSnapshot {
    pub fn transition(
        &mut self,
        next: InstanceState,
        reason: Option<String>,
    ) -> Result<(), StateTransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(StateTransitionError {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.revision = self.revision.saturating_add(1);
        self.reason = reason;
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid state transition from {from:?} to {to:?}")]
pub struct StateTransitionError {
    pub from: InstanceState,
    pub to: InstanceState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_ready() {
        let mut state = StateSnapshot::default();
        for next in [
            InstanceState::Preparing,
            InstanceState::Launching,
            InstanceState::DisplayAttached,
            InstanceState::GuestBooting,
            InstanceState::AdbConnecting,
            InstanceState::Ready,
        ] {
            state.transition(next, None).expect("valid transition");
        }
        assert_eq!(state.state, InstanceState::Ready);
        assert_eq!(state.revision, 6);
    }

    #[test]
    fn skips_are_rejected() {
        let mut state = StateSnapshot::default();
        assert!(state.transition(InstanceState::Ready, None).is_err());
    }
}
