use crate::archive::{ArchiveEntry, EntryKind};
use crate::error::{CompanionError, Result};
use crate::manifest::{JobFile, JobManifest};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameArtifactKind {
    ExfatImage,
    FfpfscImage,
    FfpfsImage,
    FfpkgImage,
    GameFolder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameArtifact {
    pub path: PathBuf,
    pub kind: GameArtifactKind,
    pub title_id: Option<String>,
    /// Image extensions alone are insufficient proof of an image. This flag records
    /// whether the small known header check succeeded.
    pub verified_header: bool,
}

/// Validate the actual extraction tree against listing entries and compute SHA-256
/// digests for every regular file. No path in the tree is followed until its file type
/// has been checked, so symlinks and reparse points are rejected.
pub fn validate_extracted_tree(
    root: impl AsRef<Path>,
    expected: &[ArchiveEntry],
) -> Result<Vec<VerifiedFile>> {
    let root = root.as_ref();
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|source| CompanionError::io("inspect extraction root", source))?;
    ensure_directory_type(root, &root_metadata)?;

    let mut expected_map = HashMap::<String, (EntryKind, u64, Option<String>)>::new();
    for entry in expected {
        if expected_map
            .insert(
                entry.path.clone(),
                (entry.kind, entry.size, entry.sha256.clone()),
            )
            .is_some()
        {
            return Err(CompanionError::Validation(format!(
                "duplicate expected path {}",
                entry.path
            )));
        }
    }
    let mut expected_dirs = HashSet::new();
    let mut expected_files = HashSet::new();
    for (path, (kind, _, _)) in &expected_map {
        match kind {
            EntryKind::Directory => {
                expected_dirs.insert(path.clone());
            }
            EntryKind::File => {
                expected_files.insert(path.clone());
            }
        }
        let mut current = path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            expected_dirs.insert(parent.to_owned());
            current = parent;
        }
    }

    let mut actual_files = Vec::new();
    let mut actual_paths = HashSet::new();
    let mut actual_dirs = HashSet::new();
    walk_tree(
        root,
        "",
        &mut actual_files,
        &mut actual_paths,
        &mut actual_dirs,
        &expected_dirs,
        &expected_files,
    )?;

    let mut actual_by_path = HashMap::new();
    for file in &actual_files {
        actual_by_path.insert(file.path.clone(), file);
    }
    for (path, (kind, size, expected_sha)) in &expected_map {
        match kind {
            EntryKind::Directory => {
                if !actual_dirs.contains(path) {
                    return Err(CompanionError::Validation(format!(
                        "expected directory is missing: {path}"
                    )));
                }
            }
            EntryKind::File => {
                let Some(actual) = actual_by_path.get(path) else {
                    return Err(CompanionError::Validation(format!(
                        "expected file is missing: {path}"
                    )));
                };
                if actual.size != *size {
                    return Err(CompanionError::Validation(format!(
                        "size mismatch for {path}: expected {size}, got {}",
                        actual.size
                    )));
                }
                if let Some(expected_sha) = expected_sha {
                    if !expected_sha.eq_ignore_ascii_case(&actual.sha256) {
                        return Err(CompanionError::Validation(format!(
                            "SHA-256 mismatch for {path}"
                        )));
                    }
                }
            }
        }
    }
    actual_files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(actual_files)
}

