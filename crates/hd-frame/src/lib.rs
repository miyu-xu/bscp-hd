//! Strict zero-copy frame-broker validation and lifetime tracking.
//!
//! This crate deliberately has no pixel-buffer fallback API. A platform adapter must provide an
//! external GPU resource plus explicit synchronization or instance startup is rejected.

use std::collections::BTreeMap;
use std::sync::Arc;

use hd_core::{
    FRAME_PROTOCOL_VERSION, FrameDescriptorV2, FrameFormatV2, FrameMetricsV2, FrameTransportKindV2,
};
use hd_platform::FrameInteropProbeV2;
use parking_lot::Mutex;
use thiserror::Error;

pub const FRAME_BUFFER_COUNT: usize = 3;

pub trait ExternalFrameHandle: Send + Sync + std::fmt::Debug {
    fn transport(&self) -> FrameTransportKindV2;
    fn out_of_band_handle_count(&self) -> u8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferState {
    Available,
    ProducerOwned,
    ConsumerOwned,
}

#[derive(Debug)]
struct BufferSlot {
    descriptor: FrameDescriptorV2,
    handle: Arc<dyn ExternalFrameHandle>,
    state: BufferState,
    last_acquire_value: u64,
    last_release_value: u64,
}

#[derive(Debug)]
struct PoolState {
    generation: u64,
    slots: BTreeMap<u8, BufferSlot>,
    metrics: FrameMetricsV2,
}

#[derive(Debug)]
pub struct FramePoolV2 {
    state: Mutex<PoolState>,
}

impl FramePoolV2 {
    pub fn create(
        probe: &FrameInteropProbeV2,
        descriptors: Vec<(FrameDescriptorV2, Arc<dyn ExternalFrameHandle>)>,
    ) -> Result<Self, FrameError> {
        validate_probe(probe)?;
        if descriptors.len() != FRAME_BUFFER_COUNT {
            return Err(FrameError::BufferCount {
                expected: FRAME_BUFFER_COUNT,
                actual: descriptors.len(),
            });
        }
        let generation = descriptors
            .first()
            .map_or(0, |(descriptor, _)| descriptor.generation);
        if generation == 0 {
            return Err(FrameError::InvalidDescriptor(
                "generation must be non-zero".to_owned(),
            ));
        }
        let mut slots = BTreeMap::new();
        for (descriptor, handle) in descriptors {
            validate_descriptor(probe, generation, &descriptor, handle.as_ref())?;
            let id = descriptor.buffer_id;
            if slots
                .insert(
                    id,
                    BufferSlot {
                        last_acquire_value: descriptor.acquire_value,
                        last_release_value: descriptor.release_value,
                        descriptor,
                        handle,
                        state: BufferState::Available,
                    },
                )
                .is_some()
            {
                return Err(FrameError::InvalidDescriptor(format!(
                    "duplicate buffer_id {id}"
                )));
            }
        }
        Ok(Self {
            state: Mutex::new(PoolState {
                generation,
                slots,
                metrics: FrameMetricsV2 {
                    generation,
                    ..FrameMetricsV2::default()
                },
            }),
        })
    }

    pub fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    pub fn producer_acquire(&self, buffer_id: u8) -> Result<FrameDescriptorV2, FrameError> {
        let mut state = self.state.lock();
        let slot = state
            .slots
            .get_mut(&buffer_id)
            .ok_or(FrameError::UnknownBuffer(buffer_id))?;
        if slot.state != BufferState::Available {
            return Err(FrameError::InvalidOwnership {
                buffer_id,
                expected: "available",
            });
        }
        slot.state = BufferState::ProducerOwned;
        Ok(slot.descriptor.clone())
    }

