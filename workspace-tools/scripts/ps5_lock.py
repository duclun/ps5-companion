"""Owner-aware download locks and local-only diagnostics/recovery. No network calls."""
import argparse
import contextlib
import ctypes
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import socket
import stat
import sys
import uuid

from pragmata_transfer import TransferError

ROOT = Path(__file__).resolve().parents[1]
LIMIT = 8192


def process_identity(pid):
    """Return live/gone/unknown plus a creation identity; never guess from age."""
    if type(pid) is not int or pid <= 0:
        return {'state': 'unknown'}
    if os.name == 'nt':
        from ctypes import wintypes as w
        k = ctypes.WinDLL('kernel32', use_last_error=True)
        k.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
        k.OpenProcess.restype = w.HANDLE
        k.GetProcessTimes.argtypes = [w.HANDLE] + [ctypes.POINTER(w.FILETIME)] * 4
        k.GetExitCodeProcess.argtypes = [w.HANDLE, ctypes.POINTER(w.DWORD)]
        k.CloseHandle.argtypes = [w.HANDLE]
        handle = k.OpenProcess(0x1000, False, pid)
        if not handle:
            return {'state': 'gone' if ctypes.get_last_error() == 87 else 'unknown'}
        try:
            times = [w.FILETIME() for _ in range(4)]
            code = w.DWORD()
            if not k.GetProcessTimes(handle, *[ctypes.byref(t) for t in times]) or not k.GetExitCodeProcess(handle, ctypes.byref(code)):
                return {'state': 'unknown'}
            created = (times[0].dwHighDateTime << 32) | times[0].dwLowDateTime
            return {'state': 'live' if code.value == 259 else 'gone', 'started': str(created)}
        finally:
            k.CloseHandle(handle)
    try:
        text = Path(f'/proc/{pid}/stat').read_text()
        started = text[text.rfind(')') + 2:].split()[19]
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        return {'state': 'live', 'started': boot + ':' + started}
    except FileNotFoundError:
        if Path('/proc/self/stat').exists():
            return {'state': 'gone'}
    except (OSError, IndexError):
        pass
    return {'state': 'unknown'}


def _regular(path):
    st = path.lstat()
    if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1 or getattr(st, 'st_file_attributes', 0) & 0x400:
        raise TransferError('Lock path is linked, shared or not a regular file; inspect manually')
    return st


def _read(path):
    st = _regular(path)
    if st.st_size > LIMIT:
        raise TransferError('Lock metadata exceeds size limit; inspect manually')
    with path.open('rb') as f:
        opened = os.fstat(f.fileno())
        if (st.st_dev, st.st_ino) != (opened.st_dev, opened.st_ino):
            raise TransferError('Lock changed while opening; retry diagnosis')
        raw = f.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise TransferError('Lock metadata grew beyond size limit')
    try:
        m = json.loads(raw)
    except (ValueError, UnicodeError):
        return raw, None
    valid = (isinstance(m, dict) and type(m.get('schema_version')) is int and m['schema_version'] == 1
             and type(m.get('pid')) is int and 0 < m['pid'] <= 0xffffffff
             and isinstance(m.get('host'), str) and bool(m['host']) and isinstance(m.get('started'), str)
             and isinstance(m.get('title_id'), str) and re.fullmatch(r'(PPSA|CUSA)\d{5}', m['title_id'])
             and bool(m['started']) and m.get('phase') in {'download', 'pipeline'}
             and isinstance(m.get('nonce'), str) and re.fullmatch('[0-9a-f]{32}', m['nonce']))
    return raw, m if valid else None


@contextlib.contextmanager
def _guard(output, create):
    """Stable OS-locked guard is never removed; marker mutations happen under it."""
    path = output / '.download.lock.guard'
    if path.exists() or path.is_symlink():
        _regular(path)
    elif not create:
        yield None
        return
    fd = os.open(path, os.O_RDWR | (os.O_CREAT if create else 0) | getattr(os, 'O_NOFOLLOW', 0), 0o600)
    locked = False
    try:
        st = os.fstat(fd)
        current = _regular(path)
        if (st.st_dev, st.st_ino) != (current.st_dev, current.st_ino):
            raise TransferError('OS guard changed; inspect manually')
        if create and st.st_size == 0:
            os.write(fd, b'0')
            os.fsync(fd)
        try:
            os.lseek(fd, 0, os.SEEK_SET)
            if os.name == 'nt':
                import msvcrt
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            locked = True
        except OSError as exc:
            raise TransferError('OS guard is busy or inaccessible; no recovery allowed') from exc
        yield fd
    finally:
        if locked:
            if os.name == 'nt':
                import msvcrt
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
            else:
                import fcntl
                fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _write_new(path, raw):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_NOFOLLOW', 0), 0o600)
    with os.fdopen(fd, 'wb') as f:
        f.write(raw)
        f.flush()
        os.fsync(f.fileno())


def _owner(m):
    if not m:
        return 'unknown'
    if m['host'].casefold() != socket.gethostname().casefold():
        return 'foreign_host'
    actual = process_identity(m['pid'])
    if actual['state'] == 'gone':
        return 'gone'
    if actual['state'] == 'live' and actual.get('started'):
        return 'live' if actual['started'] == m['started'] else 'pid_reused'
    return 'unknown'


