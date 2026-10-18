//! Game-release sourcing, adopted from the Overdrive PS5 game store.
//!
//! Overdrive keeps a per-title catalog of releases spread across file hosts.
//! Each release is classified into a source kind (a direct file link, a host
//! page that needs scraping, a gated host, or a browser capture), resolved to
//! a direct URL, and ranked so the full game on the best host wins. This
//! module ports the classification and ranking; [`crate::download`] ports the
//! resolvers. The catalog itself stays user-supplied: repository configuration
//! must not contain game URLs.

use crate::error::{CompanionError, Result};
use serde::Deserialize;

/// One entry of a title catalog file (`<TITLE_ID>.json`), matching the shape
/// Overdrive stores per game.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub browser_url: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub health: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub requires_browser: bool,
    #[serde(default)]
    pub unavailable_reason: String,
    #[serde(default)]
    pub manifest_url: String,
    #[serde(default)]
    pub title_id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub parts: Vec<ReleasePart>,
}

/// One volume of a multi-part release. Ranking only looks at the first part's
/// first source, mirroring Overdrive.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleasePart {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub browser_url: String,
    #[serde(default)]
    pub manifest_url: String,
}

/// How a release can be fetched, from Overdrive's `sourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A URL that should serve the file itself.
    Direct,
    /// A split manifest the download queue must assemble.
    Manifest,
    Mediafire,
    Vikingfile,
    Filekeeper,
    Datanodes,
    Buzzheavier,
    /// Needs a human-driven browser capture; the CLI cannot resolve it.
    Browser,
    /// No usable URL at all.
    Unavailable,
}

/// How reachable a release is, from Overdrive's `access` grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessGroup {
    Direct,
    Page,
    Browser,
    Unavailable,
}

pub(crate) struct SourceRef<'a> {
    url: &'a str,
    browser_url: &'a str,
    manifest_url: &'a str,
}

impl Release {
    pub(crate) fn first_source(&self) -> SourceRef<'_> {
        if let Some(part) = self.parts.first() {
            SourceRef {
                url: &part.url,
                browser_url: &part.browser_url,
                manifest_url: &part.manifest_url,
            }
        } else {
            SourceRef {
                url: &self.url,
                browser_url: &self.browser_url,
                manifest_url: &self.manifest_url,
            }
        }
    }

    /// The URL the classifier and resolvers work from.
    pub fn primary_url(&self) -> &str {
        let first = self.first_source();
        if !first.browser_url.is_empty() {
            first.browser_url
        } else {
            first.url
        }
    }
}

/// Known file hosts and the domains that belong to each, from Overdrive.
const KNOWN_HOSTS: &[(&str, &[&str])] = &[
    ("mediafire", &["mediafire.com"]),
    ("buzzheavier", &["buzzheavier.com"]),
    ("vikingfile", &["vikingfile.com", "vik1ngfile.site"]),
    ("datanodes", &["datanodes.to"]),
    ("filekeeper", &["filekeeper.net"]),
];

/// Hosts confirmed working from the console, from Overdrive's ranking.
fn is_confirmed_host(provider: &str) -> bool {
    let provider = provider.to_lowercase();
    provider == "vikingfile"
        || provider == "vik1ngfile"
        || provider == "datanodes"
        || provider == "filekeeper"
        || provider == "filekeeper.net"
}

fn is_overdrive_cdn(provider: &str) -> bool {
    let normalized: String = provider
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    normalized == "overdrivecdn"
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Strict URL validation, mirroring Overdrive's `webURL`: http(s) only, sane
/// length, no whitespace/control characters or backslashes, and a real host.
pub fn parse_source_url(value: &str) -> Result<url::Url> {
    if value.is_empty()
        || value.len() > 8192
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\' || c == '\0')
    {
        return Err(CompanionError::Download(format!(
            "invalid source URL: {value:?}"
        )));
    }
    let url = url::Url::parse(value)
        .map_err(|error| CompanionError::Download(format!("invalid source URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CompanionError::Download(format!(
            "source URL must be http(s): {value:?}"
        )));
    }
    match url.host_str() {
        Some(host) if !host.is_empty() && !host.starts_with('.') => Ok(url),
        _ => Err(CompanionError::Download(format!(
            "source URL has no host: {value:?}"
        ))),
    }
}

