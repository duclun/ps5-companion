import hashlib
import json
import struct
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import ps5_download as dl
from ps5_pkg import inspect_pkg
from test_pragmata_transfer import FakeResponse


def package(title='PPSA00001', *, fih=False, content_type=None, flags=0):
    data = bytearray(1024)
    base = 256 if fih else 0
    if fih:
        data[:4] = b'\x7fFIH'
        struct.pack_into('<H', data, 6, 3)
        struct.pack_into('<Q', data, 0x58, base)
    data[base:base+4] = b'\x7fCNT'
    struct.pack_into('>I', data, base+0x10, 1)
    struct.pack_into('>I', data, base+0x18, 0xa0)
    cid = f'UP0001-{title}_00-ABCDEFGHIJKLMNOP'.encode()
    data[base+0x40:base+0x40+len(cid)] = cid
    struct.pack_into('>II', data, base+0x74, content_type or (0x20 if title.startswith('PPSA') else 0x1a), flags)
    struct.pack_into('>III', data, base+0xa0+0x10, 0x200, 32, 0)
    return bytes(data)


class PackageTests(unittest.TestCase):
    def inspect(self, data, title='PPSA00001'):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/'game.pkg'
            path.write_bytes(data)
            return inspect_pkg(path, title)

    def test_cnt_fih_and_ps4_identity(self):
        for title, fih in [('PPSA00001', False), ('PPSA00001', True), ('CUSA00001', False)]:
            result = self.inspect(package(title, fih=fih), title)
            self.assertEqual(result['title_id'], title)
            self.assertEqual(result['authenticity'], 'not_verified')
            self.assertEqual(result['envelope'], 'FIH' if fih else 'CNT')

    def test_malformed_headers_bounds_and_identity_fail_closed(self):
        variants = [b'MZ'+b'\0'*1022, b'<html>ad</html>', package('PPSA00002'), package()[:180],
                    package(content_type=0x1a), package(content_type=0x21), package(flags=0x40000000)]
        for offset, fmt, value in [(0x10, '>I', 1025), (0x18, '>I', 1020), (0xb4, '>I', 100000)]:
            data=bytearray(package()); struct.pack_into(fmt,data,offset,value);variants.append(data)
        for offset, fmt, value in [(6, '<H', 4), (0x58, '<Q', 0), (0x58, '<Q', 100000)]:
            data=bytearray(package(fih=True));struct.pack_into(fmt,data,offset,value);variants.append(data)
        for data in variants:
            with self.subTest(prefix=data[:8]), self.assertRaises(dl.TransferError):
                self.inspect(data)

    def test_default_excludes_packages_explicit_mode_excludes_archives_images_and_passwords(self):
        pkg={'url':'https://example.test/game.pkg', 'format':'pkg'}
        archive=dict(pkg, format='7z', artifactFormat='FPKG')
        image=dict(pkg, format='exfat')
        self.assertEqual(dl.candidates([pkg]), [])
        self.assertEqual(dl.candidates([pkg, archive, image, dict(pkg,archivePassword='pw')],package_only=True), [[pkg]])
        self.assertEqual(dl.candidates([dict(pkg,label='Base part 1'),dict(pkg,label='Base part 2')],package_only=True), [])

    def test_package_upload_and_implicit_selection_rejected_before_network_or_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            (root/'PPSA00001.json').write_text(json.dumps([{'format':'pkg','url':'https://example.test/game.pkg'}]))
            with patch.object(dl,'download_group') as download:
                for config in [{'pipeline': {'cli': __file__}}, {}, {'source_number': 2}]:
                    with self.assertRaises(dl.TransferError):
                        dl.run('PPSA00001', root, root/'out',package_only=True,**config)
                download.assert_not_called()
            self.assertFalse((root/'out').exists())

    def test_real_download_validates_and_records_no_install_for_explicit_source(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); payload=package('CUSA00001')
            record={'format':'pkg','platform':'ps4','url':'https://example.test/game.pkg','sha256':hashlib.sha256(payload).hexdigest()}
            (root/'CUSA00001.json').write_text(json.dumps([record]))
            with patch.object(dl,'probe_url',return_value={'is_html':False,'status':200,'content_length':len(payload)}), \
                 patch('pragmata_transfer.urlopen',return_value=FakeResponse(payload,headers={'Content-Length':str(len(payload))},final_url=record['url'])), \
                 patch('ps5_pipeline.run_pipeline') as pipeline:
                dl.run('CUSA00001',root,root/'out',package_only=True,source_number=1)
                pipeline.assert_not_called()
            receipt=json.loads(next((root/'out').glob('*/package-receipt.json')).read_text())
            self.assertEqual(receipt['state'],'package_locally_validated')
            self.assertFalse(receipt['installation_submitted'])
            self.assertEqual(receipt['file']['sha256'],record['sha256'])

    def test_invalid_package_stops_without_fallback_and_retains_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); payload=package('PPSA00002')
            records=[{'format':'pkg','url':f'https://example.test/game{i}.pkg'} for i in (1,2)]
            (root/'PPSA00001.json').write_text(json.dumps(records))
            with patch.object(dl,'probe_url',return_value={'is_html':False,'status':200,'content_length':len(payload)}), \
                 patch('pragmata_transfer.urlopen',return_value=FakeResponse(payload,headers={'Content-Length':str(len(payload))},final_url=records[0]['url'])) as transport:
                with self.assertRaises(dl.PackageValidationError):
                    dl.run('PPSA00001',root,root/'out',package_only=True,source_number=1)
                self.assertEqual(transport.call_count,1)
            self.assertTrue(list((root/'out').glob('*/*.pkg')))
            self.assertFalse(list((root/'out').glob('*/package-receipt.json')))


if __name__ == '__main__':
    unittest.main()
