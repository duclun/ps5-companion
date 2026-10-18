//! Safe, first-upload transfer support for the local PS5Upload HTTP engine.
//!
//! The upload boundary is deliberately small. A plan is built entirely from a
//! completed, locally verified extraction manifest. The engine is contacted only
//! by [`UploadClient`], which accepts loopback HTTP URLs, never follows redirects,
//! bounds every response, and performs the read-only preflight before enqueueing a
//! transfer.

use crate::error::{CompanionError, Result};
use crate::manifest::{JobFile, JobManifest, JobState};
use crate::validate::{identify_game_artifact, verify_manifest_tree, VerifiedFile};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use ureq::{Agent, AgentBuilder};
use url::{Host, Url};

pub const PS5_DEST_ROOT: &str = "/data/homebrew";
pub const PS5_STAGE_ROOT: &str = "/data/downloads/ps5-companion";
pub const TRANSFER_RECEIPT_FILE_NAME: &str = "transfer-receipt.json";
const UPLOAD_ATTEMPT_FILE_NAME: &str = "upload-attempt.json";
pub const MAX_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_ENGINE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_REMOTE_LIST_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPlanFile {
    pub src: PathBuf,
    pub dest: String,
    pub stage_dest: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadArtifactKind {
    Image,
    GameFolder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadArtifact {
    pub path: PathBuf,
    pub kind: UploadArtifactKind,
    pub title_id: Option<String>,
    pub verified_header: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPlan {
    pub manifest_path: PathBuf,
    pub job_dir: PathBuf,
    pub output_root: PathBuf,
    pub dest_root: String,
    pub stage_root: String,
    pub artifact: UploadArtifact,
    pub files: Vec<UploadPlanFile>,
    pub total_bytes: u64,
}

impl UploadPlan {
    pub fn from_manifest(path: impl AsRef<Path>) -> Result<Self> {
        plan_upload(path)
    }

    pub fn receipt_path(&self) -> PathBuf {
        self.job_dir.join(TRANSFER_RECEIPT_FILE_NAME)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPreflight {
    pub total_bytes: u64,
    pub remote_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSubmission {
    pub engine_job_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadPollStatus {
    CompleteSizeVerified,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPollResult {
    pub engine_job_id: String,
    pub status: UploadPollStatus,
    pub receipt_path: PathBuf,
    pub bytes_sent: Option<u64>,
    pub files_sent: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferReceiptFile {
    pub stage_dest: String,
    pub dest: String,
    pub size: u64,
    pub sha256: String,
}

/// Durable, immutable local record for one transfer attempt. It intentionally
/// stores no source paths or engine URL, so it can be used as a cleanup allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferReceipt {
    pub schema_version: u32,
    pub engine_job_id: String,
    pub addr: String,
    pub dest_root: String,
    pub files: Vec<TransferReceiptFile>,
    pub total_bytes: u64,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct UploadClient {
    base_url: Url,
    agent: Agent,
    poll_interval: Duration,
}

impl UploadClient {
    pub fn new(base_url: &str) -> Result<Self> {
        Self::with_timeouts(
            base_url,
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_POLL_INTERVAL,
        )
    }

    pub fn with_timeouts(
        base_url: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self> {
        let base_url = validate_engine_base_url(base_url)?;
        if connect_timeout.is_zero() || request_timeout.is_zero() || poll_interval.is_zero() {
            return Err(CompanionError::Upload(
                "HTTP timeouts and poll interval must be positive".to_owned(),
            ));
        }
        let agent = AgentBuilder::new()
            .timeout_connect(connect_timeout)
            .timeout_read(request_timeout)
            .timeout_write(request_timeout)
            .redirects(0)
            .try_proxy_from_env(false)
            .build();
        Ok(Self {
            base_url,
            agent,
            poll_interval,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Perform all local and remote read-only checks. No transfer is enqueued.
    pub fn preflight(&self, plan: &UploadPlan, addr: &str) -> Result<UploadPreflight> {
        validate_addr(addr)?;
        let fresh = refresh_plan(plan)?;
        self.preflight_verified(&fresh, addr)
    }

    fn preflight_verified(&self, plan: &UploadPlan, addr: &str) -> Result<UploadPreflight> {
        let version = self.get_json("/api/version", &[])?;
        if version
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return Err(CompanionError::Upload(
                "engine version response omitted a version string".to_owned(),
            ));
        }
        self.ensure_destinations_absent(addr, plan)?;
        let remote_free_bytes = self.remote_free_bytes(addr)?;
        if remote_free_bytes < plan.total_bytes {
            return Err(CompanionError::Upload(format!(
                "PS5 free space is insufficient: need {}, have {}",
                plan.total_bytes, remote_free_bytes
            )));
        }
        Ok(UploadPreflight {
            total_bytes: plan.total_bytes,
            remote_free_bytes,
        })
    }

    /// Preflight and enqueue exactly one transfer request. A transport timeout is
    /// returned as an ambiguous submission error and is never retried.
    pub fn submit(&self, plan: &UploadPlan, addr: &str) -> Result<UploadSubmission> {
        validate_addr(addr)?;
        let fresh = refresh_plan(plan)?;
        self.preflight_verified(&fresh, addr)?;
        reserve_attempt(&fresh, addr)?;
        let files = fresh
            .files
            .iter()
            .map(|file| {
                serde_json::json!({
                    "src": file.src.to_string_lossy(),
                    "dest": file.stage_dest,
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "addr": addr,
            "dest_root": fresh.stage_root,
            "files": files,
        });
        let value = self.post_json("/api/transfer/file-list", &body)?;
        let job_id = value
            .get("job_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CompanionError::Upload("engine response omitted job_id".to_owned()))?;
        validate_job_id(job_id)?;
        // Preserve the accepted job ID before polling so a stopped CLI can be
        // reconciled manually with PS5Upload without resubmitting the transfer.
        let record_path = fresh.job_dir.join("upload-submission.json");
        let record = serde_json::to_vec_pretty(&serde_json::json!({
            "engine_job_id": job_id,
            "addr": addr,
            "stage_root": fresh.stage_root,
        }))
        .map_err(|error| CompanionError::Upload(format!("serialize submission: {error}")))?;
        let persist = || -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(record_path)?;
            file.write_all(&record)?;
            file.sync_all()
        };
        persist().map_err(|error| CompanionError::Upload(format!(
            "engine accepted job {job_id}, but its local submission record could not be saved: {error}; inspect PS5Upload, do not resubmit"
        )))?;
        Ok(UploadSubmission {
            engine_job_id: job_id.to_owned(),
        })
    }

    /// Poll a submitted transfer until it completes, fails, or reaches the
    /// caller's bounded timeout. A successful engine status is accepted only
    /// after the exact destination tree has been checked by size and kind.
    pub fn poll(
        &self,
        plan: &UploadPlan,
        addr: &str,
        engine_job_id: &str,
        timeout: Duration,
    ) -> Result<UploadPollResult> {
        validate_addr(addr)?;
        validate_job_id(engine_job_id)?;
        let fresh = refresh_plan(plan)?;
        let started = Instant::now();
        loop {
            let value = match self.get_json(&format!("/api/jobs/{engine_job_id}"), &[]) {
                Ok(value) => value,
                Err(error) => {
                    let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed");
                    return Err(CompanionError::Upload(format_poll_error(
                        engine_job_id,
                        error,
                        receipt,
                    )));
                }
            };
            let job = value.get("job").unwrap_or(&value);
            let bytes_sent = json_u64(job, &["bytes_sent", "bytesSent"]);
            let files_sent = json_u64(job, &["files_sent", "filesSent"]);
            let status = job
                .get("status")
                .or_else(|| value.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let job_error = job
                .get("error")
                .filter(|value| !value.is_null())
                .filter(|value| {
                    !value
                        .as_str()
                        .map(|text| text.trim().is_empty())
                        .unwrap_or(false)
                });
            if status.is_empty() {
                let error = CompanionError::Upload("engine job response omitted status".to_owned());
                let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed");
                return Err(CompanionError::Upload(format_poll_error(
                    engine_job_id,
                    error,
                    receipt,
                )));
            }
            if let Some(job_error) = job_error {
                let detail = job_error.to_string();
                let error = CompanionError::Upload(format!("engine job reported error: {detail}"));
                let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed");
                return Err(CompanionError::Upload(format_poll_error(
                    engine_job_id,
                    error,
                    receipt,
                )));
            }
            if is_success_status(&status) {
                match self.verify_remote_tree(addr, &fresh, &fresh.stage_root) {
                    Ok(()) => {
                        if let Err(error) = self.publish(&fresh, addr) {
                            let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed");
                            return Err(CompanionError::Upload(format_poll_error(
                                engine_job_id,
                                error,
                                receipt,
                            )));
                        }
                        match self.verify_remote_tree(addr, &fresh, &fresh.dest_root) {
                            Ok(()) => {
                                let receipt = self.write_receipt(
                                    &fresh,
                                    addr,
                                    engine_job_id,
                                    "complete_size_verified",
                                )?;
                                return Ok(UploadPollResult {
                                    engine_job_id: engine_job_id.to_owned(),
                                    status: UploadPollStatus::CompleteSizeVerified,
                                    receipt_path: receipt,
                                    bytes_sent,
                                    files_sent,
                                });
                            }
                            Err(error) => {
                                let receipt =
                                    self.write_receipt(&fresh, addr, engine_job_id, "failed");
                                return Err(CompanionError::Upload(format_poll_error(
                                    engine_job_id,
                                    error,
                                    receipt,
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed");
                        return Err(CompanionError::Upload(format_poll_error(
                            engine_job_id,
                            error,
                            receipt,
                        )));
                    }
                }
            }
            if is_failure_status(&status) {
                let receipt = self.write_receipt(&fresh, addr, engine_job_id, "failed")?;
                return Err(CompanionError::Upload(format!(
                    "engine job {engine_job_id} failed; receipt retained at {}",
                    receipt.display()
                )));
            }
            if started.elapsed() >= timeout {
                let receipt = self.write_receipt(&fresh, addr, engine_job_id, "timeout")?;
                return Err(CompanionError::Upload(format!(
                    "timed out waiting for engine job {engine_job_id}; receipt retained at {}",
                    receipt.display()
                )));
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            thread::sleep(self.poll_interval.min(remaining));
        }
    }

    pub fn upload(
        &self,
        plan: &UploadPlan,
        addr: &str,
        timeout: Duration,
    ) -> Result<UploadPollResult> {
        let submission = self.submit(plan, addr)?;
        self.poll(plan, addr, &submission.engine_job_id, timeout)
    }

    fn ensure_destinations_absent(&self, addr: &str, plan: &UploadPlan) -> Result<()> {
        let mut occupied = HashSet::new();
        for entry in self.list_dir(addr, PS5_DEST_ROOT)? {
            let path = remote_join(PS5_DEST_ROOT, &entry.path)?;
            occupied.insert(path);
        }
        let stage_entries = match self.list_dir(addr, PS5_STAGE_ROOT) {
            Ok(entries) => entries,
            Err(CompanionError::Upload(message)) if message.starts_with("HTTP 404") => Vec::new(),
            Err(error) => return Err(error),
        };
        for entry in stage_entries {
            let path = remote_join(PS5_STAGE_ROOT, &entry.path)?;
            occupied.insert(path);
        }
        if occupied.contains(&plan.stage_root) {
            return Err(CompanionError::Upload(format!(
                "refusing to reuse existing PS5 staging namespace {}",
                plan.stage_root
            )));
        }
        let roots = plan
            .files
            .iter()
            .map(|file| {
                if matches!(plan.artifact.kind, UploadArtifactKind::Image) {
                    file.dest.clone()
                } else {
                    let relative = file.dest.strip_prefix(PS5_DEST_ROOT).unwrap_or(&file.dest);
                    let component = relative.split('/').nth(1).unwrap_or("");
                    format!("{PS5_DEST_ROOT}/{component}")
                }
            })
            .collect::<BTreeSet<_>>();
        for root in roots {
            if occupied.contains(&root) {
                return Err(CompanionError::Upload(format!(
                    "refusing to overwrite existing PS5 destination {root}"
                )));
            }
        }
        for file in &plan.files {
            if occupied.contains(&file.stage_dest) {
                return Err(CompanionError::Upload(format!(
                    "refusing to overwrite existing PS5 staging destination {}",
                    file.stage_dest
                )));
            }
        }
        Ok(())
    }

    fn publish(&self, plan: &UploadPlan, addr: &str) -> Result<()> {
        // Recheck immediately before the move. The engine receives overwrite:false
        // as a second guard, but the client must fail closed before issuing it.
        let final_root = final_artifact_root(plan)?;
        let parent = final_root
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or(PS5_DEST_ROOT);
        for entry in self.list_dir(addr, parent)? {
            let path = remote_join(parent, &entry.path)?;
            if path == final_root || path.starts_with(&(final_root.clone() + "/")) {
                return Err(CompanionError::Upload(format!(
                    "refusing to publish over existing PS5 destination {final_root}"
                )));
            }
        }
        let body = serde_json::json!({
            "addr": addr,
            "from": stage_artifact_root(plan)?,
            "to": final_root,
            "overwrite": false,
            "op_id": 0,
        });
        let response = self.post_json("/api/ps5/fs/move", &body)?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(CompanionError::Upload(
                "PS5 publish response did not confirm ok:true".to_owned(),
            ));
        }
        Ok(())
    }

    fn remote_free_bytes(&self, addr: &str) -> Result<u64> {
        let volumes = match self.get_json("/api/ps5/volumes", &[("addr", addr)]) {
            Ok(value) => Some(value),
            Err(CompanionError::Upload(message)) if message.starts_with("HTTP 404") => None,
            Err(error) => return Err(error),
        };
        if let Some(value) = volumes {
            let final_free = parse_volume_free_bytes(&value, PS5_DEST_ROOT)?;
            let stage_free = parse_volume_free_bytes(&value, PS5_STAGE_ROOT)?;
            return Ok(final_free.min(stage_free));
        }
        let value = self.get_json("/api/ps5/hw/storage", &[("addr", addr)])?;
        let free = find_numeric_field(&value, &["user_free_bytes"]).ok_or_else(|| {
            CompanionError::Upload("storage response omitted user_free_bytes".to_owned())
        })?;
        if free == 0 {
            return Err(CompanionError::Upload(
                "PS5 storage response reported zero free bytes".to_owned(),
            ));
        }
        Ok(free)
    }

    fn verify_remote_tree(&self, addr: &str, plan: &UploadPlan, root: &str) -> Result<()> {
        let expected = plan
            .files
            .iter()
            .map(|file| {
                let destination = if root == plan.stage_root {
                    file.stage_dest.clone()
                } else {
                    file.dest.clone()
                };
                (destination, file.size)
            })
            .collect::<BTreeMap<_, _>>();
        let actual = if matches!(plan.artifact.kind, UploadArtifactKind::Image) {
            let mut direct = BTreeMap::new();
            for entry in self.list_dir(addr, root)? {
                let path = remote_join(root, &entry.path)?;
                if expected.contains_key(&path) {
                    let size = entry.size.ok_or_else(|| {
                        CompanionError::Upload(format!(
                            "remote listing omitted size for file {path}"
                        ))
                    })?;
                    direct.insert(path, (entry.kind, size));
                }
            }
            direct
        } else if root == plan.stage_root || root == plan.dest_root {
            let target = if root == plan.stage_root {
                stage_artifact_root(plan)?
            } else {
                final_artifact_root(plan)?
            };
            let mut found = false;
            for entry in self.list_dir(addr, root)? {
                let path = remote_join(root, &entry.path)?;
                if path == target {
                    if entry.kind != RemoteKind::Directory {
                        return Err(CompanionError::Upload(format!(
                            "remote artifact root is not a directory: {target}"
                        )));
                    }
                    found = true;
                }
            }
            if !found {
                return Err(CompanionError::Upload(format!(
                    "remote artifact root missing: {target}"
                )));
            }
            self.collect_remote_files(addr, &target, 0)?
        } else {
            self.collect_remote_files(addr, root, 0)?
        };
        if actual.len() != expected.len() {
            return Err(CompanionError::Upload(format!(
                "remote destination file count mismatch: expected {}, got {}",
                expected.len(),
                actual.len()
            )));
        }
        for (path, expected_size) in expected {
            let Some((kind, actual_size)) = actual.get(&path) else {
                return Err(CompanionError::Upload(format!(
                    "remote destination file missing: {path}"
                )));
            };
            if *kind != RemoteKind::File || *actual_size != expected_size {
                return Err(CompanionError::Upload(format!(
                    "remote destination kind or size mismatch for {path}"
                )));
            }
        }
        Ok(())
    }

    fn collect_remote_files(
        &self,
        addr: &str,
        root: &str,
        depth: usize,
    ) -> Result<BTreeMap<String, (RemoteKind, u64)>> {
        if depth > MAX_REMOTE_LIST_DEPTH {
            return Err(CompanionError::Upload(
                "remote destination tree exceeded the depth limit".to_owned(),
            ));
        }
        let entries = self.list_dir(addr, root)?;
        let mut result = BTreeMap::new();
        for entry in entries {
            let path = if entry.path.starts_with('/') {
                normalize_remote_path(&entry.path)?
            } else {
                remote_join(root, &entry.path)?
            };
            if !is_remote_child(root, &path) {
                return Err(CompanionError::Upload(format!(
                    "remote listing escaped requested directory: {path}"
                )));
            }
            match entry.kind {
                RemoteKind::Directory => {
                    let nested = self.collect_remote_files(addr, &path, depth + 1)?;
                    result.extend(nested);
                }
                RemoteKind::File => {
                    let size = entry.size.ok_or_else(|| {
                        CompanionError::Upload(format!(
                            "remote listing omitted size for file {path}"
                        ))
                    })?;
                    if result
                        .insert(path.clone(), (RemoteKind::File, size))
                        .is_some()
                    {
                        return Err(CompanionError::Upload(format!(
                            "duplicate remote destination path: {path}"
                        )));
                    }
                }
            }
        }
        Ok(result)
    }

    fn list_dir(&self, addr: &str, path: &str) -> Result<Vec<RemoteEntry>> {
        let value = self.get_json("/api/ps5/list-dir", &[("addr", addr), ("path", path)])?;
        parse_remote_entries(&value)
    }

    fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let url = self.route(path, query)?;
        let response = self.agent.get(url.as_str()).call();
        match response {
            Ok(response) => read_json_response(response, None),
            Err(ureq::Error::Status(code, response)) => {
                let body = read_limited_response(response).unwrap_or_default();
                Err(CompanionError::Upload(format!(
                    "HTTP {code} from {}{}",
                    path,
                    if body.is_empty() {
                        ""
                    } else {
                        ": response body"
                    }
                )))
            }
            Err(ureq::Error::Transport(error)) => Err(CompanionError::Upload(format!(
                "HTTP request to {path} failed: {error}"
            ))),
        }
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.route(path, &[])?;
        let body = serde_json::to_string(body).map_err(|error| {
            CompanionError::Upload(format!("serialize upload request: {error}"))
        })?;
        let response = self
            .agent
            .post(url.as_str())
            .set("Content-Type", "application/json")
            .send_string(&body);
        match response {
            Ok(response) => read_json_response(response, None),
            Err(ureq::Error::Status(code, response)) => {
                let _ = read_limited_response(response);
                Err(CompanionError::Upload(format!("HTTP {code} from {path}")))
            }
            Err(ureq::Error::Transport(error)) => Err(CompanionError::Upload(format!(
                "ambiguous upload submission: HTTP request to {path} failed: {error}"
            ))),
        }
    }

    fn route(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self.base_url.clone();
        url.set_path(path);
        url.set_query(None);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    fn write_receipt(
        &self,
        plan: &UploadPlan,
        addr: &str,
        engine_job_id: &str,
        status: &str,
    ) -> Result<PathBuf> {
        let receipt_path = plan.receipt_path();
        let receipt = TransferReceipt {
            schema_version: 1,
            engine_job_id: engine_job_id.to_owned(),
            addr: addr.to_owned(),
            dest_root: PS5_DEST_ROOT.to_owned(),
            files: plan
                .files
                .iter()
                .map(|file| TransferReceiptFile {
                    stage_dest: file.stage_dest.clone(),
                    dest: file.dest.clone(),
                    size: file.size,
                    sha256: file.sha256.clone(),
                })
                .collect(),
            total_bytes: plan.total_bytes,
            status: status.to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&receipt).map_err(|error| {
            CompanionError::Upload(format!("serialize transfer receipt: {error}"))
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&receipt_path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    CompanionError::Upload(format!(
                        "transfer receipt already exists: {}",
                        receipt_path.display()
                    ))
                } else {
                    CompanionError::io("create transfer receipt", source)
                }
            })?;
        if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&receipt_path);
            return Err(CompanionError::io("write transfer receipt", source));
        }
        Ok(receipt_path)
    }
}

pub fn plan_upload(manifest_path: impl AsRef<Path>) -> Result<UploadPlan> {
    let (manifest_path, manifest, output_root, job_dir) = load_upload_manifest(manifest_path)?;
    let verified = verify_manifest_tree(&output_root, &manifest)?;
    let artifact = find_upload_artifact(&output_root)?;
    let by_path = verified
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<HashMap<_, _>>();
    let artifact_relative = relative_utf8(&output_root, &artifact.path)?;
    let stage_artifact_component = match artifact.kind {
        UploadArtifactKind::Image => artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CompanionError::Upload("image filename is not UTF-8".to_owned()))?
            .to_owned(),
        UploadArtifactKind::GameFolder => artifact
            .title_id
            .clone()
            .ok_or_else(|| CompanionError::Upload("game folder has no title id".to_owned()))?,
    };
    validate_remote_component(&stage_artifact_component)?;
    let mut files = Vec::new();
    match artifact.kind {
        UploadArtifactKind::Image => {
            let file = by_path.get(artifact_relative.as_str()).ok_or_else(|| {
                CompanionError::Upload("identified image is absent from manifest".to_owned())
            })?;
            let basename = artifact
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| CompanionError::Upload("image filename is not UTF-8".to_owned()))?;
            validate_remote_component(basename)?;
            files.push(plan_file(
                &output_root,
                file,
                format!("{PS5_DEST_ROOT}/{basename}"),
                // An image is itself the staged artifact. Keep it directly
                // under the job namespace so publish can atomically move the
                // file rather than manufacturing a same-named directory.
                format!("{PS5_STAGE_ROOT}/{}/{basename}", manifest.job_id),
            )?);
        }
        UploadArtifactKind::GameFolder => {
            let title_id = artifact.title_id.as_deref().ok_or_else(|| {
                CompanionError::Upload("game folder has no validated title id".to_owned())
            })?;
            for file in &verified {
                let relative = if artifact_relative.is_empty() {
                    file.path.clone()
                } else if file.path == artifact_relative {
                    continue;
                } else if let Some(value) =
                    file.path.strip_prefix(&(artifact_relative.clone() + "/"))
                {
                    value.to_owned()
                } else {
                    continue;
                };
                if relative.is_empty() {
                    continue;
                }
                validate_remote_relative_path(&relative)?;
                files.push(plan_file(
                    &output_root,
                    file,
                    format!("{PS5_DEST_ROOT}/{title_id}/{relative}"),
                    format!("{PS5_STAGE_ROOT}/{}/{title_id}/{relative}", manifest.job_id),
                )?);
            }
            if files.is_empty() {
                return Err(CompanionError::Upload(
                    "validated game folder contains no files".to_owned(),
                ));
            }
        }
    }
    files.sort_by(|left, right| left.dest.cmp(&right.dest));
    let total_bytes = files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or(CompanionError::SizeOverflow)
    })?;
    Ok(UploadPlan {
        manifest_path,
        job_dir,
        output_root,
        dest_root: PS5_DEST_ROOT.to_owned(),
        stage_root: format!("{PS5_STAGE_ROOT}/{}", manifest.job_id),
        artifact,
        files,
        total_bytes,
    })
}

pub fn load_upload_manifest(
    manifest_path: impl AsRef<Path>,
) -> Result<(PathBuf, JobManifest, PathBuf, PathBuf)> {
    let supplied = manifest_path.as_ref();
    let metadata = fs::symlink_metadata(supplied)
        .map_err(|source| CompanionError::io("inspect upload manifest", source))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(CompanionError::Upload(
            "manifest must be a regular file and may not be a symlink".to_owned(),
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(CompanionError::Upload(format!(
            "manifest exceeds the {} byte limit",
            MAX_MANIFEST_BYTES
        )));
    }
    let canonical_manifest = fs::canonicalize(supplied)
        .map_err(|source| CompanionError::io("canonicalize upload manifest", source))?;
    let parent = canonical_manifest
        .parent()
        .ok_or_else(|| CompanionError::Upload("manifest has no parent directory".to_owned()))?;
    let job_dir = parent.to_path_buf();
    let job_name = job_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let bytes = fs::read(&canonical_manifest)
        .map_err(|source| CompanionError::io("read upload manifest", source))?;
    let manifest: JobManifest = serde_json::from_slice(&bytes)
        .map_err(|error| CompanionError::Upload(format!("parse upload manifest: {error}")))?;
    if manifest.schema_version != 1 || manifest.state != JobState::Complete {
        return Err(CompanionError::Upload(
            "upload requires a schema 1 complete extraction manifest".to_owned(),
        ));
    }
    if manifest.job_id != job_name {
        return Err(CompanionError::Upload(
            "manifest job_id does not match its job directory".to_owned(),
        ));
    }
    validate_manifest_files(&manifest.files)?;
    if manifest.input_archive_path.contains("://") || manifest.input_archive_path.contains('\0') {
        return Err(CompanionError::Upload(
            "manifest input path is invalid".to_owned(),
        ));
    }
    let output_root = PathBuf::from(&manifest.output_root);
    let output_meta = fs::symlink_metadata(&output_root)
        .map_err(|source| CompanionError::io("inspect manifest staging root", source))?;
    if output_meta.file_type().is_symlink()
        || is_reparse_point(&output_meta)
        || !output_meta.is_dir()
    {
        return Err(CompanionError::Upload(
            "manifest staging root must be a regular directory".to_owned(),
        ));
    }
    let canonical_output = fs::canonicalize(&output_root)
        .map_err(|source| CompanionError::io("canonicalize manifest staging root", source))?;
    let expected_output = job_dir.join("staging");
    if canonical_output != expected_output {
        return Err(CompanionError::Upload(
            "manifest output_root must be its job directory staging path".to_owned(),
        ));
    }
    Ok((canonical_manifest, manifest, canonical_output, job_dir))
}

fn refresh_plan(plan: &UploadPlan) -> Result<UploadPlan> {
    let fresh = plan_upload(&plan.manifest_path)?;
    if fresh.files != plan.files || fresh.total_bytes != plan.total_bytes {
        return Err(CompanionError::Upload(
            "local extraction changed since upload plan was created".to_owned(),
        ));
    }
    Ok(fresh)
}

fn reserve_attempt(plan: &UploadPlan, addr: &str) -> Result<()> {
    #[derive(Serialize)]
    struct Attempt<'a> {
        schema_version: u32,
        addr: &'a str,
        stage_root: &'a str,
        files: Vec<&'a str>,
    }
    let path = plan.job_dir.join(UPLOAD_ATTEMPT_FILE_NAME);
    let attempt = Attempt {
        schema_version: 1,
        addr,
        stage_root: &plan.stage_root,
        files: plan
            .files
            .iter()
            .map(|file| file.stage_dest.as_str())
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&attempt)
        .map_err(|error| CompanionError::Upload(format!("serialize upload attempt: {error}")))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                CompanionError::Upload(format!(
                    "upload attempt already exists for job directory: {}",
                    plan.job_dir.display()
                ))
            } else {
                CompanionError::io("reserve upload attempt", source)
            }
        })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&path);
        return Err(CompanionError::io("write upload attempt", source));
    }
    Ok(())
}

fn stage_artifact_root(plan: &UploadPlan) -> Result<String> {
    let first = plan
        .files
        .first()
        .ok_or_else(|| CompanionError::Upload("upload plan has no files".to_owned()))?;
    if matches!(plan.artifact.kind, UploadArtifactKind::Image) {
        validate_remote_component(
            first
                .stage_dest
                .rsplit_once('/')
                .and_then(|(_, name)| name.split('/').next())
                .unwrap_or(""),
        )?;
        return Ok(first.stage_dest.clone());
    }
    let relative = first
        .stage_dest
        .strip_prefix(&(plan.stage_root.clone() + "/"))
        .ok_or_else(|| CompanionError::Upload("invalid staging destination".to_owned()))?;
    let title_id = relative.split('/').next().unwrap_or("");
    validate_remote_component(title_id)?;
    Ok(format!("{}/{}", plan.stage_root, title_id))
}

fn final_artifact_root(plan: &UploadPlan) -> Result<String> {
    let first = plan
        .files
        .first()
        .ok_or_else(|| CompanionError::Upload("upload plan has no files".to_owned()))?;
    if matches!(plan.artifact.kind, UploadArtifactKind::Image) {
        return Ok(first.dest.clone());
    }
    let relative = first
        .dest
        .strip_prefix(&(PS5_DEST_ROOT.to_owned() + "/"))
        .ok_or_else(|| CompanionError::Upload("invalid final destination".to_owned()))?;
    let title_id = relative.split('/').next().unwrap_or("");
    validate_remote_component(title_id)?;
    Ok(format!("{PS5_DEST_ROOT}/{title_id}"))
}

fn plan_file(
    root: &Path,
    file: &VerifiedFile,
    dest: String,
    stage_dest: String,
) -> Result<UploadPlanFile> {
    let source = root.join(native_relative_path(&file.path));
    let metadata = fs::symlink_metadata(&source)
        .map_err(|source| CompanionError::io("inspect planned upload file", source))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(CompanionError::Upload(format!(
            "planned source is not a regular file: {}",
            source.display()
        )));
    }
    Ok(UploadPlanFile {
        src: fs::canonicalize(&source)
            .map_err(|source| CompanionError::io("canonicalize planned upload file", source))?,
        dest,
        stage_dest,
        size: file.size,
        sha256: file.sha256.clone(),
    })
}

fn validate_manifest_files(files: &[JobFile]) -> Result<()> {
    if files.len() > 1_000_000 {
        return Err(CompanionError::Upload(
            "manifest contains more than the supported file count".to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    for file in files {
        validate_relative_path(&file.path)?;
        if !seen.insert(file.path.clone()) {
            return Err(CompanionError::Upload(format!(
                "manifest contains duplicate file path {}",
                file.path
            )));
        }
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CompanionError::Upload(format!(
                "manifest contains invalid SHA-256 for {}",
                file.path
            )));
        }
    }
    if files.is_empty() {
        return Err(CompanionError::Upload(
            "manifest contains no files".to_owned(),
        ));
    }
    Ok(())
}

fn find_upload_artifact(root: &Path) -> Result<UploadArtifact> {
    // Keep the existing detector as the common first-line artifact check. The
    // strict scan below adds ambiguity detection required at the upload boundary.
    let _ = identify_game_artifact(root)?;
    let mut files = Vec::new();
    collect_regular_files(root, "", &mut files)?;
    let mut images = Vec::new();
    let mut folders = Vec::new();
    for (relative, path) in files {
        let lower = relative.to_ascii_lowercase();
        let image_kind = if lower.ends_with(".exfat") {
            Some("exfat")
        } else if lower.ends_with(".ffpfsc") {
            Some("ffpfsc")
        } else if lower.ends_with(".ffpfs") {
            Some("ffpfs")
        } else if lower.ends_with(".ffpkg") {
            Some("ffpkg")
        } else {
            None
        };
        if let Some(name) = image_kind {
            let verified_header = image_header_matches(&path, name)?;
            if !verified_header {
                return Err(CompanionError::Upload(format!(
                    "image candidate has an unverified header: {}",
                    path.display()
                )));
            }
            images.push((relative, path));
            continue;
        }
        if relative
            .rsplit('/')
            .next()
            .map(|name| name.eq_ignore_ascii_case("eboot.bin"))
            .unwrap_or(false)
        {
            let game_root = path
                .parent()
                .ok_or_else(|| CompanionError::Upload("game folder has no parent".to_owned()))?;
            let param_path = game_root.join("sce_sys").join("param.json");
            let param_meta = match fs::symlink_metadata(&param_path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if param_meta.file_type().is_symlink() || !param_meta.is_file() {
                return Err(CompanionError::Upload(
                    "game folder param.json must be a regular file".to_owned(),
                ));
            }
            if param_meta.len() > 4 * 1024 * 1024 {
                return Err(CompanionError::Upload(
                    "game folder param.json exceeds the metadata size limit".to_owned(),
                ));
            }
            let value: Value = serde_json::from_slice(
                &fs::read(&param_path)
                    .map_err(|source| CompanionError::io("read game folder param.json", source))?,
            )
            .map_err(|error| {
                CompanionError::Upload(format!("parse game folder param.json: {error}"))
            })?;
            let title_id = find_title_id(&value).ok_or_else(|| {
                CompanionError::Upload("game folder param.json has no title id".to_owned())
            })?;
            validate_title_id(&title_id)?;
            let relative_root = relative_parent(&relative)?;
            folders.push((relative_root, game_root.to_path_buf(), title_id));
        }
    }
    if images.len() + folders.len() != 1 {
        return Err(CompanionError::Upload(format!(
            "expected exactly one unambiguous game artifact, found {}",
            images.len() + folders.len()
        )));
    }
    if let Some((_relative, path)) = images.into_iter().next() {
        return Ok(UploadArtifact {
            path,
            kind: UploadArtifactKind::Image,
            title_id: None,
            verified_header: true,
        });
    }
    let (relative, path, title_id) = folders.into_iter().next().unwrap();
    let _ = relative;
    Ok(UploadArtifact {
        path,
        kind: UploadArtifactKind::GameFolder,
        title_id: Some(title_id),
        verified_header: false,
    })
}

fn collect_regular_files(
    root: &Path,
    relative: &str,
    output: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let directory = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(native_relative_path(relative))
    };
    for item in fs::read_dir(&directory)
        .map_err(|source| CompanionError::io("read upload artifact directory", source))?
    {
        let item =
            item.map_err(|source| CompanionError::io("read upload artifact entry", source))?;
        let name = item
            .file_name()
            .to_str()
            .ok_or_else(|| CompanionError::Upload("artifact filename is not UTF-8".to_owned()))?
            .to_owned();
        validate_component(&name)?;
        let child = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        let metadata = fs::symlink_metadata(item.path())
            .map_err(|source| CompanionError::io("inspect upload artifact entry", source))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file() && !metadata.is_dir()
        {
            return Err(CompanionError::Upload(format!(
                "unsupported upload artifact entry: {child}"
            )));
        }
        if metadata.is_dir() {
            collect_regular_files(root, &child, output)?;
        } else {
            output.push((child, item.path()));
        }
    }
    Ok(())
}

fn image_header_matches(path: &Path, kind: &str) -> Result<bool> {
    let mut file = File::open(path)
        .map_err(|source| CompanionError::io("open upload image candidate", source))?;
    let mut bytes = [0u8; 16];
    let count = file
        .read(&mut bytes)
        .map_err(|source| CompanionError::io("read upload image header", source))?;
    Ok(match kind {
        "exfat" => count >= 11 && &bytes[3..11] == b"EXFAT   ",
        "ffpfsc" | "ffpfs" | "ffpkg" => {
            (count >= 7 && &bytes[3..7] == b"PFSC") || (count >= 4 && &bytes[..4] == b"PFSC")
        }
        _ => false,
    })
}

fn find_title_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in ["titleId", "titleID", "title_id", "titleid", "TITLE_ID"] {
                if let Some(Value::String(value)) = object.get(key) {
                    if validate_title_id(value).is_ok() {
                        return Some(value.clone());
                    }
                }
            }
            object.values().find_map(find_title_id)
        }
        Value::Array(values) => values.iter().find_map(find_title_id),
        _ => None,
    }
}

fn validate_title_id(value: &str) -> Result<()> {
    let valid = value.len() == 9
        && (value.starts_with("PPSA") || value.starts_with("CUSA"))
        && value[4..].bytes().all(|byte| byte.is_ascii_digit());
    if !valid {
        return Err(CompanionError::Upload(format!(
            "game title id must be PPSA/CUSA followed by five digits: {value:?}"
        )));
    }
    Ok(())
}

fn validate_engine_base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)
        .map_err(|error| CompanionError::Upload(format!("invalid engine URL: {error}")))?;
    if url.scheme() != "http"
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(CompanionError::Upload(
            "engine URL must be plain loopback HTTP without credentials or query".to_owned(),
        ));
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address == std::net::Ipv4Addr::new(127, 0, 0, 1),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !loopback {
        return Err(CompanionError::Upload(
            "engine URL must target 127.0.0.1, localhost, or ::1".to_owned(),
        ));
    }
    if !url.path().is_empty() && url.path() != "/" {
        return Err(CompanionError::Upload(
            "engine URL may not contain a base path".to_owned(),
        ));
    }
    Ok(url)
}

fn validate_addr(addr: &str) -> Result<()> {
    if addr.is_empty() || addr.len() > 255 || addr.chars().any(|character| character.is_control()) {
        return Err(CompanionError::Upload("PS5 address is invalid".to_owned()));
    }
    Ok(())
}

fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty()
        || job_id.len() > 256
        || !job_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CompanionError::Upload(
            "engine job id is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_manifest_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(CompanionError::Upload(format!(
            "invalid manifest path {path:?}"
        )));
    }
    for component in path.split('/') {
        validate_component(component)?;
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    validate_manifest_path(path)
}

