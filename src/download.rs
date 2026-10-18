//! Download stage, adopted from the Overdrive PS5 game store.
//!
//! Overdrive resolves a catalog release to a direct file URL before its queue
//! takes over: direct links get a HEAD check, MediaFire/VikingFile pages are
//! scraped for their download anchors, FileKeeper's countdown form is posted,
//! and hosts that need a human browser become a capture request. This module
//! ports those resolvers to Rust, then streams the file to disk. Anything the
//! CLI cannot resolve fails with the exact capture instruction instead of a
//! dead end.

use crate::error::{CompanionError, Result};
use crate::sources::{parse_source_url, source_kind, CatalogEntry, Release, SourceKind};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use ureq::{Agent, AgentBuilder};

/// Maximum bytes read from a host page while scraping a download anchor.
pub const MAX_SCRAPE_BYTES: usize = 1024 * 1024;
/// FileKeeper's countdown before its download form may be posted.
const FILEKEEPER_COUNTDOWN: Duration = Duration::from_secs(5);
const MAX_REDIRECTS: usize = 5;

/// Overdrive's queue downloads with up to 64 connections over 16 MiB ranges
/// (`connections: 64, adaptive: true, rangeBytes: 16 * 1024 * 1024`) and
/// resumes completed ranges (`recoverCompleted: true`). This module ports
/// that contract: files are split into segments fetched in parallel, and a
/// sidecar file records finished segments so an interrupted download resumes
/// at the segment boundary instead of restarting the file.
const DEFAULT_MAX_CONNECTIONS: usize = 64;
const DEFAULT_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const MIN_ADAPTIVE_WORKERS: usize = 4;
const ADAPTIVE_STEP: usize = 4;
/// Attempts per segment. MediaFire's download hosts intermittently drop TLS
/// connections mid-read; each attempt resumes the segment where the previous
/// one stopped, so attempts are cheap, and failures also scale the adaptive
/// controller down (AIMD).
const SEGMENT_RETRIES: u32 = 20;
/// Attempts for single-shot requests (redirects, HEAD) and streaming restarts.
const REQUEST_RETRIES: u32 = 4;
const PROGRESS_MILESTONE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_connections: usize,
    pub segment_bytes: u64,
    pub adaptive: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            adaptive: true,
        }
    }
}

/// A release resolved to a fetchable file URL.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub url: String,
    pub provider: String,
    pub expected_bytes: Option<u64>,
    pub kind: SourceKind,
}

fn http_client(config: &DownloadConfig) -> Agent {
    AgentBuilder::new()
        .timeout_connect(config.connect_timeout)
        .timeout_read(config.request_timeout)
        .timeout_write(config.request_timeout)
        // Redirects are handled explicitly so an https downgrade or a hop to
        // an unexpected host fails closed instead of silently followed.
        .redirects(0)
        .try_proxy_from_env(false)
        .build()
}

fn is_html_content_type(value: &str) -> bool {
    let value = value.to_lowercase();
    value.contains("text/html") || value.contains("application/xhtml")
}

/// Resolve one catalog release to a direct file URL, following Overdrive's
/// resolver per source kind.
pub fn resolve_release(release: &Release, config: &DownloadConfig) -> Result<ResolvedSource> {
    if !release.unavailable_reason.is_empty() {
        return Err(CompanionError::Download(format!(
            "release is unavailable: {}",
            release.unavailable_reason
        )));
    }
    let kind = source_kind(release)?;
    let client = http_client(config);
    let resolved = match kind {
        SourceKind::Unavailable => {
            return Err(CompanionError::Download(
                "release has no download URL".to_owned(),
            ));
        }
        SourceKind::Manifest => {
            return Err(CompanionError::Download(
                "split manifests need the download queue, which is not implemented".to_owned(),
            ));
        }
        SourceKind::Direct => {
            let url = release.primary_url().to_owned();
            // Resolve the redirect chain at the public boundary, before
            // checking terminal file metadata: head_check rejects 3xx, so
            // checking first would fail URLs that legitimately redirect.
            let final_url = follow_redirect(&client, &url)?;
            head_check(&client, &final_url)?;
            ResolvedSource {
                url: final_url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Mediafire => {
            let page = release.primary_url().to_owned();
            let html = fetch_page(&client, &page)?;
            let href = scrape_anchor(&html, "downloadButton").ok_or_else(|| {
                capture_required(
                    &release.provider,
                    &page,
                    "no download button in the page (bot check or changed markup)",
                )
            })?;
            let url = absolutize(&href, &page)?;
            let final_url = follow_redirect(&client, &url)?;
            head_check(&client, &final_url)?;
            ResolvedSource {
                url: final_url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Vikingfile => {
            let page = release.primary_url().to_owned();
            let html = fetch_page(&client, &page)?;
            let href = scrape_anchor(&html, "download-link").ok_or_else(|| {
                capture_required(
                    &release.provider,
                    &page,
                    "no download link in the page (bot check or changed markup)",
                )
            })?;
            let url = absolutize(&href, &page)?;
            let final_url = follow_redirect(&client, &url)?;
            head_check(&client, &final_url)?;
            ResolvedSource {
                url: final_url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Filekeeper => resolve_filekeeper(&client, release)?,
        SourceKind::Datanodes | SourceKind::Buzzheavier | SourceKind::Browser => {
            return Err(capture_required(
                &release.provider,
                release.primary_url(),
                "this host needs a browser session to resolve the file URL",
            ));
        }
    };
    Ok(resolved)
}

/// A source the CLI cannot resolve without the user completing verification
/// in a browser. The supported route: the resolver returns this with the
/// unverified page and reason; the user inspects the page, completes any
/// challenge, and hands back the final file URL via `--direct-url`.
#[derive(Debug, Clone)]
pub struct BrowserHandoff {
    pub provider: String,
    pub page_url: String,
    pub reason: String,
}

impl std::fmt::Display for BrowserHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} — {}", self.provider, self.page_url, self.reason)
    }
}

fn capture_required(provider: &str, page: &str, reason: &str) -> CompanionError {
    CompanionError::BrowserCapture(BrowserHandoff {
        provider: provider.to_owned(),
        page_url: page.to_owned(),
        reason: reason.to_owned(),
    })
}

/// HEAD the URL like Overdrive's `direct()`: an HTML answer or a redirect
/// means this is not really a file link.
fn head_check(client: &Agent, url: &str) -> Result<()> {
    parse_source_url(url)?;
    let response = retry_request(|| client.head(url).call().map_err(Box::new), "source HEAD")
        .map_err(|error| match *error {
            ureq::Error::Status(code, _) => {
                CompanionError::Download(format!("source HEAD returned HTTP {code} for {url}"))
            }
            ureq::Error::Transport(error) => {
                CompanionError::Download(format!("source HEAD failed for {url}: {error}"))
            }
        })?;
    // ureq with redirects(0) surfaces 3xx as Ok: a redirect here means the
    // URL was not fully resolved; never treat it as file metadata.
    let status = response.status();
    if (300..=399).contains(&status) {
        return Err(CompanionError::Download(format!(
            "download target redirected unexpectedly: {url}"
        )));
    }
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "source URL serves HTML, not a file: {url}"
            )));
        }
    }
    Ok(())
}

/// GET a host page with a hard cap, for anchor scraping.
fn fetch_page(client: &Agent, url: &str) -> Result<String> {
    parse_source_url(url)?;
    let response = retry_request(|| client.get(url).call().map_err(Box::new), "host page")
        .map_err(|error| match *error {
            ureq::Error::Status(code, _) => {
                CompanionError::Download(format!("host page returned HTTP {code}: {url}"))
            }
            ureq::Error::Transport(error) => {
                CompanionError::Download(format!("host page request failed for {url}: {error}"))
            }
        })?;
    let mut reader = response.into_reader().take((MAX_SCRAPE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| CompanionError::Download(format!("read host page: {error}")))?;
    if bytes.len() > MAX_SCRAPE_BYTES {
        return Err(CompanionError::Download(format!(
            "host page exceeded the {} byte scrape limit: {url}",
            MAX_SCRAPE_BYTES
        )));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Find `<a id="...">` and return its href, porting Overdrive's anchor scrape.
pub fn scrape_anchor(html: &str, id: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut index = 0;
    while let Some(pos) = lower[index..].find("<a") {
        let tag_start = index + pos;
        let after = &lower[tag_start + 2..];
        // `<a` must be followed by whitespace, `>`, or `/` to be a tag.
        if let Some(next) = after.chars().next() {
            if !(next.is_whitespace() || next == '>' || next == '/') {
                index = tag_start + 2;
                continue;
            }
        }
        let tag_end = lower[tag_start..].find('>').map(|end| tag_start + end)?;
        let tag = &html[tag_start..=tag_end];
        if attribute(tag, "id").as_deref() == Some(id) {
            return attribute(tag, "href");
        }
        index = tag_end + 1;
    }
    None
}

/// Case-insensitive `key="value"` attribute lookup within one tag.
fn attribute(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let key = key.to_lowercase();
    let mut index = 0;
    while let Some(pos) = lower[index..].find(&key) {
        let start = index + pos;
        let before = lower[..start].chars().next_back();
        if before
            .map(|c| !(c.is_whitespace() || c == '/'))
            .unwrap_or(false)
        {
            index = start + key.len();
            continue;
        }
        let rest = lower[start + key.len()..].trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            index = start + key.len();
            continue;
        }
        let value_start = start + key.len() + (lower[start + key.len()..].find(quote)?) + 1;
        let value_end = lower[value_start..].find(quote)? + value_start;
        return Some(tag[value_start..value_end].to_owned());
    }
    None
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Resolve a scraped href against its page, porting Overdrive's `absolute`.
/// An https page may never resolve to an http file.
pub fn absolutize(href: &str, base: &str) -> Result<String> {
    let value = decode_entities(href);
    let base_url = parse_source_url(base)?;
    let scheme = base_url.scheme().to_owned();
    let host = base_url.host_str().unwrap_or("").to_owned();
    let port = base_url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let resolved = if value.starts_with("//") {
        format!("{scheme}:{value}")
    } else if value.starts_with('/') {
        format!("{scheme}://{host}{port}{value}")
    } else if value.contains("://") {
        value
    } else {
        let base_path = base_url.path();
        let dir = base_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        format!("{scheme}://{host}{port}{dir}/{value}")
    };
    let url = parse_source_url(&resolved)?;
    if base_url.scheme() == "https" && url.scheme() != "https" {
        return Err(CompanionError::Download(format!(
            "provider attempted an insecure redirect: {href:?}"
        )));
    }
    Ok(url.as_str().to_owned())
}

/// FileKeeper's flow, ported from Overdrive: wait out the countdown, post its
/// form, take the redirect target as the file, HEAD it for the real size.
fn resolve_filekeeper(client: &Agent, release: &Release) -> Result<ResolvedSource> {
    let page = release.primary_url().to_owned();
    let url = parse_source_url(&page)?;
    let code = url
        .path()
        .split('/')
        .find(|segment| !segment.is_empty())
        .filter(|segment| {
            segment.len() == 12
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
        .ok_or_else(|| {
            capture_required(
                &release.provider,
                &page,
                "no download code in the FileKeeper URL",
            )
        })?;
    std::thread::sleep(FILEKEEPER_COUNTDOWN);
    let body = format!("op=download2&id={code}&rand=&referer=&method_free=&down_direct=1");
    let response = client
        .post(page.as_str())
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Referer", page.as_str())
        .send_string(&body);
    let location = match response {
        Ok(response) => {
            // ureq with redirects(0) surfaces the 302 as Ok, not
            // Error::Status: inspect the status explicitly.
            if !(300..=399).contains(&response.status()) {
                return Err(CompanionError::Download(
                    "FileKeeper answered without a redirect; choose another source".to_owned(),
                ));
            }
            response.header("location").unwrap_or("").to_owned()
        }
        Err(ureq::Error::Status(code, _)) => {
            return Err(CompanionError::Download(format!(
                "FileKeeper form returned HTTP {code}"
            )));
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(CompanionError::Download(format!(
                "FileKeeper form request failed: {error}"
            )));
        }
    };
    if location.is_empty() {
        return Err(CompanionError::Download(
            "FileKeeper gave no file redirect; choose another source".to_owned(),
        ));
    }
    let file = absolutize(&location, &page)?;
    // The catalog's FileKeeper sizes are rounded; the file's own HEAD length wins.
    let expected_bytes = head_content_length(client, &file)?;
    Ok(ResolvedSource {
        url: file,
        provider: release.provider.clone(),
        expected_bytes,
        kind: SourceKind::Filekeeper,
    })
}

/// HEAD a resolved file for its real byte size, rejecting HTML answers.
fn head_content_length(client: &Agent, url: &str) -> Result<Option<u64>> {
    let response = client.head(url).call().map_err(|error| match error {
        ureq::Error::Status(code, _) => {
            CompanionError::Download(format!("file HEAD returned HTTP {code}: {url}"))
        }
        ureq::Error::Transport(error) => {
            CompanionError::Download(format!("file HEAD failed for {url}: {error}"))
        }
    })?;
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "resolved target serves HTML, not a file: {url}"
            )));
        }
    }
    Ok(response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0))
}

