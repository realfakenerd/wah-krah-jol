import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "analyze-streaming-comparison.py"
SPEC = importlib.util.spec_from_file_location("streaming_comparison_analysis", SCRIPT)
ANALYSIS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYSIS)

MODES = {
    "fixed64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": False, "backlog": 0, "memory_mib": 0},
    "adaptive128": {"jobs": 128, "activations": 96, "upload_mib": 32, "adaptive": True, "backlog": 256, "memory_mib": 16384},
    "controlled64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": False, "backlog": 256, "memory_mib": 16384},
    "adaptive64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": True, "backlog": 256, "memory_mib": 16384},
}


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


class CampaignTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.expected = {"cells": 1, "models": 20, "lod_chunks": 1, "terrain": 1, "water": 1}
        self.hashes = {"engine": "same-engine", "skyrim_world.db": "same-db"}
        self.experiment = {"format_version": 1, "target_frame_ms": 1000 / 60, "input_hashes": self.hashes,
                           "protocol": {"expected_startup": self.expected, "modes": MODES, "present_mode": "AutoVsync"}, "runs": []}

    def fixture(self, mode="fixed64", pair="01", scenario="normal"):
        run_id = pair + "-" + mode
        directory = self.root / run_id
        settings = MODES[mode]
        argv = ["engine", "--prioritize-streaming", "--benchmark-vsync"]
        for key, flag in {"jobs": "--max-scene-loads", "activations": "--max-model-spawns-per-frame",
                          "upload_mib": "--max-upload-mib-per-frame", "backlog": "--max-streaming-backlog",
                          "memory_mib": "--streaming-memory-mib"}.items():
            argv += [flag, str(settings[key])]
        if settings["adaptive"]:
            argv += ["--adaptive-streaming"]
        record = {"run_id": run_id, "pair_id": pair, "mode": mode, "scenario": scenario, "artifact_dir": str(directory),
                  "order": 1, "exit_code": 0, "status": "completed", "speed": 100, "route_secs": 0.08, "tail_secs": 0.09, "argv": argv}
        self.experiment["runs"].append(record)
        times = [10, 20, 30, 40, 50, 60, 70, 100, 140, 200, 240, 260]
        counts = [0, 5, 10, 15, 19, 20, 20, 20, 20, 22, 22, 22]
        samples = []
        for index, (elapsed, count) in enumerate(zip(times, counts)):
            phase = "waiting" if elapsed < 80 else "moving" if elapsed < 160 else "tail" if elapsed < 250 else "finished"
            distance = 100 * min(0.08, max(0, (elapsed - 80) / 1000))
            work = {key: 0 for key in ANALYSIS.EMPTY_STREAMING}
            work.update(resident_cells=1 if count >= 20 else 0, lifetime_cpu_validated_model_placements=count,
                        resident_lod_chunks=1 if count >= 20 else 0, lifetime_terrain_patches_validated=1 if count >= 20 else 0,
                        lifetime_water_surfaces_validated=1 if count >= 20 else 0, lifetime_asset_load_failures=0)
            if count < 20:
                work.update(loading_cells=1, pending_model_placements=20 - count)
            admission = {key: 0 for key in ANALYSIS.EMPTY_ADMISSION}
            admission.update(configured_job_limit=settings["jobs"], prioritization_enabled=True)
            samples.append({"main_frame": index + 1, "elapsed_ms": elapsed, "delta_ms": 0 if index == 0 else elapsed - times[index - 1],
                            "benchmark_window": "warmup" if elapsed < 80 else "measured", "streaming": work, "scene_admission": admission,
                            "scenes": {"identity_tracking_complete": True, "lifetime_unique_failed": 0},
                            "cpu_spans_ms": {"scene/spawn_batch": 2, "streaming/frame_commit": 3},
                            "completion_latencies_ms": {"assets/model_ready": 800} if count == 20 else {},
                            "camera": {"worldspace_id": 60, "world_position": [2048, 100, 2048 - distance]},
                            "process_memory_gib": 0, "gpu": None, "adaptive": None,
                            "streaming_benchmark_route": {"phase": phase, "elapsed_ms": elapsed, "real_elapsed_ms": elapsed,
                              "cpu_settled_elapsed_ms": 60 if elapsed >= 60 else None,
                              "movement_started_elapsed_ms": 80 if elapsed >= 80 else None,
                              "movement_finished_elapsed_ms": 160 if elapsed >= 160 else None,
                              "tail_finished_elapsed_ms": 250 if elapsed >= 250 else None, "route_distance_units": distance}})
        trace = {"format_version": 1, "dropped_samples": 0, "samples": samples}
        aggregate = {key: 0 for key in ANALYSIS.FAILURE_KEYS}
        aggregate.update(asset_failures=[], assets_ready=22, requests_submitted=1, responses_received=1, unloaded_cells=0)
        timeline = [{"elapsed_ms": 5, "frame": 1, "subject": "Exterior { worldspace_id: 60, grid_x: 0, grid_y: -1 }", "stage": "requested"},
                    {"elapsed_ms": 60, "frame": 6, "subject": "Exterior { worldspace_id: 60, grid_x: 0, grid_y: -1 }", "stage": "committed"}]
        run = {**record, "input_hashes_before": self.hashes, "input_hashes_after": self.hashes,
               "launch_utc_epoch": 999.0, "launch_monotonic": 100.0, "end_utc_epoch": 1000.01, "end_monotonic": 101.01}
        route = {"expected_startup": self.expected, "speed": 100, "movement_duration_secs": 0.08, "tail_duration_secs": 0.09,
                 **samples[-1]["streaming_benchmark_route"], "initial_request_elapsed_ms": 5}
        report = {"generated_unix_ms": 1000000, "streaming_benchmark_route": {**route, "elapsed_ms": 270}, "renderer": {"renderer_validation_failures": 0}}
        cpu = {"spans": {"assets/model_ready": {"count": 22, "total": 880, "mean": 40, "p50": 30, "p95": 60, "p99": 68, "worst": 70}}}
        rss = {"interval_secs": 1, "samples": [{"utc_epoch": 999.01, "monotonic": 100.01, "pid": 99, "rss_bytes": 100},
                                                {"utc_epoch": 999.9, "monotonic": 100.9, "pid": 99, "rss_bytes": 500}]}
        for name, value in {"run.json": run, "route.json": route, "report.json": report, "rss.json": rss,
                            "profile/streaming-frames.json": trace, "profile/streaming.json": {"aggregate": aggregate, "timeline": timeline},
                            "profile/cpu-spans.json": cpu,
                            "profile/metadata.json": {"run_id": run_id, "scenario": f"comparison-{scenario}-{mode}",
                                                                    "commit": "test", "build_profile": "quick",
                                                                    "resolution": [3200, 1802],
                                                                    "window": {"physical_resolution": [3200, 1802]},
                                                                    "worldspace_id": 60, "start_grid": [5, -12],
                                                                    "stream_radius": 2}}.items():
            write_json(directory / name, value)
        return record, trace

    def analyze(self, record):
        return ANALYSIS.analyze_run(self.experiment, record)

    def mutate(self, record, name, callback):
        path = Path(record["artifact_dir"]) / name
        value = json.loads(path.read_text())
        callback(value)
        write_json(path, value)

    def test_end_to_end_startup_includes_initialization_and_latency_sums_are_not_cpu(self):
        record, _ = self.fixture()
        result = self.analyze(record)
        self.assertTrue(result["valid"], result["errors"])
        self.assertEqual(result["startup_timing"]["process_launch_to_cpu_settled_ms"], 790)
        self.assertEqual(result["metrics"]["readiness.cpu_settled_ms"], 55)
        self.assertEqual(result["metrics"]["completion.assets/model_ready.p95_ms"], 60)
        self.assertEqual(result["phases"]["startup"]["cpu_spans_ms"]["scene/spawn_batch"]["p95"], 2)
        self.assertEqual(result["metrics"]["readiness.models_25_percent_ms"], 15)
        self.assertEqual(result["metrics"]["readiness.models_95_percent_ms"], 45)
        self.assertEqual(result["metrics"]["readiness.lod_ready_ms"], 55)

    def test_phase_boundaries_preserve_frames_and_prorate_interval_contributions(self):
        record, _ = self.fixture()
        result = self.analyze(record)
        moving, tail = (result["phases"][key] for key in ("moving_prorated", "tail_prorated"))
        self.assertAlmostEqual(moving["elapsed_seconds"], 0.08)
        self.assertAlmostEqual(tail["elapsed_seconds"], 0.09)
        self.assertEqual(moving["frame_intervals"]["max"], 60)
        self.assertAlmostEqual(moving["hitch_excess_ms"], (40 - 33.33) + (60 - 33.33) / 3)
        self.assertAlmostEqual(moving["hitch_excess_ms_per_second"], moving["hitch_excess_ms"] / 0.08)
        self.assertAlmostEqual(result["phases"]["moving"]["elapsed_seconds"], 0.07)
        self.assertEqual(result["phases"]["moving"]["frame_intervals"]["max"], 40)

    def test_later_report_clock_conversion_cannot_falsely_truncate_a_finished_trace(self):
        record, trace = self.fixture()
        last = trace["samples"][-1]
        last.update(elapsed_ms=250.5, delta_ms=10.5)
        last["streaming_benchmark_route"].update(elapsed_ms=250.5, real_elapsed_ms=250.5)
        write_json(Path(record["artifact_dir"]) / "profile/streaming-frames.json", trace)
        snapshot = copy.deepcopy(last["streaming_benchmark_route"])
        self.mutate(record, "route.json", lambda route: route.update(snapshot))
        self.mutate(record, "report.json", lambda report: report.update(streaming_benchmark_route=copy.deepcopy(snapshot)))
        baseline = self.analyze(record)
        self.assertTrue(baseline["valid"], baseline["errors"])

        def later_conversion(route):
            for key in ("elapsed_ms", "cpu_settled_elapsed_ms", "movement_started_elapsed_ms",
                        "movement_finished_elapsed_ms", "tail_finished_elapsed_ms"):
                route[key] += 4

        self.mutate(record, "route.json", later_conversion)
        self.mutate(record, "report.json", lambda report: later_conversion(report["streaming_benchmark_route"]))
        result = self.analyze(record)
        self.assertTrue(result["valid"], result["errors"])
        self.assertEqual(result["workload"]["tail_end_real_elapsed_ms"], 250)
        self.assertEqual(result["workload"]["terminal_past_tail_end_ms"], 0.5)
        for phase in ("moving_prorated", "tail_prorated"):
            self.assertEqual(result["phases"][phase], baseline["phases"][phase])

    def test_finished_report_cannot_certify_a_genuinely_truncated_or_early_finished_trace(self):
        record, trace = self.fixture(pair="truncated")
        trace["samples"].pop()
        write_json(Path(record["artifact_dir"]) / "profile/streaming-frames.json", trace)
        result = self.analyze(record)
        self.assertFalse(result["valid"])
        self.assertIn("prescribed route and tail did not finish", result["errors"])

        record, trace = self.fixture(pair="early-finished")
        last = trace["samples"][-1]
        last.update(elapsed_ms=249, delta_ms=9)
        last["streaming_benchmark_route"].update(elapsed_ms=249, real_elapsed_ms=249)
        write_json(Path(record["artifact_dir"]) / "profile/streaming-frames.json", trace)
        result = self.analyze(record)
        self.assertFalse(result["valid"])
        self.assertIn("trace ended before the route tail completed", result["errors"])

    def test_pending_model_exposure_and_phase_rss_keep_their_measurement_limits(self):
        record, _ = self.fixture()
        self.mutate(record, "profile/streaming-frames.json", lambda t: t["samples"][8]["streaming"].update(pending_model_placements=7))
        result = self.analyze(record)
        exposure = result["phases"]["moving"]["pending_exposure"]["pending_model_placements"]
        self.assertAlmostEqual(exposure["count_seconds"], 7 * 0.04)
        self.assertEqual(exposure["peak_depth"], 7)
        self.assertTrue(result["phase_os_rss"]["available"])
        self.assertIsNone(result["phase_os_rss"]["phases"]["moving"])
        self.assertEqual(result["phase_os_rss"]["phases"]["tail"]["sampled_peak_bytes"], 500)
        self.assertTrue(result["phase_os_rss"]["phases"]["tail"]["peak_is_lower_bound"])

    def test_mismatched_physical_resolution_excludes_pair_without_rewriting_run_validity(self):
        self.fixture()
        treatment, _ = self.fixture("adaptive128")
        self.mutate(treatment, "profile/metadata.json", lambda m: m["window"].update(physical_resolution=[1600, 901]))
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        self.assertTrue(all(r["valid"] for r in result["runs"]))
        self.assertEqual(result["comparisons"][0]["paired_count"], 0)
        self.assertEqual(result["comparisons"][0]["excluded_pair_ids"], ["01"])

    def test_real_profile_scenario_label_is_the_runner_label_and_pairs(self):
        self.fixture()
        self.fixture("adaptive128")
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        self.assertTrue(all(r["comparison_eligible"] for r in result["runs"]), [r["errors"] for r in result["runs"]])
        self.assertEqual(result["comparisons"][0]["paired_count"], 1)

    def test_profile_scenario_must_be_the_exact_runner_label(self):
        for label in ("normal-fixed64", "comparison-stress-fixed64", "comparison-normal-adaptive128"):
            record, _ = self.fixture()
            self.mutate(record, "profile/metadata.json", lambda m: m.update(scenario=label))
            result = self.analyze(record)
            self.assertFalse(result["comparison_eligible"], label)
            self.assertIn("profile provenance mismatch: scenario", result["errors"], label)

    def test_profile_metadata_without_scenario_label_fails_closed(self):
        record, _ = self.fixture()
        self.mutate(record, "profile/metadata.json", lambda m: m.pop("scenario"))
        result = self.analyze(record)
        self.assertFalse(result["comparison_eligible"])
        self.assertIn("profile provenance lacks scenario", result["errors"])

    def test_profile_pairing_fields_reject_a_relabelled_pair_in_a_paired_comparison(self):
        self.fixture()
        treatment, _ = self.fixture("adaptive128")
        self.mutate(treatment, "profile/metadata.json", lambda m: m.update(pair_id="unrelated-repetition"))
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        self.assertEqual(result["comparisons"][0]["paired_count"], 0)
        self.assertIn("profile provenance mismatch: pair_id", result["runs"][1]["errors"])

    def test_route_that_did_not_finish_is_invalid_but_keeps_its_counters(self):
        record, _ = self.fixture()
        self.mutate(record, "route.json", lambda r: r.update(phase="waiting"))
        result = self.analyze(record)
        self.assertFalse(result["valid"])
        self.assertFalse(result["comparison_eligible"])
        self.assertTrue(any("route did not finish" in error for error in result["errors"]), result["errors"])
        self.assertIn("moving.frame_ms_p95", result["metrics"])

    def test_lost_device_is_named_even_without_a_profile_bundle(self):
        record, _ = self.fixture()
        record.update(exit_code=ANALYSIS.EXIT_DEVICE_LOST, status="failed")
        for name in ("streaming-frames.json", "streaming.json", "cpu-spans.json", "metadata.json"):
            (Path(record["artifact_dir"]) / "profile" / name).unlink()
        result = self.analyze(record)
        self.assertTrue(result["device_lost"])
        self.assertFalse(result["valid"])
        self.assertEqual(result["errors"][0], ANALYSIS.DEVICE_LOST_ERROR)
        self.assertTrue(any("cannot read artifacts" in error for error in result["errors"]), result["errors"])

    def test_other_engine_failures_are_not_reported_as_a_lost_device(self):
        record, _ = self.fixture()
        record.update(exit_code=1, status="failed")
        result = self.analyze(record)
        self.assertFalse(result["device_lost"])
        self.assertIn("engine exit was not successful", result["errors"])
        self.assertNotIn(ANALYSIS.DEVICE_LOST_ERROR, result["errors"])

    def test_truncated_rss_coverage_drops_only_memory_metrics(self):
        record, _ = self.fixture()
        self.mutate(record, "run.json", lambda r: r.update(end_monotonic=110.0))
        result = self.analyze(record)
        self.assertTrue(result["valid"], result["errors"])
        self.assertTrue(result["comparison_eligible"])
        self.assertFalse(result["os_rss"]["available"])
        self.assertFalse(result["phase_os_rss"]["available"])
        self.assertNotIn("rss.sampled_peak_bytes", result["metrics"])
        self.assertIn("moving.frame_ms_p95", result["metrics"])

    def test_internal_rss_gap_cannot_yield_a_paired_memory_comparison(self):
        self.fixture()
        treatment, _ = self.fixture("adaptive128")
        self.mutate(treatment, "rss.json", lambda r: r.update(samples=[
            {"utc_epoch": 999.01, "monotonic": 100.01, "pid": 99, "rss_bytes": 100},
            {"utc_epoch": 1003.0, "monotonic": 104.0, "pid": 99, "rss_bytes": 500}]))
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        comparison = result["comparisons"][0]
        self.assertEqual(comparison["paired_count"], 1)
        self.assertIn("moving.frame_ms_p95", comparison["metrics"])
        self.assertFalse(any("rss" in name for name in comparison["metrics"]), sorted(comparison["metrics"]))
        treated = next(r for r in result["runs"] if r["run_id"] == treatment["run_id"])
        self.assertTrue(treated["comparison_eligible"])
        self.assertFalse(treated["os_rss"]["available"])

    def test_wrong_pair_label_in_manifest_cannot_produce_a_paired_comparison(self):
        self.fixture()
        treatment, _ = self.fixture("adaptive128")
        treatment["pair_id"] = "unrelated-repetition"
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        comparison = result["comparisons"][0]
        self.assertEqual(comparison["paired_count"], 0)
        self.assertEqual(comparison["unpaired_pair_ids"], ["01", "unrelated-repetition"])
        relabelled = next(r for r in result["runs"] if r["run_id"] == treatment["run_id"])
        self.assertFalse(relabelled["valid"])
        self.assertFalse(relabelled["comparison_eligible"])
        self.assertIn("run provenance mismatch: pair_id", relabelled["errors"])
        self.assertTrue(all(r["comparison_eligible"] for r in result["runs"] if r is not relabelled))

    def test_run_json_pairing_identity_must_match_the_manifest(self):
        for key, value in (("mode", "adaptive128"), ("scenario", "stress"), ("pair_id", "other"), ("run_id", "99-other")):
            record, _ = self.fixture()
            self.mutate(record, "run.json", lambda r: r.update({key: value}))
            result = self.analyze(record)
            self.assertFalse(result["comparison_eligible"], key)
            self.assertIn(f"run provenance mismatch: {key}", result["errors"])

    def test_missing_run_json_pairing_identity_fails_closed(self):
        for key in ("run_id", "pair_id", "scenario", "mode"):
            record, _ = self.fixture()
            self.mutate(record, "run.json", lambda r: r.pop(key))
            result = self.analyze(record)
            self.assertFalse(result["comparison_eligible"], key)
            self.assertIn(f"run provenance lacks {key}", result["errors"])

    def test_profile_metadata_pairing_fields_are_compared_only_when_present(self):
        record, _ = self.fixture()
        self.assertTrue(self.analyze(record)["comparison_eligible"])
        self.mutate(record, "profile/metadata.json", lambda m: m.update(pair_id="unrelated-repetition"))
        result = self.analyze(record)
        self.assertFalse(result["comparison_eligible"])
        self.assertIn("profile provenance mismatch: pair_id", result["errors"])

    def test_invalid_and_unpaired_runs_are_exposed_without_erasing_completed_pairs(self):
        self.fixture(pair="good")
        self.fixture("adaptive128", pair="good")
        self.fixture(pair="missing")
        bad, _ = self.fixture(pair="bad")
        self.fixture("adaptive128", pair="bad")
        self.mutate(bad, "profile/streaming-frames.json", lambda t: t.update(dropped_samples=1))
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=20)
        comparison = result["comparisons"][0]
        self.assertEqual(comparison["paired_count"], 1)
        self.assertEqual(comparison["unpaired_pair_ids"], ["missing"])
        self.assertEqual(comparison["excluded_pair_ids"], ["bad"])
        self.assertEqual(len(result["runs"]), 5)

    def test_valid_excluded_triplet_retains_measurements_but_only_repeat_is_paired(self):
        exclusion = {"reason": "unrelated Cargo build overlapped one arm; entire triplet repeated",
                     "scope": "primary_comparisons", "affected_run_id": "original-adaptive64"}
        for mode in ("fixed64", "controlled64", "adaptive64"):
            record, _ = self.fixture(mode, pair="original", scenario="ablation")
            record["analysis_exclusion"] = copy.deepcopy(exclusion)
            self.fixture(mode, pair="repeat", scenario="ablation")
        result = ANALYSIS.analyze_experiment(
            self.experiment, comparisons=[("fixed64", "controlled64"), ("controlled64", "adaptive64")], iterations=20)
        self.assertEqual(len(result["runs"]), 6)
        self.assertTrue(all(run["valid"] for run in result["runs"]))
        for run in result["runs"]:
            self.assertEqual(run["comparison_eligible"], run["pair_id"] == "repeat")
            self.assertIn("moving.frame_ms_p95", run["metrics"])
            if run["pair_id"] == "original":
                self.assertEqual(run["analysis_exclusion"], exclusion)
                self.assertEqual(run["errors"], [])
        for comparison in result["comparisons"]:
            self.assertEqual(comparison["paired_count"], 1)
            self.assertEqual(comparison["excluded_pair_ids"], ["original"])
            self.assertEqual(comparison["metrics"]["moving.frame_ms_p95"]["pair_ids"], ["repeat"])
            detail = comparison["excluded_pairs"][0]
            self.assertEqual(len(detail["analysis_exclusions"]), 2)
            self.assertTrue(all(value == exclusion for value in detail["analysis_exclusions"].values()))
            self.assertTrue(any(exclusion["reason"] in reason for reason in detail["reasons"]))
        report = ANALYSIS.report_markdown(result)
        self.assertIn("Comparison eligible", report)
        self.assertIn("Excluded pair original", report)
        self.assertIn(exclusion["reason"], report)
        self.assertIn(exclusion["scope"], report)

    def test_unplanned_contrasts_are_omitted_but_missing_planned_artifacts_remain_visible(self):
        self.fixture(pair="only", scenario="one_mode")
        self.fixture(pair="good", scenario="planned")
        self.fixture("adaptive128", pair="good", scenario="planned")
        self.fixture(pair="failed", scenario="planned")
        missing, _ = self.fixture("adaptive128", pair="failed", scenario="planned")
        (Path(missing["artifact_dir"]) / "profile/streaming-frames.json").unlink()
        comparisons = [("fixed64", "adaptive128"), ("fixed64", "controlled64"), ("controlled64", "adaptive64")]
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=comparisons, iterations=20)
        self.assertEqual(len(result["comparisons"]), 1)
        comparison = result["comparisons"][0]
        self.assertEqual(comparison["scenario"], "planned")
        self.assertEqual(comparison["paired_count"], 1)
        self.assertEqual(comparison["excluded_pair_ids"], ["failed"])
        self.assertEqual(len(result["runs"]), 5)
        self.assertTrue(next(r for r in result["runs"] if r["run_id"] == missing["run_id"])["errors"])

    def test_catalog_hash_mismatch_cannot_supply_apparently_valid_coverage(self):
        self.fixture()
        catalog = self.root / "route-cell-catalog.json"
        write_json(catalog, {"worldspace_id": 60, "cell_size_units": 4096, "stream_radius": 0,
                             "existing_cells": [[0, -1]], "database_sha256": "changed-db"})
        self.experiment["route_cell_catalog"] = str(catalog)
        with self.assertRaisesRegex(ValueError, "database hash"):
            ANALYSIS.analyze_experiment(self.experiment, iterations=20)

    def test_completed_route_with_unfinished_tail_remains_in_pacing_pairs(self):
        left, _ = self.fixture()
        right, _ = self.fixture("adaptive128")
        self.mutate(right, "profile/streaming-frames.json", lambda t: t["samples"][-1]["streaming"].update(pending_model_placements=10))
        result = ANALYSIS.analyze_experiment(self.experiment, comparisons=[("fixed64", "adaptive128")], iterations=50)
        self.assertTrue(result["runs"][1]["valid"])
        self.assertFalse(result["runs"][1]["readiness_complete"])
        self.assertEqual(result["comparisons"][0]["paired_count"], 1)
        self.assertEqual(result["runs"][1]["metrics"]["terminal.pending.pending_model_placements"], 10)

    def test_invalid_frame_identity_failure_hash_route_and_caps_are_rejected(self):
        cases = [
            ("profile/streaming-frames.json", lambda t: t.update(dropped_samples=1)),
            ("profile/streaming-frames.json", lambda t: t["samples"][2].update(main_frame=99)),
            ("profile/streaming-frames.json", lambda t: t["samples"][2]["scenes"].update(identity_tracking_complete=False)),
            ("profile/streaming.json", lambda t: t["aggregate"].update(asset_load_failures=1)),
            ("run.json", lambda t: t.update(input_hashes_after={"engine": "changed", "skyrim_world.db": "same-db"})),
            ("profile/streaming-frames.json", lambda t: t["samples"][-1]["camera"]["world_position"].__setitem__(2, 1)),
            ("profile/streaming-frames.json", lambda t: t["samples"][-1]["streaming_benchmark_route"].update(tail_finished_elapsed_ms=249)),
            ("run.json", lambda t: t["argv"].remove("--prioritize-streaming")),
        ]
        for index, (name, callback) in enumerate(cases):
            with self.subTest(name=name, case=index):
                record, _ = self.fixture(pair=str(index))
                self.mutate(record, name, callback)
                result = self.analyze(record)
                self.assertFalse(result["valid"], result)
                self.assertTrue(result["errors"])

    def test_rss_absence_and_internal_zero_are_not_measured_zero(self):
        record, _ = self.fixture()
        (Path(record["artifact_dir"]) / "rss.json").unlink()
        result = self.analyze(record)
        self.assertTrue(result["valid"])
        self.assertFalse(result["os_rss"]["available"])
        self.assertNotIn("rss.sampled_peak_bytes", result["metrics"])

    def test_wall_clock_jump_invalidates_only_end_to_end_timing(self):
        record, _ = self.fixture()
        self.mutate(record, "run.json", lambda r: r.update(end_utc_epoch=1001.01))
        result = self.analyze(record)
        self.assertTrue(result["valid"])
        self.assertNotIn("readiness.process_launch_to_cpu_settled_ms", result["metrics"])

    def test_cell_coverage_uses_commits_and_unloads_not_requests_or_empty_db_cells(self):
        samples = [
            {"main_frame": 1, "elapsed_ms": 10, "delta_ms": 10, "camera": {"worldspace_id": 60, "world_position": [2048, 0, 2048]}},
            {"main_frame": 2, "elapsed_ms": 30, "delta_ms": 20, "camera": {"worldspace_id": 60, "world_position": [6144, 0, 2048]}},
        ]
        timeline = [{"elapsed_ms": 0, "subject": "Exterior { worldspace_id: 60, grid_x: 0, grid_y: -1 }", "stage": "committed"},
                    {"elapsed_ms": 20, "subject": "Exterior { worldspace_id: 60, grid_x: 1, grid_y: -1 }", "stage": "requested"}]
        catalog = {"worldspace_id": 60, "cell_size_units": 4096, "stream_radius": 0, "existing_cells": [[0, -1], [1, -1]]}
        observations = ANALYSIS.cell_coverage(samples, timeline, catalog)
        summary = ANALYSIS.coverage_summary(samples, observations)
        self.assertEqual(observations[1]["missing"], 0)
        self.assertEqual(observations[2]["missing"], 1)
        self.assertAlmostEqual(summary["missing_committed_cell_seconds"], 0.02)
        timeline.append({"elapsed_ms": 25, "subject": "Exterior { worldspace_id: 60, grid_x: 1, grid_y: -1 }", "stage": "committed"})
        self.assertEqual(ANALYSIS.cell_coverage(samples, timeline, catalog)[2]["missing"], 0)
        timeline.append({"elapsed_ms": 26, "subject": "Exterior { worldspace_id: 60, grid_x: 1, grid_y: -1 }", "stage": "unloaded"})
        self.assertEqual(ANALYSIS.cell_coverage(samples, timeline, catalog)[2]["missing"], 1)
        catalog["existing_cells"] = [[0, -1]]
        self.assertEqual(ANALYSIS.cell_coverage(samples, timeline, catalog)[2]["desired"], 0)

    def test_cli_writes_analysis_and_discloses_no_gpu_pixel_claim(self):
        self.fixture()
        manifest = self.root / "experiment.json"
        write_json(manifest, self.experiment)
        with contextlib.redirect_stdout(io.StringIO()):
            code = ANALYSIS.main(["--experiment", str(manifest), "--output-dir", str(self.root / "out"), "--bootstrap-samples", "50"])
        self.assertEqual(code, 0)
        result = json.loads((self.root / "out/analysis.json").read_text())
        self.assertEqual(result["bootstrap"]["unit"], "matched run pair")
        self.assertIn("pixel coverage", (self.root / "out/report.md").read_text())


