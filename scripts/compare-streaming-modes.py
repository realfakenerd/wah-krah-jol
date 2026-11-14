#!/usr/bin/env python3
"""Run matched native streaming comparisons without clearing host caches."""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import random
import signal
import sqlite3
import subprocess
import threading
import time


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")
    temporary.replace(path)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def host_snapshot(thermal_probe):
    commands = {
        "power": ["/usr/bin/pmset", "-g", "batt"],
        "thermal_warnings": ["/usr/bin/pmset", "-g", "therm"],
        "swap": ["/usr/sbin/sysctl", "vm.swapusage"],
    }
    if thermal_probe:
        commands["thermal_state"] = [str(thermal_probe)]
    value = {"utc_epoch": time.time(), "monotonic": time.monotonic(),
             "load_average": list(os.getloadavg())}
    for name, command in commands.items():
        result = subprocess.run(command, capture_output=True, text=True, timeout=5)
        value[name] = {"returncode": result.returncode, "output": result.stdout.strip()}
    return value


def game_processes():
    result = subprocess.run(["/bin/ps", "-axo", "pid=,comm="],
                            capture_output=True, text=True, check=True, timeout=5)
    found = []
    for line in result.stdout.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) == 2 and (fields[1].endswith(("/engine", "/converter", "/cargo", "/rustc"))
                                 or "Mudcrab" in fields[1]):
            found.append({"pid": int(fields[0]), "command": fields[1]})
    return found


def stop_owned_process(process):
    if process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def interrupt_campaign(_signum, _frame):
    raise KeyboardInterrupt


def sample_host(process, stop, rss, host, thermal_probe):
    next_host = time.monotonic() + 10
    while not stop.is_set() and process.poll() is None:
        started = time.monotonic()
        result = subprocess.run(["/bin/ps", "-o", "rss=,%cpu=", "-p", str(process.pid)],
                                capture_output=True, text=True, timeout=5)
        fields = result.stdout.split()
        if result.returncode == 0 and len(fields) == 2:
            rss.append({"utc_epoch": time.time(), "monotonic": time.monotonic(),
                        "pid": process.pid, "rss_bytes": int(fields[0]) * 1024,
                        "cpu_percent": float(fields[1])})
        if started >= next_host:
            host.append(host_snapshot(thermal_probe))
            next_host = started + 10
        stop.wait(max(0, 1 - (time.monotonic() - started)))


MODES = {
    "fixed64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": False,
                "backlog": 0, "memory_mib": 0},
    "adaptive128": {"jobs": 128, "activations": 96, "upload_mib": 32, "adaptive": True,
                    "backlog": 256, "memory_mib": 16384},
    "controlled64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": False,
                     "backlog": 256, "memory_mib": 16384},
    "adaptive64": {"jobs": 64, "activations": 32, "upload_mib": 16, "adaptive": True,
                   "backlog": 256, "memory_mib": 16384},
}
# Engine exit code for a lost GPU device (crates/engine/src/app.rs, EXIT_DEVICE_LOST). Such a run
# measured nothing and is worth retrying, but repeated losses can still be the engine's fault.
EXIT_DEVICE_LOST = 3
EXPECTED = {"cells": 25, "models": 2029, "lod_chunks": 176, "terrain": 100, "water": 25}
# The engine stops a route at this many real seconds, so its report and profile bundle are written
# before the launch timeout kills the process. A hung process can still be killed before export.
ENGINE_DEADLINE_GRACE_SECONDS = 30


def schedule(repeats, ablation_repeats, seed, smoke):
    rng = random.Random(seed)
    runs = []
    if smoke:
        cases = [("smoke", 0, 1, 1), ("smoke-route", 370, 10, 3)]
        for index, (scenario, speed, seconds, tail) in enumerate(cases):
            modes = ["fixed64", "adaptive128"]
            if index % 2:
                modes.reverse()
            for mode in modes:
                runs.append({"scenario": scenario, "pair_id": f"{scenario}-01", "mode": mode,
                             "speed": speed, "route_secs": seconds, "tail_secs": tail})
        return runs
    first_order = rng.randrange(2)
    for pair in range(repeats):
        for scenario_index, (scenario, speed, seconds) in enumerate(
                [("normal", 370, 45), ("stress", 3000, 20)]):
            modes = ["fixed64", "adaptive128"]
            if (pair + scenario_index + first_order) % 2:
                modes.reverse()
            for mode in modes:
                runs.append({"scenario": scenario, "pair_id": f"{scenario}-{pair + 1:02}",
                             "mode": mode, "speed": speed, "route_secs": seconds, "tail_secs": 10})
    modes = ["fixed64", "controlled64", "adaptive64"]
    for pair in range(ablation_repeats):
        offset = pair % len(modes)
        for mode in modes[offset:] + modes[:offset]:
            runs.append({"scenario": "ablation", "pair_id": f"ablation-{pair + 1:02}",
                         "mode": mode, "speed": 370, "route_secs": 45, "tail_secs": 10})
    return runs


