import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import ps5_catalog as catalog
from pragmata_transfer import TransferError


def release(**values):
    row = {'id': 'source-1', 'titleId': 'PPSA00001', 'title': 'Synthetic game', 'platform': 'ps5',
           'url': 'https://example.test/game.zip', 'sourceId': 'archive', 'provider': 'Archive.org',
           'format': 'Game folder', 'compression': 'zip', 'filename': 'game.zip', 'sizeBytes': 100,
           'version': '1.0', 'region': 'EUR'}
    row.update(values)
    return row


def envelope(rows, orbit=False):
    return {'schemaVersion': 1, 'feedVersion': 8 if orbit else None, 'revision': 1, 'releases': rows}


class CatalogTests(unittest.TestCase):
    def test_normalizes_provenance_identity_and_supported_fields(self):
        rows, rejected = catalog.normalize('orbit', [envelope([release(requiredFirmware='11.20', sha256='a'*64)], True)], 'a'*40)
        self.assertFalse(rejected)
        self.assertEqual(rows[0]['titleId'], 'PPSA00001')
        self.assertEqual(rows[0]['format'], 'zip')
        self.assertEqual(rows[0]['artifactFormat'], 'Game folder')
        self.assertEqual(rows[0]['requiredFirmware'], '11.20')
        self.assertEqual(rows[0]['catalogProvenance']['commit'], 'a'*40)
        self.assertEqual(rows[0]['sha256'], 'a'*64)
        rows, rejected = catalog.normalize('blackbox', [envelope([release(version='', region='')])], 'a'*40)
        self.assertFalse(rejected)
        self.assertIsNone(rows[0]['version'])

    def test_quarantines_invalid_rows_and_keeps_dlc_out(self):
        source = [release(), release(id='bad', titleId='../escape'),
                  release(id='wrong', platform='ps4'), release(id='installer', filename='../bad.zip'),
                  release(id='hash', sha256='invalid'), release(id='dlc', addonId='expansion')]
        rows, rejected = catalog.normalize('blackbox', [envelope(source)], 'a'*40)
        self.assertEqual(len(rows), 1)
        self.assertEqual(len(rejected), 4)
        with self.assertRaises(TransferError):
            catalog.normalize('orbit', [envelope(source)], 'a'*40)

    def test_authenticates_orbit_metadata_and_rejects_wrong_purpose(self):
        try:
            from cryptography.hazmat.primitives.ciphers.aead import AESGCM
        except ImportError:
            self.skipTest('catalog crypto dependency required')
        with tempfile.TemporaryDirectory() as temp:
            header = Path(temp)/'key.h'
            header.write_text('static const unsigned char catalog_key[32] = {' + ','.join(['0x00']*32) + '};')
            aad = b'ORBITENC\x01\x01\x01\x00' + bytes(12)
            sealed = aad + AESGCM(bytes(32)).encrypt(bytes(12), b'{"test":true}', aad)
            self.assertEqual(catalog.orbit_decode(sealed, header), {'test': True})
            for bad in [sealed[:-1]+bytes([sealed[-1]^1]), b'bad', sealed[:10]+b'\x02'+sealed[11:]]:
                with self.assertRaises(TransferError): catalog.orbit_decode(bad, header)

    def test_refresh_failure_preserves_last_good_and_conflicts_refuse(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            def fetch(url, *args):
                return json.dumps({'sha': 'a'*40} if '/commits/' in url else envelope([release()])).encode()
            with patch.object(catalog, 'fetch', side_effect=fetch):
                catalog.refresh('blackbox', cache)
            pointer = (cache/'blackbox/current.json').read_bytes()
            with patch.object(catalog, 'fetch', side_effect=TransferError('offline')):
                with self.assertRaises(TransferError): catalog.refresh('blackbox', cache)
            self.assertEqual((cache/'blackbox/current.json').read_bytes(), pointer)
            self.assertFalse((cache/'blackbox/.refresh.lock').exists())
            with patch.object(catalog, 'fetch', side_effect=lambda url,*a: json.dumps({'sha':'a'*40} if '/commits/' in url else envelope([release(title='Changed')])).encode()):
                with self.assertRaisesRegex(TransferError, 'immutable'): catalog.refresh('blackbox', cache)
            self.assertEqual(catalog.load_snapshot('blackbox', cache)['commit'], 'a'*40)

    def test_build_is_offline_preserves_editions_and_deduplicates_with_provenance(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); overdrive = root/'overdrive'; cache=root/'cache'
            (overdrive/'releases').mkdir(parents=True)
            (overdrive/'catalog.js').write_text('var games = [{"id":"PPSA00001","title":"Synthetic game","platform":"ps5"},]; var twins = [{"id":"PPSA00002","title":"Synthetic EU","platform":"ps5"}]; throw Error("not executed");')
            for title in ['PPSA00001','PPSA00002']: (overdrive/'releases'/f'{title}.json').write_text('[]')
            for store in ['blackbox','orbit']:
                rows,_ = catalog.normalize(store,[envelope([release(),release(id='new',titleId='PPSA00003',title='New title')],store=='orbit')],'a'*40)
                directory=cache/store;directory.mkdir(parents=True)
                path=directory/('a'*40+f'-adapter{catalog.ADAPTER_VERSION}.json');path.write_text(json.dumps({'store':store,'commit':'a'*40,'fetched_at':'today','releases':rows}))
                (directory/'current.json').write_text(json.dumps({'commit':'a'*40,'adapter_version':catalog.ADAPTER_VERSION,'sha256':hashlib.sha256(path.read_bytes()).hexdigest()}))
            with patch.object(catalog,'fetch',side_effect=AssertionError('network')):
                destination=catalog.build(overdrive,cache)
                self.assertEqual(catalog.build(overdrive,cache),destination)
            rows=json.loads((destination/'releases/PPSA00001.json').read_text())
            self.assertEqual(len(rows),1)
            self.assertEqual({o['store'] for o in rows[0]['catalogOrigins']},{'orbit','blackbox'})
            source=(destination/'catalog.js').read_text()
            self.assertEqual(catalog.js_array(source,'twins')[0]['id'],'PPSA00002')
            self.assertIn('PPSA00003',source)
            (destination/'catalog.js').write_text('tampered')
            with self.assertRaisesRegex(TransferError,'changed'):catalog.build(overdrive,cache)

    def test_snapshot_pointer_traversal_and_tampering_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            cache=Path(temp);(cache/'orbit').mkdir()
            (cache/'orbit/current.json').write_text(json.dumps({'commit':'../escape'}))
            with self.assertRaises(TransferError):catalog.load_snapshot('orbit',cache)

    def test_js_parser_does_not_execute_and_preserves_string_escapes(self):
        source=r'var games = [{"id":"PPSA00001","title":"Name\xAE,]"},]; doSomething();'
        self.assertEqual(catalog.js_array(source,'games')[0]['title'],'Name\u00ae,]')
        self.assertEqual(catalog.js_array(source,'twins'),[])
        with self.assertRaises(TransferError):catalog.js_array('var games = [','games')


if __name__ == '__main__': unittest.main()
