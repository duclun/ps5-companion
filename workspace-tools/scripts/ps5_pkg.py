"""Bounded, read-only PKG inspection. No console or installation operations."""
import argparse
import json
import re
import struct
from pathlib import Path

from pragmata_transfer import TransferError, _sha256_file


def inspect_pkg(path, title_id):
    """Check CNT/FIH structure and identity; this does not authenticate a package."""
    path = Path(path)
    if not re.fullmatch(r'(?:PPSA|CUSA)\d{5}', title_id):
        raise TransferError('invalid expected package title ID')
    if path.is_symlink() or not path.is_file():
        raise TransferError('PKG must be a regular local file, not a symlink')
    with path.open('rb') as stream:
        size = path.stat().st_size
        def read(offset, length):
            if offset < 0 or length < 0 or offset + length > size:
                raise TransferError('PKG structure extends beyond the file; file retained')
            stream.seek(offset)
            value = stream.read(length)
            if len(value) != length:
                raise TransferError('truncated PKG; file retained')
            return value
        head = read(0, 0xa0)
        base = 0
        envelope = 'CNT'
        if head[:4] == b'\x7fFIH':
            envelope = 'FIH'
            if struct.unpack_from('<H', head, 6)[0] != 3 or head[5] not in (0, 0x80):
                raise TransferError('unsupported PS5 FIH header version or signed marker')
            base = struct.unpack_from('<Q', head, 0x58)[0]
            if base < 0xa0:
                raise TransferError('invalid PS5 embedded CNT offset')
            head = read(base, 0xa0)
        if head[:4] != b'\x7fCNT':
            raise TransferError('file is not a supported CNT/FIH package (images are separate)')
        try:
            content_id = head[0x40:0x64].split(b'\0', 1)[0].decode('ascii')
        except UnicodeDecodeError as exc:
            raise TransferError('invalid package content ID') from exc
        match = re.fullmatch(r'[A-Z]{2}\d{4}-((?:PPSA|CUSA)\d{5})_\d{2}-[A-Z0-9]{16}', content_id)
        if not match or match[1] != title_id:
            raise TransferError('package content ID does not match the selected edition')
        content_type, flags = struct.unpack_from('>II', head, 0x74)
        platform = 'ps5' if title_id.startswith('PPSA') else 'ps4'
        if content_type not in ({0x20, 0x21, 0x26} if platform == 'ps5' else {0x1a, 0x1b}):
            raise TransferError('package platform/content type contradicts its title ID')
        if envelope == 'FIH' and platform != 'ps5':
            raise TransferError('PS5 FIH envelope cannot contain a PS4 title')
        if flags & 0x40100000 or content_type in {0x1b, 0x21}:
            raise TransferError('package is an update or DLC, not a base game')
        count = struct.unpack_from('>I', head, 0x10)[0]
        table = struct.unpack_from('>I', head, 0x18)[0]
        if not 1 <= count <= 1024 or table < 0xa0:
            raise TransferError('invalid or excessive package entry table')
        entries = read(base + table, count * 0x20)
        for index in range(count):
            entry = entries[index * 0x20:(index + 1) * 0x20]
            offset, length = struct.unpack_from('>II', entry, 0x10)
            if length and (offset < 0xa0 or base + offset + length > size):
                raise TransferError('package entry extends beyond the file')
    return {'title_id': title_id, 'platform': platform, 'content_id': content_id,
            'envelope': envelope, 'content_type': content_type, 'category': 'base',
            'bytes': size, 'entry_count': count,
            'validation': 'bounded_header_identity_and_entry_ranges',
            'authenticity': 'not_verified', 'playability': 'not_tested'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('path', type=Path)
    parser.add_argument('--title-id', required=True, type=str.upper)
    args = parser.parse_args()
    try:
        result = inspect_pkg(args.path, args.title_id)
        result['sha256'] = _sha256_file(args.path)
        print(json.dumps(result, indent=2))
        return 0
    except (TransferError, OSError, ValueError) as exc:
        print(f'PKG inspection stopped: {exc}')
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