/// Classify a release the way Overdrive's `sourceKind` does.
pub fn source_kind(release: &Release) -> Result<SourceKind> {
    let first = release.first_source();
    if !first.manifest_url.is_empty() {
        return Ok(SourceKind::Manifest);
    }
    let value = if !first.browser_url.is_empty() {
        first.browser_url
    } else {
        first.url
    };
    if value.is_empty() {
        return Ok(SourceKind::Unavailable);
    }
    let url = parse_source_url(value)?;
    let host = url.host_str().unwrap_or("").to_lowercase();
    let path = url.path();
    let provider = KNOWN_HOSTS
        .iter()
        .find(|(_, domains)| domains.iter().any(|domain| host_matches(&host, domain)))
        .map(|(name, _)| *name);
    if provider == Some("mediafire") && host.starts_with("download") {
        return Ok(SourceKind::Direct);
    }
    if provider == Some("vikingfile") && path.starts_with("/d/") && first.browser_url.is_empty() {
        return Ok(SourceKind::Direct);
    }
    if let Some(provider) = provider {
        return Ok(match provider {
            "mediafire" => SourceKind::Mediafire,
            "buzzheavier" => SourceKind::Buzzheavier,
            "vikingfile" => SourceKind::Vikingfile,
            "datanodes" => SourceKind::Datanodes,
            "filekeeper" => SourceKind::Filekeeper,
            _ => SourceKind::Browser,
        });
    }
    let raw_path = raw_path_after_host(value);
    if host == "archive.org" && path.starts_with("/download/")
        || host == "github.com" && path.contains("/releases/download/")
        || has_direct_file_extension(&raw_path)
    {
        return Ok(SourceKind::Direct);
    }
    if release.requires_browser || release.source.starts_with("pegasus") {
        return Ok(SourceKind::Browser);
    }
    Ok(SourceKind::Direct)
}

/// The path-plus-query portion of the original string, for extension checks
/// that must see past `?` and `#` like Overdrive's regex does.
fn raw_path_after_host(value: &str) -> String {
    let after_scheme = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    after_scheme
        .find('/')
        .map(|index| after_scheme[index..].to_owned())
        .unwrap_or_default()
}

fn has_direct_file_extension(raw_path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        ".ffpfsc", ".ffpfs", ".ffpkg", ".exfat", ".pkg", ".zip", ".rar", ".7z",
    ];
    let lower = raw_path.to_lowercase();
    EXTENSIONS.iter().any(|extension| {
        lower
            .find(extension)
            .map(|index| {
                let rest = &lower[index + extension.len()..];
                rest.is_empty() || rest.starts_with(['?', '#'])
            })
            .unwrap_or(false)
    })
}

/// Reachability grouping, mirroring Overdrive's `access`. An unparseable URL
/// degrades to browser capture instead of failing the whole catalog.
pub fn access_group(release: &Release) -> AccessGroup {
    let kind = source_kind(release).unwrap_or(SourceKind::Browser);
    match kind {
        SourceKind::Direct | SourceKind::Manifest => AccessGroup::Direct,
        SourceKind::Browser | SourceKind::Datanodes | SourceKind::Unavailable => {
            AccessGroup::Browser
        }
        // Gated hosts surface under browser capture in the UI even though the
        // resolver can still scrape some of them.
        SourceKind::Vikingfile | SourceKind::Buzzheavier => AccessGroup::Browser,
        SourceKind::Mediafire | SourceKind::Filekeeper => AccessGroup::Page,
    }
}

fn word_contains(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    haystack.match_indices(&needle).any(|(index, _)| {
        let before = haystack[..index].chars().next_back();
        let after = haystack[index + needle.len()..].chars().next();
        let boundary = |c: Option<char>| {
            c.map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(true)
        };
        boundary(before) && boundary(after)
    })
}

/// A DLC-only release, not the full game. Mirrors Overdrive's `isDlc`.
pub fn is_dlc(label: &str) -> bool {
    if !(word_contains(label, "dlc") || word_contains(label, "dlcs")) {
        return false;
    }
    let lower = label.to_lowercase();
    if word_contains(&lower, "game") || word_contains(&lower, "full") {
        return false;
    }
    // "+ DLC", "+ all DLCs", "with (all) DLC(s)", "incl..." describe bundles,
    // not DLC-only releases.
    !(contains_plus_dlc(&lower) || contains_with_dlc(&lower) || lower.contains("incl"))
}

