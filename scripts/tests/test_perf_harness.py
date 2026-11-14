import gzip
import importlib.util
import io
import json
import os
import random
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
import zlib
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
PERF_PATH = REPO / "scripts" / "perf" / "perf.py"
spec = importlib.util.spec_from_file_location("perf_harness", PERF_PATH)
perf = importlib.util.module_from_spec(spec)
sys.modules["perf_harness"] = perf
spec.loader.exec_module(perf)

FAKE_ENGINE = """#!{python}
import json, os, sys, time
args = sys.argv[1:]
mode = os.environ.get("FAKE_MODE", "ok")
out = args[args.index("--benchmark-output") + 1]
report = {{
    "frame_ms_mean": 10.0, "frame_ms_p50": 9.0, "frame_ms_p95": 14.0, "frame_ms_p99": 18.0,
    "average_fps": 100.0, "passed": True, "warmup_frames": 60,
    "fingerprint": {{"commit": "x", "resolution": [100, 100]}},
    "scene": {{"mesh_entities": 10, "terrain_patches": 1, "water_surfaces": 0}},
    "thresholds": {{"renderer_path_active": True, "no_streaming_failures": True,
                    "commit_budget_respected": True, "streaming_lifecycle_validated": True}},
}}
def write():
    with open(out, "w") as handle:
        json.dump(report, handle)
if mode == "never":
    time.sleep(600)
elif mode == "crash":
    sys.exit(3)
elif mode == "hang":
    write()
    time.sleep(600)
elif mode == "nonzero":
    write()
    sys.exit(1)
else:
    write()
"""


def make_fake_engine(directory):
    path = Path(directory) / "fake-engine"
    path.write_text(FAKE_ENGINE.format(python=sys.executable), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


def run_entry(valid=True, fingerprint=None, scene=None, metrics=None, classification="ok"):
    return {
        "run": {"valid": valid, "classification": classification, "warnings": []},
        "fingerprint": fingerprint if fingerprint is not None else {"commit": "a", "resolution": [10, 10],
                                                                    "graphics": {"stream_radius": 2}},
        "scene": scene if scene is not None else {"mesh_entities": 1000, "terrain_patches": 10,
                                                  "water_surfaces": 2},
        "metrics": metrics if metrics is not None else {"frame_ms_mean": 10.0},
    }


def frame_metrics(scale, jitter=0.0):
    return {"frame_ms_mean": 10.0 * scale + jitter, "frame_ms_p50": 9.0 * scale + jitter,
            "frame_ms_p95": 14.0 * scale + jitter, "frame_ms_p99": 18.0 * scale + jitter,
            "average_fps": 100.0 / (scale + jitter / 10)}


class StatsTest(unittest.TestCase):
    def test_bootstrap_is_deterministic_for_a_seed(self):
        ratios = [0.98, 1.01, 1.03, 0.99, 1.0]
        a = perf.bootstrap_ci(ratios, seed=7, samples=2000)
        b = perf.bootstrap_ci(ratios, seed=7, samples=2000)
        self.assertEqual(a, b)
        self.assertLessEqual(a[0], a[1])
        self.assertGreaterEqual(a[0], min(ratios))
        self.assertLessEqual(a[1], max(ratios))

    def test_bootstrap_changes_with_the_seed(self):
        ratios = [0.9, 1.0, 1.1, 1.2, 0.95, 1.05, 1.15, 0.93, 1.02, 1.08]
        draws = {perf.bootstrap_ci(ratios, seed=seed, samples=20) for seed in range(1, 6)}
        self.assertGreater(len(draws), 1)

    def test_wilcoxon_matches_exact_tables(self):
        p = perf.wilcoxon_signed_rank_p
        # all n differences positive: two-sided p = 2 / 2**n
        self.assertAlmostEqual(p([1, 2, 3, 4, 5, 6]), 2 / 64)
        self.assertAlmostEqual(p([1, 2, 3, 4, 5]), 2 / 32)
        self.assertAlmostEqual(p([-1, -2, -3, -4, -5, -6, -7, -8]), 2 / 256)
        # n=8, the smallest rank negative: W+ = 35; W+ of 35 or 36 is 2 of 256 sign patterns per tail
        self.assertAlmostEqual(p([-1, 2, 3, 4, 5, 6, 7, 8]), 4 / 256)
        self.assertEqual(p([]), 1.0)
        self.assertEqual(p([0.0, 0.0]), 1.0)
        # symmetric data is as far from significant as it gets
        self.assertEqual(p([-3, -2, -1, 1, 2, 3]), 1.0)

    def test_wilcoxon_handles_ties(self):
        # all six equal and positive: still every sign pattern but one is less extreme
        self.assertAlmostEqual(perf.wilcoxon_signed_rank_p([0.5] * 6), 2 / 64)

    def test_noise_rarely_gives_a_verdict(self):
        # Same-binary runs: ratios scatter symmetrically around 1. The false-positive rate per metric
        # must stay near the 5% level, not the ~25% a 3-pair bootstrap interval gives.
        rng = random.Random(11)
        trials, positives = 400, 0
        for _ in range(trials):
            base = [10.0] * perf.MIN_PAIRS_FOR_VERDICT
            cand = [10.0 * rng.lognormvariate(0, 0.05) for _ in base]
            result = perf.compare_metric("frame_ms_mean", base, cand, seed=1, min_effect=0.02, samples=200)
            positives += result["verdict"] != "noise"
        self.assertLess(positives / trials, 0.07)

    def test_verdict_needs_significance_and_effect_size(self):
        v = perf.verdict_for
        self.assertEqual(v(0.90, 0.03, min_effect=0.02), "improved")
        self.assertEqual(v(1.10, 0.03, min_effect=0.02), "regressed")
        self.assertEqual(v(0.95, 0.20, min_effect=0.02), "noise")
        # significant but below the minimum effect
        self.assertEqual(v(0.99, 0.01, min_effect=0.02), "noise")
        # exactly at the minimum effect counts
        self.assertEqual(v(1.02, 0.01, min_effect=0.02), "regressed")
        self.assertEqual(v(1.20, None, min_effect=0.02), "noise")

    def test_higher_is_better_flips_direction(self):
        self.assertEqual(perf.verdict_for(1.2, 0.01, min_effect=0.02, higher_is_better=True), "improved")
        self.assertEqual(perf.verdict_for(0.8, 0.01, min_effect=0.02, higher_is_better=True), "regressed")

    def test_compare_metric_detects_a_consistent_regression(self):
        base = [10.0, 10.2, 9.9, 10.1, 10.0, 10.1]
        cand = [11.5, 11.6, 11.3, 11.6, 11.4, 11.5]
        result = perf.compare_metric("frame_ms_mean", base, cand, seed=3, min_effect=0.02, samples=2000)
        self.assertEqual(result["verdict"], "regressed")
        self.assertEqual(result["pairs"], 6)
        self.assertGreater(result["ratio_median"], 1.1)
        self.assertLessEqual(result["p_value"], 0.05)

    def test_five_consistent_pairs_are_not_enough(self):
        base = [10.0] * 5
        cand = [11.5] * 5
        result = perf.compare_metric("frame_ms_mean", base, cand, seed=3, min_effect=0.02, samples=200)
        self.assertEqual(result["verdict"], "insufficient_pairs")

    def test_compare_metric_is_noise_when_pairs_disagree(self):
        base = [10.0, 10.0, 10.0, 10.0, 10.0, 10.0]
        cand = [10.5, 9.6, 10.2, 9.8, 10.1, 9.9]
        result = perf.compare_metric("frame_ms_mean", base, cand, seed=3, min_effect=0.02, samples=2000)
        self.assertEqual(result["verdict"], "noise")

    def test_too_few_pairs_gives_no_verdict(self):
        result = perf.compare_metric("frame_ms_mean", [10.0, 10.0], [20.0, 20.0], seed=1, min_effect=0.02)
        self.assertEqual(result["verdict"], "insufficient_pairs")

    def test_missing_values_are_dropped_pairwise(self):
        result = perf.compare_metric("m", [1.0, None, 1.0, 1.0], [1.0, 1.0, 1.0, 1.0], seed=1, min_effect=0.02)
        self.assertEqual(result["pairs"], 3)


class GuardTest(unittest.TestCase):
    def analyze(self, base, cand, **kwargs):
        pairs = [{"base": b, "cand": c} for b, c in zip(base, cand)]
        return perf.analyze_scenario("dense", pairs, seed=1, min_effect=0.02, samples=500, **kwargs)

    def runs(self, count, **kwargs):
        return [run_entry(metrics=frame_metrics(1.0, 0.01 * i), **kwargs) for i in range(count)]

    def test_same_conditions_compare(self):
        result = self.analyze(self.runs(6), self.runs(6))
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["summary_verdict"], "noise")

    def test_commit_and_dirty_differences_are_ignored(self):
        cand = [run_entry(metrics=frame_metrics(1.0), fingerprint={
            "commit": "b", "dirty_worktree": True, "resolution": [10, 10], "graphics": {"stream_radius": 2}})
            for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["status"], "ok")

    def test_resolution_mismatch_refuses(self):
        cand = [run_entry(fingerprint={"commit": "a", "resolution": [20, 10], "graphics": {"stream_radius": 2}})
                for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["error"], "fingerprint_mismatch")
        self.assertTrue(any("resolution" in line for line in result["details"]))

    def test_graphics_setting_mismatch_refuses(self):
        cand = [run_entry(fingerprint={"commit": "a", "resolution": [10, 10], "graphics": {"stream_radius": 3}})
                for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["error"], "fingerprint_mismatch")
        self.assertTrue(any("graphics.stream_radius" in line for line in result["details"]))

    def test_backend_mismatch_within_one_side_refuses(self):
        base = [run_entry(fingerprint={"commit": "a", "renderer": {"backend": "Metal"}}) for _ in range(2)]
        base.append(run_entry(fingerprint={"commit": "a", "renderer": {"backend": "Vulkan"}}))
        cand = [run_entry(fingerprint={"commit": "a", "renderer": {"backend": "Metal"}}) for _ in range(3)]
        result = self.analyze(base, cand)
        self.assertEqual(result["error"], "fingerprint_mismatch")

    def test_inventory_mismatch_refuses(self):
        cand = [run_entry(scene={"mesh_entities": 700, "terrain_patches": 10, "water_surfaces": 2})
                for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["error"], "inventory_mismatch")
        self.assertIn("mesh_entities", result["details"][0])

    def test_inventory_within_tolerance_passes(self):
        cand = [run_entry(scene={"mesh_entities": 1010, "terrain_patches": 10, "water_surfaces": 2},
                          metrics=frame_metrics(1.0)) for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["status"], "ok")

    def test_small_scene_counts_get_absolute_slack(self):
        self.assertEqual(perf.check_inventory([{"water_surfaces": 0}], [{"water_surfaces": 2}]), [])
        self.assertTrue(perf.check_inventory([{"water_surfaces": 0}], [{"water_surfaces": 5}]))

    def test_invalid_runs_drop_their_pair(self):
        base = self.runs(4)
        cand = self.runs(4)
        cand[1] = run_entry(valid=False, classification="timeout_no_report")
        result = self.analyze(base, cand)
        self.assertEqual(result["pairs_used"], 3)
        self.assertEqual(result["dropped_pairs"], [1])

    def test_too_few_valid_pairs_refuses(self):
        cand = [run_entry(valid=False, classification="failed") for _ in range(3)]
        result = self.analyze(self.runs(3), cand)
        self.assertEqual(result["error"], "too_few_valid_pairs")

    def test_a_real_regression_is_reported(self):
        base = [run_entry(metrics=frame_metrics(1.0, 0.02 * i)) for i in range(6)]
        cand = [run_entry(metrics=frame_metrics(1.2, 0.02 * i)) for i in range(6)]
        result = self.analyze(base, cand)
        self.assertEqual(result["summary_verdict"], "regressed")
        self.assertEqual(result["metrics"]["average_fps"]["verdict"], "regressed")

    def test_a_report_without_primary_metrics_is_inconclusive_not_noise(self):
        base = [run_entry(metrics={"frame_ms_mean": 10.0 + 0.01 * i}) for i in range(6)]
        cand = [run_entry(metrics={"frame_ms_mean": 10.0 + 0.01 * i}) for i in range(6)]
        result = self.analyze(base, cand)
        self.assertEqual(result["summary_verdict"], "inconclusive")
        self.assertTrue(any("frame_ms_p95" in line for line in result["details"]))

    def test_a_primary_metric_with_too_few_usable_values_is_inconclusive(self):
        base = self.runs(6)
        cand = self.runs(6)
        for entry in cand[:3]:
            del entry["metrics"]["frame_ms_p99"]
        result = self.analyze(base, cand)
        self.assertEqual(result["summary_verdict"], "inconclusive")

    def test_inconclusive_never_reads_as_improved_or_noise_overall(self):
        inconclusive = {"status": "ok", "summary_verdict": "inconclusive"}
        noise = {"status": "ok", "summary_verdict": "noise"}
        improved = {"status": "ok", "summary_verdict": "improved"}
        self.assertEqual(perf.overall_verdict({"a": inconclusive, "b": noise}), "inconclusive")
        self.assertEqual(perf.overall_verdict({"a": inconclusive, "b": improved}), "inconclusive")

    def test_a_run_that_fell_back_to_another_render_path_is_refused_not_improved(self):
        base = self.runs(6)
        cand = [run_entry(metrics=frame_metrics(0.5, 0.01 * i)) for i in range(6)]
        cand[2]["health_problems"] = ["thresholds.renderer_path_active is false"]
        result = self.analyze(base, cand)
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["error"], "unhealthy_run")
        self.assertTrue(any("renderer_path_active" in line for line in result["details"]))

    def test_report_health_reads_the_specific_fields_not_passed(self):
        healthy = {"renderer_path_active": True, "no_streaming_failures": True,
                   "commit_budget_respected": True, "streaming_lifecycle_validated": True}
        # `passed` false because of an FPS limit is a slowdown to report, not a broken run.
        self.assertEqual(perf.report_health({"passed": False, "thresholds": healthy}), [])
        broken = dict(healthy, renderer_path_active=False)
        self.assertEqual(perf.report_health({"passed": True, "thresholds": broken}),
                         ["thresholds.renderer_path_active is false"])
        missing = {k: v for k, v in healthy.items() if k != "no_streaming_failures"}
        self.assertEqual(perf.report_health({"thresholds": missing}),
                         ["thresholds.no_streaming_failures is missing"])
        self.assertEqual(len(perf.report_health({})), 1)

    def test_a_scene_count_missing_from_one_side_is_reported(self):
        base = [{"mesh_entities": 1000, "terrain_patches": 10, "water_surfaces": 2}] * 3
        cand = [{"mesh_entities": None, "terrain_patches": 10, "water_surfaces": 2}] * 3
        problems = perf.check_inventory(base, cand)
        self.assertEqual(len(problems), 1)
        self.assertIn("mesh_entities", problems[0])

    def test_a_scene_count_missing_from_some_runs_is_reported(self):
        base = [{"mesh_entities": 1000}, {"mesh_entities": None}]
        cand = [{"mesh_entities": 1000}, {"mesh_entities": 1000}]
        self.assertTrue(perf.check_inventory(base, cand))

    def test_a_count_missing_everywhere_is_skipped_but_one_must_compare(self):
        both = [{"mesh_entities": 10, "terrain_patches": None, "water_surfaces": None}] * 2
        self.assertEqual(perf.check_inventory(both, both), [])
        none = [{"mesh_entities": None, "terrain_patches": None, "water_surfaces": None}] * 2
        self.assertTrue(perf.check_inventory(none, none))

    def test_a_span_absent_from_some_pairs_is_ranked_by_the_pairs_that_have_it(self):
        base = [run_entry(metrics=dict(frame_metrics(1.0, 0.01 * i), **{"cpu/rare": 3.0, "cpu/common": 1.0}))
                for i in range(6)]
        cand = [run_entry(metrics=dict(frame_metrics(1.0, 0.01 * i), **{"cpu/rare": 3.0, "cpu/common": 1.0}))
                for i in range(6)]
        for entry in base[:3] + cand[:3]:
            del entry["metrics"]["cpu/rare"]
        result = self.analyze(base, cand)
        self.assertEqual(result["status"], "ok")
        self.assertIn("cpu/rare", result["metrics"])
        self.assertEqual(list(result["metrics"])[-2:], ["cpu/rare", "cpu/common"])

    def test_overall_verdict(self):
        ok = {"status": "ok", "summary_verdict": "noise"}
        self.assertEqual(perf.overall_verdict({"a": ok}), "noise")
        self.assertEqual(perf.overall_verdict({"a": ok, "b": {"status": "refused"}}), "refused")
        self.assertEqual(perf.overall_verdict({"a": ok, "b": {"status": "ok", "summary_verdict": "improved"}}),
                         "improved")
        self.assertEqual(perf.overall_verdict({"a": {"status": "ok", "summary_verdict": "regressed"},
                                               "b": {"status": "ok", "summary_verdict": "improved"}}), "mixed")


