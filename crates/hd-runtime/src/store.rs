use std::path::{Path, PathBuf};
use std::sync::Arc;

use hd_core::{
    INSTANCE_CONFIG_VERSION, InstanceRecordV2, InstanceSpecV2, LeaseV2, MigrationRecordV2,
    OperationKindV2, OperationRecordV2, UploadRecordV2, decode_or_migrate_instance,
};
use hd_platform::{DataPaths, write_owner_only};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_LEGACY_CONFIG_BYTES: u64 = 1024 * 1024;

const INSTANCES: TableDefinition<&str, &[u8]> = TableDefinition::new("instances-v2");
const OPERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("operations-v2");
const IDEMPOTENCY: TableDefinition<&str, &[u8]> = TableDefinition::new("idempotency-v2");
const LEASES: TableDefinition<&str, &[u8]> = TableDefinition::new("leases-v2");
const MIGRATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("migrations-v2");
const UPLOADS: TableDefinition<&str, &[u8]> = TableDefinition::new("uploads-v2");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta-v2");

#[derive(Debug, Clone)]
pub struct PersistentStore {
    database: Arc<Database>,
    path: PathBuf,
}

impl PersistentStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                operation: "create database parent",
                path: parent.to_owned(),
                source,
            })?;
        }
        drop(hd_platform::open_owner_only_rw(path)?);
        let database = Database::create(path).map_err(StoreError::database)?;
        let store = Self {
            database: Arc::new(database),
            path: path.to_owned(),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn initialize(&self) -> Result<(), StoreError> {
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        {
            let _ = transaction
                .open_table(INSTANCES)
                .map_err(StoreError::database)?;
            let _ = transaction
                .open_table(OPERATIONS)
                .map_err(StoreError::database)?;
            let _ = transaction
                .open_table(IDEMPOTENCY)
                .map_err(StoreError::database)?;
            let _ = transaction
                .open_table(LEASES)
                .map_err(StoreError::database)?;
            let _ = transaction
                .open_table(MIGRATIONS)
                .map_err(StoreError::database)?;
            let _ = transaction
                .open_table(UPLOADS)
                .map_err(StoreError::database)?;
            let _ = transaction.open_table(META).map_err(StoreError::database)?;
        }
        transaction.commit().map_err(StoreError::database)
    }

    pub fn create_instance(&self, spec: InstanceSpecV2) -> Result<InstanceRecordV2, StoreError> {
        spec.validate()?;
        let record = InstanceRecordV2::new(spec);
        let key = record.spec.id.to_string();
        let encoded = encode(&record)?;
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        {
            let mut table = transaction
                .open_table(INSTANCES)
                .map_err(StoreError::database)?;
            if table
                .get(key.as_str())
                .map_err(StoreError::database)?
                .is_some()
            {
                return Err(StoreError::AlreadyExists(record.spec.id));
            }
            table
                .insert(key.as_str(), encoded.as_slice())
                .map_err(StoreError::database)?;
        }
        transaction.commit().map_err(StoreError::database)?;
        Ok(record)
    }

    pub fn put_instance(&self, record: &InstanceRecordV2) -> Result<(), StoreError> {
        record.spec.validate()?;
        put_json(
            &self.database,
            INSTANCES,
            &record.spec.id.to_string(),
            record,
        )
    }

    pub fn update_instance(
        &self,
        expected_revision: u64,
        spec: InstanceSpecV2,
    ) -> Result<InstanceRecordV2, StoreError> {
        spec.validate()?;
        let key = spec.id.to_string();
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        let updated = {
            let mut table = transaction
                .open_table(INSTANCES)
                .map_err(StoreError::database)?;
            let Some(value) = table.get(key.as_str()).map_err(StoreError::database)? else {
                return Err(StoreError::NotFound(spec.id));
            };
            let mut record: InstanceRecordV2 = decode(value.value())?;
            drop(value);
            if record.status.revision != expected_revision {
                return Err(StoreError::RevisionConflict {
                    expected: expected_revision,
                    actual: record.status.revision,
                });
            }
            record.spec = spec;
            record.status.revision = record.status.revision.saturating_add(1);
            record.status.updated_at = OffsetDateTime::now_utc();
            let encoded = encode(&record)?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .map_err(StoreError::database)?;
            record
        };
        transaction.commit().map_err(StoreError::database)?;
        Ok(updated)
    }

    pub fn get_instance(&self, id: Uuid) -> Result<Option<InstanceRecordV2>, StoreError> {
        get_json(&self.database, INSTANCES, &id.to_string())
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceRecordV2>, StoreError> {
        let mut records = list_json(&self.database, INSTANCES)?;
        records.sort_by(|left: &InstanceRecordV2, right| {
            left.spec
                .name
                .cmp(&right.spec.name)
                .then(left.spec.id.cmp(&right.spec.id))
        });
        Ok(records)
    }

    pub fn delete_instance_record(&self, id: Uuid) -> Result<(), StoreError> {
        remove(&self.database, INSTANCES, &id.to_string())
    }

    pub fn create_operation_idempotent(
        &self,
        instance_id: Option<Uuid>,
        kind: OperationKindV2,
        idempotency_key: &str,
    ) -> Result<(OperationRecordV2, bool), StoreError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 128 {
            return Err(StoreError::InvalidIdempotencyKey);
        }
        let scope = instance_id.map_or_else(|| "host".to_owned(), |id| id.to_string());
        let idempotency_record = format!("{scope}:{idempotency_key}");
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        let existing_id = {
            let table = transaction
                .open_table(IDEMPOTENCY)
                .map_err(StoreError::database)?;
            table
                .get(idempotency_record.as_str())
                .map_err(StoreError::database)?
                .map(|value| String::from_utf8(value.value().to_vec()))
                .transpose()
                .map_err(StoreError::InvalidStoredUtf8)?
        };
        if let Some(existing_id) = existing_id {
            let operation: OperationRecordV2 = {
                let table = transaction
                    .open_table(OPERATIONS)
                    .map_err(StoreError::database)?;
                let value = table
                    .get(existing_id.as_str())
                    .map_err(StoreError::database)?
                    .ok_or_else(|| {
                        StoreError::Corrupt("idempotency operation missing".to_owned())
                    })?;
                decode(value.value())?
            };
            if operation.kind != kind || operation.instance_id != instance_id {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok((operation, false));
        }
        let operation = OperationRecordV2::queued(instance_id, kind);
        let operation_key = operation.id.to_string();
        let encoded = encode(&operation)?;
        {
            let mut operations = transaction
                .open_table(OPERATIONS)
                .map_err(StoreError::database)?;
            operations
                .insert(operation_key.as_str(), encoded.as_slice())
                .map_err(StoreError::database)?;
        }
        {
            let mut idempotency = transaction
                .open_table(IDEMPOTENCY)
                .map_err(StoreError::database)?;
            idempotency
                .insert(idempotency_record.as_str(), operation_key.as_bytes())
                .map_err(StoreError::database)?;
        }
        transaction.commit().map_err(StoreError::database)?;
        Ok((operation, true))
    }

    pub fn put_operation(&self, operation: &OperationRecordV2) -> Result<(), StoreError> {
        put_json(
            &self.database,
            OPERATIONS,
            &operation.id.to_string(),
            operation,
        )
    }

    pub fn get_operation(&self, id: Uuid) -> Result<Option<OperationRecordV2>, StoreError> {
        get_json(&self.database, OPERATIONS, &id.to_string())
    }

    pub fn list_operations(&self) -> Result<Vec<OperationRecordV2>, StoreError> {
        let mut operations = list_json(&self.database, OPERATIONS)?;
        operations.sort_by_key(|operation: &OperationRecordV2| operation.created_at);
        Ok(operations)
    }

    pub fn insert_lease(&self, lease: &LeaseV2) -> Result<(), StoreError> {
        let resource_key = lease_key(lease);
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        {
            let mut table = transaction
                .open_table(LEASES)
                .map_err(StoreError::database)?;
            if let Some(existing) = table
                .get(resource_key.as_str())
                .map_err(StoreError::database)?
            {
                let existing: LeaseV2 = decode(existing.value())?;
                if existing.owner.instance_id != lease.owner.instance_id {
                    return Err(StoreError::LeaseConflict {
                        resource: lease.resource.clone(),
                        owner: existing.owner.instance_id,
                    });
                }
            }
            let encoded = encode(lease)?;
            table
                .insert(resource_key.as_str(), encoded.as_slice())
                .map_err(StoreError::database)?;
        }
        transaction.commit().map_err(StoreError::database)
    }

    pub fn put_leases(&self, leases: &[LeaseV2]) -> Result<(), StoreError> {
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        {
            let mut table = transaction
                .open_table(LEASES)
                .map_err(StoreError::database)?;
            for lease in leases {
                let key = lease_key(lease);
                if let Some(existing) = table.get(key.as_str()).map_err(StoreError::database)? {
                    let existing: LeaseV2 = decode(existing.value())?;
                    if existing.owner.instance_id != lease.owner.instance_id {
                        return Err(StoreError::LeaseConflict {
                            resource: lease.resource.clone(),
                            owner: existing.owner.instance_id,
                        });
                    }
                }
            }
            for lease in leases {
                let key = lease_key(lease);
                let encoded = encode(lease)?;
                table
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(StoreError::database)?;
            }
        }
        transaction.commit().map_err(StoreError::database)
    }

    pub fn list_leases(&self) -> Result<Vec<LeaseV2>, StoreError> {
        list_json(&self.database, LEASES)
    }

    pub fn release_instance_leases(&self, instance_id: Uuid) -> Result<Vec<LeaseV2>, StoreError> {
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        let removed = {
            let mut table = transaction
                .open_table(LEASES)
                .map_err(StoreError::database)?;
            let mut targets = Vec::new();
            for entry in table.iter().map_err(StoreError::database)? {
                let (key, value) = entry.map_err(StoreError::database)?;
                let lease: LeaseV2 = decode(value.value())?;
                if lease.owner.instance_id == instance_id {
                    targets.push((key.value().to_owned(), lease));
                }
            }
            for (key, _) in &targets {
                table.remove(key.as_str()).map_err(StoreError::database)?;
            }
            targets
                .into_iter()
                .map(|(_, lease)| lease)
                .collect::<Vec<_>>()
        };
        transaction.commit().map_err(StoreError::database)?;
        Ok(removed)
    }

    pub fn put_upload(&self, upload: &UploadRecordV2) -> Result<(), StoreError> {
        put_json(&self.database, UPLOADS, &upload.id.to_string(), upload)
    }

    pub fn get_upload(&self, id: Uuid) -> Result<Option<UploadRecordV2>, StoreError> {
        get_json(&self.database, UPLOADS, &id.to_string())
    }

    pub fn remove_upload(&self, id: Uuid) -> Result<(), StoreError> {
        remove(&self.database, UPLOADS, &id.to_string())
    }

    pub fn put_migration(&self, migration: &MigrationRecordV2) -> Result<(), StoreError> {
        put_json(
            &self.database,
            MIGRATIONS,
            &migration.instance_id.to_string(),
            migration,
        )
    }

    pub fn get_migration(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<MigrationRecordV2>, StoreError> {
        get_json(&self.database, MIGRATIONS, &instance_id.to_string())
    }

    pub fn list_migrations(&self) -> Result<Vec<MigrationRecordV2>, StoreError> {
        list_json(&self.database, MIGRATIONS)
    }

    pub fn next_event_sequence(&self) -> Result<u64, StoreError> {
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        let next = {
            let mut table = transaction.open_table(META).map_err(StoreError::database)?;
            let current = table
                .get("event-sequence")
                .map_err(StoreError::database)?
                .map_or(Ok(0_u64), |value| decode(value.value()))?;
            let next = current.saturating_add(1);
            let encoded = encode(&next)?;
            table
                .insert("event-sequence", encoded.as_slice())
                .map_err(StoreError::database)?;
            next
        };
        transaction.commit().map_err(StoreError::database)?;
        Ok(next)
    }

    pub fn migrate_legacy_instances(
        &self,
        paths: &DataPaths,
    ) -> Result<Vec<MigrationRecordV2>, StoreError> {
        if !paths.instances.is_dir() {
            return Ok(Vec::new());
        }
        let mut migrations = Vec::new();
        for entry in std::fs::read_dir(&paths.instances).map_err(|source| StoreError::Io {
            operation: "scan legacy instance directory",
            path: paths.instances.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| StoreError::Io {
                operation: "read legacy instance entry",
                path: paths.instances.clone(),
                source,
            })?;
            let config_path = entry.path().join("instance.json");
            let bytes = match read_regular_limited(&config_path, MAX_LEGACY_CONFIG_BYTES) {
                Ok(bytes) => bytes,
                Err(StoreError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let outcome = decode_or_migrate_instance(&bytes)?;
            if self.get_instance(outcome.spec.id)?.is_some() {
                continue;
            }
            let source_version = outcome.source_version;
            let instance_id = outcome.spec.id;
            let backup_path = paths.migration_backup(instance_id);
            if source_version == INSTANCE_CONFIG_VERSION {
                self.create_instance(outcome.spec)?;
            } else {
                match read_regular_limited(&backup_path, MAX_LEGACY_CONFIG_BYTES) {
                    Ok(existing) if existing != bytes => {
                        return Err(StoreError::Corrupt(format!(
                            "migration backup differs from source for instance {instance_id}"
                        )));
                    }
                    Ok(_) => {}
                    Err(StoreError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        write_owner_only(&backup_path, &bytes)?;
                    }
                    Err(error) => return Err(error),
                }
                let migration = MigrationRecordV2 {
                    instance_id,
                    source_version,
                    target_version: INSTANCE_CONFIG_VERSION,
                    backup_path,
                    notes: outcome.notes,
                    migrated_at: OffsetDateTime::now_utc(),
                };
                self.create_migrated_instance(outcome.spec, &migration)?;
                migrations.push(migration);
            }
        }
        Ok(migrations)
    }

    fn create_migrated_instance(
        &self,
        spec: InstanceSpecV2,
        migration: &MigrationRecordV2,
    ) -> Result<(), StoreError> {
        spec.validate()?;
        let record = InstanceRecordV2::new(spec);
        let instance_key = record.spec.id.to_string();
        let instance_bytes = encode(&record)?;
        let migration_bytes = encode(migration)?;
        let transaction = self.database.begin_write().map_err(StoreError::database)?;
        {
            let mut instances = transaction
                .open_table(INSTANCES)
                .map_err(StoreError::database)?;
            if instances
                .get(instance_key.as_str())
                .map_err(StoreError::database)?
                .is_some()
            {
                return Err(StoreError::AlreadyExists(record.spec.id));
            }
            instances
                .insert(instance_key.as_str(), instance_bytes.as_slice())
                .map_err(StoreError::database)?;
        }
        {
            let mut migrations = transaction
                .open_table(MIGRATIONS)
                .map_err(StoreError::database)?;
            migrations
                .insert(instance_key.as_str(), migration_bytes.as_slice())
                .map_err(StoreError::database)?;
        }
        transaction.commit().map_err(StoreError::database)
    }
}

fn read_regular_limited(path: &Path, maximum: u64) -> Result<Vec<u8>, StoreError> {
    match hd_platform::read_regular_nofollow_limited(path, maximum) {
        Ok(bytes) => Ok(bytes),
        Err(hd_platform::PlatformError::Io {
            operation,
            path,
            source,
        }) => Err(StoreError::Io {
            operation,
            path,
            source,
        }),
        Err(error) => Err(StoreError::Platform(error)),
    }
}

pub(crate) fn lease_key(lease: &LeaseV2) -> String {
    match lease.kind {
        hd_core::LeaseKindV2::FrameGeneration => format!(
            "{:?}:{}:{}",
            lease.kind, lease.owner.instance_id, lease.resource
        ),
        _ => format!("{:?}:{}", lease.kind, lease.resource),
    }
    .to_ascii_lowercase()
}

fn put_json<T: Serialize>(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
    key: &str,
    value: &T,
) -> Result<(), StoreError> {
    let encoded = encode(value)?;
    let transaction = database.begin_write().map_err(StoreError::database)?;
    {
        let mut table = transaction
            .open_table(definition)
            .map_err(StoreError::database)?;
        table
            .insert(key, encoded.as_slice())
            .map_err(StoreError::database)?;
    }
    transaction.commit().map_err(StoreError::database)
}

fn get_json<T: DeserializeOwned>(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
    key: &str,
) -> Result<Option<T>, StoreError> {
    let transaction = database.begin_read().map_err(StoreError::database)?;
    let table = transaction
        .open_table(definition)
        .map_err(StoreError::database)?;
    table
        .get(key)
        .map_err(StoreError::database)?
        .map(|value| decode(value.value()))
        .transpose()
}

fn list_json<T: DeserializeOwned>(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
) -> Result<Vec<T>, StoreError> {
    let transaction = database.begin_read().map_err(StoreError::database)?;
    let table = transaction
        .open_table(definition)
        .map_err(StoreError::database)?;
    let mut values = Vec::new();
    for entry in table.iter().map_err(StoreError::database)? {
        let (_, value) = entry.map_err(StoreError::database)?;
        values.push(decode(value.value())?);
    }
    Ok(values)
}

fn remove(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
    key: &str,
) -> Result<(), StoreError> {
    let transaction = database.begin_write().map_err(StoreError::database)?;
    {
        let mut table = transaction
            .open_table(definition)
            .map_err(StoreError::database)?;
        table.remove(key).map_err(StoreError::database)?;
    }
    transaction.commit().map_err(StoreError::database)
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(StoreError::Encode)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(StoreError::Decode)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("failed to encode persistent record: {0}")]
    Encode(serde_json::Error),
    #[error("failed to decode persistent record: {0}")]
    Decode(serde_json::Error),
    #[error("invalid UTF-8 in persistent record: {0}")]
    InvalidStoredUtf8(std::string::FromUtf8Error),
    #[error("persistent store is corrupt: {0}")]
    Corrupt(String),
    #[error("instance {0} already exists")]
    AlreadyExists(Uuid),
    #[error("instance {0} was not found")]
    NotFound(Uuid),
    #[error("revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("idempotency key must contain 1..=128 bytes")]
    InvalidIdempotencyKey,
    #[error("idempotency key was already used for a different request")]
    IdempotencyConflict,
    #[error("resource {resource} is leased by instance {owner}")]
    LeaseConflict { resource: String, owner: Uuid },
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Config(#[from] hd_core::ConfigError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

impl StoreError {
    fn database(error: impl std::fmt::Display) -> Self {
        Self::Database(error.to_string())
    }
}