/// Verify a previously written manifest against the tree again. This is useful for
/// callers that want to detect a post-extraction tamper before uploading an artifact.
pub fn verify_manifest_tree(
    root: impl AsRef<Path>,
    manifest: &JobManifest,
) -> Result<Vec<VerifiedFile>> {
    let root = root.as_ref();
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|source| CompanionError::io("inspect extraction root", source))?;
    ensure_directory_type(root, &root_metadata)?;
    let expected: HashMap<&str, &JobFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut actual = Vec::new();
    let mut actual_paths = HashSet::new();
    let mut actual_dirs = HashSet::new();
    let expected_dirs = expected
        .keys()
        .flat_map(|path| {
            let mut parents = Vec::new();
            let mut current = *path;
            while let Some((parent, _)) = current.rsplit_once('/') {
                parents.push(parent.to_owned());
                current = parent;
            }
            parents
        })
        .collect::<HashSet<_>>();
    let expected_files = expected.keys().map(|path| (*path).to_owned()).collect();
    walk_tree(
        root,
        "",
        &mut actual,
        &mut actual_paths,
        &mut actual_dirs,
        &expected_dirs,
        &expected_files,
    )?;
    if actual.len() != expected.len() {
        return Err(CompanionError::Validation(format!(
            "manifest file count mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        )));
    }
    for file in &actual {
        let Some(expected) = expected.get(file.path.as_str()) else {
            return Err(CompanionError::Validation(format!(
                "unexpected extracted file {}",
                file.path
            )));
        };
        if file.size != expected.size || !file.sha256.eq_ignore_ascii_case(&expected.sha256) {
            return Err(CompanionError::Validation(format!(
                "manifest digest or size mismatch for {}",
                file.path
            )));
        }
    }
    actual.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(actual)
}

/// Find a likely PS5 image or a folder-shaped game artifact without executing it.
pub fn identify_game_artifact(root: impl AsRef<Path>) -> Result<GameArtifact> {
    select_game_artifact(root.as_ref(), None)
}

/// One shared, conservative selector for extraction and upload planning.
pub fn select_game_artifact(root: &Path, expected_title: Option<&str>) -> Result<GameArtifact> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|source| CompanionError::io("inspect artifact root", source))?;
    ensure_directory_type(root, &metadata)?;
    let mut files = Vec::new();
    let mut paths = HashSet::new();
    collect_files(root, "", &mut files, &mut paths)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut candidates = Vec::new();

    for (relative, path) in &files {
        let lower = relative.to_ascii_lowercase();
        let kind = if lower.ends_with(".exfat") {
            Some(GameArtifactKind::ExfatImage)
        } else if lower.ends_with(".ffpfsc") {
            Some(GameArtifactKind::FfpfscImage)
        } else if lower.ends_with(".ffpfs") {
            Some(GameArtifactKind::FfpfsImage)
        } else if lower.ends_with(".ffpkg") {
            Some(GameArtifactKind::FfpkgImage)
        } else {
            None
        };
        if let Some(kind) = kind {
            let verified_header = image_header_matches(path, kind)?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let ids: HashSet<_> = stem
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|s| {
                    s.len() == 9
                        && s.starts_with("PPSA")
                        && s[4..].bytes().all(|b| b.is_ascii_digit())
                })
                .collect();
            candidates.push(GameArtifact {
                path: path.clone(),
                kind,
                title_id: if ids.len() == 1 {
                    ids.iter().next().map(|s| (*s).to_owned())
                } else {
                    None
                },
                verified_header,
            });
        }
    }

    for (relative, path) in &files {
        if !relative
            .rsplit('/')
            .next()
            .map(|name| name.eq_ignore_ascii_case("eboot.bin"))
            .unwrap_or(false)
        {
            continue;
        }
        let Some(game_root) = path.parent() else {
            continue;
        };
        let param_path = game_root.join("sce_sys").join("param.json");
        let param_metadata = match fs::symlink_metadata(&param_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        ensure_regular_file(&param_path, &param_metadata)?;
        if param_metadata.len() > 4 * 1024 * 1024 {
            return Err(CompanionError::Validation(
                "sce_sys/param.json exceeds the metadata size limit".to_owned(),
            ));
        }
        let content = fs::read_to_string(&param_path)
            .map_err(|source| CompanionError::io("read sce_sys/param.json", source))?;
        let json: Value = serde_json::from_str(&content)
            .map_err(|error| CompanionError::Validation(format!("parse param.json: {error}")))?;
        let title_id = find_title_id(&json).ok_or_else(|| {
            CompanionError::Validation("param.json does not contain a title id".to_owned())
        })?;
        candidates.push(GameArtifact {
            path: game_root.to_path_buf(),
            kind: GameArtifactKind::GameFolder,
            title_id: Some(title_id),
            verified_header: false,
        });
    }
    if candidates.is_empty() {
        return Err(CompanionError::ArtifactNotFound);
    }
    let all_paths: Vec<_> = candidates.iter().map(|c| c.path.clone()).collect();
    let parts: Vec<Vec<String>> = candidates
        .iter()
        .map(|c| {
            c.path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|p| p.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    let mut shared = 0;
    if parts.len() > 1 {
        while parts
            .iter()
            .all(|p| shared + 1 < p.len() && p[shared] == parts[0][shared])
        {
            shared += 1;
        }
    }
    if parts[0][..shared]
        .iter()
        .any(|segment| supplemental_segment(segment, false))
    {
        return Err(CompanionError::Validation(
            "archive wrapper contains only patch/update contents".to_owned(),
        ));
    }
    let multiple = candidates.len() > 1;
    let mut eligible: Vec<_> = candidates
        .into_iter()
        .zip(parts)
        .filter(|(_, parts)| {
            !parts[shared..]
                .iter()
                .any(|segment| supplemental_segment(segment, multiple))
        })
        .map(|(c, _)| c)
        .collect();
    if let Some(title) = expected_title {
        eligible.retain(|c| c.title_id.as_deref() == Some(title));
        if eligible.is_empty() {
            return Err(CompanionError::Validation("artifact title does not match the expected title ID, or contains only supplemental contents".to_owned()));
        }
    }
    if eligible.len() != 1 {
        return Err(CompanionError::Validation(format!(
            "expected exactly one unambiguous game artifact, found {}",
            eligible.len()
        )));
    }
    let selected = eligible.remove(0);
    if selected.kind == GameArtifactKind::GameFolder
        && all_paths
            .iter()
            .any(|p| p != &selected.path && p.starts_with(&selected.path))
    {
        return Err(CompanionError::Validation(
            "nested game artifacts remain ambiguous inside the selected game".to_owned(),
        ));
    }
    Ok(selected)
}

fn supplemental_segment(segment: &str, include_backport: bool) -> bool {
    let lower = segment.to_ascii_lowercase();
    // A merged base+update folder is still a base, not a standalone update.
    if lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|s| s == "base")
    {
        return false;
    }
    lower.char_indices().any(|(at, _)| {
        if at > 0 && lower.as_bytes()[at - 1].is_ascii_alphanumeric() {
            return false;
        }
        [
            "patch",
            "update",
            "upd",
            if include_backport { "backport" } else { "\0" },
        ]
        .iter()
        .any(|word| {
            let tail = &lower[at..];
            tail.starts_with(word)
                && tail
                    .as_bytes()
                    .get(word.len())
                    .map_or(true, |b| !b.is_ascii_alphabetic())
        })
    })
}

fn walk_tree(
    root: &Path,
    relative: &str,
    files: &mut Vec<VerifiedFile>,
    actual_paths: &mut HashSet<String>,
    actual_dirs: &mut HashSet<String>,
    expected_dirs: &HashSet<String>,
    expected_files: &HashSet<String>,
) -> Result<()> {
    let directory = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(native_relative_path(relative))
    };
    let iterator = fs::read_dir(&directory)
        .map_err(|source| CompanionError::io("read extraction directory", source))?;
    for item in iterator {
        let item = item.map_err(|source| CompanionError::io("read extraction entry", source))?;
        let name = item.file_name();
        let name = name.to_str().ok_or_else(|| {
            CompanionError::Validation("extraction contains a non-UTF-8 filename".to_owned())
        })?;
        validate_actual_component(name)?;
        let child_relative = if relative.is_empty() {
            name.to_owned()
        } else {
            format!("{relative}/{name}")
        };
        let metadata = fs::symlink_metadata(item.path())
            .map_err(|source| CompanionError::io("inspect extracted entry", source))?;
        if metadata.file_type().is_symlink() {
            return Err(CompanionError::Validation(format!(
                "symlink found in extraction: {child_relative}"
            )));
        }
        ensure_not_reparse(&metadata, &child_relative)?;
        if metadata.is_dir() {
            if !expected_dirs.contains(&child_relative) {
                return Err(CompanionError::Validation(format!(
                    "unexpected extracted directory: {child_relative}"
                )));
            }
            actual_dirs.insert(child_relative.clone());
            walk_tree(
                root,
                &child_relative,
                files,
                actual_paths,
                actual_dirs,
                expected_dirs,
                expected_files,
            )?;
        } else if metadata.is_file() {
            if !expected_files.contains(&child_relative) {
                return Err(CompanionError::Validation(format!(
                    "unexpected extracted file: {child_relative}"
                )));
            }
            if !actual_paths.insert(child_relative.clone()) {
                return Err(CompanionError::Validation(format!(
                    "duplicate extracted path: {child_relative}"
                )));
            }
            let size = metadata.len();
            let sha256 = sha256_file(&item.path())?;
            files.push(VerifiedFile {
                path: child_relative,
                size,
                sha256,
            });
        } else {
            return Err(CompanionError::Validation(format!(
                "special extracted entry: {child_relative}"
            )));
        }
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    relative: &str,
    files: &mut Vec<(String, PathBuf)>,
    paths: &mut HashSet<String>,
) -> Result<()> {
    let directory = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(native_relative_path(relative))
    };
    for item in fs::read_dir(&directory)
        .map_err(|source| CompanionError::io("read artifact directory", source))?
    {
        let item = item.map_err(|source| CompanionError::io("read artifact entry", source))?;
        let name = item.file_name();
        let name = name.to_str().ok_or_else(|| {
            CompanionError::Validation("artifact contains a non-UTF-8 filename".to_owned())
        })?;
        validate_actual_component(name)?;
        let child_relative = if relative.is_empty() {
            name.to_owned()
        } else {
            format!("{relative}/{name}")
        };
        let metadata = fs::symlink_metadata(item.path())
            .map_err(|source| CompanionError::io("inspect artifact entry", source))?;
        if metadata.file_type().is_symlink() {
            return Err(CompanionError::Validation(format!(
                "symlink found in artifact tree: {child_relative}"
            )));
        }
        ensure_not_reparse(&metadata, &child_relative)?;
        if metadata.is_dir() {
            collect_files(root, &child_relative, files, paths)?;
        } else if metadata.is_file() {
            if !paths.insert(child_relative.clone()) {
                return Err(CompanionError::Validation(format!(
                    "duplicate artifact path: {child_relative}"
                )));
            }
            files.push((child_relative, item.path()));
        } else {
            return Err(CompanionError::Validation(format!(
                "special artifact entry: {child_relative}"
            )));
        }
    }
    Ok(())
}

