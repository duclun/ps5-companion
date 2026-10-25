use fs2::available_space;
use ps5_companion::{
    extract_archive, inspect_archive_with_tool, list_sources, plan_source, plan_upload,
    search_games, write_source_plan, ArchiveListing, CompanionError, ExtractionRequest,
    GameArtifact, GameArtifactKind, ReleaseSource, SevenZipConfig, SourceAccess, SourceConfig,
    UploadClient, UploadPollStatus,
};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage:
  ps5-companion inspect <archive> [--seven-zip <path>]
  ps5-companion extract <archive> --work-dir <directory> [--seven-zip <path>] [--title-id <PPSA-id>]
  ps5-companion plan-upload <manifest>
  ps5-companion upload <manifest> --addr <ps5-ip> [--engine <loopback-url>]
      [--timeout-seconds <1..86400>]
  ps5-companion search <query> [--catalog-dir <app/data>]
      [--catalog-repo <owner/repo>] [--catalog-ref <ref>]
  ps5-companion sources <title-id> [--catalog-dir <app/data>]
      [--catalog-repo <owner/repo>] [--catalog-ref <ref>]
  ps5-companion plan-source <title-id> --release <release-id> --out <plan.json>
      [--catalog-dir <app/data>] [--catalog-repo <owner/repo>] [--catalog-ref <ref>]
  ps5-companion help
  ps5-companion version";
const SAFETY_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_ENGINE_URL: &str = "http://127.0.0.1:19113";
const DEFAULT_UPLOAD_TIMEOUT_SECONDS: u64 = 3600;
const MAX_UPLOAD_TIMEOUT_SECONDS: u64 = 86400;

#[derive(Debug, PartialEq, Eq)]
enum CommandLine {
    Inspect {
        archive: PathBuf,
        seven_zip: Option<PathBuf>,
    },
    Extract {
        archive: PathBuf,
        work_dir: PathBuf,
        seven_zip: Option<PathBuf>,
        expected_title_id: Option<String>,
    },
    PlanUpload {
        manifest: PathBuf,
    },
    Upload {
        manifest: PathBuf,
        addr: String,
        engine: String,
        timeout_seconds: u64,
    },
    Search {
        query: String,
        catalog: SourceConfig,
    },
    Sources {
        title_id: String,
        catalog: SourceConfig,
    },
    PlanSource {
        title_id: String,
        release_id: String,
        out: PathBuf,
        catalog: SourceConfig,
    },
    Help,
    Version,
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    Operation(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}\n\n{USAGE}"),
            Self::Operation(message) => write!(formatter, "error: {message}"),
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliError> {
    let command = parse_args(env::args_os().skip(1).collect())?;
    match command {
        CommandLine::Help => {
            println!("{USAGE}");
            println!("\nRun `ps5-companion inspect` or `extract` with an existing local archive.");
        }
        CommandLine::Version => println!("ps5-companion {}", env!("CARGO_PKG_VERSION")),
        CommandLine::Inspect { archive, seven_zip } => {
            inspect_command(&archive, seven_zip.as_deref())?
        }
        CommandLine::Extract {
            archive,
            work_dir,
            seven_zip,
            expected_title_id,
        } => extract_command(&archive, &work_dir, seven_zip.as_deref(), expected_title_id)?,
        CommandLine::PlanUpload { manifest } => plan_upload_command(&manifest)?,
        CommandLine::Upload {
            manifest,
            addr,
            engine,
            timeout_seconds,
        } => upload_command(&manifest, &addr, &engine, timeout_seconds)?,
        CommandLine::Search { query, catalog } => search_command(&catalog, &query)?,
        CommandLine::Sources { title_id, catalog } => sources_command(&catalog, &title_id)?,
        CommandLine::PlanSource {
            title_id,
            release_id,
            out,
            catalog,
        } => plan_source_command(&catalog, &title_id, &release_id, &out)?,
    }
    Ok(())
}

fn search_command(catalog: &SourceConfig, query: &str) -> Result<(), CliError> {
    let games = search_games(catalog, query).map_err(operation_error)?;
    if games.is_empty() {
        println!("No catalog games matched {query:?}.");
        return Ok(());
    }
    println!("Catalog matches for {query:?}:");
    for game in games {
        let value = serde_json::to_value(&game).map_err(|error| {
            CliError::Operation(format!("could not format catalog game: {error}"))
        })?;
        let id = json_text(&value, "id");
        let title = json_text(&value, "title");
        let platform = json_text(&value, "platform");
        let size = value
            .get("size_gb")
            .and_then(|size| size.as_f64())
            .map(|size| format!("{size:.3} GiB"))
            .unwrap_or_else(|| "size unknown".to_owned());
        let region = game.region.as_deref().unwrap_or("region unknown");
        let edition = if game.alternate_edition {
            "; alternate edition"
        } else {
            ""
        };
        let work = game
            .work
            .map(|id| format!("; work {id}"))
            .unwrap_or_default();
        let catalogs = if game.catalogs.is_empty() {
            String::new()
        } else {
            format!("; catalogs {}", game.catalogs.join(", "))
        };
        println!("  {id}  {title}  [{platform}; {region}{edition}{work}{catalogs}]  ({size})");
    }
    Ok(())
}

fn sources_command(catalog: &SourceConfig, title_id: &str) -> Result<(), CliError> {
    let sources = list_sources(catalog, title_id).map_err(operation_error)?;
    if sources.is_empty() {
        println!("No sources found for {title_id}.");
        return Ok(());
    }
    println!("Sources for {title_id}:");
    for source in sources {
        print_source(&source)?;
    }
    println!("Source records describe access and metadata; this command does not download files.");
    Ok(())
}

fn print_source(source: &ReleaseSource) -> Result<(), CliError> {
    let value = serde_json::to_value(source).map_err(|error| {
        CliError::Operation(format!("could not format release source: {error}"))
    })?;
    let id = json_text(&value, "id");
    let provider = json_text(&value, "provider");
    let version = json_text_or(&value, "version", "version unknown");
    let format = json_text_or(&value, "format", "format unknown");
    let size = value
        .get("size_bytes")
        .and_then(|size| size.as_u64())
        .map(format_bytes_value)
        .unwrap_or_else(|| "size unknown".to_owned());
    let access = match source.access() {
        SourceAccess::Direct => "direct".to_owned(),
        SourceAccess::BrowserRequired => "browser required".to_owned(),
        SourceAccess::Manifest => "manifest".to_owned(),
        SourceAccess::Unavailable => source
            .unavailable_reason
            .as_deref()
            .map(|reason| format!("unavailable: {reason}"))
            .unwrap_or_else(|| "unavailable".to_owned()),
    };
    println!(
        "  {id}  provider: {provider}  version: {version}  format: {format}  size: {size}  access: {access}"
    );
    println!(
        "    Artifact title ID: {}  region: {}",
        source.title_id,
        source.region.as_deref().unwrap_or("unknown")
    );
    if let Some(label) = value.get("label").and_then(|label| label.as_str()) {
        if !label.is_empty() {
            println!("    Firmware/release label: {label}");
        }
    }
    for field in ["url", "browser_url", "manifest_url"] {
        if let Some(url) = value.get(field).and_then(|url| url.as_str()) {
            if !url.is_empty() {
                println!("    {field}: {url}");
            }
        }
    }
    Ok(())
}

fn plan_source_command(
    catalog: &SourceConfig,
    title_id: &str,
    release_id: &str,
    out: &Path,
) -> Result<(), CliError> {
    let plan = plan_source(catalog, title_id, release_id).map_err(operation_error)?;
    write_source_plan(out, &plan).map_err(operation_error)?;
    println!("Source plan: {}", out.display());
    let rendered = serde_json::to_string_pretty(&plan)
        .map_err(|error| CliError::Operation(format!("could not format source plan: {error}")))?;
    println!("{rendered}");
    println!("Access: {:?}", plan.access);
    println!("Next step: a future resolver/transport workflow can act on this plan; no files were downloaded.");
    Ok(())
}

fn json_text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_owned()
}

