use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredStateV2 {
    Stopped,
    Running,
    Paused,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedStateV2 {
    Defined,
    Stopped,
    Preparing,
    StartingWorker,
    LaunchingGuest,
    NegotiatingDisplay,
    GuestBooting,
    AdbConnecting,
    Ready,
    Pausing,
    Paused,
    Resuming,
    Stopping,
    Recovering,
    Blocked,
    Failed,
    Deleting,
    Deleted,
}

impl ObservedStateV2 {
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::StartingWorker
                | Self::LaunchingGuest
                | Self::NegotiatingDisplay
                | Self::GuestBooting
                | Self::AdbConnecting
                | Self::Ready
                | Self::Pausing
                | Self::Paused
                | Self::Resuming
                | Self::Stopping
                | Self::Recovering
        )
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Stopped | Self::Blocked | Self::Failed | Self::Deleted
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use ObservedStateV2::{
            AdbConnecting, Blocked, Defined, Deleted, Deleting, Failed, GuestBooting,
            LaunchingGuest, NegotiatingDisplay, Paused, Pausing, Preparing, Ready, Recovering,
            Resuming, StartingWorker, Stopped, Stopping,
        };

        self == next
            || matches!(
                (self, next),
                (Defined, Stopped | Preparing | Deleting)
                    | (Stopped, Preparing | Deleting)
                    | (Preparing, StartingWorker | Stopping | Blocked | Failed)
                    | (StartingWorker, LaunchingGuest | Stopping | Blocked | Failed)
                    | (
                        LaunchingGuest,
                        NegotiatingDisplay | Stopping | Blocked | Failed
                    )
                    | (
                        NegotiatingDisplay,
                        GuestBooting | Stopping | Blocked | Failed
                    )
                    | (
                        GuestBooting,
                        AdbConnecting | Ready | Stopping | Blocked | Failed
                    )
                    | (AdbConnecting, Ready | Stopping | Blocked | Failed)
                    | (Ready, Pausing | Stopping | Recovering | Failed | Blocked)
                    | (
                        Pausing | Resuming,
                        Ready | Paused | Stopping | Recovering | Failed
                    )
                    | (Paused, Resuming | Stopping | Recovering | Failed)
                    | (Stopping, Stopped | Failed)
                    | (
                        Recovering,
                        Preparing | StartingWorker | Stopped | Blocked | Failed
                    )
                    | (
                        Blocked | Failed,
                        Preparing | Stopped | Deleting | Recovering
                    )
                    | (Deleting, Deleted | Failed)
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceStatusV2 {
    pub desired: DesiredStateV2,
    pub observed: ObservedStateV2,
    pub revision: u64,
    pub error_code: Option<String>,
    pub reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Default for InstanceStatusV2 {
    fn default() -> Self {
        Self {
            desired: DesiredStateV2::Stopped,
            observed: ObservedStateV2::Defined,
            revision: 0,
            error_code: None,
            reason: None,
            updated_at: OffsetDateTime::now_utc(),
        }
    }
}

impl InstanceStatusV2 {
    pub fn set_desired(&mut self, desired: DesiredStateV2) {
        if self.desired != desired {
            self.desired = desired;
            self.revision = self.revision.saturating_add(1);
            self.updated_at = OffsetDateTime::now_utc();
        }
    }

    pub fn transition(
        &mut self,
        next: ObservedStateV2,
        error_code: Option<String>,
        reason: Option<String>,
    ) -> Result<(), StateTransitionError> {
        if !self.observed.can_transition_to(next) {
            return Err(StateTransitionError {
                from: self.observed,
                to: next,
            });
        }
        self.observed = next;
        self.revision = self.revision.saturating_add(1);
        self.error_code = error_code;
        self.reason = reason;
        self.updated_at = OffsetDateTime::now_utc();
        Ok(())
    }

    pub fn force_reconciled(
        &mut self,
        observed: ObservedStateV2,
        error_code: Option<String>,
        reason: Option<String>,
    ) {
        self.observed = observed;
        self.revision = self.revision.saturating_add(1);
        self.error_code = error_code;
        self.reason = reason;
        self.updated_at = OffsetDateTime::now_utc();
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid observed-state transition from {from:?} to {to:?}")]
pub struct StateTransitionError {
    pub from: ObservedStateV2,
    pub to: ObservedStateV2,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_boot_path_reaches_ready() {
        let mut status = InstanceStatusV2::default();
        status.set_desired(DesiredStateV2::Running);
        for next in [
            ObservedStateV2::Preparing,
            ObservedStateV2::StartingWorker,
            ObservedStateV2::LaunchingGuest,
            ObservedStateV2::NegotiatingDisplay,
            ObservedStateV2::GuestBooting,
            ObservedStateV2::AdbConnecting,
            ObservedStateV2::Ready,
        ] {
            status
                .transition(next, None, None)
                .expect("valid transition");
        }
        assert_eq!(status.observed, ObservedStateV2::Ready);
    }

    #[test]
    fn readiness_cannot_be_forged_from_defined() {
        let mut status = InstanceStatusV2::default();
        assert!(
            status
                .transition(ObservedStateV2::Ready, None, None)
                .is_err()
        );
    }
}
