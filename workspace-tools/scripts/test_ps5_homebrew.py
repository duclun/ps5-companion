"""Offline homebrew adapter tests; no console or provider contacts."""
import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import ps5_homebrew as hb
from pragmata_transfer import TransferError
from test_pragmata_transfer import FakeResponse


def fixture():
    entry = {'id':'PPSA00001','titleid':'PPSA00001','name':'Synthetic Homebrew','version':'1.0','status':'available','category':'emulators','format':'zip','conflicts_with':[]}
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer,'w') as archive:
        archive.writestr('PPSA00001/eboot.bin', b'synthetic executable')
        archive.writestr('PPSA00001/sce_sys/param.json', json.dumps({'titleId':'PPSA00001'}))
    body = buffer.getvalue()
    detail = {**entry,'schema':3,'source_repo':'https://github.com/example/homebrew','artifact_url':'https://github.com/example/homebrew/releases/download/v1.0/PPSA00001.zip','size':len(body),'sha256':hashlib.sha256(body).hexdigest()}
    return entry, detail, body


class HomebrewTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.entry,self.detail,self.body=fixture()

    def test_shared_catalog_identities_are_kept_and_duplicates_rejected(self):
        shared={**self.entry,'id':'PPSA00001-other'}
        self.assertEqual(len(hb.index_entries({'schema':3,'apps':[self.entry],'shared':[shared]})),2)
        with self.assertRaisesRegex(TransferError,'duplicate'):
            hb.index_entries({'schema':3,'apps':[self.entry,self.entry]})
        for bad_id in ['../outside','PPSA00002-owner','PPSA00001/evil']:
            with self.assertRaises(TransferError): hb.identity({**self.entry,'id':bad_id})

    def test_invalid_download_metadata_is_rejected(self):
        cases=[{'sha256':'bad'}, {'sha256':None}, {'size':None}, {'size':True}, {'size':0}, {'titleid':'PPSA99999'}, {'version':'2.0'}, {'status':'withdrawn'}, {'format':'exe'}, {'source_repo':'https://github.com/another/repo'}, {'artifact_url':'https://github.com/example/homebrew/raw/main/PPSA00001.zip'}, {'artifact_url':'https://github.com.evil.test/example/homebrew/releases/download/v1.0/PPSA00001.zip'}, {'artifact_url':'http://github.com/example/homebrew/releases/download/v1.0/PPSA00001.zip'}, {'artifact_url':'https://github.com/example/homebrew/releases/download/v1.0/OperaSetup.exe'}]
        for change in cases:
            with self.subTest(change=change), self.assertRaises(TransferError):
                hb.validate_details(self.entry,{**self.detail,**change})

    def test_download_checks_expected_digest_and_receipt_before_pipeline(self):
        response=FakeResponse(self.body,headers={'Content-Length':str(len(self.body))},final_url=self.detail['artifact_url'])
        with patch('pragmata_transfer.urlopen',return_value=response), patch.object(hb,'run_pipeline') as pipeline:
            destination=hb.download_app(self.entry,self.detail,self.root,through_upload=True,config={'test':'mocked'})
        self.assertEqual(destination.read_bytes(),self.body)
        receipt=json.loads((destination.parent/'download-receipt.json').read_text())
        self.assertEqual(receipt['catalog_asserted_sha256'],self.detail['sha256'])
        pipeline.assert_called_once_with('PPSA00001',destination.parent,{'test':'mocked'})
        # Verified download reuse makes no further network request.
        with patch('pragmata_transfer.urlopen') as network:
            self.assertEqual(hb.download_app(self.entry,self.detail,self.root),destination)
            network.assert_not_called()

    def test_bad_digest_never_creates_success_receipt_or_starts_pipeline(self):
        response=FakeResponse(self.body,headers={'Content-Length':str(len(self.body))},final_url=self.detail['artifact_url'])
        with patch('pragmata_transfer.urlopen',return_value=response), patch.object(hb,'run_pipeline') as pipeline:
            with self.assertRaisesRegex(TransferError,'SHA-256 mismatch'):
                hb.download_app(self.entry,{**self.detail,'sha256':'0'*64},self.root,through_upload=True,config={})
        pipeline.assert_not_called()
        self.assertFalse(list(self.root.rglob('download-receipt.json')))
        self.assertTrue(list(self.root.rglob('*.partial')))
        self.assertFalse(list(self.root.rglob('.download.lock')))

    def test_title_conflicts_and_concurrent_runs_do_not_download(self):
        with patch.object(hb,'download_direct') as transfer:
            with self.assertRaisesRegex(TransferError,'shares'):
                hb.download_app(self.entry,{**self.detail,'conflicts_with':['PPSA00001-other']},self.root,through_upload=True,config={})
            title_dir=self.root/'PPSA00001'; title_dir.mkdir()
            (title_dir/'.download.lock').write_text('other process')
            with self.assertRaisesRegex(TransferError,'lock'):
                hb.download_app(self.entry,self.detail,self.root)
            transfer.assert_not_called()
        self.assertEqual((title_dir/'.download.lock').read_text(),'other process')

    def test_cache_is_validated_and_offline_is_explicit(self):
        document={'schema':3,'apps':[self.entry]}
        catalog=hb.Catalog(self.root)
        with patch.object(hb,'fetch_json',return_value=document):
            self.assertEqual(catalog.index(),[self.entry])
        with patch.object(hb,'fetch_json',side_effect=AssertionError('offline network')):
            self.assertEqual(hb.Catalog(self.root,True).index(),[self.entry])
        saved=json.loads((self.root/'index.json').read_text())
        saved['url']='https://untrusted.test/index.json'
        (self.root/'index.json').write_text(json.dumps(saved))
        with self.assertRaisesRegex(TransferError,'provenance'):
            hb.Catalog(self.root,True).index()
        with patch.object(hb,'fetch_json') as network:
            self.assertEqual(hb.main(['download','PPSA00001','--offline']),1)
            network.assert_not_called()

    def test_search_info_and_reserved_entries_do_not_download(self):
        reserved={**self.entry,'id':'PPSA00002','titleid':'PPSA00002','status':'coming_soon','version':None}
        document={'schema':3,'apps':[self.entry,reserved]}
        with patch.object(hb,'fetch_json',side_effect=lambda path: document if path=='index.json' else self.detail), patch.object(hb,'download_direct') as transfer:
            self.assertEqual(hb.main(['search','Synthetic','--category','emulators','--cache',str(self.root)]),0)
            self.assertEqual(hb.main(['info','PPSA00001','--cache',str(self.root)]),0)
            self.assertEqual(hb.main(['info','PPSA00002','--cache',str(self.root)]),0)
            self.assertEqual(hb.main(['download','PPSA00002','--cache',str(self.root)]),1)
            transfer.assert_not_called()

    def test_metadata_size_and_redirect_limits(self):
        class Response(io.BytesIO):
            headers={}
            def __enter__(self): return self
            def __exit__(self,*args): self.close()
        with patch.object(hb,'build_opener') as opener:
            opener.return_value.open.return_value=Response(b'X'*(hb.MAX_JSON+1))
            with self.assertRaisesRegex(TransferError,'4 MiB'): hb.fetch_json('index.json')
        with self.assertRaisesRegex(TransferError,'redirect'):
            hb.NoRedirect().redirect_request(None,None,None,None,None,None)

    @unittest.skipUnless((hb.ROOT/'repos/ps5-companion/target/release/ps5-companion.exe').is_file() and Path('C:/Program Files/7-Zip/7z.exe').is_file(), 'release CLI and 7-Zip required')
    def test_verified_homebrew_zip_uses_real_extraction_and_planning(self):
        import ps5_pipeline
        config={'cli':hb.ROOT/'repos/ps5-companion/target/release/ps5-companion.exe','seven_zip':Path('C:/Program Files/7-Zip/7z.exe'),'addr':'192.168.1.214','engine':'http://127.0.0.1:19113'}
        original=ps5_pipeline.command
        stages=[]
        def command(cli,args):
            stages.append(args[0])
            if args[0]=='upload':
                manifest=Path(args[1])
                (manifest.parent/'transfer-receipt.json').write_text(json.dumps({'status':'complete_size_verified','addr':config['addr']}))
                return ''
            return original(cli,args)
        response=FakeResponse(self.body,headers={'Content-Length':str(len(self.body))},final_url=self.detail['artifact_url'])
        with patch('pragmata_transfer.urlopen',return_value=response), patch.object(ps5_pipeline,'command',side_effect=command):
            destination=hb.download_app(self.entry,self.detail,self.root,through_upload=True,config=config)
        self.assertEqual(stages,['inspect','extract','plan-upload','upload'])
        state=json.loads((destination.parent.parent/'.pipeline'/destination.parent.name/'pipeline-state.json').read_text())
        self.assertEqual(state['stage'],'complete_size_verified')


if __name__=='__main__': unittest.main()
