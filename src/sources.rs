//! Bounded catalog and release-source lookup.
//!
//! This module reads the small metadata files used by Overdrive and turns them
//! into a reviewable source plan. It deliberately does not download archives or
//! contact any provider listed by a release record. Remote metadata is fetched
//! only from the pinned raw GitHub host with a small, non-redirecting client.

use crate::error::{CompanionError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// The default upstream repository containing the catalog metadata.
pub const DEFAULT_REPOSITORY: &str = "MyLuxy/overdrive";
/// The default upstream ref used for metadata lookups.
pub const DEFAULT_REFERENCE: &str = "main";
/// Maximum accepted catalog.js response size.
pub const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
/// Maximum accepted per-title release metadata response size.
pub const MAX_RELEASE_BYTES: usize = 8 * 1024 * 1024;
const RAW_GITHUB_HOST: &str = "raw.githubusercontent.com";
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

/// Selects either an offline checkout directory or the default raw GitHub
/// metadata source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Directory containing `catalog.js` and the `releases/` directory.
    pub catalog_dir: Option<PathBuf>,
    /// GitHub repository in `owner/repository` form.
    pub repository: String,
    /// Safe Git ref or commit-ish path component.
    pub reference: String,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            catalog_dir: None,
            repository: DEFAULT_REPOSITORY.to_owned(),
            reference: DEFAULT_REFERENCE.to_owned(),
        }
    }
}

/// A catalog entry with the fields needed by the CLI and source planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogGame {
    pub id: String,
    pub title: String,
    pub platform: String,
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default, alias = "sizeGb", alias = "size_gb")]
    pub size_gb: Option<f64>,
}

/// A single metadata record from a title's release JSON file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSource {
    pub id: String,
    pub provider: String,
    pub format: String,
    pub filename: String,
    pub url: Option<String>,
    #[serde(alias = "browserUrl")]
    pub browser_url: Option<String>,
    #[serde(alias = "manifestUrl")]
    pub manifest_url: Option<String>,
    #[serde(alias = "sizeBytes")]
    pub size_bytes: Option<u64>,
    pub version: Option<String>,
    pub label: Option<String>,
    #[serde(alias = "requiresBrowser")]
    pub requires_browser: bool,
    #[serde(alias = "archivePassword")]
    pub archive_password: Option<String>,
    pub health: Option<String>,
    #[serde(alias = "unavailableReason")]
    pub unavailable_reason: Option<String>,
    pub category: Option<String>,
    pub source: Option<String>,
    pub flow: Option<String>,
    pub record: Option<String>,
    pub volume: Option<String>,
}

impl ReleaseSource {
    /// Classify how a downloader may access this record without following any
    /// provider page automatically.
    pub fn access(&self) -> SourceAccess {
        classify_access(self)
    }
}

/// The handoff mode for a source record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceAccess {
    /// The record supplies a direct HTTPS object URL.
    Direct,
    /// A browser page or provider landing page must be opened by the user.
    BrowserRequired,
    /// The record supplies a manifest URL for a separate downloader flow.
    Manifest,
    /// The record is explicitly unavailable or contains no usable endpoint.
    Unavailable,
}

/// Public classification helper for callers that do not need the method form.
pub fn access(source: &ReleaseSource) -> SourceAccess {
    classify_access(source)
}

/// Alias with a descriptive name for CLI and integration callers.
pub fn classify_access(source: &ReleaseSource) -> SourceAccess {
    if source
        .unavailable_reason
        .as_deref()
        .is_some_and(|reason| !reason.trim().is_empty())
        || is_unavailable_health(source.health.as_deref())
    {
        return SourceAccess::Unavailable;
    }

    if source.manifest_url.is_some() {
        return SourceAccess::Manifest;
    }

    if source.requires_browser
        || source.browser_url.is_some()
        || is_known_landing_provider(&source.provider, source.url.as_deref())
    {
        return SourceAccess::BrowserRequired;
    }

    if source.url.is_some() {
        SourceAccess::Direct
    } else {
        SourceAccess::Unavailable
    }
}

/// Provenance recorded in a source plan for a human or downloader handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProvenance {
    pub repository: String,
    pub reference: String,
    /// The metadata URL used for the selected title, if fetched remotely.
    pub source_url: Option<String>,
    /// A human-readable local or remote origin.
    pub origin: String,
    /// Unix timestamp in seconds when the plan was created.
    pub timestamp: u64,
}

