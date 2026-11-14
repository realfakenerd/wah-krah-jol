#!/usr/bin/env python3
"""Capture repeated Riverwood startups; counters never establish pixel coverage."""

import argparse
from contextlib import contextmanager
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shlex
import signal
import struct
import subprocess
import sys
import time
from datetime import datetime, timezone
import zlib


INPUT_FILES = (
    "skyrim_world.db",
    "cell_cache.rkyv",
    "conversion-manifest.json",
    "integration-report.json",
    "lod-manifest.json",
)
SAFE_ENVIRONMENT = (
    "DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "PATH", "LD_LIBRARY_PATH",
    "VK_ICD_FILENAMES", "VK_DRIVER_FILES", "WGPU_BACKEND", "WINIT_UNIX_BACKEND",
    "RUST_LOG", "MUDCRAB_PROFILE_FREEZE_CAMERA", "MUDCRAB_PROFILE_ALWAYS_ON_TOP",
)
CAMERA_OFFSET = [0.0, 6000.0, 8000.0]
SCENE_COUNTS = {"resident_cells": 25, "assets_ready": 2029, "terrain_patches_validated": 100}
# Engine exit code for a lost GPU device (crates/engine/src/app.rs, EXIT_DEVICE_LOST). Such a run
# measured nothing and is worth retrying, but repeated losses can still be the engine's fault.
EXIT_DEVICE_LOST = 3
EMPTY_QUEUES = (
    "loading_cells", "active_requests", "arming_queue_depth", "pending_asset_instances",
    "pending_surface_instances", "pending_lod_queries", "pending_lod_chunks", "retiring_cells",
)
FAILURE_COUNTS = (
    "failed_cells", "asset_load_failures", "material_validation_failures",
    "terrain_validation_failures", "water_validation_failures",
    "transform_bounds_validation_failures", "failed_lod_queries", "failed_lod_chunks",
    "unrecovered_lod_queries", "unrecovered_lod_chunks", "lod_query_submission_failures",
    "streaming_invariant_failures", "streaming_fixture_failures", "physics_fixture_failures",
    "diagnostic_fallbacks", "duplicate_cell_roots", "orphaned_cell_roots",
    "missing_cell_roots", "out_of_range_cell_roots", "retire_backlog_overflows",
    "stale_responses", "stale_lod_query_responses", "unloaded_cells", "origin_rebases",
)
ANSI = re.compile(r"\x1b\[[0-9;]*m")
NUMBER = r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?"
CAMERA = re.compile(
    rf"camera=Vec3\(({NUMBER}),\s*({NUMBER}),\s*({NUMBER})\).*"
    rf"target=Vec3\(({NUMBER}),\s*({NUMBER}),\s*({NUMBER})\)"
)
RETRY_MESSAGES = (
    "late mesh preparation retry drain", "late mesh preparation observed for next frame",
    "late shadow preparation observed for next frame",
)
KNOWN_CURSOR_WARNING = "[gamescope] [Error] xwm: NO CURSOR IMPL XDG"


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def interrupt_campaign(_signal, _frame):
    raise KeyboardInterrupt


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def input_hashes(engine, assets):
    return {"engine": sha256(engine), **{name: sha256(assets / name) for name in INPUT_FILES}}


