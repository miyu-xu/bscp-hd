use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use hd_core::{AdbModeV2, InstanceSpecV2, LeaseKindV2, LeaseOwnerV2, LeaseV2, WorkerIdentityV2};
use hd_platform::DataPaths;
use parking_lot::Mutex;
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{PersistentStore, StoreError};

const MIN_GUEST_CID: u32 = 3;
const MAX_GUEST_CID: u32 = i32::MAX as u32;
const GPU_INSTANCE_CAPACITY: u32 = 4;
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LeaseManager {
    store: PersistentStore,
    paths: DataPaths,
    audit: Arc<Mutex<std::fs::File>>,
    allocation: Arc<Mutex<()>>,
    gpu_slots: u32,
}

impl LeaseManager {
    pub fn new(store: PersistentStore, paths: DataPaths) -> Result<Self, LeaseError> {
        std::fs::create_dir_all(&paths.logs).map_err(|source| LeaseError::Io {
            operation: "create lease log directory",
            path: paths.logs.clone(),
            source,
        })?;
        let audit_path = paths.logs.join("lease-audit-v2.jsonl");
        let audit = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
            .map_err(|source| LeaseError::Io {
                operation: "open lease audit",
                path: audit_path,
                source,
            })?;
        Ok(Self {
            store,
            paths,
            audit: Arc::new(Mutex::new(audit)),
            allocation: Arc::new(Mutex::new(())),
            gpu_slots: GPU_INSTANCE_CAPACITY,
        })
    }

    pub fn reserve_start(
        &self,
        spec: &InstanceSpecV2,
        worker: Option<&WorkerIdentityV2>,
        frame_generation: u64,
    ) -> Result<Vec<LeaseV2>, LeaseError> {
        if frame_generation == 0 {
            return Err(LeaseError::FrameGenerationExhausted);
        }
        let _allocation = self.allocation.lock();
        let existing = self.store.list_leases()?;
        let occupied_cids = resources(&existing, LeaseKindV2::GuestCid);
        let occupied_gpu = resources(&existing, LeaseKindV2::GpuSlot);
        let occupied_adb = resources(&existing, LeaseKindV2::AdbPort);
        let capacity = validate_host_capacity(&existing, spec)?;
        let guest_cid = choose_guest_cid(spec.id, &occupied_cids)?;
        let gpu_slot = (0..self.gpu_slots)
            .map(|slot| slot.to_string())
            .find(|slot| !occupied_gpu.contains(slot))
            .ok_or(LeaseError::GpuCapacity(self.gpu_slots))?;
        let now = OffsetDateTime::now_utc();
        let owner = LeaseOwnerV2 {
            instance_id: spec.id,
            worker_nonce: worker.map(|identity| identity.nonce),
            pid: worker.map(|identity| identity.pid),
            process_start_marker: worker.map(|identity| identity.process_start_marker.clone()),
        };
        let mut leases = vec![
            lease(
                LeaseKindV2::CpuCapacity,
                scoped_quantity(spec.id, capacity.cpu),
                &owner,
                1,
                now,
            ),
            lease(
                LeaseKindV2::MemoryBytes,
                scoped_quantity(spec.id, capacity.memory),
                &owner,
                1,
                now,
            ),
            lease(LeaseKindV2::GuestCid, guest_cid.to_string(), &owner, 1, now),
            lease(LeaseKindV2::GpuSlot, gpu_slot, &owner, 1, now),
            lease(
                LeaseKindV2::DiskOverlay,
                self.paths.disk_overlay(spec.id).display().to_string(),
                &owner,
                1,
                now,
            ),
            lease(
                LeaseKindV2::WorkerEndpoint,
                worker_endpoint(spec.id)?,
                &owner,
                1,
                now,
            ),
            lease(
                LeaseKindV2::FrameGeneration,
                frame_generation.to_string(),
                &owner,
                frame_generation,
                now,
            ),
        ];
        if matches!(spec.adb.mode, AdbModeV2::Loopback) {
            let port = match spec.adb.host_port {
                Some(port) => {
                    if occupied_adb.contains(&port.to_string()) {
                        return Err(LeaseError::AdbPortUnavailable(port));
                    }
                    verify_loopback_port_available(port)?;
                    port
                }
                None => choose_adb_port(spec.id, &occupied_adb)?,
            };
            leases.push(lease(
                LeaseKindV2::AdbPort,
                port.to_string(),
                &owner,
                1,
                now,
            ));
        }
        for device in enabled_device_names(spec) {
            leases.push(lease(
                LeaseKindV2::DeviceEndpoint,
                format!("{}:{device}", spec.id),
                &owner,
                1,
                now,
            ));
        }
        self.store.put_leases(&leases)?;
        for value in &leases {
            self.audit("lease.acquired", value, BTreeMap::new())?;
        }
        Ok(leases)
    }