def engine_deadline(timeout):
    """The engine's real-time deadline, kept below the launch timeout by the documented grace."""
    return f"{timeout - ENGINE_DEADLINE_GRACE_SECONDS:g}"


def command(args, run, directory):
    mode = MODES[run["mode"]]
    result = [str(args.engine), "--assets", str(args.assets), "--worldspace", "0x3c",
              "--grid-x", "5", "--grid-y", "-12", "--stream-radius", "2",
              "--prioritize-streaming", "--max-scene-loads", str(mode["jobs"]),
              "--max-model-spawns-per-frame", str(mode["activations"]),
              "--max-upload-mib-per-frame", str(mode["upload_mib"]),
              "--max-streaming-backlog", str(mode["backlog"]),
              "--streaming-memory-mib", str(mode["memory_mib"]),
              "--streaming-benchmark-route-speed", str(run["speed"]),
              "--streaming-benchmark-route-secs", str(run["route_secs"]),
              "--streaming-benchmark-tail-secs", str(run["tail_secs"]),
              "--streaming-benchmark-settle-secs", "5",
              "--streaming-benchmark-expected-models", str(EXPECTED["models"]),
              "--streaming-benchmark-expected-lod-chunks", str(EXPECTED["lod_chunks"]),
              "--streaming-benchmark-expected-terrain", str(EXPECTED["terrain"]),
              "--streaming-benchmark-expected-water", str(EXPECTED["water"]),
              "--benchmark-vsync", "--benchmark-duration", engine_deadline(args.timeout),
              "--benchmark-warmup-frames", "0", "--benchmark-output", str(directory / "report.json"),
              "--benchmark-frame-times", str(directory / "frames.csv"),
              "--profile-output", str(directory / "profile"),
              "--profile-scenario", f"comparison-{run['scenario']}-{run['mode']}",
              "--profile-run-id", run["run_id"], "--profile-commit", args.commit,
              "--profile-dirty-worktree", "--accept-min-fps", "0",
              "--accept-p95-ms", "1000000", "--accept-max-memory-growth-gib", "1000"]
    if mode["backlog"] or mode["adaptive"]:
        result += ["--streaming-costs", str(args.costs)]
    if mode["adaptive"]:
        result += ["--adaptive-streaming"]
    return result


