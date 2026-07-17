use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hd_core::{ObservedStateV2, RunEventV2, RunManifestV2, RunResultV2};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const EVENT_SEGMENT_BYTES: u64 = 50 * 1024 * 1024;
const EVENT_SEGMENTS: u8 = 8;

#[derive(Debug)]
struct EventWriter {
    file: File,
    bytes: u64,
}

#[derive(Debug)]
pub struct RunJournalV2 {
    run_dir: PathBuf,
    instance_id: Uuid,
    run_id: Uuid,
    trace_id: Uuid,
    writer: Mutex<EventWriter>,
    sequence: Mutex<u64>,
}

impl RunJournalV2 {
    pub fn create(
        run_dir: &Path,
        instance_id: Uuid,
        run_id: Uuid,
        trace_id: Uuid,
    ) -> Result<Self, JournalError> {
        std::fs::create_dir_all(run_dir).map_err(|source| JournalError::Io {
            operation: "create run directory",
            path: run_dir.to_owned(),
            source,
        })?;
        let events_path = run_dir.join("events.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .map_err(|source| JournalError::Io {
                operation: "open event stream",
                path: events_path.clone(),
                source,
            })?;
        let bytes = file.metadata().map_or(0, |metadata| metadata.len());
        Ok(Self {
            run_dir: run_dir.to_owned(),
            instance_id,
            run_id,
            trace_id,
            writer: Mutex::new(EventWriter { file, bytes }),
            sequence: Mutex::new(0),
        })
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn write_manifest(&self, manifest: &RunManifestV2) -> Result<(), JournalError> {
        if manifest.run_id != self.run_id || manifest.instance.id != self.instance_id {
            return Err(JournalError::ContextMismatch);
        }
        write_json_atomic(&self.run_dir.join("manifest.json"), manifest)
    }

    pub fn event(
        &self,
        category: impl Into<String>,
        name: impl Into<String>,
        state: Option<ObservedStateV2>,
        error_code: Option<String>,
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
        let event = RunEventV2 {
            schema_version: 2,
            timestamp: OffsetDateTime::now_utc(),
            sequence,
            trace_id: self.trace_id,
            instance_id: self.instance_id,
            run_id: self.run_id,
            category: category.into(),
            name: name.into(),
            state,
            error_code,
            fields,
        };
        let mut bytes = serde_json::to_vec(&event).map_err(JournalError::Encode)?;
        bytes.push(b'\n');
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| JournalError::Poisoned("events"))?;
        if writer.bytes.saturating_add(bytes.len() as u64) > EVENT_SEGMENT_BYTES {
            rotate_events(&self.run_dir, &mut writer)?;
        }
        writer
            .file
            .write_all(&bytes)
            .map_err(|source| JournalError::Io {
                operation: "append event",
                path: self.run_dir.join("events.jsonl"),
                source,
            })?;
        writer.file.flush().map_err(|source| JournalError::Io {
            operation: "flush event",
            path: self.run_dir.join("events.jsonl"),
            source,
        })?;
        writer.bytes = writer.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    pub fn boundary_started(
        &self,
        boundary: &str,
        fields: BTreeMap<String, String>,
    ) -> Result<(), JournalError> {
        self.event(
            "boundary",
            format!("{boundary}.started"),
            None,
            None,
            fields,
        )
    }

    pub fn boundary_succeeded(
        &self,
        boundary: &str,
        duration_ms: u64,
        mut fields: BTreeMap<String, String>,
    ) -> Result<(), JournalError> {
        fields.insert("duration_ms".to_owned(), duration_ms.to_string());
        self.event(
            "boundary",
            format!("{boundary}.succeeded"),
            None,
            None,
            fields,
        )
    }

    pub fn boundary_failed(
        &self,
        boundary: &str,
        error_code: &str,
        duration_ms: u64,
        mut fields: BTreeMap<String, String>,
    ) -> Result<(), JournalError> {
        fields.insert("duration_ms".to_owned(), duration_ms.to_string());
        self.event(
            "boundary",
            format!("{boundary}.failed"),
            None,
            Some(error_code.to_owned()),
            fields,
        )
    }

    pub fn finish(&self, result: &RunResultV2) -> Result<(), JournalError> {
        if result.run_id != self.run_id || result.instance_id != self.instance_id {
            return Err(JournalError::ContextMismatch);
        }
        write_json_atomic(&self.run_dir.join("result.json"), result)
    }
}

pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), JournalError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(JournalError::Encode)?;
    bytes.push(b'\n');
    hd_platform::write_owner_only(path, &bytes).map_err(JournalError::Platform)
}

fn rotate_events(run_dir: &Path, writer: &mut EventWriter) -> Result<(), JournalError> {
    writer.file.sync_all().map_err(|source| JournalError::Io {
        operation: "sync event segment",
        path: run_dir.join("events.jsonl"),
        source,
    })?;
    let oldest = run_dir.join(format!("events.jsonl.{EVENT_SEGMENTS}"));
    if oldest.exists() {
        std::fs::remove_file(&oldest).map_err(|source| JournalError::Io {
            operation: "remove oldest event segment",
            path: oldest,
            source,
        })?;
    }
    for index in (1..EVENT_SEGMENTS).rev() {
        let source = run_dir.join(format!("events.jsonl.{index}"));
        let destination = run_dir.join(format!("events.jsonl.{}", index + 1));
        if source.exists() {
            std::fs::rename(&source, &destination).map_err(|source_error| JournalError::Io {
                operation: "rotate event segment",
                path: source,
                source: source_error,
            })?;
        }
    }
    let active = run_dir.join("events.jsonl");
    let first = run_dir.join("events.jsonl.1");
    if active.exists() {
        std::fs::rename(&active, &first).map_err(|source| JournalError::Io {
            operation: "rotate active event segment",
            path: active.clone(),
            source,
        })?;
    }
    writer.file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&active)
        .map_err(|source| JournalError::Io {
            operation: "open new event segment",
            path: active,
            source,
        })?;
    writer.bytes = 0;
    Ok(())
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("run journal context does not match the record")]
    ContextMismatch,
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
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_contains_stable_context() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let instance_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let trace_id = Uuid::new_v4();
        let journal =
            RunJournalV2::create(directory.path(), instance_id, run_id, trace_id).expect("journal");
        journal
            .event("test", "test.event", None, None, BTreeMap::new())
            .expect("event");
        let text =
            std::fs::read_to_string(directory.path().join("events.jsonl")).expect("read event");
        assert!(text.contains(&instance_id.to_string()));
        assert!(text.contains(&run_id.to_string()));
        assert!(text.contains(&trace_id.to_string()));
    }
}
