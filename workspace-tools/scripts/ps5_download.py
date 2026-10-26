"""Catalog-to-file wrapper around the existing resumable Python downloader."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import webbrowser
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urljoin, urlparse
from urllib.request import Request, urlopen
from urllib.error import HTTPError

from pragmata_transfer import (
    TransferError, USER_AGENT, download_direct, looks_like_html, parse_content_range,
    probe_url, record_url, source_kind, validate_url, reject_windows_download, _atomic_json, _sha256_file, _check_sidecar, _validate_digest,
)

ROOT = Path(__file__).resolve().parents[1]
PAGE_LIMIT = 1024 * 1024
PART = re.compile(r"\bpart\s*(\d+)\b", re.I)
SPACE_RESERVE = 64 * 1024 * 1024


def artifact_identity(listing_id, group, *, package_only=False):
    """Bind every part to one edition; never infer identity from a URL or label."""
    pattern = r"(?:PPSA|CUSA)\d{5}" if package_only else r"PPSA\d{5}"
    if not re.fullmatch(pattern, listing_id):
        raise TransferError("invalid PS5 listing title ID")
    ids = set()
    for record in group:
        title_id = record.get("titleId", listing_id)
        if not isinstance(title_id, str) or not re.fullmatch(pattern, title_id):
            raise TransferError("source has an invalid or cross-platform artifact title ID")
        platform = "ps5" if listing_id.startswith("PPSA") else "ps4"
        if title_id[:4] != listing_id[:4] or ("platform" in record and str(record["platform"]).lower() != platform):
            raise TransferError("source platform contradicts its PS5 artifact title ID")
        ids.add(title_id)
    if len(ids) != 1:
        raise TransferError("source parts do not identify one unambiguous edition")
    return ids.pop()


def is_package(record):
    return str(record.get('artifactFormat') or record.get('format', '')).lower() in {'pkg', 'fpkg'}


def source_exclusion(record, *, include_mediafire=False, package_only=False):
    if package_only and not is_package(record):
        return 'package-only selection excludes images and game folders'
    if package_only and (str(record.get('format', '')).lower() not in {'pkg', 'fpkg'} or record.get('compression')):
        return 'archived/split packages are not supported yet; select a single uncompressed PKG'
    if not package_only and str(record.get('artifactFormat') or record.get('format', '')).lower() in {'pkg', 'fpkg'}:
        return 'PKG installation is outside the current validated image/folder pipeline'
    if record.get('archivePassword'):
        return 'password-protected extraction is not supported by the current pipeline'
    if record.get("unavailableReason"):
        return str(record["unavailableReason"])
    if str(record.get("health", "")).lower() in {"dead", "offline", "unavailable"}:
        return "catalog marks this source unavailable"
    host = (urlparse(str(record_url(record) or "")).hostname or "").lower()
    if not include_mediafire and (host == "mediafire.com" or host.endswith(".mediafire.com")
                                 or str(record.get("provider", "")).lower() == "mediafire"):
        return "MediaFire catalog sources disabled by default after widespread removals; use --include-mediafire to opt in"
    return None


def supplemental(record, resolved_name=""):
    text = " ".join(str(record.get(key, "")) for key in ("label", "filename")) + " " + resolved_name
    base = re.search(r"\bbase\b|\bfull[ -]?game\b", text, re.I)
    return (record.get("category") in {"dlc", "patch", "update"}
            or bool(re.search(r"\bonly\s+dlcs?\b|\b(?:patch|update)[ -]?only\b", text, re.I))
            or (not base and bool(re.search(r"\bdlcs?\b|\bback[ -]?port\b|\bpatch\b|\bupdate\b", text, re.I))))


class BrowserRequired(TransferError):
    """The source may need a user-completed browser session."""


def request_browser_url(record):
    page = validate_url(record_url(record) or "")
    print(f"\nUnverified catalog page: {page}", flush=True)
    print("This page has not been confirmed to provide a working download.", flush=True)
    try:
        if input("Open this source in your browser? [Y/n] ").strip().lower() not in {"n", "no"}:
            if not webbrowser.open(page):
                print("Could not open the browser automatically; open the URL above.", flush=True)
        print("Complete verification in the browser. Start the file download, then copy its download link.", flush=True)
        print("Paste the final file URL below. Press Enter to skip this source.", flush=True)
        print("If the URL needs browser cookies, finish the download in the browser instead.", flush=True)
        while True:
            value = input("Final download URL: ").strip()
            if not value:
                return None
            try:
                validate_url(value)
                reject_windows_download(value)
                if value == page:
                    raise TransferError("this is the source page; paste the final file URL")
                return value
            except TransferError as exc:
                print(f"Invalid download URL: {exc}", flush=True)
    except EOFError:
        return None


class DownloadLink(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.link = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag.lower() == "a" and attrs.get("id", "").lower() in {"downloadbutton", "download-link"}:
            self.link = attrs.get("href")


def resolve(record):
    url = validate_url(record_url(record) or "")
    kind = source_kind(record)
    if kind == "direct":
        return url
    if kind not in {"mediafire", "vikingfile"}:
        raise BrowserRequired(f"{record.get('provider', kind)} needs browser verification or a final file URL")
    try:
        response = urlopen(Request(url, headers={"User-Agent": USER_AGENT}), timeout=30)
    except HTTPError as exc:
        if exc.code in {401, 403}:
            raise BrowserRequired(f"provider returned HTTP {exc.code}; browser verification may be required") from exc
        raise
    try:
        final_url = validate_url(response.geturl())
        raw = response.read(PAGE_LIMIT + 1)
    finally:
        response.close()
    if len(raw) > PAGE_LIMIT:
        raise TransferError("provider landing page exceeds the 1 MiB limit")
    parser = DownloadLink()
    parser.feed(raw.decode("utf-8", errors="replace"))
    if not parser.link:
        raise BrowserRequired("provider page has no download link; browser verification may be required")
    return validate_url(urljoin(final_url, parser.link))


def candidates(records, *, include_mediafire=False, package_only=False):
    singles, groups = [], {}
    for record in records:
        if not isinstance(record, dict):
            continue
        label = str(record.get("label", ""))
        eligible = not source_exclusion(record, include_mediafire=include_mediafire, package_only=package_only) and not supplemental(record)
        marker = PART.search(str(record.get("volume", ""))) or PART.search(label)
        if marker:
            key = (record.get("provider"), PART.sub("", label).strip(), record.get("version"), record.get("source"), record.get("record"), record.get("titleId"), record.get("platform"), record.get("region"))
            groups.setdefault(key, []).append((int(marker.group(1)), record, eligible))
        elif eligible:
            singles.append([record])
    for parts in ([] if package_only else groups.values()):
        parts.sort(key=lambda pair: pair[0])
        if (all(eligible for _, _, eligible in parts)
                and [number for number, _, _ in parts] == list(range(1, len(parts) + 1))
                and len(parts) > 1):
            singles.append([record for _, record, _ in parts])
    def rank(group):
        try:
            kind = source_kind(group[0])
        except TransferError:
            return (99, 1)
        # Access method is not proof of availability. Catalog health only breaks
        # ties; every selected source must still pass a bounded live probe.
        return ({"direct": 0, "vikingfile": 1, "mediafire": 3}.get(kind, 2),
                int(any(record.get("health") == "flaky" for record in group)))
    return sorted(singles, key=rank)


def filename(url, record):
    name = unquote(urlparse(url).path.rsplit("/", 1)[-1])
    if not name or "." not in name:
        name = str(record.get("filename") or "download.bin")
    name = re.sub(r'[<>:"/\\|?*\x00-\x1f]', "_", name).rstrip(" .")[:200]
    if not name or name in {".", ".."} or name.split(".")[0].upper() in {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1,10)), *(f"LPT{i}" for i in range(1,10))}:
        raise TransferError("provider supplied an unsafe filename")
    return name


def progress(done, total):
    suffix = f" / {total / 2**30:.2f} GiB ({done / total:.1%})" if total else " GiB"
    print(f"  {done / 2**30:.2f}{suffix}", flush=True)


def prepare_part(record, directory, *, direct_url=None):
    expected_digest = _validate_digest(record.get("sha256") or None)
    url = validate_url(direct_url) if direct_url else resolve(record)
    destination = directory / filename(url, record)
    if supplemental(record, destination.name):
        raise TransferError("source appears to contain a backport patch, update, or DLC rather than the base game")
    reject_windows_download(url, filename=destination.name)
    receipt_path = Path(str(destination) + ".sha256.json")
    if destination.exists():
        with destination.open("rb") as existing:
            reject_windows_download(url, filename=destination.name, prefix=existing.read(64))
        if receipt_path.exists():
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            if receipt.get("record") == record and receipt.get("bytes") == destination.stat().st_size and receipt.get("sha256") == _sha256_file(destination) and (expected_digest is None or receipt.get("sha256") == expected_digest):
                print(f"Already downloaded and verified: {destination}", flush=True)
                return {"receipt": receipt, "destination": destination, "remaining": 0}
        raise TransferError(f"existing file is not verified for this source; preserving {destination}")
    try:
        probe = probe_url(url)
    except TransferError as exc:
        if not direct_url and re.search(r"HTTP Error (401|403)\b", str(exc)):
            raise BrowserRequired(str(exc)) from exc
        raise
    if probe["is_html"] or probe["status"] not in {200, 206}:
        raise BrowserRequired("source returned a browser page instead of file data")
    parsed = parse_content_range(probe.get("content_range"))
    expected = parsed[2] if parsed else probe.get("content_length")
    if probe["status"] == 206 and (not parsed or parsed[0] != 0 or expected is None):
        raise TransferError("source probe returned an invalid range or unknown total size")
    if not expected or expected <= 0:
        raise TransferError("source did not advertise a positive file size")
    partial = Path(str(destination) + ".partial")
    sidecar = Path(str(partial) + ".meta.json")
    retained = partial.stat().st_size if partial.exists() else 0
    if retained or sidecar.exists():
        _check_sidecar(sidecar, url=url, expected_size=expected, sha256=expected_digest)
    if retained > expected:
        raise TransferError("partial is larger than the server size; partial retained")
    return {"url": url, "destination": destination, "expected": expected,
            "remaining": expected - retained}


def fetch_part(record, directory, *, direct_url=None, prepared=None):
    prepared = prepared if prepared is not None else prepare_part(record, directory, direct_url=direct_url)
    if "receipt" in prepared:
        return prepared["receipt"]
    url, destination, expected = prepared["url"], prepared["destination"], prepared["expected"]
    receipt_path = Path(str(destination) + ".sha256.json")
    print(f"Downloading {destination.name} ({expected / 2**30:.2f} GiB)", flush=True)
    # Catalog sizes can be rounded. Enforce the actual server's advertised byte count.
    path, digest = download_direct(url, destination, expected_size=expected, expected_sha256=record.get('sha256') or None, progress=progress)
    receipt = {"title_id": record.get("titleId"), "path": str(path), "bytes": expected,
               "sha256": digest, "url": url, "record": record, "state": "download_verified"}
    _atomic_json(receipt_path, receipt)
    return receipt


def download_group(title_id, group, directory, *, browser_handoff=False, listing_title_id=None, package_only=False):
    listing_title_id = listing_title_id or title_id
    if artifact_identity(listing_title_id, group, package_only=package_only) != title_id:
        raise TransferError("source does not match the explicitly selected artifact title ID")
    if package_only and (len(group) != 1 or source_exclusion(group[0], include_mediafire=True, package_only=True)):
        raise TransferError('package mode requires one supported base PKG source')
    plans = []
    for record in group:
        try:
            plan = prepare_part(record, directory)
        except BrowserRequired:
            if not browser_handoff:
                raise
            direct_url = request_browser_url(record)
            if not direct_url:
                raise TransferError("browser handoff skipped")
            plan = prepare_part(record, directory, direct_url=direct_url)
        plans.append(plan)
    names = [os.path.normcase(str(plan["destination"])) for plan in plans]
    if len(set(names)) != len(names):
        raise TransferError("source parts resolve to the same local filename; no transfer started")
    remaining = sum(plan["remaining"] for plan in plans)
    free = shutil.disk_usage(directory).free
    if remaining and remaining + SPACE_RESERVE > free:
        raise TransferError(f"insufficient space for the complete download set: need {remaining + SPACE_RESERVE} bytes including reserve, have {free}; existing files retained")
    # Probe every part before downloading the first, so a dead final volume does
    # not waste a whole earlier volume. Sizes come from servers, not catalog labels.
    receipts = [fetch_part(record, directory, prepared=plan) for record, plan in zip(group, plans)]
    if package_only:
        from ps5_pkg import inspect_pkg
        try:
            inspection = inspect_pkg(Path(receipts[0]['path']), title_id)
        except (TransferError, OSError, ValueError) as exc:
            raise PackageValidationError(f'PKG downloaded but validation failed: {exc}; file retained; no source fallback') from exc
        _atomic_json(directory / 'package-receipt.json', {
            'schema_version': 1, 'state': 'package_locally_validated',
            'listing_title_id': listing_title_id, 'artifact_title_id': title_id,
            'inspection': inspection, 'file': receipts[0], 'installation_submitted': False})
        print('PKG downloaded and locally inspected. No upload or installation was submitted.', flush=True)
    _atomic_json(directory / "download-receipt.json", {"title_id": title_id,
                 "listing_title_id": listing_title_id, "artifact_title_id": title_id,
                 "state": "download_verified", "files": receipts})
    print(f"Files downloaded; byte counts and SHA-256 recorded: {directory}", flush=True)
    print("Game contents and playability have not yet been validated.", flush=True)
    return 0


class PackageValidationError(TransferError):
    """Local validation failure must stop, rather than silently switch packages."""


def run(title_id, catalog, output, *, list_only=False, interactive=False, pipeline=None, include_mediafire=False, artifact_title_id=None, package_only=False, source_number=None):
    if package_only and pipeline:
        raise TransferError('PKG mode is download and local validation only; --through-upload is not supported')
    pattern = r'(?:PPSA|CUSA)\d{5}' if package_only else r'PPSA\d{5}'
    if not re.fullmatch(pattern, title_id):
        raise TransferError("invalid PS5 title ID")
    catalog_file = catalog / f"{title_id}.json"
    if catalog_file.stat().st_size > 8 * 1024 * 1024:
        raise TransferError("catalog file exceeds the 8 MiB limit")
    records = json.loads(catalog_file.read_text(encoding="utf-8"))
    if not isinstance(records, list):
        raise TransferError("catalog must be an array")
    exclusions = [source_exclusion(record, include_mediafire=include_mediafire, package_only=package_only) for record in records if isinstance(record, dict)]
    skipped = sum(reason is not None for reason in exclusions)
    if skipped:
        print(f"Skipped {skipped} unavailable, disabled, or unsupported catalog entries. Use --package-only for single PKG downloads; password archives remain excluded. MediaFire requires --include-mediafire.", flush=True)
    options = candidates(records, include_mediafire=include_mediafire, package_only=package_only)
    # Validate before any network activity, including multipart identity conflicts.
    identities = [artifact_identity(title_id, group, package_only=package_only) for group in options]
    if not options:
        raise TransferError("no eligible complete base-game sources in the local catalog")
    print(f"{title_id}: {len(options)} candidate source(s)", flush=True)
    for index, group in enumerate(options, 1):
        record = group[0]
        identity = identities[index - 1]
        difference = " [different edition; explicit selection required]" if identity != title_id else ""
        origins = record.get('catalogOrigins') or [record.get('catalogProvenance', {'store': 'Overdrive'})]
        origin = ','.join(dict.fromkeys(p.get('store', 'unknown') for p in origins))
        firmware = record.get('requiredFirmware') or 'not provided'
        maximum = record.get('maxFirmware') or 'not provided'
        print(f"  {index}. {record.get('provider', 'unknown')} - {record.get('label', '')} ({len(group)} file(s)) | format {record.get('artifactFormat') or record.get('format') or 'unknown'} | version {record.get('version') or 'unknown'} | catalog {origin} | artifact {identity} | region {record.get('region') or 'unknown'} | FW min {firmware}, max {maximum}{difference}", flush=True)
    if list_only:
        return 0
    if package_only:
        if source_number is None and interactive:
            try:
                answer = input('Select a listed PKG source number for download and local validation ONLY, or Enter to cancel: ').strip()
            except EOFError:
                return 0
            if not answer:
                return 0
            if not answer.isdigit():
                raise TransferError('invalid package source number; no download started')
            source_number = int(answer)
        if source_number is None or not 1 <= source_number <= len(options):
            raise TransferError('PKG download requires explicit --source-number from --list; no download started')
        chosen = source_number - 1
        if artifact_title_id and artifact_title_id != identities[chosen]:
            raise TransferError('selected package source contradicts --artifact-title-id')
        options, identities = [options[chosen]], [identities[chosen]]
        artifact_title_id = identities[0]
    elif source_number is not None:
        raise TransferError('--source-number currently applies only to --package-only')
    selected_id = artifact_title_id or title_id
    if artifact_title_id is None and any(identity != title_id for identity in identities):
        print("Automatic fallback stays within one title ID; alternate editions are not selected silently.", flush=True)
        if interactive:
            editions = list(dict.fromkeys([title_id, *identities]))
            for number, edition in enumerate(editions, 1):
                print(f"  {number}. {edition}" + (" (selected listing)" if edition == title_id else " (different edition)"), flush=True)
            try:
                answer = input("Select an artifact edition number [1], or q to cancel: ").strip()
            except EOFError:
                return 0
            if answer.lower() in {"q", "quit"}:
                return 0
            if answer:
                if not answer.isdigit() or not 1 <= int(answer) <= len(editions):
                    raise TransferError("invalid edition selection; no download started")
                selected_id = editions[int(answer) - 1]
    if not re.fullmatch(pattern, selected_id):
        raise TransferError("invalid selected artifact title ID")
    options = [group for group, identity in zip(options, identities) if identity == selected_id]
    if not options:
        raise TransferError("no eligible sources for the selected artifact ID; select an advertised alternate explicitly with --artifact-title-id")
    print(f"Listing {title_id}; selected artifact {selected_id}. Validation will require {selected_id}.", flush=True)
    if pipeline and (not Path(pipeline["cli"]).is_file() or not Path(pipeline["seven_zip"]).is_file()):
        raise TransferError("inspection/upload CLI or 7-Zip is missing; no download started")
    output.mkdir(parents=True, exist_ok=True)
    lock = output / ".download.lock"
    try:
        descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
    except FileExistsError as exc:
        raise TransferError(f"another download may own {output}; lock already exists") from exc
    os.close(descriptor)
    try:
        failures = []
        blocked = []
        for group in options:
            # Each source set has its own directory: fallback cannot mix archive parts.
            key = hashlib.sha256(json.dumps(group, sort_keys=True).encode()).hexdigest()[:16]
            directory = output / key
            directory.mkdir(exist_ok=True)
            try:
                download_group(selected_id, group, directory, listing_title_id=title_id, **({"package_only": True} if package_only else {}))
            except PackageValidationError:
                raise
            except (TransferError, OSError, ValueError) as exc:
                if isinstance(exc, BrowserRequired):
                    blocked.append((group, directory))
                message = f"{group[0].get('provider', 'unknown')}: {exc}"
                failures.append(message)
                print(f"Source failed: {message}", file=sys.stderr, flush=True)
            else:
                if pipeline:
                    from ps5_pipeline import run_pipeline
                    run_pipeline(selected_id, directory, pipeline)
                return 0
        if interactive and blocked:
            while True:
                print("\nNo automatic download completed. These are unverified catalog pages:", flush=True)
                print("They may be unavailable or require browser verification; a working download is not confirmed.", flush=True)
                for index, (group, _) in enumerate(blocked, 1):
                    print(f"  {index}. {group[0].get('provider', 'unknown')} - {group[0].get('label', '')}", flush=True)
                try:
                    answer = input("Choose a page number to inspect, or press Enter / q to cancel: ").strip()
                except EOFError:
                    break
                if not answer or answer.lower() in {"q", "quit"}:
                    break
                if answer and (not answer.isdigit() or not 1 <= int(answer) <= len(blocked)):
                    print("Enter a listed source number or q.", flush=True)
                    continue
                group, directory = blocked[int(answer) - 1]
                try:
                    download_group(selected_id, group, directory, browser_handoff=True, listing_title_id=title_id, **({"package_only": True} if package_only else {}))
                except PackageValidationError:
                    raise
                except (TransferError, OSError, ValueError) as exc:
                    print(f"Browser handoff did not complete: {exc}", file=sys.stderr, flush=True)
                else:
                    if pipeline:
                        from ps5_pipeline import run_pipeline
                        run_pipeline(selected_id, directory, pipeline)
                    return 0
        raise TransferError("No source completed.\n" + "\n".join(failures))
    finally:
        lock.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("title_id", type=str.upper)
    parser.add_argument("--catalog", type=Path, default=ROOT / "repos/overdrive/app/data/releases")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--list", action="store_true", help="show candidate sources without downloading")
    parser.add_argument("--interactive", action="store_true", help="offer browser handoff if automatic sources fail")
    parser.add_argument("--include-mediafire", action="store_true", help="explicitly retry MediaFire catalog sources despite widespread removals")
    parser.add_argument("--artifact-title-id", type=str.upper, help="explicitly select an advertised alternate edition ID (same platform)")
    parser.add_argument('--package-only', action='store_true', help='explicit single base PKG download and local inspection only; no console operations')
    parser.add_argument('--source-number', type=int, help='explicit PKG source number from --list; no automatic source fallback')
    parser.add_argument("--through-upload", action="store_true", help="inspect, extract, validate, and upload after downloading")
    release_cli = ROOT / "repos/ps5-companion/target/release/ps5-companion.exe"
    parser.add_argument("--cli", type=Path, default=release_cli if release_cli.is_file() else ROOT / "repos/ps5-companion/target/debug/ps5-companion.exe")
    parser.add_argument("--seven-zip", type=Path, default=Path("C:/Program Files/7-Zip/7z.exe"))
    parser.add_argument("--addr", default="192.168.1.214")
    parser.add_argument("--engine", default="http://127.0.0.1:19113")
    args = parser.parse_args()
    try:
        pipeline = {"cli": args.cli, "seven_zip": args.seven_zip, "addr": args.addr, "engine": args.engine} if args.through_upload else None
        return run(args.title_id, args.catalog, args.out or ROOT / "downloads" / args.title_id, list_only=args.list, interactive=args.interactive, pipeline=pipeline, include_mediafire=args.include_mediafire, artifact_title_id=args.artifact_title_id, package_only=args.package_only, source_number=args.source_number)
    except (TransferError, OSError, ValueError) as exc:
        print(f"Workflow stopped: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