fn json_text_or(value: &serde_json::Value, key: &str, fallback: &str) -> String {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn format_bytes_value(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024_f64 * 1024_f64 * 1024_f64))
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024_f64 * 1024_f64))
    } else {
        format!("{bytes} bytes")
    }
}

fn plan_upload_command(manifest: &Path) -> Result<(), CliError> {
    let plan = plan_upload(manifest).map_err(operation_error)?;
    print_upload_plan(&plan);
    Ok(())
}

fn upload_command(
    manifest: &Path,
    addr: &str,
    engine_url: &str,
    timeout_seconds: u64,
) -> Result<(), CliError> {
    // Build and print the plan before any HTTP request. This repeats the local
    // verification boundary used by `plan-upload`, so an invalid or changed
    // extraction cannot reach the engine.
    let plan = plan_upload(manifest).map_err(operation_error)?;
    print_upload_plan(&plan);
    println!("PS5 address: {addr}");
    println!("PS5Upload engine: {engine_url}");
    println!("Upload timeout: {timeout_seconds} seconds");

    let client = UploadClient::new(engine_url).map_err(operation_error)?;
    let submission = client.submit(&plan, addr).map_err(operation_error)?;
    println!("Engine job ID: {}", submission.engine_job_id);
    // Ensure the job ID is visible immediately even when stdout is redirected.
    io::stdout()
        .flush()
        .map_err(|error| CliError::Operation(format!("could not flush upload status: {error}")))?;

    let result = client
        .poll(
            &plan,
            addr,
            &submission.engine_job_id,
            Duration::from_secs(timeout_seconds),
        )
        .map_err(operation_error)?;
    match result.status {
        UploadPollStatus::CompleteSizeVerified => {
            println!("Upload complete: remote sizes verified");
            println!("Transfer receipt: {}", result.receipt_path.display());
            if let Some(bytes) = result.bytes_sent {
                println!("Bytes sent: {bytes}");
            }
            if let Some(files) = result.files_sent {
                println!("Files sent: {files}");
            }
        }
        UploadPollStatus::Failed => {
            // The current core reports engine failures as an error after writing
            // its durable receipt; retain this arm for future poll adapters.
            return Err(CliError::Operation(format!(
                "engine job {} failed",
                result.engine_job_id
            )));
        }
        UploadPollStatus::TimedOut => {
            return Err(CliError::Operation(format!(
                "timed out waiting for engine job {}; it remains running and was not cancelled",
                result.engine_job_id
            )));
        }
    }
    Ok(())
}