class MetricExtractionTest(unittest.TestCase):
    def test_zero_gpu_passes_and_rare_spans_are_left_out(self):
        report = {
            "frame_ms_mean": 10.0, "average_fps": 100.0, "peak_process_memory_gib": None,
            "gpu_passes": {"passes": {"a/elapsed_gpu": {"mean": 0.0}, "b/elapsed_gpu": {"mean": 1.5}}},
            "cpu": {"schedules": {"main/Update": {"mean": 0.2, "count": 100}},
                    "spans": {"once": {"mean": 5.0, "count": 1}, "often": {"mean": 2.0, "count": 100}}},
        }
        metrics = perf.extract_metrics(report)
        self.assertEqual(metrics["gpu/b/elapsed_gpu"], 1.5)
        self.assertNotIn("gpu/a/elapsed_gpu", metrics)
        self.assertNotIn("cpu/once", metrics)
        self.assertEqual(metrics["cpu/often"], 2.0)
        self.assertEqual(metrics["cpu/main/Update"], 0.2)
        self.assertNotIn("memory_peak_gib", metrics)

    def test_memory_peak_falls_back_to_the_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "memory.json").write_text(json.dumps({"samples": [{"process_gib": 1.0},
                                                                         {"process_gib": 2.5}]}))
            metrics = perf.extract_metrics({"peak_process_memory_gib": None}, tmp)
        self.assertEqual(metrics["memory_peak_gib"], 2.5)


@unittest.skipIf(sys.platform == "win32", "the fixtures are shebang scripts, which Windows cannot execute")
class WatchdogTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.engine = make_fake_engine(self.tmp.name)
        self.scenario = {"needs_assets": False, "args": ["--benchmark-only"]}

    def run_mode(self, mode, **kwargs):
        out = Path(self.tmp.name) / mode
        options = {"assets": None, "out_dir": out, "duration": 1, "timeout": 5, "grace": 1, "poll": 0.05}
        options.update(kwargs)
        with mock.patch.dict(os.environ, {"FAKE_MODE": mode}):
            return perf.run_scenario(self.engine, "synthetic", self.scenario, **options)

    def test_clean_exit_is_ok(self):
        run = self.run_mode("ok")
        self.assertEqual(run["classification"], "ok")
        self.assertTrue(run["valid"])
        self.assertEqual(run["binary_sha256"], perf.sha256_file(self.engine))
        self.assertTrue((Path(self.tmp.name) / "ok" / "run.json").is_file())
        self.assertIn("--benchmark-output", run["command"])

    def test_hang_after_report_is_killed_and_still_valid(self):
        run = self.run_mode("hang", grace=0.5, timeout=20)
        self.assertEqual(run["classification"], "hung_after_report")
        self.assertTrue(run["valid"])
        self.assertTrue(run["killed"])
        self.assertLess(run["wall_seconds"], 10)

    def test_no_report_times_out(self):
        run = self.run_mode("never", timeout=1)
        self.assertEqual(run["classification"], "timeout_no_report")
        self.assertFalse(run["valid"])
        self.assertLess(run["wall_seconds"], 10)

    def test_crash_without_report_is_failed(self):
        run = self.run_mode("crash")
        self.assertEqual(run["classification"], "failed")
        self.assertEqual(run["exit_code"], 3)
        self.assertFalse(run["valid"])

    def test_nonzero_exit_after_a_complete_report_is_kept_with_a_warning(self):
        run = self.run_mode("nonzero")
        self.assertEqual(run["classification"], "ok")
        self.assertTrue(run["valid"])
        self.assertTrue(any("exited with 1" in w for w in run["warnings"]))

    def test_stale_report_is_not_mistaken_for_a_result(self):
        out = Path(self.tmp.name) / "never"
        out.mkdir()
        (out / "report.json").write_text(json.dumps({"frame_ms_mean": 1, "fingerprint": {}}))
        run = self.run_mode("never", timeout=1)
        self.assertEqual(run["classification"], "timeout_no_report")

    def test_half_written_report_does_not_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp, "report.json")
            path.write_text('{"frame_ms_mean": 1.0, "finger')
            self.assertIsNone(perf.read_complete_report(path))

    def test_load_run_reads_metrics(self):
        run = self.run_mode("ok")
        loaded = perf.load_run(Path(run["report"]).parent)
        self.assertEqual(loaded["metrics"]["frame_ms_mean"], 10.0)
        self.assertEqual(loaded["scene"]["mesh_entities"], 10)

    def test_load_run_records_health_problems(self):
        run = self.run_mode("ok")
        report_path = Path(run["report"])
        self.assertEqual(perf.load_run(report_path.parent)["health_problems"], [])
        report = json.loads(report_path.read_text())
        report["thresholds"]["renderer_path_active"] = False
        report_path.write_text(json.dumps(report))
        self.assertEqual(perf.load_run(report_path.parent)["health_problems"],
                         ["thresholds.renderer_path_active is false"])

    def test_an_interrupted_watch_stops_the_engine_it_started(self):
        pid_file = Path(self.tmp.name) / "pid"
        script = Path(self.tmp.name) / "sleeper.py"
        script.write_text(f"import os, time\nopen({str(pid_file)!r}, 'w').write(str(os.getpid()))\ntime.sleep(600)\n")
        calls = []

        def interrupted():
            calls.append(1)
            if not pid_file.exists():
                return False
            raise KeyboardInterrupt

        with self.assertRaises(KeyboardInterrupt):
            perf.watch_process([sys.executable, str(script)], stdout_path=Path(self.tmp.name) / "o",
                               stderr_path=Path(self.tmp.name) / "e", is_done=interrupted, timeout=30,
                               grace=1, poll=0.05)
        pid = int(pid_file.read_text())
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            self.fail("the engine was still running after the interruption")

    def test_a_system_exit_mid_watch_stops_the_engine(self):
        pid_file = Path(self.tmp.name) / "pid"
        script = Path(self.tmp.name) / "sleeper.py"
        script.write_text(f"import os, time\nopen({str(pid_file)!r}, 'w').write(str(os.getpid()))\ntime.sleep(600)\n")

        def exiting():
            if not pid_file.exists():
                return False
            raise SystemExit(143)

        with self.assertRaises(SystemExit):
            perf.watch_process([sys.executable, str(script)], stdout_path=Path(self.tmp.name) / "o",
                               stderr_path=Path(self.tmp.name) / "e", is_done=exiting, timeout=30,
                               grace=1, poll=0.05)
        pid = int(pid_file.read_text())
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            self.fail("the engine was still running after SystemExit")

    def test_termination_handlers_turn_signals_into_system_exit(self):
        registered = {}
        previous = perf.install_termination_handlers(register=lambda n, h: registered.setdefault(n, h) and "old")
        wanted = {n for n in (getattr(perf.signal, "SIGTERM", None), getattr(perf.signal, "SIGHUP", None)) if n}
        self.assertEqual(set(registered), wanted)
        self.assertEqual(set(previous), wanted)
        for number, handler in registered.items():
            with self.assertRaises(SystemExit) as caught:
                handler(number, None)
            self.assertEqual(caught.exception.code, 128 + number)

    def test_main_installs_the_handlers_and_restores_the_previous_ones(self):
        before = perf.signal.getsignal(perf.signal.SIGTERM)
        seen = []
        args = mock.Mock(func=lambda a: seen.append(perf.signal.getsignal(perf.signal.SIGTERM)) or 0)
        with mock.patch.object(perf, "build_parser") as parser:
            parser.return_value.parse_args.return_value = args
            self.assertEqual(perf.main([]), 0)
        self.assertEqual(seen, [perf._exit_on_signal])
        self.assertEqual(perf.signal.getsignal(perf.signal.SIGTERM), before)

    def test_an_interrupted_watch_uses_the_custom_terminator(self):
        stopped = []

        def terminate(proc):
            stopped.append(proc.pid)
            proc.kill()
            proc.wait()
            return True

        with self.assertRaises(RuntimeError):
            perf.watch_process([sys.executable, "-c", "import time; time.sleep(600)"],
                               stdout_path=Path(self.tmp.name) / "o", stderr_path=Path(self.tmp.name) / "e",
                               is_done=mock.Mock(side_effect=RuntimeError("boom")), timeout=30, grace=1,
                               poll=0.05, terminate=terminate)
        self.assertEqual(len(stopped), 1)

    def test_missing_binary_does_not_hang(self):
        out = Path(self.tmp.name) / "nobin"
        watch = perf.watch_process([str(Path(self.tmp.name) / "absent")], stdout_path=Path(self.tmp.name) / "o",
                                   stderr_path=Path(self.tmp.name) / "e", is_done=lambda: False, timeout=1,
                                   grace=1)
        self.assertEqual(perf.classify(watch), "failed")
        self.assertFalse(out.exists())

    def test_assets_are_required_for_world_scenarios(self):
        scenario = {"needs_assets": True, "args": []}
        with self.assertRaises(perf.HarnessError) as caught:
            perf.run_scenario(self.engine, "dense", scenario, assets=None, out_dir=Path(self.tmp.name) / "x")
        self.assertEqual(caught.exception.name, "assets_required")