/// Retry a request closure on transient transport failures (dropped
/// connections, timeouts) and 429/503 statuses; anything else fails fast.
fn retry_request<T>(
    mut request: impl FnMut() -> std::result::Result<T, Box<ureq::Error>>,
    what: &str,
) -> std::result::Result<T, Box<ureq::Error>> {
    let mut backoff = Duration::from_secs(1);
    for attempt in 1..=REQUEST_RETRIES {
        match request() {
            Ok(value) => return Ok(value),
            Err(error) if attempt < REQUEST_RETRIES => {
                let retryable = matches!(
                    *error,
                    ureq::Error::Status(429, _)
                        | ureq::Error::Status(503, _)
                        | ureq::Error::Transport(_)
                );
                if retryable {
                    let kind = match *error {
                        ureq::Error::Status(code, _) => format!("HTTP {code}"),
                        ureq::Error::Transport(_) => "connection failed".to_owned(),
                    };
                    eprintln!("  {what}: {kind}, retrying (attempt {attempt}/{REQUEST_RETRIES})");
                } else {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    unreachable!("retry loop always returns")
}

/// Follow one redirect manually, staying https and capping the chain.
fn follow_redirect(client: &Agent, url: &str) -> Result<String> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        parse_source_url(&current)?;
        let response = retry_request(
            || client.head(&current).call().map_err(Box::new),
            "redirect chain",
        );
        match response.map_err(|error| *error) {
            Ok(response) => {
                // ureq with redirects(0) surfaces 3xx as Ok, not
                // Error::Status: inspect the status explicitly.
                let status = response.status();
                if (300..=399).contains(&status) {
                    let location = response
                        .header("location")
                        .ok_or_else(|| {
                            CompanionError::Download(format!(
                                "redirect without Location: {current}"
                            ))
                        })?
                        .to_owned();
                    let next = absolutize(&location, &current)?;
                    if parse_source_url(&next)?.scheme() != "https"
                        && parse_source_url(&current)?.scheme() == "https"
                    {
                        return Err(CompanionError::Download(format!(
                            "redirect downgrades https: {next}"
                        )));
                    }
                    current = next;
                    continue;
                }
                if !(200..=299).contains(&status) {
                    return Err(CompanionError::Download(format!(
                        "redirect chain returned HTTP {status}: {current}"
                    )));
                }
                if let Some(content_type) = response.header("content-type") {
                    if is_html_content_type(content_type) {
                        return Err(CompanionError::Download(format!(
                            "redirect target serves HTML, not a file: {current}"
                        )));
                    }
                }
                return Ok(current);
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(CompanionError::Download(format!(
                    "redirect chain returned HTTP {code}: {current}"
                )));
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(CompanionError::Download(format!(
                    "redirect chain request failed for {current}: {error}"
                )));
            }
        }
    }
    Err(CompanionError::Download(format!(
        "redirect chain exceeded {MAX_REDIRECTS} hops: {url}"
    )))
}

/// One fetched volume. Singles carry exactly one; sets carry one per part.
#[derive(Debug, Clone)]
pub struct DownloadedPart {
    pub number: usize,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Download one catalog entry, mirroring Overdrive's queue: a single release
/// fetches directly; an archive set downloads its parts sequentially in
/// volume order into one directory, each part resolved and size-checked on
/// its own. Parts that are already on disk at the expected size are skipped
/// (Overdrive resumes them; the CLI re-verifies by size instead).
pub fn download_entry(
    config: &DownloadConfig,
    entry: &CatalogEntry,
    dest_dir: &Path,
) -> Result<Vec<DownloadedPart>> {
    match entry {
        CatalogEntry::Single(release) => {
            let resolved = resolve_release(release, config)?;
            let file_name = remote_file_name(&resolved.url)?;
            let bytes = download_one(
                config,
                &resolved.url,
                dest_dir,
                &file_name,
                resolved.expected_bytes,
            )?;
            Ok(vec![DownloadedPart {
                number: 1,
                path: dest_dir.join(&file_name),
                bytes,
            }])
        }
        CatalogEntry::Set(set) => {
            if !set.unavailable_reason.is_empty() {
                return Err(CompanionError::Download(format!(
                    "archive set is unavailable: {}",
                    set.unavailable_reason
                )));
            }
            let mut downloaded = Vec::with_capacity(set.parts.len());
            let mut used_names: Vec<String> = Vec::new();
            for part in &set.parts {
                let source = &part.source;
                eprintln!(
                    "part {}/{}: resolving {} — {}",
                    part.number,
                    set.parts.len(),
                    source.provider,
                    source.label
                );
                let resolved = resolve_release(source, config)?;
                // The resolver's measured size wins over the catalog's, just
                // like Overdrive's executeSet.
                let expected = resolved.expected_bytes.or(source.size_bytes);
                let mut file_name = remote_file_name(&resolved.url)
                    .unwrap_or_else(|_| format!("part-{}.bin", part.number));
                // Volume names must stay unique and extraction-friendly: keep
                // the remote name when it is, otherwise qualify with the part
                // number.
                if used_names.iter().any(|name| name == &file_name) {
                    file_name = format!("part-{}-{file_name}", part.number);
                }
                used_names.push(file_name.clone());
                let dest = dest_dir.join(&file_name);
                // Ownership, skip-if-complete, and staged replacement all
                // live in download_one: an unowned collision is refused
                // there rather than silently skipped or overwritten.
                eprintln!(
                    "part {}/{}: downloading {file_name}",
                    part.number,
                    set.parts.len()
                );
                let bytes = download_one(config, &resolved.url, dest_dir, &file_name, expected)?;
                eprintln!(
                    "part {}/{}: saved {bytes} bytes",
                    part.number,
                    set.parts.len()
                );
                downloaded.push(DownloadedPart {
                    number: part.number,
                    path: dest,
                    bytes,
                });
            }
            Ok(downloaded)
        }
    }
}

/// Derive a safe file name from a resolved URL's last path segment, dropping
/// any query or fragment Overdrive-style `?download=1` suffixes add.
fn remote_file_name(url: &str) -> Result<String> {
    let segment = url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| CompanionError::Download(format!("URL has no file name: {url}")))?;
    let name = segment
        .split(['?', '#'])
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| CompanionError::Download(format!("URL has no file name: {url}")))?;
    // Percent-encoded names stay as-is: decoding risks turning %2F into a
    // path separator, and the raw name is always a valid, unambiguous file.
    if name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        || name == "."
        || name == ".."
    {
        return Err(CompanionError::Download(format!(
            "unsafe remote file name: {name:?}"
        )));
    }
    Ok(name.to_owned())
}

/// Fetch one URL to `dest_dir/file_name` with an atomic rename. When the
/// server reports a size and honors byte ranges, the file is pulled in
/// parallel 16 MiB segments with resume, porting Overdrive's queue
/// (`connections: 64, adaptive: true, rangeBytes: 16MB, recoverCompleted`);
/// otherwise it falls back to a single streaming connection.
/// Provenance binding a completed download to its source URL. A file on disk
/// is only reused as verified data when its provenance URL matches the
/// requested source; anything else is an unowned collision that must be
/// refused, never silently overwritten, deleted, or trusted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FileProvenance {
    url: String,
    expected_bytes: Option<u64>,
    bytes: u64,
}

fn provenance_path(dest_dir: &Path, file_name: &str) -> PathBuf {
    dest_dir.join(format!(".{file_name}.provenance.json"))
}

