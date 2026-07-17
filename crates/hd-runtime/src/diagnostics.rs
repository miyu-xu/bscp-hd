use std::io::{Read as _, Write};
use std::path::{Component, Path, PathBuf};

use hd_core::{
    DIAGNOSTIC_MANIFEST_VERSION, DiagnosticBundleResponseV2, DiagnosticCheckV2, DiagnosticFileV2,
    DiagnosticManifestV2, HostCapabilitiesV2, InstanceRecordV2, LeaseV2, MigrationRecordV2,
    OperationRecordV2,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const RETAIN_BUNDLES: usize = 10;

#[derive(Debug)]
pub struct DiagnosticInputsV2 {
    pub capabilities: HostCapabilitiesV2,
    pub instance: Option<InstanceRecordV2>,
    pub operations: Vec<OperationRecordV2>,
    pub leases: Vec<LeaseV2>,
    pub migrations: Vec<MigrationRecordV2>,
    pub worker_checks: Vec<DiagnosticCheckV2>,
    pub guest_log: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct CollectedFile {
    relative_path: PathBuf,
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug, Clone)]
pub struct DiagnosticCollector {
    paths: hd_platform::DataPaths,
}

impl DiagnosticCollector {
    pub fn new(paths: hd_platform::DataPaths) -> Self {
        Self { paths }
    }

    #[allow(clippy::too_many_lines)]
    pub fn collect(
        &self,
        inputs: DiagnosticInputsV2,
    ) -> Result<DiagnosticBundleResponseV2, DiagnosticError> {
        std::fs::create_dir_all(&self.paths.diagnostics).map_err(|source| DiagnosticError::Io {
            operation: "create diagnostics directory",
            path: self.paths.diagnostics.clone(),
            source,
        })?;
        let bundle_id = Uuid::new_v4();
        let created_at = OffsetDateTime::now_utc();
        let instance_id = inputs.instance.as_ref().map(|record| record.spec.id);
        let mut files = Vec::new();
        let mut omitted = Vec::new();
        add_json(&mut files, "host/capabilities.json", &inputs.capabilities)?;
        add_json(&mut files, "host/operations.json", &inputs.operations)?;
        add_json(&mut files, "host/leases.json", &inputs.leases)?;
        add_json(&mut files, "host/migrations.json", &inputs.migrations)?;
        add_json(&mut files, "worker/checks.json", &inputs.worker_checks)?;
        if let Some(instance) = &inputs.instance {
            add_json(&mut files, "instance/record.json", instance)?;
            self.collect_run_files(instance, &mut files, &mut omitted)?;
        }
        if let Some(path) = &inputs.guest_log {
            self.collect_source(path, "guest/logcat.txt", &mut files, &mut omitted)?;
        }
        self.collect_host_logs(&mut files, &mut omitted)?;
        redact(&mut files)?;
        enforce_total_limit(&mut files, &mut omitted);

        let manifest_files = files
            .iter()
            .map(|file| DiagnosticFileV2 {
                relative_path: file.relative_path.clone(),
                sha256: hex::encode(Sha256::digest(&file.bytes)),
                size_bytes: file.bytes.len() as u64,
                truncated: file.truncated,
            })
            .collect::<Vec<_>>();
        let manifest = DiagnosticManifestV2 {
            schema_version: DIAGNOSTIC_MANIFEST_VERSION,
            bundle_id,
            instance_id,
            created_at,
            redaction_policy: "hd-diagnostics-redaction-v2".to_owned(),
            max_bytes: MAX_BUNDLE_BYTES,
            files: manifest_files,
            omitted,
            checks: inputs.worker_checks,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(DiagnosticError::Json)?;
        let manifest_sha256 = hex::encode(Sha256::digest(&manifest_bytes));
        let timestamp = created_at.unix_timestamp().max(0);
        let output = self
            .paths
            .diagnostics
            .join(format!("diag-{timestamp}-{bundle_id}.tar.zst"));
        let temporary = output.with_extension(format!("tmp-{bundle_id}"));
        let mut temporary_guard = TemporaryGuard::new(temporary.clone());
        hd_platform::write_owner_only(&temporary, &[])?;
        let output_file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|source| DiagnosticError::Io {
                operation: "open diagnostic bundle",
                path: temporary.clone(),
                source,
            })?;
        let encoder = zstd::Encoder::new(output_file, 9).map_err(DiagnosticError::Compress)?;
        let mut archive = tar::Builder::new(encoder);
        archive.mode(tar::HeaderMode::Deterministic);
        for file in &files {
            append_tar_file(&mut archive, &file.relative_path, &file.bytes, timestamp)?;
        }
        append_tar_file(
            &mut archive,
            Path::new("diagnostic-manifest-v2.json"),
            &manifest_bytes,
            timestamp,
        )?;
        let encoder = archive.into_inner().map_err(DiagnosticError::Archive)?;
        let output_file = encoder.finish().map_err(DiagnosticError::Compress)?;
        output_file
            .sync_all()
            .map_err(|source| DiagnosticError::Io {
                operation: "sync diagnostic bundle",
                path: temporary.clone(),
                source,
            })?;
        let size_bytes = output_file
            .metadata()
            .map_err(|source| DiagnosticError::Io {
                operation: "read diagnostic bundle metadata",
                path: temporary.clone(),
                source,
            })?
            .len();
        if size_bytes > MAX_BUNDLE_BYTES {
            return Err(DiagnosticError::BundleTooLarge(size_bytes));
        }
        drop(output_file);
        let archive_sha256 = crate::sha256_file(&temporary)
            .map_err(|error| DiagnosticError::Hash(error.to_string()))?;
        hd_platform::replace_file(&temporary, &output)?;
        temporary_guard.disarm();
        self.enforce_retention(&output)?;
        Ok(DiagnosticBundleResponseV2 {
            bundle_id,
            path: output,
            manifest_sha256,
            archive_sha256,
            size_bytes,
        })
    }

    fn collect_run_files(
        &self,
        instance: &InstanceRecordV2,
        files: &mut Vec<CollectedFile>,
        omitted: &mut Vec<String>,
    ) -> Result<(), DiagnosticError> {
        let run_id = instance
            .active_run_id
            .or_else(|| latest_run_id(&self.paths, instance.spec.id));
        let Some(run_id) = run_id else {
            omitted.push("run:no_run_directory".to_owned());
            return Ok(());
        };
        let run_dir = self.paths.run_dir(instance.spec.id, run_id);
        for name in [
            "manifest.json",
            "events.jsonl",
            "events.jsonl.1",
            "result.json",
            "crosvm.stdout.log",
            "crosvm.stderr.log",
            "serial.txt",
            "console-hvc0.txt",
            "guest-logcat.txt",
            "frame-ready-v2.json",
        ] {
            let source = run_dir.join(name);
            if source.is_file() {
                self.collect_source(&source, &format!("run/{run_id}/{name}"), files, omitted)?;
            }
        }
        self.collect_component_files(&run_dir, run_id, files, omitted)?;
        Ok(())
    }

    fn collect_component_files(
        &self,
        run_dir: &Path,
        run_id: Uuid,
        files: &mut Vec<CollectedFile>,
        omitted: &mut Vec<String>,
    ) -> Result<(), DiagnosticError> {
        let directory = run_dir.join("components");
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return Ok(());
        };
        let mut sources = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).ok()?;
                let name = path.file_name()?.to_str()?.to_owned();
                (metadata.file_type().is_file()
                    && !metadata.file_type().is_symlink()
                    && name.len() <= 128
                    && name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    }))
                .then_some((name, path))
            })
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, source) in sources.into_iter().take(32) {
            self.collect_source(
                &source,
                &format!("run/{run_id}/components/{name}"),
                files,
                omitted,
            )?;
        }
        Ok(())
    }

    fn collect_host_logs(
        &self,
        files: &mut Vec<CollectedFile>,
        omitted: &mut Vec<String>,
    ) -> Result<(), DiagnosticError> {
        let Ok(entries) = std::fs::read_dir(&self.paths.logs) else {
            return Ok(());
        };
        let mut candidates = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).ok()?;
                let name = path.file_name()?.to_str()?;
                (metadata.file_type().is_file()
                    && !metadata.file_type().is_symlink()
                    && name.contains(".jsonl"))
                .then_some((metadata.modified().ok(), path))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, source) in candidates.into_iter().take(3) {
            let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            self.collect_source(&source, &format!("host/logs/{name}"), files, omitted)?;
        }
        Ok(())
    }

    fn collect_source(
        &self,
        source: &Path,
        relative: &str,
        files: &mut Vec<CollectedFile>,
        omitted: &mut Vec<String>,
    ) -> Result<(), DiagnosticError> {
        validate_relative(Path::new(relative))?;
        let source_metadata =
            std::fs::symlink_metadata(source).map_err(|error| DiagnosticError::Io {
                operation: "inspect diagnostic source",
                path: source.to_owned(),
                source: error,
            })?;
        if !source_metadata.file_type().is_file() || source_metadata.file_type().is_symlink() {
            omitted.push(format!("{relative}:unsafe_source"));
            return Ok(());
        }
        let canonical_root =
            self.paths
                .root
                .canonicalize()
                .map_err(|error| DiagnosticError::Io {
                    operation: "canonicalize diagnostic data root",
                    path: self.paths.root.clone(),
                    source: error,
                })?;
        let canonical_source = source.canonicalize().map_err(|error| DiagnosticError::Io {
            operation: "canonicalize diagnostic source",
            path: source.to_owned(),
            source: error,
        })?;
        if !canonical_source.starts_with(&canonical_root) {
            return Err(DiagnosticError::UnsafePath(source.to_owned()));
        }
        let mut input = hd_platform::open_regular_read_nofollow(&canonical_source)?;
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut input)
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| DiagnosticError::Io {
                operation: "read diagnostic source",
                path: source.to_owned(),
                source: error,
            })?;
        let truncated = bytes.len() as u64 > MAX_SOURCE_BYTES;
        bytes.truncate(usize::try_from(MAX_SOURCE_BYTES).unwrap_or(usize::MAX));
        files.push(CollectedFile {
            relative_path: PathBuf::from(relative),
            bytes,
            truncated,
        });
        Ok(())
    }

    fn enforce_retention(&self, current: &Path) -> Result<(), DiagnosticError> {
        let mut bundles = std::fs::read_dir(&self.paths.diagnostics)
            .map_err(|source| DiagnosticError::Io {
                operation: "scan diagnostic retention",
                path: self.paths.diagnostics.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).ok()?;
                let name = path.file_name()?.to_str()?;
                (metadata.file_type().is_file()
                    && !metadata.file_type().is_symlink()
                    && name.starts_with("diag-")
                    && name.ends_with(".tar.zst"))
                .then_some((metadata.modified().ok(), path))
            })
            .collect::<Vec<_>>();
        bundles.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, path) in bundles.into_iter().skip(RETAIN_BUNDLES) {
            if path != current {
                std::fs::remove_file(&path).map_err(|source| DiagnosticError::Io {
                    operation: "remove expired diagnostic bundle",
                    path,
                    source,
                })?;
            }
        }
        Ok(())
    }
}

