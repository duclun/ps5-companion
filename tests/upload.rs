mod common;

use common::{CapturedRequest, MockEngine};
use ps5_companion::{
    plan_upload, write_manifest_atomic, JobFile, JobManifest, JobState, TransferReceipt,
    UploadArtifactKind, UploadClient, UploadPollStatus, PS5_DEST_ROOT,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    manifest: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ps5-companion-upload-{name}-{}-{nanos}-{id}",
        std::process::id()
    ))
}

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn job_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = fixture_root(name);
    let job_dir = root.join(".ps5-companion").join("jobs").join("job-1");
    let output = job_dir.join("staging");
    fs::create_dir_all(&output).expect("fixture staging root");
    (root, job_dir, output)
}

fn write_folder_fixture(name: &str) -> Fixture {
    let (root, job_dir, output) = job_fixture(name);
    let game = output.join("Synthetic Game");
    fs::create_dir_all(game.join("sce_sys")).expect("fixture game tree");
    let eboot = b"synthetic eboot";
    let param = br#"{"titleId":"PPSA00001"}"#;
    fs::write(game.join("eboot.bin"), eboot).expect("fixture eboot");
    fs::write(game.join("sce_sys").join("param.json"), param).expect("fixture param");
    let manifest = complete_manifest(
        &job_dir,
        &output,
        vec![
            ("Synthetic Game/eboot.bin", eboot.to_vec()),
            ("Synthetic Game/sce_sys/param.json", param.to_vec()),
        ],
    );
    Fixture { root, manifest }
}

fn write_image_fixture(name: &str, verified: bool) -> Fixture {
    let (root, job_dir, output) = job_fixture(name);
    let bytes = if verified {
        b"PFSC synthetic image".to_vec()
    } else {
        b"not a verified image".to_vec()
    };
    let path = output.join("synthetic.ffpfsc");
    fs::write(&path, &bytes).expect("fixture image");
    let manifest = complete_manifest(&job_dir, &output, vec![("synthetic.ffpfsc", bytes)]);
    Fixture { root, manifest }
}

fn complete_manifest(job_dir: &Path, output: &Path, files: Vec<(&str, Vec<u8>)>) -> PathBuf {
    let manifest_path = job_dir.join("manifest.json");
    let manifest_files = files
        .into_iter()
        .map(|(path, bytes)| JobFile {
            path: path.to_owned(),
            size: bytes.len() as u64,
            sha256: sha256(&bytes),
        })
        .collect();
    let manifest = JobManifest::complete(
        "job-1",
        "C:\\synthetic\\input.7z",
        output.to_string_lossy(),
        manifest_files,
        Some("PPSA00001".to_owned()),
    );
    write_manifest_atomic(&manifest_path, &manifest).expect("write fixture manifest");
    manifest_path
}

fn client(engine: &MockEngine) -> UploadClient {
    UploadClient::with_timeouts(
        engine.base_url(),
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_millis(1),
    )
    .expect("loopback mock client")
}

fn queue_preflight_ok(engine: &MockEngine, free_bytes: u64) {
    engine.queue_json("/api/version", 200, json!({"version":"mock"}));
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_raw("/api/ps5/list-dir", 404, "application/json", b"{}");
    engine.queue_json(
        "/api/ps5/volumes",
        200,
        json!({"volumes":[{"path":"/data","free_bytes":free_bytes}]}),
    );
}

fn request<'a>(requests: &'a [CapturedRequest], method: &str, path: &str) -> &'a CapturedRequest {
    requests
        .iter()
        .find(|request| request.method == method && request.path == path)
        .unwrap_or_else(|| panic!("missing {method} {path}; got {requests:?}"))
}

#[test]
fn valid_game_folder_plan_is_deterministic_and_stages_under_job_root() {
    let fixture = write_folder_fixture("valid-plan");
    let plan = plan_upload(&fixture.manifest).expect("valid synthetic folder plan");
    assert_eq!(plan.artifact.kind, UploadArtifactKind::GameFolder);
    assert_eq!(plan.artifact.title_id.as_deref(), Some("PPSA00001"));
    assert_eq!(plan.total_bytes, 38);
    assert_eq!(plan.files.len(), 2);
    assert_eq!(plan.files[0].dest, "/data/homebrew/PPSA00001/eboot.bin");
    assert_eq!(
        plan.files[0].stage_dest,
        "/data/downloads/ps5-companion/job-1/PPSA00001/eboot.bin"
    );
    assert_eq!(
        plan.files[1].dest,
        "/data/homebrew/PPSA00001/sce_sys/param.json"
    );
    let canonical_root = fs::canonicalize(&fixture.root).expect("canonical fixture root");
    assert!(plan
        .files
        .iter()
        .all(|file| file.src.starts_with(&canonical_root)));
}