def encode_png(width, height, channels, rows, filter_type):
    """A PNG whose rows are stored with the given filter type, to exercise the decoder."""
    stride = width * channels
    out = bytearray()
    prev = bytes(stride)
    for row in rows:
        out.append(filter_type)
        for i in range(stride):
            a = row[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = a
            elif filter_type == 2:
                predictor = b
            elif filter_type == 3:
                predictor = (a + b) // 2
            else:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                predictor = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
            out.append((row[i] - predictor) & 255)
        prev = row

    def chunk(kind, body):
        return len(body).to_bytes(4, "big") + kind + body + (zlib.crc32(kind + body) & 0xFFFFFFFF).to_bytes(4, "big")

    ctype = {1: 0, 2: 4, 3: 2, 4: 6}[channels]
    header = width.to_bytes(4, "big") + height.to_bytes(4, "big") + bytes([8, ctype, 0, 0, 0])
    return perf.PNG_SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(bytes(out))) + chunk(b"IEND", b"")


def gradient_rows(width, height, channels):
    return [bytes((x * 7 + y * 13 + c * 29) % 256 for x in range(width) for c in range(channels))
            for y in range(height)]


def solid_png(path, width, height, value, channels=3, noise=0):
    rows = []
    for y in range(height):
        rows.append(bytes(min(255, max(0, value + ((x * 31 + y * 17) % (noise + 1) if noise else 0)))
                          for x in range(width) for _ in range(channels)))
    perf.write_png(path, width, height, channels, b"".join(rows))


class NonPosixFallbackTest(unittest.TestCase):
    """The platform flag is injected; this was not run on Windows."""

    def test_non_posix_spawn_options_use_a_new_process_group(self):
        self.assertEqual(perf.spawn_options(posix=True), {"start_new_session": True})
        with mock.patch.object(perf.subprocess, "CREATE_NEW_PROCESS_GROUP", 512, create=True):
            self.assertEqual(perf.spawn_options(posix=False), {"creationflags": 512})

    def test_non_posix_termination_uses_terminate_then_kill(self):
        proc = mock.Mock(pid=1)
        proc.wait.return_value = 0
        with mock.patch.object(perf.os, "killpg", create=True) as killpg:
            self.assertFalse(perf._terminate_group(proc, grace=1, posix=False))
        proc.terminate.assert_called_once_with()
        proc.kill.assert_not_called()
        killpg.assert_not_called()

    def test_non_posix_termination_kills_when_terminate_is_ignored(self):
        proc = mock.Mock(pid=1)
        proc.wait.side_effect = [subprocess.TimeoutExpired("x", 1), 0]
        self.assertTrue(perf._terminate_group(proc, grace=1, posix=False))
        proc.terminate.assert_called_once_with()
        proc.kill.assert_called_once_with()

    def test_pid_probe_never_signals_off_posix(self):
        with mock.patch.object(perf, "POSIX", False), mock.patch.object(perf.os, "kill") as kill:
            self.assertFalse(perf.pid_alive(1234))
        kill.assert_not_called()


class PngTest(unittest.TestCase):
    def test_every_filter_type_decodes(self):
        for channels in (1, 2, 3, 4):
            rows = gradient_rows(9, 6, channels)
            for filter_type in range(5):
                with self.subTest(channels=channels, filter=filter_type), tempfile.TemporaryDirectory() as tmp:
                    path = Path(tmp, "a.png")
                    path.write_bytes(encode_png(9, 6, channels, rows, filter_type))
                    width, height, got_channels, pixels = perf.decode_png(path)
                    self.assertEqual((width, height, got_channels), (9, 6, channels))
                    self.assertEqual(pixels, b"".join(rows))

    def test_write_png_round_trips(self):
        with tempfile.TemporaryDirectory() as tmp:
            rows = gradient_rows(5, 4, 3)
            path = Path(tmp, "a.png")
            perf.write_png(path, 5, 4, 3, b"".join(rows))
            self.assertEqual(perf.decode_png(path)[3], b"".join(rows))

    def test_rejects_unsupported_and_corrupt_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp, "bad.png")
            path.write_bytes(b"not a png at all")
            with self.assertRaises(perf.PngError):
                perf.decode_png(path)
            good = encode_png(4, 4, 3, gradient_rows(4, 4, 3), 0)
            interlaced = bytearray(good)
            interlaced[8 + 8 + 12] = 1  # interlace byte of IHDR
            path.write_bytes(bytes(interlaced))
            with self.assertRaises(perf.PngError):
                perf.decode_png(path)

    def test_blank_and_black_detection(self):
        with tempfile.TemporaryDirectory() as tmp:
            black = Path(tmp, "black.png")
            solid_png(black, 32, 32, 0)
            grey = Path(tmp, "grey.png")
            solid_png(grey, 32, 32, 128)
            real = Path(tmp, "real.png")
            perf.write_png(real, 8, 8, 3, bytes((x * 9 + y * 5 + c * 40) % 256
                                                for y in range(8) for x in range(8) for c in range(3)))
            self.assertTrue(perf.blank_stats(perf.decode_png(black))["black"])
            self.assertTrue(perf.blank_stats(perf.decode_png(grey))["blank"])
            self.assertFalse(perf.blank_stats(perf.decode_png(grey))["black"])
            stats = perf.blank_stats(perf.decode_png(real))
            self.assertFalse(stats["blank"] or stats["black"])

    def test_pixel_diff(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp, "a.png"), Path(tmp, "b.png")
            solid_png(a, 16, 16, 100)
            solid_png(b, 16, 16, 120)
            same = perf.pixel_diff(perf.decode_png(a), perf.decode_png(a))
            self.assertEqual(same["mean_abs_diff"], 0)
            diff = perf.pixel_diff(perf.decode_png(a), perf.decode_png(b))
            self.assertAlmostEqual(diff["mean_abs_diff"], 20.0)
            self.assertEqual(diff["fraction_over"], 1.0)
            solid_png(Path(tmp, "c.png"), 8, 8, 100)
            with self.assertRaises(perf.PngError):
                perf.pixel_diff(perf.decode_png(a), perf.decode_png(Path(tmp, "c.png")))

    def test_rgba_alpha_is_ignored(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp, "a.png"), Path(tmp, "b.png")
            pixels = bytes([10, 20, 30, 255] * 16)
            perf.write_png(a, 4, 4, 4, pixels)
            perf.write_png(b, 4, 4, 4, bytes([10, 20, 30, 0] * 16))
            self.assertEqual(perf.pixel_diff(perf.decode_png(a), perf.decode_png(b))["mean_abs_diff"], 0)