def run_one(args, experiment, run, hashes):
    competing = game_processes()
    if competing:
        raise RuntimeError(f"Other game/build processes are active; preserving them: {competing}")
    directory = Path(run["artifact_dir"])
    directory.mkdir()
    argv = command(args, run, directory)
    run.update({"argv": argv, "status": "running"})
    record = {**run, "cache_policy": experiment["cache_policy"],
              "expected_startup": EXPECTED, "input_hashes_before": hashes(),
              "launch_utc_epoch": time.time(), "launch_monotonic": time.monotonic(),
              "launch_utc": utc_now(), "timed_out": False, "exit_code": None}
    write_json(directory / "run.json", record)
    rss, host = [], [host_snapshot(args.thermal_probe)]
    process = None
    stop = threading.Event()
    sampler = None
    try:
        with (directory / "engine.stdout.log").open("wb") as stdout, \
                (directory / "engine.stderr.log").open("wb") as stderr:
            record.update({"launch_utc_epoch": time.time(), "launch_monotonic": time.monotonic(),
                           "launch_utc": utc_now()})
            process = subprocess.Popen(argv, cwd=directory,
                                       env={**os.environ, "RUST_LOG": "info"},
                                       stdout=stdout, stderr=stderr, start_new_session=True)
            record["owned_process_group"] = process.pid
            write_json(directory / "run.json", record)
            sampler = threading.Thread(target=sample_host,
                                       args=(process, stop, rss, host, args.thermal_probe), daemon=True)
            sampler.start()
            try:
                record["exit_code"] = process.wait(timeout=args.timeout)
            except subprocess.TimeoutExpired:
                record["timed_out"] = True
                stop_owned_process(process)
                record["exit_code"] = process.returncode
    finally:
        if process is not None:
            stop_owned_process(process)
            record["exit_code"] = process.returncode
        stop.set()
        if sampler:
            sampler.join(timeout=10)
        record["end_utc_epoch"] = time.time()
        record["end_monotonic"] = time.monotonic()
        record["wall_seconds"] = record["end_monotonic"] - record["launch_monotonic"]
        record["input_hashes_after"] = hashes()
        record["device_lost"] = record["exit_code"] == EXIT_DEVICE_LOST
        record["status"] = "timed_out" if record["timed_out"] else (
            "completed" if record["exit_code"] == 0 else "failed")
        write_json(directory / "run.json", record)
        write_json(directory / "rss.json", {"interval_secs": 1, "samples": rss,
                                            "scope": "OS sampled RSS; full GPU memory is not measured separately; peak is a lower bound"})
        host.append(host_snapshot(args.thermal_probe))
        write_json(directory / "host.json", {"samples": host})
        run.update({"status": record["status"], "exit_code": record["exit_code"],
                    "wall_seconds": record["wall_seconds"]})
    route = {"expected_startup": EXPECTED, "speed": run["speed"],
             "movement_duration_secs": run["route_secs"], "tail_duration_secs": run["tail_secs"]}
    report_path = directory / "report.json"
    if report_path.is_file():
        report = json.loads(report_path.read_text())
        snapshot = report.get("streaming_benchmark_route") or {}
        route.update(snapshot)
        route["movement_start_elapsed_ms"] = snapshot.get("movement_started_elapsed_ms")
        route["movement_end_elapsed_ms"] = snapshot.get("movement_finished_elapsed_ms")
        route["tail_end_elapsed_ms"] = snapshot.get("tail_finished_elapsed_ms")
        route["startup_settled_elapsed_ms"] = snapshot.get("cpu_settled_elapsed_ms")
    write_json(directory / "route.json", route)
    print(json.dumps({"run_id": run["run_id"], "status": record["status"],
                      "wall_seconds": round(record["wall_seconds"], 3),
                      "cpu_settled_ms": route.get("startup_settled_elapsed_ms"),
                      "sampled_peak_rss_bytes": max((sample["rss_bytes"] for sample in rss), default=None)}),
          flush=True)


def resume_experiment(args, hashes):
    experiment = json.loads((args.output / "experiment.json").read_text())
    if experiment["status"] not in ("interrupted", "running"):
        raise ValueError("Only an interrupted or running campaign can be resumed")
    if experiment["commit"] != args.commit or experiment["input_hashes"] != hashes():
        raise ValueError("Resume requires the original commit, binary, and asset inputs")
    if experiment["protocol"]["modes"] != MODES:
        raise ValueError("Resume requires the original mode configurations")
    # The watchdog and the engine's deadline set how far each run may go; a resumed run under other
    # values would not be comparable with the completed ones, and an unrecorded value cannot be checked.
    for key, current in (("timeout_seconds", float(args.timeout)),
                         ("engine_deadline_seconds", float(engine_deadline(args.timeout)))):
        recorded = experiment["protocol"].get(key)
        if isinstance(recorded, bool) or not isinstance(recorded, (int, float)):
            raise ValueError(f"Resume requires the recorded {key}; this campaign does not record it, "
                             "so its deadline cannot be verified")
        if float(recorded) != current:
            raise ValueError(f"Resume requires the original {key} ({float(recorded):g}); got {current:g}")
    for run in experiment["runs"]:
        if run["status"] not in ("completed", "failed", "timed_out", "pending"):
            raise ValueError("Resolve running or partial records and add replacements before resuming")
        if run["status"] == "pending" and Path(run["artifact_dir"]).exists():
            raise ValueError("A pending run already has artifacts; refusing to overwrite them")
    experiment.setdefault("resume_history", []).append({
        "utc": utc_now(), "runner_sha256": digest(Path(__file__)),
        "host": host_snapshot(args.thermal_probe),
        "previous_interruption": experiment.get("interruption"),
    })
    experiment["status"] = "running"
    return experiment


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--costs", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--thermal-probe", type=Path)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--ablation-repeats", type=int, default=3)
    parser.add_argument("--seed", type=int, default=20261010)
    parser.add_argument("--timeout", type=float, default=240)
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--resume", action="store_true", help="Run only pending records in an existing campaign")
    args = parser.parse_args()
    for name in ("engine", "assets", "costs", "output", "thermal_probe"):
        value = getattr(args, name)
        if value is not None:
            setattr(args, name, value.resolve())
    signal.signal(signal.SIGTERM, interrupt_campaign)
    if args.repeats < 1 or args.ablation_repeats < 0:
        parser.error("repeats must be positive and ablation repeats nonnegative")
    if not math.isfinite(args.timeout) or args.timeout <= ENGINE_DEADLINE_GRACE_SECONDS + 1:
        parser.error(f"timeout must be finite and more than {ENGINE_DEADLINE_GRACE_SECONDS + 1} seconds")
    if not args.resume:
        args.output.mkdir(parents=True, exist_ok=False)
    module_spec = importlib.util.spec_from_file_location(
        "startup_helpers", Path(__file__).with_name("repeat-terrain-startup.py"))
    helpers = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(helpers)

    def hashes():
        return {**helpers.input_hashes(args.engine, args.assets), "streaming_costs": digest(args.costs)}

    if args.resume:
        experiment = resume_experiment(args, hashes)
    else:
        experiment = new_experiment(args, hashes)
    path = args.output / "experiment.json"
    write_json(path, experiment)
    try:
        for run in experiment["runs"]:
            if run["status"] != "pending":
                continue
            run_one(args, experiment, run, hashes)
            write_json(path, experiment)
            # Both arms receive the same inter-run idle interval; no cache flush or service stop.
            time.sleep(3)
        experiment["status"] = "completed"
    except BaseException as error:
        experiment["status"] = "interrupted"
        experiment["interruption"] = str(error)
        raise
    finally:
        experiment["finished_utc"] = utc_now()
        write_json(path, experiment)


