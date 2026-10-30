#!/usr/bin/env python3
"""Offline checks for the bounded transfer guards."""

import sys
import unittest
import hashlib
import json
import shutil
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pragmata_transfer import (  # noqa: E402
    TransferError,
    firmware_gate,
    looks_like_html,
    parse_content_range,
    source_kind,
    validate_resume_response,
    download_direct,
    probe_url,
    reject_windows_download,
)


class FakeHeaders(dict):
    def get(self, key, default=None):
        return super().get(key, super().get(key.lower(), default))


class FakeResponse:
    def __init__(self, body, *, status=200, headers=None, final_url="https://files.example.test/file.ffpfsc"):
        self.body = body
        self.status = status
        self.headers = FakeHeaders(headers or {})
        self.final_url = final_url
        self.position = 0

    def read(self, count=-1):
        if count < 0:
            count = len(self.body) - self.position
        chunk = self.body[self.position : self.position + count]
        self.position += len(chunk)
        return chunk

    def getcode(self):
        return self.status

    def geturl(self):
        return self.final_url

    def close(self):
        pass


class TransferGuardTests(unittest.TestCase):
    def test_reset_is_resumed_with_entity_and_exact_offset(self):
        class ResetResponse(FakeResponse):
            def read(self, count=-1):
                if self.position:
                    raise ConnectionResetError('test reset')
                return super().read(count)
        output = self.make_test_directory() / 'game.exfat'
        first = ResetResponse(b'ABCD', headers={'Content-Length': '8', 'ETag': '"same"'})
        second = FakeResponse(b'EFGH', status=206, headers={'Content-Range': 'bytes 4-7/8', 'ETag': '"same"'})
        with patch('pragmata_transfer.urlopen', side_effect=[first, second]) as network:
            download_direct('https://files.example.test/game.exfat', output, expected_size=8, retry_base=0)
        self.assertEqual(output.read_bytes(), b'ABCDEFGH')
        request = network.call_args_list[1].args[0]
        self.assertEqual(request.get_header('Range'), 'bytes=4-')
        self.assertEqual(request.get_header('If-range'), '"same"')
        self.assertEqual(json.loads(Path(str(output)+'.retry.json').read_text())['state'], 'complete')

    def test_changed_entity_and_ignored_ranges_preserve_partial(self):
        for status, headers in [(206, {'Content-Range': 'bytes 4-7/8', 'ETag': '"new"'}), (200, {'ETag': '"same"'})]:
            directory = self.make_test_directory()
            output = directory / 'game.exfat'
            partial = Path(str(output)+'.partial'); partial.write_bytes(b'ABCD')
            Path(str(partial)+'.meta.json').write_text(json.dumps({'url':'https://files.example.test/game.exfat','expected_size':8,'sha256':None}))
            Path(str(partial)+'.entity.json').write_text(json.dumps({'header':'ETag','value':'"same"'}))
            with patch('pragmata_transfer.urlopen', return_value=FakeResponse(b'EFGH',status=status,headers=headers)) as network:
                with self.assertRaises(TransferError): download_direct('https://files.example.test/game.exfat',output,expected_size=8,retry_base=0)
            self.assertEqual(network.call_count, 1)
            self.assertEqual(partial.read_bytes(), b'ABCD')

    def test_temporary_http_failure_retries_but_404_does_not(self):
        for code, attempts in [(503, 2), (404, 1)]:
            output = self.make_test_directory() / 'game.exfat'
            error = HTTPError('https://files.example.test/game.exfat',code,'test',{'Retry-After':'0'},None)
            response = FakeResponse(b'ABCD',headers={'Content-Length':'4'})
            with patch('pragmata_transfer.urlopen',side_effect=[error,response]) as network:
                if code == 503: download_direct('https://files.example.test/game.exfat',output,retry_base=0)
                else:
                    with self.assertRaises(TransferError): download_direct('https://files.example.test/game.exfat',output,retry_base=0)
            self.assertEqual(network.call_count,attempts)

    def test_retry_limit_and_retry_after_budget(self):
        output = self.make_test_directory() / 'game.exfat'
        with patch('pragmata_transfer.urlopen',side_effect=TimeoutError('test')) as network:
            with self.assertRaisesRegex(TransferError,'retry limit'):
                download_direct('https://files.example.test/game.exfat',output,max_retries=2,retry_base=0)
        self.assertEqual(network.call_count,3)
        error = HTTPError('https://files.example.test/game.exfat',503,'test',{'Retry-After':'600'},None)
        with patch('pragmata_transfer.urlopen',side_effect=error) as network, patch('pragmata_transfer.time.sleep') as sleep:
            with self.assertRaisesRegex(TransferError,'wait budget'):
                download_direct('https://files.example.test/game.exfat',output,retry_base=0)
            sleep.assert_not_called()
            self.assertEqual(network.call_count,1)

    def test_cancellation_during_backoff_stops_requests(self):
        output = self.make_test_directory() / 'game.exfat'
        with patch('pragmata_transfer.urlopen',side_effect=TimeoutError('test')) as network:
            with self.assertRaisesRegex(TransferError,'cancelled'):
                download_direct('https://files.example.test/game.exfat',output,cancelled=unittest.mock.Mock(side_effect=[False,True]))
        self.assertEqual(network.call_count,1)

    def test_truncation_without_entity_does_not_retry_partial(self):
        output = self.make_test_directory() / 'game.exfat'
        with patch('pragmata_transfer.urlopen',return_value=FakeResponse(b'ABCD',headers={'Content-Length':'8'})) as network:
            with self.assertRaisesRegex(TransferError,'entity validator'):
                download_direct('https://files.example.test/game.exfat',output,expected_size=8,retry_base=0)
        self.assertEqual(network.call_count,1)

    def test_retry_after_is_honored_within_budget(self):
        output = self.make_test_directory() / 'game.exfat'
        error = HTTPError('https://files.example.test/game.exfat',503,'test',{'Retry-After':'1'},None)
        with patch('pragmata_transfer.urlopen',side_effect=[error,FakeResponse(b'ABCD',headers={'Content-Length':'4'})]), patch('pragmata_transfer.time.sleep') as sleep:
            download_direct('https://files.example.test/game.exfat',output,retry_base=0)
            self.assertAlmostEqual(sum(c.args[0] for c in sleep.call_args_list),1)

    def test_digest_mismatch_and_disk_errors_never_retry(self):
        output = self.make_test_directory() / 'game.exfat'
        with patch('pragmata_transfer.urlopen',return_value=FakeResponse(b'ABCD',headers={'Content-Length':'4'})) as network:
            with self.assertRaisesRegex(TransferError,'SHA-256 mismatch'):
                download_direct('https://files.example.test/game.exfat',output,expected_sha256='0'*64,retry_base=0)
        self.assertEqual(network.call_count,1)
        with patch('pragmata_transfer._check_space',side_effect=TransferError('disk full')), patch('pragmata_transfer.urlopen') as network:
            with self.assertRaises(TransferError): download_direct('https://files.example.test/other.exfat',output.parent/'other.exfat',expected_size=8)
            network.assert_not_called()

    def make_test_directory(self):
        directory = Path(__file__).resolve().parent / ".test-transfer-tmp"
        if directory.exists():
            shutil.rmtree(directory)
        directory.mkdir()
        self.addCleanup(lambda: shutil.rmtree(directory, ignore_errors=True))
        return directory

    def test_html_is_rejected_from_content_type_or_sniffed_prefix(self):
        self.assertTrue(looks_like_html(b"<!doctype html><html>challenge</html>"))
        self.assertTrue(looks_like_html(b"anything", "text/html; charset=utf-8"))
        self.assertFalse(looks_like_html(b"\x02\x00\x00\x00binary payload", "application/octet-stream"))

    def test_content_range_parser_and_resume_offset(self):
        self.assertEqual(parse_content_range("bytes 100-199/1000"), (100, 199, 1000))
        self.assertEqual(parse_content_range("bytes 100-199/*"), (100, 199, None))
        self.assertEqual(validate_resume_response(206, "bytes 100-199/1000", 100), 1000)
        with self.assertRaises(TransferError):
            validate_resume_response(200, None, 100)
        with self.assertRaises(TransferError):
            validate_resume_response(206, "bytes 0-199/1000", 100)

    def test_audited_provider_boundaries(self):
        self.assertEqual(
            source_kind(
                {
                    "url": "https://vikingfile.com/d/final/file.ffpfsc",
                    "requiresBrowser": False,
                }
            ),
            "direct",
        )
        self.assertEqual(
            source_kind(
                {
                    "url": "https://vikingfile.com/f/landing",
                    "browserUrl": "https://vikingfile.com/f/landing",
                    "requiresBrowser": True,
                }
            ),
            "vikingfile",
        )
        self.assertEqual(
            source_kind(
                {
                    "url": "https://www.mediafire.com/file/id/file.zip/file",
                    "requiresBrowser": True,
                }
            ),
            "mediafire",
        )

    def test_firmware_gate_blocks_12xx_on_1120(self):
        self.assertEqual(firmware_gate({"label": "EUR 12.xx Base"}, "11.20")["status"], "blocked")
        self.assertEqual(firmware_gate({"label": "USA 11.xx BackPort"}, "11.20")["status"], "uncertain")
        self.assertEqual(firmware_gate({"label": "USA 5.xx BackPort"}, "11.20")["status"], "allowed")
        self.assertEqual(firmware_gate({"label": "USA 11.20 BackPort"}, "11.20")["status"], "allowed")
        self.assertEqual(firmware_gate({"label": "Standard"}, "11.20")["status"], "unknown")

    def test_stream_mock_checks_size_and_digest(self):
        body = b"\x02\x00complete file bytes"
        digest = hashlib.sha256(body).hexdigest()
        response = FakeResponse(
            body,
            headers={"Content-Type": "application/octet-stream", "Content-Length": str(len(body))},
        )
        directory = self.make_test_directory()
        output = directory / "file.ffpfsc"
        with patch("pragmata_transfer.urlopen", return_value=response):
            destination, actual = download_direct(
                "https://files.example.test/file.ffpfsc",
                output,
                expected_size=len(body),
                expected_sha256=digest,
            )
        self.assertEqual(destination, output.resolve())
        self.assertEqual(actual, digest)
        self.assertEqual(output.read_bytes(), body)

    def test_stream_mock_rejects_truncated_known_length(self):
        body = b"short"
        response = FakeResponse(
            body,
            headers={"Content-Type": "application/octet-stream", "Content-Length": "10"},
        )
        directory = self.make_test_directory()
        output = directory / "file.bin"
        with patch("pragmata_transfer.urlopen", return_value=response):
            with self.assertRaises(TransferError):
                download_direct("https://files.example.test/file.bin", output)
        self.assertTrue(Path(str(output) + ".partial").exists())

    def test_installer_filename_is_blocked_before_request(self):
        directory = self.make_test_directory()
        for index, url in enumerate([
            "https://files.example.test/OperaSetup.exe",
            "https://files.example.test/Setup.MSI",
            "https://files.example.test/OperaSetup%2Eexe",
            "https://files.example.test/download?filename=Setup.exe",
        ]):
            with self.subTest(url=url), patch("pragmata_transfer.urlopen") as network:
                with self.assertRaisesRegex(TransferError, "Blocked Windows"):
                    download_direct(url, directory / f"game-{index}.exfat")
                network.assert_not_called()

    def test_installer_metadata_and_signatures_rejected_before_payload_write(self):
        directory = self.make_test_directory()
        cases = [
            (b"payload", {}, "https://ads.example.test/OperaSetup.exe"),
            (b"payload", {"Content-Disposition": 'attachment; filename="Setup.msi"'}, None),
            (b"payload", {"Content-Disposition": "attachment; filename*=UTF-8''OperaSetup%2Eexe"}, None),
            (b"payload", {"Content-Type": "application/x-msdownload"}, None),
            (b"MZ" + b"disguised executable", {"Content-Type": "application/octet-stream"}, None),
            (bytes.fromhex("d0cf11e0a1b11ae1") + b"installer", {}, None),
        ]
        for index, (body, headers, final_url) in enumerate(cases):
            with self.subTest(case=index):
                output = directory / f"game-{index}.exfat"
                response = FakeResponse(body, headers=headers, final_url=final_url or "https://files.example.test/game.exfat")
                with patch("pragmata_transfer.urlopen", return_value=response):
                    with self.assertRaisesRegex(TransferError, "Blocked Windows"):
                        download_direct("https://files.example.test/game.exfat", output)
                self.assertFalse(output.exists())
                self.assertFalse(Path(str(output) + ".partial").exists())

    def test_probe_rejects_executable_disguised_as_image(self):
        response = FakeResponse(b"MZnot a game", headers={"Content-Type": "application/octet-stream"})
        with patch("pragmata_transfer.urlopen", return_value=response):
            with self.assertRaisesRegex(TransferError, "Blocked Windows"):
                probe_url("https://files.example.test/game.exfat")

    def test_archive_and_image_prefixes_are_not_blocked(self):
        for prefix in [b"PK\x03\x04zip", b"Rar!\x1a\x07\x00", b"7z\xbc\xaf\x27\x1c", b"\x02\x00\x00\x00pfs"]:
            with self.subTest(prefix=prefix):
                reject_windows_download("https://files.example.test/game.exfat", prefix=prefix)

    def test_resume_checks_file_header_not_middle_of_file(self):
        directory = self.make_test_directory()
        output = directory / "game.exfat"
        url = "https://files.example.test/game.exfat"
        partial = Path(str(output) + ".partial")
        partial.write_bytes(b"PKheader")
        Path(str(partial) + ".meta.json").write_text(json.dumps({"url": url, "expected_size": None, "sha256": None}))
        body = b"MZvalid bytes in the middle of an archive"
        total = len(b"PKheader") + len(body)
        response = FakeResponse(body, status=206, headers={"Content-Range": f"bytes 8-{total-1}/{total}"}, final_url=url)
        with patch("pragmata_transfer.urlopen", return_value=response):
            download_direct(url, output)
        self.assertEqual(output.read_bytes(), b"PKheader" + body)


if __name__ == "__main__":
    unittest.main()
