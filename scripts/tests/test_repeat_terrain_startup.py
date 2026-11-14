import copy
import importlib.util
import json
import os
from pathlib import Path
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
from unittest import mock
import zlib


SCRIPT = Path(__file__).resolve().parents[1] / "repeat-terrain-startup.py"
SPEC = importlib.util.spec_from_file_location("repeat_terrain_startup", SCRIPT)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


def json_file(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def png(width=2, height=1, compressed=None, color=2, depth=8):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, 0))
        + chunk(b"IDAT", compressed if compressed is not None else zlib.compress((b"\0" + b"\x40\x80\xa0" * width) * height))
        + chunk(b"IEND", b"")
    )


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.aggregate = {
            **RUNNER.SCENE_COUNTS,
            **{key: 0 for key in RUNNER.EMPTY_QUEUES + RUNNER.FAILURE_COUNTS},
            "asset_failures": [], "resident_roots": 25,
            "requests_submitted": 25, "responses_received": 25,
            "lod_queries_submitted": 3, "lod_query_responses": 3,
            "lod_chunks_requested": 2, "lod_chunks_ready": 2,
        }
        self.report = {"passed": True, "warmup_frames": 100, "frames": 1, "elapsed_seconds": 1.0, "scenario": "repeat-terrain-startup", "streaming": copy.deepcopy(self.aggregate), "renderer": {"renderer_validation_failures": 0}}
        self.profile = {
            "aggregate": self.aggregate,
            "timeline": [
                {"frame": 10, "stage": stage, "subject": f"{stage}-{index}"}
                for stage, count in (("committed", 25), ("asset_ready", 2029), ("ready", 2))
                for index in range(count)
            ],
        }
        self.metadata = {"worldspace_id": 60, "start_grid": [5, -12], "stream_radius": 2, "resolution": [2, 1], "run_id": "16mib-01", "scenario": "repeat-terrain-startup", "commit": "fixture"}
        self.manifest = {"exit_code": 0, "warmup_frames": 100, "duration_seconds": 1, "run_id": "16mib-01", "scenario": "repeat-terrain-startup", "commit": "fixture", "input_hashes_before": {"engine": "same"}, "input_hashes_after": {"engine": "same"}}
        self.logs = "INFO engine::app: Mudcrab runtime initialized camera=Vec3(2048.0, 5976.0, 5952.0) target=Vec3(2048.0, -24.0, -2048.0)\n"
        self.flush()

    def flush(self):
        json_file(self.root / "report.json", self.report)
        json_file(self.root / "profile/streaming.json", self.profile)
        json_file(self.root / "profile/metadata.json", self.metadata)
        json_file(self.root / "profile/cpu-spans.json", {"gauges": {"lod/pending_chunks": 0}})
        (self.root / "frame.png").write_bytes(png())
        (self.root / "frames.csv").write_text("frame,ms\n0,20\n", encoding="utf-8")
        (self.root / "engine.stdout.log").write_text("", encoding="utf-8")
        (self.root / "engine.stderr.log").write_text(self.logs, encoding="utf-8")

    def validate(self):
        return RUNNER.analyze_run(self.root, self.manifest)

    def test_success_does_not_claim_visual_coverage_or_require_a_retry(self):
        result = self.validate()
        self.assertTrue(result["functional_checks_passed"], result["failures"])
        self.assertEqual(result["visual_inspection"], "pending")
        self.assertEqual(result["retry_accounting"]["status"], "not_observed")

    def test_pending_queues_and_nonzero_failures_cannot_hide_behind_correct_counts(self):
        for key in ("pending_surface_instances", "pending_lod_chunks", "material_validation_failures", "missing_cell_roots"):
            with self.subTest(key=key):
                self.aggregate[key] = 1
                self.report["streaming"][key] = 1
                self.flush()
                result = self.validate()
                self.assertFalse(result["functional_checks_passed"])
                self.assertTrue(any(key in message for message in result["failures"]))
                self.aggregate[key] = self.report["streaming"][key] = 0

    def test_ready_counts_without_complete_lifecycle_evidence_fail(self):
        self.aggregate["lod_chunks_requested"] = 3
        self.profile["timeline"].pop()
        self.flush()
        result = self.validate()
        self.assertFalse(result["functional_checks_passed"])
        self.assertTrue(any("lifecycle mismatch" in message for message in result["failures"]))
        self.assertTrue(any("timeline ready" in message for message in result["failures"]))

    def test_late_readiness_fails_even_when_final_queues_are_empty(self):
        self.profile["timeline"][-1]["frame"] = 101
        self.flush()
        result = self.validate()
        self.assertFalse(result["functional_checks_passed"])
        self.assertIn("streaming timeline settled after warmup: frame 101", result["failures"])

    def test_report_profile_disagreement_fails(self):
        self.report["streaming"]["asset_load_failures"] = 1
        self.flush()
        self.assertIn("report/profile streaming mismatch: asset_load_failures", self.validate()["failures"])

    def test_complete_counts_do_not_accept_a_short_measurement(self):
        self.report["elapsed_seconds"] = 0.2
        self.flush()
        self.assertTrue(any("duration" in message for message in self.validate()["failures"]))

    def test_artifacts_must_match_the_recorded_invocation(self):
        for key in ("run_id", "scenario", "commit"):
            with self.subTest(key=key):
                original = self.metadata[key]
                self.metadata[key] = "another-launch"
                self.flush()
                self.assertIn(f"profile provenance mismatch: {key}", self.validate()["failures"])
                self.metadata[key] = original
        self.report["scenario"] = "another-scenario"
        self.flush()
        self.assertTrue(any("report scenario" in message for message in self.validate()["failures"]))

    def test_deployed_manifest_can_recover_provenance_from_recorded_arguments(self):
        self.manifest.pop("scenario")
        self.manifest.pop("commit")
        self.manifest["command"] = ["engine", "--profile-scenario", "repeat-terrain-startup", "--profile-commit", "fixture"]
        result = self.validate()
        self.assertTrue(result["functional_checks_passed"], result["failures"])

    def test_wrong_camera_and_changed_inputs_fail(self):
        self.logs = self.logs.replace("5976.0", "4976.0")
        self.manifest["input_hashes_after"]["engine"] = "different"
        self.flush()
        result = self.validate()
        self.assertFalse(result["functional_checks_passed"])
        self.assertTrue(any("camera offset" in message for message in result["failures"]))
        self.assertTrue(any("inputs changed" in message for message in result["failures"]))

    def test_black_capture_is_rejected_for_every_filter_and_alpha(self):
        for color, channels in ((0, 1), (2, 3), (4, 2), (6, 4)):
            for filter_type in range(5):
                with self.subTest(color=color, filter=filter_type):
                    # Opaque alpha must not make black RGB appear nonblack.
                    pixel = bytes(channels - 1) + b"\xff" if color in (4, 6) else bytes(channels)
                    first = b"\0" + pixel * 2
                    current = pixel * 2
                    previous = current
                    encoded = bytearray()
                    for i, value in enumerate(current):
                        left = current[i - channels] if i >= channels else 0
                        up = previous[i]
                        corner = previous[i - channels] if i >= channels else 0
                        predictors = (0, left, up, (left + up) // 2)
                        if filter_type == 4:
                            estimate = left + up - corner
                            distances = [abs(estimate - x) for x in (left, up, corner)]
                            predictor = (left, up, corner)[distances.index(min(distances))]
                        else:
                            predictor = predictors[filter_type]
                        encoded.append((value - predictor) & 255)
                    image = png(height=2, color=color, compressed=zlib.compress(first + bytes([filter_type]) + encoded))
                    (self.root / "frame.png").write_bytes(image)
                    self.assertTrue(any("nonblack" in x for x in self.validate()["failures"]))

    def test_visible_pixel_passes_but_transparent_color_does_not(self):
        for color, pixels, accepted in ((6, b"\xff\x00\x00\xff" * 2, True),
                                        (6, b"\xff\x00\x00\x00" * 2, False),
                                        (4, b"\x80\xff" * 2, True),
                                        (0, b"\x00\x80", True)):
            with self.subTest(color=color, accepted=accepted):
                (self.root / "frame.png").write_bytes(png(color=color, compressed=zlib.compress(b"\0" + pixels)))
                self.assertEqual(self.validate()["functional_checks_passed"], accepted)

    def test_truncated_or_corrupt_png_cannot_pass_by_header_dimensions(self):
        for image in (png()[:24], png()[:-12], png()[:40] + b"broken" + png()[46:]):
            with self.subTest(length=len(image)):
                (self.root / "frame.png").write_bytes(image)
                self.assertFalse(self.validate()["functional_checks_passed"])

    def test_crc_correct_invalid_image_data_is_rejected(self):
        for compressed in (b"not-zlib", zlib.compress(b"short"), zlib.compress(b"\x05" + b"\x40\x80\xa0" * 2), zlib.compress(b"\0" + b"\x40\x80\xa0" * 2) + b"trailing"):
            with self.subTest(compressed=compressed):
                (self.root / "frame.png").write_bytes(png(compressed=compressed))
                self.assertTrue(any("frame.png" in message for message in self.validate()["failures"]))

    def test_correct_offset_from_the_wrong_cell_center_is_rejected(self):
        self.logs = self.logs.replace("2048.0", "4096.0")
        self.flush()
        self.assertIn("runtime camera target differs from the expected cell center", self.validate()["failures"])

    def test_png_must_match_recorded_physical_surface(self):
        self.metadata["window"] = {"physical_resolution": [4, 2]}
        self.flush()
        self.assertTrue(any("PNG dimensions" in message for message in self.validate()["failures"]))

    def test_retry_totals_must_conserve_entities_and_finish_drained(self):
        for accounting, expected in (
            ("tracked_total=2 resumed_total=1 canceled_total=0 pending=0", "does not conserve"),
            ("tracked_total=2 resumed_total=1 canceled_total=0 pending=1", "remain pending"),
        ):
            with self.subTest(accounting=accounting):
                self.logs += f"INFO retry: late mesh preparation retry drain render_frame=20 {accounting}\n"
                self.flush()
                self.assertTrue(any(expected in message for message in self.validate()["failures"]))
                self.logs = self.logs.split("INFO retry:")[0]

    def test_drained_retry_and_cancellation_are_valid(self):
        self.logs += "INFO retry: late mesh preparation observed for next frame render_frame=10 tracked_total=3 resumed_total=0 canceled_total=0 pending=3\n"
        self.logs += "INFO retry: late mesh preparation retry drain render_frame=20 tracked_total=3 resumed_total=2 canceled_total=1 pending=0\n"
        self.flush()
        result = self.validate()
        self.assertTrue(result["functional_checks_passed"], result["failures"])
        self.assertEqual(result["retry_accounting"]["last"]["canceled_total"], 1)

    def test_lost_device_is_recorded_apart_from_other_engine_failures(self):
        self.manifest["exit_code"] = RUNNER.EXIT_DEVICE_LOST
        result = self.validate()
        self.assertFalse(result["functional_checks_passed"])
        self.assertTrue(result["device_lost"])
        self.assertTrue(any("GPU device lost" in message for message in result["failures"]), result["failures"])
        self.assertNotIn("engine did not complete successfully", result["failures"])
        self.manifest["exit_code"] = 1
        result = self.validate()
        self.assertFalse(result["device_lost"])
        self.assertIn("engine did not complete successfully", result["failures"])

    def test_missing_artifacts_and_raw_errors_fail(self):
        (self.root / "profile/metadata.json").unlink()
        (self.root / "engine.stderr.log").write_text(self.logs + "ERROR shader: Shader compilation error\n")
        result = self.validate()
        self.assertFalse(result["functional_checks_passed"])
        self.assertTrue(any("metadata.json" in message for message in result["failures"]))
        self.assertTrue(any("raw logs" in message for message in result["failures"]))

    def test_only_the_exact_known_gamescope_cursor_warning_is_nonfatal(self):
        warning = "[gamescope] [Error] xwm: NO CURSOR IMPL XDG"
        self.logs += warning + "\n"
        self.flush()
        result = self.validate()
        self.assertTrue(result["functional_checks_passed"], result["failures"])
        self.assertTrue(any(warning in note for note in result["notes"]))
        for message in ("ERROR shader: Shader compilation error", "[gamescope] [Error] vulkan: GPU initialization failed", warning + " GPU failure"):
            with self.subTest(message=message):
                self.logs += message + "\n"
                self.flush()
                self.assertTrue(any("raw logs" in failure for failure in self.validate()["failures"]))
                self.logs = self.logs.rsplit(message + "\n", 1)[0]

    def test_malformed_nested_artifacts_fail_without_aborting_analysis(self):
        for artifact, field, label in (
            ("report.json", "streaming", "report streaming"),
            ("report.json", "renderer", "report renderer"),
            ("profile/cpu-spans.json", "gauges", "CPU gauges"),
            ("profile/metadata.json", "window", "metadata window"),
            ("profile/streaming.json", "timeline", "streaming timeline"),
            ("profile/streaming.json", "aggregate", "streaming aggregate"),
        ):
            with self.subTest(artifact=artifact, field=field):
                self.flush()
                path = self.root / artifact
                value = json.loads(path.read_text())
                value[field] = [1] if field != "timeline" else {"invalid": "array expected"}
                json_file(path, value)
                self.assertTrue(any(label in failure for failure in self.validate()["failures"]))


@unittest.skipUnless(os.name == "posix", "owned process-group tests require POSIX")
class ProcessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for name in RUNNER.INPUT_FILES:
            (self.assets / name).write_bytes(b"primary input fixture")
        self.engine = self.root / "fixture-engine"
        self.engine.write_text("#!/usr/bin/env python3\n", encoding="utf-8")
        self.engine.chmod(0o755)

    def command(self, output, *extra):
        return [sys.executable, str(SCRIPT), "--engine", str(self.engine), "--assets", str(self.assets), "--output", str(output), "--repeats", "1", "--upload-budgets", "16", "--warmup", "100", "--duration", "1", *extra]

    def test_interrupts_during_hashing_analysis_and_cleanup_finalize_campaign(self):
        untouched = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        try:
            for stage in ("baseline", "before", "after", "campaign", "analysis", "cleanup"):
                with self.subTest(stage=stage):
                    output = self.root / stage
                    real_hashes = RUNNER.input_hashes
                    real_cleanup = RUNNER.stop_owned_group
                    calls = 0

                    def hashes(*args):
                        nonlocal calls
                        calls += 1
                        if calls == {"baseline": 1, "before": 2, "after": 3, "campaign": 4}.get(stage):
                            raise KeyboardInterrupt
                        return real_hashes(*args)

                    def cleanup(process):
                        if stage == "cleanup":
                            # Repeated actual stop signals during cleanup must not interrupt it.
                            os.kill(os.getpid(), signal.SIGTERM)
                            os.kill(os.getpid(), signal.SIGTERM)
                        return real_cleanup(process)

                    if stage == "analysis":
                        analysis = mock.patch.object(RUNNER, "analyze_run", side_effect=KeyboardInterrupt)
                    elif stage == "campaign":
                        analysis = mock.patch.object(RUNNER, "analyze_run", return_value={
                            "functional_checks_passed": True, "visual_inspection": "pending",
                            "failures": [], "notes": [], "retry_accounting": {"status": "not_observed"},
                        })
                    else:
                        analysis = mock.patch.object(RUNNER, "analyze_run", wraps=RUNNER.analyze_run)
                    with mock.patch.object(RUNNER, "input_hashes", side_effect=hashes), \
                         mock.patch.object(RUNNER, "stop_owned_group", side_effect=cleanup), analysis:
                        status = RUNNER.main(self.command(output)[2:])
                    self.assertEqual(status, 130)
                    summary = json.loads((output / "summary.json").read_text())
                    self.assertTrue(summary["interrupted"])
                    self.assertIsNotNone(summary["ended_utc"])
                    self.assertFalse(summary["functional_checks_passed"])
                    self.assertIsNone(untouched.poll())
                    if stage == "baseline":
                        self.assertEqual(summary["runs"], [])
                        continue
                    self.assertEqual(len(summary["runs"]), 1)
                    manifest = json.loads((output / "16mib-01/run.json").read_text())
                    validation = json.loads((output / "16mib-01/validation.json").read_text())
                    self.assertIsNotNone(manifest["ended_utc"])
                    self.assertFalse(validation["functional_checks_passed"])
                    if stage != "campaign":
                        self.assertTrue(manifest["interrupted"])
                    if "owned_process_group" in manifest:
                        with self.assertRaises(ProcessLookupError):
                            os.kill(manifest["owned_process_group"], 0)
        finally:
            untouched.terminate()
            untouched.wait(timeout=5)

    def test_interrupt_before_second_launch_does_not_modify_completed_evidence(self):
        output = self.root / "second-launch-interrupt"
        completed = {
            "run_id": "16mib-01", "upload_budget_mib": 16,
            "functional_checks_passed": True, "visual_inspection": "pending",
            "failures": [], "notes": [], "retry_accounting": {"status": "not_observed"},
        }
        validation = {key: value for key, value in completed.items()
                      if key not in ("run_id", "upload_budget_mib")}

        def launch(args, directory, budget, run_id, environment):
            directory.mkdir()
            if run_id == "16mib-02":
                raise KeyboardInterrupt
            json_file(directory / "validation.json", validation)
            return copy.deepcopy(completed), False

        with mock.patch.object(RUNNER, "run_one", side_effect=launch):
            status = RUNNER.main(self.command(output, "--repeats", "2")[2:])
        self.assertEqual(status, 130)
        summary = json.loads((output / "summary.json").read_text())
        self.assertTrue(summary["interrupted"])
        self.assertFalse(summary["functional_checks_passed"])
        self.assertIsNotNone(summary["ended_utc"])
        self.assertEqual(summary["runs"], [completed])
        self.assertEqual(json.loads((output / "16mib-01/validation.json").read_text()), validation)
        self.assertFalse((output / "16mib-02/validation.json").exists())

    def test_output_collision_preserves_existing_results_and_does_not_launch(self):
        output = self.root / "existing"
        output.mkdir()
        sentinel = output / "retained.json"
        sentinel.write_text("do not replace")
        self.engine.write_text(self.engine.read_text() + "from pathlib import Path\nPath('launched').write_text('bad')\n")
        result = subprocess.run(self.command(output), capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(), "do not replace")
        self.assertFalse((output / "16mib-01").exists())

    def test_timeout_stops_its_own_descendants_and_leaves_another_process_alive(self):
        self.engine.write_text(self.engine.read_text() + (
            "import os, signal, subprocess, sys, time\n"
            "from pathlib import Path\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
            "Path('child.pid').write_text(str(child.pid))\n"
            "def stop(signum, frame):\n"
            "    child.wait(timeout=4)\n"
            "    sys.exit(0)\n"
            "signal.signal(signal.SIGTERM, stop)\n"
            "time.sleep(60)\n"
        ))
        untouched = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        try:
            output = self.root / "timeout"
            result = subprocess.run(self.command(output, "--timeout", "0.8"), capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 1, result.stderr)
            manifest = json.loads((output / "16mib-01/run.json").read_text())
            self.assertTrue(manifest["timed_out"])
            child_pid = int((output / "16mib-01/child.pid").read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
            self.assertIsNone(untouched.poll(), "a process outside the owned group was stopped")
        finally:
            untouched.terminate()
            untouched.wait(timeout=5)

    def test_launch_prefix_passes_shell_metacharacters_as_literal_arguments(self):
        wrapper = self.root / "argv-wrapper.py"
        wrapper.write_text("import pathlib, subprocess, sys\npathlib.Path('prefix.txt').write_text(sys.argv[1])\nsys.exit(subprocess.run(sys.argv[2:]).returncode)\n")
        forbidden = self.root / "shell-was-used"
        literal = f"$(touch {forbidden}); echo unsafe"
        prefix = shlex.join([sys.executable, str(wrapper), literal])
        output = self.root / "literal"
        result = subprocess.run(self.command(output, "--launch-prefix", prefix), capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 1, "the fixture intentionally omits render artifacts")
        self.assertEqual((output / "16mib-01/prefix.txt").read_text(), literal)
        self.assertFalse(forbidden.exists())

    def test_sigterm_preserves_partial_evidence_and_stops_the_owned_engine(self):
        self.engine.write_text(self.engine.read_text() + "import time\ntime.sleep(60)\n")
        output = self.root / "interrupted"
        campaign = subprocess.Popen(self.command(output), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            manifest_path = output / "16mib-01/run.json"
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if manifest_path.exists():
                    manifest = json.loads(manifest_path.read_text())
                    if "owned_process_group" in manifest:
                        break
                time.sleep(0.01)
            else:
                self.fail("the fixture engine did not start")
            os.kill(campaign.pid, signal.SIGTERM)
            _, stderr = campaign.communicate(timeout=10)
            self.assertEqual(campaign.returncode, 130, stderr)
            completed = json.loads(manifest_path.read_text())
            self.assertTrue(completed["interrupted"])
            self.assertIsNotNone(completed["ended_utc"])
            summary = json.loads((output / "summary.json").read_text())
            self.assertEqual(len(summary["runs"]), 1)
            self.assertFalse(summary["functional_checks_passed"])
            self.assertTrue((output / "16mib-01/validation.json").is_file())
            with self.assertRaises(ProcessLookupError):
                os.kill(completed["owned_process_group"], 0)
        finally:
            if campaign.poll() is None:
                campaign.kill()
                campaign.communicate(timeout=5)

    def test_interrupt_while_publishing_owned_group_preserves_partial_evidence(self):
        self.engine.write_text(self.engine.read_text() + "import time\ntime.sleep(60)\n")
        output = self.root / "interrupted-publication"
        output.mkdir()
        args = RUNNER.parse_args(self.command(output)[2:])
        write_json = RUNNER.write_json
        interrupted = False

        def interrupt_publication(path, value):
            nonlocal interrupted
            write_json(path, value)
            if "owned_process_group" in value and value["ended_utc"] is None and not interrupted:
                interrupted = True
                raise KeyboardInterrupt

        directory = output / "16mib-01"
        with patch.object(RUNNER, "write_json", side_effect=interrupt_publication):
            result, stopped = RUNNER.run_one(args, directory, 16, "16mib-01", os.environ.copy())
        self.assertTrue(stopped)
        self.assertFalse(result["functional_checks_passed"])
        manifest = json.loads((directory / "run.json").read_text())
        self.assertTrue(manifest["interrupted"])
        self.assertIsNotNone(manifest["ended_utc"])
        self.assertTrue((directory / "validation.json").is_file())
        with self.assertRaises(ProcessLookupError):
            os.kill(manifest["owned_process_group"], 0)


if __name__ == "__main__":
    unittest.main()
