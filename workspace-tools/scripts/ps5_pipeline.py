"""Orchestrate the existing archive validation and staged PS5Upload commands."""
import json
import os
import re
import shutil
import subprocess
from pathlib import Path

from pragmata_transfer import TransferError, _atomic_json, _sha256_file

IMAGE_SUFFIXES = {".exfat", ".ffpfsc", ".ffpfs", ".ffpkg"}


def command(cli, arguments):
    print(f"\nStage: {arguments[0]}", flush=True)
    result = subprocess.run([str(cli), *map(str, arguments)], capture_output=True,
                            encoding="utf-8", errors="replace")
    if result.stdout:
        print(result.stdout, end="", flush=True)
    if result.stderr:
        print(result.stderr, end="", flush=True)
    if result.returncode:
        raise TransferError(f"{arguments[0]} failed; later stages were not started")
    return result.stdout


def metadata_title(value):
    if isinstance(value, dict):
        for key in ("titleId", "titleID", "title_id", "titleid", "TITLE_ID"):
            if isinstance(value.get(key), str):
                return value[key]
        for child in value.values():
            found = metadata_title(child)
            if found:
                return found
    if isinstance(value, list):
        for child in value:
            found = metadata_title(child)
            if found:
                return found
    return None


def verify_artifact_title(manifest_path, title_id):
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("state") != "complete":
        raise TransferError("extraction manifest is not complete")
    root = Path(manifest["output_root"])
    files = manifest["files"]
    selected = manifest.get("selected_artifact")
    if selected is not None:
        relative = Path(selected)
        if relative.is_absolute() or ".." in relative.parts or "\\" in selected:
            raise TransferError("unsafe selected artifact path")
        # Rust reselects and verifies the complete manifest at plan/upload boundaries.
        if relative.suffix.lower() in IMAGE_SUFFIXES:
            files = [f for f in files if f["path"] == selected]
        elif selected:
            files = [f for f in files if f["path"].startswith(selected + "/")]
    images = [file for file in files if Path(file["path"]).suffix.lower() in IMAGE_SUFFIXES]
    folders = [file for file in files if file["path"].replace("\\", "/").lower().endswith("/sce_sys/param.json") or file["path"].lower() == "sce_sys/param.json"]
    if len(images) == 1 and not folders:
        ids = set(re.findall(r"PPSA[A-Z0-9]+", Path(images[0]["path"]).name.upper().split(".")[0]))
        if ids != {title_id}:
            raise TransferError("image filename does not unambiguously match the selected title ID")
    elif len(folders) == 1 and not images:
        param = root / folders[0]["path"]
        if param.stat().st_size > 1024 * 1024:
            raise TransferError("game metadata exceeds 1 MiB")
        if metadata_title(json.loads(param.read_text(encoding="utf-8-sig"))) != title_id:
            raise TransferError("extracted game metadata does not match the selected title ID")
        if not (param.parent.parent / "eboot.bin").is_file():
            raise TransferError("game folder is missing eboot.bin")
    else:
        raise TransferError("expected exactly one game image or one game folder; refusing ambiguous/patch-only contents")


def prepare_image_manifest(source, job_dir, title_id):
    # The existing uploader requires a regular job/staging directory. Hard links
    # avoid duplicating a large image; filesystems without them use a bounded copy.
    staging = job_dir / "staging"
    staging.mkdir(exist_ok=True)
    staged = staging / source.name
    if not staged.exists():
        try:
            os.link(source, staged)
        except OSError:
            if shutil.disk_usage(staging).free < source.stat().st_size + 64 * 1024 * 1024:
                raise TransferError("not enough space to stage this image on a filesystem without hard links")
            with source.open("rb") as original, staged.open("xb") as target:
                shutil.copyfileobj(original, target, 1024 * 1024)
                target.flush()
                os.fsync(target.fileno())
    print("Hashing the image for its local upload manifest...", flush=True)
    digest = _sha256_file(source)
    staged_digest = digest if staged.samefile(source) else _sha256_file(staged)
    if staged.is_symlink() or staged.stat().st_size != source.stat().st_size or staged_digest != digest:
        raise TransferError("staged image differs from the downloaded image; existing files retained")
    files = [{"path": staged.name, "size": staged.stat().st_size, "sha256": staged_digest}]
    manifest_path = job_dir / "manifest.json"
    if manifest_path.exists():
        raise TransferError("image manifest already exists; refusing to replace previous job state")
    _atomic_json(manifest_path, {"schema_version": 1, "job_id": job_dir.name,
                                "state": "complete", "input_archive_path": str(source.resolve()),
                                "output_root": str(staging.resolve()), "files": files,
                                "artifact_title": title_id})
    return manifest_path