class VisualTest(unittest.TestCase):
    def textured(self, path, shift=0, size=24):
        pixels = bytes(((x * 9 + y * 5 + c * 40 + shift) * 7) % 256
                       for y in range(size) for x in range(size) for c in range(3))
        perf.write_png(path, size, size, 3, pixels)

    def make_dirs(self, tmp, names, shift=0):
        base, cand = Path(tmp, "base"), Path(tmp, "cand")
        base.mkdir()
        cand.mkdir()
        for i, name in enumerate(names):
            self.textured(base / f"{name}.png", size=20 + i)
            self.textured(cand / f"{name}.png", shift=shift, size=20 + i)
        return base, cand

    def test_identical_captures_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a", "b"])
            result = perf.visual_compare(base, cand, Path(tmp, "out"), force_stdlib=True)
            self.assertEqual(result["overall"], "pass")
            self.assertEqual(result["metric"], "stdlib")
            self.assertEqual(result["review"], [])
            self.assertTrue(Path(tmp, "out", "visual.json").is_file())

    # A wrong keyword for flip-evaluator does not raise at import time, and choose_metric then
    # falls back to the stdlib metric with only a warning, so check the real call wherever it can.
    @unittest.skipUnless(perf.flip_available(), "flip-evaluator and numpy are not installed")
    def test_flip_runs_when_installed(self):
        self.assertEqual(perf.choose_metric(), ("flip", None))
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a"], shift=90)
            result = perf.visual_compare(base, cand, Path(tmp, "out"))
            self.assertEqual(result["metric"], "flip")
            self.assertEqual(result["overall"], "fail")
            self.assertTrue(Path(tmp, "out", "a.flip.png").is_file())

    def test_a_changed_shot_fails_and_is_queued_for_review(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a", "b"])
            self.textured(cand / "b.png", shift=40, size=21)
            result = perf.visual_compare(base, cand, Path(tmp, "out"), force_stdlib=True)
            self.assertEqual(result["shots"]["a"]["status"], "pass")
            self.assertEqual(result["shots"]["b"]["status"], "fail")
            self.assertEqual(result["review"], ["b"])

    def test_black_capture_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a"])
            solid_png(cand / "a.png", 20, 20, 0)
            result = perf.visual_compare(base, cand, Path(tmp, "out"), force_stdlib=True)
            self.assertIn("blank_frame_cand", result["shots"]["a"]["reasons"])
            self.assertEqual(result["overall"], "fail")

    def test_identical_file_sizes_mark_a_suspect_black_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = Path(tmp, "base"), Path(tmp, "cand")
            base.mkdir()
            cand.mkdir()
            for name in ("a", "b", "c"):
                solid_png(base / f"{name}.png", 20, 20, 0)
                solid_png(cand / f"{name}.png", 20, 20, 0)
            result = perf.visual_compare(base, cand, Path(tmp, "out"), force_stdlib=True)
            self.assertTrue(result["suspect_black_run"])
            self.assertEqual(result["review"], ["a", "b", "c"])

    def test_missing_shot_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a", "b"])
            (cand / "b.png").unlink()
            result = perf.visual_compare(base, cand, Path(tmp, "out"), force_stdlib=True)
            self.assertIn("missing_cand", result["shots"]["b"]["reasons"])

    def test_noise_floor_raises_the_limit(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a"])
            self.textured(cand / "a.png", shift=1, size=20)
            strict = perf.visual_compare(base, cand, Path(tmp, "o1"), force_stdlib=True, margin_mean=0.0,
                                         margin_fraction=0.0)
            self.assertEqual(strict["shots"]["a"]["status"], "fail")
            floor = Path(tmp, "floor.json")
            floor.write_text(json.dumps({"metric": "stdlib", "measured": strict["measured"]}))
            relaxed = perf.visual_compare(base, cand, Path(tmp, "o2"), noise_floor=floor, force_stdlib=True,
                                          margin_mean=0.0, margin_fraction=0.0)
            self.assertEqual(relaxed["shots"]["a"]["status"], "pass")

    def test_noise_floor_of_another_metric_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            base, cand = self.make_dirs(tmp, ["a"])
            floor = Path(tmp, "floor.json")
            floor.write_text(json.dumps({"metric": "flip", "measured": {}}))
            with self.assertRaises(perf.HarnessError) as caught:
                perf.visual_compare(base, cand, Path(tmp, "o"), noise_floor=floor, force_stdlib=True)
            self.assertEqual(caught.exception.name, "noise_floor_metric_mismatch")


class ScenarioFileTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        source = (REPO / "crates" / "engine" / "src" / "config.rs").read_text(encoding="utf-8")
        cls.known_flags = set(re.findall(r'"(--[a-z0-9][a-z0-9-]*)"', source))
        cls.scenarios = perf.load_scenarios()

    def test_flags_exist_in_the_engine(self):
        for name, scenario in self.scenarios.items():
            args = perf.build_engine_args(name, scenario, assets="/assets", out_dir="/out", duration=10,
                                          warmup_frames=None, window="hidden", commit="c", run_id="r",
                                          frame_budget_ms=6.25)
            for flag in (a for a in args if a.startswith("--")):
                self.assertIn(flag, self.known_flags, f"{name}: {flag} is not an engine flag")

    def test_shape(self):
        self.assertIn("synthetic", self.scenarios)
        self.assertIn("terrain-water", self.scenarios)
        for name, scenario in self.scenarios.items():
            self.assertIsInstance(scenario["needs_assets"], bool, name)
            self.assertIsInstance(scenario["args"], list, name)
            self.assertTrue(scenario.get("description"), name)

    def test_phase2_baseline_places_are_kept(self):
        script = (REPO / "scripts" / "phase2-baseline.ps1").read_text(encoding="utf-8")
        defaults = dict(re.findall(r"\[int\]\$(\w+GridX|\w+GridY) = (-?\d+)", script))
        for scenario, prefix in (("rural", "Rural"), ("dense", "Dense"), ("dense-legacy", "DenseLegacy"),
                                 ("water", "Water")):
            args = self.scenarios[scenario]["args"]
            self.assertEqual(args[args.index("--grid-x") + 1], defaults[f"{prefix}GridX"], scenario)
            self.assertEqual(args[args.index("--grid-y") + 1], defaults[f"{prefix}GridY"], scenario)

    def test_route_scenarios_point_at_route_files_and_get_long_enough_windows(self):
        for name in ("travel", "check"):
            scenario = self.scenarios[name]
            self.assertTrue((REPO / "scripts" / scenario["route"]).is_file())
            travel = perf.route_travel_seconds(REPO / "scripts" / scenario["route"])
            self.assertEqual(perf.effective_duration(scenario, 10), travel + perf.ROUTE_READINESS_SECONDS)

    def test_world_scenarios_need_assets_and_fixtures_do_not(self):
        self.assertFalse(self.scenarios["synthetic"]["needs_assets"])
        self.assertFalse(self.scenarios["terrain-water"]["needs_assets"])
        self.assertTrue(self.scenarios["dense"]["needs_assets"])

    def test_shots_file_is_valid(self):
        shots = json.loads((REPO / "scripts" / "perf" / "shots" / "riverwood.json").read_text())
        self.assertEqual((shots["width"], shots["height"]), (1280, 720))
        names = [s["name"] for s in shots["shots"]]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(3 <= len(names) <= 4)
        for shot in shots["shots"]:
            self.assertEqual(shot["worldspace_id"], 60)
            self.assertEqual(len(shot["position"]), 3)
            for key in ("yaw", "pitch", "hfov"):
                self.assertIsInstance(shot[key], (int, float))


PMSET_BATT_AC = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t81%; charging; 0:53 remaining present: true\n"
PMSET_BATT_BATTERY = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t81%; discharging\n"
PMSET_THERM_OK = "Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\n"
PMSET_THERM_LIMITED = "CPU_Scheduler_Limit = 100\nCPU_Available_CPUs = 12\nCPU_Speed_Limit = 70\n"


class PreflightTest(unittest.TestCase):
    def checks(self, batt=PMSET_BATT_AC, therm=PMSET_THERM_OK, general=" powermode 0\n", busy=(), free=100 * 1024**3,
               system="Darwin"):
        def output(command):
            return {"batt": batt, "therm": therm}.get(command[2] if len(command) > 2 else "", general)

        return perf.preflight_checks(".", system=system, command_output=output, free_bytes=free,
                                     processes=lambda names: [n for n in names if n in busy])

    def status(self, checks, name):
        return next(c["status"] for c in checks if c["name"] == name)

    def test_parsers(self):
        self.assertEqual(perf.parse_power_source(PMSET_BATT_AC), "AC Power")
        self.assertEqual(perf.parse_power_source(PMSET_BATT_BATTERY), "Battery Power")
        self.assertEqual(perf.parse_cpu_speed_limit(PMSET_THERM_LIMITED), 70)
        self.assertIsNone(perf.parse_cpu_speed_limit(PMSET_THERM_OK))
        self.assertTrue(perf.parse_low_power_mode(" lowpowermode 1\n"))
        self.assertFalse(perf.parse_low_power_mode(" powermode 0\n"))
        self.assertIsNone(perf.parse_low_power_mode("nothing here"))

    def test_healthy_machine_passes(self):
        checks = self.checks()
        self.assertFalse([c for c in checks if c["status"] == "fail"])

    def test_each_failure_is_reported(self):
        self.assertEqual(self.status(self.checks(batt=PMSET_BATT_BATTERY), "ac_power"), "fail")
        self.assertEqual(self.status(self.checks(therm=PMSET_THERM_LIMITED), "thermal"), "fail")
        self.assertEqual(self.status(self.checks(general=" lowpowermode 1\n"), "low_power_mode"), "fail")
        self.assertEqual(self.status(self.checks(busy=("cargo",)), "no_builds"), "fail")
        self.assertEqual(self.status(self.checks(busy=("engine",)), "no_other_engine"), "fail")
        self.assertEqual(self.status(self.checks(free=1024**3), "disk"), "fail")

    def test_suffixed_engine_binaries_count_as_other_engines(self):
        seen = []

        def fake_output(command):
            seen.append(command)
            return "123\n" if command == ["pgrep", "^engine\\-"] else ""

        with mock.patch.object(perf, "_command_output", fake_output):
            self.assertEqual(perf.running_processes(["engine"]), [])
            self.assertEqual(perf.running_processes(["engine", "engine-*"]), ["engine-*"])
        self.assertIn(["pgrep", "-x", "engine"], seen)
        self.assertEqual(self.status(self.checks(busy=("engine-*",)), "no_other_engine"), "fail")

    def test_non_macos_skips_the_machine_checks(self):
        checks = self.checks(system="Linux")
        self.assertEqual({c["name"] for c in checks}, {"disk", "macos"})
        self.assertEqual(self.status(checks, "macos"), "skip")


class BuildTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.repo = root / "repo"
        self.repo.mkdir()
        env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"}
        for command in (["git", "init", "-q"], ["git", "commit", "-q", "--allow-empty", "-m", "first"]):
            subprocess.run(command, cwd=self.repo, env=env, check=True, capture_output=True)
        self.sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.repo, capture_output=True, text=True,
                                  check=True).stdout.strip()
        self.worktrees = root / "worktrees"
        self.bins = root / "bins"
        self.calls = []

    def runner(self, command, **kwargs):
        self.calls.append((command, kwargs))
        if command[0] == "cargo":
            binary = Path(kwargs["cwd"], "target", "release", "engine")
            binary.parent.mkdir(parents=True)
            binary.write_text("fake engine")
            return subprocess.CompletedProcess(command, 0, "", "")
        return perf._run(command, **kwargs)

    def build(self, **kwargs):
        options = {"repo": self.repo, "worktree_root": self.worktrees, "bin_dir": self.bins, "runner": self.runner,
                   "free_bytes": 100 * 1024**3}
        options.update(kwargs)
        return perf.build("HEAD", **options)

    def test_existing_binary_is_reused_without_building(self):
        self.bins.mkdir()
        (self.bins / f"engine-{self.sha[:8]}").write_text("already built")
        plan = self.build()
        self.assertTrue(plan["reused"])
        self.assertFalse([c for c, _ in self.calls if c[0] == "cargo"])
        self.assertFalse(self.worktrees.exists())

    def test_dry_run_builds_nothing(self):
        plan = self.build(dry_run=True)
        self.assertFalse(plan["reused"])
        self.assertEqual(plan["sha"], self.sha)
        self.assertFalse(self.worktrees.exists())
        self.assertFalse(self.calls and any(c[0] == "cargo" for c, _ in self.calls))

    def test_low_disk_refuses_before_creating_a_worktree(self):
        with self.assertRaises(perf.HarnessError) as caught:
            self.build(free_bytes=10 * 1024**3)
        self.assertEqual(caught.exception.name, "insufficient_disk")
        self.assertFalse((self.worktrees / f"perf-{self.sha[:8]}").exists())
        self.assertFalse([c for c, _ in self.calls if c[0] == "cargo"])

    def test_build_uses_its_own_target_dir_and_copies_the_binary(self):
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "/shared/target"}):
            plan = self.build(keep_worktree=True)
        cargo = [(c, k) for c, k in self.calls if c[0] == "cargo"]
        self.assertEqual(len(cargo), 1)
        command, kwargs = cargo[0]
        self.assertEqual(command[:3], ["cargo", "build", "--locked"])
        self.assertIn("--release", command)
        self.assertEqual(kwargs["env"]["CARGO_INCREMENTAL"], "0")
        self.assertNotIn("CARGO_TARGET_DIR", kwargs["env"])
        self.assertEqual(kwargs["cwd"], str(self.worktrees / f"perf-{self.sha[:8]}"))
        self.assertEqual(plan["binary"].read_text(), "fake engine")
        self.assertTrue(os.access(plan["binary"], os.X_OK))
        self.assertTrue((self.worktrees / f"perf-{self.sha[:8]}").exists())

    def test_worktree_is_removed_by_default(self):
        self.build()
        self.assertFalse((self.worktrees / f"perf-{self.sha[:8]}").exists())

    def test_unknown_ref_is_a_named_error(self):
        with self.assertRaises(perf.HarnessError) as caught:
            perf.build("no-such-ref", repo=self.repo, worktree_root=self.worktrees, bin_dir=self.bins,
                       runner=self.runner)
        self.assertEqual(caught.exception.name, "unknown_ref")

    def test_failed_build_is_a_named_error(self):
        def failing(command, **kwargs):
            if command[0] == "cargo":
                return subprocess.CompletedProcess(command, 101, "", "error: boom")
            return perf._run(command, **kwargs)

        with self.assertRaises(perf.HarnessError) as caught:
            self.build(runner=failing, keep_worktree=True)
        self.assertEqual(caught.exception.name, "build_failed")
        self.assertFalse((self.bins / f"engine-{self.sha[:8]}").exists())


    def failing_cargo(self, code=101):
        def failing(command, **kwargs):
            if command[0] == "cargo":
                kwargs_dir = Path(kwargs["cwd"], "target")
                kwargs_dir.mkdir()
                (kwargs_dir / "junk").write_text("x")
                return subprocess.CompletedProcess(command, code, "", "error: boom")
            return perf._run(command, **kwargs)
        return failing

    def test_a_failed_build_removes_its_worktree_and_target(self):
        with self.assertRaises(perf.HarnessError) as caught:
            self.build(runner=self.failing_cargo())
        self.assertEqual(caught.exception.name, "build_failed")
        self.assertFalse((self.worktrees / f"perf-{self.sha[:8]}").exists())

    def test_keep_worktree_also_keeps_a_failed_build(self):
        with self.assertRaises(perf.HarnessError):
            self.build(runner=self.failing_cargo(), keep_worktree=True)
        self.assertTrue((self.worktrees / f"perf-{self.sha[:8]}" / "target" / "junk").exists())

    def test_a_missing_binary_after_a_successful_build_still_cleans_up(self):
        def quiet(command, **kwargs):
            if command[0] == "cargo":
                return subprocess.CompletedProcess(command, 0, "", "")
            return perf._run(command, **kwargs)

        with self.assertRaises(perf.HarnessError) as caught:
            self.build(runner=quiet)
        self.assertEqual(caught.exception.name, "binary_missing")
        self.assertFalse((self.worktrees / f"perf-{self.sha[:8]}").exists())