fn add_json<T: Serialize>(
    files: &mut Vec<CollectedFile>,
    relative: &str,
    value: &T,
) -> Result<(), DiagnosticError> {
    validate_relative(Path::new(relative))?;
    files.push(CollectedFile {
        relative_path: PathBuf::from(relative),
        bytes: serde_json::to_vec_pretty(value).map_err(DiagnosticError::Json)?,
        truncated: false,
    });
    Ok(())
}

fn redact(files: &mut [CollectedFile]) -> Result<(), DiagnosticError> {
    let private_roots = [
        std::env::var("USERPROFILE").ok(),
        std::env::var("HOME").ok(),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>();
    for file in files {
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&file.bytes) {
            redact_json(&mut value, &private_roots);
            file.bytes = serde_json::to_vec_pretty(&value).map_err(DiagnosticError::Json)?;
        } else {
            file.bytes = redact_text_records(&file.bytes, &private_roots)?;
        }
    }
    Ok(())
}

fn redact_json(value: &mut serde_json::Value, private_roots: &[String]) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let normalized = key.to_ascii_lowercase();
                if normalized.contains("bearer")
                    || normalized.contains("password")
                    || normalized.contains("secret")
                    || normalized.contains("token")
                    || normalized.contains("private_key")
                    || normalized.contains("signing_key")
                    || normalized.contains("api_key")
                    || normalized == "authorization"
                {
                    *value = serde_json::Value::String("<redacted>".to_owned());
                } else {
                    redact_json(value, private_roots);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json(value, private_roots);
            }
        }
        serde_json::Value::String(text) => {
            for root in private_roots {
                *text = replace_ascii_case_insensitive(text, root, "<user-profile>");
            }
            *text = redact_bearer_text(text);
        }
        _ => {}
    }
}

