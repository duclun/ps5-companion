"""Bounded ProsperoStore metadata adapter for the existing PC transfer pipeline."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import sys
from urllib.parse import urlparse, unquote
from urllib.request import Request, build_opener, HTTPRedirectHandler

from pragmata_transfer import TransferError, download_direct, _atomic_json, _sha256_file
from ps5_pipeline import run_pipeline

ROOT = Path(__file__).resolve().parents[1]
API = 'https://prosperostore.net/api/v1'
MAX_JSON = 4 * 1024 * 1024
FORMATS = {'zip', 'exfat', 'ffpfsc', 'ffpfs', 'ffpkg'}
APP_ID = re.compile(r'^[A-Z]{4}[0-9]{5}(?:-[A-Za-z0-9_-]{1,80})?$')
TITLE_ID = re.compile(r'^[A-Z]{4}[0-9]{5}$')


def text(value, maximum=512):
    if not isinstance(value, str) or len(value) > maximum:
        raise TransferError('invalid or oversized catalog text')
    return ''.join(c for c in value if c >= ' ' and c != '\x7f')


def identity(item):
    if not isinstance(item, dict):
        raise TransferError('catalog entry must be an object')
    title = item.get('titleid', '')
    app_id = item.get('id', title)
    if not isinstance(title, str) or not TITLE_ID.fullmatch(title) or not isinstance(app_id, str) or not APP_ID.fullmatch(app_id):
        raise TransferError('invalid homebrew catalog identity')
    if app_id != title and not app_id.startswith(title + '-'):
        raise TransferError('homebrew identity does not match its title ID')
    return app_id, title


def index_entries(document):
    if not isinstance(document, dict) or document.get('schema') != 3 or not isinstance(document.get('apps'), list):
        raise TransferError('unsupported homebrew index schema')
    shared = document.get('shared', [])
    if not isinstance(shared, list) or len(document['apps']) + len(shared) > 5000:
        raise TransferError('invalid or oversized homebrew index')
    entries, seen = [], set()
    for entry in document['apps'] + shared:
        app_id, _ = identity(entry)
        if app_id in seen:
            raise TransferError('duplicate homebrew app identity')
        seen.add(app_id)
        text(entry.get('name', ''))
        text(entry.get('version') or '', 100)
        text(entry.get('category') or '', 100)
        if entry.get('status') not in {'available', 'coming_soon'}:
            raise TransferError('invalid index app status')
        entries.append(entry)
    return entries


def github_parts(url, *, artifact=False):
    if not isinstance(url, str) or len(url) > 2048 or any(c.isspace() for c in url) or '\\' in url:
        raise TransferError('invalid GitHub URL')
    parsed = urlparse(url)
    if parsed.scheme != 'https' or parsed.netloc.lower() != 'github.com' or parsed.query or parsed.fragment:
        raise TransferError('expected a canonical HTTPS GitHub URL')
    parts = parsed.path.strip('/').split('/')
    if any(not p or unquote(p) in {'.', '..'} or any(c in unquote(p) for c in '\r\n\\') for p in parts):
        raise TransferError('unsafe GitHub URL path')
    if artifact:
        if len(parts) != 6 or parts[2:4] != ['releases', 'download']:
            raise TransferError('artifact must be a pinned GitHub release asset')
    elif len(parts) != 2:
        raise TransferError('source_repo must identify a GitHub repository')
    return (parts[0].lower(), parts[1].removesuffix('.git').lower())


def validate_details(entry, detail):
    app_id, title = identity(entry)
    if not isinstance(detail, dict) or detail.get('schema') != 3 or identity(detail) != (app_id, title):
        raise TransferError('app details do not match the selected catalog entry')
    if entry.get('status') != 'available' or detail.get('status') != 'available':
        raise TransferError('this homebrew app is not available')
    if detail.get('version') != entry.get('version'):
        raise TransferError('catalog version changed; refresh the index and retry')
    text(detail.get('version', ''), 100)
    if not re.fullmatch(r'PPSA[0-9]{5}', title):
        raise TransferError('the current transfer pipeline supports PPSA title IDs only')
    if detail.get('format') not in FORMATS:
        raise TransferError('unsupported homebrew artifact format')
    digest = detail.get('sha256')
    if not isinstance(digest, str) or not re.fullmatch('[0-9a-fA-F]{64}', digest):
        raise TransferError('missing or invalid expected SHA-256')
    size = detail.get('size')
    if isinstance(size, bool) or not isinstance(size, int) or not 0 < size < 2**63:
        raise TransferError('download requires a positive catalog byte count')
    if github_parts(detail.get('artifact_url'), artifact=True) != github_parts(detail.get('source_repo')):
        raise TransferError('release asset belongs to a different repository than source_repo')
    filename = unquote(urlparse(detail['artifact_url']).path.rsplit('/', 1)[-1])
    if Path(filename).suffix.lower() != '.' + detail['format']:
        raise TransferError('release filename does not match its declared format')
    conflicts = detail.get('conflicts_with', [])
    if not isinstance(conflicts, list) or any(not isinstance(c, str) or not APP_ID.fullmatch(c) for c in conflicts):
        raise TransferError('invalid homebrew conflict metadata')
    return detail


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise TransferError('catalog metadata redirect refused')


def fetch_json(path):
    # Paths are generated only from the validated catalog identity.
    request = Request(f'{API}/{path}', headers={'Accept': 'application/json', 'Accept-Encoding': 'identity', 'User-Agent': 'PS5-companion-homebrew/1.0'})
    try:
        with build_opener(NoRedirect()).open(request, timeout=20) as response:
            length = response.headers.get('Content-Length')
            if length and int(length) > MAX_JSON:
                raise TransferError('homebrew metadata exceeds 4 MiB')
            body = response.read(MAX_JSON + 1)
        if len(body) > MAX_JSON:
            raise TransferError('homebrew metadata exceeds 4 MiB')
        return json.loads(body.decode('utf-8'))
    except (OSError, ValueError) as exc:
        raise TransferError(f'homebrew metadata request failed: {exc}') from exc


class Catalog:
    def __init__(self, cache, offline=False):
        self.cache = Path(cache)
        self.offline = offline

    def document(self, path, validate):
        cache_path = self.cache / path
        if self.offline:
            if not cache_path.is_file() or cache_path.stat().st_size > MAX_JSON + 4096:
                raise TransferError('offline metadata cache is missing or oversized')
            cached = json.loads(cache_path.read_text(encoding='utf-8'))
            if cached.get('url') != f'{API}/{path}' or not isinstance(cached.get('fetched_at'), str):
                raise TransferError('invalid metadata cache provenance')
            document = cached['document']
            print(f'Offline cache from {cached["fetched_at"]}; availability may have changed.', file=sys.stderr)
            validate(document)
        else:
            document = fetch_json(path)
            validate(document)
            _atomic_json(cache_path, {'url': f'{API}/{path}', 'fetched_at': datetime.now(timezone.utc).isoformat(), 'document': document})
        return document

    def index(self):
        return index_entries(self.document('index.json', index_entries))

    def details(self, entry):
        app_id, _ = identity(entry)
        return self.document(f'apps/{app_id}.json', lambda d: validate_details(entry, d))


def download_app(entry, detail, out, *, through_upload=False, config=None):
    validate_details(entry, detail)
    app_id, title = identity(entry)
    if through_upload and (detail.get('conflicts_with') or entry.get('conflicts_with')):
        raise TransferError('this app shares a console title ID; automatic upload refused. Review conflicts first.')
    provenance = {k: detail[k] for k in ('id', 'titleid', 'version', 'format', 'size', 'sha256', 'artifact_url', 'source_repo') if k in detail}
    key = hashlib.sha256(json.dumps(provenance, sort_keys=True).encode()).hexdigest()[:16]
    title_dir = Path(out) / app_id
    title_dir.mkdir(parents=True, exist_ok=True)
    lock = title_dir / '.download.lock'
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError as exc:
        raise TransferError('another homebrew download owns this app; lock retained') from exc
    os.close(fd)
    try:
        directory = title_dir / key
        directory.mkdir(exist_ok=True)
        destination = directory / f'{title}.{detail["format"]}'
        digest = detail['sha256'].lower()
        if destination.is_symlink():
            raise TransferError('homebrew artifact destination is a symbolic link')
        if destination.exists():
            if destination.is_symlink() or destination.stat().st_size != detail['size'] or _sha256_file(destination) != digest:
                raise TransferError('existing homebrew artifact differs; refusing overwrite')
        else:
            last = [0.0]
            import time
            def progress(done, total):
                now = time.monotonic()
                if now - last[0] >= 15 or done == total:
                    print(f'{done / 1e6:.1f} / {total / 1e6:.1f} MB ({done / total:.1%})', flush=True)
                    last[0] = now
            download_direct(detail['artifact_url'], destination, expected_size=detail['size'], expected_sha256=digest, progress=progress)
        _atomic_json(directory / 'download-receipt.json', {'title_id': title, 'app_id': app_id, 'state': 'download_verified', 'catalog_url': f'{API}/apps/{app_id}.json', 'catalog_asserted_sha256': digest, 'record': provenance, 'files': [{'path': str(destination.resolve()), 'bytes': detail['size'], 'sha256': digest}]})
        print(f'Download verified against the catalog SHA-256: {destination}')
        if through_upload:
            if config is None:
                raise TransferError('upload configuration missing')
            run_pipeline(title, directory, config)
        return destination
    finally:
        lock.unlink(missing_ok=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['search', 'info', 'download', 'refresh'])
    parser.add_argument('value', nargs='?', default='')
    parser.add_argument('--category')
    parser.add_argument('--offline', action='store_true', help='use explicitly stale cached metadata; search/info only')
    parser.add_argument('--cache', type=Path, default=ROOT / 'cache/homebrew')
    parser.add_argument('--out', type=Path, default=ROOT / 'downloads/homebrew')
    parser.add_argument('--through-upload', action='store_true', help='also validate/extract and stage an upload; never launch')
    parser.add_argument('--addr', default='192.168.1.214')
    parser.add_argument('--engine', default='http://127.0.0.1:19113')
    args = parser.parse_args(argv)
    try:
        if args.offline and args.command in {'download', 'refresh'}:
            raise TransferError('offline mode supports search/info only')
        if args.through_upload and args.command != 'download':
            raise TransferError('--through-upload requires download')
        catalog = Catalog(args.cache, args.offline)
        entries = catalog.index()
        if args.command in {'search', 'refresh'}:
            matches = [e for e in entries if all(w in f'{e.get("name", "")} {e.get("author", "")} {e.get("titleid", "")}'.casefold() for w in args.value.casefold().split()) and (not args.category or e.get('category') == args.category)]
            print(f'{len(matches)} homebrew app(s)')
            for entry in sorted(matches, key=lambda e: e['name'].casefold()):
                app_id, _ = identity(entry)
                print(f'{app_id}  {text(entry["name"])}  {text(entry.get("version") or "",100)}  [{entry["status"]}; {text(entry.get("category") or "",100)}]')
            return 0
        selected = next((e for e in entries if identity(e)[0] == args.value), None)
        if selected is None:
            raise TransferError('select an exact app ID from homebrew search')
        if selected.get('status') != 'available':
            if args.command == 'info':
                print(json.dumps(selected, indent=2, ensure_ascii=True))
                return 0
            raise TransferError('this app is reserved/coming soon; no download available')
        detail = catalog.details(selected)
        if args.command == 'info':
            fields = ('id','titleid','name','author','version','status','format','size','license','source_repo','artifact_url','sha256','conflicts_with','safety')
            print(json.dumps({k: detail.get(k) for k in fields}, indent=2, ensure_ascii=True))
            return 0
        cli = ROOT / 'repos/ps5-companion/target/release/ps5-companion.exe'
        seven_zip = Path('C:/Program Files/7-Zip/7z.exe')
        if args.through_upload and (not cli.is_file() or not seven_zip.is_file()):
            raise TransferError('release CLI or 7-Zip is missing; no download started')
        download_app(selected, detail, args.out, through_upload=args.through_upload, config={'cli':cli,'seven_zip':seven_zip,'addr':args.addr,'engine':args.engine})
        return 0
    except (TransferError, OSError, ValueError, KeyError) as exc:
        print(f'Homebrew workflow stopped: {exc}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
