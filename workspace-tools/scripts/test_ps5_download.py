import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from urllib.error import HTTPError
from unittest.mock import patch

import ps5_download as dl
from test_pragmata_transfer import FakeResponse


class DownloadWorkflowTests(unittest.TestCase):
    def test_filters_extras_and_incomplete_parts(self):
        base = {"url": "https://files.example.test/base.rar", "provider": "direct"}
        records = [base, dict(base, label="DLC"), dict(base, unavailableReason="gone"),
                   dict(base, label="Base part 2")]
        self.assertEqual(dl.candidates(records), [[base]])

    def test_groups_complete_parts_in_order(self):
        base = {"provider": "MediaFire", "source": "catalog", "url": "https://www.mediafire.com/file/x"}
        p1 = dict(base, label="Base part 1")
        p2 = dict(base, label="Base part 2")
        self.assertEqual(dl.candidates([p2, p1], include_mediafire=True), [[p1, p2]])

    def test_mediafire_disabled_by_default_with_explicit_opt_in(self):
        records = [{"url": "https://www.mediafire.com/file/x", "provider": "MediaFire"},
                   {"url": "https://download123.mediafire.com/a/game.rar", "provider": "direct"}]
        self.assertEqual(dl.candidates(records), [])
        self.assertEqual(len(dl.candidates(records, include_mediafire=True)), 2)

    def test_unavailable_last_volume_rejects_entire_set(self):
        records = [{"url": f"https://example.test/part{i}.rar", "label": f"Base part {i}"} for i in (1, 2, 3)]
        records[-1]["unavailableReason"] = "HTTP 404"
        self.assertEqual(dl.candidates(records), [])

    def test_parts_from_different_catalog_records_never_mix(self):
        records = [{"url": "https://example.test/a.rar", "label": "Base part 1", "record": "edition-a"},
                   {"url": "https://example.test/b.rar", "label": "Base part 2", "record": "edition-b"}]
        self.assertEqual(dl.candidates(records), [])

    def test_source_health_and_access_ranking(self):
        direct = {"url": "https://example.test/game.exfat"}
        flaky = dict(direct, url="https://example.test/other.exfat", health="flaky")
        viking = {"url": "https://vikingfile.com/f/a"}
        mediafire = {"url": "https://www.mediafire.com/file/a"}
        dead = dict(direct, health="dead")
        self.assertEqual(dl.candidates([mediafire, viking, flaky, dead, direct], include_mediafire=True),
                         [[direct], [flaky], [viking], [mediafire]])

    def test_entire_multipart_set_checked_for_space_before_transfer(self):
        records = [{"url": f"https://example.test/part{i}.rar", "label": f"Base part {i}"} for i in (1, 2)]
        with tempfile.TemporaryDirectory() as temp:
            probe = {"status": 200, "is_html": False, "content_length": 80}
            usage = type("Usage", (), {"free": dl.SPACE_RESERVE + 100})()
            with patch.object(dl, "probe_url", return_value=probe) as probes, \
                 patch.object(dl.shutil, "disk_usage", return_value=usage), \
                 patch.object(dl, "download_direct") as transfer:
                with self.assertRaisesRegex(dl.TransferError, "complete download set"):
                    dl.download_group("PPSA00001", records, Path(temp))
                self.assertEqual(probes.call_count, 2)
                transfer.assert_not_called()

    def test_dead_last_part_stops_before_first_volume_download(self):
        records = [{"url": f"https://example.test/part{i}.rar"} for i in (1, 2)]
        with tempfile.TemporaryDirectory() as temp:
            with patch.object(dl, "probe_url", side_effect=[
                    {"status": 200, "is_html": False, "content_length": 80},
                    dl.TransferError("HTTP Error 404")]), patch.object(dl, "download_direct") as transfer:
                with self.assertRaisesRegex(dl.TransferError, "404"):
                    dl.download_group("PPSA00001", records, Path(temp))
                transfer.assert_not_called()

    def test_duplicate_resolved_part_names_rejected(self):
        records = [{"url": "https://one.test/game.rar"}, {"url": "https://two.test/game.rar"}]
        with tempfile.TemporaryDirectory() as temp:
            with patch.object(dl, "probe_url", return_value={"status": 200, "is_html": False, "content_length": 80}), \
                 patch.object(dl, "download_direct") as transfer:
                with self.assertRaisesRegex(dl.TransferError, "same local filename"):
                    dl.download_group("PPSA00001", records, Path(temp))
                transfer.assert_not_called()

    def test_resume_space_credits_only_matching_partial(self):
        record = {"url": "https://example.test/game.exfat"}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            partial = directory / "game.exfat.partial"
            partial.write_bytes(b"12345678")
            sidecar = directory / "game.exfat.partial.meta.json"
            sidecar.write_text(json.dumps({"url": record["url"], "expected_size": 10, "sha256": None}))
            with patch.object(dl, "probe_url", return_value={"status": 200, "is_html": False, "content_length": 10}):
                self.assertEqual(dl.prepare_part(record, directory)["remaining"], 2)
                sidecar.write_text(json.dumps({"url": "https://other.test/a", "expected_size": 10, "sha256": None}))
                with self.assertRaisesRegex(dl.TransferError, "different URL"):
                    dl.prepare_part(record, directory)
            self.assertEqual(partial.read_bytes(), b"12345678")

    def test_mediafire_resolves_relative_link_without_executing_html(self):
        record = {"url": "https://www.mediafire.com/file/page", "provider": "MediaFire"}
        page = FakeResponse(b'<a id="downloadButton" href="/payload.rar">Download</a>',
                            final_url=record["url"])
        with patch.object(dl, "urlopen", return_value=page):
            self.assertEqual(dl.resolve(record), "https://www.mediafire.com/payload.rar")

    def test_browser_challenge_is_reported(self):
        record = {"url": "https://vikingfile.com/f/page"}
        page = FakeResponse(b"<html>Verify you are human</html>", final_url=record["url"])
        with patch.object(dl, "urlopen", return_value=page):
            with self.assertRaisesRegex(dl.TransferError, "browser verification"):
                dl.resolve(record)

    def test_unsafe_filename_is_not_used_as_path(self):
        self.assertNotIn("/", dl.filename("https://example.test/a%2Fb.rar", {}))
        with self.assertRaises(dl.TransferError):
            dl.filename("https://example.test/CON.rar", {})

    def test_full_workflow_downloads_and_writes_digest_receipt(self):
        payload = b"verified binary archive payload"
        record = {"url": "https://files.example.test/game.rar", "provider": "direct", "id": "base"}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            catalog = root / "catalog"
            catalog.mkdir()
            (catalog / "PPSA00001.json").write_text(json.dumps([record]))
            out = root / "out"
            probe = {"is_html": False, "status": 200, "content_length": len(payload)}
            response = FakeResponse(payload, headers={"Content-Length": str(len(payload))}, final_url=record["url"])
            with patch.object(dl, "probe_url", return_value=probe), patch("pragmata_transfer.urlopen", return_value=response):
                self.assertEqual(dl.run("PPSA00001", catalog, out), 0)
            receipt_path = next(out.glob("*/download-receipt.json"))
            receipt = json.loads(receipt_path.read_text())
            self.assertEqual(receipt["files"][0]["sha256"], hashlib.sha256(payload).hexdigest())
            self.assertEqual(Path(receipt["files"][0]["path"]).read_bytes(), payload)
            self.assertFalse((out / ".download.lock").exists())
            # A second run verifies the receipt and file without a new transfer.
            with patch.object(dl, "probe_url", side_effect=AssertionError("unexpected transfer")):
                self.assertEqual(dl.run("PPSA00001", catalog, out), 0)

    def test_existing_unowned_file_survives(self):
        record = {"url": "https://files.example.test/game.rar"}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            existing = directory / "game.rar"
            existing.write_bytes(b"keep me")
            with self.assertRaisesRegex(dl.TransferError, "preserving"):
                dl.fetch_part(record, directory)
            self.assertEqual(existing.read_bytes(), b"keep me")

    def test_list_does_not_create_download_directory_or_use_network(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "PPSA00001.json").write_text(json.dumps([{"url": "https://example.test/a.rar"}]))
            out = root / "out"
            with patch.object(dl, "resolve", side_effect=AssertionError("network")):
                self.assertEqual(dl.run("PPSA00001", root, out, list_only=True), 0)
            self.assertFalse(out.exists())

    def test_browser_handoff_downloads_pasted_url_with_original_provenance(self):
        record = {"id": "base", "url": "https://vikingfile.com/f/source", "provider": "VikingFile"}
        direct_url = "https://files.example.test/game.rar"
        payload = b"browser handoff payload"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "PPSA00001.json").write_text(json.dumps([record]))
            response = FakeResponse(payload, headers={"Content-Length": str(len(payload))}, final_url=direct_url)
            probe = {"is_html": False, "status": 200, "content_length": len(payload)}
            with patch.object(dl, "resolve", side_effect=dl.BrowserRequired("verification")), \
                 patch("builtins.input", side_effect=["1", "n", direct_url]), \
                 patch.object(dl, "probe_url", return_value=probe), \
                 patch("pragmata_transfer.urlopen", return_value=response), \
                 patch.object(dl.webbrowser, "open") as browser:
                self.assertEqual(dl.run("PPSA00001", root, root / "out", interactive=True), 0)
                browser.assert_not_called()
            receipt = json.loads(next((root / "out").glob("*/download-receipt.json")).read_text())
            self.assertEqual(receipt["files"][0]["record"], record)
            self.assertEqual(receipt["files"][0]["url"], direct_url)
            self.assertEqual(Path(receipt["files"][0]["path"]).read_bytes(), payload)

    def test_handoff_opens_only_source_and_reprompts_invalid_url(self):
        record = {"url": "https://vikingfile.com/f/source"}
        final = "https://files.example.test/game.rar"
        with patch("builtins.input", side_effect=["y", "http://insecure.test/a", record["url"], final]), \
             patch.object(dl.webbrowser, "open", return_value=True) as browser:
            self.assertEqual(dl.request_browser_url(record), final)
            browser.assert_called_once_with(record["url"])

    def test_handoff_can_skip_without_opening_browser(self):
        with patch("builtins.input", side_effect=["n", ""]), patch.object(dl.webbrowser, "open") as browser:
            self.assertIsNone(dl.request_browser_url({"url": "https://vikingfile.com/f/source"}))
            browser.assert_not_called()

    def test_404_does_not_prompt_browser_handoff(self):
        record = {"url": "https://vikingfile.com/f/missing", "provider": "VikingFile"}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "PPSA00001.json").write_text(json.dumps([record]))
            with patch.object(dl, "resolve", side_effect=HTTPError(record["url"], 404, "Not Found", {}, None)), \
                 patch("builtins.input", side_effect=AssertionError("unexpected prompt")):
                with self.assertRaisesRegex(dl.TransferError, "No source completed"):
                    dl.run("PPSA00001", root, root / "out", interactive=True)
            self.assertFalse((root / "out/.download.lock").exists())


if __name__ == "__main__":
    unittest.main()