class SummaryTest(unittest.TestCase):
    def test_summary_lists_primary_metrics_and_refusals(self):
        verdict = {
            "base": {"path": "b", "sha256": "a" * 64}, "cand": {"path": "c", "sha256": "a" * 64},
            "pairs": 3, "min_effect": 0.02, "seed": 1, "same_binary": True, "overall": "refused",
            "preflight": [{"name": "ac_power", "status": "fail", "detail": "battery"}],
            "scenarios": {
                "dense": {"status": "refused", "error": "inventory_mismatch", "details": ["mesh_entities: 1 vs 2"]},
                "synthetic": {"status": "ok", "pairs_used": 3, "pairs_requested": 3, "summary_verdict": "noise",
                              "metrics": {"frame_ms_mean": {"verdict": "noise", "base_median": 10.0,
                                                            "cand_median": 10.0, "ratio_median": 1.0,
                                                            "ci_lo": 0.99, "ci_hi": 1.01},
                                          "cpu/x": {"verdict": "noise", "base_median": 1.0, "cand_median": 1.0,
                                                    "ratio_median": 1.0, "ci_lo": 0.9, "ci_hi": 1.1}}},
            },
        }
        text = perf.summary_markdown(verdict)
        self.assertIn("REFUSED `inventory_mismatch`", text)
        self.assertIn("| frame_ms_mean |", text)
        self.assertNotIn("| cpu/x |", text)
        self.assertIn("noise-floor run", text)
        self.assertIn("preflight fail", text)
        self.assertIn("`noise` means not distinguishable from run-to-run variation at 3 pairs, not \"no change\"",
                      text)
        self.assertIn("uncorrected for multiple comparisons", text)

    def test_the_noise_caveat_appears_only_next_to_a_noise_verdict(self):
        def verdict(summary):
            return {"base": {"path": "b", "sha256": "a" * 64}, "cand": {"path": "c", "sha256": "b" * 64},
                    "pairs": 6, "min_effect": 0.02, "seed": 1, "same_binary": False, "overall": summary,
                    "scenarios": {"s": {"status": "ok", "pairs_used": 6, "pairs_requested": 6,
                                        "summary_verdict": summary, "metrics": {}}}}
        self.assertIn("at 6 pairs, not \"no change\"", perf.summary_markdown(verdict("noise")))
        self.assertNotIn("not \"no change\"", perf.summary_markdown(verdict("regressed")))

    def test_the_overall_verdict_depends_on_the_primary_metrics_only(self):
        pairs = [{"base": run_entry(metrics={**frame_metrics(1.0, 0.01 * i), "gpu/x": 1.0}),
                  "cand": run_entry(metrics={**frame_metrics(1.0, 0.01 * i), "gpu/x": 5.0 + i})}
                 for i in range(8)]
        result = perf.analyze_scenario("s", pairs, seed=1, min_effect=0.02, samples=200)
        self.assertEqual(result["metrics"]["gpu/x"]["verdict"], "regressed")
        self.assertEqual(result["summary_verdict"], "noise")


class BuildIdentityTest(unittest.TestCase):
    def test_a_built_binary_name_gives_a_clean_commit(self):
        self.assertEqual(perf.build_identity("/bins/engine-0123abcd"),
                         {"commit": "0123abcd", "dirty": False, "commit_source": "binary name"})

    def test_an_unnamed_binary_has_no_known_commit_or_dirty_state(self):
        info = perf.build_identity("/tmp/engine")
        self.assertIsNone(info["commit"])
        self.assertIsNone(info["dirty"])

    def test_flags_override_the_name_and_record_dirty(self):
        info = perf.build_identity("/bins/engine-0123abcd", "feedface", True)
        self.assertEqual((info["commit"], info["dirty"]), ("feedface", True))

    def verdict(self, base, cand):
        return {"base": {"path": "b", "sha256": "a" * 64, **base}, "cand": {"path": "c", "sha256": "b" * 64, **cand},
                "pairs": 6, "min_effect": 0.02, "seed": 1, "same_binary": False, "overall": "noise",
                "scenarios": {}}

    def test_summary_shows_commits_and_warns_about_a_dirty_side(self):
        clean = {"commit": "aaaa1111", "dirty": False}
        text = perf.summary_markdown(self.verdict(clean, {"commit": "bbbb2222", "dirty": True}))
        self.assertIn("commit aaaa1111, clean", text)
        self.assertIn("commit bbbb2222, dirty", text)
        self.assertIn("WARNING: the candidate binary was built from a dirty working tree", text)
        self.assertNotIn("the base binary", text)

    def test_summary_warns_about_an_unknown_commit(self):
        text = perf.summary_markdown(self.verdict({"commit": None, "dirty": None},
                                                  {"commit": "bbbb2222", "dirty": False}))
        self.assertIn("WARNING: the base binary's commit is unknown", text)

    def test_two_clean_known_sides_have_no_warning(self):
        clean = {"commit": "aaaa1111", "dirty": False}
        self.assertNotIn("WARNING", perf.summary_markdown(self.verdict(clean, clean)))

    def test_compare_records_the_identity_in_the_verdict(self):
        with tempfile.TemporaryDirectory() as tmp:
            engine = make_fake_engine(tmp)
            named = Path(tmp, "engine-0123abcd")
            shutil.copy2(engine, named)
            out = Path(tmp, "out")
            args = perf.build_parser().parse_args(
                ["compare", "--base", str(named), "--cand", str(engine), "--scenarios", "synthetic",
                 "--out", str(out), "--pairs", "1", "--cooldown", "0", "--cand-dirty", "--cand-commit", "cafe1234",
                 "--force"])
            with mock.patch.object(perf, "load_scenarios",
                                   return_value={"synthetic": {"needs_assets": False, "args": ["--benchmark-only"]}}), \
                 mock.patch.object(perf, "preflight_checks", return_value=[]), \
                 mock.patch.object(perf, "resolve_window", return_value="visible"), \
                 mock.patch.object(perf, "run_scenario", side_effect=self.fake_run), \
                 mock.patch.object(perf, "load_run", return_value=run_entry()), \
                 mock.patch.object(perf.sys, "stdout", io.StringIO()):
                perf.compare(args)
            verdict = json.loads((out / "verdict.json").read_text())
            summary = (out / "summary.md").read_text()
        self.assertEqual((verdict["base"]["commit"], verdict["base"]["dirty"]), ("0123abcd", False))
        self.assertEqual((verdict["cand"]["commit"], verdict["cand"]["dirty"]), ("cafe1234", True))
        self.assertIn("WARNING: the candidate binary was built from a dirty working tree", summary)

    @staticmethod
    def fake_run(*args, **kwargs):
        return {"classification": "ok", "wall_seconds": 0.0}


class WindowModeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.engine = make_fake_engine(self.tmp.name)
        self.cache = Path(self.tmp.name) / "flags.json"
        perf._PROBE_MEMORY.clear()
        self.addCleanup(perf._PROBE_MEMORY.clear)

    def test_probe_reads_the_flag_from_help_and_caches_by_sha(self):
        calls = []

        def help_text(engine):
            calls.append(engine)
            return "Options:\n  --hidden-window\n  --background-window   place under other windows\n"

        self.assertTrue(perf.supports_background_window(self.engine, help_text=help_text, cache_path=self.cache))
        self.assertTrue(perf.supports_background_window(self.engine, help_text=help_text, cache_path=self.cache))
        perf._PROBE_MEMORY.clear()
        self.assertTrue(perf.supports_background_window(self.engine, help_text=help_text, cache_path=self.cache))
        self.assertEqual(len(calls), 1)
        self.assertIn(perf.sha256_file(self.engine), json.loads(self.cache.read_text()))

    def test_probe_does_not_match_a_longer_flag(self):
        found = perf.supports_background_window(self.engine, cache_path=self.cache,
                                                help_text=lambda e: "--background-window-color <C>\n")
        self.assertFalse(found)

    def test_a_changed_binary_is_probed_again(self):
        calls = []
        text = lambda e: calls.append(1) or "--background-window"
        perf.supports_background_window(self.engine, help_text=text, cache_path=self.cache)
        self.engine.write_text(self.engine.read_text() + "\n# changed\n", encoding="utf-8")
        perf.supports_background_window(self.engine, help_text=text, cache_path=self.cache)
        self.assertEqual(len(calls), 2)

    def test_auto_prefers_background_only_when_every_engine_lists_it(self):
        resolve = perf.resolve_window
        self.assertEqual(resolve("auto", ["a", "b"], supports=lambda e: True), "background")
        self.assertEqual(resolve("auto", ["a", "b"], supports=lambda e: e == "a"), "visible")
        self.assertEqual(resolve("visible", ["a"], supports=lambda e: True), "visible")
        self.assertEqual(resolve("hidden", ["a"], supports=lambda e: True), "hidden")

    def test_explicit_background_needs_the_flag(self):
        with self.assertRaises(perf.HarnessError) as caught:
            perf.resolve_window("background", ["old-engine"], supports=lambda e: False)
        self.assertEqual(caught.exception.name, "background_window_unsupported")
        with self.assertRaises(perf.HarnessError) as caught:
            perf.resolve_window("sideways", ["a"], supports=lambda e: True)
        self.assertEqual(caught.exception.name, "bad_window_mode")

    def test_shots_never_use_hidden_on_macos(self):
        with self.assertRaises(perf.HarnessError) as caught:
            perf.resolve_window("hidden", ["a"], system="Darwin", for_shots=True)
        self.assertEqual(caught.exception.name, "hidden_window_black_frames")
        self.assertEqual(perf.resolve_window("hidden", ["a"], system="Linux", for_shots=True), "hidden")
        with mock.patch.object(perf.platform, "system", return_value="Darwin"):
            with self.assertRaises(perf.HarnessError):
                perf.capture_shots("e", "s.json", "assets", Path(self.tmp.name) / "shots", window="hidden")
            with mock.patch("sys.stderr", new_callable=io.StringIO) as stderr:
                code = perf.main(["shots", "--engine", "e", "--shots", "s", "--assets", "a", "--out", "o",
                                  "--window", "hidden"])
            self.assertEqual(code, 2)
            self.assertIn("hidden_window_black_frames", stderr.getvalue())

    def test_engine_flag_per_mode(self):
        scenario = {"needs_assets": False, "args": []}
        flags = {}
        for mode in ("hidden", "background", "visible"):
            args = perf.build_engine_args("synthetic", scenario, assets=None, out_dir="/o", duration=5,
                                          warmup_frames=None, window=mode, commit="c", run_id="r",
                                          frame_budget_ms=6.25)
            flags[mode] = {a for a in args if a in ("--hidden-window", "--background-window")}
        self.assertEqual(flags, {"hidden": {"--hidden-window"}, "background": {"--background-window"},
                                 "visible": set()})
        with self.assertRaises(perf.HarnessError):
            perf.build_engine_args("synthetic", scenario, assets=None, out_dir="/o", duration=5,
                                   warmup_frames=None, window="auto", commit="c", run_id="r", frame_budget_ms=6.25)

    def test_visible_is_an_alias_and_auto_is_the_default(self):
        parse = perf.build_parser().parse_args
        base = ["run", "--engine", "e", "--scenario", "s", "--out", "o"]
        self.assertEqual(parse(base).window, "auto")
        self.assertEqual(parse(base + ["--visible"]).window, "visible")
        self.assertEqual(parse(base + ["--window", "background"]).window, "background")
        self.assertEqual(parse(["compare", "--base", "a", "--cand", "b", "--scenarios", "s", "--out", "o",
                                "--visible"]).window, "visible")

    def test_run_records_the_mode_and_warns_about_hidden_on_macos(self):
        scenario = {"needs_assets": False, "args": []}
        with mock.patch.dict(os.environ, {"FAKE_MODE": "ok"}), \
                mock.patch.object(perf.platform, "system", return_value="Darwin"):
            run = perf.run_scenario(self.engine, "synthetic", scenario, assets=None,
                                    out_dir=Path(self.tmp.name) / "hidden", duration=1, timeout=10, grace=1,
                                    poll=0.05, window="hidden")
            visible = perf.run_scenario(self.engine, "synthetic", scenario, assets=None,
                                        out_dir=Path(self.tmp.name) / "visible", duration=1, timeout=10,
                                        grace=1, poll=0.05, window="background")
        self.assertEqual(run["window_mode"], "hidden")
        self.assertIn("--hidden-window", run["command"])
        self.assertTrue(any("never acquires a drawable" in w for w in run["warnings"]))
        self.assertEqual(visible["window_mode"], "background")
        self.assertIn("--background-window", visible["command"])
        self.assertFalse(any("drawable" in w for w in visible["warnings"]))
        stored = json.loads((Path(self.tmp.name) / "hidden" / "run.json").read_text())
        self.assertEqual(stored["window_mode"], "hidden")

    def test_compare_refuses_runs_in_different_window_modes(self):
        def pair(base_mode, cand_mode):
            base, cand = run_entry(), run_entry()
            base["run"]["window_mode"], cand["run"]["window_mode"] = base_mode, cand_mode
            return {"base": base, "cand": cand}

        result = perf.analyze_scenario("s", [pair("visible", "hidden")] * 3, seed=1, min_effect=0.02)
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["error"], "window_mode_mismatch")
        ok = perf.analyze_scenario("s", [pair("visible", "visible")] * 3, seed=1, min_effect=0.02)
        self.assertEqual(ok["status"], "ok")

    def test_runs_without_a_recorded_mode_read_as_hidden_or_visible(self):
        self.assertEqual(perf.window_mode_of({"hidden_window": True}), "hidden")
        self.assertEqual(perf.window_mode_of({"hidden_window": False}), "visible")
        self.assertEqual(perf.window_mode_of({"window_mode": "background", "hidden_window": False}), "background")

    def test_summary_warns_about_hidden_on_macos_only(self):
        verdict = {"base": {"path": "b", "sha256": "a" * 64}, "cand": {"path": "c", "sha256": "b" * 64},
                   "pairs": 6, "min_effect": 0.02, "seed": 1, "same_binary": False, "overall": "noise",
                   "scenarios": {}, "window_mode": "hidden", "platform": "Darwin"}
        self.assertIn("never acquires a drawable", perf.summary_markdown(verdict))
        verdict["platform"] = "Linux"
        self.assertNotIn("never acquires a drawable", perf.summary_markdown(verdict))
        verdict.update(platform="Darwin", window_mode="background")
        text = perf.summary_markdown(verdict)
        self.assertNotIn("never acquires a drawable", text)
        self.assertIn("window mode: background", text)


class ProfilerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_samply_command_wraps_the_engine(self):
        plan = perf.plan_profiler("samply", ["/bin/engine", "--x", "1"], self.root / "run", samply="/opt/samply")
        out = (self.root / "run").resolve()
        self.assertEqual(plan["launch_command"], [
            "/opt/samply", "record", "--save-only", "--unstable-presymbolicate", "--rate", "1000",
            "--output", str(out / "cpu-profile.json.gz"), "--", "/bin/engine", "--x", "1"])
        self.assertEqual(plan["env"], {})
        self.assertTrue(plan["outputs"]["symbols"].endswith("cpu-profile.json.syms.json"))

    def test_metal_trace_sets_the_observer_environment(self):
        plan = perf.plan_profiler("metal-trace", ["/bin/engine"], self.root / "run", trace_start=900)
        out = (self.root / "run").resolve()
        self.assertEqual(plan["launch_command"], ["/bin/engine"])
        self.assertEqual(plan["env"]["MUDCRAB_METAL_TRACE_FILE"], str(out / "native.ndjson"))
        self.assertTrue(os.path.isabs(plan["env"]["MUDCRAB_METAL_TRACE_FILE"]))
        self.assertEqual(plan["env"]["MUDCRAB_METAL_TRACE_START_ACQUISITION"], "900")
        self.assertEqual(plan["env"]["DYLD_INSERT_LIBRARIES"], str(out / "libmudcrab_metal_trace.dylib"))
        default = perf.plan_profiler("metal-trace", ["/bin/engine"], self.root / "run")
        self.assertEqual(default["env"]["MUDCRAB_METAL_TRACE_START_ACQUISITION"], "600")

    def test_none_leaves_the_command_alone(self):
        plan = perf.plan_profiler("none", ["/bin/engine"], self.root)
        self.assertEqual((plan["launch_command"], plan["env"], plan["outputs"]), (["/bin/engine"], {}, {}))
        with self.assertRaises(perf.HarnessError):
            perf.plan_profiler("perf", ["/bin/engine"], self.root)

    def test_dry_run_shows_the_wrapped_command_and_environment(self):
        scenario = {"needs_assets": False, "args": []}
        engine = make_fake_engine(self.root)
        run = perf.run_scenario(engine, "synthetic", scenario, assets=None, out_dir=self.root / "r", dry_run=True,
                                profiler="samply", samply="/opt/samply")
        self.assertEqual(run["command"][:2], ["/opt/samply", "record"])
        self.assertIn(str(engine), run["command"])
        run = perf.run_scenario(engine, "synthetic", scenario, assets=None, out_dir=self.root / "r", dry_run=True,
                                profiler="metal-trace", trace_start=7)
        self.assertEqual(run["env"]["MUDCRAB_METAL_TRACE_START_ACQUISITION"], "7")

    def test_dylib_is_compiled_once_per_source_hash(self):
        source = self.root / "metal_trace.m"
        source.write_text("// v1", encoding="utf-8")
        commands = []

        def runner(command):
            commands.append(command)
            Path(command[command.index("-o") + 1]).write_bytes(b"dylib")
            return subprocess.CompletedProcess(command, 0, "", "")

        first = self.root / "run1"
        first.mkdir()
        copy = perf.metal_trace_dylib(first, source=source, cache_root=self.root / "cache", runner=runner)
        self.assertEqual(copy.read_bytes(), b"dylib")
        self.assertEqual(copy.parent, first)
        command = commands[0]
        for flag in ("-fobjc-arc", "-O2", "-Werror", "-dynamiclib", "AppKit", "QuartzCore", "Metal", "Foundation"):
            self.assertIn(flag, command)
        second = self.root / "run2"
        second.mkdir()
        perf.metal_trace_dylib(second, source=source, cache_root=self.root / "cache", runner=runner)
        self.assertEqual(len(commands), 1)
        source.write_text("// v2", encoding="utf-8")
        perf.metal_trace_dylib(second, source=source, cache_root=self.root / "cache", runner=runner)
        self.assertEqual(len(commands), 2)

    def test_compile_failure_is_a_named_error(self):
        source = self.root / "metal_trace.m"
        source.write_text("x", encoding="utf-8")
        failing = lambda command: subprocess.CompletedProcess(command, 1, "", "error: boom")
        with self.assertRaises(perf.HarnessError) as caught:
            perf.metal_trace_dylib(self.root, source=source, cache_root=self.root / "c", runner=failing)
        self.assertEqual(caught.exception.name, "profiler_build_failed")

    def test_the_engine_is_found_below_samply(self):
        table = [(1, 0, "/sbin/launchd"), (100, 1, "/opt/samply record --output x -- /bin/engine --a"),
                 (101, 100, "/bin/engine --a"), (102, 101, "/bin/helper"), (300, 1, "/bin/engine --other")]
        self.assertEqual(perf.find_engine_pid(100, "/bin/engine", table), 101)
        self.assertIsNone(perf.find_engine_pid(100, "/bin/missing", table))
        scripted = [(100, 1, "samply"), (101, 100, "/usr/bin/python3 /tmp/fake-engine --a")]
        self.assertEqual(perf.find_engine_pid(100, "/tmp/fake-engine", scripted), 101)

    def _terminate(self, table, alive_after_term=False, proc_exits_after=("signal",)):
        events = []
        alive = {101: True}
        proc = mock.Mock(pid=100)
        state = {"exited": False}
        proc.poll.side_effect = lambda: 0 if state["exited"] else None

        def send_signal(pid, sig):
            events.append((pid, sig))
            if pid == 101 and (sig == perf.signal.SIGKILL or not alive_after_term):
                alive[101] = False
                state["exited"] = True
            if pid == 100 and sig == perf.signal.SIGINT:
                state["exited"] = True

        result = perf.terminate_engine_under_samply(
            proc, "/bin/engine", grace=0.0, finalize_timeout=0.5, processes=lambda: table, send_signal=send_signal,
            alive=lambda pid: alive.get(pid, False), sleep=lambda s: None, clock=iter(range(0, 10000)).__next__)
        return result, events

    def test_only_the_engine_is_signalled_never_samply(self):
        table = [(100, 1, "/opt/samply record -- /bin/engine"), (101, 100, "/bin/engine --a")]
        result, events = self._terminate(table)
        self.assertTrue(result)
        self.assertEqual(events, [(101, perf.signal.SIGTERM)])

    def test_a_stuck_engine_gets_sigkill_and_still_not_samply(self):
        table = [(100, 1, "/opt/samply record -- /bin/engine"), (101, 100, "/bin/engine --a")]
        result, events = self._terminate(table, alive_after_term=True)
        self.assertTrue(result)
        self.assertEqual(events, [(101, perf.signal.SIGTERM), (101, perf.signal.SIGKILL)])

    def test_without_an_engine_samply_is_interrupted_so_it_saves(self):
        result, events = self._terminate([(100, 1, "/opt/samply record -- /bin/engine")])
        self.assertTrue(result)
        self.assertEqual(events, [(100, perf.signal.SIGINT)])

    def test_an_exited_engine_means_samply_was_only_saving(self):
        proc = mock.Mock(pid=100)
        polls = iter([None, None, 0, 0, 0])
        proc.poll.side_effect = lambda: next(polls)
        signalled = []
        result = perf.terminate_engine_under_samply(
            proc, "/bin/engine", processes=lambda: [(100, 1, "/opt/samply record -- /bin/engine")],
            send_signal=lambda pid, sig: signalled.append((pid, sig)), alive=lambda pid: False,
            sleep=lambda s: None, finalize_timeout=60.0, clock=iter(range(0, 10000)).__next__)
        self.assertTrue(result)  # no engine below samply and samply still running: it is interrupted
        proc2 = mock.Mock(pid=100)
        proc2.poll.side_effect = lambda: 0
        self.assertFalse(perf.terminate_engine_under_samply(
            proc2, "/bin/engine", processes=lambda: [], send_signal=lambda *a: self.fail("signalled"),
            alive=lambda pid: False, sleep=lambda s: None))

    @unittest.skipIf(sys.platform == "win32", "the fixtures are shebang scripts, which Windows cannot execute")
    def test_watchdog_does_not_call_a_still_saving_profiler_a_hang(self):
        script = self.root / "slow"
        script.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(1.5)\n", encoding="utf-8")
        script.chmod(0o755)
        watch = perf.watch_process([str(script)], stdout_path=self.root / "o", stderr_path=self.root / "e",
                                   is_done=lambda: True, timeout=30, grace=0.2, poll=0.05,
                                   terminate=lambda proc: (proc.wait(), False)[1])
        self.assertFalse(watch["killed"])
        self.assertEqual(watch["exit_code"], 0)
        self.assertEqual(perf.classify(watch), "ok")

    @unittest.skipUnless(shutil.which("ps"), "needs ps")
    @unittest.skipIf(sys.platform == "win32", "the fixtures are shebang scripts, which Windows cannot execute")
    def test_hung_engine_under_a_fake_samply_is_killed_and_the_profile_is_still_written(self):
        engine = make_fake_engine(self.root)
        fake_samply = self.root / "samply"
        fake_samply.write_text(textwrap.dedent(f"""\
            #!{sys.executable}
            import subprocess, sys, time
            args = sys.argv[1:]
            output = args[args.index("--output") + 1]
            child = subprocess.Popen(args[args.index("--") + 1:])
            child.wait()
            time.sleep(0.5)
            open(output, "wb").write(b"profile")
            open(output[:-3] + ".syms.json", "w").write("{{}}")
            """), encoding="utf-8")
        fake_samply.chmod(0o755)
        scenario = {"needs_assets": False, "args": []}
        with mock.patch.dict(os.environ, {"FAKE_MODE": "hang"}):
            run = perf.run_scenario(engine, "synthetic", scenario, assets=None, out_dir=self.root / "out",
                                    duration=1, timeout=30, grace=1, poll=0.05, window="visible",
                                    profiler="samply", samply=str(fake_samply), finalize_timeout=20)
        self.assertEqual(run["classification"], "hung_after_report")
        self.assertTrue(run["valid"])
        self.assertTrue(run["killed"])
        outputs = run["profiler"]["outputs"]
        self.assertTrue(outputs["profile"]["present"], "samply was killed before it saved its profile")
        self.assertTrue(outputs["symbols"]["present"])
        self.assertTrue(any("profiled with samply" in w for w in run["warnings"]))

    def test_a_missing_samply_is_a_named_error(self):
        with mock.patch.object(perf.shutil, "which", return_value=None), \
                mock.patch.object(perf.Path, "home", return_value=self.root):
            with self.assertRaises(perf.HarnessError) as caught:
                perf.find_samply()
        self.assertEqual(caught.exception.name, "profiler_unavailable")


