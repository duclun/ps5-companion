use crate::error::{CompanionError, Result};
use std::collections::HashSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Maximum amount of listing output accepted from 7-Zip.
pub const MAX_LISTING_BYTES: usize = 32 * 1024 * 1024;
/// A corrupt archive should not be able to make inspection allocate without bound.
pub const MAX_ARCHIVE_ENTRIES: usize = 1_000_000;
const MAX_STDERR_BYTES: usize = 4 * 1024 * 1024;
const LISTING_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for the external 7-Zip process.
#[derive(Debug, Clone)]
pub struct SevenZipConfig {
    pub executable: PathBuf,
    pub listing_limit: usize,
}

impl Default for SevenZipConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("7z"),
            listing_limit: MAX_LISTING_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// A normalized, relative path using `/` separators.
    pub path: String,
    pub size: u64,
    pub kind: EntryKind,
    /// A SHA-256 can be supplied by a caller that has an external expected manifest.
    /// 7-Zip's normal listing does not provide one, so this is `None` after inspection.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    /// Physical bytes reported by 7-Zip, when present in `-slt` output.
    pub archive_bytes: Option<u64>,
    /// Sum of uncompressed regular-file sizes.
    pub total_size: u64,
    pub file_count: usize,
    pub entries: Vec<ArchiveEntry>,
    pub encrypted: bool,
    pub multipart: bool,
}

impl ArchiveListing {
    pub fn files(&self) -> impl Iterator<Item = &ArchiveEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
    }
}

/// Inspect an archive using the default local `7z` executable.
pub fn inspect_archive(path: impl AsRef<Path>) -> Result<ArchiveListing> {
    inspect_archive_with_config(path.as_ref(), &SevenZipConfig::default())
}

/// Inspect an archive using an explicitly selected executable, useful for a configured
/// installation such as `7z.exe` on Windows.
pub fn inspect_archive_with_tool(
    path: impl AsRef<Path>,
    executable: impl AsRef<Path>,
) -> Result<ArchiveListing> {
    let config = SevenZipConfig {
        executable: executable.as_ref().to_path_buf(),
        ..SevenZipConfig::default()
    };
    inspect_archive_with_config(path.as_ref(), &config)
}

pub(crate) fn inspect_archive_with_config(
    path: &Path,
    config: &SevenZipConfig,
) -> Result<ArchiveListing> {
    let archive = resolve_first_volume(path)?;
    let metadata = std::fs::metadata(&archive)
        .map_err(|source| CompanionError::io("read archive metadata", source))?;
    if !metadata.is_file() {
        return Err(CompanionError::InvalidInput(format!(
            "archive path is not a regular file: {}",
            archive.display()
        )));
    }

    let mut child = Command::new(&config.executable)
        .arg("l")
        .arg("-slt")
        .arg("-sccUTF-8")
        .arg("--")
        .arg(&archive)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CompanionError::io("start 7-Zip listing", source))?;

    let stderr = child.stderr.take();
    let stderr_thread =
        stderr.map(|pipe| thread::spawn(move || read_bounded_reader(pipe, MAX_STDERR_BYTES)));
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| CompanionError::InvalidListing("7-Zip stdout was not piped".to_owned()))?;
    let listing_limit = config.listing_limit;
    let stdout_thread = thread::spawn(move || {
        let mut pipe = stdout_pipe;
        read_bounded(&mut pipe, listing_limit)
    });
    let started = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|source| CompanionError::io("poll 7-Zip listing", source))?
        {
            Some(status) => break status,
            None if started.elapsed() >= LISTING_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                if let Some(thread) = stderr_thread {
                    let _ = thread.join();
                }
                return Err(CompanionError::SevenZipTimedOut {
                    program: config.executable.clone(),
                    seconds: LISTING_TIMEOUT.as_secs(),
                });
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| CompanionError::InvalidListing("7-Zip stdout reader panicked".to_owned()))??;
    let stderr = stderr_thread
        .and_then(|thread| thread.join().ok())
        .and_then(|result| result.ok())
        .unwrap_or_default();

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr).trim().to_owned();
        if listing_mentions_encryption(&stdout) || listing_mentions_encryption(&stderr) {
            return Err(CompanionError::EncryptedArchive);
        }
        return Err(CompanionError::SevenZipFailed {
            program: config.executable.clone(),
            code: status.code(),
            stderr: stderr_text,
        });
    }

    let mut listing = parse_7z_listing_with_limit(&stdout, config.listing_limit)?;
    if listing.archive_bytes.is_none() {
        listing.archive_bytes = Some(metadata.len());
    }
    listing.multipart = is_multipart_first_volume(&archive);
    if listing.encrypted {
        return Err(CompanionError::EncryptedArchive);
    }
    Ok(listing)
}

