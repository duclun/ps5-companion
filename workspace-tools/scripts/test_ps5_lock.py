import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import ps5_lock as locks
from pragmata_transfer import TransferError


class LockTests(unittest.TestCase):
    def seed(self, root, **changes):
        marker = {'schema_version': 1, 'pid': os.getpid(), 'started': 'old-creation',
                  'nonce': 'a'*32, 'host': socket.gethostname(), 'phase': 'download', 'title_id': 'PPSA00001'}
        marker.update(changes)
        (root/'.download.lock').write_text(json.dumps(marker))
        return marker

    def test_current_owner_identity_and_normal_release(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); lock = locks.DownloadLock(root, 'PPSA00001'); lock.acquire()
            try:
                info = locks.diagnose(root)
                self.assertEqual(info['owner'], 'live')
                self.assertEqual(info['os_guard'], 'busy_or_unknown')
                self.assertFalse(info['recoverable'])
                self.assertEqual(info['metadata']['pid'], os.getpid())
                self.assertTrue(info['metadata']['started'])
            finally: lock.release()
            self.assertFalse((root/'.download.lock').exists())
            self.assertTrue((root/'.download.lock.guard').exists())

    def test_competing_download_cannot_replace_owner(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); first=locks.DownloadLock(root,'PPSA00001'); first.acquire()
            original=(root/'.download.lock').read_bytes()
            try:
                with self.assertRaises(TransferError): locks.DownloadLock(root,'PPSA00001').acquire()
                self.assertEqual((root/'.download.lock').read_bytes(),original)
            finally: first.release()

    def test_abrupt_child_exit_can_be_recovered_without_touching_partial_or_receipt(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); partial=root/'game.partial'; partial.write_bytes(b'keep downloaded bytes')
            receipt=root/'download-receipt.json'; receipt.write_text('{"retain":true}')
            module=str(Path(locks.__file__).parent)
            code='import sys,os;sys.path.insert(0,sys.argv[1]);from pathlib import Path;from ps5_lock import DownloadLock;l=DownloadLock(Path(sys.argv[2]),"PPSA00001");l.acquire();os._exit(0)'
            subprocess.run([sys.executable,'-B','-c',code,module,str(root)],check=True,timeout=15)
            marker=(root/'.download.lock').read_bytes()
            info=locks.diagnose(root);self.assertEqual(info['owner'],'gone');self.assertTrue(info['recoverable'])
            recovered=locks.recover(root)
            self.assertEqual(Path(recovered['archive']).read_bytes(),marker)
            self.assertFalse((root/'.download.lock').exists())
            self.assertEqual(partial.read_bytes(),b'keep downloaded bytes')
            self.assertEqual(receipt.read_text(),'{"retain":true}')

    def test_live_child_is_not_recovered(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);module=str(Path(locks.__file__).parent)
            code='import sys;sys.path.insert(0,sys.argv[1]);from pathlib import Path;from ps5_lock import DownloadLock;l=DownloadLock(Path(sys.argv[2]),"PPSA00001");l.acquire();print("ready",flush=True);sys.stdin.readline();l.release()'
            p=subprocess.Popen([sys.executable,'-B','-c',code,module,str(root)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
            try:
                self.assertEqual(p.stdout.readline().strip(),'ready')
                before=(root/'.download.lock').read_bytes()
                self.assertEqual(locks.diagnose(root)['owner'],'live')
                with self.assertRaises(TransferError): locks.recover(root)
                self.assertEqual((root/'.download.lock').read_bytes(),before)
            finally:
                p.communicate('\n',timeout=15)
                self.assertEqual(p.returncode,0)

    def test_pid_reuse_is_distinguished_by_creation_identity(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root)
            with patch.object(locks,'process_identity',return_value={'state':'live','started':'different-process'}):
                self.assertEqual(locks.diagnose(root)['owner'],'pid_reused')
                self.assertTrue(locks.recover(root)['recovered'])

    def test_unknown_owner_is_not_recovered_even_with_old_timestamp(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root,created_utc='2000-01-01T00:00:00Z')
            with patch.object(locks,'process_identity',return_value={'state':'unknown'}):
                self.assertFalse(locks.diagnose(root)['recoverable'])
                with self.assertRaises(TransferError): locks.recover(root)
            self.assertTrue((root/'.download.lock').exists())

    def test_foreign_host_is_not_recovered(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root,host='another-machine')
            with patch.object(locks,'process_identity',side_effect=AssertionError('foreign PID must not be queried')):
                self.assertEqual(locks.diagnose(root)['owner'],'foreign_host')
                with self.assertRaises(TransferError):locks.recover(root)

    def test_legacy_or_malformed_lock_requires_manual_inspection(self):
        for value in ['', 'not json', '{"schema_version":99}', '[]']:
            with self.subTest(value=value), tempfile.TemporaryDirectory() as temp:
                root=Path(temp);(root/'.download.lock').write_text(value)
                self.assertFalse(locks.diagnose(root)['recoverable'])
                with self.assertRaises(TransferError):locks.recover(root)
                self.assertEqual((root/'.download.lock').read_text(),value)

    def test_pipeline_phase_and_evidence_block_recovery(self):
        for phase,evidence in [('pipeline',False),('download',True)]:
            with self.subTest(phase=phase),tempfile.TemporaryDirectory() as temp:
                root=Path(temp);self.seed(root,phase=phase)
                if evidence:
                    (root/'.pipeline').mkdir();(root/'.pipeline/transfer-receipt.json').write_text('{}')
                with patch.object(locks,'process_identity',return_value={'state':'gone'}):
                    self.assertFalse(locks.diagnose(root)['recoverable'])
                    with self.assertRaisesRegex(TransferError,'reconciliation'): locks.recover(root)
                self.assertTrue((root/'.download.lock').exists())

    def test_phase_changes_before_pipeline_and_owner_still_controls_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);lock=locks.DownloadLock(root,'PPSA00001');lock.acquire()
            try:
                lock.set_pipeline()
                self.assertEqual(json.loads((root/'.download.lock').read_text())['phase'],'pipeline')
            finally:lock.release()
            self.assertFalse((root/'.download.lock').exists())

    def test_replaced_nonce_is_never_deleted_on_release(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);lock=locks.DownloadLock(root,'PPSA00001');lock.acquire()
            self.seed(root,nonce='b'*32)
            with self.assertRaisesRegex(TransferError,'ownership changed'):lock.set_pipeline()
            lock.release()
            self.assertEqual(json.loads((root/'.download.lock').read_text())['nonce'],'b'*32)

    def test_os_guard_alone_blocks_recovery_even_when_owner_appears_gone(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root)
            with locks._guard(root,True),patch.object(locks,'process_identity',return_value={'state':'gone'}):
                self.assertFalse(locks.diagnose(root)['recoverable'])
                with self.assertRaises(TransferError):locks.recover(root)
            self.assertTrue((root/'.download.lock').exists())

    def test_diagnosis_creates_no_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.assertFalse(locks.diagnose(root)['exists']);self.assertEqual(list(root.iterdir()),[])
            self.seed(root);before={p.name:p.read_bytes() for p in root.iterdir()}
            locks.diagnose(root)
            self.assertEqual({p.name:p.read_bytes() for p in root.iterdir()},before)

    def test_linked_or_oversized_marker_is_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root);os.link(root/'.download.lock',root/'another-name')
            with self.assertRaises(TransferError):locks.diagnose(root)
            with self.assertRaises(TransferError):locks.recover(root)
            self.assertTrue((root/'another-name').exists())
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);(root/'.download.lock').write_bytes(b'x'*(locks.LIMIT+1))
            with self.assertRaises(TransferError):locks.diagnose(root)

    def test_wrong_requested_title_cannot_recover_custom_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.seed(root)
            with patch.object(locks,'process_identity',return_value={'state':'gone'}):
                with self.assertRaisesRegex(TransferError,'Requested title'): locks.recover(root,'PPSA00002')
            self.assertTrue((root/'.download.lock').exists())

    def test_unknown_self_identity_cannot_leave_an_unremovable_lock(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            with patch.object(locks,'process_identity',return_value={'state':'unknown'}):
                with self.assertRaisesRegex(TransferError,'creation identity'): locks.DownloadLock(root,'PPSA00001')
            self.assertEqual(list(root.iterdir()),[])


if __name__=='__main__':unittest.main()