/// A complete, side-effect-free handoff record for a selected release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcePlan {
    pub schema_version: u32,
    pub title_id: String,
    pub title: String,
    pub release: ReleaseSource,
    pub access: SourceAccess,
    pub provenance: SourceProvenance,
}

#[derive(Debug, Deserialize)]
struct RawCatalogGame {
    id: String,
    title: String,
    platform: String,
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default, alias = "sizeGb", alias = "size_gb")]
    size_gb: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawReleaseSource {
    id: String,
    #[serde(default)]
    provider: String,
    format: String,
    filename: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, alias = "browserUrl")]
    browser_url: Option<String>,
    #[serde(default, alias = "manifestUrl")]
    manifest_url: Option<String>,
    #[serde(default, alias = "sizeBytes")]
    size_bytes: Option<u64>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default, alias = "requiresBrowser")]
    requires_browser: bool,
    #[serde(default, alias = "archivePassword")]
    archive_password: Option<String>,
    #[serde(default)]
    health: Option<String>,
    #[serde(default, alias = "unavailableReason")]
    unavailable_reason: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    flow: Option<String>,
    #[serde(default)]
    record: Option<String>,
    #[serde(default)]
    volume: Option<String>,
}

#[derive(Debug, Clone)]
struct MetadataOrigin {
    source_url: Option<String>,
    origin: String,
}

/// Search catalog entries by case-insensitive, whitespace-separated terms.
/// Every term must occur in the title, title ID, platform, or provider list.
pub fn search_games(config: &SourceConfig, query: &str) -> Result<Vec<CatalogGame>> {
    let (games, _) = load_catalog(config)?;
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    if terms.is_empty() {
        return Ok(games);
    }

    Ok(games
        .into_iter()
        .filter(|game| {
            let mut haystack = String::with_capacity(
                game.id.len()
                    + game.title.len()
                    + game.platform.len()
                    + game.providers.iter().map(String::len).sum::<usize>()
                    + 3,
            );
            haystack.push_str(&game.id);
            haystack.push(' ');
            haystack.push_str(&game.title);
            haystack.push(' ');
            haystack.push_str(&game.platform);
            for provider in &game.providers {
                haystack.push(' ');
                haystack.push_str(provider);
            }
            let haystack = haystack.to_lowercase();
            terms.iter().all(|term| haystack.contains(term))
        })
        .collect())
}

/// List all release records for a catalog title ID.
pub fn list_sources(config: &SourceConfig, title_id: &str) -> Result<Vec<ReleaseSource>> {
    validate_id(title_id, "title ID")?;
    load_game(config, title_id)?;
    let (sources, _) = load_releases(config, title_id)?;
    Ok(sources)
}

/// Build a source handoff plan without downloading or opening a provider URL.
pub fn plan_source(config: &SourceConfig, title_id: &str, release_id: &str) -> Result<SourcePlan> {
    validate_id(title_id, "title ID")?;
    validate_id(release_id, "release ID")?;
    let (game, _) = load_game(config, title_id)?;
    let (sources, origin) = load_releases(config, title_id)?;
    let release = sources
        .into_iter()
        .find(|source| source.id == release_id)
        .ok_or_else(|| {
            CompanionError::Source(format!(
                "release {} was not found for title {}",
                release_id, title_id
            ))
        })?;

    let access = release.access();
    if access == SourceAccess::Unavailable {
        let reason = release
            .unavailable_reason
            .clone()
            .unwrap_or_else(|| "the source has no usable endpoint".to_owned());
        return Err(CompanionError::Source(format!(
            "release {} is unavailable: {}",
            release_id, reason
        )));
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(SourcePlan {
        schema_version: 1,
        title_id: title_id.to_owned(),
        title: game.title,
        release,
        access,
        provenance: SourceProvenance {
            repository: config.repository.clone(),
            reference: config.reference.clone(),
            source_url: origin.source_url,
            origin: origin.origin,
            timestamp,
        },
    })
}

/// Write a plan once, using an exclusive create. Existing plans are never
/// replaced, even if their contents differ.
pub fn write_source_plan(path: &Path, plan: &SourcePlan) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|source| CompanionError::io("create source plan directory", source))?;
    let bytes = serde_json::to_vec_pretty(plan)
        .map_err(|error| CompanionError::Source(format!("serialize source plan: {error}")))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                CompanionError::SourcePlanExists(path.to_path_buf())
            } else {
                CompanionError::io("create source plan", source)
            }
        })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        return Err(CompanionError::io("write source plan", source));
    }
    Ok(())
}