fn contains_plus_dlc(lower: &str) -> bool {
    let mut index = 0;
    while let Some(pos) = lower[index..].find('+') {
        let rest = lower[index + pos + 1..].trim_start_matches([' ', '\t']);
        let rest = rest.strip_prefix("all ").unwrap_or(rest).trim_start();
        if rest.starts_with("dlc") {
            return true;
        }
        index += pos + 1;
    }
    false
}

fn contains_with_dlc(lower: &str) -> bool {
    let mut index = 0;
    while let Some(pos) = lower[index..].find("with") {
        let before = lower[..index + pos].chars().next_back();
        if before
            .map(|c| c.is_ascii_alphanumeric() || c == '_')
            .unwrap_or(false)
        {
            index += pos + 4;
            continue;
        }
        let rest = lower[index + pos + 4..].trim_start();
        let rest = rest.strip_prefix("all ").unwrap_or(rest).trim_start();
        if rest.starts_with("dlc") {
            return true;
        }
        index += pos + 4;
    }
    false
}

/// A backport release, from Overdrive's `isBackport`.
pub fn is_backport(label: &str) -> bool {
    let lower = label.to_lowercase();
    let mut index = 0;
    while let Some(pos) = lower[index..].find("back") {
        let start = index + pos;
        let before = lower[..start].chars().next_back();
        if before
            .map(|c| c.is_ascii_alphanumeric() || c == '_')
            .unwrap_or(false)
        {
            index = start + 4;
            continue;
        }
        let rest = lower[start + 4..].trim_start();
        if let Some(after_port) = rest.strip_prefix("port") {
            let after = after_port.chars().next();
            if after
                .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(true)
            {
                return true;
            }
        }
        index = start + 4;
    }
    false
}

/// A patch-only file set, from Overdrive's `isPatchOnly` (`/backport[+_ -]*files/i`).
/// DLPSGAME's "backport files" downloads hold only the files to copy over the
/// base game; the catalog sometimes numbers one as a part of a full backport
/// archive, but it is its own file, so it never joins a set.
pub fn is_patch_only(release: &Release) -> bool {
    let first = release.first_source();
    let combined = format!("{} {} {}", first.url, first.browser_url, release.label).to_lowercase();
    let mut index = 0;
    while let Some(pos) = combined[index..].find("backport") {
        let rest = combined[index + pos + 8..].trim_start_matches(['+', '_', ' ', '-']);
        if rest.starts_with("files") {
            return true;
        }
        index += pos + 8;
    }
    false
}

/// A PS5 PKG (not a PS4 fake PKG), from Overdrive's `isPs5Package`.
pub fn is_ps5_package(release: &Release) -> bool {
    let format = release.format.to_lowercase();
    if format != "pkg" && format != "fpkg" {
        return false;
    }
    let title = if !release.title_id.is_empty() {
        &release.title_id
    } else {
        &release.filename
    };
    title.to_lowercase().starts_with("ppsa")
}

fn provider_key(provider: &str) -> String {
    provider.to_lowercase().replace("www.", "")
}

/// Host preference, from Overdrive's `hostRank`: the project's own CDN, then
/// archive.org, then MediaFire, then console-confirmed hosts, then the rest.
fn host_rank(provider: &str) -> i32 {
    let provider = provider_key(provider);
    if is_overdrive_cdn(&provider) {
        0
    } else if provider == "archive.org" {
        1
    } else if provider == "mediafire" {
        2
    } else if is_confirmed_host(&provider) {
        3
    } else {
        4
    }
}

fn health_rank(health: &str) -> i32 {
    match health.to_lowercase().as_str() {
        "ok" => 0,
        "flaky" => 2,
        _ => 1,
    }
}

/// The full Overdrive ranking key: full game first, available first, then host
/// preference, reachability, health, and capture requirements. Lower wins.
pub fn rank_key(release: &Release) -> [i32; 10] {
    [
        is_dlc(&release.label) as i32,
        (!release.unavailable_reason.is_empty()) as i32,
        is_patch_only(release) as i32,
        is_ps5_package(release) as i32,
        is_backport(&release.label) as i32,
        host_rank(&release.provider),
        access_group(release) as i32,
        health_rank(&release.health),
        (release.source != "orbit") as i32,
        release.requires_browser as i32,
    ]
}