def read_png_dimensions(path):
    """Check chunks, CRCs, and the decompressed scanline lengths and filter bytes."""
    with path.open("rb") as stream:
        if stream.read(8) != b"\x89PNG\r\n\x1a\n":
            raise ValueError("invalid PNG signature")
        dimensions = None
        image_data = False
        compressed = []
        compressed_size = 0
        header_data = None
        while True:
            header = stream.read(8)
            if len(header) != 8:
                raise ValueError("PNG is truncated before IEND")
            size, kind = struct.unpack(">I4s", header)
            if size > 128 * 1024 * 1024:
                raise ValueError("PNG chunk is unreasonably large")
            payload = stream.read(size)
            checksum = stream.read(4)
            if len(payload) != size or len(checksum) != 4:
                raise ValueError("PNG chunk is truncated")
            if zlib.crc32(kind + payload) & 0xFFFFFFFF != struct.unpack(">I", checksum)[0]:
                raise ValueError("PNG chunk CRC mismatch")
            if dimensions is None:
                if kind != b"IHDR" or size != 13:
                    raise ValueError("PNG must start with IHDR")
                dimensions = list(struct.unpack(">II", payload[:8]))
                if min(dimensions) <= 0:
                    raise ValueError("PNG has zero dimensions")
                header_data = payload
            elif kind == b"IHDR":
                raise ValueError("PNG has multiple headers")
            if kind == b"IDAT":
                image_data = image_data or bool(payload)
                compressed_size += len(payload)
                if compressed_size > 128 * 1024 * 1024:
                    raise ValueError("PNG compressed image exceeds the validation limit")
                compressed.append(payload)
            if kind == b"IEND":
                if size != 0 or not image_data or stream.read(1):
                    raise ValueError("PNG has an invalid end or no image data")
                validate_png_scanlines(header_data, b"".join(compressed))
                return dimensions