#[test]
fn image_plan_requires_verified_header_and_rejects_ambiguous_artifacts() {
    let unverified = write_image_fixture("unverified", false);
    let error = plan_upload(&unverified.manifest).expect_err("unverified image must fail");
    assert!(error.to_string().contains("unverified header"));

    let (root, job_dir, output) = job_fixture("ambiguous");
    let image = b"PFSC synthetic image";
    fs::write(output.join("image.ffpfsc"), image).expect("ambiguous image");
    let game = output.join("Game");
    fs::create_dir_all(game.join("sce_sys")).expect("ambiguous game tree");
    let eboot = b"eboot";
    let param = br#"{"titleId":"PPSA00001"}"#;
    fs::write(game.join("eboot.bin"), eboot).expect("ambiguous eboot");
    fs::write(game.join("sce_sys").join("param.json"), param).expect("ambiguous param");
    let manifest = complete_manifest(
        &job_dir,
        &output,
        vec![
            ("image.ffpfsc", image.to_vec()),
            ("Game/eboot.bin", eboot.to_vec()),
            ("Game/sce_sys/param.json", param.to_vec()),
        ],
    );
    let error = plan_upload(manifest).expect_err("ambiguous artifacts must fail");
    assert!(error.to_string().contains("unambiguous"));
    drop(root);
}

#[test]
fn tampered_schema_and_outside_job_root_manifests_fail_before_http() {
    let fixture = write_folder_fixture("manifest-rejections");
    let mut plan_manifest: JobManifest =
        serde_json::from_slice(&fs::read(&fixture.manifest).expect("fixture manifest bytes"))
            .expect("fixture manifest JSON");
    fs::write(
        Path::new(&plan_manifest.output_root)
            .join("Synthetic Game")
            .join("eboot.bin"),
        b"tampered",
    )
    .expect("tamper fixture");
    let error = plan_upload(&fixture.manifest).expect_err("tampered tree must fail");
    assert!(error.to_string().contains("mismatch"), "{error}");

    let failed = write_folder_fixture("failed-schema");
    plan_manifest.state = JobState::Failed;
    write_manifest_atomic(failed.manifest.with_extension("tmp"), &plan_manifest)
        .expect("write failed schema copy");
    let error = plan_upload(failed.manifest.with_extension("tmp"))
        .expect_err("failed manifest state must reject");
    assert!(error.to_string().contains("schema 1 complete"));

    let outside = write_folder_fixture("outside-root");
    let outside_root = fixture_root("outside-output");
    fs::create_dir_all(&outside_root).expect("outside output root");
    let mut outside_manifest: JobManifest =
        serde_json::from_slice(&fs::read(&outside.manifest).expect("outside manifest bytes"))
            .expect("outside manifest JSON");
    outside_manifest.output_root = outside_root.to_string_lossy().into_owned();
    let outside_path = outside.manifest.with_extension("outside");
    write_manifest_atomic(&outside_path, &outside_manifest).expect("outside manifest copy");
    let error = plan_upload(outside_path).expect_err("outside staging root must reject");
    assert!(error.to_string().contains("job directory staging path"));
    let _ = fs::remove_dir_all(outside_root);
}

#[test]
fn existing_destination_fails_preflight_without_submit_post() {
    let fixture = write_image_fixture("existing-destination", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/version", 200, json!({"version":"mock"}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{"path":"synthetic.ffpfsc","kind":"file","size":20}]),
    );
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    let error = client(&engine)
        .submit(&plan, "192.0.2.20")
        .expect_err("occupied destination must reject");
    assert!(error.to_string().contains("overwrite"), "{error}");
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
}