/// Index of the best release, or `None` when the catalog is empty. Ties keep
/// catalog order, mirroring Overdrive's `preferredRelease`.
pub fn preferred_release(releases: &[Release]) -> Option<usize> {
    let mut best: Option<(usize, [i32; 10])> = None;
    for (index, release) in releases.iter().enumerate() {
        let key = rank_key(release);
        let replace = match &best {
            None => true,
            Some((_, best_key)) => key < *best_key,
        };
        if replace {
            best = Some((index, key));
        }
    }
    best.map(|(index, _)| index)
}

/// Load a title catalog file (`<TITLE_ID>.json`) from a catalog directory.
pub fn load_catalog_file(catalog_dir: &std::path::Path, title_id: &str) -> Result<Vec<Release>> {
    let normalized = title_id.to_uppercase();
    if normalized.is_empty()
        || normalized.len() > 16
        || !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CompanionError::Download(format!(
            "invalid title id: {title_id:?}"
        )));
    }
    let path = catalog_dir.join(format!("{normalized}.json"));
    let bytes =
        std::fs::read(&path).map_err(|source| CompanionError::io("read catalog file", source))?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(CompanionError::Download(format!(
            "catalog file is too large: {}",
            path.display()
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| CompanionError::Download(format!("parse catalog file: {error}")))
}

// ---------------------------------------------------------------------------
// Archive sets, ported from Overdrive's `releaseSets` (app/state/release-sets.js).
//
// Overdrive's catalog lists each volume of a split archive as its own
// release ("... part 1", "... part 2"). At load time the queue groups those
// into one synthetic entry whose parts download sequentially, then the set is
// extracted as a unit. Grouping key: provider + label with the part marker
// stripped. A set whose volumes are gapped, duplicated, or misnumbered is
// marked unavailable with the reason, exactly like Overdrive.
// ---------------------------------------------------------------------------

/// One volume of an archive set: a single catalog release carrying it.
#[derive(Debug, Clone)]
pub struct SetPart {
    pub number: usize,
    pub source: Release,
}

/// A grouped multi-part release. `label` is the part marker stripped, with
/// the "· N parts" suffix Overdrive appends.
#[derive(Debug, Clone)]
pub struct ArchiveSet {
    pub label: String,
    pub provider: String,
    pub parts: Vec<SetPart>,
    pub size_bytes: Option<u64>,
    pub unavailable_reason: String,
}

/// One downloadable catalog entry: either a single release or a grouped set.
#[derive(Debug, Clone)]
pub enum CatalogEntry {
    Single(Box<Release>),
    Set(ArchiveSet),
}

impl CatalogEntry {
    /// The release ranking and resolution work from, mirroring Overdrive's
    /// `first_source()`: a set ranks by its first part's source.
    pub fn representative(&self) -> &Release {
        match self {
            CatalogEntry::Single(release) => release,
            CatalogEntry::Set(set) => &set.parts[0].source,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            CatalogEntry::Single(release) => &release.label,
            CatalogEntry::Set(set) => &set.label,
        }
    }

    pub fn provider(&self) -> &str {
        match self {
            CatalogEntry::Single(release) => &release.provider,
            CatalogEntry::Set(set) => &set.provider,
        }
    }

    pub fn unavailable_reason(&self) -> &str {
        match self {
            CatalogEntry::Single(release) => &release.unavailable_reason,
            CatalogEntry::Set(set) => &set.unavailable_reason,
        }
    }

    pub fn size_bytes(&self) -> Option<u64> {
        match self {
            CatalogEntry::Single(release) => release.size_bytes,
            CatalogEntry::Set(set) => set.size_bytes,
        }
    }

    pub fn part_count(&self) -> usize {
        match self {
            CatalogEntry::Single(_) => 1,
            CatalogEntry::Set(set) => set.parts.len(),
        }
    }
}

/// Match `/\bpart\s*(\d+)\b/i` against a label, returning the volume number.
/// No regex crate in the dependency set, so the pattern is hand-rolled.
fn parse_part_number(label: &str) -> Option<usize> {
    let lower = label.to_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while let Some(pos) = lower[index..].find("part") {
        let start = index + pos;
        let is_boundary = |i: usize| {
            i == 0 || {
                let c = bytes[i - 1] as char;
                !(c.is_ascii_alphanumeric() || c == '_')
            }
        };
        if is_boundary(start) {
            let rest = &lower[start + 4..];
            let trimmed = rest.trim_start();
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let end = start + 4 + (rest.len() - trimmed.len()) + digits.len();
                let boundary_after = end >= bytes.len() || {
                    let c = bytes[end] as char;
                    !(c.is_ascii_alphanumeric() || c == '_')
                };
                if boundary_after {
                    return digits.parse::<usize>().ok();
                }
            }
        }
        index = start + 4;
    }
    None
}

