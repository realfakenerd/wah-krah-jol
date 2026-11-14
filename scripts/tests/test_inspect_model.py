"""CPU-only inspector launcher tests; no renderer, Cargo or assets are launched."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from model_inspector import forwarding, host, processes, runtime


class InspectorTests(unittest.TestCase):
    def test_native_fault_detection_distinguishes_lowercase_error_from_failure(self):
        for line in ['INSPECTOR_INPUT meshes/clutter/error.glb', 'INFO: recoverable error']:
            self.assertIsNone(runtime.FAULT.search(line), line)
        for line in ['ERROR renderer failed', 'INSPECTOR_LOAD_FAILED', 'INSPECTOR_LOAD_TIMEOUT',
                     'thread panicked at source', 'Validation Error', 'DEVICE LOST']:
            self.assertIsNotNone(runtime.FAULT.search(line), line)

    def test_invalid_catalog_has_no_run_or_reservation_side_effect(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(FileNotFoundError):
                runtime.options(['--assets', root, '--record', '1', '--catalog', root + '/missing.db'])
            self.assertEqual(list(Path(root).iterdir()), [])

    def test_zero_and_excess_inputs_refuse(self):
        with tempfile.TemporaryDirectory() as root:
            for tail in [[], ['--model', 'meshes/test.glb'] * 9]:
                with self.assertRaises(RuntimeError):
                    runtime.options(['--assets', root] + tail)

    def test_offscreen_export_arguments_validate_and_forward(self):
        with tempfile.TemporaryDirectory() as root:
            base = ['--assets', root, '--model', 'meshes/test.glb']
            args = runtime.options(base + ['--export-video', '--seconds', '3', '--fps', '30',
                                           '--size', '640x480', '--background', '#102030', '--video-format', 'mov'])
            command = runtime.append_export_args(['inspector'], args)
            self.assertIn('--export-video', command)
            self.assertIn('--video-format', command)
            self.assertEqual(command[command.index('--video-format') + 1], 'mov')
            for tail in [
                ['--export-video', '--background', 'transparent'],
                ['--export-video', '--size', '641x480'],
                ['--export-video', '--fps', '61'],
                ['--export-gif', '--seconds', '61'],
                ['--export-frames', '--size', '2024x2024', '--seconds', '60', '--fps', '60'],
            ]:
                with self.subTest(tail=tail), self.assertRaises(RuntimeError):
                    runtime.options(base + tail)

    def test_successful_export_requires_complete_manifests(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            captures = output / 'captures'
            captures.mkdir()
            (captures / 'manifest.json').write_text(json.dumps({'frames': [{}, {}]}))
            with self.assertRaises(RuntimeError):
                runtime.verify_export_success(output, 'frames', 1, 1)
            runtime.verify_export_success(output, 'frames', 1, 2)
            with self.assertRaises(FileNotFoundError):
                runtime.verify_export_success(output, 'gif', 1, 2)
            (captures / 'encoding-manifest.json').write_text(json.dumps({
                'capture_manifest': 'manifest.json', 'requested': {'frame_count': 2}}))
            runtime.verify_export_success(output, 'gif', 1, 2)

    def test_orphan_inspector_names_are_protected(self):
        for name in ['model_inspector', 'model-inspector-deadbeef', 'human_goat_lamps', 'engine', 'actor_idle_probe']:
            self.assertTrue(runtime.PROTECTED.fullmatch(name), name)

    def test_quiet_guard_never_terminates_a_peer(self):
        with tempfile.TemporaryDirectory() as root:
            with patch.object(runtime, 'held_build_leases', return_value=[Path(root) / 'isolated-peer.lock']), patch.object(runtime.processes, 'processes') as census:
                with self.assertRaises(RuntimeError):
                    runtime.quiet_native(Path(root), Path(root) / 'own')
                census.assert_not_called()

    def test_pid_reuse_does_not_retain_or_signal_an_unrelated_process(self):
        rows = {10: {'birth': 'new', 'ppid': 1, 'pgid': 10}}
        self.assertEqual(processes.family(rows, 10, {10: 'old'}), set())
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            (output / 'worker.json').write_text(json.dumps({'pid': 10, 'birth': 'old'}))
            with patch.object(forwarding.processes, 'processes', return_value=rows), patch.object(forwarding.os, 'kill') as kill:
                forwarding.ForwardSignals(output).forward(signal.SIGTERM, None)
                kill.assert_not_called()
            self.assertTrue((output / 'stop-request.json').exists())

    def test_parent_signal_forwards_only_to_its_matching_direct_worker(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            (output / 'worker.json').write_text(json.dumps({'pid': 10, 'birth': 'same'}))
            rows = {10: {'birth': 'same', 'ppid': os.getpid(), 'pgid': 10}}
            with patch.object(forwarding.processes, 'processes', return_value=rows), patch.object(forwarding.os, 'kill') as kill:
                forwarding.ForwardSignals(output).forward(signal.SIGTERM, None)
                kill.assert_called_once_with(10, signal.SIGTERM)
            rows[10]['ppid'] = 99999
            with patch.object(forwarding.processes, 'processes', return_value=rows), patch.object(forwarding.os, 'kill') as kill:
                forwarding.ForwardSignals(output).forward(signal.SIGTERM, None)
                kill.assert_not_called()

    def test_partial_spawn_failure_preserves_first_failure_and_restores_handlers(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            owned = MagicMock()
            owned.spawn.side_effect = RuntimeError('first spawn failure')
            owned.observe.return_value = ({}, set())
            owned.known = {}
            sample = {'sample': {'swap': 0}}
            with patch.object(runtime.lifecycle, 'OwnedChild', return_value=owned), patch.object(runtime.memory, 'Darwin'), patch.object(runtime, 'sample_host', side_effect=[sample, RuntimeError('later host failure')]):
                with self.assertRaisesRegex(RuntimeError, 'first spawn failure'):
                    runtime.monitor(['unused'], output, output, 'native.log', 'native', output, 40, 60)
            owned.cleanup.assert_called_once()
            owned.restore.assert_called_once()
            receipt = json.loads((output / 'native.log.release.json').read_text())
            self.assertEqual(receipt['failure'], 'first spawn failure')
            self.assertEqual(receipt['postflight_failure'], 'later host failure')
            self.assertEqual(receipt['remaining_owned'], [])

    def test_pending_signal_before_worker_is_durable(self):
        with tempfile.TemporaryDirectory() as root:
            with patch.object(forwarding.processes, 'processes') as census:
                forwarding.ForwardSignals(Path(root)).forward(signal.SIGINT, None)
                census.assert_not_called()
            self.assertEqual(json.loads((Path(root) / 'stop-request.json').read_text())['signal'], signal.SIGINT)

    def test_host_parser_rejects_duplicate_counter_and_unknown_pressure(self):
        text = 'Mach Virtual Memory Statistics: (page size of 16384 bytes)\n' + '\n'.join(name + ': 1.' for name in host.FIELDS)
        self.assertEqual(host.parse_vm_stat(text)[0], 16384)
        with self.assertRaises(host.HostContractError):
            host.parse_vm_stat(text + '\nPages inactive: 1.')
        with self.assertRaises(host.HostContractError):
            host.parse_pressure('9')


    def test_fresh_managed_evidence_parent_is_created_without_extra_setup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / 'evidence').mkdir()  # Workspace.setup's documented top-level output.
            output = runtime.create_output(root)
            self.assertEqual(output.parent, root / 'evidence/model-inspector')
            self.assertTrue(output.is_dir())
            self.assertNotEqual(runtime.create_output(root), output)

    @unittest.skipUnless(os.name == 'posix', 'POSIX managed launch')
    def test_nested_evidence_symlink_refuses_before_any_external_write(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            root = Path(directory).resolve()
            (root / 'evidence').mkdir()
            (root / 'evidence/model-inspector').symlink_to(outside, target_is_directory=True)
            with self.assertRaises(RuntimeError):
                runtime.create_output(root)
            self.assertEqual(list(Path(outside).iterdir()), [])

    @unittest.skipUnless(os.name == 'posix', 'POSIX process groups')
    def test_actual_synthetic_child_group_is_reaped_by_owned_cleanup(self):
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'], start_new_session=True)
        try:
            row = processes.processes()[child.pid]
            known = {child.pid: row['birth']}
            processes.cleanup(child, known)
            self.assertIsNotNone(child.poll())
            self.assertEqual(processes.family(processes.processes(), child.pid, known), set())
        finally:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)



if __name__ == '__main__':
    unittest.main()