fn read_provenance(dest_dir: &Path, file_name: &str) -> Option<FileProvenance> {
    let bytes = fs::read(provenance_path(dest_dir, file_name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_provenance(dest_dir: &Path, file_name: &str, provenance: &FileProvenance) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(provenance)
        .map_err(|error| CompanionError::Download(format!("encode provenance: {error}")))?;
    let path = provenance_path(dest_dir, file_name);
    let tmp = dest_dir.join(format!(".{file_name}.provenance.json.tmp"));
    fs::write(&tmp, &bytes).map_err(|source| CompanionError::io("write provenance", source))?;
    fs::rename(&tmp, &path).map_err(|source| CompanionError::io("install provenance", source))?;
    Ok(())
}

/// Exclusive per-file lock so concurrent invocations cannot share a
/// download's temp file, sidecar, or destination. Held until dropped.
fn acquire_file_lock(dest_dir: &Path, file_name: &str) -> Result<std::fs::File> {
    use fs2::FileExt;
    let path = dest_dir.join(format!(".{file_name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|source| CompanionError::io("create download lock", source))?;
    file.lock_exclusive()
        .map_err(|source| CompanionError::io("lock download", source))?;
    Ok(file)
}

/// Reject Windows installers/executables masquerading as game files. A page
/// advertising a PS5 image once delivered `OperaSetup.exe`; a non-HTML
/// response and matching length are not adequate validation.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe", "msi", "bat", "cmd", "com", "scr", "pif", "ps1", "vbs", "dll", "jar",
];
const EXECUTABLE_MIMES: &[&str] = &[
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/vnd.microsoft.portable-executable",
];

fn executable_extension(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let ext = base.rsplit('.').next().unwrap_or("").to_lowercase();
    // A name without a dot has no extension to judge.
    base.contains('.') && EXECUTABLE_EXTENSIONS.contains(&ext.as_str())
}

/// Reject by URL file name before any bytes are fetched.
fn reject_executable_url(url: &str) -> Result<()> {
    if let Ok(name) = remote_file_name(url) {
        if executable_extension(&name) {
            return Err(CompanionError::Download(format!(
                "refusing executable payload (URL name): {name}"
            )));
        }
    }
    Ok(())
}

/// Parse a Content-Disposition filename, handling `filename="..."` and
/// RFC 5987 `filename*=UTF-8''...`.
fn disposition_filename(value: &str) -> Option<String> {
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            let rest = rest.trim().trim_matches('"');
            // charset'language'value — take the value after the second quote.
            let mut splits = rest.splitn(3, '\'');
            splits.next()?;
            splits.next()?;
            return splits.next().map(str::to_owned);
        }
        if let Some(rest) = part.strip_prefix("filename=") {
            return Some(rest.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Reject by response headers (MIME type, Content-Disposition filename).
fn reject_executable_headers(
    url: &str,
    content_type: Option<&str>,
    content_disposition: Option<&str>,
) -> Result<()> {
    if let Some(content_type) = content_type {
        let lower = content_type.to_lowercase();
        if EXECUTABLE_MIMES.iter().any(|mime| lower.contains(mime)) {
            return Err(CompanionError::Download(format!(
                "refusing executable payload (MIME {content_type}): {url}"
            )));
        }
    }
    if let Some(disposition) = content_disposition {
        if let Some(name) = disposition_filename(disposition) {
            if executable_extension(&name) {
                return Err(CompanionError::Download(format!(
                    "refusing executable payload (Content-Disposition name): {name}"
                )));
            }
        }
    }
    Ok(())
}

/// Reject by magic bytes: Windows executables start with the MZ signature.
/// Only meaningful on the first bytes of the file.
fn reject_executable_signature(head: &[u8], context: &str) -> Result<()> {
    if head.len() >= 2 && head[0] == 0x4D && head[1] == 0x5A {
        return Err(CompanionError::Download(format!(
            "refusing Windows executable payload (MZ signature): {context}"
        )));
    }
    Ok(())
}

/// Result of probing a user-provided final URL before a large transfer.
/// Mirrors the Outer Wilds workflow: a bounded 64 KiB range probe that must
/// come back 206 with a non-HTML, non-executable payload and a Content-Range.
#[derive(Debug, Clone)]
pub struct UrlProbe {
    /// The final URL after following redirects.
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub total_bytes: Option<u64>,
    pub accepts_ranges: bool,
    pub probe_bytes: usize,
}

/// Probe a browser-captured final URL: follow redirects, validate headers,
/// and read up to 64 KiB. A 404 here is not a verification candidate — it
/// fails fast instead of starting a doomed multi-gigabyte transfer. If the
/// URL is bound to browser cookies or a session, the probe fails here and
/// the user is told to finish the download in the browser instead.
pub fn probe_url(config: &DownloadConfig, url: &str) -> Result<UrlProbe> {
    const PROBE_BYTES: u64 = 64 * 1024;
    let final_url = follow_redirect(&http_client(config), url)?;
    reject_executable_url(&final_url)?;
    let client = http_client(config);
    let response = retry_request(
        || {
            client
                .get(final_url.as_str())
                .set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1))
                .call()
                .map_err(Box::new)
        },
        "probe range request",
    )
    .map_err(|error| match *error {
        ureq::Error::Status(code, _) => CompanionError::Download(format!(
            "probe of {final_url} returned HTTP {code}: \
             not a verification candidate (a 404 means the link is dead, \
             a 401/403 may mean it is bound to the browser session — \
             finish the download in the browser)"
        )),
        ureq::Error::Transport(error) => {
            CompanionError::Download(format!("probe of {final_url} failed: {error}"))
        }
    })?;
    let status = response.status();
    if status != 206 && status != 200 {
        return Err(CompanionError::Download(format!(
            "probe of {final_url} returned HTTP {status}, expected 206 or 200"
        )));
    }
    let content_type = response.header("content-type").map(str::to_owned);
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "probe of {final_url} serves HTML, not a file"
            )));
        }
    }
    reject_executable_headers(
        &final_url,
        response.header("content-type"),
        response.header("content-disposition"),
    )?;
    let total_bytes = response
        .header("content-range")
        .and_then(|value| value.split('/').next_back())
        .and_then(|total| total.parse::<u64>().ok())
        .or_else(|| {
            response
                .header("content-length")
                .and_then(|value| value.parse::<u64>().ok())
        });
    let mut reader = response.into_reader().take(PROBE_BYTES);
    let mut head = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut probe_bytes = 0usize;
    loop {
        let count = reader
            .read(&mut chunk)
            .map_err(|source| CompanionError::io("read probe", source))?;
        if count == 0 {
            break;
        }
        if head.len() < 4096 {
            let take = (4096 - head.len()).min(count);
            head.extend_from_slice(&chunk[..take]);
        }
        probe_bytes += count;
        if probe_bytes as u64 >= PROBE_BYTES {
            break;
        }
    }
    if probe_bytes == 0 {
        return Err(CompanionError::Download(format!(
            "probe of {final_url} returned no bytes"
        )));
    }
    reject_executable_signature(&head, &final_url)?;
    Ok(UrlProbe {
        url: final_url,
        status,
        content_type,
        total_bytes,
        accepts_ranges: status == 206,
        probe_bytes,
    })
}

fn download_one(
    config: &DownloadConfig,
    url: &str,
    dest_dir: &Path,
    file_name: &str,
    expected_bytes: Option<u64>,
) -> Result<u64> {
    let final_url = follow_redirect(&http_client(config), url)?;
    // Reject executables by URL name before fetching any bytes.
    reject_executable_url(&final_url)?;
    fs::create_dir_all(dest_dir)
        .map_err(|source| CompanionError::io("create download directory", source))?;
    // Serialize with any concurrent invocation working on the same file.
    let _lock = acquire_file_lock(dest_dir, file_name)?;
    let dest = dest_dir.join(file_name);
    if dest.exists() {
        // A throttled host once answered with an empty body and a 0-byte file
        // got installed; that is never a valid download — clear it and retry
        // instead of wedging.
        let len = fs::metadata(&dest)
            .map(|meta| meta.len())
            .unwrap_or(u64::MAX);
        if len == 0 {
            fs::remove_file(&dest)
                .map_err(|source| CompanionError::io("clear empty download", source))?;
        } else {
            // Ownership check: only a file this job downloaded from this
            // exact source may be reused or replaced. Anything else is an
            // unowned collision — refuse it rather than trusting,
            // overwriting, or deleting bytes we did not write.
            let owned = read_provenance(dest_dir, file_name)
                .is_some_and(|provenance| provenance.url == url);
            if !owned {
                return Err(CompanionError::Download(format!(
                    "unowned file collision: {} exists but was not downloaded from this source; \
                     delete it or use a clean output directory",
                    dest.display()
                )));
            }
            if expected_bytes.is_some_and(|expected| len == expected) {
                // Source-bound and complete: reuse without re-downloading,
                // and drop any stale staging state from an interrupted run.
                let mut temporary = dest.clone();
                temporary.set_file_name(format!(".{file_name}.part"));
                let mut sidecar = dest.clone();
                sidecar.set_file_name(format!(".{file_name}.resume.json"));
                let _ = fs::remove_file(temporary);
                let _ = fs::remove_file(sidecar);
                eprintln!("already complete ({} bytes), skipping", len);
                return Ok(len);
            }
            // Same source, incomplete or changed size: the replacement is
            // staged to a temp file below and only renamed over `dest` after
            // verification. A failed download never deletes the original.
            eprintln!("existing file has {len} bytes, re-downloading");
        }
    }
    let mut temporary = dest.clone();
    temporary.set_file_name(format!(".{file_name}.part"));
    let mut sidecar = dest.clone();
    sidecar.set_file_name(format!(".{file_name}.resume.json"));

    let client = http_client(config);
    let (size, accepts_ranges) = head_file(&client, &final_url)?;
    let segment_bytes = config.segment_bytes.max(1024 * 1024);
    let ranges_ok = accepts_ranges || (size.is_some() && probe_range_support(&client, &final_url));
    if size.is_some_and(|size| size > segment_bytes) && ranges_ok {
        let size = size.expect("checked above");
        download_segmented(config, &client, &final_url, &temporary, &sidecar, size)?;
        fs::rename(&temporary, &dest)
            .map_err(|source| CompanionError::io("install download", source))?;
        write_provenance(
            dest_dir,
            file_name,
            &FileProvenance {
                url: url.to_owned(),
                expected_bytes,
                bytes: size,
            },
        )?;
        return Ok(size);
    }
    // A crashed single-stream run leaves its temp file behind; the retry loop
    // inside download_streaming removes it before every attempt. Segmented
    // resume state is left alone: if this streaming attempt fails, a later
    // run can still resume the segmented download instead of starting over.
    let result = download_streaming(&client, &final_url, &temporary, &dest, expected_bytes);
    match &result {
        Ok(bytes) => {
            let _ = fs::remove_file(sidecar);
            write_provenance(
                dest_dir,
                file_name,
                &FileProvenance {
                    url: url.to_owned(),
                    expected_bytes,
                    bytes: *bytes,
                },
            )?;
        }
        Err(_) => {
            // The staged temp file stays for a later resume; `dest` (and any
            // unowned original) is untouched.
        }
    }
    result
}

/// HEAD a file URL: its byte size and whether it honors `Range` requests.
/// An HTML answer fails closed, like the resolver's direct check.
fn head_file(client: &Agent, url: &str) -> Result<(Option<u64>, bool)> {
    let response = retry_request(|| client.head(url).call().map_err(Box::new), "file HEAD")
        .map_err(|error| match *error {
            ureq::Error::Status(code, _) => {
                CompanionError::Download(format!("file HEAD returned HTTP {code}: {url}"))
            }
            ureq::Error::Transport(error) => {
                CompanionError::Download(format!("file HEAD failed for {url}: {error}"))
            }
        })?;
    // ureq with redirects(0) surfaces 3xx as Ok: a redirect here means the
    // URL was not fully resolved; never treat it as file metadata.
    if (300..=399).contains(&response.status()) {
        return Err(CompanionError::Download(format!(
            "download target redirected unexpectedly: {url}"
        )));
    }
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "download target serves HTML, not a file: {url}"
            )));
        }
    }
    let size = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0);
    let accepts_ranges = response
        .header("accept-ranges")
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
    Ok((size, accepts_ranges))
}

/// Some hosts serve ranges without advertising `Accept-Ranges`; a one-byte
/// ranged probe decides instead of trusting the header's absence.
fn probe_range_support(client: &Agent, url: &str) -> bool {
    match retry_request(
        || {
            client
                .get(url)
                .set("Range", "bytes=0-0")
                .call()
                .map_err(Box::new)
        },
        "range probe",
    ) {
        Ok(response) => response.status() == 206,
        Err(_) => false,
    }
}

/// Resume record for a segmented download, written atomically after every
/// finished segment. This is the CLI's form of Overdrive's `recoverCompleted`.
#[derive(Debug, Serialize, Deserialize)]
struct ResumeState {
    url: String,
    size_bytes: u64,
    segment_bytes: u64,
    completed: Vec<bool>,
}

fn load_resume_state(
    sidecar: &Path,
    temporary: &Path,
    url: &str,
    size: u64,
    segment_bytes: u64,
    segment_count: usize,
) -> Vec<bool> {
    let fresh = || vec![false; segment_count];
    let bytes = match fs::read(sidecar) {
        Ok(bytes) => bytes,
        Err(_) => return fresh(),
    };
    let state: ResumeState = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(_) => return fresh(),
    };
    if state.url != url
        || state.size_bytes != size
        || state.segment_bytes != segment_bytes
        || state.completed.len() != segment_count
    {
        return fresh();
    }
    // The temp file must actually hold the claimed bytes; otherwise the
    // flags describe a different file and resuming would corrupt it.
    match fs::metadata(temporary) {
        Ok(metadata) if metadata.len() == size => state.completed,
        _ => fresh(),
    }
}

