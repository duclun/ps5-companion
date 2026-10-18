use crate::download::BrowserHandoff;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Errors returned by the local inspection, extraction, and verification pipeline.
#[derive(Debug)]
pub enum CompanionError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    InvalidInput(String),
    InvalidListing(String),
    ListingTooLarge {
        limit: usize,
    },
    ListingTruncated,
    TooManyEntries {
        limit: usize,
    },
    SizeOverflow,
    UnsafeArchivePath(String),
    UnsafeEntry {
        path: String,
        reason: String,
    },
    EncryptedArchive,
    SevenZipFailed {
        program: PathBuf,
        code: Option<i32>,
        stderr: String,
    },
    SevenZipTimedOut {
        program: PathBuf,
        seconds: u64,
    },
    InsufficientSpace {
        required: u64,
        available: u64,
    },
    StagingExists(PathBuf),
    ManifestExists(PathBuf),
    Manifest(String),
    Validation(String),
    Download(String),
    /// A source that needs the user to complete verification in a browser
    /// and hand back the final file URL. Not a failure — a supported handoff.
    BrowserCapture(BrowserHandoff),
    Upload(String),
    ArtifactNotFound,
}

impl CompanionError {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for CompanionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{operation} failed: {source}"),
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::InvalidListing(message) => write!(f, "invalid 7-Zip listing: {message}"),
            Self::ListingTooLarge { limit } => {
                write!(f, "7-Zip listing exceeded the {limit} byte limit")
            }
            Self::ListingTruncated => write!(f, "7-Zip listing was truncated"),
            Self::TooManyEntries { limit } => {
                write!(f, "archive contains more than the {limit} supported entries")
            }
            Self::SizeOverflow => write!(f, "archive size total overflowed u64"),
            Self::UnsafeArchivePath(path) => write!(f, "unsafe archive path: {path}"),
            Self::UnsafeEntry { path, reason } => {
                write!(f, "unsafe archive entry {path}: {reason}")
            }
            Self::EncryptedArchive => write!(
                f,
                "encrypted archives are unsupported in this milestone; provide an unencrypted archive"
            ),
            Self::SevenZipFailed {
                program,
                code,
                stderr,
            } => write!(
                f,
                "7-Zip command {} failed with status {:?}: {}",
                program.display(),
                code,
                stderr
            ),
            Self::SevenZipTimedOut { program, seconds } => write!(
                f,
                "7-Zip command {} exceeded the {seconds} second timeout",
                program.display()
            ),
            Self::InsufficientSpace {
                required,
                available,
            } => write!(
                f,
                "insufficient free space: need {required} bytes, have {available}"
            ),
            Self::StagingExists(path) => {
                write!(f, "refusing to reuse existing staging path {}", path.display())
            }
            Self::ManifestExists(path) => {
                write!(f, "refusing to overwrite existing manifest {}", path.display())
            }
            Self::Manifest(message) => write!(f, "manifest error: {message}"),
            Self::Validation(message) => write!(f, "extraction validation failed: {message}"),
            Self::Download(message) => write!(f, "download error: {message}"),
            Self::BrowserCapture(handoff) => write!(
                f,
                "browser capture needed: {} — open {} in a browser, complete verification, then rerun with --direct-url <final-file-url>",
                handoff.reason, handoff.page_url
            ),
            Self::Upload(message) => write!(f, "upload error: {message}"),
            Self::ArtifactNotFound => write!(f, "no supported PS5 game artifact was found"),
        }
    }
}

impl std::error::Error for CompanionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, CompanionError>;