fn redact_bearer_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(index) = find_ascii_case_insensitive(remaining, "bearer ") {
        let (prefix, tail) = remaining.split_at(index);
        output.push_str(prefix);
        output.push_str("Bearer <redacted>");
        let after = &tail[7..];
        let token_end = after
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']')
            })
            .unwrap_or(after.len());
        remaining = &after[token_end..];
    }
    output.push_str(remaining);
    let mut redacted = output;
    for key in [
        "authorization",
        "bearer_token",
        "access_token",
        "refresh_token",
        "password",
        "client_secret",
        "secret_path",
        "secret",
        "private_key",
        "signing_key",
        "api_key",
    ] {
        redacted = redact_assignment(&redacted, key);
    }
    redacted
}

fn redact_assignment(text: &str, key: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    loop {
        let Some(index) = find_key_assignment(remaining, key) else {
            output.push_str(remaining);
            break;
        };
        let (prefix, tail) = remaining.split_at(index);
        output.push_str(prefix);
        let after_key = &tail[key.len()..];
        let separator = after_key
            .char_indices()
            .find(|(_, character)| !character.is_ascii_whitespace());
        let Some((separator_index, separator_character @ (':' | '='))) = separator else {
            output.push_str(&tail[..key.len()]);
            remaining = after_key;
            continue;
        };
        let value_start = separator_index + separator_character.len_utf8();
        let value = &after_key[value_start..];
        let leading_space = value.len() - value.trim_start_matches(char::is_whitespace).len();
        let value = &value[leading_space..];
        let value_end = value
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']')
            })
            .unwrap_or(value.len());
        output.push_str(&tail[..key.len() + value_start + leading_space]);
        output.push_str("<redacted>");
        remaining = &value[value_end..];
    }
    output
}