class StatisticsTests(unittest.TestCase):
    def test_paired_median_uses_run_deltas_not_difference_of_medians_or_frame_counts(self):
        pairs = [({"pair_id": str(i), "metrics": {"p95": a}, "frames": 1000000},
                  {"pair_id": str(i), "metrics": {"p95": b}, "frames": 10})
                 for i, (a, b) in enumerate([(1, 30), (10, 11), (20, 21)])]
        result = ANALYSIS.paired_summary(pairs, iterations=1000, seed=7)["p95"]
        self.assertEqual(result["paired_count"], 3)
        self.assertEqual(result["paired_absolute_delta_median"], 1)
        self.assertEqual(result["treatment_median"] - result["baseline_median"], 11)
        self.assertEqual(result["paired_absolute_delta_ci95"], [1, 29])
        self.assertEqual(result, ANALYSIS.paired_summary(pairs, iterations=1000, seed=7)["p95"])

    def test_single_pair_and_zero_baseline_do_not_fabricate_confidence_or_relative_change(self):
        pairs = [({"pair_id": "one", "metrics": {"excess": 0}}, {"pair_id": "one", "metrics": {"excess": 2}})]
        result = ANALYSIS.paired_summary(pairs, iterations=100, seed=7)["excess"]
        self.assertIsNone(result["paired_absolute_delta_ci95"])
        self.assertIsNone(result["paired_relative_delta_percent_median"])
        self.assertEqual(result["relative_paired_count"], 0)

    def test_hitch_thresholds_are_strict_and_excess_has_distinct_33ms_and_target_bases(self):
        samples = [{"delta_ms": value, "cpu_spans_ms": {}} for value in [0, 33.33, 50, 100, 101]]
        result = ANALYSIS.frame_metrics(samples, 16.67)
        self.assertEqual(result["hitches"]["33.33"]["count"], 3)
        self.assertEqual(result["hitches"]["50"]["count"], 2)
        self.assertEqual(result["hitches"]["100"]["count"], 1)
        self.assertAlmostEqual(result["hitch_excess_ms"], 151.01)
        self.assertGreater(result["stutter_excess_ms"], result["hitch_excess_ms"])


if __name__ == "__main__":
    unittest.main()