fn print_upload_plan(plan: &ps5_companion::UploadPlan) {
    println!("Upload plan: {}", plan.manifest_path.display());
    println!("Artifact: {}", plan.artifact.path.display());
    println!("Artifact bytes: {}", plan.total_bytes);
    println!("Artifact files: {}", plan.files.len());
    println!("Staging directory: {}", plan.output_root.display());
    println!("PS5 staging root: {}", plan.stage_root);
    println!("PS5 destination root: {}", plan.dest_root);
    if let Some(first) = plan.files.first() {
        let final_target = match &plan.artifact.kind {
            ps5_companion::UploadArtifactKind::Image => first.dest.clone(),
            ps5_companion::UploadArtifactKind::GameFolder => {
                first.dest.split('/').take(4).collect::<Vec<_>>().join("/")
            }
        };
        println!("Final target: {final_target}");
    }
    println!("PS5 destinations:");
    for file in &plan.files {
        println!(
            "  {} (staged at {}; {} bytes)",
            file.dest, file.stage_dest, file.size
        );
    }
}

fn inspect_command(archive: &Path, explicit_seven_zip: Option<&Path>) -> Result<(), CliError> {
    ensure_archive_file(archive)?;
    let seven_zip = resolve_seven_zip(explicit_seven_zip)?;
    let listing = inspect_archive_with_tool(archive, &seven_zip).map_err(operation_error)?;
    let available = available_space(space_probe_path(archive)).map_err(|error| {
        CliError::Operation(format!("could not check available disk space: {error}"))
    })?;
    print_inspection(archive, &seven_zip, &listing, available, "archive volume");
    Ok(())
}

fn extract_command(
    archive: &Path,
    work_dir: &Path,
    explicit_seven_zip: Option<&Path>,
    expected_title_id: Option<String>,
) -> Result<(), CliError> {
    ensure_archive_file(archive)?;
    ensure_directory(work_dir, "work directory")?;
    let seven_zip = resolve_seven_zip(explicit_seven_zip)?;
    println!("Inspecting and extracting archive: {}", archive.display());
    let mut request = ExtractionRequest::new(archive, work_dir, make_job_id(archive));
    request.expected_title_id = expected_title_id;
    request.seven_zip = SevenZipConfig {
        executable: seven_zip.clone(),
        ..SevenZipConfig::default()
    };
    let result = extract_archive(&request).map_err(operation_error)?;
    print_inspection(
        archive,
        &seven_zip,
        &result.listing,
        result.preflight.available_bytes,
        "work directory volume",
    );
    println!("Output directory: {}", result.output_root.display());
    println!("Manifest path: {}", result.manifest_path.display());
    print_artifact(result.artifact.as_ref());
    println!("Verification: complete");
    Ok(())
}

fn print_inspection(
    archive: &Path,
    seven_zip: &Path,
    listing: &ArchiveListing,
    available_bytes: u64,
    available_space_label: &str,
) {
    let archive_bytes = listing
        .archive_bytes
        .or_else(|| fs::metadata(archive).ok().map(|metadata| metadata.len()));
    let required_bytes = listing.total_size.saturating_add(SAFETY_MARGIN_BYTES);
    println!("Archive: {}", archive.display());
    println!("7-Zip: {}", seven_zip.display());
    println!("Archive bytes: {}", format_bytes(archive_bytes));
    println!(
        "Estimated extracted bytes: {}",
        format_bytes(Some(listing.total_size))
    );
    println!("Archive files: {}", listing.file_count);
    println!(
        "Available space ({available_space_label}): {}",
        format_bytes(Some(available_bytes))
    );
    println!(
        "Required free space (estimate + safety margin): {}",
        format_bytes(Some(required_bytes))
    );
    if available_bytes >= required_bytes {
        println!("Space check: sufficient");
    } else {
        println!("Space check: insufficient");
    }
}

fn print_artifact(artifact: Option<&GameArtifact>) {
    match artifact {
        Some(artifact) => {
            println!("Detected artifact: {}", artifact.path.display());
            println!("Artifact kind: {}", artifact_kind_name(artifact.kind));
            if let Some(title_id) = &artifact.title_id {
                println!("Artifact title ID: {title_id}");
            }
            println!("Artifact header verified: {}", artifact.verified_header);
        }
        None => println!("Detected artifact: none"),
    }
}