/// Strip the first `/\bpart\s*\d+\b/i` occurrence from a label, like
/// Overdrive's group-label normalization.
fn strip_part_marker(label: &str) -> String {
    let lower = label.to_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while let Some(pos) = lower[index..].find("part") {
        let start = index + pos;
        let boundary_before = start == 0 || {
            let c = bytes[start - 1] as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        };
        if boundary_before {
            let rest = &lower[start + 4..];
            let trimmed = rest.trim_start();
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let end = start + 4 + (rest.len() - trimmed.len()) + digits.len();
                let boundary_after = end >= bytes.len() || {
                    let c = bytes[end] as char;
                    !(c.is_ascii_alphanumeric() || c == '_')
                };
                if boundary_after {
                    let mut out = label[..start].to_owned();
                    out.push_str(label[end..].trim_start());
                    return out.trim().to_owned();
                }
            }
        }
        index = start + 4;
    }
    label.to_owned()
}

/// Group part-labeled releases into archive sets, porting Overdrive's
/// `releaseSets`. Singles pass through untouched; patch-only releases are
/// never grouped.
pub fn group_archive_sets(releases: Vec<Release>) -> Vec<CatalogEntry> {
    // (provider key, normalized label) -> index into `entries` (must be a Set)
    let mut groups: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut entries: Vec<CatalogEntry> = Vec::with_capacity(releases.len());

    for release in releases {
        let Some(number) = (!is_patch_only(&release))
            .then(|| parse_part_number(&release.label))
            .flatten()
        else {
            entries.push(CatalogEntry::Single(Box::new(release)));
            continue;
        };
        if !(1..=1024).contains(&number) {
            let mut release = release;
            release.unavailable_reason = "Invalid volume number.".to_owned();
            entries.push(CatalogEntry::Single(Box::new(release)));
            continue;
        }
        let base_label = strip_part_marker(&release.label);
        let key = (release.provider.to_lowercase(), base_label.to_lowercase());
        let same_source =
            |a: &Release, b: &Release| a.url == b.url && a.browser_url == b.browser_url;
        match groups.get(&key).copied() {
            Some(index) => {
                let CatalogEntry::Set(set) = &mut entries[index] else {
                    unreachable!("group index always points at a set");
                };
                if let Some(part) = set.parts.iter().find(|part| part.number == number) {
                    if !same_source(&part.source, &release) {
                        set.unavailable_reason =
                            "Ambiguous duplicate volume. Choose another complete set.".to_owned();
                    }
                    // Same source seen twice: ignore the duplicate.
                } else {
                    set.parts.push(SetPart {
                        number,
                        source: release,
                    });
                }
            }
            None => {
                let provider = release.provider.clone();
                let set = ArchiveSet {
                    label: base_label,
                    provider,
                    parts: vec![SetPart {
                        number,
                        source: release,
                    }],
                    size_bytes: None,
                    unavailable_reason: String::new(),
                };
                // Overdrive drops per-release size/sha on the group; the real
                // sizes live on the parts.
                entries.push(CatalogEntry::Set(set));
                groups.insert(key, entries.len() - 1);
            }
        }
    }

    for entry in entries.iter_mut() {
        let CatalogEntry::Set(set) = entry else {
            continue;
        };
        set.parts.sort_by_key(|part| part.number);
        if set.unavailable_reason.is_empty() && missing_volumes(&set.parts).is_some() {
            set.unavailable_reason =
                "This source has missing volumes. Choose another complete set.".to_owned();
        }
        if set.unavailable_reason.is_empty() {
            set.size_bytes = sum_some(set.parts.iter().map(|part| part.source.size_bytes));
        }
        set.label = format!("{} · {} parts", set.label, set.parts.len());
    }
    entries
}