#[test]
fn truncated_directory_listings_never_submit_or_publish() {
    let fixture = write_image_fixture("truncated-preflight", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/version", 200, json!({"version":"mock"}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!({"entries":[],"truncated":true}),
    );
    let error = client(&engine)
        .submit(&plan, "192.0.2.20")
        .expect_err("partial listing must reject");
    assert!(error.to_string().contains("truncated"), "{error}");
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);

    let engine = MockEngine::new();
    engine.queue_json("/api/jobs/job-partial", 200, json!({"status":"done"}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!({
            "entries":[{"name":"synthetic.ffpfsc","kind":"file","size":plan.total_bytes}],
            "truncated":true
        }),
    );
    let error = client(&engine)
        .poll(&plan, "192.0.2.20", "job-partial", Duration::from_secs(2))
        .expect_err("partial verification must reject");
    assert!(error.to_string().contains("truncated"), "{error}");
    assert_eq!(engine.request_count("POST", "/api/ps5/fs/move"), 0);
}

#[test]
fn no_free_space_malformed_volume_and_engine_errors_fail_without_submit_post() {
    for (name, volume_response, expected) in [
        (
            "no-space",
            json!({"volumes":[{"path":"/data","free_bytes":1}]}),
            "free space",
        ),
        (
            "malformed-volume",
            json!({"volumes":[{"path":"/data"}]}),
            "free-space data",
        ),
    ] {
        let fixture = write_image_fixture(name, true);
        let plan = plan_upload(&fixture.manifest).expect("valid image plan");
        let engine = MockEngine::new();
        engine.queue_json("/api/version", 200, json!({"version":"mock"}));
        engine.queue_json("/api/ps5/list-dir", 200, json!([]));
        engine.queue_raw("/api/ps5/list-dir", 404, "application/json", b"{}");
        engine.queue_json("/api/ps5/volumes", 200, volume_response);
        let error = client(&engine)
            .submit(&plan, "192.0.2.21")
            .expect_err("preflight should fail");
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
    }

    let fixture = write_image_fixture("engine-error", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/version", 500, json!({"error":"mock"}));
    let error = client(&engine)
        .submit(&plan, "192.0.2.22")
        .expect_err("engine error should fail");
    assert!(error.to_string().contains("HTTP 500"));
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
}

#[test]
fn safe_url_encoding_is_used_for_ps5_address_and_remote_path() {
    let fixture = write_image_fixture("url-encoding", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    queue_preflight_ok(&engine, plan.total_bytes + 100);
    let address = "10.0.0.7:8043/path?probe=one";
    let _ = client(&engine)
        .preflight(&plan, address)
        .expect("mock preflight");
    let requests = engine.requests();
    let list_request = requests
        .iter()
        .find(|request| request.method == "GET" && request.path.starts_with("/api/ps5/list-dir?"))
        .expect("list-dir request");
    assert!(list_request
        .path
        .contains("addr=10.0.0.7%3A8043%2Fpath%3Fprobe%3Done"));
    assert!(list_request.path.contains("path=%2Fdata%2Fhomebrew"));
    assert!(!list_request.path.contains(address));
}

#[test]
fn submit_sends_staged_sources_and_exact_destinations() {
    let fixture = write_folder_fixture("submit-json");
    let plan = plan_upload(&fixture.manifest).expect("valid folder plan");
    let engine = MockEngine::new();
    queue_preflight_ok(&engine, plan.total_bytes + 100);
    engine.queue_json("/api/transfer/file-list", 202, json!({"job_id":"job-42"}));
    let submission = client(&engine)
        .submit(&plan, "192.0.2.23")
        .expect("submit mock transfer");
    assert_eq!(submission.engine_job_id, "job-42");
    let saved: serde_json::Value = serde_json::from_slice(
        &fs::read(plan.job_dir.join("upload-submission.json")).expect("durable submission record"),
    )
    .expect("submission JSON");
    assert_eq!(saved["engine_job_id"], "job-42");
    let requests = engine.requests();
    let request = request(&requests, "POST", "/api/transfer/file-list");
    let body = request.json();
    assert_eq!(body["addr"], "192.0.2.23");
    assert_eq!(body["dest_root"], plan.stage_root);
    assert_eq!(
        request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str()),
        Some("application/json")
    );
    let files = body["files"].as_array().expect("file list");
    assert_eq!(files.len(), plan.files.len());
    for (body, planned) in files.iter().zip(&plan.files) {
        assert_eq!(body["src"], planned.src.to_string_lossy().as_ref());
        assert_eq!(body["dest"], planned.stage_dest);
        assert!(!body["dest"].as_str().unwrap().starts_with(PS5_DEST_ROOT));
        assert!(body["dest"].as_str().unwrap().starts_with(&plan.stage_root));
    }
}

#[test]
fn second_submit_same_job_is_reserved_without_second_post() {
    let fixture = write_image_fixture("duplicate-submit", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    queue_preflight_ok(&engine, plan.total_bytes + 100);
    queue_preflight_ok(&engine, plan.total_bytes + 100);
    engine.queue_json("/api/transfer/file-list", 202, json!({"job_id":"job-once"}));

    client(&engine)
        .submit(&plan, "192.0.2.231")
        .expect("first submit should reserve the job");
    let error = client(&engine)
        .submit(&plan, "192.0.2.231")
        .expect_err("a reserved job must reject a second submit");
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 1);
}

#[test]
fn completed_job_verifies_staging_then_publishes_and_writes_receipt() {
    let fixture = write_folder_fixture("completed-job");
    let plan = plan_upload(&fixture.manifest).expect("valid folder plan");
    let engine = MockEngine::new();
    engine.queue_json(
        "/api/jobs/job-42",
        200,
        json!({"status":"complete","bytes_sent":plan.total_bytes,"files_sent":2}),
    );
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"PPSA00001", "kind":"directory" }]),
    );
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([
            {"path":"eboot.bin","kind":"file","size":15},
            {"path":"sce_sys","kind":"directory"}
        ]),
    );
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"param.json", "kind":"file", "size":23 }]),
    );
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_json("/api/ps5/fs/move", 200, json!({"ok":true}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"PPSA00001", "kind":"directory" }]),
    );
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([
            {"path":"eboot.bin","kind":"file","size":15},
            {"path":"sce_sys","kind":"directory"}
        ]),
    );
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"param.json", "kind":"file", "size":23 }]),
    );

    let result = client(&engine)
        .poll(&plan, "192.0.2.24", "job-42", Duration::from_millis(50))
        .expect("completed mock transfer");
    assert_eq!(result.status, UploadPollStatus::CompleteSizeVerified);
    let requests = engine.requests();
    let move_request = request(&requests, "POST", "/api/ps5/fs/move");
    let move_body = move_request.json();
    assert_eq!(move_body["addr"], "192.0.2.24");
    assert_eq!(
        move_body["from"],
        "/data/downloads/ps5-companion/job-1/PPSA00001"
    );
    assert_eq!(move_body["to"], "/data/homebrew/PPSA00001");
    assert_eq!(move_body["overwrite"], false);
    assert_eq!(move_body["op_id"], 0);
    let receipt: TransferReceipt =
        serde_json::from_slice(&fs::read(&result.receipt_path).expect("completion receipt"))
            .expect("receipt JSON");
    assert_eq!(receipt.engine_job_id, "job-42");
    assert_eq!(receipt.status, "complete_size_verified");
    assert_eq!(receipt.total_bytes, plan.total_bytes);
}