fn native_relative_path(relative: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for component in relative.split('/') {
        path.push(component);
    }
    path
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .map_err(|source| CompanionError::io("open extracted file for hashing", source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| CompanionError::io("hash extracted file", source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0f) as usize] as char);
    }
    text
}

fn image_header_matches(path: &Path, kind: GameArtifactKind) -> Result<bool> {
    let mut file = fs::File::open(path)
        .map_err(|source| CompanionError::io("open image candidate", source))?;
    let mut bytes = [0u8; 16];
    let count = file
        .read(&mut bytes)
        .map_err(|source| CompanionError::io("read image candidate header", source))?;
    Ok(match kind {
        GameArtifactKind::ExfatImage => count >= 11 && &bytes[3..11] == b"EXFAT   ",
        GameArtifactKind::FfpfscImage
        | GameArtifactKind::FfpfsImage
        | GameArtifactKind::FfpkgImage => {
            (count >= 7 && &bytes[3..7] == b"PFSC")
                || (count >= 4 && &bytes[..4] == b"PFSC")
                || crate::image::is_nested_pfsc_image(path)?
        }
        GameArtifactKind::GameFolder => false,
    })
}

fn find_title_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in ["titleId", "titleID", "title_id", "titleid", "TITLE_ID"] {
                if let Some(Value::String(title_id)) = object.get(key) {
                    if valid_title_id(title_id) {
                        return Some(title_id.clone());
                    }
                }
            }
            object.values().find_map(find_title_id)
        }
        Value::Array(values) => values.iter().find_map(find_title_id),
        _ => None,
    }
}

