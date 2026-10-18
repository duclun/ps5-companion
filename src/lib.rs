//! Safe, local-only archive inspection and extraction primitives for PS5 Companion.
//!
//! The library deliberately delegates decompression to a locally installed 7-Zip
//! executable. It never downloads archives, transfers them to a console, or executes
//! anything from an extracted tree.

pub mod archive;
pub mod error;
pub mod extract;
pub mod manifest;
pub mod upload;
pub mod validate;

pub use archive::{
    inspect_archive, inspect_archive_with_tool, parse_7z_listing, ArchiveEntry, ArchiveListing,
    EntryKind, SevenZipConfig, MAX_ARCHIVE_ENTRIES, MAX_LISTING_BYTES,
};
pub use error::{CompanionError, Result};
pub use extract::{extract_archive, ExtractionRequest, ExtractionResult, PreflightReport};
pub use manifest::{write_manifest_atomic, JobFile, JobManifest, JobState, MANIFEST_FILE_NAME};
pub use upload::{
    load_upload_manifest, plan_upload, TransferReceipt, TransferReceiptFile, UploadArtifact,
    UploadArtifactKind, UploadClient, UploadPlan, UploadPlanFile, UploadPollResult,
    UploadPollStatus, UploadPreflight, UploadSubmission, PS5_DEST_ROOT, PS5_STAGE_ROOT,
    TRANSFER_RECEIPT_FILE_NAME,
};
pub use validate::{
    identify_game_artifact, validate_extracted_tree, verify_manifest_tree, GameArtifact,
    GameArtifactKind, VerifiedFile,
};