    pub fn bind_worker_identity(
        &self,
        instance_id: Uuid,
        identity: &WorkerIdentityV2,
    ) -> Result<Vec<LeaseV2>, LeaseError> {
        let now = OffsetDateTime::now_utc();
        let mut leases = self
            .store
            .list_leases()?
            .into_iter()
            .filter(|lease| lease.owner.instance_id == instance_id)
            .collect::<Vec<_>>();
        for lease in &mut leases {
            lease.owner.worker_nonce = Some(identity.nonce);
            lease.owner.pid = Some(identity.pid);
            lease.owner.process_start_marker = Some(identity.process_start_marker.clone());
            lease.last_verified_at = now;
        }
        self.store.put_leases(&leases)?;
        for lease in &leases {
            self.audit("lease.owner_bound", lease, BTreeMap::new())?;
        }
        Ok(leases)
    }

    pub fn commit_auto_adb_port(
        &self,
        instance_id: Uuid,
        identity: &WorkerIdentityV2,
        port: u16,
    ) -> Result<LeaseV2, LeaseError> {
        if port == 0 {
            return Err(LeaseError::InvalidPort(port));
        }
        let now = OffsetDateTime::now_utc();
        let lease = lease(
            LeaseKindV2::AdbPort,
            port.to_string(),
            &LeaseOwnerV2 {
                instance_id,
                worker_nonce: Some(identity.nonce),
                pid: Some(identity.pid),
                process_start_marker: Some(identity.process_start_marker.clone()),
            },
            1,
            now,
        );
        self.store.insert_lease(&lease)?;
        self.audit("lease.adb_committed", &lease, BTreeMap::new())?;
        Ok(lease)
    }

    pub fn verify_instance(&self, instance_id: Uuid) -> Result<Vec<LeaseV2>, LeaseError> {
        let now = OffsetDateTime::now_utc();
        let mut leases = self
            .store
            .list_leases()?
            .into_iter()
            .filter(|lease| lease.owner.instance_id == instance_id)
            .collect::<Vec<_>>();
        for lease in &mut leases {
            lease.last_verified_at = now;
        }
        self.store.put_leases(&leases)?;
        Ok(leases)
    }

    pub fn release_instance(&self, instance_id: Uuid) -> Result<Vec<LeaseV2>, LeaseError> {
        let removed = self.store.release_instance_leases(instance_id)?;
        for lease in &removed {
            self.audit("lease.released", lease, BTreeMap::new())?;
        }
        Ok(removed)
    }

    pub fn guest_cid(leases: &[LeaseV2]) -> Result<u32, LeaseError> {
        leases
            .iter()
            .find(|lease| lease.kind == LeaseKindV2::GuestCid)
            .ok_or(LeaseError::Missing(LeaseKindV2::GuestCid))?
            .resource
            .parse()
            .map_err(|_| LeaseError::Corrupt("guest CID is not numeric".to_owned()))
    }

    pub fn frame_generation(leases: &[LeaseV2]) -> Result<u64, LeaseError> {
        leases
            .iter()
            .find(|lease| lease.kind == LeaseKindV2::FrameGeneration)
            .ok_or(LeaseError::Missing(LeaseKindV2::FrameGeneration))?
            .resource
            .parse()
            .map_err(|_| LeaseError::Corrupt("frame generation is not numeric".to_owned()))
    }

    fn audit(
        &self,
        event: &str,
        lease: &LeaseV2,
        fields: BTreeMap<String, String>,
    ) -> Result<(), LeaseError> {
        #[derive(Serialize)]
        struct Audit<'a> {
            schema_version: u32,
            timestamp: OffsetDateTime,
            event: &'a str,
            lease: &'a LeaseV2,
            fields: BTreeMap<String, String>,
        }
        let mut bytes = serde_json::to_vec(&Audit {
            schema_version: 2,
            timestamp: OffsetDateTime::now_utc(),
            event,
            lease,
            fields,
        })
        .map_err(LeaseError::Encode)?;
        bytes.push(b'\n');
        let mut file = self.audit.lock();
        file.write_all(&bytes).map_err(|source| LeaseError::Io {
            operation: "append lease audit",
            path: self.paths.logs.join("lease-audit-v2.jsonl"),
            source,
        })?;
        file.flush().map_err(|source| LeaseError::Io {
            operation: "flush lease audit",
            path: self.paths.logs.join("lease-audit-v2.jsonl"),
            source,
        })
    }
}