#[test]
fn failed_job_writes_failed_receipt_and_wrong_size_never_publishes() {
    let fixture = write_image_fixture("failed-job", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json(
        "/api/jobs/job-fail",
        200,
        json!({"status":"failed","error":"disk"}),
    );
    let error = client(&engine)
        .poll(&plan, "192.0.2.25", "job-fail", Duration::from_millis(50))
        .expect_err("failed job should reject");
    assert!(error.to_string().contains("failed"));
    let receipt: TransferReceipt =
        serde_json::from_slice(&fs::read(plan.receipt_path()).expect("failed receipt"))
            .expect("failed receipt JSON");
    assert_eq!(receipt.status, "failed");
    assert_eq!(engine.request_count("POST", "/api/ps5/fs/move"), 0);

    let fixture = write_image_fixture("wrong-size", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/jobs/job-size", 200, json!({"status":"complete"}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"synthetic.ffpfsc","kind":"file","size":999 }]),
    );
    let error = client(&engine)
        .poll(&plan, "192.0.2.26", "job-size", Duration::from_millis(50))
        .expect_err("wrong remote size should reject");
    assert!(error.to_string().contains("mismatch"));
    assert_eq!(engine.request_count("POST", "/api/ps5/fs/move"), 0);
    let receipt: TransferReceipt =
        serde_json::from_slice(&fs::read(plan.receipt_path()).expect("wrong-size receipt"))
            .expect("wrong-size receipt JSON");
    assert_eq!(receipt.status, "failed");
}

#[test]
fn ambiguous_submit_timeout_is_not_retried() {
    let fixture = write_image_fixture("submit-timeout", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    queue_preflight_ok(&engine, plan.total_bytes + 100);
    engine.queue_delayed_json(
        "/api/transfer/file-list",
        200,
        json!({"job_id":"job-ambiguous"}),
        Duration::from_millis(250),
    );
    let client = UploadClient::with_timeouts(
        engine.base_url(),
        Duration::from_millis(100),
        Duration::from_millis(20),
        Duration::from_millis(1),
    )
    .expect("short-timeout loopback client");
    let error = client
        .submit(&plan, "192.0.2.27")
        .expect_err("ambiguous submit should fail");
    assert!(error.to_string().contains("ambiguous upload submission"));
    std::thread::sleep(Duration::from_millis(25));
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 1);
}

#[test]
fn redirect_is_rejected_without_following_or_external_network() {
    let fixture = write_image_fixture("redirect", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_redirect(
        "/api/version",
        "http://203.0.113.44:19113/should-not-follow",
    );
    let error = client(&engine)
        .submit(&plan, "192.0.2.28")
        .expect_err("redirect must fail closed");
    assert!(!error.to_string().is_empty(), "redirect should fail closed");
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
    assert_eq!(engine.request_count("GET", "/should-not-follow"), 0);
}

#[test]
fn existing_staging_destination_fails_before_submit_post() {
    let fixture = write_image_fixture("existing-staging", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/version", 200, json!({"version":"mock"}));
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{
            "path":"job-1",
            "kind":"directory"
        }]),
    );
    let error = client(&engine)
        .submit(&plan, "192.0.2.29")
        .expect_err("existing staging destination must reject");
    assert!(error.to_string().contains("staging namespace"), "{error}");
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
}

