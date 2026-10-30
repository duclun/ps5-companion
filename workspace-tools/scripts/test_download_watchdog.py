import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import pragmata_transfer as t
from test_pragmata_transfer import FakeResponse


class Clock:
    def __init__(self):self.value=0.0
    def __call__(self):return self.value


class WatchdogTests(unittest.TestCase):
    def read(self,watch,clock,seconds,payload=b'x'):
        class Response:
            def read1(self,count):clock.value+=seconds;return payload
            def read(self,count):raise AssertionError('should prefer read1')
        with patch.object(t.time,'monotonic',clock):return watch.read(Response(),1024)

    def test_idle_read_retries_and_bursty_long_waits_trigger(self):
        clock=Clock();watch=t.DownloadWatchdog(stall_timeout=90,idle_window=300)
        with self.assertRaises(t.TransientTransferError):self.read(watch,clock,91)
        clock=Clock();watch=t.DownloadWatchdog(stall_timeout=90,idle_window=300)
        with self.assertRaisesRegex(t.TransientTransferError,'between bursts'):
            for _ in range(10):
                self.read(watch,clock,45,b'x'*1024)
                self.read(watch,clock,1,b'x'*1024*1024)

    def test_steady_progress_and_non_network_wait_do_not_trigger(self):
        clock=Clock();watch=t.DownloadWatchdog(stall_timeout=90,idle_window=300)
        for _ in range(100):
            self.read(watch,clock,1)
            clock.value+=1000 # disk/callback/backoff time is deliberately outside reads
        self.assertEqual(watch.elapsed,100)
        self.assertLess(len(watch.waits),2)
        for _ in range(100):self.read(watch,clock,0,b'')

    def test_invalid_watchdog_settings_rejected(self):
        for value in [0,-1,float('inf'),float('nan')]:
            with self.assertRaises(t.TransferError):t.DownloadWatchdog(stall_timeout=value)

    def test_real_checked_download_retries_body_stall_using_validated_entity(self):
        payload=b'abc'
        class Response(FakeResponse):
            def __init__(self):super().__init__(payload,headers={'Content-Length':'3','ETag':'"same"'},final_url='https://example.test/game.bin');self.once=False
            def read1(self,count):
                if not self.once:self.once=True;clock.value+=100
                return self.read(count)
        clock=Clock()
        with tempfile.TemporaryDirectory() as temp:
            target=Path(temp)/'game.bin'
            responses=[Response(),FakeResponse(payload,headers={'Content-Length':'3','ETag':'"same"'},final_url='https://example.test/game.bin')]
            with patch.object(t,'urlopen',side_effect=responses) as opening,patch.object(t.time,'monotonic',clock):
                path,digest=t.download_direct('https://example.test/game.bin',target,expected_size=3,expected_sha256=hashlib.sha256(payload).hexdigest(),retry_base=0)
            self.assertEqual(path.read_bytes(),payload)
            self.assertEqual(opening.call_count,2)

    def test_body_stall_resumes_from_written_offset_with_same_validator(self):
        clock=Clock()
        payload=b'a'*t.PROBE_BYTES+b'def'
        class Response(FakeResponse):
            def __init__(self):super().__init__(payload,headers={'Content-Length':str(len(payload)),'ETag':'"same"'},final_url='https://example.test/game.bin');self.calls=0
            def read1(self,count):
                self.calls+=1
                if self.calls==1:return self.read(t.PROBE_BYTES)
                clock.value+=100
                return self.read(count)
        with tempfile.TemporaryDirectory() as temp:
            target=Path(temp)/'game.bin'
            resumed=FakeResponse(b'def',status=206,headers={'Content-Range':f'bytes {t.PROBE_BYTES}-{len(payload)-1}/{len(payload)}','Content-Length':'3','ETag':'"same"'},final_url='https://example.test/game.bin')
            with patch.object(t,'urlopen',side_effect=[Response(),resumed]) as opening,patch.object(t.time,'monotonic',clock):
                path,_=t.download_direct('https://example.test/game.bin',target,expected_size=len(payload),retry_base=0)
            self.assertEqual(path.read_bytes(),payload)
            request=opening.call_args_list[1].args[0]
            self.assertEqual(request.get_header('Range'),f'bytes={t.PROBE_BYTES}-')
            self.assertEqual(request.get_header('If-range'),'"same"')

    def test_split_windows_and_html_prefixes_are_rejected_with_read1(self):
        class ByteResponse(FakeResponse):
            def read1(self,count):return self.read(min(count,1))
        for payload in [b'MZ'+b'\0'*100, b'<!DOCTYPE html><html>advert</html>']:
            with self.subTest(payload=payload[:8]),tempfile.TemporaryDirectory() as temp:
                target=Path(temp)/'game.bin'
                response=ByteResponse(payload,headers={'Content-Length':str(len(payload))},final_url='https://example.test/game.bin')
                with patch.object(t,'urlopen',return_value=response):
                    with self.assertRaises(t.TransferError):
                        t.download_direct('https://example.test/game.bin',target,expected_size=len(payload))
                self.assertFalse(target.exists())

    def test_stall_exhaustion_does_not_switch_source_or_overwrite(self):
        with tempfile.TemporaryDirectory() as temp:
            target=Path(temp)/'game.bin'
            with patch.object(t,'_download_direct_once',side_effect=t.TransientTransferError('watchdog')) as run:
                with self.assertRaisesRegex(t.TransferError,'retry limit'):
                    t.download_direct('https://example.test/game.bin',target,retry_base=0,max_retries=2)
            self.assertEqual(run.call_count,3)
            self.assertFalse(target.exists())
            self.assertTrue(all(call.args[0]=='https://example.test/game.bin' for call in run.call_args_list))


if __name__=='__main__':unittest.main()