def validate_png_scanlines(header, compressed):
    width, height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", header)
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}.get(color)
    depths = {0: (1, 2, 4, 8, 16), 2: (8, 16), 3: (1, 2, 4, 8), 4: (8, 16), 6: (8, 16)}
    if channels is None or depth not in depths[color] or compression != 0 or filtering != 0 or interlace not in (0, 1):
        raise ValueError("PNG header uses an invalid encoding")
    passes = [(0, 0, 1, 1)] if interlace == 0 else [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
    rows = []
    for x, y, dx, dy in passes:
        columns = max(0, (width - x + dx - 1) // dx)
        count = max(0, (height - y + dy - 1) // dy)
        if columns and count:
            rows.append((1 + (columns * channels * depth + 7) // 8, count, columns))
    expected = sum(length * count for length, count, _ in rows)
    if expected > 512 * 1024 * 1024:
        raise ValueError("PNG decoded image exceeds the validation limit")
    decoder = zlib.decompressobj()
    try:
        data = decoder.decompress(compressed, expected + 1)
    except zlib.error as error:
        raise ValueError(f"PNG image data cannot decompress: {error}") from error
    if len(data) != expected or not decoder.eof or decoder.unused_data or decoder.unconsumed_tail:
        raise ValueError("PNG decompressed image length or stream ending is invalid")
    offset = 0
    # Palette indices need PLTE/tRNS decoding; structural validation still supports them.
    visible = color == 3
    bytes_per_pixel = max(1, (channels * depth + 7) // 8)
    for length, count, columns in rows:
        previous = bytearray(length - 1)
        for _ in range(count):
            filter_type = data[offset]
            if filter_type > 4:
                raise ValueError("PNG scanline filter is invalid")
            row = bytearray(data[offset + 1:offset + length])
            offset += length
            if visible:
                continue
            if filter_type and (any(row) or any(previous)):
                for i in range(len(row)):
                    left = row[i - bytes_per_pixel] if i >= bytes_per_pixel else 0
                    up = previous[i]
                    upper_left = previous[i - bytes_per_pixel] if i >= bytes_per_pixel else 0
                    if filter_type == 1:
                        predictor = left
                    elif filter_type == 2:
                        predictor = up
                    elif filter_type == 3:
                        predictor = (left + up) // 2
                    else:
                        estimate = left + up - upper_left
                        distances = (abs(estimate - left), abs(estimate - up), abs(estimate - upper_left))
                        predictor = (left, up, upper_left)[distances.index(min(distances))]
                    row[i] = (row[i] + predictor) & 255
            previous = row
            if depth < 8:
                mask = (1 << depth) - 1
                visible = any((row[i * depth // 8] >> (8 - depth - i * depth % 8)) & mask for i in range(columns))
            elif color in (0, 2):
                visible = any(row)
            else:
                sample_bytes = depth // 8
                stride = channels * sample_bytes
                color_bytes = (channels - 1) * sample_bytes
                visible = any(any(row[i:i + color_bytes]) and any(row[i + color_bytes:i + stride])
                              for i in range(0, len(row), stride))
    if not visible:
        raise ValueError("PNG has no visible nonblack pixels")


def recorded_option(manifest, option, fallback=None):
    command = manifest.get("command", [])
    indices = [index for index, argument in enumerate(command[:-1]) if argument == option]
    return command[indices[-1] + 1] if indices else fallback


def load_json(path, failures):
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(value, dict):
            raise ValueError("expected a JSON object")
        return value
    except (OSError, ValueError) as error:
        failures.append(f"{path.name}: {error}")
        return {}


def finite_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def object_field(document, key, failures, label):
    value = document.get(key, {})
    if not isinstance(value, dict):
        failures.append(f"{label} is not a JSON object")
        return {}
    return value


def retry_accounting(logs, failures):
    records = []
    for line in logs.splitlines():
        if not any(message in line for message in RETRY_MESSAGES):
            continue
        fields = {key: int(value) for key, value in re.findall(r"\b(\w+)=(\d+)\b", line)}
        if not all(key in fields for key in ("tracked_total", "resumed_total", "canceled_total", "pending")):
            failures.append("retry log has incomplete accounting fields")
            continue
        if fields["tracked_total"] != fields["resumed_total"] + fields["canceled_total"] + fields["pending"]:
            failures.append("retry accounting does not conserve tracked entities")
        records.append(fields)
    if not records:
        return {"status": "not_observed", "note": "Retry path untriggered or retry logs unavailable; inspect the raw logs."}
    records.sort(key=lambda item: (item.get("render_frame", 0), item["tracked_total"], item["resumed_total"] + item["canceled_total"]))
    last = records[-1]
    if last["pending"] != 0:
        failures.append(f"retry records remain pending: {last['pending']}")
    return {"status": "observed", "record_count": len(records), "last": last}


def analyze_run(directory, manifest):
    failures = []
    notes = ["Visual inspection pending: lifecycle counters and PNG validity do not prove terrain pixel coverage."]
    device_lost = manifest.get("exit_code") == EXIT_DEVICE_LOST
    if device_lost:
        failures.append(f"GPU device lost (engine exit code {EXIT_DEVICE_LOST}); retry the run")
    elif manifest.get("exit_code") != 0 or manifest.get("timed_out") or manifest.get("interrupted"):
        failures.append("engine did not complete successfully")
    if manifest.get("input_hashes_before") != manifest.get("input_hashes_after"):
        failures.append("engine or primary asset inputs changed during the run")
    report = load_json(directory / "report.json", failures)
    profile = load_json(directory / "profile" / "streaming.json", failures)
    metadata = load_json(directory / "profile" / "metadata.json", failures)
    cpu = load_json(directory / "profile" / "cpu-spans.json", failures)
    aggregate = object_field(profile, "aggregate", failures, "streaming aggregate")
    if report.get("passed") is not True:
        failures.append("engine acceptance report did not pass its functional smoke gates")
    if report.get("warmup_frames") != manifest["warmup_frames"]:
        failures.append("report warmup differs from the requested warmup")
    if not finite_number(report.get("frames")) or report.get("frames", 0) <= 0:
        failures.append("no measured frames recorded")
    requested_duration = manifest.get("duration_seconds")
    if not finite_number(requested_duration) or not finite_number(report.get("elapsed_seconds")) or report["elapsed_seconds"] + 0.001 < requested_duration:
        failures.append("measured duration is shorter than the requested duration or is missing")
    expected_scenario = manifest.get("scenario", recorded_option(manifest, "--profile-scenario", "repeat-terrain-startup"))
    expected_commit = manifest.get("commit", recorded_option(manifest, "--profile-commit", "unknown"))
    for key, expected in (("run_id", manifest.get("run_id")), ("scenario", expected_scenario), ("commit", expected_commit)):
        if metadata.get(key) != expected or expected is None:
            failures.append(f"profile provenance mismatch: {key}")
    if report.get("scenario") != expected_scenario:
        failures.append("report scenario differs from the recorded invocation")
    for key, expected in SCENE_COUNTS.items():
        if aggregate.get(key) != expected:
            failures.append(f"{key}: expected {expected}, got {aggregate.get(key)!r}")
    for key in EMPTY_QUEUES + FAILURE_COUNTS:
        if aggregate.get(key) != 0:
            failures.append(f"{key}: expected zero, got {aggregate.get(key)!r}")
    report_streaming = object_field(report, "streaming", failures, "report streaming")
    for key in tuple(SCENE_COUNTS) + EMPTY_QUEUES + FAILURE_COUNTS:
        if report_streaming.get(key) != aggregate.get(key):
            failures.append(f"report/profile streaming mismatch: {key}")
    if aggregate.get("asset_failures") != []:
        failures.append("asset failure details are present or missing")
    if aggregate.get("resident_roots") != 25:
        failures.append("resident cell-root count differs from 25")
    for submitted, completed in (
        ("requests_submitted", "responses_received"),
        ("lod_queries_submitted", "lod_query_responses"),
        ("lod_chunks_requested", "lod_chunks_ready"),
    ):
        if not finite_number(aggregate.get(submitted)) or aggregate.get(submitted) != aggregate.get(completed):
            failures.append(f"lifecycle mismatch: {submitted} versus {completed}")
    if not finite_number(aggregate.get("lod_chunks_ready")) or aggregate.get("lod_chunks_ready", 0) <= 0:
        failures.append("no ready LOD chunks recorded")
    gauges = object_field(cpu, "gauges", failures, "CPU gauges")
    for key, value in gauges.items():
        if ("/pending_" in key or key in ("streaming/arming_queue_depth", "streaming/active_requests", "streaming/loading_cells", "streaming/retiring_cells")) and value != 0:
            failures.append(f"final gauge {key} is not zero: {value!r}")
    if object_field(report, "renderer", failures, "report renderer").get("renderer_validation_failures") != 0:
        failures.append("renderer validation failures are nonzero or missing")
    for key, expected in (("worldspace_id", 60), ("start_grid", [5, -12]), ("stream_radius", 2)):
        if metadata.get(key) != expected:
            failures.append(f"profile {key} differs from the requested camera/scene")
    timeline = profile.get("timeline", [])
    if not isinstance(timeline, list):
        failures.append("streaming timeline is not a JSON array")
        timeline = []
    last_frame = 0
    for stage, expected in (("committed", 25), ("asset_ready", 2029), ("ready", aggregate.get("lod_chunks_ready"))):
        events = [event for event in timeline if isinstance(event, dict) and event.get("stage") == stage]
        if len(events) != expected:
            failures.append(f"timeline {stage}: expected {expected} events, got {len(events)}")
        for event in events:
            if not finite_number(event.get("frame")) or event["frame"] < 0:
                failures.append(f"timeline {stage} has an invalid frame")
            else:
                last_frame = max(last_frame, event["frame"])
    for event in timeline:
        if isinstance(event, dict) and event.get("stage") in ("failed", "unloaded"):
            failures.append(f"stationary timeline contains {event['stage']}")
        if isinstance(event, dict) and event.get("stage") == "initial_uploads_drained":
            if finite_number(event.get("frame")):
                last_frame = max(last_frame, event["frame"])
            else:
                failures.append("terrain upload-drain event has an invalid frame")
    if last_frame > manifest["warmup_frames"]:
        failures.append(f"streaming timeline settled after warmup: frame {last_frame}")
    notes.append("Warmup settlement covers recorded cell/model/LOD timeline events; final surface and upload gauges have no historical timestamps.")
    notes.append("The runtime log proves the initial camera offset; the final image pose is not independently recorded. Keep the graphics session free of camera input.")
    logs = ""
    for name in ("engine.stdout.log", "engine.stderr.log"):
        try:
            logs += ANSI.sub("", (directory / name).read_text(encoding="utf-8", errors="replace")) + "\n"
        except OSError as error:
            failures.append(f"{name}: {error}")
    error_logs = []
    for line in logs.splitlines():
        if line.strip() == KNOWN_CURSOR_WARNING:
            if not any(KNOWN_CURSOR_WARNING in note for note in notes):
                notes.append(f"Known compositor cursor warning retained in raw logs: {KNOWN_CURSOR_WARNING}")
        else:
            error_logs.append(line)
    if re.search(r"\bERROR\b|thread .*panicked|Validation Error|Shader compilation error", "\n".join(error_logs), re.IGNORECASE):
        failures.append("raw logs contain an error, panic, validation error or shader compilation error")
    camera_matches = CAMERA.findall(logs)
    if len(camera_matches) != 1:
        failures.append(f"expected one runtime camera record, got {len(camera_matches)}")
        camera = None
    else:
        camera = [float(value) for value in camera_matches[0]]
        offset = [camera[index] - camera[index + 3] for index in range(3)]
        if not all(math.isclose(value, expected, abs_tol=0.1) for value, expected in zip(offset, CAMERA_OFFSET)):
            failures.append(f"runtime camera offset is incorrect: {offset}")
        if not math.isclose(camera[3], 2048.0, abs_tol=0.1) or not math.isclose(camera[5], -2048.0, abs_tol=0.1):
            failures.append("runtime camera target differs from the expected cell center")
    retry = retry_accounting(logs, failures)
    if retry["status"] == "observed" and retry["last"].get("render_frame", 0) > manifest["warmup_frames"]:
        failures.append("mesh preparation retries were still changing after warmup")
    try:
        dimensions = read_png_dimensions(directory / "frame.png")
        window = object_field(metadata, "window", failures, "metadata window")
        expected_dimensions = window.get("physical_resolution", metadata.get("resolution"))
        if "physical_resolution" not in window:
            notes.append("PNG dimensions match the metadata resolution fallback; that field records the requested resolution, not an independent physical surface measurement.")
        if dimensions != expected_dimensions:
            failures.append(f"PNG dimensions {dimensions} differ from recorded surface {expected_dimensions!r}")
    except (OSError, ValueError) as error:
        dimensions = None
        failures.append(f"frame.png: {error}")
    try:
        with (directory / "frames.csv").open(newline="", encoding="utf-8") as stream:
            rows = list(csv.DictReader(stream))
        if not rows or len(rows) != report.get("frames"):
            failures.append("frame-time CSV count differs from the acceptance report")
    except OSError as error:
        failures.append(f"frames.csv: {error}")
    return {
        "functional_checks_passed": not failures,
        "device_lost": device_lost,
        "visual_inspection": "pending",
        "failures": list(dict.fromkeys(failures)),
        "notes": notes,
        "scene_counts": {key: aggregate.get(key) for key in SCENE_COUNTS},
        "last_streaming_event_frame": last_frame,
        "initial_camera_record": camera,
        "png_dimensions": dimensions,
        "retry_accounting": retry,
    }


@contextmanager
def defer_campaign_interrupts(mark_interrupted):
    """Finish owned-child cleanup and artifact writes after repeated stop signals."""
    previous = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    for sig in previous:
        signal.signal(sig, lambda _sig, _frame: mark_interrupted())
    try:
        yield
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def stop_owned_group(process):
    """Reap the leader and stop only its original, separately launched process group."""
    errors = []
    process.poll()
    for sig, timeout in ((signal.SIGTERM, 5), (signal.SIGKILL, 2)):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(f"owned group signal {sig}: {error}")
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            if sig == signal.SIGKILL:
                errors.append("owned engine did not exit before cleanup deadline")
        process.poll()
    return errors


def engine_command(args, directory, budget, run_id):
    return shlex.split(args.launch_prefix) + [
        str(args.engine), "--assets", str(args.assets), "--worldspace", "0x3c",
        "--grid-x", "5", "--grid-y", "-12", "--stream-radius", "2",
        "--max-upload-mib-per-frame", str(budget), "--auto-fly-speed", "0",
        "--benchmark-duration", str(args.duration), "--benchmark-warmup-frames", str(args.warmup),
        "--benchmark-output", str(directory / "report.json"),
        "--benchmark-frame-times", str(directory / "frames.csv"),
        "--profile-output", str(directory / "profile"),
        "--profile-scenario", "repeat-terrain-startup", "--profile-run-id", run_id,
        "--profile-commit", args.commit, "--accept-min-fps", "0", "--accept-p95-ms", "1000000",
        "--accept-max-memory-growth-gib", "1000", "--acceptance-screenshot", str(directory / "frame.png"),
        "--screenshot-camera-offset", "0,6000,8000",
    ]


def interrupted_validation(reason):
    """Record incomplete analysis without implying functional or visual acceptance."""
    return {
        "functional_checks_passed": False, "visual_inspection": "pending",
        "failures": [reason], "notes": [],
        "retry_accounting": {"status": "not_observed"},
    }


def run_one(args, directory, budget, run_id, environment):
    directory.mkdir()
    command = engine_command(args, directory, budget, run_id)
    manifest = {
        "format_version": 1, "run_id": run_id, "started_utc": utc_now(), "ended_utc": None,
        "engine": str(args.engine), "assets": str(args.assets), "command": command,
        "cwd": str(directory), "upload_budget_mib": budget, "warmup_frames": args.warmup,
        "duration_seconds": args.duration, "timeout_seconds": args.timeout,
        "worldspace": 60, "start_grid": [5, -12], "stream_radius": 2,
        "camera_offset": CAMERA_OFFSET, "environment": {key: environment[key] for key in SAFE_ENVIRONMENT if key in environment},
        "commit": args.commit, "scenario": "repeat-terrain-startup",
        "input_hashes_before": None, "input_hashes_after": None,
        "hash_scope": "engine plus five primary asset inputs; mesh/texture payload files are not individually hashed",
        "exit_code": None, "timed_out": False, "interrupted": False,
        "cache_policy": "fresh process; operating-system caches preserved",
        "purpose": "functional startup and visual capture; no performance comparison",
    }
    write_json(directory / "run.json", manifest)
    started = time.monotonic()
    process = None
    try:
        manifest["input_hashes_before"] = input_hashes(args.engine, args.assets)
        with (directory / "engine.stdout.log").open("wb") as stdout, (directory / "engine.stderr.log").open("wb") as stderr:
            process = subprocess.Popen(command, cwd=directory, env=environment, stdout=stdout, stderr=stderr, start_new_session=True)
            manifest["owned_process_group"] = process.pid
            write_json(directory / "run.json", manifest)
            try:
                manifest["exit_code"] = process.wait(timeout=args.timeout)
            except subprocess.TimeoutExpired:
                manifest["timed_out"] = True
    except KeyboardInterrupt:
        manifest["interrupted"] = True
    except OSError as error:
        manifest["run_error"] = str(error)
    finally:
        def mark_interrupted():
            manifest["interrupted"] = True

        with defer_campaign_interrupts(mark_interrupted):
            if process is not None:
                try:
                    manifest["cleanup_errors"] = stop_owned_group(process)
                except KeyboardInterrupt:
                    # Also retain evidence if a caller-injected interruption reaches cleanup.
                    mark_interrupted()
                    manifest["cleanup_errors"] = stop_owned_group(process)
                manifest["exit_code"] = process.returncode
            manifest["ended_utc"] = utc_now()
            manifest["wall_seconds"] = time.monotonic() - started
            try:
                manifest["input_hashes_after"] = input_hashes(args.engine, args.assets)
            except KeyboardInterrupt:
                mark_interrupted()
                manifest["input_hash_error"] = "input hashing interrupted"
            except OSError as error:
                manifest["input_hash_error"] = str(error)
            write_json(directory / "run.json", manifest)
            try:
                result = analyze_run(directory, manifest)
            except KeyboardInterrupt:
                mark_interrupted()
                result = interrupted_validation("run analysis interrupted")
            if manifest["interrupted"]:
                result["functional_checks_passed"] = False
                result["failures"].append("campaign interrupted")
            if manifest.get("cleanup_errors"):
                result["functional_checks_passed"] = False
                result["failures"].extend(manifest["cleanup_errors"])
            write_json(directory / "run.json", manifest)
            write_json(directory / "validation.json", result)
    return {"run_id": run_id, "upload_budget_mib": budget, **result}, manifest["interrupted"]


def positive_int(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def positive_float(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repeats", type=positive_int, default=10, help="fresh launches per upload budget")
    parser.add_argument("--upload-budgets", default="16,1", help="comma-separated positive MiB budgets")
    parser.add_argument("--warmup", type=positive_int, default=1200)
    parser.add_argument("--duration", type=positive_float, default=30)
    parser.add_argument("--timeout", type=positive_float, default=180)
    parser.add_argument("--launch-prefix", default="", help="argv prefix parsed with shlex; never run through a shell")
    parser.add_argument("--commit", default="unknown", help="source revision supplied by the binary's builder")
    args = parser.parse_args(argv)
    try:
        args.budgets = [positive_int(value.strip()) for value in args.upload_budgets.split(",")]
        if len(set(args.budgets)) != len(args.budgets):
            raise ValueError("upload budgets must be distinct")
        shlex.split(args.launch_prefix)
    except (ValueError, argparse.ArgumentTypeError) as error:
        parser.error(str(error))
    args.engine, args.assets, args.output = (path.expanduser().resolve() for path in (args.engine, args.assets, args.output))
    return args


def main(argv=None):
    args = parse_args(argv)
    if os.name != "posix":
        raise SystemExit("This runner requires POSIX process groups (Linux or macOS).")
    if not args.engine.is_file() or not os.access(args.engine, os.X_OK):
        raise SystemExit("--engine must name an executable file")
    try:
        args.output.mkdir(parents=True, exist_ok=False)
    except OSError as error:
        raise SystemExit(str(error)) from error
    environment = os.environ.copy()
    environment.setdefault("RUST_LOG", "info")
    summary = {
        "format_version": 1, "started_utc": utc_now(), "ended_utc": None,
        "planned_launches": args.repeats * len(args.budgets), "runs": [],
        "baseline_input_hashes": None, "functional_checks_passed": False,
        "visual_inspection": "pending", "performance_claim": "none; repeated functional captures",
        "interrupted": False,
    }
    write_json(args.output / "summary.json", summary)
    previous_term = signal.signal(signal.SIGTERM, interrupt_campaign)
    interrupted = False
    current_result = None
    current_directory = None
    try:
        baseline = input_hashes(args.engine, args.assets)
        summary["baseline_input_hashes"] = baseline
        for budget in args.budgets:
            for repeat in range(1, args.repeats + 1):
                run_id = f"{budget}mib-{repeat:02d}"
                current_result = None
                current_directory = args.output / run_id
                result, interrupted = run_one(args, current_directory, budget, run_id, environment)
                current_result = result
                # Retain completed run evidence before the next interruptible hash operation.
                summary["runs"].append(result)
                if not interrupted and input_hashes(args.engine, args.assets) != baseline:
                    result["functional_checks_passed"] = False
                    result["failures"].append("inputs differ from the campaign baseline")
                    write_json(args.output / run_id / "validation.json", {key: value for key, value in result.items() if key not in ("run_id", "upload_budget_mib")})
                write_json(args.output / "summary.json", summary)
                print(json.dumps({"run_id": run_id, "functional_checks_passed": result["functional_checks_passed"], "failures": result["failures"], "retry_status": result["retry_accounting"]["status"]}), flush=True)
                if interrupted:
                    break
            if interrupted:
                break
    except KeyboardInterrupt:
        interrupted = True
        if current_result is not None:
            current_result["functional_checks_passed"] = False
            current_result["failures"].append("campaign input verification interrupted")
    except OSError as error:
        summary["campaign_error"] = str(error)
    finally:
        def mark_interrupted():
            summary["interrupted"] = True
            summary["functional_checks_passed"] = False

        with defer_campaign_interrupts(mark_interrupted):
            summary["interrupted"] = interrupted
            summary["ended_utc"] = utc_now()
            if interrupted and current_result is not None:
                write_json(current_directory / "validation.json", {key: value for key, value in current_result.items() if key not in ("run_id", "upload_budget_mib")})
            summary["functional_checks_passed"] = (
                not summary["interrupted"] and "campaign_error" not in summary
                and len(summary["runs"]) == summary["planned_launches"]
                and all(run["functional_checks_passed"] for run in summary["runs"])
            )
            write_json(args.output / "summary.json", summary)
        signal.signal(signal.SIGTERM, previous_term)
    return 130 if summary["interrupted"] else (0 if summary["functional_checks_passed"] else 1)


if __name__ == "__main__":
    sys.exit(main())
