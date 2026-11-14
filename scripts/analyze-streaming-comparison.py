#!/usr/bin/env python3
"""Compare streaming campaigns using matched runs, never frames as replicates."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import statistics
import sys
import tempfile


DEFAULT_COMPARISONS = (
    ("fixed64", "adaptive128"),
    ("fixed64", "controlled64"),
    ("controlled64", "adaptive64"),
    ("adaptive64", "adaptive128"),
)
EXPECTED_STARTUP = {"cells": 25, "models": 2029, "lod_chunks": 176, "terrain": 100, "water": 25}
EMPTY_STREAMING = (
    "loading_cells", "retiring_cells", "active_cell_requests", "pending_lod_queries",
    "pending_lod_chunks", "arming_queue_depth", "pending_model_placements", "pending_surface_instances",
)
EMPTY_ADMISSION = ("active_jobs", "queued_jobs", "orphan_active_jobs")
# Identity a run must carry consistently in the manifest and its own run.json before pairing.
PAIR_IDENTITY_KEYS = ("run_id", "pair_id", "scenario", "mode")
ROUTE_SCENARIOS = ("normal", "stress", "ablation", "smoke-route")
# Engine exit code for a lost GPU device (crates/engine/src/app.rs, EXIT_DEVICE_LOST). Such a run
# measured nothing and is worth retrying, but repeated losses can still be the engine's fault.
EXIT_DEVICE_LOST = 3
DEVICE_LOST_ERROR = f"GPU device lost (engine exit code {EXIT_DEVICE_LOST}); retry the run"
FAILURE_KEYS = (
    "failed_cells", "asset_load_failures", "material_validation_failures", "diagnostic_fallbacks",
    "terrain_validation_failures", "water_validation_failures", "transform_bounds_validation_failures",
    "failed_lod_queries", "failed_lod_chunks", "unrecovered_lod_queries", "unrecovered_lod_chunks",
    "lod_query_submission_failures", "duplicate_cell_roots", "orphaned_cell_roots", "missing_cell_roots",
    "out_of_range_cell_roots", "streaming_invariant_failures", "streaming_fixture_failures",
    "physics_fixture_failures", "retire_backlog_overflows",
)
COMPLETION_NAMES = (
    "assets/model_ready", "lod/chunk_ready", "streaming/scene_admission_wait",
    "streaming/db_query", "streaming/db_queue_wait", "streaming/db_request_total",
    "lod/db_query", "lod/db_queue_wait", "lod/db_request_total",
)
ANSI = re.compile(r"\x1b\[[0-9;]*m")
LOG_ERRORS = re.compile(r"\bERROR\b|thread .*panicked|Validation Error|Shader compilation error", re.IGNORECASE)
CELL_KEY = re.compile(r"Exterior \{ worldspace_id: (\d+), grid_x: (-?\d+), grid_y: (-?\d+) \}")
LIMITATIONS = [
    "Main-frame Time<Real> intervals include waits, controller and profiling overhead; they do not isolate CPU or GPU cost.",
    "CPU spans can overlap. Completed-latency frame values are sums, not CPU durations or individual latency samples.",
    "Readiness means recorded CPU validation and drained queues; it does not establish drawing, pixel coverage or walking collision readiness.",
    "OS RSS is independently sampled, normally at 1 Hz. Its sampled peak is a lower bound on the true peak; RSS is not GPU memory or a physical-memory guarantee.",
    "Adaptive reservation estimates are diagnostics only and are not compared against fixed-mode RSS or treated as measured memory.",
    "Paired percentile bootstrap intervals use run pairs within a scenario. Few pairs give coarse, uncertain intervals; no multiplicity adjustment is applied.",
    "Invalid workloads, functional failures and manifest-marked analysis exclusions are excluded from paired estimates. Completed routes with unresolved tail work remain eligible for pacing; their readiness is censored and coverage/pending outcomes are reported.",
    "Cell coverage means database-existing desired cells have committed CPU data. It does not certify individual models, drawing or pixel coverage.",
    "AutoVsync is the requested presentation mode; the resolved swapchain mode is not independently observed. Physical resolution and reported build profile are checked across pairs when available.",
]


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def read_json(path, required=True):
    if not path.exists() and not required:
        return None
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path.name} must be a JSON object")
    return value


def percentile(values, fraction):
    """Linear interpolation at (n-1)*fraction; unavailable data stays None."""
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def distribution(values):
    return {
        "count": len(values),
        "mean": statistics.mean(values) if values else None,
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values) if values else None,
    }


def frame_metrics(samples, target_ms):
    values = [s["delta_ms"] for s in samples if s["delta_ms"] > 0]
    result = {"sample_count": len(samples), "frame_intervals": distribution(values)}
    observed = [(s["delta_ms"], s.get("_interval_weight", 1.0)) for s in samples if s["delta_ms"] > 0]
    elapsed_ms = sum(value * weight for value, weight in observed)
    excess_ms = sum(max(0.0, value - target_ms) * weight for value, weight in observed)
    hitch_excess = sum(max(0.0, value - 33.33) * weight for value, weight in observed)
    result.update(
        elapsed_seconds=elapsed_ms / 1000.0,
        target_frame_ms=target_ms,
        stutter_excess_ms=excess_ms,
        stutter_excess_fraction=excess_ms / elapsed_ms if elapsed_ms else None,
        stutter_excess_ms_per_minute=excess_ms * 60000.0 / elapsed_ms if elapsed_ms else None,
        hitch_excess_ms=hitch_excess,
        hitch_excess_ms_per_second=hitch_excess * 1000.0 / elapsed_ms if elapsed_ms else None,
        hitches={},
    )
    for threshold in (33.33, 50.0, 100.0):
        count = sum(value > threshold for value in values)
        weighted_count = sum(weight for value, weight in observed if value > threshold)
        result["hitches"][f"{threshold:g}"] = {
            "count": count,
            "boundary_weighted_count": weighted_count,
            "per_minute": weighted_count * 60000.0 / elapsed_ms if elapsed_ms else None,
        }
    # Missing names in an otherwise recorded frame are zero work for that name.
    names = sorted({name for sample in samples for name in sample.get("cpu_spans_ms", {})})
    result["cpu_spans_ms"] = {
        name: distribution([sample.get("cpu_spans_ms", {}).get(name, 0.0) for sample in samples])
        for name in names
    }
    result["pending_exposure"] = {}
    for field in ("pending_model_placements", "arming_queue_depth", "pending_surface_instances"):
        depths = [(s.get("streaming") or {}).get(field) for s in samples]
        if samples and all(number(depth) and depth >= 0 for depth in depths):
            total = sum(depth * s["delta_ms"] * s.get("_interval_weight", 1.0) for s, depth in zip(samples, depths)) / 1000.0
            result["pending_exposure"][field] = {
                "count_seconds": total, "mean_depth": total * 1000.0 / elapsed_ms if elapsed_ms else None,
                "peak_depth": max(depths), "terminal_depth": depths[-1],
                "meaning": "aggregate recorded backlog exposure; not unique asset wait time or drawable-asset coverage",
            }
    return result


def route_state(sample):
    return sample.get("streaming_benchmark_route") or sample.get("benchmark_route") or {}


def milestone(samples, predicate, initial_ms):
    sample = next((sample for sample in samples if predicate(sample)), None)
    if sample is None:
        return None
    return {
        "main_frame": sample["main_frame"],
        "profiler_elapsed_ms": sample["elapsed_ms"],
        "from_initial_request_ms": sample["elapsed_ms"] - initial_ms,
    }


def cpu_settled(sample, expected):
    work, admission = sample.get("streaming") or {}, sample.get("scene_admission") or {}
    counts = {
        "cells": work.get("resident_cells"), "models": work.get("lifetime_cpu_validated_model_placements"),
        "lod_chunks": work.get("resident_lod_chunks"), "terrain": work.get("lifetime_terrain_patches_validated"),
        "water": work.get("lifetime_water_surfaces_validated"),
    }
    return counts == expected and all(work.get(key) == 0 for key in EMPTY_STREAMING) and all(
        admission.get(key) == 0 for key in EMPTY_ADMISSION
    )


def route_value(route, samples, name):
    value = route.get(name)
    if number(value):
        return value
    aliases = {
        "movement_start_elapsed_ms": "movement_started_elapsed_ms",
        "movement_end_elapsed_ms": "movement_finished_elapsed_ms",
        "tail_end_elapsed_ms": "tail_finished_elapsed_ms",
        "startup_settled_elapsed_ms": "cpu_settled_elapsed_ms",
    }
    key = aliases.get(name, name)
    if number(route.get(key)):
        return route[key]
    return next((route_state(s)[key] for s in reversed(samples) if number(route_state(s).get(key))), None)


def phase_samples(samples, route, settled_ms, initial_ms):
    movement_start = route_value(route, samples, "movement_start_elapsed_ms")
    movement_end = route_value(route, samples, "movement_end_elapsed_ms")
    tail_end = route_value(route, samples, "tail_end_elapsed_ms")
    if movement_start is not None and movement_end is None:
        duration = route.get("movement_duration_secs")
        if not number(duration):
            duration = next((route_state(s).get("movement_duration_secs") for s in samples
                             if number(route_state(s).get("movement_duration_secs"))), None)
        if number(duration):
            movement_end = movement_start + duration * 1000.0
    if movement_end is not None and tail_end is None:
        duration = route.get("tail_duration_secs")
        if not number(duration):
            duration = next((route_state(s).get("tail_duration_secs") for s in samples
                             if number(route_state(s).get("tail_duration_secs"))), None)
        if number(duration):
            tail_end = movement_end + duration * 1000.0
    phases = {
        "full": samples,
        "warmup": [s for s in samples if s.get("benchmark_window") == "warmup"],
        "measured": [s for s in samples if s.get("benchmark_window") == "measured"],
        "startup": [s for s in samples if s["elapsed_ms"] >= initial_ms
                    and (settled_ms is None or s["elapsed_ms"] <= settled_ms)
                    and (movement_start is None or s["elapsed_ms"] < movement_start)],
        "moving": [], "tail": [], "moving_prorated": [], "tail_prorated": [],
    }
    tagged = any(route_state(s).get("phase") for s in samples)
    anchor = route if number(route.get("real_elapsed_ms")) else route_state(samples[-1])
    offset = anchor.get("elapsed_ms", 0) - anchor.get("real_elapsed_ms", 0)
    use_real_clock = number(anchor.get("elapsed_ms")) and number(anchor.get("real_elapsed_ms"))
    for sample in samples:
        elapsed = sample["elapsed_ms"]
        state = route_state(sample)
        if tagged and state.get("phase") in ("moving", "tail"):
            phases[state["phase"]].append(sample)
        if use_real_clock and number(state.get("real_elapsed_ms")) and movement_start is not None and movement_end is not None:
            end = state["real_elapsed_ms"]
            start = end - sample["delta_ms"]
            for phase, low, high in (("moving", movement_start - offset, movement_end - offset),
                                     ("tail", movement_end - offset, tail_end - offset if tail_end is not None else math.inf)):
                overlap = max(0.0, min(end, high) - max(start, low))
                if overlap > 0 and sample["delta_ms"] > 0:
                    phases[phase + "_prorated"].append({**sample, "_interval_weight": overlap / sample["delta_ms"]})
        elif tagged:
            pass
        elif movement_start is not None and movement_end is not None:
            if movement_start <= elapsed < movement_end:
                phases["moving"].append(sample)
            elif elapsed >= movement_end and (tail_end is None or elapsed <= tail_end):
                phases["tail"].append(sample)
    return phases, {"movement_start_elapsed_ms": movement_start, "movement_end_elapsed_ms": movement_end,
                    "tail_end_elapsed_ms": tail_end}


def phase_rss_metrics(document, run, report, boundaries, warnings):
    result = {"available": False, "phases": {"startup": None, "moving": None, "tail": None}}
    snapshot = (report or {}).get("streaming_benchmark_route") or {}
    values = [run.get(key) for key in ("launch_utc_epoch", "launch_monotonic", "end_utc_epoch", "end_monotonic")]
    generated, elapsed = (report or {}).get("generated_unix_ms"), snapshot.get("elapsed_ms")
    samples = (document or {}).get("samples") or []
    if not samples or not all(number(v) for v in values + [generated, elapsed]):
        warnings.append("phase RSS clock alignment unavailable")
        return result
    launch_utc, launch_mono, end_utc, end_mono = values
    if abs((end_utc - launch_utc) - (end_mono - launch_mono)) > 0.1 or any(
        not all(number(s.get(key)) for key in ("utc_epoch", "monotonic", "rss_bytes")) or
        abs(s["utc_epoch"] - launch_utc - (s["monotonic"] - launch_mono)) > 0.1
        for s in samples
    ):
        warnings.append("phase RSS wall/monotonic clock bridge is inconsistent")
        return result
    profiler_utc_origin = generated / 1000.0 - elapsed / 1000.0
    mono = lambda stamp: launch_mono + profiler_utc_origin - launch_utc + stamp / 1000.0
    ready = snapshot.get("cpu_settled_elapsed_ms")
    start, finish, end = (boundaries.get(key) for key in ("movement_start_elapsed_ms", "movement_end_elapsed_ms", "tail_end_elapsed_ms"))
    windows = {"startup": (launch_mono, mono(ready)) if number(ready) else None,
               "moving": (mono(start), mono(finish)) if number(start) and number(finish) else None,
               "tail": (mono(finish), mono(end)) if number(finish) and number(end) else None}
    for phase, window in windows.items():
        if window is None or window[1] <= window[0]:
            continue
        selected = [s for s in samples if window[0] <= s["monotonic"] <= window[1] and s["rss_bytes"] > 0]
        if not selected:
            continue
        rss = [s["rss_bytes"] for s in selected]
        result["phases"][phase] = {
            "sample_count": len(selected), "samples": distribution(rss), "sampled_peak_bytes": max(rss),
            "first_rss_bytes": rss[0], "last_rss_bytes": rss[-1], "observed_change_bytes": rss[-1] - rss[0],
            "first_sample_after_phase_start_seconds": selected[0]["monotonic"] - window[0],
            "last_sample_before_phase_end_seconds": window[1] - selected[-1]["monotonic"],
            "peak_is_lower_bound": True,
        }
    interval = document.get("interval_secs", 1.0)
    def prior_sample(boundary):
        if boundary is None:
            return None
        prior = [s for s in samples if s["monotonic"] <= boundary and s["rss_bytes"] > 0]
        if not prior or boundary - prior[-1]["monotonic"] > interval * 1.5:
            return None
        return {"rss_bytes": prior[-1]["rss_bytes"], "age_seconds": boundary - prior[-1]["monotonic"]}
    at_start, at_finish, at_end = (prior_sample(mono(t) if number(t) else None) for t in (start, finish, end))
    result.update(available=True, clock_alignment="report profiler elapsed_ms to launch wall/monotonic bridge; 100 ms consistency tolerance",
                  movement_start_prior_sample=at_start, movement_end_prior_sample=at_finish, tail_end_prior_sample=at_end)
    result["tail_end_minus_movement_start_bytes"] = at_end["rss_bytes"] - at_start["rss_bytes"] if at_end and at_start else None
    result["tail_end_minus_movement_end_bytes"] = at_end["rss_bytes"] - at_finish["rss_bytes"] if at_end and at_finish else None
    result["boundary_change_meaning"] = "difference of recent observed samples at or before boundaries; ages reported, not exact instantaneous RSS or a leak verdict"
    return result


def flag_value(argv, flag):
    try:
        index = argv.index(flag)
        return argv[index + 1]
    except (ValueError, IndexError):
        return None


def validate_configuration(experiment, record, run, samples, errors, warnings):
    argv = run.get("argv") or record.get("argv") or []
    mode = ((experiment.get("protocol") or {}).get("modes") or {}).get(record.get("mode"))
    if not isinstance(mode, dict):
        warnings.append("effective CLI mode settings were not provided for verification")
        return
    flags = {"jobs": "--max-scene-loads", "activations": "--max-model-spawns-per-frame",
             "upload_mib": "--max-upload-mib-per-frame", "backlog": "--max-streaming-backlog",
             "memory_mib": "--streaming-memory-mib"}
    for name, flag in flags.items():
        if str(mode.get(name)) != flag_value(argv, flag):
            errors.append(f"effective CLI setting differs for {flag}")
    if ("--adaptive-streaming" in argv) != bool(mode.get("adaptive")):
        errors.append("effective adaptive mode differs from protocol")
    if "--prioritize-streaming" not in argv:
        errors.append("nearest-first prioritization was not enabled")
    if (experiment.get("protocol") or {}).get("present_mode") == "AutoVsync" and "--benchmark-vsync" not in argv:
        errors.append("effective presentation mode differs from protocol")
    if any(s["scene_admission"].get("configured_job_limit") != mode.get("jobs") or
           s["scene_admission"].get("prioritization_enabled") is not True or
           s["scene_admission"].get("active_jobs", 0) > mode.get("jobs", 0) for s in samples):
        errors.append("observed scene-admission settings differ from protocol or exceeded the hard limit")


def validate_route(record, run, route, samples, errors, warnings):
    tagged = [s for s in samples if route_state(s)]
    if not tagged:
        if record.get("scenario") in ("normal", "stress", "ablation", "smoke-route"):
            errors.append("prescribed movement route observations unavailable")
        else:
            warnings.append("route/camera workload verification unavailable")
        return {"verified": False}
    speed = record.get("speed", run.get("speed", route.get("speed")))
    duration = record.get("route_secs", run.get("route_secs", route.get("movement_duration_secs")))
    tail = record.get("tail_secs", run.get("tail_secs", route.get("tail_duration_secs")))
    if not all(number(value) and value >= 0 for value in (speed, duration, tail)):
        errors.append("prescribed route speed or duration unavailable")
        return {"verified": False}
    # The engine skips movement for a stationary route, even with a nonzero configured duration.
    if speed == 0:
        duration = 0.0
    positions = [(s.get("camera") or {}).get("world_position") for s in tagged]
    if any(not isinstance(p, list) or len(p) != 3 or not all(number(v) for v in p) for p in positions):
        errors.append("absolute route camera observations unavailable")
        return {"verified": False}
    first_state = route_state(tagged[0])
    if first_state.get("phase") != "waiting" or first_state.get("route_distance_units") != 0:
        errors.append("route lacks its stationary initial camera boundary")
    initial = positions[0]
    previous_real = None
    for sample, position in zip(tagged, positions):
        state = route_state(sample)
        if not all(number(state.get(key)) for key in ("elapsed_ms", "real_elapsed_ms", "route_distance_units")):
            errors.append("route time or distance observations malformed")
            return {"verified": False}
        now = state["real_elapsed_ms"]
        if previous_real is not None and now <= previous_real:
            errors.append("route real clock is not strictly advancing")
        previous_real = now
        offset = state["elapsed_ms"] - now
        started = state.get("movement_started_elapsed_ms")
        distance = 0.0 if not number(started) else speed * min(duration, max(0.0, (now - (started - offset)) / 1000.0))
        if abs(state["route_distance_units"] - distance) > 0.01:
            errors.append("observed route distance differs from prescribed speed and real time")
        expected = [initial[0], initial[1], initial[2] - state["route_distance_units"]]
        if any(abs(a - b) > 0.01 for a, b in zip(position, expected)):
            errors.append("camera position differs from the prescribed absolute -Z route")
    last = route_state(tagged[-1])
    start, finish, end = (last.get(key) for key in ("movement_started_elapsed_ms", "movement_finished_elapsed_ms", "tail_finished_elapsed_ms"))
    tail_end_real = None
    if last.get("phase") != "finished" or not all(number(v) for v in (start, finish, end)):
        errors.append("prescribed route and tail did not finish")
    elif abs(finish - start - duration * 1000.0) > 0.01 or abs(end - finish - tail * 1000.0) > 0.01:
        errors.append("observed movement or tail duration differs from protocol")
    else:
        # Each snapshot converts route boundaries with its own Instant-to-Real offset.
        # A later report reuses the final Real time but has a later Instant timestamp.
        tail_end_real = end - (last["elapsed_ms"] - last["real_elapsed_ms"])
        if last["real_elapsed_ms"] + 1e-6 < tail_end_real:
            errors.append("trace ended before the route tail completed")
    if abs(last["route_distance_units"] - speed * duration) > 0.01:
        errors.append("route endpoint distance differs from protocol")
    return {"verified": not errors, "speed_units_per_second": speed, "movement_seconds": duration,
            "tail_seconds": tail, "initial_world_position": initial, "final_world_position": positions[-1],
            "route_distance_units": last["route_distance_units"], "position_tolerance_units": 0.01,
            "completion_clock": "final trace snapshot Time<Real> with that snapshot's own profiler-to-real offset",
            "terminal_real_elapsed_ms": last["real_elapsed_ms"], "tail_end_real_elapsed_ms": tail_end_real,
            "terminal_past_tail_end_ms": last["real_elapsed_ms"] - tail_end_real if tail_end_real is not None else None}


def cell_coverage(samples, timeline, catalog):
    if not catalog:
        return None
    existing = {tuple(cell) for cell in catalog["existing_cells"]}
    worldspace, size, radius = (catalog[k] for k in ("worldspace_id", "cell_size_units", "stream_radius"))
    events = []
    for event in timeline:
        match = CELL_KEY.fullmatch(str(event.get("subject", "")))
        if match and int(match[1]) == worldspace and event.get("stage") in ("committed", "unloaded") and number(event.get("elapsed_ms")):
            events.append((event["elapsed_ms"], event["stage"], (int(match[2]), int(match[3]))))
    events.sort(key=lambda event: event[0])
    resident, index, observations = set(), 0, {}
    for sample in samples:
        while index < len(events) and events[index][0] <= sample["elapsed_ms"]:
            _, stage, cell = events[index]
            if stage == "committed":
                resident.add(cell)
            else:
                resident.discard(cell)
            index += 1
        camera = sample.get("camera") or {}
        position = camera.get("world_position")
        if camera.get("worldspace_id") != worldspace or not isinstance(position, list) or len(position) != 3:
            return None
        center = (math.floor(position[0] / size), math.floor(-position[2] / size))
        desired = {(center[0] + x, center[1] + y) for x in range(-radius, radius + 1)
                   for y in range(-radius, radius + 1)} & existing
        observations[sample["main_frame"]] = {"desired": len(desired), "missing": len(desired - resident),
                                              "committed_desired": len(desired & resident)}
    return observations


def coverage_summary(samples, observations):
    if observations is None or not samples:
        return None
    seconds = sum(s["delta_ms"] * s.get("_interval_weight", 1.0) for s in samples) / 1000.0
    deficit = sum(observations[s["main_frame"]]["missing"] * s["delta_ms"] * s.get("_interval_weight", 1.0) for s in samples) / 1000.0
    desired_seconds = sum(observations[s["main_frame"]]["desired"] * s["delta_ms"] * s.get("_interval_weight", 1.0) for s in samples) / 1000.0
    return {"observed_seconds": seconds, "missing_committed_cell_seconds": deficit,
            "mean_missing_committed_cells": deficit / seconds if seconds else None,
            "max_missing_committed_cells": max(observations[s["main_frame"]]["missing"] for s in samples),
            "terminal_missing_committed_cells": observations[samples[-1]["main_frame"]]["missing"],
            "missing_fraction_of_desired_cell_seconds": deficit / desired_seconds if desired_seconds else None,
            "terminal_desired_cells": observations[samples[-1]["main_frame"]]["desired"]}


def rss_metrics(document, run, warnings):
    unavailable = {"available": False, "sampled_peak_bytes": None}
    if document is None:
        warnings.append("OS RSS samples unavailable")
        return unavailable
    samples = document.get("samples")
    if not isinstance(samples, list) or not samples:
        warnings.append("OS RSS sample list is empty or malformed")
        return unavailable
    if any(not isinstance(s, dict) or not number(s.get("rss_bytes")) or s["rss_bytes"] <= 0 for s in samples):
        warnings.append("OS RSS contains invalid or zero-valued samples")
        return unavailable
    clock = "monotonic" if all(number(s.get("monotonic")) for s in samples) else "utc_epoch"
    if any(not number(s.get(clock)) for s in samples):
        warnings.append("OS RSS sample timestamps unavailable")
        return unavailable
    intervals = [b[clock] - a[clock] for a, b in zip(samples, samples[1:])]
    if any(interval <= 0 for interval in intervals):
        warnings.append("OS RSS sample clock is not strictly increasing")
        return unavailable
    interval = document.get("interval_secs", 1.0)
    if not number(interval) or interval <= 0:
        warnings.append("OS RSS sampling interval is invalid")
        return unavailable
    # Memory metrics are only comparable over complete coverage. Gaps invalidate the memory
    # metrics (not the timing run) and the samples stay recorded as evidence.
    gaps = []
    if intervals and max(intervals) > interval * 1.5:
        gaps.append("internal sampling gap exceeds 1.5 times the requested interval")
    if clock != "monotonic":
        warnings.append("OS RSS uses wall-clock timestamps; monotonic timestamps unavailable")
    values = [s["rss_bytes"] for s in samples]
    result = {
        "available": True, "source": "external OS process RSS", "clock": clock,
        "requested_interval_seconds": interval, "samples": distribution(values),
        "sampled_peak_bytes": max(values), "peak_is_lower_bound": True,
        "first_rss_bytes": values[0], "last_rss_bytes": values[-1],
        "observed_change_bytes": values[-1] - values[0],
        "covered_seconds": samples[-1][clock] - samples[0][clock],
        "max_sample_interval_seconds": max(intervals) if intervals else None,
        "pid_count": len({s.get("pid") for s in samples}),
    }
    launch = run.get("launch_monotonic" if clock == "monotonic" else "launch_utc_epoch")
    result["first_sample_after_launch_seconds"] = samples[0][clock] - launch if number(launch) else None
    end = run.get("end_monotonic" if clock == "monotonic" else "end_utc_epoch")
    result["last_sample_before_exit_seconds"] = end - samples[-1][clock] if number(end) else None
    for name in ("first_sample_after_launch_seconds", "last_sample_before_exit_seconds"):
        if number(result[name]) and result[name] > interval * 1.5:
            gaps.append(f"coverage gap: {name}={result[name]:.3f}")
    if result["pid_count"] != 1:
        gaps.append("more than one PID sampled")
    if gaps:
        warnings.append("OS RSS memory metrics unavailable: " + "; ".join(gaps))
        result["available"] = False
        result["unavailable_reasons"] = gaps
    return result


def profile_scenario_label(record):
    """The --profile-scenario label the runner passes to the engine for this run."""
    return f"comparison-{record.get('scenario')}-{record.get('mode')}"


def verify_identity(experiment, record, run, errors, warnings):
    baseline = experiment.get("input_hashes") or {}
    before = run.get("input_hashes_before", record.get("input_hashes_before"))
    after = run.get("input_hashes_after", record.get("input_hashes_after"))
    if before is not None and after is not None and before != after:
        errors.append("binary or pack hashes changed during the run")
    for label, observed in (("before", before), ("after", after)):
        if observed is not None and not isinstance(observed, dict):
            errors.append(f"input hashes {label} are malformed")
        elif isinstance(observed, dict) and any(key in observed and observed[key] != value for key, value in baseline.items()):
            errors.append(f"input hashes {label} differ from campaign inputs")
    verified = bool(baseline) and isinstance(before, dict) and isinstance(after, dict) and all(
        before.get(key) == after.get(key) == value for key, value in baseline.items()
    )
    if not verified:
        warnings.append("per-run binary/pack identity is not fully verified")
    return {"verified": verified, "before": before, "after": after}


def process_startup_metrics(report, run, initial_ms, settled_ms, warnings):
    snapshot = (report or {}).get("streaming_benchmark_route") or {}
    generated = (report or {}).get("generated_unix_ms")
    launch = run.get("launch_utc_epoch")
    elapsed = snapshot.get("elapsed_ms")
    ready = snapshot.get("cpu_settled_elapsed_ms")
    unavailable = {"process_launch_to_cpu_settled_ms": None}
    if not all(number(v) for v in (generated, launch, elapsed, ready)):
        warnings.append("process-launch readiness clock anchor unavailable")
        return unavailable
    clocks = [run.get(k) for k in ("launch_utc_epoch", "end_utc_epoch", "launch_monotonic", "end_monotonic")]
    if all(number(v) for v in clocks) and abs((clocks[1] - clocks[0]) - (clocks[3] - clocks[2])) > 0.1:
        warnings.append("wall clock shifted during the run; end-to-end startup timing unavailable")
        return unavailable
    profiler_origin_after_launch = generated - launch * 1000.0 - elapsed
    result = {
        "process_launch_to_cpu_settled_ms": profiler_origin_after_launch + ready,
        "process_launch_to_initial_request_ms": profiler_origin_after_launch + initial_ms,
        "profiler_origin_after_process_launch_ms": profiler_origin_after_launch,
        "initial_request_to_route_cpu_settled_ms": ready - initial_ms,
        "clock_source": "report generated_unix_ms and route elapsed_ms anchored to launch_utc_epoch",
        "route_cpu_settled_profiler_elapsed_ms": ready,
        "first_observed_cpu_settled_profiler_elapsed_ms": settled_ms,
    }
    if profiler_origin_after_launch < -10.0 or result["process_launch_to_cpu_settled_ms"] < 0 or ready > elapsed:
        warnings.append("process-launch readiness clock anchor is inconsistent")
        return unavailable
    return result


def analyze_run(experiment, record):
    errors, warnings, censored = [], [], []
    result = {key: record.get(key) for key in ("run_id", "pair_id", "mode", "scenario", "order", "artifact_dir", "cache_policy", "analysis_exclusion")}
    result.update(valid=False, comparison_eligible=False, errors=errors, warnings=warnings, censored=censored, metrics={})
    # Checked before the artifacts: a lost device usually ends the run before the engine writes its
    # profile bundle, and the missing files must not hide why.
    result["device_lost"] = record.get("exit_code") == EXIT_DEVICE_LOST
    if result["device_lost"]:
        errors.append(DEVICE_LOST_ERROR)
    try:
        directory = Path(record["artifact_dir"])
        trace = read_json(directory / "profile/streaming-frames.json")
        streaming = read_json(directory / "profile/streaming.json")
        cpu = read_json(directory / "profile/cpu-spans.json")
        run = read_json(directory / "run.json")
        route = read_json(directory / "route.json", required=False) or {}
        validation = read_json(directory / "validation.json", required=False)
        report = read_json(directory / "report.json", required=False)
        metadata = read_json(directory / "profile/metadata.json", required=False)
        host = read_json(directory / "host.json", required=False)
        rss = read_json(directory / "rss.json", required=False)
        if rss is None:
            rss = read_json(directory / "os-rss.json", required=False)
    except (OSError, ValueError, TypeError, KeyError) as error:
        errors.append(f"cannot read artifacts: {error}")
        return result
    result["identity"] = verify_identity(experiment, record, run, errors, warnings)
    result["render_configuration"] = {
        "physical_resolution": ((metadata or {}).get("window") or {}).get("physical_resolution"),
        "reported_build_profile": (metadata or {}).get("build_profile"),
        "requested_present_mode": (experiment.get("protocol") or {}).get("present_mode"),
        "resolved_present_mode_observed": False,
    }
    if not result["render_configuration"]["physical_resolution"] or not result["render_configuration"]["reported_build_profile"]:
        warnings.append("physical resolution or reported build profile unavailable for pair verification")
    result["host_observations"] = host
    if run.get("exit_code") == EXIT_DEVICE_LOST and not result["device_lost"]:
        result["device_lost"] = True
        errors.append(DEVICE_LOST_ERROR)
    if not result["device_lost"] and (record.get("exit_code", run.get("exit_code")) != 0 or run.get("exit_code") != 0):
        errors.append("engine exit was not successful")
    if run.get("timed_out") or run.get("interrupted") or record.get("status") in ("timeout", "timed_out", "aborted", "interrupted"):
        errors.append("run was interrupted or timed out")
        censored.append("process did not finish")
    if record.get("status") in ("pending", "running", "scheduled", "failed", "error"):
        errors.append(f"run status is {record['status']}")
    if record.get("scenario") in ROUTE_SCENARIOS and route.get("phase") != "finished":
        # Counters are still analysed below as evidence; the run cannot pass as a completed route.
        errors.append(f"route did not finish before the engine deadline (phase {route.get('phase')!r})")
    # The manifest's pairing identity must match what the engine run recorded. run.json is the
    # launcher's own record, so a missing field there cannot be certified: it fails closed.
    for key in PAIR_IDENTITY_KEYS:
        if run.get(key) is None:
            errors.append(f"run provenance lacks {key}")
        elif record.get(key) != run[key]:
            errors.append(f"run provenance mismatch: {key}")
    # The engine's profile metadata carries the runner's --profile-scenario label, which differs
    # from the bare scenario in run.json. The runner always passes it, so a missing label fails closed.
    if metadata is not None:
        if metadata.get("scenario") is None:
            errors.append("profile provenance lacks scenario")
        elif metadata["scenario"] != profile_scenario_label(record):
            errors.append("profile provenance mismatch: scenario")
    for key in ("run_id", "pair_id", "mode"):
        if metadata and metadata.get(key) is not None and record.get(key) != metadata[key]:
            errors.append(f"profile provenance mismatch: {key}")
    if trace.get("format_version") != 1 or trace.get("dropped_samples") != 0:
        errors.append("trace format unsupported or frames were dropped")
    samples = trace.get("samples")
    if not isinstance(samples, list) or not samples:
        errors.append("trace has no frames")
        return result
    previous = None
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict) or not isinstance(sample.get("main_frame"), int) or isinstance(sample["main_frame"], bool):
            errors.append("trace frame identity is missing or malformed")
            return result
        if sample["main_frame"] != index + 1:
            errors.append("trace frame identities are incomplete or out of order")
        if not number(sample.get("elapsed_ms")) or not number(sample.get("delta_ms")) or sample["delta_ms"] < 0:
            errors.append("trace frame timing is invalid")
            return result
        if index and (sample["delta_ms"] == 0 or sample["elapsed_ms"] <= previous):
            errors.append("trace frame timing is not strictly advancing")
        previous = sample["elapsed_ms"]
        if not (sample.get("scenes") or {}).get("identity_tracking_complete"):
            errors.append("scene identity tracking is incomplete")
        if not isinstance(sample.get("streaming"), dict) or not isinstance(sample.get("scene_admission"), dict):
            errors.append("shared streaming or scene-admission observations unavailable")
            return result
        for field in ("cpu_spans_ms", "completion_latencies_ms"):
            values = sample.get(field)
            if not isinstance(values, dict) or any(not number(value) or value < 0 for value in values.values()):
                errors.append(f"trace {field} is malformed")
                return result
    aggregate = streaming.get("aggregate")
    if not isinstance(aggregate, dict):
        errors.append("streaming aggregate unavailable")
        return result
    failure_counts = {key: aggregate[key] for key in FAILURE_KEYS if key in aggregate}
    if "asset_load_failures" not in failure_counts or "failed_cells" not in failure_counts:
        errors.append("required functional failure counters unavailable")
    result["failure_counts"] = failure_counts
    errors.extend(f"functional failure {key}={value}" for key, value in failure_counts.items() if value != 0)
    if aggregate.get("asset_failures"):
        errors.append("asset failure details recorded")
    if any((s.get("scenes") or {}).get("lifetime_unique_failed", 0) or s["streaming"].get("lifetime_asset_load_failures", 0) for s in samples):
        errors.append("trace recorded failed scenes or model loads")
    validate_configuration(experiment, record, run, samples, errors, warnings)
    result["workload"] = validate_route(record, run, route, samples, errors, warnings)
    if validation and validation.get("functional_checks_passed") is False:
        errors.append("functional validator failed")
    if report and (report.get("renderer") or {}).get("renderer_validation_failures", 0):
        errors.append("renderer validation failures recorded")
    log_paths = [directory / name for name in ("engine.stdout.log", "engine.stderr.log")]
    for path in log_paths:
        if path.exists():
            if LOG_ERRORS.search(ANSI.sub("", path.read_text(encoding="utf-8", errors="replace"))):
                errors.append(f"runtime errors in {path.name}")
    expected = route.get("expected_startup") or (experiment.get("protocol") or {}).get("expected_startup") or EXPECTED_STARTUP
    if set(expected) != set(EXPECTED_STARTUP) or any(not isinstance(v, int) or isinstance(v, bool) or v < 0 for v in expected.values()) or expected["models"] == 0:
        errors.append("expected startup counts are malformed")
        return result
    timeline = streaming.get("timeline") or []
    initial_ms = route.get("initial_request_elapsed_ms")
    if not number(initial_ms):
        initial_ms = next((e.get("elapsed_ms") for e in timeline if e.get("stage") == "requested"
                           and str(e.get("subject", "")).startswith(("Exterior", "Interior"))
                           and number(e.get("elapsed_ms"))), None)
    if initial_ms is None:
        initial_ms = samples[0]["elapsed_ms"]
        warnings.append("initial request timestamp unavailable; readiness uses first trace frame")
    moving_start = route_value(route, samples, "movement_start_elapsed_ms")
    startup_samples = [s for s in samples if moving_start is None or s["elapsed_ms"] < moving_start]
    milestones = {}
    for percent in (25, 50, 75, 95, 100):
        threshold = math.ceil(expected["models"] * percent / 100.0)
        milestones[f"models_{percent}_percent"] = milestone(
            startup_samples, lambda s, t=threshold: s["streaming"].get("lifetime_cpu_validated_model_placements", -1) >= t, initial_ms
        )
    settled = milestone(startup_samples, lambda s: cpu_settled(s, expected), initial_ms)
    milestones["cpu_settled"] = settled
    milestones["lod_ready"] = milestone(startup_samples, lambda s:
        s["streaming"].get("resident_lod_chunks", -1) >= expected["lod_chunks"] and
        s["streaming"].get("pending_lod_chunks") == 0 and s["streaming"].get("pending_lod_queries") == 0, initial_ms)
    if settled is None:
        censored.append("initial CPU scene did not settle in the observed startup window")
    settled_ms = settled["profiler_elapsed_ms"] if settled else None
    declared_settled = route_value(route, samples, "startup_settled_elapsed_ms")
    if declared_settled is not None and (settled_ms is None or abs(declared_settled - settled_ms) > 100.0):
        warnings.append("route startup-settlement timestamp differs from shared CPU observations")
    phases, boundaries = phase_samples(samples, route, settled_ms, initial_ms)
    if record.get("scenario") in ("normal", "stress"):
        if not phases["moving"]:
            errors.append("movement window was not observed")
        if not phases["tail"]:
            errors.append("tail window was not observed")
    last = samples[-1]
    result["terminal_streaming"] = last["streaming"]
    result["terminal_admission"] = last["scene_admission"]
    result["terminal_scene_counts"] = last.get("scenes")
    pending = {key: last["streaming"].get(key) for key in EMPTY_STREAMING}
    pending.update({f"scene_{key}": last["scene_admission"].get(key) for key in EMPTY_ADMISSION})
    result["terminal_pending"] = pending
    if any(value != 0 for value in pending.values()):
        censored.append("recorded downstream work remains pending at trace end")
    result.update(expected_startup=expected, milestones=milestones, initial_request_elapsed_ms=initial_ms, route_boundaries=boundaries)
    result["phases"] = {key: frame_metrics(value, experiment["target_frame_ms"]) for key, value in phases.items()}
    coverage = cell_coverage(samples, timeline, experiment.get("_route_cell_catalog_data"))
    result["cell_coverage"] = {key: coverage_summary(value, coverage) for key, value in phases.items()}
    if coverage is None:
        warnings.append("desired-cell CPU committed coverage unavailable")
    elif result["cell_coverage"]["full"]["terminal_missing_committed_cells"]:
        censored.append("desired database-existing cells lack committed CPU data at trace end")
    spans = cpu.get("spans") or {}
    result["completion_latencies_ms"] = {name: spans[name] for name in COMPLETION_NAMES if name in spans}
    result["os_rss"] = rss_metrics(rss, run, warnings)
    result["startup_timing"] = process_startup_metrics(report, run, initial_ms, settled_ms, warnings)
    result["phase_os_rss"] = phase_rss_metrics(rss, run, report, boundaries, warnings) if result["os_rss"]["available"] else {
        "available": False, "phases": {"startup": None, "moving": None, "tail": None}}
    adaptive = [s["adaptive"] for s in samples if isinstance(s.get("adaptive"), dict)]
    if adaptive:
        result["reservation_diagnostics"] = {
            "peak_resident_plus_transient_bytes": max(s.get("resident_reserved_bytes", 0) + s.get("transient_reserved_bytes", 0) for s in adaptive),
            "peak_orphan_bytes": max(s.get("orphan_reserved_bytes", 0) for s in adaptive),
            "terminal_transient_bytes": adaptive[-1].get("transient_reserved_bytes"),
            "terminal_orphan_bytes": adaptive[-1].get("orphan_reserved_bytes"),
            "comparison_eligible": False,
        }
    metrics = result["metrics"]
    for phase, summary in result["phases"].items():
        if not summary["frame_intervals"]["count"]:
            continue
        for key in ("elapsed_seconds", "stutter_excess_ms", "stutter_excess_fraction", "stutter_excess_ms_per_minute",
                    "hitch_excess_ms", "hitch_excess_ms_per_second"):
            metrics[f"{phase}.{key}"] = summary[key]
        for key in ("p50", "p95", "p99", "max"):
            metrics[f"{phase}.frame_ms_{key}"] = summary["frame_intervals"][key]
        for threshold, hitch in summary["hitches"].items():
            metrics[f"{phase}.hitches_gt_{threshold}_ms_per_minute"] = hitch["per_minute"]
        if result["cell_coverage"].get(phase):
            for key, value in result["cell_coverage"][phase].items():
                if number(value):
                    metrics[f"{phase}.coverage.{key}"] = value
        for field, exposure in summary["pending_exposure"].items():
            for key in ("count_seconds", "mean_depth", "peak_depth", "terminal_depth"):
                if number(exposure[key]):
                    metrics[f"{phase}.pending.{field}.{key}"] = exposure[key]
    for key, value in pending.items():
        if number(value):
            metrics[f"terminal.pending.{key}"] = value
    for key in ("assets_ready", "requests_submitted", "responses_received", "unloaded_cells", "lod_chunks_ready",
                "terrain_patches_validated", "water_surfaces_validated", "origin_rebases"):
        if number(aggregate.get(key)):
            metrics[f"work_completed.{key}"] = aggregate[key]
    for name, value in milestones.items():
        if value:
            metrics[f"readiness.{name}_ms"] = value["from_initial_request_ms"]
    for key in ("process_launch_to_cpu_settled_ms", "initial_request_to_route_cpu_settled_ms"):
        if number(result["startup_timing"].get(key)):
            metrics[f"readiness.{key}"] = result["startup_timing"][key]
    for name, summary in result["completion_latencies_ms"].items():
        for key in ("mean", "p50", "p95", "p99", "worst"):
            if number(summary.get(key)):
                metrics[f"completion.{name}.{key}_ms"] = summary[key]
    if result["os_rss"]["available"]:
        metrics["rss.sampled_peak_bytes"] = result["os_rss"]["sampled_peak_bytes"]
        metrics["rss.observed_change_bytes"] = result["os_rss"]["observed_change_bytes"]
        for key in ("p50", "p95"):
            metrics[f"rss.{key}_bytes"] = result["os_rss"]["samples"][key]
    if result["phase_os_rss"]["available"]:
        for phase, summary in result["phase_os_rss"]["phases"].items():
            if summary:
                metrics[f"{phase}.rss.sampled_peak_bytes"] = summary["sampled_peak_bytes"]
                metrics[f"{phase}.rss.observed_change_bytes"] = summary["observed_change_bytes"]
                for key in ("p50", "p95"):
                    metrics[f"{phase}.rss.{key}_bytes"] = summary["samples"][key]
        for key in ("tail_end_minus_movement_start_bytes", "tail_end_minus_movement_end_bytes"):
            if number(result["phase_os_rss"].get(key)):
                metrics[f"rss.{key}"] = result["phase_os_rss"][key]
    result["errors"] = list(dict.fromkeys(errors))
    result["warnings"] = list(dict.fromkeys(warnings))
    result["readiness_complete"] = not censored and not errors
    result["valid"] = not errors
    result["comparison_eligible"] = result["valid"] and not result["analysis_exclusion"]
    return result


def bootstrap_median(values, iterations, seed):
    if len(values) < 2:
        return None
    rng = random.Random(seed)
    replicates = [statistics.median(rng.choices(values, k=len(values))) for _ in range(iterations)]
    return [percentile(replicates, 0.025), percentile(replicates, 0.975)]


def paired_summary(pairs, iterations=10000, seed=20261010):
    names = sorted({name for baseline, treatment in pairs for name in baseline["metrics"] if name in treatment["metrics"]})
    results = {}
    for name in names:
        usable = [(a["metrics"][name], b["metrics"][name], a["pair_id"])
                  for a, b in pairs if number(a["metrics"].get(name)) and number(b["metrics"].get(name))]
        deltas = [b - a for a, b, _ in usable]
        relative = [(b - a) * 100.0 / a for a, b, _ in usable if a > 0]
        metric_seed = seed + int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big")
        results[name] = {
            "paired_count": len(usable), "pair_ids": [p for _, _, p in usable],
            "baseline_median": statistics.median(a for a, _, _ in usable) if usable else None,
            "treatment_median": statistics.median(b for _, b, _ in usable) if usable else None,
            "paired_absolute_delta_median": statistics.median(deltas) if deltas else None,
            "paired_absolute_delta_ci95": bootstrap_median(deltas, iterations, metric_seed),
            "relative_paired_count": len(relative),
            "paired_relative_delta_percent_median": statistics.median(relative) if relative else None,
            "paired_relative_delta_percent_ci95": bootstrap_median(relative, iterations, metric_seed),
        }
    return results


def analysis_exclusion_description(exclusion):
    if isinstance(exclusion, dict):
        reason = exclusion.get("reason") or json.dumps(exclusion, sort_keys=True)
        scope = exclusion.get("scope")
        return f"{reason} (scope: {scope})" if scope else str(reason)
    return str(exclusion)


def analyze_experiment(experiment, comparisons=DEFAULT_COMPARISONS, iterations=10000, seed=20261010):
    if experiment.get("format_version") != 1:
        raise ValueError("unsupported experiment format_version")
    if not number(experiment.get("target_frame_ms")) or experiment["target_frame_ms"] <= 0:
        raise ValueError("target_frame_ms must be finite and positive")
    records = experiment.get("runs")
    if not isinstance(records, list):
        raise ValueError("experiment runs must be a list")
    catalog_path = experiment.get("route_cell_catalog")
    if catalog_path:
        catalog = read_json(Path(catalog_path))
        if not isinstance(catalog.get("existing_cells"), list) or any(
            not isinstance(cell, list) or len(cell) != 2 or any(not isinstance(v, int) or isinstance(v, bool) for v in cell)
            for cell in catalog["existing_cells"]
        ):
            raise ValueError("route cell catalog coordinates are malformed")
        if not number(catalog.get("cell_size_units")) or catalog["cell_size_units"] <= 0 or not isinstance(catalog.get("stream_radius"), int) or catalog["stream_radius"] < 0:
            raise ValueError("route cell catalog dimensions are malformed")
        expected_db = (experiment.get("input_hashes") or {}).get("skyrim_world.db")
        if expected_db is None or catalog.get("database_sha256") != expected_db:
            raise ValueError("route cell catalog database hash differs from campaign inputs")
        experiment = {**experiment, "_route_cell_catalog_data": catalog}
    runs = [analyze_run(experiment, record) for record in records]
    groups = {}
    for run in runs:
        key = (run["scenario"], run["pair_id"], run["mode"])
        groups.setdefault(key, []).append(run)
    for key, duplicates in groups.items():
        if key[1] is None or len(duplicates) > 1:
            for run in duplicates:
                run["errors"].append("missing pair_id or duplicate scenario/pair/mode identity")
                run["valid"] = False
                run["comparison_eligible"] = False
    summaries = []
    for scenario in sorted({run["scenario"] for run in runs}):
        for baseline, treatment in comparisons:
            relevant = [r for r in runs if r["scenario"] == scenario and r["mode"] in (baseline, treatment)]
            if not relevant:
                continue
            if {r["mode"] for r in relevant} != {baseline, treatment}:
                continue
            pairs, unpaired, excluded, excluded_details = [], [], [], []
            for pair_id in sorted({r["pair_id"] for r in relevant}, key=str):
                left = groups.get((scenario, pair_id, baseline), [])
                right = groups.get((scenario, pair_id, treatment), [])
                if len(left) != 1 or len(right) != 1:
                    unpaired.append(pair_id)
                    continue
                reasons = []
                if not left[0]["comparison_eligible"] or not right[0]["comparison_eligible"]:
                    for run in (left[0], right[0]):
                        reasons.extend(f"{run['run_id']}: {error}" for error in run["errors"])
                        if run["analysis_exclusion"]:
                            reasons.append(f"{run['run_id']}: analysis exclusion: {analysis_exclusion_description(run['analysis_exclusion'])}")
                elif (left[0]["identity"]["before"] is not None and right[0]["identity"]["before"] is not None
                      and left[0]["identity"]["before"] != right[0]["identity"]["before"]):
                    reasons.append("input hashes differ between paired runs")
                elif left[0].get("workload", {}).get("verified") and right[0].get("workload", {}).get("verified") and any(
                    left[0]["workload"].get(key) != right[0]["workload"].get(key)
                    for key in ("speed_units_per_second", "movement_seconds", "tail_seconds")
                ):
                    reasons.append("route speed or prescribed durations differ between paired runs")
                elif any(left[0]["render_configuration"].get(key) is not None and right[0]["render_configuration"].get(key) is not None
                         and left[0]["render_configuration"][key] != right[0]["render_configuration"][key]
                         for key in ("physical_resolution", "reported_build_profile")):
                    reasons.append("physical resolution or reported build profile differs between paired runs")
                elif left[0].get("workload", {}).get("verified") and right[0].get("workload", {}).get("verified") and any(
                    abs(a - b) > 0.01 for a, b in zip(left[0]["workload"]["initial_world_position"], right[0]["workload"]["initial_world_position"])
                ):
                    reasons.append("initial camera position differs between paired runs")
                if reasons:
                    excluded.append(pair_id)
                    excluded_details.append({
                        "pair_id": pair_id, "run_ids": [left[0]["run_id"], right[0]["run_id"]], "reasons": reasons,
                        "analysis_exclusions": {run["run_id"]: run["analysis_exclusion"] for run in (left[0], right[0]) if run["analysis_exclusion"]},
                    })
                else:
                    pairs.append((left[0], right[0]))
            summaries.append({
                "scenario": scenario, "baseline_mode": baseline, "treatment_mode": treatment,
                "paired_count": len(pairs), "unpaired_pair_ids": unpaired, "excluded_pair_ids": excluded,
                "excluded_pairs": excluded_details,
                "metrics": paired_summary(pairs, iterations, seed),
            })
    return {
        "format_version": 1, "target_frame_ms": experiment["target_frame_ms"],
        "input_hashes": experiment.get("input_hashes"), "protocol": experiment.get("protocol"),
        "bootstrap": {"iterations": iterations, "seed": seed, "unit": "matched run pair", "statistic": "median paired difference", "interval": "percentile 95%"},
        "definitions": {
            "quantiles": "frame/RSS linear interpolation at (n-1)*q; completion summaries preserve engine quantiles",
            "readiness_epoch": "first cell request and process launch reported separately; end-to-end timing uses report wall clock minus elapsed time since route CPU settlement",
            "hitches": "strictly greater than 33.33, 50 or 100 ms; rates use sum of positive Time<Real> intervals",
            "stutter": "sum(max(delta_ms-target_frame_ms, 0)); includes waits and baseline rendering above the target",
            "phase_boundary": "moving/tail use preregistered ending-phase assignment; moving_prorated/tail_prorated use exact real-clock boundary sensitivity; both keep full-delta quantiles",
            "method_history": "ending-phase protocol and prorated implementation both existed before the main campaign; four smoke artifacts validated the schema, including exact 10 s prorated movement windows",
            "tail_completion_clock": "completion uses the final trace snapshot's own Time<Real> clock conversion; later report snapshots recompute the profiler offset and cannot be compared directly with earlier trace elapsed_ms",
            "comparison_eligibility": "valid means workload/functional checks pass; comparison_eligible also requires no manifest analysis_exclusion; excluded runs and their measurements remain in the report",
            "delta_sign": "treatment minus baseline; a negative value means smaller, not automatically better for every metric",
        },
        "runs": runs, "comparisons": summaries, "limitations": LIMITATIONS,
    }


def display(value, digits=2):
    return "unavailable" if value is None else f"{value:,.{digits}f}"


def report_markdown(analysis):
    lines = ["> Written with an AI assistant; awaiting human review.", "", "Streaming comparison", "",
             f"Matched-run analysis; {analysis['bootstrap']['iterations']:,} bootstrap resamples per metric. Negative deltas mean treatment values are smaller.", "",
             "| Run | Mode | Scenario | Valid | Comparison eligible | Recorded tail ready | Launch → CPU ms | Moving p95 ms | Sampled RSS GiB |",
             "| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: |"]
    for run in analysis["runs"]:
        metrics = run["metrics"]
        rss = metrics.get("rss.sampled_peak_bytes")
        lines.append(f"| {run['run_id']} | {run['mode']} | {run['scenario']} | {'yes' if run['valid'] else 'no'} | {'yes' if run['comparison_eligible'] else 'no'} | {'yes' if run.get('readiness_complete') else 'no'} | {display(metrics.get('readiness.process_launch_to_cpu_settled_ms'))} | {display(metrics.get('moving.frame_ms_p95'))} | {display(rss / 1024**3 if rss is not None else None, 3)} |")
    selected = (
        "readiness.process_launch_to_cpu_settled_ms", "readiness.initial_request_to_route_cpu_settled_ms",
        "readiness.cpu_settled_ms", "readiness.lod_ready_ms", "readiness.models_50_percent_ms", "readiness.models_95_percent_ms",
        "startup.frame_ms_p95", "startup.frame_ms_p99", "startup.frame_ms_max", "startup.stutter_excess_ms_per_minute",
        "moving.frame_ms_p95", "moving.frame_ms_p99", "moving.frame_ms_max", "moving.hitch_excess_ms_per_second", "moving.stutter_excess_ms_per_minute",
        "moving_prorated.hitch_excess_ms_per_second",
        "moving.hitches_gt_33.33_ms_per_minute", "moving.hitches_gt_50_ms_per_minute", "moving.hitches_gt_100_ms_per_minute",
        "moving.coverage.missing_committed_cell_seconds", "moving.coverage.mean_missing_committed_cells",
        "moving.pending.pending_model_placements.count_seconds", "tail.pending.pending_model_placements.count_seconds",
        "tail.coverage.terminal_missing_committed_cells", "tail.frame_ms_p95", "moving.rss.sampled_peak_bytes",
        "tail.rss.sampled_peak_bytes", "rss.tail_end_minus_movement_end_bytes", "rss.sampled_peak_bytes",
    )
    for comparison in analysis["comparisons"]:
        lines += ["", f"{comparison['scenario']}: {comparison['baseline_mode']} → {comparison['treatment_mode']}; {comparison['paired_count']} usable pairs."]
        if comparison["unpaired_pair_ids"] or comparison["excluded_pair_ids"]:
            lines.append(f"Unpaired: {comparison['unpaired_pair_ids']}; excluded: {comparison['excluded_pair_ids']}.")
        for exclusion in comparison["excluded_pairs"]:
            lines.append(f"Excluded pair {exclusion['pair_id']}: {'; '.join(exclusion['reasons'])}.")
        if not comparison["metrics"]:
            continue
        lines += ["", "| Metric | Pairs | Baseline median | Treatment median | Paired delta | 95% CI | Relative delta |",
                  "| --- | ---: | ---: | ---: | ---: | --- | ---: |"]
        for name in selected:
            value = comparison["metrics"].get(name)
            if value is None:
                continue
            interval = value["paired_absolute_delta_ci95"]
            ci = "unavailable" if interval is None else f"[{display(interval[0])}, {display(interval[1])}]"
            relative = value["paired_relative_delta_percent_median"]
            lines.append(f"| {name} | {value['paired_count']} | {display(value['baseline_median'])} | {display(value['treatment_median'])} | {display(value['paired_absolute_delta_median'])} | {ci} | {display(relative)}% |")
    details = [r for r in analysis["runs"] if r["errors"] or r["censored"] or r["warnings"] or r["analysis_exclusion"]]
    if details:
        lines += ["", "Run limits and exclusions:", ""]
        for run in details:
            notes = run["errors"] + run["censored"] + run["warnings"]
            if run["analysis_exclusion"]:
                notes = notes + [f"analysis exclusion: {analysis_exclusion_description(run['analysis_exclusion'])}"]
            lines.append(f"- {run['run_id']}: {'; '.join(notes)}.")
    lines += ["", analysis["definitions"]["phase_boundary"] + ".", analysis["definitions"]["method_history"] + ".",
              "", "Interpretation limits:", ""] + [f"- {note}" for note in analysis["limitations"]]
    lines += ["", "Full per-run phase metrics, completion latencies and all paired effects are in `analysis.json`.", ""]
    return "\n".join(lines)


def atomic_write(path, text):
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(text)
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--experiment", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--bootstrap-samples", type=int, default=10000)
    parser.add_argument("--seed", type=int, default=20261010)
    parser.add_argument("--comparison", action="append", help="BASELINE:TREATMENT; repeat for multiple contrasts")
    args = parser.parse_args(argv)
    if args.bootstrap_samples < 1:
        parser.error("--bootstrap-samples must be positive")
    comparisons = DEFAULT_COMPARISONS
    if args.comparison:
        comparisons = []
        for value in args.comparison:
            names = value.split(":")
            if len(names) != 2 or not all(names) or names[0] == names[1]:
                parser.error("--comparison requires two distinct modes separated by a colon")
            comparisons.append(tuple(names))
    try:
        analysis = analyze_experiment(read_json(args.experiment), comparisons, args.bootstrap_samples, args.seed)
        analysis["experiment_path"] = str(args.experiment.resolve())
        args.output_dir.mkdir(parents=True, exist_ok=True)
        atomic_write(args.output_dir / "analysis.json", json.dumps(analysis, indent=2, allow_nan=False) + "\n")
        atomic_write(args.output_dir / "report.md", report_markdown(analysis))
    except (OSError, ValueError, TypeError, KeyError) as error:
        print(f"analysis failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps({"analysis": str(args.output_dir / "analysis.json"), "report": str(args.output_dir / "report.md"),
                      "valid_runs": sum(r["valid"] for r in analysis["runs"]), "total_runs": len(analysis["runs"])}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
