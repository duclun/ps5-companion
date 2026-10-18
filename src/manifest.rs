use crate::error::{CompanionError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_FILE_NAME: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Inspecting,
    Preflight,
    Extracting,
    Verifying,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobManifest {
    pub schema_version: u32,
    pub job_id: String,
    pub state: JobState,
    pub input_archive_path: String,
    pub output_root: String,
    pub files: Vec<JobFile>,
    pub artifact_title: Option<String>,
}

impl JobManifest {
    pub fn failed(
        job_id: impl Into<String>,
        input_archive_path: impl Into<String>,
        output_root: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            job_id: job_id.into(),
            state: JobState::Failed,
            input_archive_path: input_archive_path.into(),
            output_root: output_root.into(),
            files: Vec::new(),
            artifact_title: None,
        }
    }

    pub fn complete(
        job_id: impl Into<String>,
        input_archive_path: impl Into<String>,
        output_root: impl Into<String>,
        files: Vec<JobFile>,
        artifact_title: Option<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            job_id: job_id.into(),
            state: JobState::Complete,
            input_archive_path: input_archive_path.into(),
            output_root: output_root.into(),
            files,
            artifact_title,
        }
    }
}

/// Write a manifest exactly once. A create-new lock serializes cooperating writers for
/// the newly-created job directory, then a flushed temporary file is renamed into place.
/// The job directory is owned by this extraction job, and an existing destination is
/// always rejected before the rename. This uses only portable create/rename operations,
/// including on exFAT volumes where hard links are unavailable.
pub fn write_manifest_atomic(path: impl AsRef<Path>, manifest: &JobManifest) -> Result<()> {
    let path = path.as_ref();
    if path.exists() {
        return Err(CompanionError::ManifestExists(path.to_path_buf()));
    }
    if manifest.job_id.is_empty()
        || manifest.job_id.contains('/')
        || manifest.job_id.contains('\\')
        || manifest.job_id == "."
        || manifest.job_id == ".."
    {
        return Err(CompanionError::Manifest("invalid job id".to_owned()));
    }
    for value in [&manifest.input_archive_path, &manifest.output_root] {
        if value.contains("://") || value.contains('\0') {
            return Err(CompanionError::Manifest(
                "manifest paths may not contain URLs or NUL bytes".to_owned(),
            ));
        }
    }
    for file in &manifest.files {
        if file.path.contains("://") || file.path.contains('\0') || file.sha256.len() != 64 {
            return Err(CompanionError::Manifest(
                "manifest contains an invalid file entry".to_owned(),
            ));
        }
    }
    let parent = path.parent().ok_or_else(|| {
        CompanionError::Manifest("manifest path has no parent directory".to_owned())
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| CompanionError::io("create manifest directory", source))?;
    let lock_path = parent.join(".manifest.lock");
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                CompanionError::ManifestExists(path.to_path_buf())
            } else {
                CompanionError::io("create manifest lock", source)
            }
        })?;
    drop(lock);
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        let _ = fs::remove_file(&lock_path);
        CompanionError::Manifest(format!("serialize manifest: {error}"))
    })?;

    let pid = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary = path.to_path_buf();
    temporary.set_file_name(format!(
        ".{}.tmp-{pid}-{nonce}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest")
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| {
            let _ = fs::remove_file(&lock_path);
            CompanionError::io("create temporary manifest", source)
        })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(&lock_path);
        return Err(CompanionError::io("flush temporary manifest", source));
    }
    drop(file); // close before rename on Windows.
    if path.exists() {
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(&lock_path);
        return Err(CompanionError::ManifestExists(path.to_path_buf()));
    }
    let result = fs::rename(&temporary, path);
    let _ = fs::remove_file(&lock_path);
    result.map_err(|source| {
        if path.exists() {
            CompanionError::ManifestExists(path.to_path_buf())
        } else {
            CompanionError::io("atomically install manifest", source)
        }
    })
}

pub(crate) fn path_string(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        CompanionError::Manifest(format!("path is not valid UTF-8: {}", path.display()))
    })
}

pub(crate) fn manifest_path(job_dir: &Path) -> PathBuf {
    job_dir.join(MANIFEST_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_json_and_does_not_overwrite() {
        let root =
            std::env::temp_dir().join(format!("ps5-companion-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test directory");
        let path = root.join(MANIFEST_FILE_NAME);
        let manifest = JobManifest::failed("job", "C:\\input.7z", "C:\\output");
        write_manifest_atomic(&path, &manifest).expect("write manifest");
        assert!(write_manifest_atomic(&path, &manifest).is_err());
        let parsed: JobManifest =
            serde_json::from_slice(&fs::read(&path).expect("manifest bytes")).expect("valid json");
        assert_eq!(parsed.state, JobState::Failed);
        let _ = fs::remove_dir_all(&root);
    }
}