fn load_game(config: &SourceConfig, title_id: &str) -> Result<(CatalogGame, MetadataOrigin)> {
    let (games, origin) = load_catalog(config)?;
    games
        .into_iter()
        .find(|game| game.id == title_id)
        .map(|game| (game, origin))
        .ok_or_else(|| CompanionError::Source(format!("title ID {} was not found", title_id)))
}

fn load_catalog(config: &SourceConfig) -> Result<(Vec<CatalogGame>, MetadataOrigin)> {
    let (bytes, origin) = if let Some(directory) = config.catalog_dir.as_ref() {
        let path = directory.join("catalog.js");
        (
            read_bounded_file(&path, MAX_CATALOG_BYTES, "catalog")?,
            MetadataOrigin {
                source_url: None,
                origin: format!("local catalog: {}", directory.display()),
            },
        )
    } else {
        let url = metadata_url(config, "app/data/catalog.js")?;
        (
            fetch_remote_metadata(&url, MAX_CATALOG_BYTES, "catalog")?,
            MetadataOrigin {
                source_url: Some(url.clone()),
                origin: format!("remote catalog: {url}"),
            },
        )
    };

    let array = extract_games_array(&bytes)?;
    let normalized = normalize_js_json_escapes(array)?;
    let raw: Vec<RawCatalogGame> = serde_json::from_slice(&normalized)
        .map_err(|error| CompanionError::Source(format!("parse catalog games JSON: {error}")))?;
    if raw.is_empty() {
        return Err(CompanionError::Source(
            "catalog contains no game records".to_owned(),
        ));
    }

    let mut ids = HashSet::with_capacity(raw.len());
    let games = raw
        .into_iter()
        .map(|game| {
            validate_id(&game.id, "catalog title ID")?;
            validate_nonempty_text(&game.title, "catalog title")?;
            validate_nonempty_text(&game.platform, "catalog platform")?;
            if !ids.insert(game.id.clone()) {
                return Err(CompanionError::Source(format!(
                    "catalog contains duplicate title ID {}",
                    game.id
                )));
            }
            if let Some(size) = game.size_gb {
                if !size.is_finite() || size < 0.0 {
                    return Err(CompanionError::Source(format!(
                        "catalog size for {} is invalid",
                        game.id
                    )));
                }
            }
            Ok(CatalogGame {
                id: game.id,
                title: game.title,
                platform: game.platform,
                providers: game.providers,
                size_gb: game.size_gb,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((games, origin))
}

fn load_releases(
    config: &SourceConfig,
    title_id: &str,
) -> Result<(Vec<ReleaseSource>, MetadataOrigin)> {
    let (bytes, origin) = if let Some(directory) = config.catalog_dir.as_ref() {
        let path = directory.join("releases").join(format!("{title_id}.json"));
        (
            read_bounded_file(&path, MAX_RELEASE_BYTES, "release metadata")?,
            MetadataOrigin {
                source_url: None,
                origin: format!("local releases: {}", directory.display()),
            },
        )
    } else {
        let url = metadata_url(config, &format!("app/data/releases/{title_id}.json"))?;
        (
            fetch_remote_metadata(&url, MAX_RELEASE_BYTES, "release metadata")?,
            MetadataOrigin {
                source_url: Some(url.clone()),
                origin: format!("remote releases: {url}"),
            },
        )
    };

    let raw: Vec<RawReleaseSource> = serde_json::from_slice(&bytes)
        .map_err(|error| CompanionError::Source(format!("parse release metadata JSON: {error}")))?;
    let mut ids = HashSet::with_capacity(raw.len());
    let sources = raw
        .into_iter()
        .map(|source| normalize_release(source, &mut ids))
        .collect::<Result<Vec<_>>>()?;
    Ok((sources, origin))
}

fn normalize_release(source: RawReleaseSource, ids: &mut HashSet<String>) -> Result<ReleaseSource> {
    validate_id(&source.id, "release ID")?;
    validate_nonempty_text(&source.format, "release format")?;
    validate_filename(&source.filename)?;
    if !ids.insert(source.id.clone()) {
        return Err(CompanionError::Source(format!(
            "release metadata contains duplicate release ID {}",
            source.id
        )));
    }

    let url = normalize_url(source.url, "source URL")?;
    let browser_url = normalize_url(source.browser_url, "browser URL")?;
    let manifest_url = normalize_url(source.manifest_url, "manifest URL")?;
    if let Some(size) = source.size_bytes {
        if size > u64::MAX / 2 {
            return Err(CompanionError::Source(format!(
                "release {} size is unreasonable",
                source.id
            )));
        }
    }
    Ok(ReleaseSource {
        id: source.id,
        provider: source.provider,
        format: source.format,
        filename: source.filename,
        url,
        browser_url,
        manifest_url,
        size_bytes: source.size_bytes,
        version: normalize_optional_text(source.version),
        label: normalize_optional_text(source.label),
        requires_browser: source.requires_browser,
        archive_password: normalize_optional_text(source.archive_password),
        health: normalize_optional_text(source.health),
        unavailable_reason: normalize_optional_text(source.unavailable_reason),
        category: normalize_optional_text(source.category),
        source: normalize_optional_text(source.source),
        flow: normalize_optional_text(source.flow),
        record: normalize_optional_text(source.record),
        volume: normalize_optional_text(source.volume),
    })
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

fn normalize_url(value: Option<String>, field: &str) -> Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    if value.trim().is_empty() {
        return Ok(None);
    }
    validate_https_url(&value, field)?;
    Ok(Some(value))
}

fn validate_https_url(value: &str, field: &str) -> Result<()> {
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(CompanionError::Source(format!(
            "{field} contains control characters or whitespace"
        )));
    }
    let parsed = Url::parse(value)
        .map_err(|error| CompanionError::Source(format!("invalid {field}: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(CompanionError::Source(format!("{field} must use HTTPS")));
    }
    if parsed.host_str().is_none() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CompanionError::Source(format!(
            "{field} must have a host and no embedded credentials"
        )));
    }
    if parsed.fragment().is_some() {
        return Err(CompanionError::Source(format!(
            "{field} may not contain a URL fragment"
        )));
    }
    Ok(())
}

fn metadata_url(config: &SourceConfig, path: &str) -> Result<String> {
    validate_repository(&config.repository)?;
    validate_reference(&config.reference)?;
    if path.contains('\\')
        || path.contains("..")
        || path.chars().any(|character| character.is_control())
    {
        return Err(CompanionError::InvalidInput(
            "metadata path contains unsafe path components".to_owned(),
        ));
    }
    let raw = format!(
        "https://{RAW_GITHUB_HOST}/{}/{}/{}",
        config.repository, config.reference, path
    );
    let parsed = Url::parse(&raw)
        .map_err(|error| CompanionError::InvalidInput(format!("invalid metadata URL: {error}")))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some(RAW_GITHUB_HOST)
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CompanionError::InvalidInput(
            "metadata URL failed the raw GitHub host policy".to_owned(),
        ));
    }
    Ok(parsed.to_string())
}

fn validate_repository(repository: &str) -> Result<()> {
    let mut parts = repository.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || owner.is_empty()
        || name.is_empty()
        || !is_safe_ref_component(owner)
        || !is_safe_ref_component(name)
    {
        return Err(CompanionError::InvalidInput(
            "repository must be a safe GitHub owner/repository value".to_owned(),
        ));
    }
    Ok(())
}

fn validate_reference(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.starts_with('/')
        || reference.ends_with('/')
        || reference.contains("//")
        || reference
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !reference.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/')
        })
    {
        return Err(CompanionError::InvalidInput(
            "reference contains unsafe Git path components".to_owned(),
        ));
    }
    Ok(())
}