fn find_key_assignment(text: &str, key: &str) -> Option<usize> {
    let mut offset = 0;
    while offset <= text.len().saturating_sub(key.len()) {
        let candidate = find_ascii_case_insensitive(&text[offset..], key)? + offset;
        let before_valid = candidate == 0
            || !text.as_bytes()[candidate - 1].is_ascii_alphanumeric()
                && text.as_bytes()[candidate - 1] != b'_';
        let after = candidate + key.len();
        let after_valid = after == text.len()
            || !text.as_bytes()[after].is_ascii_alphanumeric() && text.as_bytes()[after] != b'_';
        if before_valid && after_valid {
            return Some(candidate);
        }
        offset = candidate + key.len();
    }
    None
}

fn find_ascii_case_insensitive(text: &str, needle: &str) -> Option<usize> {
    text.as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn replace_ascii_case_insensitive(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(index) = find_ascii_case_insensitive(remaining, needle) {
        output.push_str(&remaining[..index]);
        output.push_str(replacement);
        remaining = &remaining[index + needle.len()..];
    }
    output.push_str(remaining);
    output
}

fn redact_text_records(bytes: &[u8], private_roots: &[String]) -> Result<Vec<u8>, DiagnosticError> {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    for record in text.split_inclusive('\n') {
        let has_newline = record.ends_with('\n');
        let line = record.trim_end_matches(['\r', '\n']);
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) {
            redact_json(&mut value, private_roots);
            output.push_str(&serde_json::to_string(&value).map_err(DiagnosticError::Json)?);
        } else {
            let mut redacted = line.to_owned();
            for root in private_roots {
                redacted = replace_ascii_case_insensitive(&redacted, root, "<user-profile>");
            }
            output.push_str(&redact_bearer_text(&redacted));
        }
        if has_newline {
            output.push('\n');
        }
    }
    Ok(output.into_bytes())
}

