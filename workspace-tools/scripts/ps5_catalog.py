"""Metadata-only storefront adapters; export an offline catalog for the existing pipeline."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from datetime import datetime, timezone
from urllib.request import Request, HTTPRedirectHandler, build_opener

from pragmata_transfer import TransferError, validate_url, _atomic_json

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / 'cache/storefronts'
LIMIT = 16 * 1024 * 1024
ADAPTER_VERSION = 4
REPOS = {'blackbox': 'D3ATHLY/blackbox', 'orbit': 'saawant12/orbit-store-ps5'}
TITLE = re.compile(r'(PPSA|CUSA)\d{5}\Z')


def text(value, field, limit=4096):
    if not isinstance(value, str) or not value.strip() or len(value) > limit or any(ord(c) < 32 for c in value):
        raise TransferError(f'invalid catalog {field}')
    return value


def bounded_bytes(path, limit=LIMIT):
    if path.is_symlink() or path.stat().st_size > limit:
        raise TransferError('catalog file is linked or oversized')
    with path.open('rb') as source:
        data = source.read(limit + 1)
    if len(data) > limit: raise TransferError('catalog file grew beyond size limit')
    return data


def bounded_json(path, limit=LIMIT):
    return json.loads(bounded_bytes(path, limit).decode('utf-8-sig'))


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise TransferError('catalog metadata redirect refused')


def fetch(url, limit=LIMIT):
    validate_url(url)
    if not url.startswith(('https://api.github.com/repos/', 'https://raw.githubusercontent.com/')):
        raise TransferError('catalog metadata host refused')
    with build_opener(NoRedirect()).open(Request(url, headers={'User-Agent': 'PS5-companion-catalog/1', 'Accept-Encoding': 'identity'}), timeout=25) as response:
        if int(response.headers.get('Content-Length', '0')) > limit:
            raise TransferError('catalog response exceeds size limit')
        data = response.read(limit + 1)
    if len(data) > limit:
        raise TransferError('catalog response exceeds size limit')
    return data


def orbit_decode(data, header):
    # Orbit's GPL source publishes this shared key for scraping resistance, not
    # account authentication. Do not execute its compiler helper or import tools.
    if not 40 < len(data) <= LIMIT + 40 or data[:12] != b'ORBITENC\x01\x01\x01\x00':
        raise TransferError('invalid Orbit feed envelope/version/purpose')
    if header.stat().st_size > 4096:
        raise TransferError('oversized Orbit catalog key header')
    source = header.read_text(encoding='utf-8')
    match = re.search(r'catalog_key\[32\]\s*=\s*\{([^}]+)\}', source)
    if not match:
        raise TransferError('Orbit public catalog header format is unsupported')
    tokens = re.findall(r'0x[0-9a-fA-F]{2}', match[1])
    if len(tokens) != 32 or re.sub(r'0x[0-9a-fA-F]{2}|[\s,]', '', match[1]):
        raise TransferError('invalid Orbit public catalog header')
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    except ImportError as exc:
        raise TransferError('Orbit refresh needs cryptography; use the catalog virtual environment or install requirements-catalog.txt') from exc
    try:
        plain = AESGCM(bytes(int(t, 16) for t in tokens)).decrypt(data[12:24], data[24:], data[:24])
    except Exception as exc:
        raise TransferError('Orbit catalog authentication failed; previous snapshot retained') from exc
    return json.loads(plain)


def normalize(store, documents, commit):
    releases, rejected = [], []
    for document in documents:
        if not isinstance(document, dict) or document.get('schemaVersion') != 1 or not isinstance(document.get('releases'), list):
            raise TransferError(f'unsupported {store} catalog schema')
        if store == 'orbit' and document.get('feedVersion') != 8:
            raise TransferError('unsupported Orbit feed generation')
        if len(document['releases']) > 10000:
            raise TransferError('too many catalog records')
        seen = set()
        for item in document['releases']:
            try:
                if not isinstance(item, dict):
                    raise TransferError('release must be an object')
                rid = text(item.get('id'), 'release ID', 200)
                if rid in seen:
                    raise TransferError('duplicate release ID')
                seen.add(rid)
                title = text(item.get('titleId'), 'title ID', 9)
                if not TITLE.fullmatch(title):
                    raise TransferError('unsupported title identity')
                platform = 'ps5' if title.startswith('PPSA') else 'ps4'
                if item.get('platform', platform) != platform:
                    raise TransferError('title/platform contradiction')
                name = text(item.get('title'), 'title')
                url = validate_url(text(item.get('url') or item.get('browserUrl'), 'source URL', 8192))
                fmt = text(item.get('format'), 'format', 64)
                filename = text(item.get('filename'), 'filename', 250)
                if any(c in filename for c in '/\\:') or filename in {'.', '..'}:
                    raise TransferError('unsafe filename')
                size = item.get('sizeBytes')
                if size is not None and (type(size) is not int or not 0 < size < 2**63):
                    raise TransferError('invalid size')
                provider = text(item.get('provider'), 'provider', 100)
                compression = item.get('compression')
                if compression not in {None, 'zip', 'rar', '7z'}:
                    raise TransferError('unsupported archive compression')
                content = item.get('contentFormat')
                if content is not None:
                    text(content, 'content format', 64)
                    if content != 'LZ4' or platform != 'ps5' or not compression or fmt not in {'Unknown', 'Game folder'}:
                        raise TransferError('unsupported content format or contradictory LZ4 packaging')
                # Add-ons never enter the base-game fallback pool.
                if item.get('addonId'):
                    continue
                record = {
                    'id': store + '-' + hashlib.sha256(rid.encode()).hexdigest()[:24],
                    'titleId': title, 'platform': platform, 'title': name,
                    'format': compression or fmt.lower(), 'artifactFormat': fmt,
                    'filename': filename, 'url': url, 'provider': provider,
                    'requiresBrowser': str(item.get('sourceId', '')).lower() != 'archive',
                    'sizeBytes': size, 'version': item.get('version'),
                    'label': fmt + (f' in {compression}' if compression else ''),
                    'category': 'game', 'source': store, 'record': rid,
                    'region': item.get('region'), 'archivePassword': item.get('archivePassword'),
                    'requiredFirmware': item.get('requiredFirmware'), 'maxFirmware': item.get('maxFirmware'),
                    'catalogProvenance': {'store': store, 'repository': REPOS[store], 'commit': commit, 'release_id': rid},
                }
                if content:
                    record['contentFormat'] = content
                    record['label'] = f'{content} assets in {compression} (compatible AMPR runtime required)'
                digest = item.get('sha256')
                if digest:
                    if not isinstance(digest, str) or not re.fullmatch('[0-9a-fA-F]{64}', digest):
                        raise TransferError('invalid catalog SHA-256')
                    record['sha256'] = digest.lower()
                if isinstance(item.get('notes'), str):
                    record['catalogNotes'] = item['notes'][:4096]
                if record['requiresBrowser']:
                    record['browserUrl'] = validate_url(item.get('browserUrl') or url)
                for field in ['version', 'region', 'archivePassword', 'requiredFirmware', 'maxFirmware']:
                    if isinstance(record[field], str) and not record[field].strip():
                        record[field] = None
                    if record[field] is not None:
                        text(record[field], field)
                releases.append(record)
            except (TransferError, ValueError, TypeError) as exc:
                rejected.append({'index': len(releases) + len(rejected), 'reason': str(exc)})
    if not releases:
        raise TransferError('catalog contains no valid base-game records')
    return releases, rejected


def refresh(store, cache=CACHE, reference='main', key_header=None):
    directory = cache / store
    directory.mkdir(parents=True, exist_ok=True)
    lock = directory / '.refresh.lock'
    try:
        descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
    except FileExistsError as exc:
        raise TransferError('catalog refresh lock exists; inspect the owning refresh before retrying') from exc
    os.close(descriptor)
    try:
        return _refresh(store, cache, reference, key_header)
    finally:
        lock.unlink(missing_ok=True)


def _refresh(store, cache, reference, key_header):
    if reference != 'main' and not re.fullmatch('[0-9a-f]{40}', reference):
        raise TransferError('catalog ref must be main or an immutable commit')
    repository = REPOS[store]
    commit = json.loads(fetch(f'https://api.github.com/repos/{repository}/commits/{reference}', 1024 * 1024)).get('sha')
    if not isinstance(commit, str) or not re.fullmatch('[0-9a-f]{40}', commit):
        raise TransferError('upstream did not resolve an immutable commit')
    paths = ['catalog/catalogue-v2.json', 'catalog/catalogue-ps4.json'] if store == 'blackbox' else ['catalogue-v8.enc']
    raw = {path: fetch(f'https://raw.githubusercontent.com/{repository}/{commit}/{path}', LIMIT + 40) for path in paths}
    header = key_header or cache / 'orbit/catalog_key.h'
    documents = [json.loads(data) for data in raw.values()] if store == 'blackbox' else [orbit_decode(next(iter(raw.values())), header)]
    rows, rejected = normalize(store, documents, commit)
    snapshot = {'schema_version': 1, 'adapter_version': ADAPTER_VERSION, 'store': store, 'repository': repository, 'commit': commit,
                'fetched_at': datetime.now(timezone.utc).isoformat(),
                'input_sha256': {p: hashlib.sha256(v).hexdigest() for p, v in raw.items()},
                'releases': rows, 'rejected': rejected}
    # Immutable snapshots; pointer replacement is the only publication step.
    directory = cache / store
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / (commit + f'-adapter{ADAPTER_VERSION}.json')
    if path.exists():
        previous = bounded_json(path, 32 * 1024 * 1024)
        if previous.get('releases') != rows or previous.get('input_sha256') != snapshot['input_sha256']:
            raise TransferError('existing immutable snapshot differs; refusing overwrite')
    else:
        _atomic_json(path, snapshot)
    pointer = {'schema_version': 1, 'adapter_version': ADAPTER_VERSION, 'commit': commit, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
    current = directory / 'current.json'
    if current.exists() and bounded_json(current, 4096) != pointer:
        _atomic_json(directory / 'previous.json', bounded_json(current, 4096))
    _atomic_json(current, pointer)
    return {'store': store, 'commit': commit, 'releases': len(rows), 'titles': len({r['titleId'] for r in rows}), 'rejected': len(rejected)}


def load_snapshot(store, cache):
    pointer = bounded_json(cache / store / 'current.json', 4096)
    commit = pointer.get('commit')
    if not isinstance(commit, str) or not re.fullmatch('[0-9a-f]{40}', commit):
        raise TransferError('invalid catalog pointer')
    if pointer.get('adapter_version') != ADAPTER_VERSION:
        raise TransferError(f'{store} snapshot requires refresh for this adapter version')
    path = cache / store / (commit + f'-adapter{ADAPTER_VERSION}.json')
    snapshot = bounded_json(path, 32 * 1024 * 1024)
    if hashlib.sha256(path.read_bytes()).hexdigest() != pointer.get('sha256') or snapshot.get('store') != store or snapshot.get('commit') != commit:
        raise TransferError('catalog snapshot integrity mismatch')
    return snapshot


def js_array(data, name):
    """Parse only the bounded JSON array; never evaluate JavaScript."""
    match = re.search(r'\bvar\s+' + re.escape(name) + r'\s*=\s*\[', data)
    if not match:
        if name == 'twins':
            return []
        raise TransferError('Overdrive games array missing')
    i = match.end() - 1
    output, depth, inside = [], 0, False
    while i < len(data):
        char = data[i]
        if inside and char == '\\':
            if data[i:i+2] == '\\x' and re.fullmatch('[0-9a-fA-F]{2}', data[i+2:i+4]):
                output.append('\\u00' + data[i+2:i+4]); i += 4; continue
            output.append(data[i:i+2]); i += 2; continue
        if char == '"': inside = not inside
        if not inside:
            if char == '[': depth += 1
            elif char == ']': depth -= 1
            if char == ',':
                ahead = i + 1
                while ahead < len(data) and data[ahead].isspace(): ahead += 1
                if ahead < len(data) and data[ahead] in ']}': i += 1; continue
        output.append(char); i += 1
        if depth == 0 and not inside: return json.loads(''.join(output))
    raise TransferError('unclosed Overdrive catalog array')


def build(overdrive, cache=CACHE, *, use_delta=True):
    original_overdrive = Path(overdrive)
    delta_origin = None
    if use_delta:
        from ps5_catalog_delta import select
        overdrive = select(overdrive, cache)
        if overdrive != original_overdrive:
            receipt = bounded_json(overdrive.parent / 'snapshot.json', 8*1024*1024)
            delta_origin = {'store': 'Overdrive', 'repository': 'MyLuxy/overdrive', 'commit': receipt['feed_commit'], 'base_commit': receipt['base_commit']}
    catalog = overdrive / 'catalog.js'
    raw = bounded_bytes(catalog)
    inputs = {catalog: (hashlib.sha256(raw).hexdigest(), LIMIT)}
    data = raw.decode('utf-8-sig')
    blocks = {'games': js_array(data, 'games'), 'twins': js_array(data, 'twins')}
    titles, releases = {}, {}
    for block in blocks.values():
        for game in block:
            title = game.get('id')
            if not isinstance(title, str) or not TITLE.fullmatch(title) or title in titles:
                raise TransferError('invalid or duplicate Overdrive title ID')
            titles[title] = game
            game['catalogs'] = ['Overdrive']
            path = overdrive / 'releases' / (title + '.json')
            raw = bounded_bytes(path, 8 * 1024 * 1024)
            inputs[path] = (hashlib.sha256(raw).hexdigest(), 8 * 1024 * 1024)
            records = json.loads(raw.decode('utf-8-sig'))
            if not isinstance(records, list): raise TransferError('invalid Overdrive release list')
            if delta_origin:
                for row in records:
                    row.setdefault('catalogProvenance', dict(delta_origin, release_id=row['id']))
            releases[title] = records
    origins = [delta_origin] if delta_origin else []
    for store in REPOS:
        if not (cache / store / 'current.json').exists(): continue
        snapshot = load_snapshot(store, cache)
        origins.append({k: snapshot[k] for k in ['store', 'commit', 'fetched_at']})
        for row in snapshot['releases']:
            title = row['titleId']
            if title not in titles:
                game = {'id': title, 'title': row['title'], 'platform': row['platform'], 'providers': [], 'region': row.get('region'), 'catalogs': []}
                blocks['games'].append(game); titles[title] = game; releases[title] = []
            game = titles[title]
            if game['platform'] != row['platform']: raise TransferError('merged catalog platform conflict')
            if store not in game['catalogs']: game['catalogs'].append(store)
            if row['provider'] not in game.get('providers', []): game.setdefault('providers', []).append(row['provider'])
            releases[title].append(row)
    for title, rows in releases.items():
        content_formats = sorted({r['contentFormat'] for r in rows if r.get('contentFormat')})
        if content_formats:
            titles[title]['contentFormats'] = content_formats
    encoded = ('var games = ' + json.dumps(blocks['games'], ensure_ascii=True) + ';\nvar twins = ' + json.dumps(blocks['twins'], ensure_ascii=True) + ';\n').encode()
    if len(encoded) > LIMIT: raise TransferError('combined catalog exceeds Rust reader limit')
    blobs = {'catalog.js': encoded}
    for title, rows in releases.items():
        # Only exact semantic duplicates merge. Different versions, formats,
        # identities, passwords, hashes or firmware claims remain separate.
        unique = {}
        for row in rows:
            identity = (row.get('titleId', title), row.get('url'), row.get('browserUrl'),
                        row.get('format'), row.get('artifactFormat'), row.get('contentFormat'), row.get('version'),
                        row.get('archivePassword'), row.get('sha256'), row.get('region'),
                        row.get('requiredFirmware'), row.get('maxFirmware'), row.get('category'))
            if identity in unique:
                prior = unique[identity]
                if 'catalogProvenance' in row:
                    prior.setdefault('catalogOrigins', []).append(row['catalogProvenance'])
            else:
                unique[identity] = row
                if 'catalogProvenance' in row:
                    row['catalogOrigins'] = [row['catalogProvenance']]
        rows = list(unique.values())
        payload = json.dumps(rows, ensure_ascii=True).encode()
        if len(payload) > 8 * 1024 * 1024: raise TransferError('combined release list oversized')
        blobs['releases/' + title + '.json'] = payload
    manifest = {name: hashlib.sha256(value).hexdigest() for name, value in sorted(blobs.items())}
    for path, (digest, limit) in inputs.items():
        if hashlib.sha256(bounded_bytes(path, limit)).hexdigest() != digest:
            raise TransferError('Overdrive catalog changed during build; retry without publishing mixed metadata')
    key = hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest()
    destination = cache / 'combined' / key
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        for name, digest in manifest.items():
            if hashlib.sha256((destination / name).read_bytes()).hexdigest() != digest: raise TransferError('combined snapshot changed')
        return destination
    stage = Path(tempfile.mkdtemp(prefix='stage-', dir=destination.parent))
    try:
        for name, value in blobs.items():
            target = stage / name; target.parent.mkdir(parents=True, exist_ok=True); target.write_bytes(value)
        _atomic_json(stage / 'provenance.json', {'stores': origins, 'files': manifest, 'titles': len(titles)})
        os.rename(stage, destination)
    except Exception:
        # Keep failed staging for diagnosis; never recursively delete user paths.
        raise
    return destination


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['refresh', 'build'])
    parser.add_argument('store', nargs='?', choices=['blackbox', 'orbit', 'overdrive', 'all'], default='all')
    parser.add_argument('--cache', type=Path, default=CACHE)
    parser.add_argument('--overdrive', type=Path, default=ROOT / 'repos/overdrive/app/data')
    parser.add_argument('--ref', default='main')
    parser.add_argument('--orbit-key-header', type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == 'build': print(build(args.overdrive, args.cache))
        else:
            failed = False
            for store in [*REPOS, 'overdrive'] if args.store == 'all' else [args.store]:
                try:
                    if store == 'overdrive':
                        from ps5_catalog_delta import refresh as refresh_delta
                        result = refresh_delta(args.overdrive, args.cache)
                    else:
                        result = refresh(store, args.cache, args.ref, args.orbit_key_header)
                    print(json.dumps(result))
                except (TransferError, OSError, ValueError, KeyError, TypeError) as exc:
                    failed = True
                    print(f'{store} refresh stopped: {exc}', file=sys.stderr)
            if failed: return 1
        return 0
    except (TransferError, OSError, ValueError, KeyError, TypeError) as exc:
        print(f'Catalog operation stopped: {exc}', file=sys.stderr)
        return 1


if __name__ == '__main__': raise SystemExit(main())