/// Match Overdrive's `VOLUME_NAME` (`/\bpart(\d+)\.(?:rar|7z|zip)\b/i`) against a
/// file name or URL, returning the volume number the name claims.
fn parse_volume_number(name: &str) -> Option<usize> {
    let lower = name.to_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while let Some(pos) = lower[index..].find("part") {
        let start = index + pos;
        let boundary_before = start == 0 || {
            let c = bytes[start - 1] as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        };
        if boundary_before {
            let rest = &lower[start + 4..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let after_digits = &rest[digits.len()..];
                if let Some(ext) = after_digits.strip_prefix('.') {
                    for candidate in ["rar", "7z", "zip"] {
                        if ext.starts_with(candidate) {
                            let end = start + 4 + digits.len() + 1 + candidate.len();
                            let boundary_after = end >= bytes.len() || {
                                let c = bytes[end] as char;
                                !(c.is_ascii_alphanumeric() || c == '_')
                            };
                            if boundary_after {
                                return digits.parse::<usize>().ok();
                            }
                        }
                    }
                }
            }
        }
        index = start + 4;
    }
    None
}

/// Full port of Overdrive's `missingVolumes`: gapped numbering, volume names
/// that disagree with the part order ("renumbered"), a lone volume-named
/// part, and a last volume as large as the first — which means the real last
/// volume (always the small one) is missing. `declaredBytes` is always unset
/// on the group, mirroring Overdrive, so the size leg always applies.
fn missing_volumes(parts: &[SetPart]) -> Option<&'static str> {
    if parts
        .iter()
        .enumerate()
        .any(|(index, part)| part.number != index + 1)
    {
        return Some("gap");
    }
    let volumes: Vec<Option<usize>> = parts
        .iter()
        .map(|part| {
            let source = &part.source;
            let name = if !source.url.is_empty() {
                source.url.as_str()
            } else {
                source.browser_url.as_str()
            };
            parse_volume_number(name)
        })
        .collect();
    if volumes
        .iter()
        .enumerate()
        .any(|(index, volume)| volume.is_some_and(|number| number != index + 1))
    {
        return Some("renumbered");
    }
    if parts.len() == 1 && volumes[0].is_some() {
        return Some("count");
    }
    if parts.len() >= 2 {
        if let (Some(_), Some(_), Some(first_bytes), Some(last_bytes)) = (
            volumes[0],
            volumes[parts.len() - 1],
            parts[0].source.size_bytes,
            parts[parts.len() - 1].source.size_bytes,
        ) {
            if first_bytes > 0
                && last_bytes > 0
                && last_bytes.abs_diff(first_bytes) <= first_bytes / 100
            {
                return Some("last");
            }
        }
    }
    None
}

fn sum_some(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for value in values {
        let Some(value) = value else { continue };
        any = true;
        total = total.saturating_add(value);
    }
    any.then_some(total)
}

/// Ranking key for an entry: the set ranks by its first part's source.
pub fn entry_rank_key(entry: &CatalogEntry) -> [i32; 10] {
    rank_key(entry.representative())
}