/// Parse UTF-8 `7z l -slt` output without executing a process.
pub fn parse_7z_listing(bytes: &[u8]) -> Result<ArchiveListing> {
    parse_7z_listing_with_limit(bytes, MAX_LISTING_BYTES)
}

pub(crate) fn parse_7z_listing_with_limit(
    bytes: &[u8],
    listing_limit: usize,
) -> Result<ArchiveListing> {
    if bytes.len() > listing_limit {
        return Err(CompanionError::ListingTooLarge {
            limit: listing_limit,
        });
    }
    if bytes.is_empty() {
        return Err(CompanionError::ListingTruncated);
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        CompanionError::InvalidListing(format!("listing is not UTF-8: {error}"))
    })?;

    let mut blocks: Vec<Vec<(String, String)>> = Vec::new();
    let mut current = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim();
        if trimmed.is_empty() || is_listing_separator(trimmed) {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let value = if key.eq_ignore_ascii_case("path") {
                value.trim_start().to_owned()
            } else {
                value.trim().to_owned()
            };
            current.push((key.to_ascii_lowercase(), value));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    if blocks.is_empty() {
        return Err(CompanionError::ListingTruncated);
    }

    let mut archive_bytes = None;
    let mut entries = Vec::new();
    let mut encrypted = false;
    let mut seen_paths = HashSet::new();
    let mut seen_windows_paths = HashSet::new();

    for block in blocks {
        let mut values = std::collections::HashMap::<&str, &str>::new();
        for (key, value) in &block {
            values.insert(key.as_str(), value.as_str());
        }
        if values
            .get("encrypted")
            .map(|value| is_true_flag(value))
            .unwrap_or(false)
        {
            encrypted = true;
        }
        if archive_bytes.is_none() {
            archive_bytes = values
                .get("physical size")
                .and_then(|value| parse_u64(value).ok());
        }

        let Some(raw_path) = values.get("path") else {
            continue;
        };
        // The first `Path` block in `-slt` output describes the archive itself. It has
        // Type/Physical Size but no member Attributes/Folder/CRC and is not an entry.
        if !values.contains_key("attributes")
            && !values.contains_key("folder")
            && !values.contains_key("crc")
        {
            continue;
        }
        let path = validate_archive_path(raw_path)?;
        let size = values
            .get("size")
            .ok_or_else(|| CompanionError::InvalidListing(format!("missing size for {path}")))
            .and_then(|value| parse_u64(value))?;
        if entries.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(CompanionError::TooManyEntries {
                limit: MAX_ARCHIVE_ENTRIES,
            });
        }
        if !seen_paths.insert(path.clone()) || !seen_windows_paths.insert(path.to_lowercase()) {
            return Err(CompanionError::UnsafeEntry {
                path,
                reason: "duplicate archive path (including a Windows case collision)".to_owned(),
            });
        }
        let kind = classify_entry(&values, &path)?;
        if values
            .get("encrypted")
            .map(|value| is_true_flag(value))
            .unwrap_or(false)
        {
            encrypted = true;
        }
        entries.push(ArchiveEntry {
            path,
            size,
            kind,
            sha256: None,
        });
    }

    if entries.is_empty() {
        return Err(CompanionError::ListingTruncated);
    }
    let mut total_size = 0u64;
    let mut file_count = 0usize;
    for entry in &entries {
        if entry.kind == EntryKind::File {
            total_size = total_size
                .checked_add(entry.size)
                .ok_or(CompanionError::SizeOverflow)?;
            file_count += 1;
        }
    }
    Ok(ArchiveListing {
        archive_bytes,
        total_size,
        file_count,
        entries,
        encrypted,
        multipart: false,
    })
}