def new_experiment(args, hashes):
    runs = schedule(args.repeats, args.ablation_repeats, args.seed, args.smoke)
    for order, run in enumerate(runs):
        run.update({"order": order + 1, "run_id": f"{order + 1:02}-{run['pair_id']}-{run['mode']}",
                    "status": "pending", "exit_code": None})
        run["artifact_dir"] = str(args.output / run["run_id"])
    experiment = {"format_version": 1, "created_utc": utc_now(), "commit": args.commit,
                  "input_hashes": hashes(), "target_frame_ms": 1000 / 60,
                  "cache_policy": "fresh process; OS caches preserved; input hashing warms metadata inputs",
                  "protocol": {"expected_startup": EXPECTED, "modes": MODES,
                               "build_profile": "quick",
                               "present_mode": "AutoVsync", "cpu_settlement_quiet_seconds": 5,
                               "route_height": "initial terrain center +6000 units; fixed elevated -Z view",
                               "collision_physics": False, "screenshot_capture": False,
                               "gpu_inventory": False, "seed": args.seed, "smoke": args.smoke,
                               "rss_interval_seconds": 1, "timeout_seconds": args.timeout,
                               "engine_deadline_seconds": float(engine_deadline(args.timeout))},
                  "host_initial": host_snapshot(args.thermal_probe), "runs": runs,
                  "status": "running"}
    protocol = Path(__file__).parents[1] / "docs/research/streaming-comparison-protocol.md"
    experiment["runner_sha256"] = digest(Path(__file__))
    if protocol.is_file():
        experiment["protocol_document"] = str(protocol)
        experiment["protocol_sha256"] = digest(protocol)
    receipt = args.engine.parent / "build-receipt.json"
    if receipt.is_file():
        experiment["build_receipt"] = json.loads(receipt.read_text())
    with sqlite3.connect(f"file:{args.assets / 'skyrim_world.db'}?mode=ro", uri=True) as database:
        cells = database.execute(
            "SELECT DISTINCT grid_x, grid_y FROM cells WHERE worldspace_id = ? "
            "AND grid_x IS NOT NULL AND grid_y IS NOT NULL", (0x3c,)).fetchall()
    cell_catalog = {"worldspace_id": 0x3c, "cell_size_units": 4096, "stream_radius": 2,
                    "existing_cells": sorted([list(cell) for cell in cells]),
                    "database_sha256": experiment["input_hashes"]["skyrim_world.db"],
                    "meaning": "Database cell existence; CPU committed cell coverage does not certify drawable assets"}
    write_json(args.output / "route-cell-catalog.json", cell_catalog)
    experiment["route_cell_catalog"] = str(args.output / "route-cell-catalog.json")
    return experiment


if __name__ == "__main__":
    main()
