use anyhow::{Result, ensure};
use hd_core::{TrackpadEventV2, TrackpadPhaseV2};
use hd_ui::trackpad_queue::{TRACKPAD_QUEUE_CAPACITY, TrackpadQueue, TrackpadQueueError};
use serde_json::json;
use uuid::Uuid;

fn event(phase: TrackpadPhaseV2, x: u16, y: u16) -> TrackpadEventV2 {
    TrackpadEventV2 { phase, x, y }
}

fn main() -> Result<()> {
    let first_instance = Uuid::from_u128(1);
    let other_instance = Uuid::from_u128(2);
    let mut queue = TrackpadQueue::default();

    queue.enqueue(Some(first_instance), event(TrackpadPhaseV2::Down, 100, 200))?;
    let down = queue.pop_front().expect("accepted DOWN must be queued");
    ensure!(
        down.instance_id == first_instance && down.event.phase == TrackpadPhaseV2::Down,
        "DOWN was not bound to its selected instance"
    );

    queue.enqueue(None, event(TrackpadPhaseV2::Move, 300, 400))?;
    queue.enqueue(None, event(TrackpadPhaseV2::Move, 500, 600))?;
    ensure!(queue.len() == 1, "MOVE events were not coalesced");
    let latest_move = queue.pop_front().expect("latest MOVE must remain queued");
    ensure!(
        latest_move.instance_id == first_instance
            && latest_move.event == event(TrackpadPhaseV2::Move, 500, 600),
        "coalesced MOVE did not preserve the latest point and DOWN instance"
    );

    queue.enqueue(Some(other_instance), event(TrackpadPhaseV2::Up, 700, 800))?;
    let up = queue.pop_front().expect("UP must be queued");
    ensure!(
        up.instance_id == first_instance && up.event.phase == TrackpadPhaseV2::Up,
        "selection change redirected the active gesture release"
    );
    ensure!(
        queue.active_instance().is_none(),
        "UP did not end the active gesture"
    );

    queue.enqueue(
        Some(first_instance),
        event(TrackpadPhaseV2::Down, 1_000, 2_000),
    )?;
    queue.enqueue(None, event(TrackpadPhaseV2::Move, 3_000, 4_000))?;
    queue.enqueue(None, event(TrackpadPhaseV2::Up, 5_000, 6_000))?;
    ensure!(
        queue.len() == 2,
        "a pending MOVE superseded by UP was retained"
    );
    ensure!(
        queue.pop_front().is_some_and(|queued| {
            queued.event.phase == TrackpadPhaseV2::Down && queued.instance_id == first_instance
        }),
        "bounded tap sequence lost DOWN"
    );
    ensure!(
        queue.pop_front().is_some_and(|queued| {
            queued.event == event(TrackpadPhaseV2::Up, 5_000, 6_000)
                && queued.instance_id == first_instance
        }),
        "bounded tap sequence lost UP or its latest coordinates"
    );

    let mut saturated = TrackpadQueue::default();
    let mut accepted_gestures = 0_u32;
    loop {
        let down = saturated.enqueue(
            Some(first_instance),
            event(TrackpadPhaseV2::Down, 9_000, 9_000),
        );
        if down == Err(TrackpadQueueError::Saturated) {
            break;
        }
        down?;
        saturated.enqueue(None, event(TrackpadPhaseV2::Up, 9_000, 9_000))?;
        accepted_gestures += 1;
    }
    ensure!(accepted_gestures > 0, "queue rejected its first gesture");
    ensure!(
        saturated.len() <= TRACKPAD_QUEUE_CAPACITY,
        "queue exceeded its declared capacity"
    );
    ensure!(
        saturated.active_instance().is_none(),
        "saturation accepted a DOWN without reserving its UP"
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "gate": "trackpad-queue-smoke",
            "status": "pass",
            "capacity": TRACKPAD_QUEUE_CAPACITY,
            "accepted_buffered_gestures": accepted_gestures,
            "move_coalesced": true,
            "release_instance_pinned": true,
            "release_reserved": true,
        }))?
    );
    Ok(())
}