fn valid_title_id(value: &str) -> bool {
    (3..=128).contains(&value.len())
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn validate_actual_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(':')
        || component.ends_with('.')
        || component.ends_with(' ')
        || component.chars().any(|character| character.is_control())
        || is_windows_reserved_name(component)
    {
        return Err(CompanionError::Validation(format!(
            "unsafe extracted filename component {component:?}"
        )));
    }
    Ok(())
}

fn is_windows_reserved_name(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(component)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

fn ensure_directory_type(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure_not_reparse(metadata, &path.display().to_string())?;
    if !metadata.is_dir() {
        return Err(CompanionError::Validation(format!(
            "expected directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure_not_reparse(metadata, &path.display().to_string())?;
    if !metadata.is_file() {
        return Err(CompanionError::Validation(format!(
            "expected regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_not_reparse(metadata: &fs::Metadata, path: &str) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CompanionError::Validation(format!(
            "reparse point found at {path}"
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn ensure_not_reparse(_metadata: &fs::Metadata, _path: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveEntry;

    #[test]
    fn validates_sizes_and_digests() {
        let root =
            std::env::temp_dir().join(format!("ps5-companion-validate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sce_sys")).expect("fixture root");
        fs::write(root.join("eboot.bin"), b"boot").expect("eboot");
        fs::write(
            root.join("sce_sys").join("param.json"),
            br#"{"titleId":"PPSA00001"}"#,
        )
        .expect("param");
        let expected = vec![
            ArchiveEntry {
                path: "eboot.bin".to_owned(),
                size: 4,
                kind: EntryKind::File,
                sha256: None,
            },
            ArchiveEntry {
                path: "sce_sys".to_owned(),
                size: 0,
                kind: EntryKind::Directory,
                sha256: None,
            },
            ArchiveEntry {
                path: "sce_sys/param.json".to_owned(),
                size: 23,
                kind: EntryKind::File,
                sha256: None,
            },
        ];
        let files = validate_extracted_tree(&root, &expected).expect("valid tree");
        assert_eq!(files.len(), 2);
        let artifact = identify_game_artifact(&root).expect("game folder");
        assert_eq!(artifact.title_id.as_deref(), Some("PPSA00001"));
        fs::write(root.join("eboot.bin"), b"tampered").expect("tamper");
        assert!(validate_extracted_tree(&root, &expected).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn image_extension_is_reported_unverified_without_header() {
        let root = std::env::temp_dir().join(format!("ps5-companion-image-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        fs::write(root.join("candidate.ffpfsc"), b"tiny").expect("candidate");
        let artifact = identify_game_artifact(&root).expect("candidate");
        assert!(!artifact.verified_header);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_expected_empty_directory_is_rejected() {
        let root =
            std::env::temp_dir().join(format!("ps5-companion-empty-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        let expected = [ArchiveEntry {
            path: "empty".to_owned(),
            size: 0,
            kind: EntryKind::Directory,
            sha256: None,
        }];
        assert!(validate_extracted_tree(&root, &expected).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