fn is_safe_ref_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn validate_id(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 128
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(|character| character.is_control())
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(CompanionError::InvalidInput(format!(
            "{field} is not a safe path component"
        )));
    }
    Ok(())
}

fn validate_filename(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 512
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return Err(CompanionError::Source(format!(
            "release filename is not a safe path component: {value:?}"
        )));
    }
    Ok(())
}

fn validate_nonempty_text(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(|character| character.is_control()) {
        return Err(CompanionError::Source(format!(
            "{field} is empty or unsafe"
        )));
    }
    Ok(())
}

fn is_unavailable_health(health: Option<&str>) -> bool {
    health
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "unavailable" | "offline" | "blocked" | "disabled" | "dead" | "error"
            )
        })
        .unwrap_or(false)
}

fn is_known_landing_provider(provider: &str, url: Option<&str>) -> bool {
    let provider = provider.to_ascii_lowercase();
    if ["vikingfile", "viking file", "mediafire", "filebrowser"]
        .iter()
        .any(|known| provider.contains(known))
    {
        return true;
    }
    url.and_then(|value| Url::parse(value).ok())
        .and_then(|value| value.host_str().map(str::to_ascii_lowercase))
        .map(|host| {
            host.contains("vikingfile")
                || host.contains("mediafire")
                || host.contains("filebrowser")
        })
        .unwrap_or(false)
}