fn enforce_total_limit(files: &mut Vec<CollectedFile>, omitted: &mut Vec<String>) {
    let mut total = 0_u64;
    files.retain(|file| {
        let size = file.bytes.len() as u64;
        if total.saturating_add(size) <= MAX_BUNDLE_BYTES.saturating_sub(1024 * 1024) {
            total = total.saturating_add(size);
            true
        } else {
            omitted.push(format!("{}:bundle_limit", file.relative_path.display()));
            false
        }
    });
}

fn append_tar_file<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &Path,
    bytes: &[u8],
    timestamp: i64,
) -> Result<(), DiagnosticError> {
    validate_relative(path)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(u64::try_from(timestamp).unwrap_or(0));
    header.set_cksum();
    archive
        .append_data(&mut header, path, bytes)
        .map_err(DiagnosticError::Archive)
}

fn validate_relative(path: &Path) -> Result<(), DiagnosticError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(DiagnosticError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn latest_run_id(paths: &hd_platform::DataPaths, instance_id: Uuid) -> Option<Uuid> {
    let root = paths.runs.join(instance_id.to_string());
    let mut runs = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            let id = entry.file_name().to_str()?.parse::<Uuid>().ok()?;
            Some((metadata.modified().ok(), id))
        })
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| right.0.cmp(&left.0));
    runs.first().map(|(_, id)| *id)
}

struct TemporaryGuard(Option<PathBuf>);

impl TemporaryGuard {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TemporaryGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiagnosticError {
    #[error("diagnostic source path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("diagnostic bundle exceeded {MAX_BUNDLE_BYTES} bytes: {0}")]
    BundleTooLarge(u64),
    #[error("diagnostic JSON failed: {0}")]
    Json(serde_json::Error),
    #[error("diagnostic archive failed: {0}")]
    Archive(std::io::Error),
    #[error("diagnostic compression failed: {0}")]
    Compress(std::io::Error),
    #[error("diagnostic archive hashing failed: {0}")]
    Hash(String),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_and_profile_are_redacted() {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:/Users/test".to_owned());
        let differently_cased_profile = profile.to_ascii_uppercase();
        let text = format!(
            "Authorization: bearer abc123 password=hunter2 client_secret=client-value \
             api_key=api-value secret_path={profile}/key {differently_cased_profile}/private"
        );
        let mut files = vec![CollectedFile {
            relative_path: PathBuf::from("log.txt"),
            bytes: text.into_bytes(),
            truncated: false,
        }];
        redact(&mut files).expect("redact");
        let output = String::from_utf8(files.remove(0).bytes).expect("utf8");
        assert!(!output.contains("abc123"));
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("client-value"));
        assert!(!output.contains("api-value"));
        assert!(!output.contains("/key"));
        assert!(
            !output
                .to_ascii_lowercase()
                .contains(&profile.to_ascii_lowercase())
        );
    }
}
