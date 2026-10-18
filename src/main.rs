use fs2::available_space;
use ps5_companion::{
    extract_archive, inspect_archive_with_tool, plan_upload, ArchiveListing, CompanionError,
    ExtractionRequest, GameArtifact, GameArtifactKind, SevenZipConfig, UploadClient,
    UploadPollStatus,
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
  ps5-companion extract <archive> --work-dir <directory> [--seven-zip <path>]
  ps5-companion plan-upload <manifest>
  ps5-companion upload <manifest> --addr <ps5-ip> [--engine <loopback-url>]
      [--timeout-seconds <1..86400>]
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
        } => extract_command(&archive, &work_dir, seven_zip.as_deref())?,
        CommandLine::PlanUpload { manifest } => plan_upload_command(&manifest)?,
        CommandLine::Upload {
            manifest,
            addr,
            engine,
            timeout_seconds,
        } => upload_command(&manifest, &addr, &engine, timeout_seconds)?,
    }
    Ok(())
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
) -> Result<(), CliError> {
    ensure_archive_file(archive)?;
    ensure_directory(work_dir, "work directory")?;
    let seven_zip = resolve_seven_zip(explicit_seven_zip)?;
    println!("Inspecting and extracting archive: {}", archive.display());
    let mut request = ExtractionRequest::new(archive, work_dir, make_job_id(archive));
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
        "inspect" | "extract" | "plan-upload" | "upload"
    ) {
        return Err(CliError::Usage(format!(
            "unknown command `{command}`; choose inspect, extract, plan-upload, upload, help, or version"
        )));
    }

    let mut positional = None;
    let mut work_dir = None;
    let mut seven_zip = None;
    let mut addr = None;
    let mut engine = None;
    let mut timeout_seconds = None;
    let mut show_help = false;
    let mut index = 1;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        match argument.as_ref() {
            "--help" | "-h" => show_help = true,
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
                    return Err(CliError::Usage(
                        "only one archive or manifest path may be provided".to_owned(),
                    ));
                }
                positional = Some(PathBuf::from(&args[index]));
            }
        }
        index += 1;
    }

    if show_help {
        return Ok(CommandLine::Help);
    }
    let positional = positional.ok_or_else(|| {
        let (article, value) = if command == "plan-upload" || command == "upload" {
            ("a", "manifest")
        } else {
            ("an", "archive")
        };
        CliError::Usage(format!(
            "{command} requires {article} {value} path as <{value}>"
        ))
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
        _ => unreachable!(),
    }
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
}
