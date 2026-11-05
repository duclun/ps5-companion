#!/usr/bin/env python3
"""Bounded Overdrive catalog inspection and explicitly-authorized file transfer.

The default operation is metadata listing.  Downloads require ``--download`` and
either an Overdrive record classified as a direct source or an explicitly supplied
final file URL (``--url``).  Browser pages, CAPTCHAs, and provider controls are
reported and never automated.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import time
import random
import math
from collections import deque
import socket
import errno
from http.client import IncompleteRead, RemoteDisconnected
from urllib.error import HTTPError, URLError
from email.utils import parsedate_to_datetime
from datetime import datetime, timezone
from email.message import Message
from pathlib import Path
from typing import Any, BinaryIO, Iterable, Callable
from urllib.parse import unquote, urlparse, parse_qs
from urllib.request import Request, urlopen


TITLE_IDS = {
    "pragmata": "PPSA02530",
    "ace-combat-8": "PPSA24913",
}
OVERDRIVE_RAW = (
    "https://raw.githubusercontent.com/MyLuxy/overdrive/v1.0.5/"
    "app/data/releases/{title_id}.json"
)
USER_AGENT = "PS5-bounded-transfer/1.0"
PROBE_BYTES = 64 * 1024
BLOCK_BYTES = 1024 * 1024
MAX_METADATA_BYTES = 8 * 1024 * 1024
DIRECT_SUFFIXES = ("ffpfsc", "ffpfs", "ffpkg", "exfat", "pkg", "zip", "rar", "7z")
HTML_TYPES = re.compile(r"(?:text/html|application/xhtml)", re.I)
HTML_MARKER = re.compile(rb"^\s*(?:<!doctype\s+html|<html(?:\s|>))", re.I)
CONTENT_RANGE = re.compile(r"^bytes\s+(\d+)-(\d+)/(\d+|\*)$", re.I)
WINDOWS_SUFFIXES = {".exe", ".msi", ".msix", ".msixbundle", ".appx", ".appxbundle",
                    ".com", ".scr", ".dll", ".cpl", ".bat", ".cmd", ".ps1", ".vbs", ".hta"}
WINDOWS_MIME_TYPES = {"application/x-msdownload", "application/x-msdos-program",
                      "application/vnd.microsoft.portable-executable", "application/x-msi",
                      "application/x-ms-installer", "application/x-dosexec"}


class TransferError(RuntimeError):
    """A user-actionable refusal or bounded-transfer failure."""


def reject_windows_download(url: str, *, filename: str | None = None,
                            headers: Any = None, prefix: bytes = b"") -> None:
    """Reject installers/executables masquerading as PS5 images or archives."""
    parsed = urlparse(url)
    names = [unquote(parsed.path).replace("\\", "/").rsplit("/", 1)[-1]]
    if filename:
        names.append(filename)
    for key, values in parse_qs(parsed.query).items():
        if key.lower() in {"filename", "file", "name", "download"}:
            names.extend(values)
    disposition = _header(headers, "Content-Disposition")
    if disposition:
        message = Message()
        message["Content-Disposition"] = disposition
        if message.get_filename():
            names.append(message.get_filename())
    for name in names:
        suffix = Path(unquote(name).rstrip(" .")).suffix.lower()
        if suffix in WINDOWS_SUFFIXES:
            raise TransferError(f"Blocked Windows installer/executable ({suffix}); expected a PS5 image or archive")
    mime = (_header(headers, "Content-Type") or "").split(";", 1)[0].strip().lower()
    if mime in WINDOWS_MIME_TYPES:
        raise TransferError("Blocked Windows executable/installer content type; expected a PS5 image or archive")
    if prefix.startswith(b"MZ") or prefix.startswith(bytes.fromhex("d0cf11e0a1b11ae1")):
        raise TransferError("Blocked Windows executable/installer file signature; expected a PS5 image or archive")


def validate_url(value: str) -> str:
    parsed = urlparse(value)
    if parsed.scheme.lower() != "https" or not parsed.hostname:
        raise TransferError("URL must be an absolute HTTPS URL")
    if parsed.username or parsed.password:
        raise TransferError("URLs containing embedded credentials are refused")
    if any(ord(ch) < 32 or ch.isspace() for ch in value) or len(value) > 8192:
        raise TransferError("URL contains whitespace/control characters or is too long")
    return value


def _host_path(value: str) -> tuple[str, str]:
    parsed = urlparse(validate_url(value))
    return (parsed.hostname or "").lower(), parsed.path or "/"


def _is_host(host: str, domain: str) -> bool:
    return host == domain or host.endswith("." + domain)


def source_kind(record: dict[str, Any]) -> str:
    """Mirror the audited Overdrive resolver's source classification."""
    if record.get("manifestUrl"):
        return "manifest"
    value = record.get("browserUrl") or record.get("url")
    if not value:
        return "unavailable"
    host, path = _host_path(str(value))
    provider = None
    if _is_host(host, "mediafire.com"):
        provider = "mediafire"
    elif _is_host(host, "buzzheavier.com"):
        provider = "buzzheavier"
    elif _is_host(host, "vikingfile.com") or _is_host(host, "vik1ngfile.site"):
        provider = "vikingfile"
    elif _is_host(host, "datanodes.to"):
        provider = "datanodes"
    if provider == "mediafire" and host.startswith("download"):
        return "direct"
    if provider == "vikingfile" and path.startswith("/d/") and not record.get("browserUrl"):
        return "direct"
    if provider:
        return provider
    direct_path = path.lower()
    if (_is_host(host, "archive.org") and direct_path.startswith("/download/")) or (
        host == "github.com" and "/releases/download/" in direct_path
    ) or re.search(r"\.(?:ffpfsc|ffpfs|ffpkg|exfat|pkg|zip|rar|7z)(?:[?#]|$)", direct_path):
        return "direct"
    if record.get("requiresBrowser") or str(record.get("source", "")).startswith("pegasus"):
        return "browser"
    return "direct"