/// Index of the best entry, or `None` when the catalog is empty. Ties keep
/// catalog order, mirroring Overdrive's `preferredRelease`.
pub fn preferred_entry(entries: &[CatalogEntry]) -> Option<usize> {
    let mut best: Option<(usize, [i32; 10])> = None;
    for (index, entry) in entries.iter().enumerate() {
        let key = entry_rank_key(entry);
        let replace = match &best {
            None => true,
            Some((_, best_key)) => key < *best_key,
        };
        if replace {
            best = Some((index, key));
        }
    }
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(provider: &str, label: &str, url: &str) -> Release {
        Release {
            provider: provider.to_owned(),
            format: "binary".to_owned(),
            label: label.to_owned(),
            url: url.to_owned(),
            browser_url: String::new(),
            size_bytes: None,
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

    #[test]
    fn classifies_source_kinds() {
        assert!(matches!(
            source_kind(&release("x", "", "https://archive.org/download/a/game.7z")),
            Ok(SourceKind::Direct)
        ));
        assert!(matches!(
            source_kind(&release(
                "MediaFire",
                "",
                "https://www.mediafire.com/file/abc/game.rar"
            )),
            Ok(SourceKind::Mediafire)
        ));
        assert!(matches!(
            source_kind(&release(
                "VikingFile",
                "",
                "https://vikingfile.com/f/abc123"
            )),
            Ok(SourceKind::Vikingfile)
        ));
        assert!(matches!(
            source_kind(&release("x", "", "https://filekeeper.net/abcdefgh1234")),
            Ok(SourceKind::Filekeeper)
        ));
        assert!(source_kind(&release("x", "", "not a url")).is_err());
        assert!(matches!(
            source_kind(&release("x", "", "")),
            Ok(SourceKind::Unavailable)
        ));
    }

    #[test]
    fn mediafire_download_host_is_direct() {
        assert!(matches!(
            source_kind(&release(
                "MediaFire",
                "",
                "https://download123.mediafire.com/abc/game.rar"
            )),
            Ok(SourceKind::Direct)
        ));
    }

    #[test]
    fn prefers_full_game_on_best_host() {
        let releases = vec![
            release(
                "Buzzheavier",
                "EUR 9.xx Base",
                "https://buzzheavier.com/f/abc",
            ),
            release(
                "MediaFire",
                "EUR 9.xx BackPort 4.xx",
                "https://www.mediafire.com/file/abc/game.rar",
            ),
            release(
                "archive.org",
                "EUR 9.xx Base",
                "https://archive.org/download/a/game.7z",
            ),
        ];
        // archive.org outranks mediafire; the backport loses to both full games.
        assert_eq!(preferred_release(&releases), Some(2));
    }

    #[test]
    fn unavailable_releases_lose() {
        let mut good = release("archive.org", "Base", "https://archive.org/download/a/g.7z");
        let mut bad = release("archive.org", "Base", "https://archive.org/download/a/g.7z");
        bad.unavailable_reason = "HTTP 401".to_owned();
        assert_eq!(preferred_release(&[bad, good.clone()]), Some(1));
        good.health = "flaky".to_owned();
        assert_eq!(preferred_release(&[good],), Some(0));
    }

    #[test]
    fn detects_dlc_and_backport_labels() {
        assert!(is_dlc("EUR DLC Pack"));
        assert!(!is_dlc("EUR Base + all DLCs"));
        assert!(!is_dlc("EUR Base Game"));
        assert!(is_backport("EUR 9.xx BackPort 4.xx"));
        assert!(is_backport("back port files"));
        assert!(!is_backport("EUR 9.xx Base"));
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_source_url("https://example.com/game.7z").is_ok());
        assert!(parse_source_url("ftp://example.com/game.7z").is_err());
        assert!(parse_source_url("https://example.com/a b").is_err());
        assert!(parse_source_url("https://.example.com/").is_err());
        assert!(parse_source_url(&"https://example.com/".repeat(1000)).is_err());
    }

    fn part_release(provider: &str, label: &str, url: &str) -> Release {
        let mut release = release(provider, label, url);
        release.size_bytes = Some(100);
        release
    }

    #[test]
    fn groups_part_labeled_releases_into_sets() {
        let releases = vec![
            part_release(
                "MediaFire",
                "EUR Base - Mediafire part 1",
                "https://a.example.com/p1",
            ),
            part_release(
                "MediaFire",
                "EUR Base - Mediafire part 2",
                "https://a.example.com/p2",
            ),
            part_release(
                "MediaFire",
                "EUR Base - Mediafire part 3",
                "https://a.example.com/p3",
            ),
            part_release(
                "MediaFire",
                "EUR Base - Mediafire part 4",
                "https://a.example.com/p4",
            ),
        ];
        let entries = group_archive_sets(releases);
        assert_eq!(entries.len(), 1);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert_eq!(set.parts.len(), 4);
        assert_eq!(set.parts[0].number, 1);
        assert_eq!(set.parts[3].number, 4);
        assert_eq!(set.label, "EUR Base - Mediafire · 4 parts");
        assert_eq!(set.size_bytes, Some(400));
        assert!(set.unavailable_reason.is_empty());
    }

    #[test]
    fn set_parts_need_not_arrive_in_order() {
        let releases = vec![
            part_release("MediaFire", "Base part 3", "https://a.example.com/p3"),
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1"),
            part_release("MediaFire", "Base part 2", "https://a.example.com/p2"),
        ];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert_eq!(set.parts[0].number, 1);
        assert_eq!(set.parts[2].number, 3);
        assert!(set.unavailable_reason.is_empty());
    }

    #[test]
    fn gapped_sets_are_unavailable() {
        let releases = vec![
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1"),
            part_release("MediaFire", "Base part 2", "https://a.example.com/p2"),
            part_release("MediaFire", "Base part 4", "https://a.example.com/p4"),
        ];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.contains("missing volumes"));
    }

    #[test]
    fn duplicate_part_sources_poison_the_set() {
        let releases = vec![
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1a"),
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1b"),
        ];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.contains("Ambiguous duplicate"));
        assert_eq!(set.parts.len(), 1);
    }

    #[test]
    fn identical_duplicate_releases_are_deduped() {
        let releases = vec![
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1"),
            part_release("MediaFire", "Base part 1", "https://a.example.com/p1"),
        ];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.is_empty());
        assert_eq!(set.parts.len(), 1);
    }

    #[test]
    fn invalid_part_numbers_stay_single_and_unavailable() {
        let releases = vec![
            part_release("MediaFire", "Base part 0", "https://a.example.com/p0"),
            part_release("MediaFire", "Base", "https://a.example.com/whole"),
        ];
        let entries = group_archive_sets(releases);
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], CatalogEntry::Single(_)));
        assert!(entries[0]
            .unavailable_reason()
            .contains("Invalid volume number"));
        assert!(matches!(entries[1], CatalogEntry::Single(_)));
    }

    #[test]
    fn providers_and_labels_group_separately() {
        let releases = vec![
            part_release("MediaFire", "EUR Base part 1", "https://a.example.com/p1"),
            part_release("Buzzheavier", "EUR Base part 1", "https://b.example.com/p1"),
            part_release("MediaFire", "USA Base part 1", "https://a.example.com/u1"),
        ];
        let entries = group_archive_sets(releases);
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn patch_only_releases_are_never_grouped() {
        let mut backport = part_release(
            "MediaFire",
            "EUR Backport files part 1",
            "https://a.example.com/p1",
        );
        // "backport files" in the URL/label makes it patch-only.
        backport.url = "https://a.example.com/backport_files/part1".to_owned();
        let entries = group_archive_sets(vec![backport]);
        assert!(matches!(entries[0], CatalogEntry::Single(_)));
    }

    #[test]
    fn renumbered_volume_names_are_unavailable() {
        let releases = vec![
            part_release(
                "MediaFire",
                "Base part 1",
                "https://a.example.com/game.part2.rar",
            ),
            part_release(
                "MediaFire",
                "Base part 2",
                "https://a.example.com/game.part1.rar",
            ),
            part_release(
                "MediaFire",
                "Base part 3",
                "https://a.example.com/game.part3.rar",
            ),
        ];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.contains("missing volumes"));
    }

    #[test]
    fn lone_volume_named_part_is_unavailable() {
        let releases = vec![part_release(
            "MediaFire",
            "Base part 1",
            "https://a.example.com/game.part1.rar",
        )];
        let entries = group_archive_sets(releases);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.contains("missing volumes"));
    }

    #[test]
    fn full_sized_last_volume_means_a_missing_volume() {
        let sized = |n: usize, url: &str, size: u64| {
            let mut release = part_release("MediaFire", &format!("Base part {n}"), url);
            release.size_bytes = Some(size);
            release
        };
        // Last volume as large as the first: the real (small) last volume is missing.
        let bad = vec![
            sized(1, "https://a.example.com/game.part1.rar", 1000),
            sized(2, "https://a.example.com/game.part2.rar", 1000),
            sized(3, "https://a.example.com/game.part3.rar", 995),
        ];
        let entries = group_archive_sets(bad);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.contains("missing volumes"));

        // A genuinely small last volume is fine.
        let good = vec![
            sized(1, "https://a.example.com/game.part1.rar", 1000),
            sized(2, "https://a.example.com/game.part2.rar", 1000),
            sized(3, "https://a.example.com/game.part3.rar", 500),
        ];
        let entries = group_archive_sets(good);
        let CatalogEntry::Set(set) = &entries[0] else {
            panic!("expected a set");
        };
        assert!(set.unavailable_reason.is_empty());
    }

    #[test]
    fn set_ranks_by_its_first_part_source() {
        let set_source = part_release(
            "archive.org",
            "Base part 1",
            "https://archive.org/download/a/game.7z.001",
        );
        let entries = vec![
            CatalogEntry::Single(Box::new(release(
                "Buzzheavier",
                "Base",
                "https://buzzheavier.com/f/abc",
            ))),
            CatalogEntry::Set(ArchiveSet {
                label: "Base · 2 parts".to_owned(),
                provider: "archive.org".to_owned(),
                parts: vec![
                    SetPart {
                        number: 1,
                        source: set_source,
                    },
                    SetPart {
                        number: 2,
                        source: release("archive.org", "Base part 2", "https://x.example.com/p2"),
                    },
                ],
                size_bytes: None,
                unavailable_reason: String::new(),
            }),
        ];
        // archive.org's host rank beats buzzheavier even though the single
        // release came first in catalog order.
        assert_eq!(preferred_entry(&entries), Some(1));
    }
}