fn choose_adb_port(instance_id: Uuid, occupied: &BTreeSet<String>) -> Result<u16, LeaseError> {
    const FIRST: u32 = 20_000;
    const LAST: u32 = 60_000;
    let span = u128::from(LAST - FIRST + 1);
    let offset = u32::try_from(instance_id.as_u128() % span)
        .map_err(|error| LeaseError::Corrupt(format!("derive ADB port: {error}")))?;
    for attempt in 0..=(LAST - FIRST) {
        let candidate = u16::try_from(FIRST + (offset + attempt) % (LAST - FIRST + 1))
            .map_err(|error| LeaseError::Corrupt(format!("derive ADB port: {error}")))?;
        if !occupied.contains(&candidate.to_string())
            && verify_loopback_port_available(candidate).is_ok()
        {
            return Ok(candidate);
        }
    }
    Err(LeaseError::AdbPortExhausted)
}

fn verify_loopback_port_available(port: u16) -> Result<(), LeaseError> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .map(|_| ())
        .map_err(|source| LeaseError::PortIo { port, source })
}

pub fn worker_endpoint(instance_id: Uuid) -> Result<String, LeaseError> {
    #[cfg(windows)]
    {
        let scope = hd_platform::current_user_scope()?;
        Ok(format!(r"\\.\pipe\bscp-hd-worker-{scope}-{instance_id}"))
    }
    #[cfg(unix)]
    {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Ok(runtime
            .join(format!(
                "bscp-hd-worker-{}-{instance_id}.sock",
                hd_platform::current_user_scope()?
            ))
            .to_string_lossy()
            .into_owned())
    }
}

fn lease(
    kind: LeaseKindV2,
    resource: String,
    owner: &LeaseOwnerV2,
    generation: u64,
    now: OffsetDateTime,
) -> LeaseV2 {
    LeaseV2 {
        id: Uuid::new_v4(),
        kind,
        resource,
        owner: owner.clone(),
        generation,
        acquired_at: now,
        last_verified_at: now,
    }
}

fn resources(leases: &[LeaseV2], kind: LeaseKindV2) -> BTreeSet<String> {
    leases
        .iter()
        .filter(|lease| lease.kind == kind)
        .map(|lease| lease.resource.clone())
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct CapacityReservation {
    cpu: u64,
    memory: u64,
}

fn validate_host_capacity(
    existing: &[LeaseV2],
    spec: &InstanceSpecV2,
) -> Result<CapacityReservation, LeaseError> {
    let host = hd_platform::host_resources()?;
    let reserved_cpu = leased_quantity(existing, LeaseKindV2::CpuCapacity)?;
    let requested_cpu = u64::from(spec.cpu_count);
    let cpu_capacity = u64::try_from(host.logical_cpus)
        .map_err(|_| LeaseError::Corrupt("logical CPU capacity overflows u64".to_owned()))?;
    if reserved_cpu.saturating_add(requested_cpu) > cpu_capacity {
        return Err(LeaseError::CpuCapacity {
            requested: requested_cpu,
            available: cpu_capacity.saturating_sub(reserved_cpu),
        });
    }
    let host_reserve = (host.total_memory_bytes / 10).max(GIB);
    let memory_capacity = host.total_memory_bytes.saturating_sub(host_reserve);
    let reserved_memory = leased_quantity(existing, LeaseKindV2::MemoryBytes)?;
    let requested_memory = u64::from(spec.memory_mib).saturating_mul(1024 * 1024);
    if reserved_memory.saturating_add(requested_memory) > memory_capacity {
        return Err(LeaseError::MemoryCapacity {
            requested: requested_memory,
            available: memory_capacity.saturating_sub(reserved_memory),
        });
    }
    Ok(CapacityReservation {
        cpu: requested_cpu,
        memory: requested_memory,
    })
}

fn scoped_quantity(instance_id: Uuid, quantity: u64) -> String {
    format!("{instance_id}:{quantity}")
}

fn leased_quantity(leases: &[LeaseV2], kind: LeaseKindV2) -> Result<u64, LeaseError> {
    leases
        .iter()
        .filter(|lease| lease.kind == kind)
        .try_fold(0_u64, |total, lease| {
            let (scope, quantity) = lease
                .resource
                .rsplit_once(':')
                .ok_or_else(|| LeaseError::Corrupt(format!("{kind:?} lease has no quantity")))?;
            if scope != lease.owner.instance_id.to_string() {
                return Err(LeaseError::Corrupt(format!(
                    "{kind:?} lease scope does not match its owner"
                )));
            }
            let quantity = quantity.parse::<u64>().map_err(|_| {
                LeaseError::Corrupt(format!("{kind:?} lease quantity is not numeric"))
            })?;
            total
                .checked_add(quantity)
                .ok_or_else(|| LeaseError::Corrupt(format!("{kind:?} lease total overflows")))
        })
}

fn choose_guest_cid(instance_id: Uuid, occupied: &BTreeSet<String>) -> Result<u32, LeaseError> {
    let range = u128::from(MAX_GUEST_CID - MIN_GUEST_CID + 1);
    let offset = u32::try_from(instance_id.as_u128() % range)
        .map_err(|error| LeaseError::Corrupt(format!("derive guest CID: {error}")))?;
    let first = MIN_GUEST_CID + offset;
    for attempt in 0..4096_u32 {
        let candidate =
            MIN_GUEST_CID + (first - MIN_GUEST_CID + attempt) % (MAX_GUEST_CID - MIN_GUEST_CID + 1);
        if !occupied.contains(&candidate.to_string()) {
            return Ok(candidate);
        }
    }
    Err(LeaseError::GuestCidExhausted)
}

pub(crate) fn enabled_device_names(spec: &InstanceSpecV2) -> Vec<&'static str> {
    let mut names = Vec::new();
    for (enabled, name) in [
        (spec.devices.bluetooth, "bluetooth"),
        (spec.devices.nfc, "nfc"),
        (spec.devices.uwb, "uwb"),
        (spec.devices.modem, "modem"),
        (spec.devices.gnss, "gnss"),
        (spec.devices.sensors, "sensors"),
        (spec.devices.network, "network"),
        (spec.devices.audio, "audio"),
        (spec.devices.camera, "camera"),
        (spec.devices.power, "power"),
    ] {
        if enabled {
            names.push(name);
        }
    }
    names
}