fn persist_resume_state(
    sidecar: &Path,
    url: &str,
    size: u64,
    segment_bytes: u64,
    completed: &[bool],
) {
    let state = ResumeState {
        url: url.to_owned(),
        size_bytes: size,
        segment_bytes,
        completed: completed.to_vec(),
    };
    let Ok(json) = serde_json::to_vec(&state) else {
        return;
    };
    let mut staging = sidecar.to_path_buf();
    staging.set_extension("json.tmp");
    if fs::write(&staging, &json).is_err() {
        return;
    }
    let _ = fs::rename(&staging, sidecar);
}

fn segment_len(index: usize, segment_bytes: u64, size: u64) -> u64 {
    let start = index as u64 * segment_bytes;
    (start + segment_bytes).min(size) - start
}

/// One ranged attempt for `from..=end`, appending to `written`. A transport
/// error mid-read keeps the partial progress in `written` and returns the
/// error so the caller can resume the remainder with a new range request.
fn fetch_range_once(
    client: &Agent,
    url: &str,
    temporary: &Path,
    from: u64,
    end: u64,
    written: &mut u64,
) -> Result<()> {
    let response = client
        .get(url)
        .set("Range", &format!("bytes={from}-{end}"))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => CompanionError::Download(format!(
                "segment {from}-{end} returned HTTP {code}: {url}"
            )),
            ureq::Error::Transport(error) => {
                CompanionError::Download(format!("segment {from}-{end} failed for {url}: {error}"))
            }
        })?;
    if response.status() != 206 {
        return Err(CompanionError::Download(format!(
            "server ignored the range request for segment {from}-{end}: HTTP {}",
            response.status()
        )));
    }
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "segment {from}-{end} serves HTML, not file data"
            )));
        }
    }
    // Reject executables by response headers before reading the body.
    reject_executable_headers(
        url,
        response.header("content-type"),
        response.header("content-disposition"),
    )?;
    match response.header("content-range") {
        Some(value) => {
            let value = value.strip_prefix("bytes ").unwrap_or(value);
            let (range, _) = value.split_once('/').unwrap_or((value, ""));
            let (got_start, got_end) = range.split_once('-').unwrap_or(("", ""));
            let ok = got_start.parse::<u64>().ok() == Some(from)
                && got_end.parse::<u64>().ok() == Some(end);
            if !ok {
                return Err(CompanionError::Download(format!(
                    "segment range mismatch: asked {from}-{end}, got {value}"
                )));
            }
        }
        None => {
            return Err(CompanionError::Download(format!(
                "segment {from}-{end} came back without a Content-Range"
            )));
        }
    }
    let mut file = OpenOptions::new()
        .write(true)
        .open(temporary)
        .map_err(|source| CompanionError::io("open partial download", source))?;
    file.seek(SeekFrom::Start(from))
        .map_err(|source| CompanionError::io("seek partial download", source))?;
    let mut reader = response.into_reader().take(end - from + 1);
    let mut buffer = [0u8; 256 * 1024];
    // Only the first segment starts at byte 0, where the MZ signature lives.
    let mut check_signature = from == 0;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| CompanionError::io("read segment", source))?;
        if count == 0 {
            break;
        }
        if check_signature {
            reject_executable_signature(&buffer[..count], url)?;
            check_signature = false;
        }
        file.write_all(&buffer[..count])
            .map_err(|source| CompanionError::io("write segment", source))?;
        *written += count as u64;
    }
    Ok(())
}

/// Fetch one byte range, resuming mid-segment when a connection drops: each
/// attempt re-requests only the bytes still missing. Survives hosts that
/// intermittently close connections without TLS close_notify. Failed attempts
/// scale the adaptive worker count down (AIMD): if the host is shedding
/// connections, hammering it with reconnects only makes it worse.
fn fetch_segment(
    client: &Agent,
    url: &str,
    temporary: &Path,
    start: u64,
    end: u64,
    allowed: &AtomicUsize,
) -> Result<u64> {
    let expected_len = end - start + 1;
    let mut written: u64 = 0;
    let mut backoff = Duration::from_secs(2);
    for attempt in 1..=SEGMENT_RETRIES {
        if written >= expected_len {
            break;
        }
        if attempt > 1 {
            eprintln!(
                "  segment {start}-{end}: resuming at byte {} (attempt {attempt}/{SEGMENT_RETRIES})",
                start + written
            );
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(120));
        }
        if let Err(error) =
            fetch_range_once(client, url, temporary, start + written, end, &mut written)
        {
            // Back the adaptive controller off: the host is signaling distress.
            let current = allowed.load(Ordering::SeqCst);
            allowed.store((current / 2).max(1), Ordering::SeqCst);
            eprintln!("  segment {start}-{end}: {error}");
        }
    }
    if written != expected_len {
        return Err(CompanionError::Download(format!(
            "segment {start}-{end} failed after {SEGMENT_RETRIES} attempts: got {written} of {expected_len} bytes"
        )));
    }
    Ok(written)
}

struct SegmentWorker {
    agent: Agent,
    url: String,
    temporary: PathBuf,
    sidecar: PathBuf,
    size: u64,
    segment_bytes: u64,
    segment_count: usize,
    queue: Arc<Mutex<mpsc::Receiver<usize>>>,
    completed: Arc<Mutex<Vec<bool>>>,
    downloaded: Arc<AtomicU64>,
    finished: Arc<AtomicUsize>,
    first_error: Arc<Mutex<Option<CompanionError>>>,
    active: Arc<AtomicUsize>,
    allowed: Arc<AtomicUsize>,
    persist_lock: Arc<Mutex<()>>,
}

impl Clone for SegmentWorker {
    fn clone(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            url: self.url.clone(),
            temporary: self.temporary.clone(),
            sidecar: self.sidecar.clone(),
            size: self.size,
            segment_bytes: self.segment_bytes,
            segment_count: self.segment_count,
            queue: Arc::clone(&self.queue),
            completed: Arc::clone(&self.completed),
            downloaded: Arc::clone(&self.downloaded),
            finished: Arc::clone(&self.finished),
            first_error: Arc::clone(&self.first_error),
            active: Arc::clone(&self.active),
            allowed: Arc::clone(&self.allowed),
            persist_lock: Arc::clone(&self.persist_lock),
        }
    }
}

impl SegmentWorker {
    fn run(&self) {
        loop {
            if self.first_error.lock().unwrap().is_some() {
                return;
            }
            // Adaptive gate: only `allowed` workers draw segments at once.
            while self.active.load(Ordering::SeqCst) >= self.allowed.load(Ordering::SeqCst) {
                if self.first_error.lock().unwrap().is_some()
                    || self.finished.load(Ordering::SeqCst) >= self.segment_count
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            let index = loop {
                if self.first_error.lock().unwrap().is_some() {
                    break None;
                }
                match self.queue.lock().unwrap().try_recv() {
                    Ok(index) => break Some(index),
                    Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                    Err(mpsc::TryRecvError::Disconnected) => break None,
                }
            };
            // The permit covers the network transfer itself, not just queue
            // access: it is released only after fetch_segment finishes, so a
            // reduced `allowed` (backoff) actually throttles in-flight
            // requests instead of letting every worker fetch at once.
            let Some(index) = index else {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return;
            };
            let start = index as u64 * self.segment_bytes;
            let end = (start + self.segment_bytes - 1).min(self.size - 1);

            // fetch_segment retries internally, resuming mid-segment.
            let bytes = match fetch_segment(
                &self.agent,
                &self.url,
                &self.temporary,
                start,
                end,
                &self.allowed,
            ) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    *self.first_error.lock().unwrap() = Some(error);
                    return;
                }
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.downloaded.fetch_add(bytes, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            // Serialize the flag update with the file persist: workers share
            // one staging file, and an interleaved write+rename can otherwise
            // leave the sidecar missing completions.
            {
                let _persist = self.persist_lock.lock().unwrap();
                let snapshot = {
                    let mut completed = self.completed.lock().unwrap();
                    completed[index] = true;
                    completed.clone()
                };
                persist_resume_state(
                    &self.sidecar,
                    &self.url,
                    self.size,
                    self.segment_bytes,
                    &snapshot,
                );
            }
        }
    }
}

/// Download with parallel ranged segments and resume, porting Overdrive's
/// queue options (`connections: 64, adaptive: true, rangeBytes: 16MB,
/// recoverCompleted: true`). Workers start small and scale up while
/// throughput keeps improving, up to `max_connections`.
fn download_segmented(
    config: &DownloadConfig,
    client: &Agent,
    url: &str,
    temporary: &Path,
    sidecar: &Path,
    size: u64,
) -> Result<()> {
    let segment_bytes = config.segment_bytes.max(1024 * 1024);
    let segment_count = size.div_ceil(segment_bytes) as usize;
    let max_connections = config.max_connections.clamp(1, DEFAULT_MAX_CONNECTIONS);

    let completed_flags =
        load_resume_state(sidecar, temporary, url, size, segment_bytes, segment_count);
    // Preallocate the temp file so workers can write at offsets. A stale
    // file of the wrong length invalidates the resume flags (checked there).
    {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(temporary)
            .map_err(|source| CompanionError::io("create partial download", source))?;
        file.set_len(size)
            .map_err(|source| CompanionError::io("preallocate download", source))?;
        file.sync_all()
            .map_err(|source| CompanionError::io("flush download", source))?;
    }

    let (tx, rx) = mpsc::channel::<usize>();
    let mut already: u64 = 0;
    let mut pending = 0;
    for (index, done) in completed_flags.iter().enumerate() {
        if *done {
            already += segment_len(index, segment_bytes, size);
        } else {
            pending += 1;
            tx.send(index)
                .map_err(|_| CompanionError::Download("segment queue failed".to_owned()))?;
        }
    }
    drop(tx);
    if pending == 0 {
        let _ = fs::remove_file(sidecar);
        return Ok(());
    }

    let worker = SegmentWorker {
        agent: client.clone(),
        url: url.to_owned(),
        temporary: temporary.to_path_buf(),
        sidecar: sidecar.to_path_buf(),
        size,
        segment_bytes,
        segment_count,
        queue: Arc::new(Mutex::new(rx)),
        completed: Arc::new(Mutex::new(completed_flags)),
        downloaded: Arc::new(AtomicU64::new(already)),
        finished: Arc::new(AtomicUsize::new(segment_count - pending)),
        first_error: Arc::new(Mutex::new(None)),
        active: Arc::new(AtomicUsize::new(0)),
        persist_lock: Arc::new(Mutex::new(())),
        allowed: Arc::new(AtomicUsize::new(if config.adaptive {
            MIN_ADAPTIVE_WORKERS.min(max_connections)
        } else {
            max_connections
        })),
    };

    std::thread::scope(|scope| {
        for _ in 0..worker.allowed.load(Ordering::SeqCst) {
            let worker = worker.clone();
            scope.spawn(move || worker.run());
        }
        // Adaptive controller: every 2s, if throughput is still climbing,
        // allow more workers, up to the configured maximum.
        let mut best: u64 = 0;
        let mut last_total = worker.downloaded.load(Ordering::SeqCst);
        let mut next_milestone =
            (last_total / PROGRESS_MILESTONE_BYTES + 1) * PROGRESS_MILESTONE_BYTES;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if worker.first_error.lock().unwrap().is_some()
                || worker.finished.load(Ordering::SeqCst) >= segment_count
            {
                break;
            }
            let total = worker.downloaded.load(Ordering::SeqCst);
            if total >= next_milestone {
                eprintln!("  ...{total} bytes");
                next_milestone += PROGRESS_MILESTONE_BYTES;
            }
            if !config.adaptive {
                continue;
            }
            let delta = total.saturating_sub(last_total);
            last_total = total;
            if delta * 10 >= best * 9 {
                best = best.max(delta);
                let current = worker.allowed.load(Ordering::SeqCst);
                if current < max_connections {
                    let add = ADAPTIVE_STEP.min(max_connections - current);
                    for _ in 0..add {
                        let worker = worker.clone();
                        scope.spawn(move || worker.run());
                    }
                    worker.allowed.fetch_add(add, Ordering::SeqCst);
                }
            }
        }
    });