fn validate_remote_relative_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(CompanionError::Upload(format!(
            "invalid destination path {path:?}"
        )));
    }
    for component in path.split('/') {
        validate_remote_component(component)?;
    }
    Ok(())
}

fn validate_remote_component(component: &str) -> Result<()> {
    validate_component(component)
}

fn validate_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(':')
        || component.chars().any(|character| character.is_control())
        || component.ends_with('.')
        || component.ends_with(' ')
    {
        return Err(CompanionError::Upload(format!(
            "unsafe upload path component {component:?}"
        )));
    }
    Ok(())
}

fn relative_utf8(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .map_err(|_| CompanionError::Upload("artifact escaped staging root".to_owned()))?
        .to_str()
        .ok_or_else(|| CompanionError::Upload("artifact path is not UTF-8".to_owned()))
        .map(|value| value.replace('\\', "/"))
}

fn relative_parent(path: &str) -> Result<String> {
    Ok(path
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_owned())
        .unwrap_or_default())
}

fn native_relative_path(value: &str) -> PathBuf {
    value
        .split('/')
        .fold(PathBuf::new(), |mut path, component| {
            path.push(component);
            path
        })
}

fn read_json_response(response: ureq::Response, _status: Option<u16>) -> Result<Value> {
    let bytes = read_limited_response(response)
        .map_err(|message| CompanionError::Upload(format!("read engine response: {message}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| CompanionError::Upload(format!("parse engine JSON response: {error}")))
}

fn read_limited_response(response: ureq::Response) -> std::result::Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut reader = response
        .into_reader()
        .take((MAX_ENGINE_RESPONSE_BYTES + 1) as u64);
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_ENGINE_RESPONSE_BYTES {
        return Err(format!(
            "engine response exceeded the {} byte limit",
            MAX_ENGINE_RESPONSE_BYTES
        ));
    }
    Ok(bytes)
}

fn json_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn find_numeric_field(value: &Value, keys: &[&str]) -> Option<u64> {
    if let Some(value) = json_u64(value, keys) {
        return Some(value);
    }
    match value {
        Value::Object(object) => object
            .values()
            .find_map(|value| find_numeric_field(value, keys)),
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_numeric_field(value, keys)),
        _ => None,
    }
}

fn parse_volume_free_bytes(value: &Value, target: &str) -> Result<u64> {
    let volumes = value
        .get("volumes")
        .and_then(Value::as_array)
        .ok_or_else(|| CompanionError::Upload("volume response omitted volumes".to_owned()))?;
    let mut best: Option<(&Map<String, Value>, usize)> = None;
    for volume in volumes {
        let object = volume.as_object().ok_or_else(|| {
            CompanionError::Upload("volume response contains a malformed volume".to_owned())
        })?;
        let path = object.get("path").and_then(Value::as_str).ok_or_else(|| {
            CompanionError::Upload("volume response omitted volume path".to_owned())
        })?;
        let path = normalize_remote_path(path)?;
        if (path == "/" || target == path || target.starts_with(&(path.clone() + "/")))
            && best.map(|(_, length)| path.len() > length).unwrap_or(true)
        {
            best = Some((object, path.len()));
        }
    }
    let (volume, _) = best
        .ok_or_else(|| CompanionError::Upload(format!("no PS5 storage volume covers {target}")))?;
    let values = ["free_bytes", "allocatable_bytes", "likely_fits_bytes"]
        .iter()
        .filter_map(|key| volume.get(*key).and_then(Value::as_u64))
        .collect::<Vec<_>>();
    if values.is_empty() || values.contains(&0) {
        return Err(CompanionError::Upload(
            "PS5 volume response omitted positive numeric free-space data".to_owned(),
        ));
    }
    Ok(*values.iter().min().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct RemoteEntry {
    path: String,
    kind: RemoteKind,
    size: Option<u64>,
}

fn parse_remote_entries(value: &Value) -> Result<Vec<RemoteEntry>> {
    let entries = match value {
        Value::Array(values) => values,
        Value::Object(object) => {
            if let Some(truncated) = object.get("truncated") {
                if truncated.as_bool() != Some(false) {
                    return Err(CompanionError::Upload(
                        "directory listing is truncated or has an invalid truncation flag"
                            .to_owned(),
                    ));
                }
            }
            let nested = ["entries", "files", "items", "children", "data"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_array))
                .ok_or_else(|| {
                    CompanionError::Upload("directory response omitted entries".to_owned())
                })?;
            nested
        }
        _ => {
            return Err(CompanionError::Upload(
                "directory response is not a JSON array or object".to_owned(),
            ))
        }
    };
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            CompanionError::Upload("directory response contains a malformed entry".to_owned())
        })?;
        let path = object
            .get("path")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| CompanionError::Upload("directory entry omitted path".to_owned()))?;
        let kind = if object
            .get("is_dir")
            .or_else(|| object.get("is_directory"))
            .and_then(Value::as_bool)
            == Some(true)
            || object.get("directory").and_then(Value::as_bool) == Some(true)
            || object
                .get("kind")
                .or_else(|| object.get("type"))
                .and_then(Value::as_str)
                .map(|value| {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "dir" | "directory" | "folder"
                    )
                })
                == Some(true)
        {
            RemoteKind::Directory
        } else if object.get("is_file").and_then(Value::as_bool) == Some(true)
            || object
                .get("kind")
                .or_else(|| object.get("type"))
                .and_then(Value::as_str)
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "file" | "regular"))
                == Some(true)
        {
            RemoteKind::File
        } else {
            return Err(CompanionError::Upload(format!(
                "directory entry has no recognized kind: {path}"
            )));
        };
        let size = object
            .get("size")
            .or_else(|| object.get("bytes"))
            .and_then(Value::as_u64);
        result.push(RemoteEntry {
            path: path.to_owned(),
            kind,
            size,
        });
    }
    Ok(result)
}

