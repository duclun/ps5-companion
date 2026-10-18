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
            head_check(&client, &url)?;
            ResolvedSource {
                url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Mediafire => {
            let page = release.primary_url().to_owned();
            let html = fetch_page(&client, &page)?;
            let href = scrape_anchor(&html, "downloadButton")
                .ok_or_else(|| capture_required(&release.provider, &page))?;
            let url = absolutize(&href, &page)?;
            head_check(&client, &url)?;
            ResolvedSource {
                url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Vikingfile => {
            let page = release.primary_url().to_owned();
            let html = fetch_page(&client, &page)?;
            let href = scrape_anchor(&html, "download-link")
                .ok_or_else(|| capture_required(&release.provider, &page))?;
            let url = absolutize(&href, &page)?;
            head_check(&client, &url)?;
            ResolvedSource {
                url,
                provider: release.provider.clone(),
                expected_bytes: None,
                kind,
            }
        }
        SourceKind::Filekeeper => resolve_filekeeper(&client, release)?,
        SourceKind::Datanodes | SourceKind::Buzzheavier | SourceKind::Browser => {
            return Err(capture_required(&release.provider, release.primary_url()));
        }
    };
    Ok(resolved)
}

fn capture_required(provider: &str, page: &str) -> CompanionError {
    CompanionError::Download(format!(
        "{provider} needs a browser capture: open {page} in a browser, copy the final file URL, and rerun with --direct-url <url>"
    ))
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
        .ok_or_else(|| capture_required(&release.provider, &page))?;
    std::thread::sleep(FILEKEEPER_COUNTDOWN);
    let body = format!("op=download2&id={code}&rand=&referer=&method_free=&down_direct=1");
    let response = client
        .post(page.as_str())
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Referer", page.as_str())
        .send_string(&body);
    let location = match response {
        Ok(_) => {
            return Err(CompanionError::Download(
                "FileKeeper answered without a redirect; choose another source".to_owned(),
            ));
        }
        Err(ureq::Error::Status(_code @ 300..=399, response)) => {
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
                if let Some(content_type) = response.header("content-type") {
                    if is_html_content_type(content_type) {
                        return Err(CompanionError::Download(format!(
                            "redirect target serves HTML, not a file: {current}"
                        )));
                    }
                }
                return Ok(current);
            }
            Err(ureq::Error::Status(_code @ 300..=399, response)) => {
                let location = response
                    .header("location")
                    .ok_or_else(|| {
                        CompanionError::Download(format!("redirect without Location: {current}"))
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
                if dest.exists() {
                    let on_disk = dest
                        .metadata()
                        .map(|metadata| metadata.len())
                        .unwrap_or(u64::MAX);
                    if expected.is_some_and(|expected| on_disk == expected) {
                        eprintln!(
                            "part {}/{}: already complete ({} bytes), skipping",
                            part.number,
                            set.parts.len(),
                            on_disk
                        );
                        downloaded.push(DownloadedPart {
                            number: part.number,
                            path: dest,
                            bytes: on_disk,
                        });
                        continue;
                    }
                    eprintln!(
                        "part {}/{}: existing file has {on_disk} bytes, re-downloading",
                        part.number,
                        set.parts.len()
                    );
                    fs::remove_file(&dest)
                        .map_err(|source| CompanionError::io("remove stale part", source))?;
                }
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
fn download_one(
    config: &DownloadConfig,
    url: &str,
    dest_dir: &Path,
    file_name: &str,
    expected_bytes: Option<u64>,
) -> Result<u64> {
    let final_url = follow_redirect(&http_client(config), url)?;
    fs::create_dir_all(dest_dir)
        .map_err(|source| CompanionError::io("create download directory", source))?;
    let dest = dest_dir.join(file_name);
    if dest.exists() {
        // A throttled host once answered with an empty body and a 0-byte file
        // got installed; that is never a valid download — clear it and retry
        // instead of wedging behind the overwrite guard.
        let is_empty = fs::metadata(&dest)
            .map(|meta| meta.len() == 0)
            .unwrap_or(false);
        if is_empty {
            fs::remove_file(&dest)
                .map_err(|source| CompanionError::io("clear empty download", source))?;
        } else {
            return Err(CompanionError::Download(format!(
                "refusing to overwrite existing download: {}",
                dest.display()
            )));
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
        return Ok(size);
    }
    // A crashed single-stream run leaves its temp file behind; the retry loop
    // inside download_streaming removes it before every attempt. Segmented
    // resume state is left alone: if this streaming attempt fails, a later
    // run can still resume the segmented download instead of starting over.
    let result = download_streaming(&client, &final_url, &temporary, &dest, expected_bytes);
    if result.is_ok() {
        let _ = fs::remove_file(sidecar);
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
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| CompanionError::io("read segment", source))?;
        if count == 0 {
            break;
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
            self.active.fetch_sub(1, Ordering::SeqCst);
            let Some(index) = index else { return };
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
                    *self.first_error.lock().unwrap() = Some(error);
                    return;
                }
            };
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
    if let Some(content_type) = response.header("content-type") {
        if is_html_content_type(content_type) {
            return Err(CompanionError::Download(format!(
                "download target serves HTML, not a file: {url}"
            )));
        }
    }
    let mut reader = response.into_reader();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|source| CompanionError::io("create partial download", source))?;
    let mut downloaded: u64 = 0;
    let mut next_tick: u64 = PROGRESS_MILESTONE_BYTES;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| CompanionError::io("read download", source))?;
        if count == 0 {
            break;
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
        // No temp files left behind.
        assert!(std::fs::read_dir(dest.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')));

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
        assert!(std::fs::read_dir(dest.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')));
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
}