pub(crate) fn validate_archive_path(raw: &str) -> Result<String> {
    if raw.is_empty() || raw.len() > 32 * 1024 || raw.contains('\0') {
        return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
    }
    if raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.starts_with("//")
        || raw.starts_with("\\\\")
        || (raw.as_bytes().get(1) == Some(&b':'))
    {
        return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
    }
    let mut normalized = String::with_capacity(raw.len());
    let mut component_count = 0usize;
    let mut previous_was_separator = false;
    for component in raw.split(['/', '\\']) {
        component_count += 1;
        if component_count > 128 {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if component.is_empty() {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if component == "." || component == ".." || component.contains(':') {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if is_windows_reserved_name(component) {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if component.chars().any(|character| character.is_control()) {
            return Err(CompanionError::UnsafeArchivePath(raw.to_owned()));
        }
        if previous_was_separator {
            normalized.push('/');
        }
        normalized.push_str(component);
        previous_was_separator = true;
    }
    Ok(normalized)
}

fn classify_entry(values: &std::collections::HashMap<&str, &str>, path: &str) -> Result<EntryKind> {
    let dangerous_keys = [
        "symbolic link",
        "hard link",
        "reparse point",
        "reparse",
        "special",
        "device",
    ];
    if dangerous_keys.iter().any(|key| values.contains_key(key)) {
        return Err(CompanionError::UnsafeEntry {
            path: path.to_owned(),
            reason: "symbolic link, hard link, reparse point, or special entry".to_owned(),
        });
    }
    if let Some(entry_type) = values.get("type") {
        let lower = entry_type.to_ascii_lowercase();
        if lower.contains("symbol")
            || lower.contains("hard")
            || lower.contains("reparse")
            || lower.contains("device")
            || lower.contains("special")
        {
            return Err(CompanionError::UnsafeEntry {
                path: path.to_owned(),
                reason: format!("unsupported entry type {entry_type}"),
            });
        }
    }
    let is_directory = values
        .get("attributes")
        .map(|attributes| attributes.to_ascii_uppercase().contains('D'))
        .unwrap_or(false)
        || values
            .get("type")
            .map(|entry_type| {
                matches!(
                    entry_type.to_ascii_lowercase().as_str(),
                    "d" | "dir" | "directory"
                )
            })
            .unwrap_or(false)
        || values
            .get("folder")
            .map(|folder| *folder == "+")
            .unwrap_or(false);
    Ok(if is_directory {
        EntryKind::Directory
    } else {
        EntryKind::File
    })
}

fn parse_u64(value: &str) -> Result<u64> {
    value.trim().parse::<u64>().map_err(|error| {
        CompanionError::InvalidListing(format!("invalid unsigned number {value:?}: {error}"))
    })
}

fn is_true_flag(value: &str) -> bool {
    let value = value.trim();
    value == "+"
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("encrypted")
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

fn listing_mentions_encryption(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let positive_field = lower
            .split_once('=')
            .map(|(key, value)| key.trim() == "encrypted" && is_true_flag(value.trim()))
            .unwrap_or(false);
        if positive_field {
            return true;
        }
        lower.contains("password")
            || (lower.contains("encrypted")
                && (lower.contains("archive") || lower.contains("header")))
    })
}

fn is_listing_separator(line: &str) -> bool {
    line == "--" || line.chars().all(|character| character == '-')
}

fn read_bounded<R: Read>(reader: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0u8; 32 * 1024];
    loop {
        let count = reader
            .read(&mut chunk)
            .map_err(|source| CompanionError::io("read 7-Zip output", source))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(CompanionError::ListingTooLarge { limit });
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

fn read_bounded_reader<R: Read>(mut reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0u8; 32 * 1024];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() < limit {
            let remaining = limit - output.len();
            output.extend_from_slice(&chunk[..count.min(remaining)]);
        }
        // Continue draining after the diagnostic cap so the child cannot block on a
        // full stderr pipe. Only the bounded prefix is retained.
    }
}

pub(crate) fn resolve_first_volume(path: &Path) -> Result<PathBuf> {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return Ok(path.to_path_buf());
    };
    if extension.len() != 3
        || !extension
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Ok(path.to_path_buf());
    }
    let number = extension
        .parse::<u16>()
        .map_err(|_| CompanionError::InvalidInput("invalid multipart volume suffix".to_owned()))?;
    if number <= 1 {
        return Ok(path.to_path_buf());
    }
    let first = path.with_extension("001");
    if first.is_file() {
        Ok(first)
    } else {
        Err(CompanionError::InvalidInput(format!(
            "multipart archive starts at volume {number}; first volume {} is missing",
            first.display()
        )))
    }
}

