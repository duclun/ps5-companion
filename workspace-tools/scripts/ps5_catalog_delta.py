"""Pinned Overdrive delta materialization into immutable cache; never edits checkout."""
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import tempfile

from pragmata_transfer import TransferError, _atomic_json

REPO = 'MyLuxy/overdrive'
MAX_FEED = 4 * 1024 * 1024
MAX_TOTAL = 256 * 1024 * 1024
PATH = re.compile(r'(?:catalog\.js|(?:releases|pages)/(?:PPSA|CUSA)\d{5}\.json)\Z')
SHA = re.compile(r'[a-f0-9]{40}\Z')


def parse(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise TransferError('duplicate metadata key')
            result[key] = value
        return result
    def constant(value):
        raise TransferError('non-finite metadata number')
    try:
        return json.loads(raw, object_pairs_hook=pairs, parse_constant=constant)
    except (ValueError, RecursionError) as exc:
        raise TransferError("invalid bounded metadata JSON") from exc


def mapping(raw):
    value = parse(raw)
    if not isinstance(value, dict):
        raise TransferError("metadata object required")
    return value


def inventory(root):
    from ps5_catalog import bounded_bytes
    result = {}
    total = 0
    root = Path(root)
    if root.is_symlink() or root.is_junction():
        raise TransferError('linked catalog root refused')
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            item = Path(directory)/name
            if item.is_symlink() or item.is_junction():
                raise TransferError('linked catalog path refused')
        for name in files:
            path = Path(directory)/name
            relative = path.relative_to(root).as_posix()
            if not PATH.fullmatch(relative):
                raise TransferError('unexpected catalog path')
            raw = bounded_bytes(path, (16 if relative == 'catalog.js' else 8)*1024*1024)
            total += len(raw)
            if total > MAX_TOTAL or len(result) >= 20000:
                raise TransferError('catalog snapshot exceeds aggregate limits')
            result[relative] = raw
    if 'catalog.js' not in result:
        raise TransferError('catalog.js missing')
    return result


def hashes(blobs):
    return {p: hashlib.sha256(raw).hexdigest() for p, raw in sorted(blobs.items())}


def game(row):
    from ps5_catalog import TITLE, text
    if not isinstance(row, dict) or not isinstance(row.get('id'), str) or not TITLE.fullmatch(row['id']):
        raise TransferError('invalid game title ID')
    text(row.get('title'), 'title', 300)
    expected = 'ps5' if row['id'].startswith('PPSA') else 'ps4'
    if row.get('platform') != expected:
        raise TransferError('game platform contradicts title ID')
    for key in ('providers', 'genres', 'features', 'vr'):
        if key in row and (not isinstance(row[key], list) or len(row[key]) > 64 or any(not isinstance(v, str) or len(v) > 300 for v in row[key])):
            raise TransferError('invalid game list metadata')
    for key in ('recommendations', 'sizeGb', 'year', 'metacritic', 'work'):
        if key in row and row[key] is not None and (type(row[key]) not in (int, float) or abs(row[key]) > 10**18 or not math.isfinite(row[key])):
            raise TransferError('invalid game numeric metadata')
    for key in ('developer', 'publisher', 'releaseDate', 'added'):
        if key in row and (not isinstance(row[key], str) or len(row[key]) > 4096):
            raise TransferError('invalid game text metadata')
    if 'direct' in row and not isinstance(row['direct'], bool):
        raise TransferError('invalid direct-source flag')
    if 'art' in row and not isinstance(row['art'], dict):
        raise TransferError('invalid game art metadata')
    return row['id']


def validate(blobs):
    from ps5_catalog import js_array, TITLE, text
    source = blobs['catalog.js'].decode('utf-8-sig')
    blocks = {k: js_array(source, k) for k in ('games', 'twins')}
    seen = set()
    for rows in blocks.values():
        for row in rows:
            tid = game(row)
            if tid in seen:
                raise TransferError('duplicate catalog title ID')
            seen.add(tid)
    for tid in seen:
        if f'releases/{tid}.json' not in blobs:
            raise TransferError('active title missing its source list')
    for path, raw in blobs.items():
        if path == 'catalog.js':
            continue
        value = parse(raw)
        if path.startswith('releases/'):
            if not isinstance(value, list) or len(value) > 10000:
                raise TransferError('invalid source list')
            ids = set()
            for row in value:
                if not isinstance(row, dict):
                    raise TransferError('invalid source record')
                rid = text(row.get('id'), 'source ID', 200)
                text(row.get('format'), 'format', 20)
                name = text(row.get('filename'), 'filename', 220)
                if any(ch in name for ch in '/\\:') or name in {'.', '..'} or rid in ids:
                    raise TransferError('unsafe filename or duplicate source ID')
                ids.add(rid)
                for field in ('url', 'browserUrl', 'provider', 'label', 'version', 'region', 'archivePassword', 'requiredFirmware', 'maxFirmware', 'health', 'unavailableReason'):
                    value = row.get(field)
                    if value is not None and (not isinstance(value, str) or len(value) > 16384 or any(ord(ch) < 32 for ch in value)):
                        raise TransferError(f'invalid {field} metadata in {path}')
                for field in ('requiresBrowser', 'direct'):
                    if field in row and not isinstance(row[field], bool):
                        raise TransferError(f'invalid {field} flag in {path}')
                size = row.get('sizeBytes')
                if size is not None and (type(size) is not int or not 0 <= size < 2**63):
                    raise TransferError(f'invalid source size in {path}')
                if row.get('sha256') and (not isinstance(row['sha256'], str) or not re.fullmatch('[a-fA-F0-9]{64}',row['sha256'])):
                    raise TransferError(f'invalid source digest in {path}')
                if 'titleId' in row and (not isinstance(row['titleId'], str) or not TITLE.fullmatch(row['titleId'])):
                    raise TransferError('invalid artifact title ID')
        elif not isinstance(value, dict) or not isinstance(value.get('about'), list) or not isinstance(value.get('shots'), list):
            raise TransferError('invalid catalog page')
    return blocks, seen


def materialize(base_blobs, feed):
    from ps5_catalog import TITLE
    blocks, old_ids = validate(base_blobs)
    if not isinstance(feed, dict) or type(feed.get('format')) is not int or feed['format'] != 1:
        raise TransferError('unsupported delta format')
    if not isinstance(feed.get('games'), list) or len(feed['games']) > 10000 or not isinstance(feed.get('removed'), list):
        raise TransferError('invalid game changes')
    removed = feed['removed']
    if any(not isinstance(t, str) or not TITLE.fullmatch(t) for t in removed) or len(set(removed)) != len(removed) or not set(removed) <= old_ids:
        raise TransferError('invalid or uncertain title removal')
    changed = {}
    for row in feed['games']:
        if not isinstance(row, dict) or row.get('block') not in blocks:
            raise TransferError('invalid catalog block')
        tid = game(row.get('line'))
        if tid in changed or tid in removed:
            raise TransferError('duplicate or conflicting title changes')
        changed[tid] = row
    for key in blocks:
        blocks[key] = [g for g in blocks[key] if g['id'] not in changed and g['id'] not in removed]
    for row in changed.values():
        blocks[row['block']].append(row['line'])
    result = dict(base_blobs)
    for tid in removed:
        for folder in ('releases', 'pages'):
            result.pop(f'{folder}/{tid}.json', None)
    for folder in ('releases', 'pages'):
        values = feed.get(folder)
        if not isinstance(values, dict) or len(values) > 10000:
            raise TransferError('invalid delta file map')
        for tid, value in values.items():
            if not TITLE.fullmatch(tid) or tid not in old_ids | set(changed) | set(removed):
                raise TransferError('delta file refers to unknown title')
            path = f'{folder}/{tid}.json'
            if value is None:
                result.pop(path, None)
            else:
                if tid in removed:
                    raise TransferError('removed title still supplies metadata')
                raw = json.dumps(value, ensure_ascii=True).encode()
                if len(raw) > 8*1024*1024:
                    raise TransferError('delta file oversized')
                result[path] = raw
    result['catalog.js'] = ('\n'.join('var '+k+' = '+json.dumps(v, ensure_ascii=True)+';' for k,v in blocks.items())+'\n').encode()
    if len(result['catalog.js']) > 16*1024*1024 or sum(map(len, result.values())) > MAX_TOTAL:
        raise TransferError('materialized snapshot oversized')
    validate(result)
    return result


def verify_search(stage):
    from ps5_catalog import ROOT
    cli = ROOT/'repos/ps5-companion/target/release/ps5-companion.exe'
    if not cli.is_file():
        raise TransferError('release CLI required to validate staged search')
    for query in ('Bloodborne', 'Armored Core'):
        try:
            check = subprocess.run([str(cli), 'search', query, '--catalog-dir', str(stage)], capture_output=True, timeout=60)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise TransferError('staged catalog search could not be completed') from exc
        if check.returncode:
            raise TransferError('staged catalog search validation failed')


def refresh(overdrive, cache, *, verify=verify_search):
    import ps5_catalog as c
    directory = Path(cache)/'overdrive'
    if directory.is_symlink() or directory.is_junction():
        raise TransferError('linked cache directory refused')
    directory.mkdir(parents=True, exist_ok=True)
    lock = directory/'.refresh.lock'
    try:
        fd = os.open(lock, os.O_CREAT|os.O_EXCL|os.O_WRONLY)
    except FileExistsError as exc:
        raise TransferError('Overdrive refresh lock exists; inspect owner') from exc
    os.close(fd)
    try:
        head = mapping(c.fetch(f'https://api.github.com/repos/{REPO}/commits/catalog', 1024*1024)).get('sha')
        if not isinstance(head, str) or not SHA.fullmatch(head):
            raise TransferError('invalid immutable feed commit')
        raw = c.fetch(f'https://raw.githubusercontent.com/{REPO}/{head}/catalog-update.json', MAX_FEED)
        feed = parse(raw)
        if not isinstance(feed, dict):
            raise TransferError('delta must be an object')
        base, version = feed.get('base'), feed.get('version')
        if not isinstance(base, str) or not SHA.fullmatch(base) or not isinstance(version,str) or len(version) > 20 or not re.fullmatch(r'\d+\.\d+\.\d+', version):
            raise TransferError('invalid delta release/base identity')
        release = mapping(c.fetch(f'https://api.github.com/repos/{REPO}/commits/v{version}',1024*1024))
        declaration = c.fetch(f'https://raw.githubusercontent.com/{REPO}/{base}/app/version.js', 4096).decode('utf-8')
        match = re.search(r'\bvar\s+VERSION\s*=\s*"(\d+\.\d+\.\d+)"\s*;', declaration)
        if release.get('sha') != base or not match or match[1] != version:
            raise TransferError('delta does not match its official release/version')
        tree_sha = release.get('commit', {}).get('tree', {}).get('sha')
        if not isinstance(tree_sha, str) or not SHA.fullmatch(tree_sha):
            raise TransferError('release tree identity missing')
        tree = mapping(c.fetch(f'https://api.github.com/repos/{REPO}/git/trees/{tree_sha}?recursive=1'))
        if tree.get('truncated') is not False or tree.get('sha') != tree_sha or not isinstance(tree.get('tree'), list):
            raise TransferError('incomplete base tree')
        expected = {}
        for item in tree['tree']:
            if not isinstance(item, dict):
                raise TransferError('invalid base tree entry')
            path = item.get('path','')
            if not path.startswith('app/data/') or item.get('type') == 'tree':
                continue
            relative = path[len('app/data/'):]
            if not PATH.fullmatch(relative) or item.get('type') != 'blob' or item.get('mode') != '100644' or relative in expected:
                raise TransferError('unsupported base metadata path')
            expected[relative] = item.get('sha')
        original = inventory(overdrive)
        if set(original) != set(expected):
            raise TransferError('checkout catalog does not match the release base file set; preserve local edits')
        for path, blob in original.items():
            oid = hashlib.sha1(b'blob '+str(len(blob)).encode()+b'\0'+blob).hexdigest()
            if oid != expected[path]:
                # Git checkouts can convert LF to CRLF. Accept only this exact
                # reversible text conversion when it matches the official blob.
                canonical = blob.replace(b'\r\n', b'\n')
                oid = hashlib.sha1(b'blob '+str(len(canonical)).encode()+b'\0'+canonical).hexdigest()
            if oid != expected[path]:
                raise TransferError('checkout catalog differs from release base; preserve local edits')
        base_hashes = hashes(original)
        result = materialize(original, feed)
        manifest = hashes(result)
        identity = hashlib.sha256(json.dumps({'feed':head,'base':base,'files':manifest},sort_keys=True).encode()).hexdigest()
        destination = directory/identity
        receipt = {'schema_version':1,'feed_commit':head,'base_commit':base,'version':version,'base_files':base_hashes,'files':manifest,'feed_sha256':hashlib.sha256(raw).hexdigest()}
        if destination.is_symlink() or destination.is_junction():
            raise TransferError('linked snapshot refused')
        if destination.exists():
            if hashes(inventory(destination/'data')) != manifest or mapping(c.bounded_bytes(destination/'snapshot.json',8*1024*1024)) != receipt:
                raise TransferError('immutable delta snapshot changed')
        else:
            stage = Path(tempfile.mkdtemp(prefix='stage-',dir=directory))
            data = stage/'data'; data.mkdir()
            for name, blob in result.items():
                target=data/name; target.parent.mkdir(parents=True,exist_ok=True); target.write_bytes(blob)
            verify(data)
            c.build(data, Path(cache), use_delta=False)
            _atomic_json(stage/'snapshot.json', receipt)
            if stage.resolve().parent != directory.resolve() or destination.resolve().parent != directory.resolve():
                raise TransferError('snapshot move escaped cache directory')
            os.rename(stage,destination)
        if hashes(inventory(overdrive)) != base_hashes:
            raise TransferError('checkout changed during refresh; snapshot not published')
        snapshot = (destination/'snapshot.json').read_bytes()
        pointer={'schema_version':1,'snapshot':identity,'sha256':hashlib.sha256(snapshot).hexdigest()}
        current=directory/'current.json'
        if current.exists() and mapping(c.bounded_bytes(current,4096)) != pointer:
            _atomic_json(directory/'previous.json',mapping(c.bounded_bytes(current,4096)))
        _atomic_json(current,pointer)
        return {'store':'overdrive','feed_commit':head,'base_commit':base,'version':version,'snapshot':str(destination/'data'),'changed_entries':len(feed['games']),'removed_entries':len(feed['removed'])}
    finally:
        lock.unlink(missing_ok=True)


def select(overdrive, cache):
    import ps5_catalog as c
    directory=Path(cache)/'overdrive'; pointer=directory/'current.json'
    if directory.is_symlink() or directory.is_junction():
        raise TransferError('linked cache directory refused')
    if not pointer.exists():return Path(overdrive)
    info=mapping(c.bounded_bytes(pointer,4096));name=info.get('snapshot')
    if info.get('schema_version') != 1:
        raise TransferError('unsupported delta pointer version')
    if not isinstance(name,str) or not re.fullmatch('[a-f0-9]{64}',name):
        raise TransferError('invalid delta snapshot pointer')
    root=directory/name;raw=c.bounded_bytes(root/'snapshot.json',8*1024*1024)
    if hashlib.sha256(raw).hexdigest()!=info.get('sha256'):
        raise TransferError('delta snapshot receipt changed')
    snapshot=mapping(raw)
    if snapshot.get('schema_version') != 1:
        raise TransferError('unsupported delta snapshot version')
    if hashes(inventory(overdrive)) != snapshot.get('base_files'):
        raise TransferError('local catalog changed since delta publication; refresh Overdrive metadata first')
    blobs = inventory(root/'data')
    if hashes(blobs) != snapshot.get('files'):
        raise TransferError('delta snapshot changed')
    validate(blobs)
    return root/'data'