def make_bundle(directory, *, scenario="dense", window="hidden", frame_ms=10.0, spans=None, frames=1000,
                relative_report=False, passes=None, fingerprint=None):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    span = lambda count, mean: {"count": count, "total": count * mean, "mean": mean, "p50": mean, "p95": mean * 1.2,
                                "p99": mean * 1.5, "worst": mean * 3, "minimum": mean * 0.5}
    default = {
        "render_world/main_world": span(frames, 3.0), "render_world/extract": span(frames, 0.5),
        "render_world/wait_for_render_thread": span(frames, 6.5), "render_world/render_thread": span(frames, 9.5),
        "render_world/render/prepare": span(frames, 4.0), "render_world/render/graph_and_present": span(frames, 4.5),
        "lod/db_request_total": span(3, 300.0), "assets/model_ready": span(frames - 100, 350.0),
        "assets_added/mesh": span(frames, 20.0), "streaming/frame_commit": span(40, 0.9),
    }
    default.update(spans or {})
    report = {"frames": frames, "warmup_frames": 300, "elapsed_seconds": frames * frame_ms / 1000,
              "average_fps": 1000 / frame_ms, "frame_ms_mean": frame_ms, "frame_ms_p50": frame_ms,
              "frame_ms_p95": frame_ms * 1.2, "frame_ms_p99": frame_ms * 1.5, "frame_ms_worst": frame_ms * 4,
              "entity_count": 500, "world_ready_reached": True, "frames_to_world_ready": 80,
              "scene": {"mesh_entities": 100, "terrain_patches": 4, "water_surfaces": 1},
              "system": {"os": "macOS 26.2"}, "fingerprint": fingerprint or {"resolution": [100, 100], "commit": "x"},
              "gpu_passes": {"passes": passes or {}},
              "cpu": {"schedules": {"main_schedule/PostUpdate": span(frames + 300, 2.0)}, "spans": default}}
    (directory / "report.json").write_text(json.dumps(report), encoding="utf-8")
    run = {"scenario": scenario, "classification": "ok", "valid": True, "window_mode": window,
           "binary_sha256": "ab" * 32, "warnings": [],
           "report": "relative/elsewhere/report.json" if relative_report else str(directory / "report.json")}
    (directory / "run.json").write_text(json.dumps(run), encoding="utf-8")
    return directory


class SummarizeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_spans_split_into_per_frame_and_asynchronous(self):
        run = make_bundle(self.root / "a")
        summary = perf.summarize_run(run)
        per_frame = [r["name"] for r in summary["per_frame_spans"]]
        asynchronous = [r["name"] for r in summary["asynchronous_latencies"]]
        self.assertEqual(per_frame[0], "render_world/render_thread")
        self.assertIn("main_schedule/PostUpdate", per_frame)
        self.assertIn("lod/db_request_total", asynchronous)
        self.assertIn("streaming/frame_commit", asynchronous)
        # a latency far above the frame time is not a per-frame cost even when its count is high
        self.assertIn("assets/model_ready", asynchronous)
        self.assertNotIn("assets/model_ready", per_frame)
        self.assertEqual([r["name"] for r in summary["counters"]], ["assets_added/mesh"])
        means = {r["name"]: r["mean_ms"] for r in summary["asynchronous_latencies"]}
        self.assertEqual(asynchronous, sorted(asynchronous, key=lambda n: -means[n]))
        self.assertEqual(sorted(r["mean_ms"] for r in summary["per_frame_spans"])[::-1],
                         [r["mean_ms"] for r in summary["per_frame_spans"]])

    def test_the_report_path_in_run_json_may_be_relative_to_somewhere_else(self):
        run = make_bundle(self.root / "a", relative_report=True)
        self.assertEqual(perf.summarize_run(run)["frames"]["frames"], 1000)

    def test_missing_report_is_a_named_error(self):
        (self.root / "empty").mkdir()
        (self.root / "empty" / "run.json").write_text("{}", encoding="utf-8")
        with self.assertRaises(perf.HarnessError) as caught:
            perf.summarize_run(self.root / "empty")
        self.assertEqual(caught.exception.name, "report_missing")

    def test_bound_classification(self):
        budget = lambda frame, main, wait, render: perf.thread_budget({
            "frame_ms_mean": frame, "cpu": {"spans": {
                "render_world/main_world": {"mean": main}, "render_world/extract": {"mean": 0.2},
                "render_world/wait_for_render_thread": {"mean": wait}, "render_world/render_thread": {"mean": render}}}})
        self.assertEqual(perf.classify_bound(budget(10, 3, 6.5, 9.7))["class"], "render-thread-bound")
        self.assertEqual(perf.classify_bound(budget(10, 9.5, 0.2, 3))["class"], "main-thread-bound")
        self.assertEqual(perf.classify_bound(budget(10, 4, 3, 5))["class"], "unclear")
        gpu = {"busy_fraction": 0.96, "gpu_ms_per_frame": 14.6, "acquisitions": 900}
        bound = perf.classify_bound(budget(15.2, 3, 12, 14.5), gpu)
        self.assertEqual(bound["class"], "gpu-bound")
        self.assertIn("96%", bound["evidence"][0])
        self.assertIn("14.6 ms of 15.2 ms", bound["evidence"][0])
        low = perf.classify_bound(budget(10, 4, 3, 5), {"busy_fraction": 0.6, "acquisitions": 10})
        self.assertEqual(low["class"], "unclear")
        self.assertIn("GPU busy 60%", low["evidence"][-1])
        self.assertEqual(perf.classify_bound(perf.thread_budget({"frame_ms_mean": 10.0}))["class"], "unclear")

    def test_render_thread_bound_without_a_trace_points_at_gpu_busy(self):
        text = perf.format_summary(perf.summarize_run(make_bundle(self.root / "a")))
        self.assertIn("bound: render-thread-bound", text)
        self.assertIn("gpu-busy", text)
        self.assertIn("asynchronous latencies", text)
        self.assertIn("never acquires a drawable", text)

    def test_zero_pass_timings_and_late_world_ready_are_warned(self):
        run = make_bundle(self.root / "a", passes={"render/x/elapsed_gpu": {"mean": 0.0}})
        report = json.loads((run / "report.json").read_text())
        report["frames_to_world_ready"] = 500
        (run / "report.json").write_text(json.dumps(report))
        warnings = perf.summarize_run(run)["warnings"]
        self.assertTrue(any("GPU pass timing is zero" in w for w in warnings))
        self.assertTrue(any("world ready at frame 500" in w for w in warnings))

    def test_against_reports_changes_and_conditions(self):
        a = make_bundle(self.root / "a", window="visible", frame_ms=15.0,
                        fingerprint={"resolution": [3024, 1800], "commit": "x"})
        b = make_bundle(self.root / "b", window="hidden", frame_ms=10.0,
                        fingerprint={"resolution": [3200, 1800], "commit": "y"})
        comparison = perf.compare_summaries(perf.summarize_run(a), perf.summarize_run(b))
        mean = next(r for r in comparison["rows"] if r["metric"] == "frame_ms_mean")
        self.assertEqual((mean["against"], mean["this"]), (15.0, 10.0))
        self.assertAlmostEqual(mean["percent"], -100 / 3, places=3)
        self.assertTrue(any("window modes differ" in n for n in comparison["notes"]))
        self.assertTrue(any("resolution" in n for n in comparison["notes"]))
        self.assertFalse(any("commit" in n for n in comparison["notes"]))

    def test_command_writes_json_and_prints_text(self):
        a = make_bundle(self.root / "a")
        b = make_bundle(self.root / "b", frame_ms=12.0)
        out = self.root / "out.json"
        with mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
            code = perf.main(["summarize", str(a), "--against", str(b), "--json", str(out)])
        self.assertEqual(code, 0)
        self.assertIn("changes:", stdout.getvalue())
        data = json.loads(out.read_text())
        self.assertEqual(data["summary"]["bound"]["class"], "render-thread-bound")
        self.assertIn("rows", data["comparison"])


def write_trace(path, rows, tail=b""):
    with open(path, "wb") as handle:
        for row in rows:
            handle.write(json.dumps(row).encode() + b"\n")
        handle.write(tail)


def completed(start, end, valid=True):
    return {"type": "completed", "cb_id": 1, "host_ns": int(end * 1e9), "gpu_start_s": start, "gpu_end_s": end,
            "gpu_time_valid": valid}


class GpuBusyTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "native.ndjson"

    def rows(self, acquisitions=True, extra_health=None):
        rows = [{"type": "trace_begin", "max_rows": 1000000}, {"type": "hook_installed", "name": "x"}]
        for i in range(200):  # 0.05 s busy every 0.1 s: half of the wall time
            rows.append(completed(100 + i * 0.1, 100 + i * 0.1 + 0.05))
            if i % 3 == 0:  # an overlapping command buffer must not count twice
                rows.append(completed(100 + i * 0.1 + 0.01, 100 + i * 0.1 + 0.04))
        rows.append(completed(105.0, 105.0))  # empty command buffers complete with start == end
        rows.append(completed(0.0, 0.0, valid=False))
        if acquisitions:
            for i in range(20):
                t = 105 + i * 0.5
                rows.append({"type": "acquire_end", "host_ns": int(t * 1e9), "end_ns": int((t + 0.001) * 1e9)})
                rows.append({"type": "presented", "host_ns": int((t + 0.01) * 1e9)})
                rows.append({"type": "present_encode_begin", "host_ns": int(t * 1e9)})
        health = {"type": "health", "rows_attempted": 500, "dropped": 0, "exceptions": 0, "hook_failures": 0,
                  "thread_cpu_clock_failures": 0, "enabled": True}
        health.update(extra_health or {})
        rows.append(health)
        return rows

    def test_busy_fraction_is_the_union_of_intervals_cropped_at_both_ends(self):
        write_trace(self.path, self.rows())
        result = perf.analyze_metal_trace(self.path, fps=50.0, crop=1.0)
        self.assertAlmostEqual(result["busy_fraction"], 0.5, delta=0.01)
        self.assertAlmostEqual(result["window"]["seconds"], 17.95, places=2)
        self.assertAlmostEqual(result["gpu_ms_per_frame"], 10.0, delta=0.25)
        self.assertAlmostEqual(result["frame_ms"], 20.0)
        cb = result["command_buffers"]
        self.assertEqual(cb["invalid"], 1)
        self.assertAlmostEqual(cb["per_frame"], cb["in_window"] / (17.95 * 50), places=6)
        self.assertGreater(cb["in_window"], 180)
        self.assertEqual(result["acquisitions"], 20)
        self.assertEqual(result["presents"], 20)
        self.assertEqual(result["present_encodes"], 20)
        self.assertEqual(result["acquisitions_in_window"], 20)
        self.assertFalse(result["never_acquired_drawable"])
        self.assertEqual(result["warnings"], [])

    def test_crop_changes_the_window_not_the_ratio(self):
        write_trace(self.path, self.rows())
        wide = perf.analyze_metal_trace(self.path, fps=50.0, crop=0.0)
        narrow = perf.analyze_metal_trace(self.path, fps=50.0, crop=5.0)
        self.assertGreater(wide["window"]["seconds"], narrow["window"]["seconds"])
        self.assertAlmostEqual(narrow["busy_fraction"], 0.5, delta=0.02)

    def test_without_a_report_frames_come_from_presents(self):
        write_trace(self.path, self.rows())
        result = perf.analyze_metal_trace(self.path)
        self.assertEqual(result["frames_source"], "presents in the window")
        self.assertIsNone(result["frame_ms"])
        self.assertIsNotNone(result["gpu_ms_per_frame"])

    def test_no_acquisitions_is_said_plainly(self):
        write_trace(self.path, self.rows(acquisitions=False))
        result = perf.analyze_metal_trace(self.path, fps=50.0)
        self.assertTrue(result["never_acquired_drawable"])
        self.assertEqual(result["acquisitions"], 0)
        text = perf.format_gpu_busy(result)
        self.assertIn("this run never acquired a drawable (hidden window?): timings exclude presentation", text)

    def test_dropped_events_are_reported_not_refused(self):
        write_trace(self.path, self.rows(extra_health={"dropped": 18, "rows_attempted": 1000001}))
        result = perf.analyze_metal_trace(self.path, fps=50.0)
        self.assertEqual(result["health"]["loss_max"]["dropped"], 18)
        self.assertTrue(result["health"]["row_cap_reached"])
        text = perf.format_gpu_busy(result)
        self.assertIn("dropped 18", text)
        self.assertIn("row cap", text)
        self.assertAlmostEqual(result["busy_fraction"], 0.5, delta=0.01)

    def test_malformed_and_truncated_lines_are_skipped(self):
        write_trace(self.path, self.rows(), tail=b'not json at all\n{"type":"completed","cb_id":9,"gpu_start')
        result = perf.analyze_metal_trace(self.path, fps=50.0)
        self.assertEqual(result["bad_lines"], 2)
        self.assertAlmostEqual(result["busy_fraction"], 0.5, delta=0.01)

    def test_trace_shorter_than_the_crop_is_not_cropped(self):
        write_trace(self.path, [completed(10.0, 10.5), completed(10.6, 11.0)])
        result = perf.analyze_metal_trace(self.path, fps=10.0, crop=1.0)
        self.assertEqual(result["window"]["crop_seconds"], 0.0)
        self.assertTrue(any("too short" in w for w in result["warnings"]))
        self.assertAlmostEqual(result["busy_fraction"], 0.9 / 1.0, places=6)

    def test_empty_trace_has_no_intervals(self):
        write_trace(self.path, [{"type": "trace_begin", "max_rows": 10}])
        result = perf.analyze_metal_trace(self.path)
        self.assertIsNone(result["busy_fraction"])
        self.assertIn("nothing to report", perf.format_gpu_busy(result))

    def test_command_uses_the_report_fps_and_writes_json(self):
        write_trace(self.path, self.rows())
        report = Path(self.tmp.name) / "report.json"
        report.write_text(json.dumps({"average_fps": 50.0}), encoding="utf-8")
        out = Path(self.tmp.name) / "gpu.json"
        with mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
            code = perf.main(["gpu-busy", str(self.path), "--report", str(report), "--json", str(out)])
        self.assertEqual(code, 0)
        self.assertIn("gpu busy: 50.", stdout.getvalue())
        self.assertAlmostEqual(json.loads(out.read_text())["gpu_ms_per_frame"], 10.0, delta=0.25)

    def test_summarize_adds_the_trace_to_the_bound(self):
        bundle = make_bundle(Path(self.tmp.name) / "run", frame_ms=20.0)
        rows = self.rows()
        # make the GPU busy for nearly the whole window
        rows = [r for r in rows if r["type"] != "completed"] + [completed(100.0, 120.0)]
        write_trace(self.path, rows)
        with mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
            perf.main(["summarize", str(bundle), "--gpu-trace", str(self.path)])
        self.assertIn("bound: gpu-bound", stdout.getvalue())