def record_url(record: dict[str, Any]) -> str | None:
    value = record.get("browserUrl") or record.get("url")
    return str(value) if value else None


def looks_like_html(data: bytes, content_type: str | None = None) -> bool:
    return bool(content_type and HTML_TYPES.search(content_type)) or bool(HTML_MARKER.search(data[:4096]))


def parse_content_range(value: str | None) -> tuple[int, int, int | None] | None:
    if not value:
        return None
    match = CONTENT_RANGE.match(value.strip())
    if not match:
        return None
    total = None if match.group(3) == "*" else int(match.group(3))
    return int(match.group(1)), int(match.group(2)), total


def validate_resume_response(status: int, content_range: str | None, offset: int) -> int | None:
    """Require a range response that starts exactly where the partial ends."""
    if offset == 0:
        if status == 206:
            parsed = parse_content_range(content_range)
            if not parsed or parsed[0] != 0:
                raise TransferError("server returned an invalid initial Content-Range")
            return parsed[2]
        if status != 200:
            raise TransferError(f"unexpected initial response HTTP {status}")
        return None
    if status != 206:
        raise TransferError(
            f"server did not honor resume range at byte {offset} (HTTP {status}); partial retained"
        )
    parsed = parse_content_range(content_range)
    if not parsed or parsed[0] != offset or parsed[1] < parsed[0]:
        raise TransferError("server returned an invalid Content-Range for the retained partial")
    return parsed[2]


def _header(headers: Any, name: str) -> str | None:
    if headers is None:
        return None
    try:
        return headers.get(name)
    except AttributeError:
        return None


def _response_status(response: Any) -> int:
    status = getattr(response, "status", None)
    return int(status if status is not None else response.getcode())