#[test]
fn volume_fallback_does_not_trust_aggregate_free_bytes() {
    let fixture = write_image_fixture("aggregate-free", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/version", 200, json!({"version":"mock"}));
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_raw("/api/ps5/volumes", 404, "application/json", b"{}");
    engine.queue_json(
        "/api/ps5/hw/storage",
        200,
        json!({"free_bytes": plan.total_bytes + 1000}),
    );
    let error = client(&engine)
        .submit(&plan, "192.0.2.30")
        .expect_err("aggregate free bytes must not authorize upload");
    assert!(error.to_string().contains("storage") || error.to_string().contains("volume"));
    assert_eq!(engine.request_count("POST", "/api/transfer/file-list"), 0);
}

#[test]
fn completed_job_with_error_does_not_publish() {
    let fixture = write_image_fixture("completed-error", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json(
        "/api/jobs/job-error",
        200,
        json!({"status":"complete","error":"remote reported a failure"}),
    );
    let error = client(&engine)
        .poll(&plan, "192.0.2.31", "job-error", Duration::from_millis(50))
        .expect_err("completed job carrying an error must reject");
    assert!(error.to_string().contains("error") || error.to_string().contains("failed"));
    assert_eq!(engine.request_count("POST", "/api/ps5/fs/move"), 0);
}

#[test]
fn move_ok_false_preserves_staging_and_failed_receipt() {
    let fixture = write_image_fixture("move-failure", true);
    let plan = plan_upload(&fixture.manifest).expect("valid image plan");
    let engine = MockEngine::new();
    engine.queue_json("/api/jobs/job-move", 200, json!({"status":"complete"}));
    engine.queue_json(
        "/api/ps5/list-dir",
        200,
        json!([{ "path":"synthetic.ffpfsc","kind":"file","size":20 }]),
    );
    engine.queue_json("/api/ps5/list-dir", 200, json!([]));
    engine.queue_json(
        "/api/ps5/fs/move",
        200,
        json!({"ok":false,"error":"move refused"}),
    );
    let error = client(&engine)
        .poll(&plan, "192.0.2.32", "job-move", Duration::from_millis(50))
        .expect_err("move refusal must fail the upload");
    assert!(
        error.to_string().contains("move") || error.to_string().contains("ok"),
        "{error}"
    );
    assert_eq!(engine.request_count("POST", "/api/ps5/fs/move"), 1);
    let receipt: TransferReceipt =
        serde_json::from_slice(&fs::read(plan.receipt_path()).expect("move failure receipt"))
            .expect("move failure receipt JSON");
    assert_eq!(receipt.status, "failed");
    let list_requests = engine
        .requests()
        .iter()
        .filter(|request| request.method == "GET" && request.path.starts_with("/api/ps5/list-dir?"))
        .count();
    assert_eq!(list_requests, 2);
}