#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("CPU lease requires {requested} cores but only {available} remain")]
    CpuCapacity { requested: u64, available: u64 },
    #[error("memory lease requires {requested} bytes but only {available} remain")]
    MemoryCapacity { requested: u64, available: u64 },
    #[error("no GPU slot is available from the configured capacity {0}")]
    GpuCapacity(u32),
    #[error("guest CID search exhausted")]
    GuestCidExhausted,
    #[error("frame generation is exhausted")]
    FrameGenerationExhausted,
    #[error("invalid ADB port {0}")]
    InvalidPort(u16),
    #[error("ADB port {0} is already leased or bound")]
    AdbPortUnavailable(u16),
    #[error("no loopback ADB port is available")]
    AdbPortExhausted,
    #[error("cannot reserve loopback ADB port {port}: {source}")]
    PortIo {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("required lease is missing: {0:?}")]
    Missing(LeaseKindV2),
    #[error("lease record is corrupt: {0}")]
    Corrupt(String),
    #[error("failed to encode lease audit: {0}")]
    Encode(serde_json::Error),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

impl LeaseError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::CpuCapacity { .. } => "lease_cpu_capacity",
            Self::MemoryCapacity { .. } => "lease_memory_capacity",
            Self::GpuCapacity(_) => "lease_gpu_capacity",
            Self::GuestCidExhausted => "lease_guest_cid_exhausted",
            Self::FrameGenerationExhausted => "lease_frame_generation_exhausted",
            Self::InvalidPort(_) => "lease_invalid_port",
            Self::AdbPortUnavailable(_) => "lease_adb_port_unavailable",
            Self::AdbPortExhausted => "lease_adb_port_exhausted",
            Self::PortIo { .. } => "lease_adb_port_io",
            Self::Missing(_) => "lease_missing",
            Self::Corrupt(_) => "lease_corrupt",
            Self::Encode(_) => "lease_audit_encode",
            Self::Io { .. } => "lease_io",
            Self::Store(_) => "lease_store",
            Self::Platform(_) => "lease_platform",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_cid_selection_resolves_collisions() {
        let id = Uuid::nil();
        let first = choose_guest_cid(id, &BTreeSet::new()).expect("first");
        let occupied = BTreeSet::from([first.to_string()]);
        let second = choose_guest_cid(id, &occupied).expect("second");
        assert_ne!(first, second);
    }

    #[test]
    fn frame_generation_lease_is_scoped_to_instance() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let first_owner = LeaseOwnerV2 {
            instance_id: Uuid::new_v4(),
            worker_nonce: None,
            pid: None,
            process_start_marker: None,
        };
        let second_owner = LeaseOwnerV2 {
            instance_id: Uuid::new_v4(),
            worker_nonce: None,
            pid: None,
            process_start_marker: None,
        };
        let first = lease(
            LeaseKindV2::FrameGeneration,
            "1".to_owned(),
            &first_owner,
            1,
            now,
        );
        let second = lease(
            LeaseKindV2::FrameGeneration,
            "1".to_owned(),
            &second_owner,
            1,
            now,
        );
        assert_ne!(
            crate::store::lease_key(&first),
            crate::store::lease_key(&second)
        );
    }
}