fn artifact_kind_name(kind: GameArtifactKind) -> &'static str {
    match kind {
        GameArtifactKind::ExfatImage => "exFAT image",
        GameArtifactKind::FfpfscImage => "FFPFSC image",
        GameArtifactKind::FfpfsImage => "FFPFS image",
        GameArtifactKind::FfpkgImage => "FFPKG image",
        GameArtifactKind::GameFolder => "game folder",
    }
}

fn operation_error(error: CompanionError) -> CliError {
    CliError::Operation(error.to_string())
}

fn ensure_archive_file(path: &Path) -> Result<(), CliError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CliError::Operation(format!(
            "could not read archive {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CliError::Operation(format!(
            "archive must be a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_directory(path: &Path, description: &str) -> Result<(), CliError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CliError::Operation(format!(
            "could not read {description} {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CliError::Operation(format!(
            "{description} must be a regular directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn space_probe_path(path: &Path) -> &Path {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return if candidate.is_dir() {
                candidate
            } else {
                candidate.parent().unwrap_or(candidate)
            };
        }
        match candidate.parent() {
            Some(parent) if parent != candidate => candidate = parent,
            _ => return Path::new("."),
        }
    }
}

fn format_bytes(value: Option<u64>) -> String {
    match value {
        Some(bytes) => format!("{bytes} bytes"),
        None => "unknown".to_owned(),
    }
}

fn make_job_id(archive: &Path) -> String {
    let stem = archive
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("archive");
    let mut safe = String::with_capacity(stem.len());
    for character in stem.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            safe.push(character);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        safe.push_str("archive");
    }
    safe.truncate(64);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{safe}-{}-{nanos}", std::process::id())
}

fn resolve_seven_zip(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        return validate_seven_zip(path, "--seven-zip");
    }

    if let Some(path) = find_on_path() {
        return Ok(path);
    }
    for variable in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = env::var_os(variable) {
            let candidate = PathBuf::from(root).join("7-Zip").join("7z.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(CliError::Operation(
        "could not find 7-Zip. Install it, add 7z/7z.exe to PATH, or pass --seven-zip <path>"
            .to_owned(),
    ))
}

