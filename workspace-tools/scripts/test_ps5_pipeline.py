import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import ps5_pipeline as pipeline
from pragmata_transfer import TransferError
from ps5_download import ROOT, candidates, supplemental

CLI = ROOT / "repos/ps5-companion/target/debug/ps5-companion.exe"
SEVEN_ZIP = Path("C:/Program Files/7-Zip/7z.exe")


class SourceSelectionTests(unittest.TestCase):
    def test_standalone_backport_and_plural_dlcs_are_excluded(self):
        base = {"url": "https://files.example.test/game.rar", "label": "USA Base"}
        records = [base, dict(base, label="USA 12.xx BackPort"),
                   dict(base, label="USA 12.xx BackPort 4.xx"),
                   dict(base, label="EUR Only DLCs (PKG)"),
                   dict(base, label="", filename="PPSA05144-Backport.rar")]
        self.assertEqual(candidates(records), [[base]])
        self.assertTrue(supplemental({}, "PPSA05144-Backport+[+9.xx+8.xx+].rar"))


@unittest.skipUnless(CLI.is_file() and SEVEN_ZIP.is_file(), "existing CLI and 7-Zip required")
class PipelineIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.directory = self.root / "source123"
        self.directory.mkdir()
        self.config = {"cli": CLI, "seven_zip": SEVEN_ZIP, "addr": "192.168.1.214", "engine": "http://127.0.0.1:19113"}

    def archive(self, title="PPSA00001", patch_only=False):
        path = self.directory / "game.zip"
        with zipfile.ZipFile(path, "w") as archive:
            if patch_only:
                archive.writestr("patch/readme.txt", "patch only")
            else:
                archive.writestr(f"{title}/eboot.bin", b"test game executable")
                archive.writestr(f"{title}/sce_sys/param.json", json.dumps({"titleId": title}))
                archive.writestr(f"{title}/data/level.dat", b"test game data")
        receipt = {"files": [{"path": str(path), "bytes": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}]}
        (self.directory / "download-receipt.json").write_text(json.dumps(receipt))
        return path

    def test_real_inspection_extraction_and_plan_then_mocked_upload(self):
        self.archive()
        original = pipeline.command
        stages = []
        def command(cli, args):
            stages.append(args[0])
            if args[0] == "upload":
                manifest = Path(args[1])
                (manifest.parent / "transfer-receipt.json").write_text(json.dumps({"status": "complete_size_verified", "addr": self.config["addr"]}))
                return ""
            return original(cli, args)
        with patch.object(pipeline, "command", side_effect=command):
            pipeline.run_pipeline("PPSA00001", self.directory, self.config)
        self.assertEqual(stages, ["inspect", "extract", "plan-upload", "upload"])
        state = json.loads((self.root / ".pipeline/source123/pipeline-state.json").read_text())
        self.assertEqual(state["stage"], "complete_size_verified")
        manifest = json.loads(Path(state["manifest"]).read_text())
        self.assertEqual(len(manifest["files"]), 3)
        self.assertTrue(all(len(file["sha256"]) == 64 for file in manifest["files"]))
        # Never automatically resubmit a previously attempted upload.
        with patch.object(pipeline, "command", side_effect=AssertionError("duplicate upload")):
            with self.assertRaisesRegex(TransferError, "already attempted"):
                pipeline.run_pipeline("PPSA00001", self.directory, self.config)

    def test_wrong_title_is_not_uploaded(self):
        self.archive(title="PPSA99999")
        original = pipeline.command
        stages = []
        def command(cli, args):
            stages.append(args[0])
            return original(cli, args)
        with patch.object(pipeline, "command", side_effect=command):
            with self.assertRaisesRegex(TransferError, "extract failed"):
                pipeline.run_pipeline("PPSA00001", self.directory, self.config)
        self.assertEqual(stages, ["inspect", "extract"])

    def test_alternate_edition_extracts_against_artifact_and_preserves_listing(self):
        self.archive(title="PPSA00002")
        receipt_path = self.directory / "download-receipt.json"
        receipt = json.loads(receipt_path.read_text())
        receipt.update(title_id="PPSA00002", artifact_title_id="PPSA00002", listing_title_id="PPSA00001")
        receipt_path.write_text(json.dumps(receipt))
        original = pipeline.command
        def command(cli, args):
            if args[0] == "extract":
                self.assertEqual(args[-2:], ["--title-id", "PPSA00002"])
            if args[0] == "upload":
                manifest = Path(args[1])
                plan = original(cli, ["plan-upload", manifest])
                self.assertIn("/data/homebrew/PPSA00002", plan)
                (manifest.parent / "transfer-receipt.json").write_text(json.dumps({"status":"complete_size_verified", "addr":self.config["addr"]}))
                return ""
            return original(cli, args)
        with patch.object(pipeline, "command", side_effect=command):
            pipeline.run_pipeline("PPSA00002", self.directory, self.config)
        state = json.loads((self.root / ".pipeline/source123/pipeline-state.json").read_text())
        self.assertEqual(state["listing_title_id"], "PPSA00001")
        self.assertEqual(state["artifact_title_id"], "PPSA00002")

    def test_receipt_cannot_switch_expected_artifact_identity(self):
        self.archive()
        path = self.directory / "download-receipt.json"
        receipt = json.loads(path.read_text())
        receipt.update(title_id="PPSA00002", artifact_title_id="PPSA00002")
        path.write_text(json.dumps(receipt))
        with patch.object(pipeline, "command") as command:
            with self.assertRaisesRegex(TransferError, "receipt identity"):
                pipeline.run_pipeline("PPSA00001", self.directory, self.config)
            command.assert_not_called()

    def test_base_with_optional_backport_uses_only_base_in_upload_plan(self):
        path = self.directory / 'game.zip'
        with zipfile.ZipFile(path, 'w') as archive:
            for folder in ['wrapper/PPSA00001', 'wrapper/backport 4.xx/PPSA00001']:
                archive.writestr(folder+'/eboot.bin', b'test executable')
                archive.writestr(folder+'/sce_sys/param.json', json.dumps({'titleId':'PPSA00001'}))
        (self.directory/'download-receipt.json').write_text(json.dumps({'files':[{'path':str(path),'bytes':path.stat().st_size,'sha256':hashlib.sha256(path.read_bytes()).hexdigest()}]}))
        original = pipeline.command
        def command(cli, args):
            if args[0] == 'upload':
                manifest = Path(args[1])
                text = original(cli, ['plan-upload',manifest])
                self.assertNotIn('backport', text)
                self.assertIn('Artifact files: 2', text)
                (manifest.parent/'transfer-receipt.json').write_text(json.dumps({'status':'complete_size_verified','addr':self.config['addr']}))
                return ''
            return original(cli,args)
        with patch.object(pipeline,'command',side_effect=command):
            pipeline.run_pipeline('PPSA00001',self.directory,self.config)
        state=json.loads((self.root/'.pipeline/source123/pipeline-state.json').read_text())
        manifest=json.loads(Path(state['manifest']).read_text())
        self.assertEqual(len(manifest['files']),4)  # extras retained and hashed
        self.assertEqual(manifest['selected_artifact'],'wrapper/PPSA00001')

    def test_patch_only_archive_is_not_uploaded(self):
        self.archive(patch_only=True)
        original = pipeline.command
        stages = []
        def command(cli, args):
            stages.append(args[0])
            return original(cli, args)
        with patch.object(pipeline, "command", side_effect=command):
            with self.assertRaisesRegex(TransferError, "patch-only"):
                pipeline.run_pipeline("PPSA00001", self.directory, self.config)
        self.assertEqual(stages, ["inspect", "extract"])

    def test_changed_archive_is_not_extracted(self):
        source = self.archive()
        with source.open("ab") as stream:
            stream.write(b"changed")
        with patch.object(pipeline, "command", side_effect=AssertionError("should stop before commands")):
            with self.assertRaisesRegex(TransferError, "changed"):
                pipeline.run_pipeline("PPSA00001", self.directory, self.config)

    def test_direct_image_skips_extraction_and_uses_real_upload_plan(self):
        source = self.directory / "PPSA00001.exfat"
        source.write_bytes(b"\xeb\x76\x90EXFAT   " + bytes(501))
        (self.directory / "download-receipt.json").write_text(json.dumps({"files": [
            {"path": str(source), "bytes": source.stat().st_size, "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}
        ]}))
        stages = []
        original = pipeline.command
        def command(cli, args):
            stages.append(args[0])
            if args[0] == "upload":
                (Path(args[1]).parent / "transfer-receipt.json").write_text(json.dumps({"status": "complete_size_verified", "addr": self.config["addr"]}))
                return ""
            return original(cli, args)
        with patch.object(pipeline, "command", side_effect=command):
            pipeline.run_pipeline("PPSA00001", self.directory, self.config)
        self.assertEqual(stages, ["plan-upload", "upload"])
        self.assertTrue(source.is_file())

    def test_failed_local_stage_does_not_download_another_source(self):
        import ps5_download as download
        catalog = self.root / "catalog"
        catalog.mkdir()
        (catalog / "PPSA00001.json").write_text(json.dumps([
            {"url": "https://example.test/a.zip"}, {"url": "https://example.test/b.zip"}
        ]))
        with patch.object(download, "download_group", return_value=0) as fetch, \
             patch.object(pipeline, "run_pipeline", side_effect=TransferError("inspect failed")):
            with self.assertRaisesRegex(TransferError, "inspect failed"):
                download.run("PPSA00001", catalog, self.root / "downloads", pipeline=self.config)
            self.assertEqual(fetch.call_count, 1)