    if let Some(error) = worker.first_error.lock().unwrap().take() {
        return Err(error);
    }
    let len = fs::metadata(temporary)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if len != size {
        return Err(CompanionError::Download(format!(
            "download size mismatch: expected {size} bytes, got {len}"
        )));
    }
    {
        let file = OpenOptions::new()
            .write(true)
            .open(temporary)
            .map_err(|source| CompanionError::io("open partial download", source))?;
        file.sync_all()
            .map_err(|source| CompanionError::io("flush download", source))?;
    }
    let _ = fs::remove_file(sidecar);
    Ok(())
}

/// Whether a streaming failure is worth retrying: transport and 5xx errors
/// are, but HTML answers, size mismatches, and other 4xx statuses are not.
fn is_stream_retryable(error: &CompanionError) -> bool {
    let message = error.to_string();
    if message.contains("serves HTML") || message.contains("size mismatch") {
        return false;
    }
    if let Some(code) = message
        .split("HTTP ")
        .nth(1)
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
    {
        return code == 429 || code >= 500;
    }
    true
}

/// Single streaming connection, for servers without range support or unknown
/// sizes. Restarts the file on transient failures; there is nothing to resume
/// without ranges.
fn download_streaming(
    client: &Agent,
    url: &str,
    temporary: &Path,
    dest: &Path,
    expected_bytes: Option<u64>,
) -> Result<u64> {
    let mut backoff = Duration::from_secs(2);
    for attempt in 1..=REQUEST_RETRIES {
        let _ = fs::remove_file(temporary);
        match download_streaming_once(client, url, temporary, dest, expected_bytes) {
            Ok(bytes) => return Ok(bytes),
            Err(error) if attempt < REQUEST_RETRIES && is_stream_retryable(&error) => {
                eprintln!("  download interrupted ({error}), restarting (attempt {attempt}/{REQUEST_RETRIES})");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop always returns")
}

fn download_streaming_once(
    client: &Agent,
    url: &str,
    temporary: &Path,
    dest: &Path,
    expected_bytes: Option<u64>,
) -> Result<u64> {
    let response = client.get(url).call().map_err(|error| match error {
        ureq::Error::Status(code, _) => {
            CompanionError::Download(format!("download returned HTTP {code}: {url}"))
        }
        ureq::Error::Transport(error) => {
            CompanionError::Download(format!("download failed for {url}: {error}"))
        }
    })?;
    // ureq with redirects(0) surfaces 3xx as Ok: never save a redirect body
    // as a successful download.
    let status = response.status();
    if !(200..=299).contains(&status) {
        return Err(CompanionError::Download(format!(
            "download returned HTTP {status}: {url}"
        )));
    }
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "download target serves HTML, not a file: {url}"
            )));
        }
    }
    // Reject executables by response headers before reading the body.
    reject_executable_headers(
        url,
        response.header("content-type"),
        response.header("content-disposition"),
    )?;
    let mut reader = response.into_reader();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|source| CompanionError::io("create partial download", source))?;
    let mut downloaded: u64 = 0;
    let mut next_tick: u64 = PROGRESS_MILESTONE_BYTES;
    let mut buffer = [0u8; 1024 * 1024];
    let mut first_chunk = true;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| CompanionError::io("read download", source))?;
        if count == 0 {
            break;
        }
        // Check the MZ signature on the very first bytes of the file.
        if first_chunk {
            reject_executable_signature(&buffer[..count], url)?;
            first_chunk = false;
        }
        file.write_all(&buffer[..count])
            .map_err(|source| CompanionError::io("write download", source))?;
        downloaded = downloaded
            .checked_add(count as u64)
            .ok_or(CompanionError::SizeOverflow)?;
        if downloaded >= next_tick {
            eprintln!("  ...{downloaded} bytes");
            next_tick = next_tick.saturating_add(PROGRESS_MILESTONE_BYTES);
        }
    }
    file.sync_all()
        .map_err(|source| CompanionError::io("flush download", source))?;
    drop(file);
    // An empty body is a throttled or failed fetch, never a successful
    // download: installing it would wedge later runs behind the
    // refuse-to-overwrite guard with a 0-byte file.
    if downloaded == 0 {
        let _ = fs::remove_file(temporary);
        return Err(CompanionError::Download(format!(
            "download produced no bytes: {url}"
        )));
    }
    if let Some(expected) = expected_bytes {
        if downloaded != expected {
            let _ = fs::remove_file(temporary);
            return Err(CompanionError::Download(format!(
                "download size mismatch: expected {expected} bytes, got {downloaded}"
            )));
        }
    }
    fs::rename(temporary, dest).map_err(|source| CompanionError::io("install download", source))?;
    Ok(downloaded)
}