def _pipeline_evidence(output):
    # Pipeline children can outlive the downloader. Do not infer safety from PID alone.
    return (output / '.pipeline').exists() or (output / '.pipeline').is_symlink()


def diagnose(output):
    output = Path(output)
    marker = output / '.download.lock'
    try:
        _, m = _read(marker)
    except FileNotFoundError:
        m = None
        exists = False
    else:
        exists = True
    try:
        with _guard(output, False) as fd:
            guard = 'free' if fd is not None else 'absent'
    except (OSError, TransferError):
        guard = 'busy_or_unknown'
    owner = _owner(m) if exists else 'none'
    pipeline = _pipeline_evidence(output)
    recoverable = exists and m is not None and owner in {'gone', 'pid_reused'} and m['phase'] == 'download' and not pipeline and guard != 'busy_or_unknown'
    return {'lock': str(marker), 'exists': exists, 'owner': owner, 'os_guard': guard,
            'metadata': m, 'pipeline_evidence_present': pipeline, 'recoverable': recoverable,
            'reason': 'Explicit local recovery permitted; artifacts stay intact' if recoverable else
                      'No recovery: live/unknown owner, legacy metadata, OS guard or pipeline evidence requires inspection' if exists else 'No download marker present'}


def recover(output, title_id=None):
    output = Path(output)
    # Does not create folders, start a download, query a console or resubmit a job.
    with _guard(output, True):
        marker = output / '.download.lock'
        raw, m = _read(marker)
        if not m or _owner(m) not in {'gone', 'pid_reused'}:
            raise TransferError('Owner is live, foreign or unknown (including legacy empty locks); recovery refused')
        if title_id is not None and m.get('title_id') != title_id:
            raise TransferError('Requested title does not own this lock; recovery refused')
        if m['phase'] != 'download' or _pipeline_evidence(output):
            raise TransferError('Pipeline/upload evidence requires receipt and job reconciliation; recovery refused')
        archive = output / ('.download.lock.recovered-' + uuid.uuid4().hex + '.json')
        _write_new(archive, raw)
        if _read(marker)[0] != raw:
            raise TransferError('Lock changed during recovery; original marker retained')
        marker.unlink()
        return {'recovered': True, 'archive': str(archive), 'artifacts_retained': True,
                'next': 'Rerun the original command explicitly; recovery did not restart it'}


class DownloadLock:
    def __init__(self, output, title_id, *, artifact_title_id=None):
        self.output = Path(output)
        self.path = self.output / '.download.lock'
        actual = process_identity(os.getpid())
        if actual['state'] != 'live' or not actual.get('started'):
            raise TransferError('Cannot establish current process creation identity; no download lock created')
        self.metadata = {'schema_version': 1, 'nonce': uuid.uuid4().hex, 'pid': os.getpid(),
                         'started': actual.get('started', ''), 'host': socket.gethostname(),
                         'created_utc': datetime.now(timezone.utc).isoformat(), 'phase': 'download', 'title_id': title_id, 'artifact_title_id': artifact_title_id or title_id}
        self.guard = None

    def acquire(self):
        self.guard = _guard(self.output, True)
        try:
            self.guard.__enter__()
        except (OSError, TransferError) as exc:
            self.guard = None
            raise TransferError(f'{exc}. Run ps5-lock diagnose {self.metadata["title_id"]} (use -Out for a custom output).') from exc
        try:
            _write_new(self.path, (json.dumps(self.metadata, indent=2) + '\n').encode())
        except Exception as exc:
            self.guard.__exit__(*sys.exc_info())
            self.guard = None
            if isinstance(exc, FileExistsError):
                raise TransferError(f'Download marker exists: {self.path}. Run ps5-lock diagnose {self.metadata["title_id"]} (use -Out for a custom output). No download started.') from exc
            raise

    def set_pipeline(self):
        _, old = _read(self.path)
        if not old or old['nonce'] != self.metadata['nonce']:
            raise TransferError('Lock ownership changed; pipeline not started')
        self.metadata['phase'] = 'pipeline'
        temp = self.output / ('.download.lock.' + uuid.uuid4().hex + '.tmp')
        _write_new(temp, (json.dumps(self.metadata, indent=2) + '\n').encode())
        os.replace(temp, self.path)

    def release(self):
        try:
            try:
                _, old = _read(self.path)
                if old and old['nonce'] == self.metadata['nonce']:
                    self.path.unlink()
            except FileNotFoundError:
                pass
        finally:
            if self.guard:
                self.guard.__exit__(None, None, None)
                self.guard = None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['diagnose', 'recover'])
    parser.add_argument('title_id', type=str.upper)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args(argv)
    if not re.fullmatch(r'(PPSA|CUSA)\d{5}', args.title_id):
        parser.error('Expected a PPSA or CUSA title ID')
    output = args.out or ROOT / 'downloads' / args.title_id
    try:
        print(json.dumps(diagnose(output) if args.command == 'diagnose' else recover(output, args.title_id), indent=2))
        return 0
    except (TransferError, OSError, ValueError) as exc:
        print(f'Lock operation stopped: {exc}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
