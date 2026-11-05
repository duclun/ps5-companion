import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import ps5_catalog as c
import ps5_catalog_delta as delta
from pragmata_transfer import TransferError


def game(tid, title='Synthetic'):
    return {'id':tid,'title':title,'platform':'ps5','providers':[],'genres':[],'features':[],'art':{}}


def fixture(root):
    root.mkdir()
    (root/'releases').mkdir()
    (root/'catalog.js').write_text('var games = '+json.dumps([game('PPSA00001')])+'; var twins = [];')
    (root/'releases/PPSA00001.json').write_text('[]')
    return delta.inventory(root)


def feed():
    return {'format':1,'version':'1.0.10','base':'a'*40,'games':[{'block':'twins','line':game('PPSA00002','New edition')}],
            'removed':[], 'releases':{'PPSA00002':[{'id':'s1','filename':'game.zip','format':'zip','url':'https://example.test/a'}]},'pages':{}}


def metadata(blobs, update):
    tree=[{'path':'app/data/'+p,'mode':'100644','type':'blob','sha':hashlib.sha1(b'blob '+str(len(b)).encode()+b'\0'+b).hexdigest()} for p,b in blobs.items()]
    def fetch(url,*args):
        if url.endswith('/commits/catalog'):return json.dumps({'sha':'b'*40}).encode()
        if '/commits/v' in url:return json.dumps({'sha':'a'*40,'commit':{'tree':{'sha':'c'*40}}}).encode()
        if '/git/trees/' in url:return json.dumps({'sha':'c'*40,'truncated':False,'tree':tree}).encode()
        if url.endswith('/app/version.js'):return b'var VERSION = "1.0.10"; export { VERSION };'
        if url.endswith('/catalog-update.json'):return json.dumps(update).encode()
        raise AssertionError(url)
    return fetch