    pub fn producer_publish(
        &self,
        buffer_id: u8,
        acquire_value: u64,
    ) -> Result<FrameDescriptorV2, FrameError> {
        let mut state = self.state.lock();
        let descriptor = {
            let slot = state
                .slots
                .get_mut(&buffer_id)
                .ok_or(FrameError::UnknownBuffer(buffer_id))?;
            if slot.state != BufferState::ProducerOwned {
                return Err(FrameError::InvalidOwnership {
                    buffer_id,
                    expected: "producer_owned",
                });
            }
            if acquire_value <= slot.last_acquire_value {
                return Err(FrameError::NonMonotonicSync {
                    buffer_id,
                    previous: slot.last_acquire_value,
                    next: acquire_value,
                });
            }
            slot.last_acquire_value = acquire_value;
            slot.descriptor.acquire_value = acquire_value;
            slot.state = BufferState::ConsumerOwned;
            slot.descriptor.clone()
        };
        state.metrics.produced_frames = state.metrics.produced_frames.saturating_add(1);
        Ok(descriptor)
    }

    pub fn consumer_imported(&self, buffer_id: u8) -> Result<(), FrameError> {
        let mut state = self.state.lock();
        let slot = state
            .slots
            .get(&buffer_id)
            .ok_or(FrameError::UnknownBuffer(buffer_id))?;
        if slot.state != BufferState::ConsumerOwned {
            return Err(FrameError::InvalidOwnership {
                buffer_id,
                expected: "consumer_owned",
            });
        }
        state.metrics.imported_frames = state.metrics.imported_frames.saturating_add(1);
        Ok(())
    }

    pub fn consumer_release(
        &self,
        buffer_id: u8,
        release_value: u64,
        presented: bool,
    ) -> Result<(), FrameError> {
        let mut state = self.state.lock();
        {
            let slot = state
                .slots
                .get_mut(&buffer_id)
                .ok_or(FrameError::UnknownBuffer(buffer_id))?;
            if slot.state != BufferState::ConsumerOwned {
                return Err(FrameError::InvalidOwnership {
                    buffer_id,
                    expected: "consumer_owned",
                });
            }
            if release_value <= slot.last_release_value {
                return Err(FrameError::NonMonotonicSync {
                    buffer_id,
                    previous: slot.last_release_value,
                    next: release_value,
                });
            }
            slot.last_release_value = release_value;
            slot.descriptor.release_value = release_value;
            slot.state = BufferState::Available;
        }
        if presented {
            state.metrics.presented_frames = state.metrics.presented_frames.saturating_add(1);
        } else {
            state.metrics.dropped_frames = state.metrics.dropped_frames.saturating_add(1);
        }
        Ok(())
    }

    pub fn metrics(&self) -> FrameMetricsV2 {
        self.state.lock().metrics.clone()
    }