fn find_on_path() -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for directory in env::split_paths(&path_var) {
        for name in ["7z.exe", "7z"] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn validate_seven_zip(path: &Path, flag: &str) -> Result<PathBuf, CliError> {
    if !path.is_file() {
        return Err(CliError::Operation(format!(
            "{flag} must point to a 7-Zip executable file: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn parse_args(args: Vec<OsString>) -> Result<CommandLine, CliError> {
    let Some(command) = args.first() else {
        return Err(CliError::Usage(
            "missing command; choose inspect, extract, help, or version".to_owned(),
        ));
    };
    let command = command.to_string_lossy();
    if command == "help" || command == "--help" || command == "-h" {
        if args.len() > 1 {
            return Err(CliError::Usage(
                "help does not accept additional arguments".to_owned(),
            ));
        }
        return Ok(CommandLine::Help);
    }
    if command == "version" || command == "--version" || command == "-V" {
        if args.len() > 1 {
            return Err(CliError::Usage(
                "version does not accept additional arguments".to_owned(),
            ));
        }
        return Ok(CommandLine::Version);
    }
    if !matches!(
        command.as_ref(),
        "inspect" | "extract" | "plan-upload" | "upload" | "search" | "sources" | "plan-source"
    ) {
        return Err(CliError::Usage(format!(
            "unknown command `{command}`; choose inspect, extract, plan-upload, upload, search, sources, plan-source, help, or version"
        )));
    }

    let mut positional = None;
    let mut work_dir = None;
    let mut expected_title_id = None;
    let mut seven_zip = None;
    let mut addr = None;
    let mut engine = None;
    let mut timeout_seconds = None;
    let mut catalog_dir = None;
    let mut catalog_repo = None;
    let mut catalog_ref = None;
    let mut release_id = None;
    let mut out = None;
    let mut show_help = false;
    let mut index = 1;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        match argument.as_ref() {
            "--help" | "-h" => show_help = true,
            "--catalog-dir" => {
                if !is_source_command(&command) {
                    return Err(CliError::Usage(
                        "--catalog-dir is only valid with search, sources, or plan-source"
                            .to_owned(),
                    ));
                }
                if catalog_dir.is_some() {
                    return Err(CliError::Usage(
                        "--catalog-dir may only be provided once".to_owned(),
                    ));
                }
                catalog_dir = Some(next_path(&args, &mut index, "--catalog-dir")?);
            }
            "--catalog-repo" => {
                if !is_source_command(&command) {
                    return Err(CliError::Usage(
                        "--catalog-repo is only valid with search, sources, or plan-source"
                            .to_owned(),
                    ));
                }
                if catalog_repo.is_some() {
                    return Err(CliError::Usage(
                        "--catalog-repo may only be provided once".to_owned(),
                    ));
                }
                catalog_repo = Some(next_text(&args, &mut index, "--catalog-repo")?);
            }
            "--catalog-ref" => {
                if !is_source_command(&command) {
                    return Err(CliError::Usage(
                        "--catalog-ref is only valid with search, sources, or plan-source"
                            .to_owned(),
                    ));
                }
                if catalog_ref.is_some() {
                    return Err(CliError::Usage(
                        "--catalog-ref may only be provided once".to_owned(),
                    ));
                }
                catalog_ref = Some(next_text(&args, &mut index, "--catalog-ref")?);
            }
            "--release" => {
                if command != "plan-source" {
                    return Err(CliError::Usage(
                        "--release is only valid with plan-source".to_owned(),
                    ));
                }
                if release_id.is_some() {
                    return Err(CliError::Usage(
                        "--release may only be provided once".to_owned(),
                    ));
                }
                release_id = Some(next_text(&args, &mut index, "--release")?);
            }
            "--out" => {
                if command != "plan-source" {
                    return Err(CliError::Usage(
                        "--out is only valid with plan-source".to_owned(),
                    ));
                }
                if out.is_some() {
                    return Err(CliError::Usage(
                        "--out may only be provided once".to_owned(),
                    ));
                }
                out = Some(next_path(&args, &mut index, "--out")?);
            }
            "--title-id" => {
                if command != "extract" || expected_title_id.is_some() {
                    return Err(CliError::Usage(
                        "--title-id is allowed once with extract".to_owned(),
                    ));
                }
                let title = next_text(&args, &mut index, "--title-id")?;
                if title.len() != 9
                    || !title.starts_with("PPSA")
                    || !title[4..].bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(CliError::Usage("invalid expected PPSA title ID".to_owned()));
                }
                expected_title_id = Some(title);
            }
            "--work-dir" => {
                if command != "extract" {
                    return Err(CliError::Usage(
                        "--work-dir is only valid with extract".to_owned(),
                    ));
                }
                if work_dir.is_some() {
                    return Err(CliError::Usage(
                        "--work-dir may only be provided once".to_owned(),
                    ));
                }
                work_dir = Some(next_path(&args, &mut index, "--work-dir")?);
            }
            "--seven-zip" => {
                if command != "inspect" && command != "extract" {
                    return Err(CliError::Usage(
                        "--seven-zip is only valid with inspect or extract".to_owned(),
                    ));
                }
                if seven_zip.is_some() {
                    return Err(CliError::Usage(
                        "--seven-zip may only be provided once".to_owned(),
                    ));
                }
                seven_zip = Some(next_path(&args, &mut index, "--seven-zip")?);
            }
            "--addr" => {
                if command != "upload" {
                    return Err(CliError::Usage(
                        "--addr is only valid with upload".to_owned(),
                    ));
                }
                if addr.is_some() {
                    return Err(CliError::Usage(
                        "--addr may only be provided once".to_owned(),
                    ));
                }
                let value = next_text(&args, &mut index, "--addr")?;
                if value.parse::<IpAddr>().is_err() {
                    return Err(CliError::Usage(format!(
                        "--addr must be a valid IPv4 or IPv6 address: {value}"
                    )));
                }
                addr = Some(value);
            }
            "--engine" => {
                if command != "upload" {
                    return Err(CliError::Usage(
                        "--engine is only valid with upload".to_owned(),
                    ));
                }
                if engine.is_some() {
                    return Err(CliError::Usage(
                        "--engine may only be provided once".to_owned(),
                    ));
                }
                engine = Some(next_text(&args, &mut index, "--engine")?);
            }
            "--timeout-seconds" => {
                if command != "upload" {
                    return Err(CliError::Usage(
                        "--timeout-seconds is only valid with upload".to_owned(),
                    ));
                }
                if timeout_seconds.is_some() {
                    return Err(CliError::Usage(
                        "--timeout-seconds may only be provided once".to_owned(),
                    ));
                }
                let value = next_text(&args, &mut index, "--timeout-seconds")?;
                let seconds = value.parse::<u64>().map_err(|_| {
                    CliError::Usage(format!(
                        "--timeout-seconds must be an integer from 1 to {MAX_UPLOAD_TIMEOUT_SECONDS}"
                    ))
                })?;
                if !(1..=MAX_UPLOAD_TIMEOUT_SECONDS).contains(&seconds) {
                    return Err(CliError::Usage(format!(
                        "--timeout-seconds must be an integer from 1 to {MAX_UPLOAD_TIMEOUT_SECONDS}"
                    )));
                }
                timeout_seconds = Some(seconds);
            }
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!("unknown flag `{value}`")));
            }
            _ => {
                if positional.is_some() {
                    let message = if command == "search" {
                        "search accepts one query argument; quote multi-word queries, for example `search \"Outer Wilds\"`"
                    } else if command == "sources" || command == "plan-source" {
                        "only one title ID may be provided"
                    } else {
                        "only one archive or manifest path may be provided"
                    };
                    return Err(CliError::Usage(message.to_owned()));
                }
                positional = Some(PathBuf::from(&args[index]));
            }
        }
        index += 1;
    }

    if show_help {
        return Ok(CommandLine::Help);
    }
    let positional = positional.ok_or_else(|| match command.as_ref() {
        "plan-upload" | "upload" => {
            CliError::Usage(format!("{command} requires a manifest path as <manifest>"))
        }
        "search" => CliError::Usage(
            "search requires a query argument as <query>; quote multi-word queries".to_owned(),
        ),
        "sources" | "plan-source" => {
            CliError::Usage(format!("{command} requires a title ID as <title-id>"))
        }
        _ => CliError::Usage(format!("{command} requires an archive path as <archive>")),
    })?;
    match command.as_ref() {
        "inspect" => {
            if work_dir.is_some() {
                return Err(CliError::Usage(
                    "inspect does not accept --work-dir".to_owned(),
                ));
            }
            if addr.is_some() || engine.is_some() || timeout_seconds.is_some() {
                return Err(CliError::Usage(
                    "upload options require the upload command".to_owned(),
                ));
            }
            Ok(CommandLine::Inspect {
                archive: positional,
                seven_zip,
            })
        }
        "extract" => {
            if addr.is_some() || engine.is_some() || timeout_seconds.is_some() {
                return Err(CliError::Usage(
                    "upload options require the upload command".to_owned(),
                ));
            }
            Ok(CommandLine::Extract {
                archive: positional,
                work_dir: work_dir.ok_or_else(|| {
                    CliError::Usage("extract requires --work-dir <directory>".to_owned())
                })?,
                seven_zip,
                expected_title_id,
            })
        }
        "plan-upload" => {
            if work_dir.is_some()
                || seven_zip.is_some()
                || addr.is_some()
                || engine.is_some()
                || timeout_seconds.is_some()
            {
                return Err(CliError::Usage(
                    "plan-upload accepts only a manifest path".to_owned(),
                ));
            }
            Ok(CommandLine::PlanUpload {
                manifest: positional,
            })
        }
        "upload" => {
            if work_dir.is_some() || seven_zip.is_some() {
                return Err(CliError::Usage(
                    "upload accepts only --addr, --engine, and --timeout-seconds options"
                        .to_owned(),
                ));
            }
            Ok(CommandLine::Upload {
                manifest: positional,
                addr: addr
                    .ok_or_else(|| CliError::Usage("upload requires --addr <ps5-ip>".to_owned()))?,
                engine: engine.unwrap_or_else(|| DEFAULT_ENGINE_URL.to_owned()),
                timeout_seconds: timeout_seconds.unwrap_or(DEFAULT_UPLOAD_TIMEOUT_SECONDS),
            })
        }
        "search" | "sources" | "plan-source" => {
            if work_dir.is_some()
                || seven_zip.is_some()
                || addr.is_some()
                || engine.is_some()
                || timeout_seconds.is_some()
            {
                return Err(CliError::Usage(
                    "sourcing commands accept only catalog options (and --release/--out for plan-source)"
                        .to_owned(),
                ));
            }
            if catalog_dir.is_some() && catalog_repo.is_some() {
                return Err(CliError::Usage(
                    "--catalog-dir and --catalog-repo are conflicting; choose one catalog source"
                        .to_owned(),
                ));
            }
            let mut catalog = SourceConfig::default();
            catalog.catalog_dir = catalog_dir;
            if let Some(repository) = catalog_repo {
                catalog.repository = repository;
            }
            if let Some(reference) = catalog_ref {
                catalog.reference = reference;
            }
            match command.as_ref() {
                "search" => {
                    if release_id.is_some() || out.is_some() {
                        return Err(CliError::Usage(
                            "search does not accept --release or --out".to_owned(),
                        ));
                    }
                    Ok(CommandLine::Search {
                        query: positional.to_string_lossy().into_owned(),
                        catalog,
                    })
                }
                "sources" => {
                    if release_id.is_some() || out.is_some() {
                        return Err(CliError::Usage(
                            "sources does not accept --release or --out".to_owned(),
                        ));
                    }
                    Ok(CommandLine::Sources {
                        title_id: positional.to_string_lossy().into_owned(),
                        catalog,
                    })
                }
                "plan-source" => Ok(CommandLine::PlanSource {
                    title_id: positional.to_string_lossy().into_owned(),
                    release_id: release_id.ok_or_else(|| {
                        CliError::Usage("plan-source requires --release <release-id>".to_owned())
                    })?,
                    out: out.ok_or_else(|| {
                        CliError::Usage("plan-source requires --out <plan.json>".to_owned())
                    })?,
                    catalog,
                }),
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
}

fn is_source_command(command: &str) -> bool {
    matches!(command, "search" | "sources" | "plan-source")
}

fn next_text(args: &[OsString], index: &mut usize, flag: &str) -> Result<String, CliError> {
    *index += 1;
    let Some(value) = args.get(*index) else {
        return Err(CliError::Usage(format!("{flag} requires a value")));
    };
    let value = value.to_string_lossy();
    if value.is_empty() || value.starts_with('-') {
        return Err(CliError::Usage(format!("{flag} requires a value")));
    }
    Ok(value.into_owned())
}

fn next_path(args: &[OsString], index: &mut usize, flag: &str) -> Result<PathBuf, CliError> {
    *index += 1;
    let Some(value) = args.get(*index) else {
        return Err(CliError::Usage(format!("{flag} requires a path value")));
    };
    if value.to_string_lossy().is_empty() || value.to_string_lossy().starts_with('-') {
        return Err(CliError::Usage(format!("{flag} requires a path value")));
    }
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_inspect_and_optional_tool() {
        let parsed = parse_args(args(&[
            "inspect",
            "archive.7z",
            "--seven-zip",
            "tools/7z.exe",
        ]))
        .expect("valid inspect command");
        assert_eq!(
            parsed,
            CommandLine::Inspect {
                archive: PathBuf::from("archive.7z"),
                seven_zip: Some(PathBuf::from("tools/7z.exe")),
            }
        );
    }

    #[test]
    fn rejects_unknown_flags_with_the_flag_name() {
        let error = parse_args(args(&["inspect", "archive.7z", "--wat"]))
            .expect_err("unknown flag should fail")
            .to_string();
        assert!(error.contains("unknown flag `--wat`"));
    }

    #[test]
    fn rejects_unknown_options_for_extract() {
        let error = parse_args(args(&[
            "extract",
            "archive.7z",
            "--work-dir",
            "stage",
            "--wat",
        ]))
        .expect_err("unknown option should fail")
        .to_string();
        assert!(error.contains("unknown flag `--wat`"));
    }

    #[test]
    fn rejects_extract_without_work_directory() {
        let error = parse_args(args(&["extract", "archive.7z"]))
            .expect_err("missing work directory should fail")
            .to_string();
        assert!(error.contains("extract requires --work-dir <directory>"));
    }

    #[test]
    fn extract_accepts_one_valid_expected_title_only() {
        let parsed = parse_args(args(&[
            "extract",
            "archive.zip",
            "--work-dir",
            "stage",
            "--title-id",
            "PPSA00001",
        ]))
        .unwrap();
        assert!(
            matches!(parsed, CommandLine::Extract { expected_title_id: Some(ref title), .. } if title == "PPSA00001")
        );
        for values in [
            vec![
                "extract",
                "archive.zip",
                "--work-dir",
                "stage",
                "--title-id",
                "../bad",
            ],
            vec!["inspect", "archive.zip", "--title-id", "PPSA00001"],
            vec![
                "extract",
                "archive.zip",
                "--work-dir",
                "stage",
                "--title-id",
                "PPSA00001",
                "--title-id",
                "PPSA99999",
            ],
        ] {
            assert!(parse_args(args(&values)).is_err());
        }
    }

    #[test]
    fn rejects_missing_option_values() {
        let error = parse_args(args(&["inspect", "archive.7z", "--seven-zip"]))
            .expect_err("missing seven-zip path should fail")
            .to_string();
        assert!(error.contains("--seven-zip requires a path value"));
    }

    #[test]
    fn rejects_missing_commands_and_archive_paths() {
        let error = parse_args(Vec::new())
            .expect_err("missing command should fail")
            .to_string();
        assert!(error.contains("missing command"));

        let error = parse_args(args(&["inspect"]))
            .expect_err("missing archive should fail")
            .to_string();
        assert!(error.contains("inspect requires an archive path"));
    }

    #[test]
    fn parses_upload_options_and_defaults() {
        let parsed = parse_args(args(&[
            "upload",
            "manifest.json",
            "--addr",
            "192.168.1.214",
            "--timeout-seconds",
            "7200",
        ]))
        .expect("valid upload command");
        assert_eq!(
            parsed,
            CommandLine::Upload {
                manifest: PathBuf::from("manifest.json"),
                addr: "192.168.1.214".to_owned(),
                engine: DEFAULT_ENGINE_URL.to_owned(),
                timeout_seconds: 7200,
            }
        );
    }

    #[test]
    fn rejects_invalid_upload_address() {
        let error = parse_args(args(&["upload", "manifest.json", "--addr", "not-an-ip"]))
            .expect_err("invalid address should fail")
            .to_string();
        assert!(error.contains("--addr must be a valid IPv4 or IPv6 address"));
    }

    #[test]
    fn rejects_upload_without_address() {
        let error = parse_args(args(&["upload", "manifest.json"]))
            .expect_err("missing address should fail")
            .to_string();
        assert!(error.contains("upload requires --addr <ps5-ip>"));
    }

    #[test]
    fn rejects_ambiguous_command_paths() {
        let error = parse_args(args(&["plan-upload", "one.json", "two.json"]))
            .expect_err("multiple manifests should fail")
            .to_string();
        assert!(error.contains("only one archive or manifest path may be provided"));
    }

    #[test]
    fn rejects_unbounded_upload_timeout() {
        let error = parse_args(args(&[
            "upload",
            "manifest.json",
            "--addr",
            "192.168.1.214",
            "--timeout-seconds",
            "86401",
        ]))
        .expect_err("timeout above the bound should fail")
        .to_string();
        assert!(error.contains("--timeout-seconds must be an integer from 1 to 86400"));
    }

    #[test]
    fn generated_job_ids_fit_the_library_limit() {
        let archive = PathBuf::from(format!("{}.7z", "a".repeat(256)));
        let id = make_job_id(&archive);
        assert!(id.len() <= 128);
        assert!(id.starts_with(&"a".repeat(64)));
    }

    #[test]
    fn parses_quoted_multiword_search_and_catalog_defaults() {
        let parsed = parse_args(args(&["search", "Outer Wilds"]))
            .expect("quoted multi-word query should parse as one argument");
        assert_eq!(
            parsed,
            CommandLine::Search {
                query: "Outer Wilds".to_owned(),
                catalog: SourceConfig::default(),
            }
        );
    }

    #[test]
    fn parses_offline_catalog_options_for_all_source_commands() {
        let parsed = parse_args(args(&["sources", "PPSA00001", "--catalog-dir", "data"]))
            .expect("offline source lookup should parse");
        let CommandLine::Sources { title_id, catalog } = parsed else {
            panic!("expected sources command");
        };
        assert_eq!(title_id, "PPSA00001");
        assert_eq!(catalog.catalog_dir, Some(PathBuf::from("data")));

        let parsed = parse_args(args(&[
            "plan-source",
            "PPSA00001",
            "--release",
            "release-1",
            "--out",
            "plan.json",
            "--catalog-repo",
            "example/catalog",
            "--catalog-ref",
            "abc123",
        ]))
        .expect("remote source plan options should parse");
        let CommandLine::PlanSource {
            title_id,
            release_id,
            out,
            catalog,
        } = parsed
        else {
            panic!("expected plan-source command");
        };
        assert_eq!(title_id, "PPSA00001");
        assert_eq!(release_id, "release-1");
        assert_eq!(out, PathBuf::from("plan.json"));
        assert_eq!(catalog.repository, "example/catalog");
        assert_eq!(catalog.reference, "abc123");
    }

    #[test]
    fn rejects_ambiguous_unquoted_search_terms() {
        let error = parse_args(args(&["search", "Outer", "Wilds"]))
            .expect_err("separate query terms should be rejected")
            .to_string();
        assert!(error.contains("quote multi-word queries"), "{error}");
    }

    #[test]
    fn rejects_duplicate_and_conflicting_catalog_options() {
        let error = parse_args(args(&[
            "search",
            "Outer Wilds",
            "--catalog-ref",
            "main",
            "--catalog-ref",
            "stable",
        ]))
        .expect_err("duplicate catalog ref should fail")
        .to_string();
        assert!(
            error.contains("--catalog-ref may only be provided once"),
            "{error}"
        );

        let error = parse_args(args(&[
            "search",
            "Outer Wilds",
            "--catalog-dir",
            "data",
            "--catalog-repo",
            "example/catalog",
        ]))
        .expect_err("local and remote catalog sources should conflict")
        .to_string();
        assert!(error.contains("conflicting"), "{error}");
    }

    #[test]
    fn rejects_source_options_for_non_source_commands() {
        let error = parse_args(args(&["inspect", "archive.7z", "--catalog-dir", "data"]))
            .expect_err("catalog options should be rejected by inspect")
            .to_string();
        assert!(error.contains("--catalog-dir is only valid"), "{error}");

        let error = parse_args(args(&["search", "Outer Wilds", "--release", "release-1"]))
            .expect_err("release option should be rejected by search")
            .to_string();
        assert!(
            error.contains("--release is only valid with plan-source"),
            "{error}"
        );
    }

    #[test]
    fn rejects_incomplete_source_plan_options() {
        let error = parse_args(args(&[
            "plan-source",
            "PPSA00001",
            "--release",
            "release-1",
        ]))
        .expect_err("source plan output path is required")
        .to_string();
        assert!(
            error.contains("plan-source requires --out <plan.json>"),
            "{error}"
        );

        let error = parse_args(args(&["plan-source", "PPSA00001", "--out", "plan.json"]))
            .expect_err("source plan release is required")
            .to_string();
        assert!(
            error.contains("plan-source requires --release <release-id>"),
            "{error}"
        );
    }
}