def make_thread(name, samples, *, is_main=False, tid="1", start_ms=100.0):
    """A thread in Samply's processed format. samples: (time_ms, [leaf address, ..., root address], cpu_us)."""
    addresses = sorted({a for _, stack, _ in samples for a in stack})
    frame_of = {a: i for i, a in enumerate(addresses)}
    strings = [f"0x{a:x}" for a in addresses]
    stack_ids, table_frame, table_prefix = {}, [], []
    sample_stack = []
    for _, stack, _ in samples:
        prefix = None
        for address in reversed(stack):
            key = (prefix, address)
            if key not in stack_ids:
                stack_ids[key] = len(table_frame)
                table_frame.append(frame_of[address])
                table_prefix.append(prefix)
            prefix = stack_ids[key]
        sample_stack.append(prefix)
    n = len(addresses)
    return {
        "name": name, "tid": tid, "isMainThread": is_main, "processStartupTime": start_ms,
        "frameTable": {"length": n, "address": list(addresses), "func": list(range(n))},
        "funcTable": {"length": n, "name": list(range(n)), "resource": [0] * n},
        "resourceTable": {"length": 1, "lib": [0]},
        "stackTable": {"length": len(table_frame), "frame": table_frame, "prefix": table_prefix},
        "stringArray": strings,
        "samples": {"length": len(samples), "stack": sample_stack, "time": [t for t, _, _ in samples],
                    "weight": [5] * len(samples), "threadCPUDelta": [c for _, _, c in samples]},
    }


SYMBOLS = {
    0x10: "bevy_render::render_system", 0x20: "wgpu_core::queue_submit", 0x30: "__psynch_cvwait",
    0x40: "bevy_render::pipelined_rendering::run", 0x50: "main_loop", 0x60: "compute_work",
}


def write_samply(directory, threads, *, unit="µs", name="cpu-profile"):
    directory = Path(directory)
    profile = {"meta": {"sampleUnits": {"threadCPUDelta": unit, "time": "ms"}},
               "libs": [{"name": "engine", "debugName": "engine", "breakpadId": "0123456789ABCDEF0123456789ABCDEF0"}],
               "threads": threads}
    strings = ["UNKNOWN"] + list(SYMBOLS.values())
    table = [{"rva": a, "size": 16, "symbol": i + 1} for i, a in enumerate(SYMBOLS)]
    syms = {"string_table": strings,
            "data": [{"debug_name": "engine", "debug_id": "01234567-89ab-cdef-0123-456789abcdef",
                      "symbol_table": table, "known_addresses": [[a, i] for i, a in enumerate(SYMBOLS)]}]}
    path = directory / f"{name}.json.gz"
    path.write_bytes(gzip.compress(json.dumps(profile).encode()))
    (directory / f"{name}.json.syms.json").write_text(json.dumps(syms), encoding="utf-8")
    return path


class SamplyTopTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        # the profile starts at 100 ms; the first 10 s after that are dropped
        loading = 5000.0
        steady = 20000.0
        self.threads = [
            make_thread("engine-main", [(loading, [0x10, 0x50], 9_000_000),
                                        (steady, [0x10, 0x50], 2000), (steady + 1, [0x30, 0x50], 8000),
                                        (steady + 2, [0x99, 0x50], 1000)], is_main=True, tid="1"),
            make_thread("Thread <7>", [(steady, [0x20, 0x10, 0x40], 4000), (steady + 1, [0x30, 0x40], 1000)], tid="7"),
            make_thread("Thread <8>", [(steady, [0x60], 50_000)], tid="8"),
            make_thread("Compute Task Pool (0)", [(steady, [0x60], 3000), (steady + 1, [0x30], 500)], tid="2"),
            make_thread("Compute Task Pool (1)", [(steady, [0x60], 2000)], tid="3"),
        ]
        self.path = write_samply(self.root, self.threads)

    def test_sidecar_path_is_the_same_basename(self):
        self.assertEqual(perf.samply_sidecar_path("/x/cpu-profile.json.gz").name, "cpu-profile.json.syms.json")
        self.assertEqual(perf.samply_sidecar_path("/x/p.json").name, "p.json.syms.json")

    def test_cpu_comes_from_thread_cpu_delta_in_the_declared_units_without_weights(self):
        analysis = perf.analyze_samply(self.path, skip_seconds=10)
        main = analysis["merged"]["main"]
        self_ms = {k[1]: v for k, v in main["self_ms"].items()}
        # 2000 us = 2 ms; the sample weight of 5 is ignored; the loading sample is cropped away
        self.assertAlmostEqual(self_ms["bevy_render::render_system"], 2.0)
        self.assertAlmostEqual(self_ms["__psynch_cvwait"], 8.0)
        self.assertAlmostEqual(main["cpu_ms"], 11.0)
        self.assertEqual(main["samples"], 3)

    def test_units_are_honoured(self):
        path = write_samply(self.root, self.threads, unit="ms", name="ms")
        main = perf.analyze_samply(path, skip_seconds=10)["merged"]["main"]
        self.assertAlmostEqual(main["cpu_ms"], 11000.0)

    def test_inclusive_counts_a_function_once_per_stack(self):
        analysis = perf.analyze_samply(self.path, skip_seconds=10)
        inclusive = {k[1]: v for k, v in analysis["merged"]["main"]["inclusive_ms"].items()}
        self.assertAlmostEqual(inclusive["main_loop"], 11.0)
        self.assertAlmostEqual(inclusive["bevy_render::render_system"], 2.0)

    def test_groups_main_render_compute(self):
        groups = perf.analyze_samply(self.path, skip_seconds=10)["groups"]
        self.assertEqual([t["name"] for t in groups["main"]], ["engine-main"])
        # the render thread is the unnamed thread running the pipelined-rendering loop, not the busiest one
        self.assertEqual([t["name"] for t in groups["render"]], ["Thread <7>"])
        self.assertEqual(sorted(t["name"] for t in groups["compute"]), ["Compute Task Pool (0)", "Compute Task Pool (1)"])
        merged = perf.analyze_samply(self.path, skip_seconds=10)["merged"]["compute"]
        self.assertAlmostEqual(merged["self_ms"][next(k for k in merged["self_ms"] if k[1] == "compute_work")], 5.0)

    def test_unsymbolicated_samples_and_addresses_are_counted(self):
        analysis = perf.analyze_samply(self.path, skip_seconds=10)
        u = analysis["unresolved"]
        # 0x99 is in no symbol range; the busy unnamed thread's 0x60 is symbolicated
        self.assertEqual(u["samples"], 1)
        self.assertEqual(u["leaf_addresses"], 1)
        text = perf.format_samply_top(analysis)
        self.assertIn("unsymbolicated: 1 of", text)
        self.assertIn("engine+0x99", text)

    def test_skip_drops_loading(self):
        everything = perf.analyze_samply(self.path, skip_seconds=0)["merged"]["main"]["cpu_ms"]
        self.assertAlmostEqual(everything, 9000.0 + 11.0)

    def test_output_marks_parking_endpoints_and_hides_root_frames(self):
        text = perf.format_samply_top(perf.analyze_samply(self.path, skip_seconds=10), top=5)
        self.assertIn("== main:", text)
        self.assertIn("== render:", text)
        self.assertIn("== compute: 2 threads", text)
        self.assertRegex(text, r"%\* __psynch_cvwait")
        self.assertIn("parking endpoints (marked *)", text)
        self.assertIn("root frames on every sample omitted", text)

    def test_thread_selection(self):
        analysis = perf.analyze_samply(self.path, skip_seconds=10)
        self.assertEqual(perf.select_groups(analysis, None), ["main", "render", "compute"])
        self.assertEqual(perf.select_groups(analysis, "compute"), ["compute"])
        self.assertEqual(perf.select_groups(analysis, "engine-main"), ["main"])
        with self.assertRaises(perf.HarnessError) as caught:
            perf.select_groups(analysis, "nope")
        self.assertEqual(caught.exception.name, "no_such_thread")

    def test_against_lists_the_biggest_changes(self):
        other_threads = [make_thread("engine-main", [(20000.0, [0x10, 0x50], 2000), (20001.0, [0x30, 0x50], 1000)],
                                     is_main=True, tid="1")]
        other = perf.analyze_samply(write_samply(self.root, other_threads, name="other"), skip_seconds=10)
        this = perf.analyze_samply(self.path, skip_seconds=10)
        changes = perf.compare_samply(this, other, "main", top=5)
        rows = changes["main"]["self_ms"]
        self.assertEqual(rows[0][0][1], "__psynch_cvwait")
        self.assertGreater(rows[0][3], 0)
        self.assertIn("changes in CPU ms per second", perf.format_samply_comparison(changes, this, other))

    def test_missing_sidecar_leaves_raw_addresses(self):
        (self.root / "cpu-profile.json.syms.json").unlink()
        analysis = perf.analyze_samply(self.path, skip_seconds=10)
        self.assertIsNone(analysis["symbols"])
        self.assertEqual(analysis["unresolved"]["samples"], analysis["unresolved"]["total_samples"])
        self.assertIn("no .syms.json sidecar", perf.format_samply_top(analysis))

    def test_command_prints_and_writes_json(self):
        out = self.root / "top.json"
        with mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
            code = perf.main(["samply-top", str(self.path), "--top", "3", "--thread", "main", "--against",
                              str(self.path), "--json", str(out)])
        self.assertEqual(code, 0)
        self.assertIn("== main:", stdout.getvalue())
        data = json.loads(out.read_text())
        self.assertEqual(data["this"]["groups"]["main"]["self"][0]["function"], "__psynch_cvwait")
        self.assertIn("main", data["changes"])


if __name__ == "__main__":
    unittest.main()