    pub fn handle(&self, buffer_id: u8) -> Result<Arc<dyn ExternalFrameHandle>, FrameError> {
        self.state
            .lock()
            .slots
            .get(&buffer_id)
            .map(|slot| Arc::clone(&slot.handle))
            .ok_or(FrameError::UnknownBuffer(buffer_id))
    }
}

pub fn validate_probe(probe: &FrameInteropProbeV2) -> Result<(), FrameError> {
    if probe.component_protocol_version != hd_core::COMPONENT_PROTOCOL_VERSION
        || !probe.service_lifecycle
    {
        return Err(FrameError::InteropBlocked(
            "formal frame service lifecycle is unavailable",
        ));
    }
    if !probe.memory_export {
        return Err(FrameError::InteropBlocked("external memory unavailable"));
    }
    if !probe.explicit_sync {
        return Err(FrameError::InteropBlocked(
            "explicit synchronization unavailable",
        ));
    }
    if !probe.same_adapter {
        return Err(FrameError::InteropBlocked(
            "producer and consumer GPU differ",
        ));
    }
    if !probe.validation_clean {
        return Err(FrameError::InteropBlocked(
            "interop validation did not complete cleanly",
        ));
    }
    Ok(())
}

fn validate_descriptor(
    probe: &FrameInteropProbeV2,
    generation: u64,
    descriptor: &FrameDescriptorV2,
    handle: &dyn ExternalFrameHandle,
) -> Result<(), FrameError> {
    if descriptor.protocol_version != FRAME_PROTOCOL_VERSION {
        return Err(FrameError::InvalidDescriptor(format!(
            "frame protocol {} is unsupported",
            descriptor.protocol_version
        )));
    }
    if descriptor.generation != generation {
        return Err(FrameError::InvalidDescriptor(
            "all buffers must use one generation".to_owned(),
        ));
    }
    if descriptor.transport != probe.transport || handle.transport() != probe.transport {
        return Err(FrameError::InvalidDescriptor(
            "frame transport differs from the successful capability probe".to_owned(),
        ));
    }
    if descriptor.out_of_band_handle_count == 0
        || descriptor.out_of_band_handle_count != handle.out_of_band_handle_count()
    {
        return Err(FrameError::InvalidDescriptor(
            "out-of-band handle count is invalid".to_owned(),
        ));
    }
    if descriptor.format != FrameFormatV2::Bgra8Srgb {
        return Err(FrameError::InvalidDescriptor(
            "only BGRA8 sRGB is accepted by the V2 contract".to_owned(),
        ));
    }
    if descriptor.width < 320
        || descriptor.height < 320
        || descriptor.stride_bytes < descriptor.width.saturating_mul(4)
    {
        return Err(FrameError::InvalidDescriptor(
            "frame dimensions or stride are invalid".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("zero-copy interop is blocked: {0}")]
    InteropBlocked(&'static str),
    #[error("frame pool requires {expected} buffers, received {actual}")]
    BufferCount { expected: usize, actual: usize },
    #[error("invalid frame descriptor: {0}")]
    InvalidDescriptor(String),
    #[error("unknown frame buffer {0}")]
    UnknownBuffer(u8),
    #[error("buffer {buffer_id} has invalid ownership; expected {expected}")]
    InvalidOwnership {
        buffer_id: u8,
        expected: &'static str,
    },
    #[error("buffer {buffer_id} sync value is not monotonic: {previous} -> {next}")]
    NonMonotonicSync {
        buffer_id: u8,
        previous: u64,
        next: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[derive(Debug)]
    struct TestHandle;

    impl ExternalFrameHandle for TestHandle {
        fn transport(&self) -> FrameTransportKindV2 {
            FrameTransportKindV2::VulkanWin32
        }

        fn out_of_band_handle_count(&self) -> u8 {
            2
        }
    }

    #[test]
    fn pool_requires_monotonic_explicit_sync() {
        let probe = FrameInteropProbeV2 {
            component_protocol_version: hd_core::COMPONENT_PROTOCOL_VERSION,
            service_lifecycle: true,
            transport: FrameTransportKindV2::VulkanWin32,
            memory_export: true,
            explicit_sync: true,
            same_adapter: true,
            validation_clean: true,
            detail: String::new(),
            properties: BTreeMap::new(),
        };
        let descriptors = (0..3)
            .map(|buffer_id| {
                (
                    FrameDescriptorV2 {
                        protocol_version: FRAME_PROTOCOL_VERSION,
                        instance_id: Uuid::nil(),
                        generation: 1,
                        buffer_id,
                        transport: FrameTransportKindV2::VulkanWin32,
                        format: FrameFormatV2::Bgra8Srgb,
                        width: 1080,
                        height: 1920,
                        stride_bytes: 4320,
                        acquire_value: 0,
                        release_value: 0,
                        out_of_band_handle_count: 2,
                    },
                    Arc::new(TestHandle) as Arc<dyn ExternalFrameHandle>,
                )
            })
            .collect();
        let pool = FramePoolV2::create(&probe, descriptors).expect("create");
        pool.producer_acquire(0).expect("producer acquire");
        pool.producer_publish(0, 1).expect("publish");
        pool.consumer_imported(0).expect("import");
        pool.consumer_release(0, 1, true).expect("release");
        assert_eq!(pool.metrics().presented_frames, 1);
    }
}