fn is_multipart_first_volume(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.len() == 3 && extension.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// Run the extraction command with bounded diagnostics. This helper is intentionally
/// crate-visible: callers should use `extract_archive`, which performs all checks around it.
pub(crate) fn run_extract_command(
    config: &SevenZipConfig,
    archive: &Path,
    output_root: &Path,
) -> Result<()> {
    let mut child = Command::new(&config.executable)
        .arg("x")
        .arg("-y")
        .arg("-sccUTF-8")
        .arg(format!("-o{}", output_root.display()))
        .arg("--")
        .arg(archive)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CompanionError::io("start 7-Zip extraction", source))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CompanionError::InvalidListing("7-Zip stdout was not piped".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CompanionError::InvalidListing("7-Zip stderr was not piped".to_owned()))?;
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    let stdout_thread = spawn_bounded_diagnostic_reader(stdout, stdout_sender);
    let stderr_thread = spawn_bounded_diagnostic_reader(stderr, stderr_sender);
    let status = child
        .wait()
        .map_err(|source| CompanionError::io("wait for 7-Zip extraction", source))?;
    let stdout_text = stdout_receiver.recv().unwrap_or_default();
    let stderr_text = stderr_receiver.recv().unwrap_or_default();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    if !status.success() {
        let diagnostics = if stderr_text.is_empty() {
            stdout_text
        } else {
            stderr_text
        };
        return Err(CompanionError::SevenZipFailed {
            program: config.executable.clone(),
            code: status.code(),
            stderr: diagnostics,
        });
    }
    Ok(())
}

fn spawn_bounded_diagnostic_reader<R: Read + Send + 'static>(
    mut reader: R,
    sender: mpsc::Sender<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 32 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let remaining = MAX_STDERR_BYTES.saturating_sub(bytes.len());
                    if remaining > 0 {
                        bytes.extend_from_slice(&chunk[..count.min(remaining)]);
                    }
                }
            }
        }
        let _ = sender.send(String::from_utf8_lossy(&bytes).trim().to_owned());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(path: &str, size: &str, attrs: &str) -> String {
        format!("Path = {path}\nSize = {size}\nAttributes = {attrs}\n\n")
    }

    #[test]
    fn parses_entries_and_totals() {
        let data = format!(
            "7-Zip\nPhysical Size = 99\n\n{}{}",
            listing("folder", "0", "D"),
            listing("folder/file.bin", "4", "A")
        );
        let parsed = parse_7z_listing(data.as_bytes()).expect("listing parses");
        assert_eq!(parsed.archive_bytes, Some(99));
        assert_eq!(parsed.total_size, 4);
        assert_eq!(parsed.file_count, 1);
        assert_eq!(parsed.entries[1].path, "folder/file.bin");
    }

    #[test]
    fn rejects_traversal_and_ads_paths() {
        for path in [
            "../escape",
            "folder/../../escape",
            r"C:\escape",
            r"\\server\share\escape",
            "dir/file:stream",
            "trailing ",
            "CON.txt",
            "COM1",
        ] {
            let data = listing(path, "1", "A");
            assert!(matches!(
                parse_7z_listing(data.as_bytes()),
                Err(CompanionError::UnsafeArchivePath(_))
            ));
        }
    }

    #[test]
    fn rejects_duplicate_case_collisions() {
        let data = format!(
            "{}{}",
            listing("A.bin", "1", "A"),
            listing("a.bin", "1", "A")
        );
        assert!(matches!(
            parse_7z_listing(data.as_bytes()),
            Err(CompanionError::UnsafeEntry { .. })
        ));
    }

    #[test]
    fn rejects_overflow_and_oversize_listing() {
        let data = format!(
            "{}{}",
            listing("a", u64::MAX.to_string().as_str(), "A"),
            listing("b", "1", "A")
        );
        assert!(matches!(
            parse_7z_listing(data.as_bytes()),
            Err(CompanionError::SizeOverflow)
        ));
        assert!(matches!(
            parse_7z_listing_with_limit(b"x", 0),
            Err(CompanionError::ListingTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_link_and_special_metadata() {
        let data = "Path = link\nSize = 0\nAttributes = A\nSymbolic Link = target\n";
        assert!(matches!(
            parse_7z_listing(data.as_bytes()),
            Err(CompanionError::UnsafeEntry { .. })
        ));
        let data = "Path = device\nSize = 0\nAttributes = A\nType = special\n";
        assert!(matches!(
            parse_7z_listing(data.as_bytes()),
            Err(CompanionError::UnsafeEntry { .. })
        ));
    }

    #[test]
    fn records_encryption_for_actionable_rejection() {
        let data = "Path = secret\nSize = 1\nAttributes = A\nEncrypted = +\n";
        let listing = parse_7z_listing(data.as_bytes()).expect("parse encrypted listing");
        assert!(listing.encrypted);
    }
}
