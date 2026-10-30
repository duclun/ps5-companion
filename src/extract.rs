use crate::archive::{
    inspect_archive_with_config, resolve_first_volume, run_extract_command, ArchiveListing,
    SevenZipConfig,
};
use crate::error::{CompanionError, Result};
use crate::manifest::{manifest_path, path_string, write_manifest_atomic, JobFile, JobManifest};
use crate::validate::{select_game_artifact, validate_extracted_tree, GameArtifact, VerifiedFile};
use fs2::available_space;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_SAFETY_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// Inputs for one local extraction job. The job directory is derived under
/// `<workspace>/.ps5-companion/jobs/<job_id>` and is never reused.
#[derive(Debug, Clone)]
pub struct ExtractionRequest {
    pub archive_path: PathBuf,
    pub workspace: PathBuf,
    pub job_id: String,
    pub seven_zip: SevenZipConfig,
    pub safety_margin_bytes: u64,
    pub expected_title_id: Option<String>,
}

impl ExtractionRequest {
    pub fn new(
        archive_path: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        job_id: impl Into<String>,
    ) -> Self {
        Self {
            archive_path: archive_path.into(),
            workspace: workspace.into(),
            job_id: job_id.into(),
            seven_zip: SevenZipConfig::default(),
            safety_margin_bytes: DEFAULT_SAFETY_MARGIN_BYTES,
            expected_title_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub total_uncompressed_bytes: u64,
    pub required_bytes: u64,
    pub available_bytes: u64,
    pub file_count: usize,
}

#[derive(Debug, Clone)]
pub struct ExtractionResult {
    pub job_dir: PathBuf,
    pub output_root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: JobManifest,
    pub listing: ArchiveListing,
    pub artifact: Option<GameArtifact>,
    pub preflight: PreflightReport,
}

pub fn extract_archive(request: &ExtractionRequest) -> Result<ExtractionResult> {
    validate_job_id(&request.job_id)?;
    ensure_workspace(&request.workspace)?;
    let workspace = fs::canonicalize(&request.workspace)
        .map_err(|source| CompanionError::io("canonicalize workspace", source))?;
    let input_volume = resolve_first_volume(&request.archive_path)?;
    let input_metadata = fs::symlink_metadata(&input_volume)
        .map_err(|source| CompanionError::io("inspect input archive", source))?;
    if input_metadata.file_type().is_symlink() || !input_metadata.is_file() {
        return Err(CompanionError::InvalidInput(
            "input archive must be a regular file".to_owned(),
        ));
    }
    let input_archive = fs::canonicalize(&input_volume)
        .map_err(|source| CompanionError::io("canonicalize input archive", source))?;
    let input_for_manifest = path_string(&input_archive)?;
    let job_dir = workspace
        .join(".ps5-companion")
        .join("jobs")
        .join(&request.job_id);
    create_new_job_dir(&job_dir)?;
    let output_root = job_dir.join("staging");
    let manifest_file = manifest_path(&job_dir);
    let output_for_manifest = path_string(&output_root)?;

    let result = run_job(
        request,
        &input_archive,
        &job_dir,
        &output_root,
        &manifest_file,
        &input_for_manifest,
        &output_for_manifest,
    );
    match result {
        Ok(result) => Ok(result),
        Err(error) => {
            // The staging directory is deliberately preserved. A failed manifest is
            // best-effort so the original actionable error remains visible; an existing
            // manifest is never replaced.
            if !manifest_file.exists() {
                let failed = JobManifest::failed(
                    request.job_id.clone(),
                    input_for_manifest,
                    output_for_manifest,
                );
                let _ = write_manifest_atomic(&manifest_file, &failed);
            }
            Err(error)
        }
    }
}

fn run_job(
    request: &ExtractionRequest,
    input_archive: &Path,
    job_dir: &Path,
    output_root: &Path,
    manifest_file: &Path,
    input_for_manifest: &str,
    output_for_manifest: &str,
) -> Result<ExtractionResult> {
    let archive_metadata = fs::symlink_metadata(input_archive)
        .map_err(|source| CompanionError::io("inspect input archive", source))?;
    if archive_metadata.file_type().is_symlink() {
        return Err(CompanionError::InvalidInput(
            "input archive may not be a symbolic link".to_owned(),
        ));
    }
    if !archive_metadata.is_file() {
        return Err(CompanionError::InvalidInput(
            "input archive must be a regular file".to_owned(),
        ));
    }

    let listing = inspect_archive_with_config(input_archive, &request.seven_zip)?;
    let required_bytes = listing
        .total_size
        .checked_add(request.safety_margin_bytes)
        .ok_or(CompanionError::SizeOverflow)?;
    let available_bytes = available_space(job_dir)
        .map_err(|source| CompanionError::io("check available extraction space", source))?;
    let preflight = PreflightReport {
        total_uncompressed_bytes: listing.total_size,
        required_bytes,
        available_bytes,
        file_count: listing.file_count,
    };
    if available_bytes < required_bytes {
        return Err(CompanionError::InsufficientSpace {
            required: required_bytes,
            available: available_bytes,
        });
    }

    fs::create_dir(output_root).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            CompanionError::StagingExists(output_root.to_path_buf())
        } else {
            CompanionError::io("create extraction staging directory", source)
        }
    })?;
    run_extract_command(&request.seven_zip, input_archive, output_root)?;
    let verified = validate_extracted_tree(output_root, &listing.entries)?;
    let artifact = match select_game_artifact(output_root, request.expected_title_id.as_deref()) {
        Ok(artifact) => Some(artifact),
        Err(CompanionError::ArtifactNotFound) => None,
        Err(error) => return Err(error),
    };
    let files = verified.iter().map(job_file).collect::<Vec<_>>();
    let artifact_title = artifact.as_ref().and_then(|artifact| {
        artifact.title_id.clone().or_else(|| {
            artifact
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
    });
    let mut manifest = JobManifest::complete(
        request.job_id.clone(),
        input_for_manifest.to_owned(),
        output_for_manifest.to_owned(),
        files,
        artifact_title,
    );
    manifest.selected_artifact = artifact.as_ref().map(|a| {
        a.path
            .strip_prefix(output_root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    });
    write_manifest_atomic(manifest_file, &manifest)?;
    Ok(ExtractionResult {
        job_dir: job_dir.to_path_buf(),
        output_root: output_root.to_path_buf(),
        manifest_path: manifest_file.to_path_buf(),
        manifest,
        listing,
        artifact,
        preflight,
    })
}

fn job_file(file: &VerifiedFile) -> JobFile {
    JobFile {
        path: file.path.clone(),
        size: file.size,
        sha256: file.sha256.clone(),
    }
}

fn ensure_workspace(workspace: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(workspace)
        .map_err(|source| CompanionError::io("inspect workspace", source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err(CompanionError::InvalidInput(format!(
            "workspace is not a regular directory: {}",
            workspace.display()
        )));
    }
    Ok(())
}

fn create_new_job_dir(job_dir: &Path) -> Result<()> {
    let jobs_dir = job_dir
        .parent()
        .ok_or_else(|| CompanionError::InvalidInput("job path has no parent".to_owned()))?;
    let app_dir = jobs_dir
        .parent()
        .ok_or_else(|| CompanionError::InvalidInput("jobs path has no parent".to_owned()))?;
    ensure_or_create_directory(app_dir)?;
    ensure_or_create_directory(jobs_dir)?;
    fs::create_dir(job_dir).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            CompanionError::StagingExists(job_dir.to_path_buf())
        } else {
            CompanionError::io("create job directory", source)
        }
    })
}

fn ensure_or_create_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || is_reparse_point(&metadata)
            {
                return Err(CompanionError::InvalidInput(format!(
                    "job path component is not a regular directory: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)
            .map_err(|source| CompanionError::io("create job path component", source)),
        Err(source) => Err(CompanionError::io("inspect job path component", source)),
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty()
        || job_id == "."
        || job_id == ".."
        || job_id.len() > 128
        || !job_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CompanionError::InvalidInput(
            "job id must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_like_job_ids() {
        for value in ["", ".", "..", "a/b", "a\\b", "job:1"] {
            assert!(validate_job_id(value).is_err());
        }
        assert!(validate_job_id("job-1_ok").is_ok());
    }
}