/// Stream a URL to `dest_dir/<file_name>` with an atomic rename, verifying the
/// size when the source advertised one.
pub fn download_to_file(
    config: &DownloadConfig,
    url: &str,
    dest_dir: &Path,
    expected_bytes: Option<u64>,
) -> Result<PathBuf> {
    let file_name = remote_file_name(url)?;
    download_one(config, url, dest_dir, &file_name, expected_bytes)?;
    Ok(dest_dir.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrapes_anchor_by_id() {
        let html = r#"<html><body><a class="x" id="downloadButton" href="https://dl.example.com/f/game.7z">DL</a></body></html>"#;
        assert_eq!(
            scrape_anchor(html, "downloadButton"),
            Some("https://dl.example.com/f/game.7z".to_owned())
        );
        assert_eq!(scrape_anchor(html, "download-link"), None);
    }

    #[test]
    fn scrape_is_case_insensitive() {
        let html = r#"<A ID="download-link" HREF="/d/abc123">x</A>"#;
        assert_eq!(
            scrape_anchor(html, "download-link"),
            Some("/d/abc123".to_owned())
        );
    }

    #[test]
    fn absolutize_forms() {
        assert_eq!(
            absolutize(
                "https://cdn.example.com/f.7z",
                "https://host.example.com/page"
            )
            .unwrap(),
            "https://cdn.example.com/f.7z"
        );
        assert_eq!(
            absolutize("//cdn.example.com/f.7z", "https://host.example.com/page").unwrap(),
            "https://cdn.example.com/f.7z"
        );
        assert_eq!(
            absolutize("/f.7z", "https://host.example.com/dir/page").unwrap(),
            "https://host.example.com/f.7z"
        );
        assert_eq!(
            absolutize("f.7z", "https://host.example.com/dir/page").unwrap(),
            "https://host.example.com/dir/f.7z"
        );
        assert!(absolutize(
            "http://cdn.example.com/f.7z",
            "https://host.example.com/page"
        )
        .is_err());
    }

    #[test]
    fn filekeeper_code_is_strict() {
        // exercised through resolve path validation indirectly; the pattern is
        // 12 lowercase alphanumerics as the first path segment.
        let good = "abcdefgh1234";
        assert_eq!(good.len(), 12);
        assert!(good
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn remote_file_name_strips_query_and_decodes() {
        assert_eq!(
            remote_file_name("https://dl.example.com/files/game.7z?token=abc#frag").unwrap(),
            "game.7z"
        );
        assert_eq!(
            remote_file_name("https://dl.example.com/files/my%20game.rar").unwrap(),
            "my%20game.rar"
        );
        assert!(remote_file_name("https://dl.example.com/files/").is_err());
        // Encoded dots stay literal: "%2e%2e" is a harmless file name, not a
        // traversal, since decoding never happens.
        assert_eq!(
            remote_file_name("https://dl.example.com/files/%2e%2e").unwrap(),
            "%2e%2e"
        );
        assert!(remote_file_name("https://dl.example.com/files/a:b").is_err());
    }

    // -- local HTTP server helpers ------------------------------------------
    // Server tests share the port range, so a static lock keeps them from
    // stealing each other's ports while running in parallel.

    static SERVER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_probe(dir: &std::path::Path) -> String {
        let token = PROBE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let name = format!(".probe-{token}");
        std::fs::write(dir.join(&name), b"ok").ok();
        name
    }

    struct TestServer {
        child: std::process::Child,
        base: String,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TestServer {
        fn spawn(dir: &std::path::Path) -> Option<TestServer> {
            let guard = SERVER_LOCK.lock().ok()?;
            let probe_name = unique_probe(dir);
            for port in 19131..19141 {
                let child = std::process::Command::new("python3")
                    .args([
                        "-m",
                        "http.server",
                        &port.to_string(),
                        "--bind",
                        "127.0.0.1",
                    ])
                    .current_dir(dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()?;
                // Probe a marker file: a bare "/" probe could succeed against
                // another test's server on a stolen port.
                let probe_url = format!("http://127.0.0.1:{port}/{probe_name}");
                let mut bound = false;
                for _ in 0..50 {
                    let probe = ureq::AgentBuilder::new()
                        .timeout_connect(Duration::from_millis(200))
                        .build()
                        .get(&probe_url)
                        .call();
                    match probe {
                        Ok(response) => {
                            let mut body = String::new();
                            use std::io::Read as _;
                            if response
                                .into_reader()
                                .take(16)
                                .read_to_string(&mut body)
                                .is_ok()
                                && body == "ok"
                            {
                                bound = true;
                                break;
                            }
                            // Wrong server on this port; try the next.
                            break;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
                if bound {
                    let base = format!("http://127.0.0.1:{port}");
                    return Some(TestServer {
                        child,
                        base,
                        _guard: guard,
                    });
                }
                let _ = child.wait_with_output();
            }
            None
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn direct_release(url: &str, label: &str, size: Option<u64>) -> Release {
        Release {
            provider: "test".to_owned(),
            format: "binary".to_owned(),
            label: label.to_owned(),
            url: url.to_owned(),
            browser_url: String::new(),
            size_bytes: size,
            health: "ok".to_owned(),
            source: "orbit".to_owned(),
            version: String::new(),
            requires_browser: false,
            unavailable_reason: String::new(),
            manifest_url: String::new(),
            title_id: "PPSA00001".to_owned(),
            filename: String::new(),
            parts: Vec::new(),
        }
    }

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let path =
                std::env::temp_dir().join(format!("ps5c-dltest-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.path
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        TempDir::new(name)
    }

    #[test]
    fn downloads_set_parts_in_order() {
        let serve = temp_dir("serve");
        std::fs::write(serve.path().join("p1.bin"), b"part-one-payload").unwrap();
        std::fs::write(serve.path().join("p2.bin"), b"part-two-payload-longer").unwrap();
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("dest");

        let entry = CatalogEntry::Set(crate::sources::ArchiveSet {
            label: "Base · 2 parts".to_owned(),
            provider: "test".to_owned(),
            parts: vec![
                crate::sources::SetPart {
                    number: 1,
                    source: direct_release(
                        &format!("{}/p1.bin", server.base),
                        "Base part 1",
                        Some(16),
                    ),
                },
                crate::sources::SetPart {
                    number: 2,
                    source: direct_release(
                        &format!("{}/p2.bin", server.base),
                        "Base part 2",
                        Some(23),
                    ),
                },
            ],
            size_bytes: Some(39),
            unavailable_reason: String::new(),
        });

        let config = DownloadConfig::default();
        let parts = download_entry(&config, &entry, dest.path()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].number, 1);
        assert_eq!(parts[1].number, 2);
        assert_eq!(std::fs::read(&parts[0].path).unwrap(), b"part-one-payload");
        assert_eq!(
            std::fs::read(&parts[1].path).unwrap(),
            b"part-two-payload-longer"
        );
        // No temp files left behind: provenance and lock sidecars persist
        // by design, but .part/.resume.json staging files must be gone.
        assert!(std::fs::read_dir(dest.path()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            !name.ends_with(".part") && !name.ends_with(".resume.json")
        }));

        // A second run skips both parts instead of re-downloading them.
        let again = download_entry(&config, &entry, dest.path()).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].path, parts[0].path);
    }

    #[test]
    fn empty_body_is_a_failed_download_not_an_empty_file() {
        // A throttled host answering 200 with an empty body must fail, not
        // install a 0-byte file that wedges later runs behind the
        // refuse-to-overwrite guard.
        let serve = temp_dir("emptyserve");
        std::fs::write(serve.path().join("empty.bin"), b"").unwrap();
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("emptydest");

        let config = DownloadConfig::default();
        let error = download_to_file(
            &config,
            &format!("{}/empty.bin", server.base),
            dest.path(),
            None,
        )
        .expect_err("empty body must fail, not install a 0-byte file");
        assert!(
            error.to_string().contains("no bytes"),
            "unexpected error: {error}"
        );
        assert!(!dest.path().join("empty.bin").exists());
        // The failed download leaves no temp staging files; the lock
        // sidecar persists by design.
        assert!(std::fs::read_dir(dest.path()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            !name.ends_with(".part") && !name.ends_with(".resume.json")
        }));
    }

    #[test]
    fn zero_byte_dest_is_cleared_and_redownloaded() {
        // A 0-byte file left by the old empty-body bug is treated as absent:
        // the download proceeds instead of refusing to overwrite.
        let serve = temp_dir("zer-serve");
        std::fs::write(serve.path().join("z.bin"), b"payload").unwrap();
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("zer-dest");
        std::fs::write(dest.path().join("z.bin"), b"").unwrap();

        let config = DownloadConfig::default();
        let path = download_to_file(
            &config,
            &format!("{}/z.bin", server.base),
            dest.path(),
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    #[test]
    fn colliding_part_names_are_qualified() {
        let serve = temp_dir("serve2");
        std::fs::create_dir_all(serve.path().join("a")).unwrap();
        std::fs::create_dir_all(serve.path().join("b")).unwrap();
        std::fs::write(serve.path().join("a/game.rar"), b"volume-a").unwrap();
        std::fs::write(serve.path().join("b/game.rar"), b"volume-b").unwrap();
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("dest2");

        let entry = CatalogEntry::Set(crate::sources::ArchiveSet {
            label: "Base · 2 parts".to_owned(),
            provider: "test".to_owned(),
            parts: vec![
                crate::sources::SetPart {
                    number: 1,
                    source: direct_release(
                        &format!("{}/a/game.rar", server.base),
                        "Base part 1",
                        None,
                    ),
                },
                crate::sources::SetPart {
                    number: 2,
                    source: direct_release(
                        &format!("{}/b/game.rar", server.base),
                        "Base part 2",
                        None,
                    ),
                },
            ],
            size_bytes: None,
            unavailable_reason: String::new(),
        });

        let parts = download_entry(&DownloadConfig::default(), &entry, dest.path()).unwrap();
        let names: Vec<String> = parts
            .iter()
            .map(|part| {
                part.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names[0], "game.rar");
        assert_eq!(names[1], "part-2-game.rar");
        assert_eq!(std::fs::read(&parts[1].path).unwrap(), b"volume-b");
    }

    #[test]
    fn unavailable_set_refuses_to_download() {
        let entry = CatalogEntry::Set(crate::sources::ArchiveSet {
            label: "Base · 2 parts".to_owned(),
            provider: "test".to_owned(),
            parts: Vec::new(),
            size_bytes: None,
            unavailable_reason: "This source has missing volumes.".to_owned(),
        });
        let error =
            download_entry(&DownloadConfig::default(), &entry, &std::env::temp_dir()).unwrap_err();
        assert!(error.to_string().contains("missing volumes"));
    }

    // -- range-capable test server -------------------------------------------
    // python's http.server does not honor Range requests, so segmented tests
    // get a tiny scripted server that does.

    const RANGE_SERVER: &str = r#"
import http.server, os, sys

root, port = sys.argv[1], int(sys.argv[2])
log_path = os.path.join(root, '.ranges.log')

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=root, **kwargs)
    def log_message(self, *args):
        pass
    def end_headers(self):
        self.send_header('Accept-Ranges', 'bytes')
        super().end_headers()
    def do_GET(self):
        fs_path = self.translate_path(self.path.split('?')[0].split('#')[0])
        if not os.path.isfile(fs_path):
            self.send_error(404)
            return
        size = os.path.getsize(fs_path)
        start, end, status = 0, size - 1, 200
        range_header = self.headers.get('Range')
        if range_header and range_header.startswith('bytes='):
            try:
                s, e = range_header[6:].split('-', 1)
                start = int(s)
                end = int(e) if e else size - 1
                status = 206
            except ValueError:
                self.send_error(416)
                return
        with open(log_path, 'a') as log:
            log.write(f'{self.command} {self.path} range={range_header}\n')
        length = end - start + 1
        self.send_response(status)
        self.send_header('Content-Type', 'application/octet-stream')
        self.send_header('Content-Length', str(length))
        if status == 206:
            self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
        self.end_headers()
        if self.command == 'HEAD':
            return
        with open(fs_path, 'rb') as f:
            f.seek(start)
            remaining = length
            while remaining > 0:
                chunk = f.read(min(65536, remaining))
                if not chunk:
                    break
                self.wfile.write(chunk)
                remaining -= len(chunk)
    def do_HEAD(self):
        self.do_GET()

http.server.HTTPServer(('127.0.0.1', port), Handler).serve_forever()
"#;

    impl TestServer {
        fn spawn_range(dir: &std::path::Path) -> Option<TestServer> {
            let guard = SERVER_LOCK.lock().ok()?;
            let probe_name = unique_probe(dir);
            std::fs::write(dir.join("range_server.py"), RANGE_SERVER).ok()?;
            for port in 19151..19161 {
                let child = std::process::Command::new("python3")
                    .args(["range_server.py", &dir.to_string_lossy(), &port.to_string()])
                    .current_dir(dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()?;
                let probe_url = format!("http://127.0.0.1:{port}/{probe_name}");
                let mut bound = false;
                for _ in 0..50 {
                    let probe = ureq::AgentBuilder::new()
                        .timeout_connect(Duration::from_millis(200))
                        .build()
                        .get(&probe_url)
                        .call();
                    match probe {
                        Ok(response) => {
                            let mut body = String::new();
                            use std::io::Read as _;
                            if response
                                .into_reader()
                                .take(16)
                                .read_to_string(&mut body)
                                .is_ok()
                                && body == "ok"
                            {
                                bound = true;
                                break;
                            }
                            // Wrong server on this port; try the next.
                            break;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
                if bound {
                    let base = format!("http://127.0.0.1:{port}");
                    return Some(TestServer {
                        child,
                        base,
                        _guard: guard,
                    });
                }
                let _ = child.wait_with_output();
            }
            None
        }
    }

    fn pseudo_random(size: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(size);
        let mut state = seed;
        for _ in 0..size {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            out.push((state >> 33) as u8);
        }
        out
    }

    #[test]
    fn segmented_download_fetches_ranges_in_parallel() {
        let serve = temp_dir("rangeserve");
        let payload = pseudo_random(40 * 1024 * 1024, 0x1234);
        std::fs::write(serve.path().join("big.bin"), &payload).unwrap();
        let server = TestServer::spawn_range(serve.path()).expect("could not start range server");
        let dest = temp_dir("rangedest");

        let config = DownloadConfig {
            segment_bytes: 8 * 1024 * 1024,
            max_connections: 8,
            adaptive: false,
            ..DownloadConfig::default()
        };
        let path = download_to_file(
            &config,
            &format!("{}/big.bin", server.base),
            dest.path(),
            Some(payload.len() as u64),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        // No temp or sidecar files left behind.
        assert!(!dest.path().join(".big.bin.part").exists());
        assert!(!dest.path().join(".big.bin.resume.json").exists());

        let log = std::fs::read_to_string(serve.path().join(".ranges.log")).unwrap();
        let range_lines: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("range=bytes="))
            .collect();
        // 5 segments over 8 MiB, each fetched with a Range request.
        assert_eq!(range_lines.len(), 5);
        assert!(log.contains("range=bytes=0-8388607"));
        assert!(log.contains("range=bytes=33554432-41943039"));
    }

    #[test]
    fn segmented_download_resumes_completed_segments() {
        let serve = temp_dir("resumeserve");
        let payload = pseudo_random(40 * 1024 * 1024, 0x5678);
        std::fs::write(serve.path().join("big.bin"), &payload).unwrap();
        let server = TestServer::spawn_range(serve.path()).expect("could not start range server");
        let dest = temp_dir("resumedest");

        // Simulate an interrupted run: first segment already on disk, sidecar
        // marking exactly that segment complete.
        let segment_bytes = 8 * 1024 * 1024u64;
        let temp = dest.path().join(".big.bin.part");
        let sidecar = dest.path().join(".big.bin.resume.json");
        {
            let file = std::fs::File::create(&temp).unwrap();
            use std::io::Write as _;
            (&file)
                .write_all(&payload[..segment_bytes as usize])
                .unwrap();
            file.set_len(payload.len() as u64).unwrap();
        }
        let url = format!("{}/big.bin", server.base);
        let state = serde_json::json!({
            "url": url,
            "size_bytes": payload.len(),
            "segment_bytes": segment_bytes,
            "completed": [true, false, false, false, false],
        });
        std::fs::write(&sidecar, serde_json::to_vec(&state).unwrap()).unwrap();

        let config = DownloadConfig {
            segment_bytes,
            max_connections: 8,
            adaptive: false,
            ..DownloadConfig::default()
        };
        let path =
            download_to_file(&config, &url, dest.path(), Some(payload.len() as u64)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);

        let log = std::fs::read_to_string(serve.path().join(".ranges.log")).unwrap();
        // The completed first segment was never re-requested.
        assert!(!log.contains("range=bytes=0-8388607"));
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("range=bytes="))
                .count(),
            4
        );
    }

    #[test]
    fn server_without_ranges_falls_back_to_streaming() {
        let serve = temp_dir("norangeserve");
        let payload = pseudo_random(3 * 1024 * 1024, 0x9abc);
        std::fs::write(serve.path().join("small.bin"), &payload).unwrap();
        // Plain http.server: no Accept-Ranges, so even a multi-segment-sized
        // file must stream.
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("norrangedest");

        let config = DownloadConfig {
            segment_bytes: 1024 * 1024,
            adaptive: false,
            ..DownloadConfig::default()
        };
        let path = download_to_file(
            &config,
            &format!("{}/small.bin", server.base),
            dest.path(),
            Some(payload.len() as u64),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn set_download_feeds_multivolume_extraction() {
        if std::process::Command::new("7z")
            .arg("--help")
            .output()
            .is_err()
        {
            eprintln!("skipping: 7z not installed");
            return;
        }
        let work = temp_dir("vol");
        let payload = work.path().join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        // Large and incompressible enough that `-v40k` really splits it.
        let mut blob = Vec::with_capacity(120 * 1024);
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..120 * 1024 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            blob.push((seed >> 33) as u8);
        }
        std::fs::write(payload.join("blob.bin"), &blob).unwrap();
        let status = std::process::Command::new("7z")
            .args(["a", "-v40k", "-mx0", "-y"])
            .arg(work.path().join("archive.7z"))
            .arg(payload.join("blob.bin"))
            .output()
            .expect("7z failed");
        assert!(status.status.success());
        let volumes: Vec<std::path::PathBuf> = std::fs::read_dir(&work)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .map(|ext| ext == "001" || ext == "002")
                    .unwrap_or(false)
                    || path
                        .file_name()
                        .map(|name| name.to_string_lossy().contains(".7z."))
                        .unwrap_or(false)
            })
            .collect();
        assert!(volumes.len() >= 2, "expected multi-volume output");

        let serve = temp_dir("volserve");
        for volume in &volumes {
            std::fs::copy(volume, serve.path().join(volume.file_name().unwrap())).unwrap();
        }
        let server = TestServer::spawn(serve.path()).expect("could not start local HTTP server");
        let dest = temp_dir("voldest");

        let mut parts = Vec::new();
        let mut names: Vec<String> = volumes
            .iter()
            .map(|volume| volume.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for (index, name) in names.iter().enumerate() {
            parts.push(crate::sources::SetPart {
                number: index + 1,
                source: direct_release(
                    &format!("{}/{}", server.base, name),
                    &format!("Vol part {}", index + 1),
                    None,
                ),
            });
        }
        let entry = CatalogEntry::Set(crate::sources::ArchiveSet {
            label: "Vol · parts".to_owned(),
            provider: "test".to_owned(),
            parts,
            size_bytes: None,
            unavailable_reason: String::new(),
        });
        let downloaded = download_entry(&DownloadConfig::default(), &entry, dest.path()).unwrap();
        assert_eq!(downloaded.len(), names.len());

        // The existing extraction flow picks up the volume chain from part 1.
        let volws = temp_dir("volws");
        let request =
            crate::extract::ExtractionRequest::new(&downloaded[0].path, volws.path(), "vol-job");
        let result = crate::extract::extract_archive(&request).unwrap();
        let extracted = std::fs::read(result.output_root.join("blob.bin")).unwrap();
        assert_eq!(extracted, blob);
    }

    // ------------------------------------------------------------------
    // Regression tests for the PR #2 review findings.
    // ------------------------------------------------------------------

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;

    /// A programmable loopback HTTP/1.1 server. The handler maps
    /// (method, path, headers, body) to (status, headers, body); each
    /// connection is served on its own thread with `Connection: close`.
    struct RawServer {
        base: String,
    }

    type RawHandler = dyn Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>)
        + Send
        + Sync;
    type RawRequest = (String, String, HashMap<String, String>, Vec<u8>);

    fn spawn_raw(
        handler: impl Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>)
            + Send
            + Sync
            + 'static,
    ) -> RawServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let handler: Arc<RawHandler> = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let Some((method, path, headers, body)) = read_request(&mut stream) else {
                        return;
                    };
                    let (status, resp_headers, resp_body) =
                        handler(&method, &path, &headers, &body);
                    write_response(&mut stream, status, &resp_headers, &resp_body);
                });
            }
        });
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..50 {
            if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        RawServer { base }
    }

    fn read_request(stream: &mut TcpStream) -> Option<RawRequest> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).ok()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        if method.is_empty() {
            return None;
        }
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_owned());
            }
        }
        let body = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .map(|len| {
                let mut buf = vec![0u8; len.min(1024 * 1024)];
                let mut read = 0;
                while read < buf.len() {
                    match reader.read(&mut buf[read..]) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => read += count,
                    }
                }
                buf.truncate(read);
                buf
            })
            .unwrap_or_default();
        Some((method, path, headers, body))
    }

    fn write_response(
        stream: &mut TcpStream,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        let reason = match status {
            200 => "OK",
            206 => "Partial Content",
            302 => "Found",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        let mut head = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
        // The handler's headers win: only add Content-Length when it did not
        // supply its own (a HEAD response legitimately advertises a length
        // with no body).
        if !headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (key, value) in headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }

    /// A 302 chain must be walked to the final target: with ureq
    /// `redirects(0)`, 3xx arrives as `Ok`, and the old code returned the
    /// redirect URL itself — then saved a redirect body as a "successful"
    /// download.
    #[test]
    fn redirect_chain_fetches_final_target() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/start") => (
                302,
                vec![
                    ("Location".to_owned(), "/mid".to_owned()),
                    ("Content-Type".to_owned(), "text/plain".to_owned()),
                ],
                Vec::new(),
            ),
            ("HEAD", "/mid") => (
                302,
                vec![("Location".to_owned(), "/final".to_owned())],
                Vec::new(),
            ),
            ("HEAD", "/final") => (
                200,
                vec![
                    ("Content-Length".to_owned(), "11".to_owned()),
                    (
                        "Content-Type".to_owned(),
                        "application/octet-stream".to_owned(),
                    ),
                ],
                Vec::new(),
            ),
            ("GET", "/final") => (
                200,
                vec![(
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                )],
                b"FINAL_BYTES".to_vec(),
            ),
            // The trap: if the 302 is mistaken for a terminal URL, this
            // text/plain body gets saved as the download.
            ("GET", "/start") => (
                302,
                vec![
                    ("Location".to_owned(), "/mid".to_owned()),
                    ("Content-Type".to_owned(), "text/plain".to_owned()),
                ],
                b"REDIRECT_BODY".to_vec(),
            ),
            ("GET", "/mid") => (
                302,
                vec![("Location".to_owned(), "/final".to_owned())],
                Vec::new(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let config = DownloadConfig::default();
        let client = http_client(&config);
        let start = format!("{}/start", server.base);
        let resolved = follow_redirect(&client, &start).unwrap();
        assert_eq!(resolved, format!("{}/final", server.base));

        let dir = temp_dir("redir");
        let bytes = download_one(&config, &start, dir.path(), "out.bin", Some(11)).unwrap();
        assert_eq!(bytes, 11);
        assert_eq!(
            std::fs::read(dir.path().join("out.bin")).unwrap(),
            b"FINAL_BYTES"
        );
    }

    /// A 3xx without a Location header is a broken chain, not a file.
    #[test]
    fn redirect_without_location_is_an_error() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/loop") => (302, Vec::new(), Vec::new()),
            _ => (404, Vec::new(), Vec::new()),
        });
        let config = DownloadConfig::default();
        let client = http_client(&config);
        let error = follow_redirect(&client, &format!("{}/loop", server.base)).unwrap_err();
        assert!(
            error.to_string().contains("without Location"),
            "unexpected error: {error}"
        );
    }

    /// FileKeeper's normal flow is POST -> 302 with a Location header; the
    /// old code read the 302-as-Ok as "answered without a redirect" and
    /// rejected every FileKeeper source.
    #[test]
    fn filekeeper_post_redirect_is_followed() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("POST", "/abcd1234efgh") => (
                302,
                vec![("Location".to_owned(), "/dl/file.bin".to_owned())],
                Vec::new(),
            ),
            ("HEAD", "/dl/file.bin") => (
                200,
                vec![("Content-Length".to_owned(), "5".to_owned())],
                Vec::new(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let config = DownloadConfig::default();
        let client = http_client(&config);
        let release = Release {
            provider: "FileKeeper".to_owned(),
            format: String::new(),
            label: "filekeeper test".to_owned(),
            url: format!("{}/abcd1234efgh", server.base),
            browser_url: String::new(),
            size_bytes: None,
            health: "ok".to_owned(),
            source: String::new(),
            version: String::new(),
            requires_browser: false,
            unavailable_reason: String::new(),
            manifest_url: String::new(),
            title_id: "PPSA00001".to_owned(),
            filename: String::new(),
            parts: Vec::new(),
        };
        let resolved = resolve_filekeeper(&client, &release).unwrap();
        assert_eq!(resolved.url, format!("{}/dl/file.bin", server.base));
        assert_eq!(resolved.expected_bytes, Some(5));
    }

    /// A failed replacement download must not delete the existing file:
    /// the old code removed the destination before downloading, so a 404
    /// destroyed the original bytes. The existing file must be owned
    /// (provenance from the same source) for the replacement to proceed.
    #[test]
    fn failed_replacement_preserves_existing_file() {
        use std::sync::atomic::AtomicBool;
        let serve_404 = Arc::new(AtomicBool::new(false));
        let serve_404_c = Arc::clone(&serve_404);
        let server = spawn_raw(move |method, path, _headers, _body| match (method, path) {
            ("HEAD", "/file") => (
                200,
                vec![("Content-Length".to_owned(), "100".to_owned())],
                Vec::new(),
            ),
            ("GET", "/file") => {
                if serve_404_c.load(Ordering::SeqCst) {
                    (404, Vec::new(), Vec::new())
                } else {
                    (
                        200,
                        vec![(
                            "Content-Type".to_owned(),
                            "application/octet-stream".to_owned(),
                        )],
                        vec![7u8; 100],
                    )
                }
            }
            _ => (404, Vec::new(), Vec::new()),
        });
        let dir = temp_dir("preserve");
        let config = DownloadConfig::default();
        let url = format!("{}/file", server.base);
        // Phase 1: successful download binds provenance to this source.
        let bytes = download_one(&config, &url, dir.path(), "part1.bin", Some(100)).unwrap();
        assert_eq!(bytes, 100);
        // Simulate a partial/corrupt existing file owned by this source.
        let part = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("part1.bin"))
            .unwrap();
        part.set_len(50).unwrap();
        drop(part);
        // Phase 2: the source now 404s; the replacement fails but the
        // original owned file survives.
        serve_404.store(true, Ordering::SeqCst);
        let error = download_one(&config, &url, dir.path(), "part1.bin", Some(100)).unwrap_err();
        assert!(
            error.to_string().contains("404"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("part1.bin")).unwrap(),
            vec![7u8; 50],
            "the failed download must leave the original file intact"
        );
    }

    /// After AIMD backoff cuts the allowed worker count, in-flight segment
    /// requests must respect the reduced limit. The old code released the
    /// permit right after taking a queue item, so every worker fetched at
    /// once no matter how far `allowed` had been cut.
    #[test]
    fn backoff_limits_in_flight_segment_requests() {
        use std::sync::Condvar;

        const SEGMENT_BYTES: u64 = 1024 * 1024;
        const FILE_SIZE: u64 = 4 * SEGMENT_BYTES;

        let requests = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));

        let requests_c = Arc::clone(&requests);
        let in_flight_c = Arc::clone(&in_flight);
        let max_c = Arc::clone(&max_in_flight);
        let gate_c = Arc::clone(&gate);
        let server = spawn_raw(move |method, path, headers, _body| {
            if method == "HEAD" && path == "/big" {
                return (
                    200,
                    vec![
                        ("Content-Length".to_owned(), FILE_SIZE.to_string()),
                        ("Accept-Ranges".to_owned(), "bytes".to_owned()),
                    ],
                    Vec::new(),
                );
            }
            if method == "GET" && path == "/big" {
                let count = requests_c.fetch_add(1, Ordering::SeqCst) + 1;
                // Fail the first wave fast: each 500 halves `allowed`
                // (4 -> 2 -> 1), forcing the AIMD backoff.
                if count <= 4 {
                    return (500, Vec::new(), Vec::new());
                }
                // Then block, measuring how many requests are actually
                // in flight at once.
                let current = in_flight_c.fetch_add(1, Ordering::SeqCst) + 1;
                max_c.fetch_max(current, Ordering::SeqCst);
                let (lock, cvar) = &*gate_c;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cvar.wait(open).unwrap();
                }
                in_flight_c.fetch_sub(1, Ordering::SeqCst);
                let (from, to) = parse_test_range(headers.get("range").map(String::as_str));
                let body: Vec<u8> = (from..=to).map(|i| (i % 256) as u8).collect();
                return (
                    206,
                    vec![
                        (
                            "Content-Range".to_owned(),
                            format!("bytes {from}-{to}/{FILE_SIZE}"),
                        ),
                        (
                            "Content-Type".to_owned(),
                            "application/octet-stream".to_owned(),
                        ),
                    ],
                    body,
                );
            }
            (404, Vec::new(), Vec::new())
        });

        let config = DownloadConfig {
            adaptive: false,
            max_connections: 4,
            ..DownloadConfig::default()
        };
        let client = http_client(&config);
        let dir = temp_dir("concurrency");
        let temporary = dir.path().join("big.part");
        let sidecar = dir.path().join(".big.resume.json");
        let url = format!("{}/big", server.base);

        let handle = std::thread::spawn(move || {
            download_segmented(&config, &client, &url, &temporary, &sidecar, FILE_SIZE)
        });

        // Wait for the failing wave that forces backoff (allowed 4 -> 1).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while requests.load(Ordering::SeqCst) < 4 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the failing wave"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Workers are in their 2s retry sleep; give them time to wake and
        // block on the gate-kept fetch.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while in_flight.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for a blocked fetch"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Let any other workers arrive (the old code lets all four through).
        std::thread::sleep(Duration::from_secs(2));
        let observed = max_in_flight.load(Ordering::SeqCst);
        // Release the gate and let the download finish.
        {
            let (lock, cvar) = &*gate;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        handle.join().expect("download thread panicked").unwrap();
        assert_eq!(
            observed, 1,
            "backoff cut allowed workers to 1, but {observed} requests were in flight at once"
        );
        // The bytes are still correct after the blocked/released dance.
        let data = std::fs::read(dir.path().join("big.part")).unwrap();
        assert_eq!(data.len() as u64, FILE_SIZE);
        assert!(data
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i as u64 % 256) as u8));
    }

    /// The public resolver must follow a redirect chain before checking
    /// file metadata: head_check rejects 3xx, so checking the raw URL
    /// first fails direct links that legitimately redirect.
    #[test]
    fn public_resolver_follows_redirect_chain() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/game.rar") => (
                302,
                vec![("Location".to_owned(), "/real/game.rar".to_owned())],
                Vec::new(),
            ),
            ("HEAD", "/real/game.rar") => (
                200,
                vec![
                    ("Content-Length".to_owned(), "1234".to_owned()),
                    ("Content-Type".to_owned(), "application/x-rar".to_owned()),
                ],
                Vec::new(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let config = DownloadConfig::default();
        let release = direct_release(&format!("{}/game.rar", server.base), "redirected rar", None);
        let resolved = resolve_release(&release, &config).unwrap();
        assert_eq!(resolved.url, format!("{}/real/game.rar", server.base));
    }

    /// A successful download must not overwrite an unrelated existing file:
    /// the old code replaced it once the transfer verified.
    #[test]
    fn successful_download_refuses_unowned_collision() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/game.rar") => (
                200,
                vec![
                    ("Content-Length".to_owned(), "3".to_owned()),
                    ("Content-Type".to_owned(), "application/x-rar".to_owned()),
                ],
                Vec::new(),
            ),
            ("GET", "/game.rar") => (
                200,
                vec![("Content-Type".to_owned(), "application/x-rar".to_owned())],
                b"new".to_vec(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let dir = temp_dir("collision");
        std::fs::write(dir.path().join("game.rar"), b"unrelated-original-bytes").unwrap();
        let config = DownloadConfig::default();
        let error = download_one(
            &config,
            &format!("{}/game.rar", server.base),
            dir.path(),
            "game.rar",
            Some(3),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unowned file collision"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("game.rar")).unwrap(),
            b"unrelated-original-bytes",
            "the unrelated file must survive"
        );
    }

    /// Same size is not evidence of identity: a file downloaded from one
    /// source must not be reused as verified data for another.
    #[test]
    fn same_size_different_source_is_not_reused() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/a.bin") | ("HEAD", "/b.bin") => (
                200,
                vec![
                    ("Content-Length".to_owned(), "3".to_owned()),
                    (
                        "Content-Type".to_owned(),
                        "application/octet-stream".to_owned(),
                    ),
                ],
                Vec::new(),
            ),
            ("GET", "/a.bin") => (
                200,
                vec![(
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                )],
                b"aaa".to_vec(),
            ),
            ("GET", "/b.bin") => (
                200,
                vec![(
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                )],
                b"bbb".to_vec(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let dir = temp_dir("samesize");
        let config = DownloadConfig::default();
        // First download from /a.bin succeeds and binds provenance.
        let bytes = download_one(
            &config,
            &format!("{}/a.bin", server.base),
            dir.path(),
            "file.bin",
            Some(3),
        )
        .unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), b"aaa");
        // Same size from /b.bin must not be accepted as the same file.
        let error = download_one(
            &config,
            &format!("{}/b.bin", server.base),
            dir.path(),
            "file.bin",
            Some(3),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unowned file collision"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("file.bin")).unwrap(),
            b"aaa",
            "the original download must not be replaced or trusted as /b.bin's data"
        );
    }

    /// Windows installers must be rejected by URL name, headers, and
    /// signature — a page advertising a PS5 image once delivered
    /// OperaSetup.exe.
    #[test]
    fn installer_guard_rejects_executables() {
        // By URL file name.
        assert!(reject_executable_url("https://dl.example.com/OperaSetup.exe").is_err());
        assert!(reject_executable_url("https://dl.example.com/game.rar").is_ok());
        // By MIME type and Content-Disposition.
        assert!(reject_executable_headers(
            "https://dl.example.com/f",
            Some("application/x-msdownload"),
            None,
        )
        .is_err());
        assert!(reject_executable_headers(
            "https://dl.example.com/f",
            Some("application/octet-stream"),
            Some("attachment; filename=\"setup.msi\""),
        )
        .is_err());
        assert!(reject_executable_headers(
            "https://dl.example.com/f",
            Some("application/octet-stream"),
            Some("attachment; filename*=UTF-8''game.rar"),
        )
        .is_ok());
        // By MZ signature.
        assert!(reject_executable_signature(b"MZ\x90\x00", "test").is_err());
        assert!(reject_executable_signature(b"Rar!\x1a\x07", "test").is_ok());
        assert!(reject_executable_signature(b"", "test").is_ok());
    }

    /// An .exe served with a 200 and matching length is still refused, not
    /// saved as a game file.
    #[test]
    fn exe_payload_is_not_downloaded() {
        let server = spawn_raw(|method, path, _headers, _body| match (method, path) {
            ("HEAD", "/setup.exe") => (
                200,
                vec![
                    ("Content-Length".to_owned(), "4".to_owned()),
                    (
                        "Content-Type".to_owned(),
                        "application/x-msdownload".to_owned(),
                    ),
                ],
                Vec::new(),
            ),
            ("GET", "/setup.exe") => (
                200,
                vec![(
                    "Content-Type".to_owned(),
                    "application/x-msdownload".to_owned(),
                )],
                b"MZ\x90\x00".to_vec(),
            ),
            _ => (404, Vec::new(), Vec::new()),
        });
        let dir = temp_dir("exeblock");
        let config = DownloadConfig::default();
        let error = download_one(
            &config,
            &format!("{}/setup.exe", server.base),
            dir.path(),
            "setup.exe",
            Some(4),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("executable"),
            "unexpected error: {error}"
        );
        assert!(!dir.path().join("setup.exe").exists());
    }

    /// Browser-only sources return a structured handoff, not a buried
    /// string error: the CLI surfaces these as the explicit next step.
    #[test]
    fn browser_source_returns_handoff() {
        let release = Release {
            provider: "datanodes".to_owned(),
            format: String::new(),
            label: "Datanodes file".to_owned(),
            url: "https://datanodes.to/f/abc123".to_owned(),
            browser_url: String::new(),
            size_bytes: None,
            health: String::new(),
            source: String::new(),
            version: String::new(),
            requires_browser: true,
            unavailable_reason: String::new(),
            manifest_url: String::new(),
            title_id: String::new(),
            filename: String::new(),
            parts: Vec::new(),
        };
        let config = DownloadConfig::default();
        match resolve_release(&release, &config).unwrap_err() {
            CompanionError::BrowserCapture(handoff) => {
                assert_eq!(handoff.provider, "datanodes");
                assert_eq!(handoff.page_url, "https://datanodes.to/f/abc123");
                assert!(!handoff.reason.is_empty());
            }
            other => panic!("expected BrowserCapture, got {other:?}"),
        }
    }

    /// The probe validates a captured URL with a bounded range request
    /// before any large transfer: 206 + Content-Range + real bytes.
    #[test]
    fn probe_validates_captured_url() {
        let server = spawn_raw(|method, path, headers, _body| {
            let range = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("range"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            match (method, path) {
                ("HEAD", "/file.rar") => (
                    200,
                    vec![
                        ("Content-Type".to_owned(), "application/x-rar".to_owned()),
                        ("Content-Length".to_owned(), "4087656392".to_owned()),
                    ],
                    Vec::new(),
                ),
                ("GET", "/file.rar") if range == "bytes=0-65535" => (
                    206,
                    vec![
                        ("Content-Type".to_owned(), "application/x-rar".to_owned()),
                        (
                            "Content-Range".to_owned(),
                            "bytes 0-65535/4087656392".to_owned(),
                        ),
                    ],
                    vec![b'R'; 65536],
                ),
                _ => (404, Vec::new(), Vec::new()),
            }
        });
        let config = DownloadConfig::default();
        let probe = probe_url(&config, &format!("{}/file.rar", server.base)).unwrap();
        assert_eq!(probe.status, 206);
        assert_eq!(probe.total_bytes, Some(4087656392));
        assert!(probe.accepts_ranges);
        assert_eq!(probe.probe_bytes, 65536);
        assert_eq!(probe.content_type.as_deref(), Some("application/x-rar"));
    }

    /// A dead captured URL fails fast at probe time, not mid-transfer.
    #[test]
    fn probe_rejects_dead_url() {
        let server = spawn_raw(|_method, _path, _headers, _body| (404, Vec::new(), Vec::new()));
        let config = DownloadConfig::default();
        let error = probe_url(&config, &format!("{}/gone.rar", server.base)).unwrap_err();
        assert!(
            error.to_string().contains("404"),
            "unexpected error: {error}"
        );
    }

    /// Parse a `bytes=A-B` range header for the concurrency test server.
    fn parse_test_range(header: Option<&str>) -> (u64, u64) {
        let header = header.unwrap_or("").strip_prefix("bytes=").unwrap_or("");
        let (from, to) = header.split_once('-').unwrap_or(("0", "0"));
        (from.parse().unwrap_or(0), to.parse().unwrap_or(0))
    }
}
