use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use hd_core::{TrackpadEventV2, TrackpadPhaseV2};
use uuid::Uuid;

/// Maximum number of Host-bound trackpad events retained while the Host is busy.
///
/// A newly accepted DOWN reserves enough capacity for its latest MOVE and mandatory UP. This
/// makes the queue strictly bounded without ever accepting a gesture whose release could be
/// dropped later.
pub const TRACKPAD_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedTrackpadEvent {
    pub instance_id: Uuid,
    pub event: TrackpadEventV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackpadQueueError {
    MissingDownInstance,
    GestureAlreadyActive,
    NoActiveGesture,
    Saturated,
}

impl Display for TrackpadQueueError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingDownInstance => "trackpad DOWN is missing its instance",
            Self::GestureAlreadyActive => "a trackpad gesture is already active",
            Self::NoActiveGesture => "trackpad MOVE or UP has no active gesture",
            Self::Saturated => "trackpad input queue is saturated",
        })
    }
}

impl Error for TrackpadQueueError {}

/// Bounded single-pointer queue used between the `WebView` and asynchronous Host client.
///
/// MOVE events are coalesced in place. MOVE and UP are bound to the instance captured by DOWN,
/// so a selection change cannot redirect or lose a release event.
#[derive(Debug, Default)]
pub struct TrackpadQueue {
    events: VecDeque<QueuedTrackpadEvent>,
    active_instance: Option<Uuid>,
}

impl TrackpadQueue {
    pub fn enqueue(
        &mut self,
        down_instance: Option<Uuid>,
        event: TrackpadEventV2,
    ) -> Result<(), TrackpadQueueError> {
        match event.phase {
            TrackpadPhaseV2::Down => self.enqueue_down(down_instance, event),
            TrackpadPhaseV2::Move => self.enqueue_move(event),
            TrackpadPhaseV2::Up => self.enqueue_up(event),
        }
    }

    pub fn pop_front(&mut self) -> Option<QueuedTrackpadEvent> {
        self.events.pop_front()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub const fn active_instance(&self) -> Option<Uuid> {
        self.active_instance
    }

    fn enqueue_down(
        &mut self,
        down_instance: Option<Uuid>,
        event: TrackpadEventV2,
    ) -> Result<(), TrackpadQueueError> {
        if self.active_instance.is_some() {
            return Err(TrackpadQueueError::GestureAlreadyActive);
        }
        let instance_id = down_instance.ok_or(TrackpadQueueError::MissingDownInstance)?;
        if self.events.len().saturating_add(3) > TRACKPAD_QUEUE_CAPACITY {
            return Err(TrackpadQueueError::Saturated);
        }
        self.events
            .push_back(QueuedTrackpadEvent { instance_id, event });
        self.active_instance = Some(instance_id);
        Ok(())
    }

    fn enqueue_move(&mut self, event: TrackpadEventV2) -> Result<(), TrackpadQueueError> {
        let instance_id = self
            .active_instance
            .ok_or(TrackpadQueueError::NoActiveGesture)?;
        if let Some(queued) = self.events.back_mut()
            && queued.instance_id == instance_id
            && queued.event.phase == TrackpadPhaseV2::Move
        {
            queued.event = event;
            return Ok(());
        }
        // DOWN admission reserved this slot plus the mandatory UP slot.
        if self.events.len().saturating_add(2) > TRACKPAD_QUEUE_CAPACITY {
            return Err(TrackpadQueueError::Saturated);
        }
        self.events
            .push_back(QueuedTrackpadEvent { instance_id, event });
        Ok(())
    }

    fn enqueue_up(&mut self, event: TrackpadEventV2) -> Result<(), TrackpadQueueError> {
        let instance_id = self
            .active_instance
            .ok_or(TrackpadQueueError::NoActiveGesture)?;
        // UP already carries the latest absolute coordinates. A MOVE that has not reached the
        // Host yet is therefore redundant and removing it bounds release latency.
        if self.events.back().is_some_and(|queued| {
            queued.instance_id == instance_id && queued.event.phase == TrackpadPhaseV2::Move
        }) {
            self.events.pop_back();
        }
        if self.events.len() >= TRACKPAD_QUEUE_CAPACITY {
            return Err(TrackpadQueueError::Saturated);
        }
        self.events
            .push_back(QueuedTrackpadEvent { instance_id, event });
        self.active_instance = None;
        Ok(())
    }
}
