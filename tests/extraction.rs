use ps5_companion::{
    extract_archive, inspect_archive_with_tool, verify_manifest_tree, CompanionError,
    ExtractionRequest, JobState, SevenZipConfig,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn seven_zip() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PS5_COMPANION_7Z") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        panic!("PS5_COMPANION_7Z was set but does not name a regular file");
    }
    let status = Command::new("7z").arg("-h").status().ok()?;
    status.success().then(|| PathBuf::from("7z"))
}

fn fixture_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ps5-companion-{name}-{}-{nonce}",
        std::process::id()
    ))
}

fn archive_fixture(tool: &Path, root: &Path, name: &str, payload: &[u8]) -> PathBuf {
    fs::create_dir_all(root).expect("fixture root");
    fs::write(root.join(name), payload).expect("fixture file");
    let archive = root.join("fixture.7z");
    let status = Command::new(tool)
        .current_dir(root)
        .args(["a", "-t7z", "fixture.7z", "--", name])
        .status()
        .expect("create archive");
    assert!(status.success(), "7z failed to create test archive");
    archive
}

#[test]
fn actual_7z_valid_extraction_and_tamper_detection() {
    let Some(tool) = seven_zip() else {
        eprintln!("skipping actual 7-Zip integration test: 7z unavailable");
        return;
    };
    let root = fixture_dir("valid");
    let archive = archive_fixture(&tool, &root, "--literal.bin", b"safe fixture");
    let listing = inspect_archive_with_tool(&archive, &tool).expect("inspect fixture");
    assert_eq!(listing.file_count, 1);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let mut request = ExtractionRequest::new(&archive, &workspace, "valid-job");
    request.seven_zip = SevenZipConfig {
        executable: tool,
        listing_limit: ps5_companion::MAX_LISTING_BYTES,
    };
    request.safety_margin_bytes = 0;
    let result = extract_archive(&request).expect("extract fixture");
    assert_eq!(result.manifest.state, JobState::Complete);
    verify_manifest_tree(&result.output_root, &result.manifest).expect("verify fixture");
    let file = result.output_root.join("--literal.bin");
    fs::write(&file, b"tampered xxx").expect("tamper fixture");
    assert!(verify_manifest_tree(&result.output_root, &result.manifest).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn actual_7z_failure_writes_failed_manifest_and_keeps_job() {
    let Some(tool) = seven_zip() else {
        eprintln!("skipping actual 7-Zip failure test: 7z unavailable");
        return;
    };
    let root = fixture_dir("failure");
    fs::create_dir_all(&root).expect("fixture root");
    let archive = root.join("broken.7z");
    fs::write(&archive, b"not a 7z archive").expect("broken archive");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let mut request = ExtractionRequest::new(&archive, &workspace, "failed-job");
    request.seven_zip.executable = tool;
    request.safety_margin_bytes = 0;
    assert!(extract_archive(&request).is_err());
    let manifest = workspace
        .join(".ps5-companion")
        .join("jobs")
        .join("failed-job")
        .join("manifest.json");
    let bytes = fs::read(&manifest).expect("failed manifest");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("manifest JSON");
    assert_eq!(value["state"], "failed");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn actual_7z_preflight_can_reject_without_staging() {
    let Some(tool) = seven_zip() else {
        eprintln!("skipping actual 7-Zip preflight test: 7z unavailable");
        return;
    };
    let root = fixture_dir("space");
    let archive = archive_fixture(&tool, &root, "payload.bin", b"space fixture");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let listing = inspect_archive_with_tool(&archive, &tool).expect("inspect fixture");
    let available = fs2::available_space(&workspace).expect("available space");
    let mut request = ExtractionRequest::new(&archive, &workspace, "space-job");
    request.seven_zip.executable = tool;
    request.safety_margin_bytes = available
        .saturating_sub(listing.total_size)
        .saturating_add(1);
    assert!(matches!(
        extract_archive(&request),
        Err(CompanionError::InsufficientSpace { .. })
    ));
    assert!(!workspace
        .join(".ps5-companion")
        .join("jobs")
        .join("space-job")
        .join("staging")
        .exists());
    let _ = fs::remove_dir_all(root);
}