class DeltaTests(unittest.TestCase):
    def test_materialize_preserves_base_and_requires_complete_active_sources(self):
        with tempfile.TemporaryDirectory() as temp:
            blobs=fixture(Path(temp)/'base');original=dict(blobs)
            result=delta.materialize(blobs,feed())
            self.assertEqual(blobs,original)
            blocks,ids=delta.validate(result)
            self.assertEqual(ids,{'PPSA00001','PPSA00002'})
            self.assertEqual(blocks['twins'][0]['title'],'New edition')
            bad=feed();bad['releases']={}
            with self.assertRaisesRegex(TransferError,'missing'):
                delta.materialize(blobs,bad)

    def test_unsafe_duplicate_cross_platform_and_uncertain_deletion_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            blobs=fixture(Path(temp)/'base')
            mutations=[lambda f:f['games'].append(f['games'][0]),
                       lambda f:f['games'][0]['line'].update(platform='ps4'),
                       lambda f:f['releases'].update({'../escape':[]}),
                       lambda f:f.update(removed=['PPSA99999']),
                       lambda f:f['releases']['PPSA00002'][0].update(filename='../bad.zip'),
                       lambda f:f['releases']['PPSA00002'][0].update(url={}),
                       lambda f:f['releases']['PPSA00002'][0].update(url='https://example.test/a\r\nhttps://example.test/b'),
                       lambda f:f['releases']['PPSA00002'][0].update(sizeBytes='100'),
                       lambda f:f['releases'].update({'PPSA00001':None})]
            for change in mutations:
                f=feed();change(f)
                with self.subTest(feed=f),self.assertRaises(TransferError):delta.materialize(blobs,f)
            removed=feed();removed['removed']=['PPSA00001'];removed['releases']['PPSA00001']=None
            result=delta.materialize(blobs,removed)
            self.assertNotIn('releases/PPSA00001.json',result)
            self.assertEqual(delta.validate(result)[1],{'PPSA00002'})

    def test_metadata_json_duplicates_and_nonfinite_refused(self):
        for raw in [b'{"x":1,"x":2}',b'{"x":NaN}']:
            with self.assertRaises(TransferError):delta.parse(raw)

    def test_publication_pins_snapshot_and_offline_build_uses_it(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);base=root/'base';blobs=fixture(base);cache=root/'cache'
            def verify(stage):self.assertEqual(delta.validate(delta.inventory(stage))[1],{'PPSA00001','PPSA00002'})
            with patch.object(c,'fetch',side_effect=metadata(blobs,feed())):
                result=delta.refresh(base,cache,verify=verify)
            self.assertEqual(delta.inventory(base),blobs)
            selected=delta.select(base,cache)
            self.assertEqual(str(selected),result['snapshot'])
            with patch.object(c,'fetch',side_effect=AssertionError('offline')):
                combined=c.build(base,cache)
                self.assertTrue((combined/'releases/PPSA00002.json').exists())
                source=json.loads((combined/'releases/PPSA00002.json').read_text())[0]
                self.assertEqual(source['catalogOrigins'][0]['commit'], 'b'*40)
                self.assertEqual(source['catalogOrigins'][0]['store'], 'Overdrive')
            (selected/'releases/PPSA00002.json').write_text('[]')
            with self.assertRaisesRegex(TransferError,'snapshot changed'):delta.select(base,cache)

    def test_bad_version_base_and_failed_staging_keep_last_good_pointer(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);base=root/'base';blobs=fixture(base);cache=root/'cache'
            with patch.object(c,'fetch',side_effect=metadata(blobs,feed())):
                delta.refresh(base,cache,verify=lambda stage:None)
            pointer=(cache/'overdrive/current.json').read_bytes()
            bad=feed();bad['version']='9.0.0'
            with patch.object(c,'fetch',side_effect=metadata(blobs,bad)):
                with self.assertRaises(TransferError):delta.refresh(base,cache,verify=lambda stage:None)
            changed=feed();changed['games'][0]['line']['title']='Different content'
            with patch.object(c,'fetch',side_effect=metadata(blobs,changed)):
                with self.assertRaisesRegex(TransferError,'test verifier'):
                    delta.refresh(base,cache,verify=lambda stage:(_ for _ in ()).throw(TransferError('test verifier failed')))
            self.assertEqual((cache/'overdrive/current.json').read_bytes(),pointer)
            self.assertFalse((cache/'overdrive/.refresh.lock').exists())
            (base/'releases/PPSA00001.json').write_text('[{"local":"edit"}]')
            with patch.object(c,'fetch',side_effect=metadata(blobs,feed())):
                with self.assertRaisesRegex(TransferError,'differs from release base'):delta.refresh(base,cache,verify=lambda stage:None)
            self.assertEqual((cache/'overdrive/current.json').read_bytes(),pointer)

    def test_concurrent_checkout_edit_does_not_publish_and_lock_not_stolen(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);base=root/'base';blobs=fixture(base);cache=root/'cache'
            def verify(stage):
                (base/'releases/PPSA00001.json').write_text('[{"edited":true}]')
            with patch.object(c,'fetch',side_effect=metadata(blobs,feed())):
                with self.assertRaisesRegex(TransferError,'changed during refresh'):delta.refresh(base,cache,verify=verify)
            self.assertFalse((cache/'overdrive/current.json').exists())
            (cache/'overdrive/.refresh.lock').touch()
            with self.assertRaisesRegex(TransferError,'lock exists'):delta.refresh(base,cache,verify=lambda stage:None)
            self.assertTrue((cache/'overdrive/.refresh.lock').exists())

    def test_crlf_checkout_matches_only_exact_official_lf_blob(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);base=root/'base';blobs=fixture(base);cache=root/'cache'
            canonical={k:v.replace(b'\r\n',b'\n') for k,v in blobs.items()}
            raw=blobs['catalog.js']+b'\r\n'
            (base/'catalog.js').write_bytes(raw)
            canonical['catalog.js']=raw.replace(b'\r\n',b'\n')
            with patch.object(c,'fetch',side_effect=metadata(canonical,feed())):
                delta.refresh(base,cache,verify=lambda stage:None)
            self.assertEqual((base/'catalog.js').read_bytes(),raw)
            self.assertTrue(delta.select(base,cache).exists())

    def test_all_refreshes_continue_when_one_store_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            def store_refresh(store,*args):
                if store=='blackbox':raise TransferError('offline')
                return {'store':store}
            with patch.object(c,'refresh',side_effect=store_refresh) as stores,patch.object(delta,'refresh',return_value={'store':'overdrive'}) as od:
                self.assertEqual(c.main(['refresh','all','--cache',temp]),1)
                self.assertEqual([call.args[0] for call in stores.call_args_list],['blackbox','orbit'])
                od.assert_called_once()

    def test_pointer_traversal_and_duplicate_edition_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);base=root/'base';blobs=fixture(base);directory=root/'cache/overdrive';directory.mkdir(parents=True)
            (directory/'current.json').write_text('{"snapshot":"../elsewhere"}')
            with self.assertRaises(TransferError):delta.select(base,root/'cache')
            blobs['catalog.js']=('var games = '+json.dumps([game('PPSA00001')])+'; var twins = '+json.dumps([game('PPSA00001')])+';').encode()
            with self.assertRaisesRegex(TransferError,'duplicate catalog'):delta.validate(blobs)


if __name__=='__main__':unittest.main()