def probe_url(url: str, *, timeout: float = 30.0, max_bytes: int = PROBE_BYTES) -> dict[str, Any]:
    """Read at most ``max_bytes`` and classify a URL without following page links."""
    validate_url(url)
    reject_windows_download(url)
    request = Request(
        url,
        headers={
            "Accept-Encoding": "identity",
            "Range": f"bytes=0-{max_bytes - 1}",
            "User-Agent": USER_AGENT,
        },
        method="GET",
    )
    try:
        response = urlopen(request, timeout=timeout)
    except Exception as exc:  # urllib errors vary by provider and platform
        raise TransferError(f"probe failed: {exc}") from exc
    try:
        status = _response_status(response)
        headers = response.headers
        data = response.read(max_bytes + 1)
        final_url = response.geturl()
    finally:
        response.close()
    content_type = _header(headers, "Content-Type")
    content_range = _header(headers, "Content-Range")
    if urlparse(final_url).scheme.lower() != "https":
        raise TransferError("provider redirected the probe to a non-HTTPS URL")
    reject_windows_download(final_url, headers=headers, prefix=data)
    return {
        "status": status,
        "final_host": (urlparse(final_url).hostname or "").lower(),
        "content_type": content_type,
        "content_length": int(_header(headers, "Content-Length"))
        if (_header(headers, "Content-Length") or "").isdigit()
        else None,
        "content_range": content_range,
        "range_honored": status == 206 and parse_content_range(content_range) is not None,
        "bytes_read": min(len(data), max_bytes),
        "truncated": len(data) > max_bytes,
        "is_html": looks_like_html(data, content_type),
        "is_probable_file": not looks_like_html(data, content_type),
    }


def _atomic_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_name("." + path.name + ".tmp")
    temp_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    os.replace(temp_path, path)


def fetch_metadata(title_id: str, path: Path, *, timeout: float = 30.0) -> list[dict[str, Any]]:
    url = OVERDRIVE_RAW.format(title_id=title_id)
    request = Request(url, headers={"Accept-Encoding": "identity", "User-Agent": USER_AGENT})
    try:
        response = urlopen(request, timeout=timeout)
        raw = response.read(MAX_METADATA_BYTES + 1)
        response.close()
    except Exception as exc:
        raise TransferError(f"metadata fetch failed: {exc}") from exc
    if len(raw) > MAX_METADATA_BYTES:
        raise TransferError("metadata response exceeded the bounded metadata limit")
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TransferError("metadata response was not valid UTF-8 JSON") from exc
    records = _records_from_payload(payload)
    _atomic_json(path, records)
    return records


