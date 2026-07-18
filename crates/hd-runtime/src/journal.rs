use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hd_core::{InstanceConfigV1, InstanceState, LaunchPlanV1, RunResultV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ArtifactFingerprintV1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifestV1 {
    pub schema_version: u32,
    pub run_id: Uuid,
    pub instance: InstanceConfigV1,
    pub launch_plan: LaunchPlanV1,
    pub artifacts: Vec<ArtifactFingerprintV1>,
    pub toolchain: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventV1 {
    pub schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub sequence: u64,
    pub category: String,
    pub name: String,
    pub state: Option<InstanceState>,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct RunJournal {
    run_dir: PathBuf,
    events: Mutex<std::fs::File>,
    sequence: Mutex<u64>,
}

impl RunJournal {
    pub fn create(run_dir: &Path) -> Result<Self, JournalError> {
        std::fs::create_dir_all(run_dir).map_err(|source| JournalError::Io {
            operation: "create run directory",
            path: run_dir.to_owned(),
            source,
        })?;
        let events_path = run_dir.join("events.jsonl");
        let events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .map_err(|source| JournalError::Io {
                operation: "open event stream",
                path: events_path,
                source,
            })?;
        Ok(Self {
            run_dir: run_dir.to_owned(),
            events: Mutex::new(events),
            sequence: Mutex::new(0),
        })
    }

    pub fn write_manifest(&self, manifest: &RunManifestV1) -> Result<(), JournalError> {
        write_json_atomic(&self.run_dir.join("manifest.json"), manifest)
    }

    pub fn event(
        &self,
        category: impl Into<String>,
        name: impl Into<String>,
        state: Option<InstanceState>,
        fields: BTreeMap<String, String>,
    ) -> Result<(), JournalError> {
        let sequence = {
            let mut guard = self
                .sequence
                .lock()
                .map_err(|_| JournalError::Poisoned("sequence"))?;
            *guard = guard.saturating_add(1);
            *guard
        };
        let event = RunEventV1 {
            schema_version: 1,
            timestamp: OffsetDateTime::now_utc(),
            sequence,
            category: category.into(),
            name: name.into(),
            state,
            fields,
        };
        let mut bytes = serde_json::to_vec(&event).map_err(JournalError::Encode)?;
        bytes.push(b'\n');
        let mut file = self
            .events
            .lock()
            .map_err(|_| JournalError::Poisoned("events"))?;
        file.write_all(&bytes).map_err(|source| JournalError::Io {
            operation: "append event",
            path: self.run_dir.join("events.jsonl"),
            source,
        })?;
        file.flush().map_err(|source| JournalError::Io {
            operation: "flush event",
            path: self.run_dir.join("events.jsonl"),
            source,
        })
    }

    pub fn finish(&self, result: &RunResultV1) -> Result<(), JournalError> {
        write_json_atomic(&self.run_dir.join("result.json"), result)
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), JournalError> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(JournalError::Encode)?;
    std::fs::write(&tmp, bytes).map_err(|source| JournalError::Io {
        operation: "write temporary JSON",
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| JournalError::Io {
        operation: "commit JSON",
        path: path.to_owned(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to encode journal entry: {0}")]
    Encode(serde_json::Error),
    #[error("journal mutex was poisoned: {0}")]
    Poisoned(&'static str),
}