def run_pipeline(title_id, directory, config):
    cli = Path(config["cli"])
    if not cli.is_file():
        raise TransferError(f"archive/upload CLI missing: {cli}")
    job_dir = directory.parent / ".pipeline" / directory.name
    job_dir.mkdir(parents=True, exist_ok=True)
    state_path = job_dir / "pipeline-state.json"
    state = json.loads(state_path.read_text(encoding="utf-8")) if state_path.exists() else {}
    if state.get("upload_started"):
        raise TransferError(f"this pipeline already attempted an upload; inspect its transfer receipt before retrying: {state_path}")
    if state.get("artifact_title_id", state.get("title_id", title_id)) != title_id:
        raise TransferError("existing pipeline belongs to a different artifact title ID")
    state.update({"title_id": title_id, "stage": "inspecting", "ps5_addr": config["addr"]})
    state.pop("error", None)
    _atomic_json(state_path, state)
    try:
        receipt = json.loads((directory / "download-receipt.json").read_text(encoding="utf-8"))
        if receipt.get("title_id", title_id) != title_id or receipt.get("artifact_title_id", title_id) != title_id:
            raise TransferError("download receipt identity does not match the selected artifact title ID")
        state.update({"listing_title_id": receipt.get("listing_title_id", title_id),
                      "artifact_title_id": title_id})
        _atomic_json(state_path, state)
        inputs = [Path(item["path"]) for item in receipt["files"]]
        if not inputs or any(not path.is_file() for path in inputs):
            raise TransferError("download receipt is missing one or more input files")
        for item, path in zip(receipt["files"], inputs):
            if path.stat().st_size != item["bytes"] or _sha256_file(path) != item["sha256"]:
                raise TransferError("downloaded input changed since its receipt was written")
        source = inputs[0]
        manifest_path = Path(state["manifest"]) if state.get("manifest") else None
        if manifest_path is None:
            if source.suffix.lower() in IMAGE_SUFFIXES:
                if len(inputs) != 1:
                    raise TransferError("multiple image inputs are ambiguous")
                manifest_path = prepare_image_manifest(source, job_dir, title_id)
            else:
                command(cli, ["inspect", source, "--seven-zip", config["seven_zip"]])
                state["stage"] = "extracting"
                _atomic_json(state_path, state)
                work = job_dir / "work"
                work.mkdir(exist_ok=True)
                output = command(cli, ["extract", source, "--work-dir", work, "--seven-zip", config["seven_zip"], "--title-id", title_id])
                paths = re.findall(r"^Manifest path: (.+)$", output, re.M)
                if len(paths) != 1:
                    raise TransferError("extract command did not report one manifest path")
                manifest_path = Path(paths[0].strip())
            state["manifest"] = str(manifest_path)
            state["stage"] = "verifying"
            _atomic_json(state_path, state)
        verify_artifact_title(manifest_path, title_id)
        # plan-upload rehashes the extracted files and checks image headers locally.
        command(cli, ["plan-upload", manifest_path])
        state.update({"stage": "uploading", "upload_started": True})
        _atomic_json(state_path, state)
        command(cli, ["upload", manifest_path, "--addr", config["addr"], "--engine", config["engine"]])
        transfer_path = manifest_path.parent / "transfer-receipt.json"
        transfer = json.loads(transfer_path.read_text(encoding="utf-8"))
        if transfer.get("status") != "complete_size_verified" or transfer.get("addr") != config["addr"]:
            raise TransferError("upload command did not produce a complete size-verified transfer receipt")
        state.update({"stage": "complete_size_verified", "transfer_receipt": str(transfer_path)})
        _atomic_json(state_path, state)
        print("Pipeline complete: uploaded to PS5 and remote sizes verified. Game launch was not tested.", flush=True)
    except (TransferError, OSError, ValueError) as exc:
        if state.get("manifest") and not (Path(state["manifest"]).parent / "upload-attempt.json").exists():
            state["upload_started"] = False
        state.update({"stage": "failed", "error": str(exc)})
        _atomic_json(state_path, state)
        raise