fn read_bounded_file(path: &Path, limit: usize, kind: &str) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path)
        .map_err(|source| CompanionError::io("inspect metadata source file", source))?;
    if metadata.len() > limit as u64 {
        return Err(CompanionError::SourceTooLarge {
            kind: kind.to_owned(),
            limit,
        });
    }
    let file = File::open(path)
        .map_err(|source| CompanionError::io("open source metadata file", source))?;
    read_bounded(file, limit, kind)
}

fn fetch_remote_metadata(url: &str, limit: usize, kind: &str) -> Result<Vec<u8>> {
    let parsed = Url::parse(url)
        .map_err(|error| CompanionError::Source(format!("invalid remote metadata URL: {error}")))?;
    if parsed.scheme() != "https" || parsed.host_str() != Some(RAW_GITHUB_HOST) {
        return Err(CompanionError::Source(
            "remote metadata host is not raw.githubusercontent.com".to_owned(),
        ));
    }

    let agent = ureq::AgentBuilder::new()
        .https_only(true)
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(REMOTE_TIMEOUT)
        .timeout_connect(REMOTE_TIMEOUT)
        .timeout_read(REMOTE_TIMEOUT)
        .timeout_write(REMOTE_TIMEOUT)
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| CompanionError::Source(format!("fetch {kind}: {error}")))?;
    if !(200..300).contains(&response.status()) {
        return Err(CompanionError::Source(format!(
            "fetch {kind} returned HTTP {}",
            response.status()
        )));
    }
    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
    {
        return Err(CompanionError::SourceTooLarge {
            kind: kind.to_owned(),
            limit,
        });
    }
    read_bounded(response.into_reader(), limit, kind)
}

fn read_bounded<R: Read>(mut reader: R, limit: usize, kind: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut limited = (&mut reader).take(limit as u64 + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|source| CompanionError::io("read source metadata", source))?;
    if bytes.len() > limit {
        return Err(CompanionError::SourceTooLarge {
            kind: kind.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

fn extract_games_array(bytes: &[u8]) -> Result<&[u8]> {
    let marker = b"var games";
    if bytes.len() < marker.len() {
        return Err(CompanionError::Source(
            "catalog does not contain a var games JSON array".to_owned(),
        ));
    }
    let mut marker_start = None;
    let last_start = bytes.len().saturating_sub(marker.len());
    for index in 0..=last_start {
        if &bytes[index..index + marker.len()] == marker {
            let mut cursor = index + marker.len();
            while bytes
                .get(cursor)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'=') {
                cursor += 1;
                while bytes
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    cursor += 1;
                }
                if bytes.get(cursor) == Some(&b'[') {
                    marker_start = Some(cursor);
                    break;
                }
            }
        }
    }
    let start = marker_start.ok_or_else(|| {
        CompanionError::Source("catalog does not contain a var games JSON array".to_owned())
    })?;

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(&bytes[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    Err(CompanionError::Source(
        "catalog games array is not closed".to_owned(),
    ))
}

/// Convert the limited JavaScript `\\xHH` string escape used by the upstream
/// catalog into JSON's equivalent `\\u00HH` escape. The scanner only rewrites
/// escapes while inside a double-quoted string; it never evaluates JavaScript
/// or changes identifiers, comments, or array structure.
fn normalize_js_json_escapes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if !in_string {
            if byte == b',' {
                let mut lookahead = index + 1;
                while bytes
                    .get(lookahead)
                    .is_some_and(|next| next.is_ascii_whitespace())
                {
                    lookahead += 1;
                }
                if matches!(bytes.get(lookahead), Some(next) if *next == b']' || *next == b'}') {
                    index += 1;
                    continue;
                }
            }
            normalized.push(byte);
            if byte == b'"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if escaped {
            if byte == b'x' && index + 2 < bytes.len() {
                let high = bytes[index + 1];
                let low = bytes[index + 2];
                if is_hex(high) && is_hex(low) {
                    normalized.extend_from_slice(b"u00");
                    normalized.push(high);
                    normalized.push(low);
                    index += 3;
                    escaped = false;
                    continue;
                }
            }
            normalized.push(byte);
            escaped = false;
            index += 1;
            continue;
        }

        normalized.push(byte);
        match byte {
            b'\\' => escaped = true,
            b'"' => in_string = false,
            _ => {}
        }
        index += 1;
    }
    if in_string || escaped {
        return Err(CompanionError::Source(
            "catalog contains an unterminated string escape".to_owned(),
        ));
    }
    Ok(normalized)
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'A'..=b'F')
}