def _records_from_payload(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        payload = payload.get("releases", payload.get("items", payload.get("records", [])))
    if not isinstance(payload, list):
        raise TransferError("metadata must be a JSON array of release records")
    records: list[dict[str, Any]] = []
    for item in payload:
        if isinstance(item, dict):
            records.append(dict(item))
    return records


def load_metadata(path: Path, *, title_id: str, refresh: bool = False, timeout: float = 30.0) -> list[dict[str, Any]]:
    if refresh or not path.exists():
        return fetch_metadata(title_id, path, timeout=timeout)
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TransferError(f"cannot read metadata {path}: {exc}") from exc
    return _records_from_payload(payload)


def _size(value: Any) -> int | None:
    return int(value) if isinstance(value, (int, float)) and int(value) >= 0 else None


def firmware_gate(record: dict[str, Any], firmware: str) -> dict[str, Any]:
    try:
        target = float(firmware)
    except ValueError as exc:
        raise TransferError("--firmware must be numeric, for example 11.20") from exc
    label = str(record.get("label", ""))
    match = re.search(r"(?<!\d)(\d+)\.(\d+|xx)\b|(?<!\d)(\d+)\s*xx\b", label, re.I)
    if not match:
        return {
            "status": "unknown",
            "reason": "no firmware floor was stated in the catalog label",
            "advertised_min": None,
        }
    major = int(match.group(1) or match.group(3))
    minor_token = match.group(2)
    target_major = int(target)
    target_minor = int(round((target - target_major) * 100))
    if minor_token and minor_token.lower() != "xx":
        minor = int(minor_token)
        advertised = float(f"{major}.{minor_token}")
        blocked = major > target_major or (major == target_major and minor > target_minor)
        status = "blocked" if blocked else "allowed"
        return {
            "status": status,
            "reason": f"label advertises {advertised:g}, {'above' if blocked else 'at or below'} target firmware {firmware}",
            "advertised_min": advertised,
        }
    advertised = float(major)
    if major > target_major:
        return {
            "status": "blocked",
            "reason": f"label advertises {major}.xx, above target firmware {firmware}",
            "advertised_min": advertised,
        }
    if major < target_major:
        return {
            "status": "allowed",
            "reason": f"label advertises {major}.xx below target firmware {firmware}",
            "advertised_min": advertised,
        }
    return {
        "status": "uncertain",
        "reason": f"label advertises {major}.xx; its exact minor floor is unknown near firmware {firmware}",
        "advertised_min": advertised,
    }


def safe_filename(record: dict[str, Any], url: str | None = None) -> str:
    raw = str(record.get("filename") or "")
    if not raw and url:
        raw = unquote(urlparse(url).path.rsplit("/", 1)[-1])
    raw = raw.replace("\\", "/").rsplit("/", 1)[-1]
    if not raw or raw in {".", ".."} or len(raw) > 240 or any(ord(ch) < 32 for ch in raw):
        raise TransferError("record does not provide a safe output filename")
    return raw


def normalized_record(record: dict[str, Any], firmware: str) -> dict[str, Any]:
    url = record_url(record)
    host = (urlparse(url).hostname or "").lower() if url else None
    gate = firmware_gate(record, firmware)
    return {
        "id": record.get("id"),
        "format": record.get("format"),
        "filename": record.get("filename"),
        "version": record.get("version"),
        "size_bytes": _size(record.get("sizeBytes")),
        "provider": record.get("provider"),
        "label": record.get("label"),
        "volume": record.get("volume"),
        "url_host": host,
        "requires_browser": bool(record.get("requiresBrowser")),
        "source_kind": source_kind(record),
        "firmware": gate,
    }


def _validate_digest(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip().lower()
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise TransferError("SHA-256 must be exactly 64 hexadecimal characters")
    return value


def _write_sidecar(path: Path, *, url: str, expected_size: int | None, sha256: str | None) -> None:
    _atomic_json(path, {"url": url, "expected_size": expected_size, "sha256": sha256})


def _check_sidecar(path: Path, *, url: str, expected_size: int | None, sha256: str | None) -> None:
    try:
        saved = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TransferError(f"partial sidecar is unreadable; refusing unsafe resume: {exc}") from exc
    if saved != {"url": url, "expected_size": expected_size, "sha256": sha256}:
        raise TransferError("partial belongs to a different URL/size/digest; refusing unsafe resume")


def _check_space(directory: Path, required: int) -> None:
    if required <= 0:
        return
    free = shutil.disk_usage(directory).free
    if free < required:
        raise TransferError(f"insufficient free space: need {required} bytes, have {free}")


class TransientTransferError(TransferError):
    """Only transport failures eligible for a bounded same-source retry."""
    def __init__(self, message, retry_after=None):
        super().__init__(message)
        self.retry_after = retry_after


def _transport_error(exc):
    if isinstance(exc, HTTPError):
        if exc.code not in {408, 429, 500, 502, 503, 504}:
            return None
        value = exc.headers.get('Retry-After', '') if exc.headers else ''
        try:
            delay = float(value) if value.isdigit() else max(0, (parsedate_to_datetime(value) - datetime.now(timezone.utc)).total_seconds())
        except (ValueError, TypeError, OverflowError):
            delay = None
        return TransientTransferError(f'temporary HTTP {exc.code}; partial retained', delay)
    reason = exc.reason if isinstance(exc, URLError) else exc
    if isinstance(reason, (TimeoutError, socket.timeout, ConnectionError, IncompleteRead, RemoteDisconnected)) or (
        isinstance(reason, OSError) and (reason.errno in {errno.ECONNRESET, errno.ECONNABORTED, errno.ETIMEDOUT} or getattr(reason, 'winerror', None) in {10053, 10054, 10060})
    ):
        return TransientTransferError(f'download connection interrupted: {reason}; partial retained')
    return None


class DownloadWatchdog:
    """Measure network waits only; disk writes, callbacks and retry sleeps are excluded."""
    def __init__(self, stall_timeout=90.0, idle_window=300.0, *, idle_gap=20.0, idle_fraction=0.75):
        for value in (stall_timeout, idle_window, idle_gap):
            if not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
                raise TransferError('invalid download watchdog limits')
        if not 0 < idle_fraction <= 1:
            raise TransferError('invalid idle fraction')
        self.stall_timeout, self.window = stall_timeout, idle_window
        self.gap, self.fraction = idle_gap, idle_fraction
        self.elapsed = 0.0
        self.waits = deque()

    def read(self, response, count):
        began = time.monotonic()
        # HTTPResponse.read1 returns available data instead of waiting for a whole
        # MiB while a peer trickles bytes. Test/custom responses retain read().
        read = getattr(response, 'read1', None) or response.read
        chunk = read(count)
        duration = max(0.0, time.monotonic() - began)
        self.elapsed += duration
        self.waits.append((self.elapsed - duration, self.elapsed, duration >= self.gap))
        start = self.elapsed - self.window
        while self.waits and self.waits[0][1] <= start:
            self.waits.popleft()
        # Zero-duration reads must not grow memory without bound. Coalesce short
        # waits: only long idle intervals need storing for the rolling idle sum.
        if duration < self.gap:
            self.waits.pop()
        if duration >= self.stall_timeout:
            raise TransientTransferError('download made no observable progress within the watchdog limit; partial retained')
        idle = sum(end - max(begin, start) for begin, end, is_idle in self.waits if is_idle)
        if self.elapsed >= self.window and idle >= self.window * self.fraction:
            raise TransientTransferError('download spent most of its recent network time idle between bursts; partial retained')
        return chunk


def _download_prefix(response, count, watchdog):
    # read1 may return one byte: retain the original bounded screening window so
    # split MZ/HTML signatures cannot slip past installer/page rejection.
    if not watchdog or not callable(getattr(response, 'read1', None)):
        return watchdog.read(response, count) if watchdog else response.read(count)
    chunks, received = [], 0
    while received < count:
        chunk = watchdog.read(response, count - received)
        if not chunk:
            break
        chunks.append(chunk)
        received += len(chunk)
    return b''.join(chunks)


def download_direct(url, destination, *, expected_size=None, expected_sha256=None,
                    timeout=60.0, progress=None, max_retries=3, retry_base=2.0,
                    retry_wait_budget=300.0, cancelled=None, stall_timeout=90.0, idle_window=300.0):
    """Retry only transient transport failures; never resubmit an upload."""
    if not isinstance(max_retries, int) or not 0 <= max_retries <= 10 or not 0 <= retry_base <= 60 or not 0 <= retry_wait_budget <= 3600:
        raise TransferError('invalid bounded download retry policy')
    DownloadWatchdog(stall_timeout, idle_window)
    if not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or timeout <= 0:
        raise TransferError('invalid download socket timeout')
    destination = Path(destination)
    history = Path(str(destination) + '.retry.json')
    partial = Path(str(destination) + '.partial')
    entity = Path(str(partial) + '.entity.json')
    attempts = []
    waited = 0.0
    for attempt in range(max_retries + 1):
        if cancelled and cancelled():
            raise TransferError('download cancelled; partial retained')
        try:
            result = _download_direct_once(url, destination, expected_size=expected_size,
                expected_sha256=expected_sha256, timeout=min(timeout, stall_timeout), progress=progress, cancelled=cancelled,
                watchdog=DownloadWatchdog(stall_timeout, idle_window))
            if attempts:
                _atomic_json(history, {'state': 'complete', 'attempts': attempts})
            return result
        except TransientTransferError as exc:
            retained = partial.stat().st_size if partial.exists() else 0
            attempts.append({'attempt': attempt + 1, 'bytes_retained': retained, 'error': str(exc)})
            _atomic_json(history, {'state': 'interrupted', 'attempts': attempts})
            if attempt == max_retries:
                raise TransferError(f'download retry limit reached after {attempt + 1} attempts: {exc}') from exc
            if retained and not expected_sha256 and not entity.exists():
                raise TransferError('cannot automatically resume without an entity validator or expected digest; partial retained') from exc
            delay = max(retry_base * 2**attempt + random.uniform(0, retry_base / 4), exc.retry_after or 0)
            if waited + delay > retry_wait_budget:
                raise TransferError('download retry wait budget exceeded; partial retained') from exc
            print(f'Temporary download failure; retry {attempt + 1}/{max_retries} in {delay:.1f}s from byte {retained}', flush=True)
            remaining = delay
            while remaining > 0:
                if cancelled and cancelled():
                    raise TransferError('download cancelled; partial retained')
                step = min(remaining, 0.25)
                time.sleep(step)
                remaining -= step
            waited += delay


def _download_direct_once(
    url: str,
    destination: Path,
    *,
    expected_size: int | None = None,
    expected_sha256: str | None = None,
    timeout: float = 60.0,
    progress: Callable[[int, int | None], None] | None = None,
    cancelled=None,
    watchdog=None,
) -> tuple[Path, str]:
    """Stream a direct file into a resumable partial, then atomically finalize it."""
    validate_url(url)
    reject_windows_download(url, filename=destination.name)
    expected_sha256 = _validate_digest(expected_sha256)
    destination = destination.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise TransferError(f"refusing to overwrite existing file: {destination}")
    partial = Path(str(destination) + ".partial")
    sidecar = Path(str(partial) + ".meta.json")
    offset = partial.stat().st_size if partial.exists() else 0
    if offset:
        with partial.open("rb") as retained:
            reject_windows_download(url, filename=destination.name, prefix=retained.read(64))
    if offset and not sidecar.exists():
        raise TransferError("partial has no sidecar metadata; refusing unsafe resume")
    if sidecar.exists():
        _check_sidecar(sidecar, url=url, expected_size=expected_size, sha256=expected_sha256)
    else:
        _write_sidecar(sidecar, url=url, expected_size=expected_size, sha256=expected_sha256)
    if expected_size is not None and offset > expected_size:
        raise TransferError("partial is larger than the catalog size; refusing resume")
    _check_space(destination.parent, max(0, (expected_size or 0) - offset))

    headers = {"Accept-Encoding": "identity", "User-Agent": USER_AGENT}
    entity_path = Path(str(partial) + '.entity.json')
    saved_entity = None
    if entity_path.exists():
        try:
            saved_entity = json.loads(entity_path.read_text(encoding='utf-8'))
            if (not isinstance(saved_entity, dict) or saved_entity.get('header') not in {'ETag', 'Last-Modified'}
                or not isinstance(saved_entity.get('value'), str) or not saved_entity['value']
                or len(saved_entity['value']) > 2048 or any(c in saved_entity['value'] for c in '\r\n')):
                raise ValueError('invalid entity validator')
        except (OSError, ValueError) as exc:
            raise TransferError(f'unreadable source entity metadata; partial retained: {exc}') from exc
    if offset:
        headers["Range"] = f"bytes={offset}-"
        if saved_entity:
            headers['If-Range'] = saved_entity['value']
    request = Request(url, headers=headers, method="GET")
    try:
        response = urlopen(request, timeout=timeout)
    except Exception as exc:
        transient = _transport_error(exc)
        if isinstance(exc, HTTPError):
            exc.close()
        if transient:
            raise transient from exc
        raise TransferError(f"download request failed: {exc}; partial retained") from exc
    try:
        status = _response_status(response)
        headers_obj = response.headers
        if status >= 400 or status not in {200, 206}:
            raise TransferError(f"server returned HTTP {status}; partial retained")
        if urlparse(response.geturl()).scheme.lower() != "https":
            raise TransferError("provider redirected the download to a non-HTTPS URL; partial retained")
        reject_windows_download(response.geturl(), filename=destination.name, headers=headers_obj)
        total_from_range = validate_resume_response(status, _header(headers_obj, "Content-Range"), offset)
        etag = _header(headers_obj, 'ETag')
        modified = _header(headers_obj, 'Last-Modified')
        entity = ({'header': 'ETag', 'value': etag} if etag and not etag.startswith('W/') else
                  {'header': 'Last-Modified', 'value': modified} if modified else None)
        if offset and saved_entity and _header(headers_obj, saved_entity['header']) != saved_entity['value']:
            raise TransferError('source entity changed or lost its validator; partial retained')
        content_type = _header(headers_obj, "Content-Type")
        prefix_size = min(BLOCK_BYTES, PROBE_BYTES)
        known_total = expected_size or total_from_range
        if known_total is not None:
            prefix_size = min(prefix_size, max(0, known_total - offset))
        first = _download_prefix(response, prefix_size, watchdog)
        if offset == 0:
            reject_windows_download(response.geturl(), filename=destination.name, headers=headers_obj, prefix=first)
        if looks_like_html(first, content_type):
            raise TransferError("provider returned HTML instead of a file; browser verification is required")
        total = total_from_range
        if total is None:
            length = _header(headers_obj, "Content-Length")
            if length and length.isdigit():
                total = offset + int(length)
        if expected_size is not None and total is not None and total != expected_size:
            raise TransferError(f"server size {total} differs from expected catalog size {expected_size}")
        if offset and saved_entity and saved_entity.get('size') is not None and total != saved_entity['size']:
            raise TransferError('source size changed since the previous response; partial retained')
        if total is not None:
            _check_space(destination.parent, max(0, total - offset))
        if entity and (not offset or not saved_entity):
            _atomic_json(entity_path, {**entity, 'size': total})
        mode = "ab" if offset else "wb"
        written = offset
        last_progress = 0.0
        with partial.open(mode) as output:
            for chunk in _chunks(first, response, expected_size, written, watchdog=watchdog):
                if cancelled and cancelled():
                    raise TransferError('download cancelled; partial retained')
                output.write(chunk)
                written += len(chunk)
                if progress and time.monotonic() - last_progress >= 2:
                    progress(written, expected_size or total)
                    last_progress = time.monotonic()
            output.flush()
            os.fsync(output.fileno())
    except TransferError:
        raise
    except Exception as exc:
        transient = _transport_error(exc)
        if transient:
            raise transient from exc
        raise TransferError(f"download failed: {exc}; partial retained") from exc
    finally:
        response.close()

    final_size = partial.stat().st_size
    if progress:
        progress(final_size, expected_size or total)
    if expected_size is not None and final_size != expected_size:
        raise TransientTransferError(f"download ended at {final_size} bytes; expected {expected_size}; partial retained")
    if expected_size is None and total is not None and final_size != total:
        raise TransientTransferError(f"download ended at {final_size} bytes; server advertised {total}; partial retained")
    if final_size == 0:
        raise TransferError("download produced an empty file; partial retained")
    digest = _sha256_file(partial)
    if expected_sha256 and digest != expected_sha256:
        raise TransferError(f"SHA-256 mismatch ({digest} != supplied {expected_sha256}); partial retained")
    if destination.exists():
        raise TransferError(f"destination appeared during transfer; refusing overwrite: {destination}")
    partial.rename(destination)
    sidecar.unlink(missing_ok=True)
    return destination, digest


def _chunks(first: bytes, response: Any, expected_size: int | None, written: int, *, watchdog=None) -> Iterable[bytes]:
    current = written
    if first:
        if expected_size is not None and current + len(first) > expected_size:
            raise TransferError("server sent more bytes than expected; partial retained")
        yield first
        current += len(first)
    while True:
        chunk = watchdog.read(response, BLOCK_BYTES) if watchdog else response.read(BLOCK_BYTES)
        if not chunk:
            return
        if expected_size is not None and current + len(chunk) > expected_size:
            raise TransferError("server sent more bytes than expected; partial retained")
        yield chunk
        current += len(chunk)


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(BLOCK_BYTES):
            digest.update(chunk)
    return digest.hexdigest()


def _default_metadata(root: Path, title_id: str, title: str | None) -> Path:
    slug = title or title_id.lower()
    return root / "downloads" / slug / "index-v1.0.5.json"


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--title", choices=sorted(TITLE_IDS), help="catalog title alias")
    parser.add_argument("--title-id", help="catalog title ID, for example PPSA02530")
    parser.add_argument("--metadata", type=Path, help="local metadata JSON path")
    parser.add_argument("--refresh", action="store_true", help="fetch v1.0.5 metadata before listing")
    parser.add_argument("--id", dest="record_id", help="select one metadata record")
    parser.add_argument(
        "--url",
        "--direct-url",
        dest="url",
        help="explicit final direct file URL; never a provider landing page",
    )
    parser.add_argument("--probe", action="store_true", help="bounded GET probe (at most 64 KiB)")
    parser.add_argument("--download", action="store_true", help="download the selected direct file")
    parser.add_argument("--output", type=Path, help="download destination")
    parser.add_argument("--expected-size", type=int, help="expected bytes for --url mode")
    parser.add_argument("--sha256", help="expected SHA-256, if independently supplied")
    parser.add_argument("--firmware", default="11.20", help="target PS5 firmware gate (default: 11.20)")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--json", action="store_true", help="emit normalized metadata JSON without URLs")
    return parser


def _title_id(args: argparse.Namespace) -> str | None:
    return (args.title_id or (TITLE_IDS.get(args.title) if args.title else None))


def _print(value: Any, as_json: bool) -> None:
    if as_json:
        print(json.dumps(value, indent=2, ensure_ascii=False))
    elif isinstance(value, list):
        for item in value:
            print(
                f"{item.get('id')}  {item.get('format') or '-':8}  "
                f"{item.get('size_bytes') or '?':>12}  {item.get('source_kind'):16}  "
                f"{item.get('firmware', {}).get('status'):8}  {item.get('label') or item.get('filename') or ''}"
            )
    else:
        print(json.dumps(value, indent=2, ensure_ascii=False))


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    root = Path(__file__).resolve().parents[1]
    title_id = _title_id(args)
    if not title_id and not args.url:
        print("error: provide --title/--title-id or explicit --url", file=sys.stderr)
        return 2
    records: list[dict[str, Any]] = []
    metadata_path = None
    if title_id:
        metadata_path = args.metadata or _default_metadata(root, title_id, args.title)
        try:
            records = load_metadata(metadata_path, title_id=title_id, refresh=args.refresh, timeout=args.timeout)
        except TransferError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 2
    if args.url and args.record_id:
        selected = next((item for item in records if item.get("id") == args.record_id), None)
        if selected is None:
            print(f"error: record ID not found: {args.record_id}", file=sys.stderr)
            return 2
        selected = dict(selected)
        selected["url"] = args.url
        selected.pop("browserUrl", None)
        selected["requiresBrowser"] = False
        selected["source"] = "explicit-direct-url"
        records = [selected]
    if args.record_id and not args.url:
        records = [item for item in records if item.get("id") == args.record_id]
        if not records:
            print(f"error: record ID not found: {args.record_id}", file=sys.stderr)
            return 2
    if args.url and not args.record_id:
        records = [
            {
                "id": "explicit-url",
                "format": Path(urlparse(args.url).path).suffix.removeprefix("."),
                "filename": None,
                "url": args.url,
                "sizeBytes": args.expected_size,
                "sha256": args.sha256,
                "requiresBrowser": False,
                "source": "explicit-direct-url",
                "label": "explicit final URL",
            }
        ]
    normalized = [normalized_record(item, args.firmware) for item in records]
    if not args.record_id and not args.url:
        _print(normalized, args.json)
        return 0
    selected = records[0]
    item = normalized[0]
    url = args.url or record_url(selected)
    kind = source_kind(selected)
    if args.probe:
        if not url:
            print("error: selected record has no URL", file=sys.stderr)
            return 2
        try:
            probe = probe_url(url, timeout=args.timeout)
        except TransferError as exc:
            print(f"probe error: {exc}", file=sys.stderr)
            return 2
        _print({"record": item, "probe": probe}, args.json)
    elif args.json:
        _print(item, True)
    else:
        _print(item, False)
    if not args.download:
        return 0
    if kind != "direct":
        print(
            f"refusing download: source kind is {kind}; open it in a browser and supply its final file URL with --url",
            file=sys.stderr,
        )
        return 2
    gate = item["firmware"]
    if gate["status"] not in {"allowed", "unknown"}:
        print(f"refusing download: firmware gate: {gate['reason']}", file=sys.stderr)
        return 2
    if not url:
        print("refusing download: selected record has no URL", file=sys.stderr)
        return 2
    expected_size = args.expected_size if args.expected_size is not None else item.get("size_bytes")
    expected_sha = _validate_digest(args.sha256 or selected.get("sha256"))
    try:
        output_slug = args.title or (title_id.lower() if title_id else "explicit")
        output = args.output or (root / "downloads" / output_slug / safe_filename(selected, url))
        destination, digest = download_direct(
            url,
            output,
            expected_size=expected_size,
            expected_sha256=expected_sha,
            timeout=max(args.timeout, 60.0),
        )
    except TransferError as exc:
        print(f"download refused/failed: {exc}", file=sys.stderr)
        return 2
    print(f"downloaded {destination} ({digest})")
    print("The digest is a corruption check against the supplied value; it does not prove source authenticity.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