fn remote_join(root: &str, child: &str) -> Result<String> {
    if child.starts_with('/') {
        return normalize_remote_path(child);
    }
    validate_remote_relative_path(child)?;
    normalize_remote_path(&format!("{}/{}", root.trim_end_matches('/'), child))
}

fn normalize_remote_path(path: &str) -> Result<String> {
    if !path.starts_with('/') {
        return Err(CompanionError::Upload(format!(
            "remote path is not absolute: {path:?}"
        )));
    }
    let mut result = String::from('/');
    for component in path.split('/').filter(|component| !component.is_empty()) {
        validate_remote_component(component)?;
        if result.len() > 1 {
            result.push('/');
        }
        result.push_str(component);
    }
    Ok(result)
}

fn is_remote_child(parent: &str, child: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    child.starts_with(&(parent.to_owned() + "/")) && child != parent
}

fn is_success_status(status: &str) -> bool {
    matches!(
        status,
        "complete" | "completed" | "success" | "succeeded" | "done"
    )
}

fn format_poll_error(
    engine_job_id: &str,
    error: CompanionError,
    receipt: Result<PathBuf>,
) -> String {
    match receipt {
        Ok(path) => format!(
            "polling engine job {engine_job_id} failed: {error}; receipt retained at {}",
            path.display()
        ),
        Err(receipt_error) => format!(
            "polling engine job {engine_job_id} failed: {error}; could not retain receipt: {receipt_error}"
        ),
    }
}

fn is_failure_status(status: &str) -> bool {
    status.contains("fail") || matches!(status, "error" | "cancelled" | "canceled" | "aborted")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_url_is_loopback_only() {
        assert!(validate_engine_base_url("http://127.0.0.1:19113").is_ok());
        assert!(validate_engine_base_url("http://localhost:19113/").is_ok());
        assert!(validate_engine_base_url("https://127.0.0.1:19113").is_err());
        assert!(validate_engine_base_url("http://192.168.1.2:19113").is_err());
    }

    #[test]
    fn remote_paths_cannot_escape() {
        assert!(normalize_remote_path("/data/homebrew/x").is_ok());
        assert!(normalize_remote_path("/data/homebrew/../x").is_err());
    }

    #[test]
    fn title_ids_are_strict() {
        assert!(validate_title_id("PPSA00001").is_ok());
        assert!(validate_title_id("CUSA12345").is_ok());
        assert!(validate_title_id("PPSA0000/").is_err());
        assert!(validate_title_id("GAME").is_err());
    }
}
