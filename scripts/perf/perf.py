#!/usr/bin/env python3
"""Measure the Mudcrab engine, compare two binaries statistically, and compare screenshots.

Subcommands: run, compare, preflight, shots, visual, build, scenarios, and the analysis commands
summarize, gpu-busy and samply-top. The guide is docs/contributing/perf-loop.md. Standard library only; numpy and flip-evaluator are used for the
visual metric when they happen to be installed.

Exit codes: 0 done, 2 bad usage or input, 3 refused (a guard, a failed preflight, or too few valid
runs), 4 a run or capture did not produce a valid result.
"""

import argparse
import bisect
import collections
import gzip
import hashlib
import itertools
import json
import math
import os
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPTS_DIR = HERE.parent
SCENARIOS_FILE = HERE / "scenarios.json"

GIB = 1024**3
DEFAULT_DURATION = 60.0
DEFAULT_MIN_EFFECT = 0.02
DEFAULT_SEED = 1
BOOTSTRAP_SAMPLES = 10000
# Significance comes from an exact two-sided Wilcoxon signed-rank test on log ratios. Below six
# pairs its smallest attainable p-value is 0.0625, so no difference could ever be significant.
# A percentile bootstrap of the median is not used for the decision: with three pairs its interval
# is about [min, max] and excludes 1.0 a quarter of the time when nothing changed.
MIN_PAIRS_FOR_VERDICT = 6
SIGNIFICANCE = 0.05
MIN_FREE_BUILD_GIB = 15
MIN_FREE_RUN_GIB = 5
ROUTE_READINESS_SECONDS = 60
# The report keys the verdict is drawn from; the other metrics are listed but never decide it.
PRIMARY_METRICS = ("frame_ms_mean", "frame_ms_p50", "frame_ms_p95", "frame_ms_p99")
FRAME_METRICS = (
    "frame_ms_mean",
    "frame_ms_p50",
    "frame_ms_p95",
    "frame_ms_p99",
    "average_fps",
)
HIGHER_IS_BETTER = {"average_fps"}
# Fingerprint fields that name the build rather than the conditions it was measured under.
FINGERPRINT_IGNORED = ("commit", "dirty_worktree")
SCENE_COUNTS = ("mesh_entities", "terrain_patches", "water_surfaces")
# Counts below this absolute difference are never a mismatch: a streaming scene can differ by a
# handful of entities between two runs of the same binary.
INVENTORY_ABSOLUTE_SLACK = 2
DEFAULT_INVENTORY_TOLERANCE = 0.02
TOP_CPU_SPANS = 8
MIN_SPAN_SAMPLES = 5

WINDOW_MODES = ("auto", "background", "visible", "hidden")
BACKGROUND_FLAG = "--background-window"
HIDDEN_FLAG = "--hidden-window"
# What a hidden window does on macOS, measured on the dense Riverwood scenario: 10.45 ms/frame
# hidden against 15.71 ms/frame visible with the same binary, and a Metal trace of the hidden run
# recorded ~157,000 command buffers and no drawable acquisitions.
HIDDEN_WINDOW_MACOS_WARNING = (
    "the hidden window never acquires a drawable or presents on macOS, so these timings exclude "
    "presentation and are not representative (Riverwood dense: 10.45 ms/frame hidden, 15.71 ms/frame "
    "visible, same binary); use --window background or visible")
PROFILERS = ("none", "samply", "metal-trace")
METAL_TRACE_SOURCE = SCRIPTS_DIR / "profiling" / "metal_trace.m"
METAL_TRACE_START_ACQUISITION = 600
SAMPLY_RATE_HZ = 1000
# Samply serialises and presymbolicates after the profiled process exits; a large profile takes a
# while, and killing samply during that loses the whole capture.
SAMPLY_FINALIZE_SECONDS = 240.0

# Analysis thresholds. A frame is bound by the first resource that is busy for most of it.
GPU_BUSY_BOUND = 0.90
THREAD_BUSY_BOUND = 0.85
WAIT_SHARE_RENDER_BOUND = 0.25
WAIT_SHARE_MAIN_BOUND = 0.10
PER_FRAME_SPAN_SHARE = 0.5
COUNTER_SPAN_PREFIXES = ("assets_added/",)
DEFAULT_TRACE_CROP_SECONDS = 1.0
DEFAULT_SAMPLY_SKIP_SECONDS = 10.0


class HarnessError(Exception):
    """An error with a stable name, so a verdict or an agent can match on it."""

    def __init__(self, name, message):
        super().__init__(f"{name}: {message}")
        self.name = name
        self.message = message


# --------------------------------------------------------------------------------------------
# Scenarios


def load_scenarios(path=SCENARIOS_FILE):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    return data["scenarios"]


def route_travel_seconds(route_path):
    route = json.loads(Path(route_path).read_text(encoding="utf-8"))
    if route.get("loop"):
        raise HarnessError("route_loops", f"{route_path} loops, so it has no travel time")
    points = route["waypoints"]
    length = sum(
        math.hypot(points[i][0] - points[i - 1][0], points[i][1] - points[i - 1][1])
        for i in range(1, len(points))
    )
    return int(math.ceil(length / float(route["speed"])))


def effective_duration(scenario, requested):
    """The window a scenario needs: a route is walked end to end, so it is never cut short."""
    duration = requested if requested is not None else scenario.get("default_duration", DEFAULT_DURATION)
    route = scenario.get("route")
    if route:
        needed = route_travel_seconds(SCRIPTS_DIR / route) + ROUTE_READINESS_SECONDS
        duration = max(duration, needed)
    return duration


def build_engine_args(name, scenario, *, assets, out_dir, duration, warmup_frames, window, commit, run_id,
                      frame_budget_ms, allow_incomplete=True):
    """The engine command line (without the binary) for one scenario run."""
    if scenario.get("needs_assets") and not assets:
        raise HarnessError("assets_required", f"scenario {name} needs --assets")
    out_dir = Path(out_dir)
    warmup = warmup_frames if warmup_frames is not None else scenario.get("warmup_frames")
    args = [
        "--benchmark-duration", fmt_number(effective_duration(scenario, duration)),
        "--benchmark-output", str(out_dir / "report.json"),
        "--profile-output", str(out_dir / "bundle"),
        "--profile-scenario", name,
        "--profile-run-id", run_id,
        "--profile-commit", commit,
        # The engine fails its own acceptance gate on a slow frame; a measurement run must not.
        "--accept-min-fps", "0",
        "--accept-p95-ms", "1000000",
        "--accept-max-memory-growth-gib", "1000000",
        "--frame-budget-ms", fmt_number(frame_budget_ms),
    ]
    if warmup is not None:
        args += ["--benchmark-warmup-frames", str(int(warmup))]
    args += list(scenario["args"])
    if scenario.get("route"):
        args += ["--route", str(SCRIPTS_DIR / scenario["route"])]
    if scenario.get("needs_assets"):
        args += ["--assets", str(assets)]
        if allow_incomplete:
            args.append("--allow-incomplete-assets")
    if window == "hidden":
        args.append(HIDDEN_FLAG)
    elif window == "background":
        args.append(BACKGROUND_FLAG)
    elif window != "visible":
        raise HarnessError("bad_window_mode", f"{window!r} is not a concrete window mode (background, visible, hidden)")
    return args


def fmt_number(value):
    return str(int(value)) if float(value).is_integer() else repr(float(value))


# --------------------------------------------------------------------------------------------
# Window mode


def cache_dir():
    """Where probe results and compiled helpers are kept between invocations."""
    override = os.environ.get("MUDCRAB_PERF_CACHE")
    return Path(override) if override else Path.home() / ".cache" / "mudcrab-perf"


def engine_help_text(engine, timeout=30.0):
    """The engine's `--help` output. This starts the binary only to print usage and exit."""
    try:
        result = subprocess.run([str(engine), "--help"], capture_output=True, text=True, timeout=timeout,
                                stdin=subprocess.DEVNULL)
    except (OSError, subprocess.SubprocessError) as error:
        raise HarnessError("help_probe_failed", f"{engine} --help: {error}") from error
    return (result.stdout or "") + (result.stderr or "")


_PROBE_MEMORY = {}


def supports_background_window(engine, *, help_text=engine_help_text, cache_path=None):
    """Whether the engine lists `--background-window`. Cached per binary SHA-256, on disk and in memory."""
    sha = sha256_file(engine)
    if sha in _PROBE_MEMORY:
        return _PROBE_MEMORY[sha]
    path = Path(cache_path) if cache_path else cache_dir() / "engine-flags.json"
    try:
        cached = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        cached = {}
    entry = cached.get(sha) if isinstance(cached, dict) else None
    if isinstance(entry, dict) and isinstance(entry.get("background_window"), bool):
        _PROBE_MEMORY[sha] = entry["background_window"]
        return _PROBE_MEMORY[sha]
    found = re.search(r"(?<![\w-])" + re.escape(BACKGROUND_FLAG) + r"(?![\w-])", help_text(engine)) is not None
    cached = cached if isinstance(cached, dict) else {}
    cached[sha] = {"background_window": found}
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(cached, indent=2), encoding="utf-8")
    except OSError:
        pass  # the cache is an optimisation; an unwritable directory must not fail a run
    _PROBE_MEMORY[sha] = found
    return found


def resolve_window(mode, engines, *, system=None, supports=supports_background_window, for_shots=False):
    """The concrete window mode for `mode` when every binary in `engines` must use the same one.

    `auto` is `background` when every engine lists `--background-window`, else `visible`. An explicit
    `background` for an engine without the flag is refused here rather than left to fail as an
    unknown option. A capture of pixels never uses a hidden window on macOS: its frames are black.
    """
    if mode not in WINDOW_MODES:
        raise HarnessError("bad_window_mode", f"{mode!r}; choose from {', '.join(WINDOW_MODES)}")
    system = system or platform.system()
    if mode == "hidden" and for_shots and system == "Darwin":
        raise HarnessError("hidden_window_black_frames",
                           "a hidden window renders uniformly black frames on macOS; use --window "
                           "background or visible for shots")
    if mode == "auto":
        return "background" if all(supports(e) for e in engines) else "visible"
    if mode == "background":
        missing = [str(e) for e in engines if not supports(e)]
        if missing:
            raise HarnessError("background_window_unsupported",
                               f"{', '.join(missing)} does not list {BACKGROUND_FLAG}; "
                               "use --window visible or a newer binary")
    return mode


# --------------------------------------------------------------------------------------------
# Running an engine under a watchdog


def sha256_file(path, _cache={}):
    path = str(Path(path).resolve())
    stat = os.stat(path)
    key = (path, stat.st_size, stat.st_mtime_ns)
    if key not in _cache:
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for block in iter(lambda: handle.read(1 << 20), b""):
                digest.update(block)
        _cache[key] = digest.hexdigest()
    return _cache[key]


def read_complete_report(path):
    """The benchmark report once the engine has finished writing it, else None.

    The report is a single JSON object written at the end of the window; a half-written file does
    not parse, so parsing is the completeness check.
    """
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if isinstance(data, dict) and "frame_ms_mean" in data and "fingerprint" in data:
        return data
    return None


# Process groups, killpg and SIGKILL are POSIX-only. Windows gets the child's own terminate/kill,
# which does not reach grandchildren; the harness targets macOS and this keeps it importable.
POSIX = os.name == "posix"
KILL_SIGNAL = getattr(signal, "SIGKILL", signal.SIGTERM)


def spawn_options(posix=None):
    """Popen options that put the engine outside this process's signal group."""
    if POSIX if posix is None else posix:
        return {"start_new_session": True}
    return {"creationflags": getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)}


def _terminate_group(proc, grace=5.0, posix=None):
    """Stop the engine and anything it spawned. Returns True when it had to be killed."""
    if not (POSIX if posix is None else posix):
        proc.terminate()
        try:
            proc.wait(timeout=grace)
            return False
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            return True
    try:
        pgid = os.getpgid(proc.pid)
    except (ProcessLookupError, OSError):
        pgid = None

    def signal_group(sig):
        try:
            if pgid is not None:
                os.killpg(pgid, sig)
            else:
                proc.send_signal(sig)
        except (ProcessLookupError, OSError):
            pass

    signal_group(signal.SIGTERM)
    try:
        proc.wait(timeout=grace)
        return False
    except subprocess.TimeoutExpired:
        signal_group(KILL_SIGNAL)
        proc.wait()
        return True


def _reap_after_interruption(proc, terminate):
    """Stop and reap a child the watchdog is abandoning. Never raises: the caller is already unwinding."""
    if proc.poll() is not None:
        return
    for stop in (terminate, _terminate_group):
        try:
            stop(proc)
        except Exception:
            continue
        if proc.poll() is not None:
            return


def watch_process(command, *, stdout_path, stderr_path, is_done, timeout, grace, poll=0.2, env=None,
                  terminate=None):
    """Run a command and return what happened.

    `is_done()` says the output the run exists for is complete. The engine has been seen to write
    its report and then never exit (a hang in pipelined-render teardown), so once the output is
    complete the process gets `grace` seconds to exit by itself and is killed after that. A
    process that never produces the output is killed at `timeout`.

    `terminate(proc)` replaces the default kill of the whole process group. It must return only
    once `proc` has exited, and say whether it had to stop anything: a profiler that is still
    writing its file after the engine already exited is not a hang.
    """
    terminate = terminate or (lambda proc: (_terminate_group(proc), True)[1])
    started = time.monotonic()
    done_at = None
    killed = False
    reason = None
    with open(stdout_path, "wb") as out, open(stderr_path, "wb") as err:
        try:
            proc = subprocess.Popen(command, stdout=out, stderr=err, stdin=subprocess.DEVNULL,
                                    env=env, **spawn_options())
        except OSError as error:
            return {"spawned": False, "error": str(error), "exit_code": None, "killed": False,
                    "output_complete": False, "wall_seconds": 0.0, "complete_after_seconds": None,
                    "kill_reason": None}
        try:
            while True:
                code = proc.poll()
                now = time.monotonic() - started
                if done_at is None and is_done():
                    done_at = now
                if code is not None:
                    break
                wanted = None
                if done_at is not None and now - done_at >= grace:
                    wanted = "hung_after_output"
                elif now >= timeout:
                    wanted = "timeout"
                if wanted:
                    if terminate(proc):
                        killed, reason = True, wanted
                    code = proc.returncode
                    break
                time.sleep(poll)
        except BaseException:
            # The engine runs in its own session, so Ctrl-C reaches only this process. Without
            # this the engine outlives the harness and skews the next measurement.
            _reap_after_interruption(proc, terminate)
            raise
        if done_at is None and is_done():
            done_at = time.monotonic() - started
    return {"spawned": True, "exit_code": code, "killed": killed, "kill_reason": reason,
            "output_complete": done_at is not None, "wall_seconds": round(time.monotonic() - started, 3),
            "complete_after_seconds": None if done_at is None else round(done_at, 3)}


def classify(watch):
    """ok, hung_after_report, timeout_no_report or failed."""
    if not watch["spawned"]:
        return "failed"
    if watch["output_complete"]:
        return "hung_after_report" if watch["killed"] else "ok"
    return "timeout_no_report" if watch["killed"] else "failed"


VALID_CLASSES = ("ok", "hung_after_report")


def run_scenario(engine, name, scenario, *, assets, out_dir, duration=None, warmup_frames=None, timeout=None,
                 grace=20.0, window="visible", run_id=None, commit=None, frame_budget_ms=6.25, dry_run=False,
                 extra_args=(), poll=0.2, profiler="none", samply=None,
                 trace_start=METAL_TRACE_START_ACQUISITION, finalize_timeout=SAMPLY_FINALIZE_SECONDS,
                 dylib_builder=None):
    """Run one scenario and write run.json beside the report. Returns the run.json dict.

    `window` is a concrete mode (background, visible or hidden); resolve `auto` first.
    """
    out_dir = Path(out_dir)
    engine = str(engine)
    binary_sha = sha256_file(engine)
    commit = commit or binary_sha[:8]
    run_id = run_id or out_dir.name
    duration_window = effective_duration(scenario, duration)
    args = build_engine_args(name, scenario, assets=assets, out_dir=out_dir, duration=duration,
                             warmup_frames=warmup_frames, window=window, commit=commit, run_id=run_id,
                             frame_budget_ms=frame_budget_ms) + list(extra_args)
    command = [engine] + args
    if dry_run:
        plan = plan_profiler(profiler, command, out_dir, samply=samply, trace_start=trace_start)
        return {"command": plan["launch_command"], "env": plan["env"]}
    if profiler == "samply":
        samply = find_samply(samply)
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    dylib = (dylib_builder or metal_trace_dylib)(out_dir) if profiler == "metal-trace" else None
    plan = plan_profiler(profiler, command, out_dir, samply=samply, trace_start=trace_start, dylib=dylib)
    env = dict(os.environ, **plan["env"]) if plan["env"] else None
    report_path = out_dir / "report.json"
    # The default wall limit leaves room for loading the world, which the window includes.
    timeout = timeout if timeout is not None else duration_window + 120
    terminate = None
    if profiler == "samply":
        terminate = lambda proc: terminate_engine_under_samply(proc, engine, finalize_timeout=finalize_timeout)
    watch = watch_process(plan["launch_command"], stdout_path=out_dir / "stdout.log",
                          stderr_path=out_dir / "stderr.log",
                          is_done=lambda: read_complete_report(report_path) is not None,
                          timeout=timeout, grace=grace, poll=poll, env=env, terminate=terminate)
    report = read_complete_report(report_path)
    classification = classify(watch)
    warnings = []
    if report is not None:
        if watch["exit_code"] not in (0, None) and not watch["killed"]:
            warnings.append(f"engine exited with {watch['exit_code']} after writing a complete report")
        if report.get("world_ready_reached") is False:
            warnings.append("the world never became ready inside the window")
        ready = report.get("frames_to_world_ready")
        warmup = report.get("warmup_frames")
        if ready is not None and warmup is not None and ready > warmup:
            warnings.append(f"world ready at frame {ready}, after the {warmup}-frame warm-up: "
                            "the measured frames include loading; raise --warmup-frames")
    if window == "hidden" and platform.system() == "Darwin":
        warnings.append(HIDDEN_WINDOW_MACOS_WARNING)
    outputs = {}
    for label, path in plan["outputs"].items():
        present = Path(path).is_file() and Path(path).stat().st_size > 0
        outputs[label] = {"path": path, "present": present}
        if not present:
            warnings.append(f"the {profiler} {label} was not written: {path}")
    if profiler != "none":
        warnings.append(f"profiled with {profiler}: timings carry the profiler's overhead and are not "
                        "comparable with unprofiled runs")
    run = {
        "scenario": name,
        "classification": classification,
        "valid": classification in VALID_CLASSES and report is not None,
        "command": command,
        "binary": engine,
        "binary_sha256": binary_sha,
        "window_mode": window,
        "hidden_window": window == "hidden",
        "duration_seconds": duration_window,
        "timeout_seconds": timeout,
        "grace_seconds": grace,
        "exit_code": watch["exit_code"],
        "killed": watch["killed"],
        "kill_reason": watch["kill_reason"],
        "wall_seconds": watch["wall_seconds"],
        "report_complete_after_seconds": watch["complete_after_seconds"],
        "report_passed": None if report is None else report.get("passed"),
        "bundle_complete": (out_dir / "bundle" / "summary.md").exists(),
        "report": str(report_path) if report is not None else None,
        "warnings": warnings,
    }
    if profiler != "none":
        run["profiler"] = {"name": profiler, "launch_command": plan["launch_command"],
                           "env": plan["env"], "outputs": outputs}
    if watch.get("error"):
        run["error"] = watch["error"]
    (out_dir / "run.json").write_text(json.dumps(run, indent=2), encoding="utf-8")
    return run


# --------------------------------------------------------------------------------------------
# Profilers


def list_processes():
    """(pid, ppid, command line) of every process, from `ps`."""
    out = _command_output(["ps", "-axo", "pid=,ppid=,command="]) or ""
    rows = []
    for line in out.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
            rows.append((int(parts[0]), int(parts[1]), parts[2] if len(parts) > 2 else ""))
    return rows


def find_engine_pid(root_pid, engine, processes):
    """The pid of `engine` among the descendants of `root_pid`, or None.

    `processes` is a list of (pid, ppid, command). Samply starts the engine as its child; the
    engine is recognised by its executable path (or name, when launched through a relative path).
    """
    children = collections.defaultdict(list)
    for pid, ppid, command in processes:
        children[ppid].append((pid, command))
    engine = str(engine)
    names = {engine, Path(engine).name}
    queue = list(children.get(root_pid, []))
    seen = set()
    while queue:
        pid, command = queue.pop(0)
        if pid in seen:
            continue
        seen.add(pid)
        tokens = command.split()
        # A script engine shows up as `interpreter script ...`, a binary as `binary ...`.
        if any(t in names for t in tokens[:2]) or (tokens and Path(tokens[0]).name == Path(engine).name):
            return pid
        queue.extend(children.get(pid, []))
    return None


def pid_alive(pid):
    if not POSIX:
        # os.kill(pid, 0) terminates the process on Windows instead of probing it.
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def terminate_engine_under_samply(proc, engine, *, grace=5.0, finalize_timeout=SAMPLY_FINALIZE_SECONDS,
                                  processes=list_processes, send_signal=os.kill, alive=pid_alive,
                                  sleep=time.sleep, clock=time.monotonic):
    """Stop the engine that samply is recording and let samply write its profile.

    Signalling samply, or the whole process group, would end the recording before the profile is
    saved. Only the engine gets SIGTERM (SIGKILL after `grace`); samply sees its child exit, writes
    the file and exits by itself. If it has not exited after `finalize_timeout` it gets SIGINT, which
    also makes it save, and the group is killed only if that is ignored as well. Returns True when
    an engine process had to be stopped, False when it had already exited (samply was only still
    saving).
    """
    stopped = False
    pid = find_engine_pid(proc.pid, engine, processes())
    if pid is not None and alive(pid):
        stopped = True
        send_signal(pid, signal.SIGTERM)
        deadline = clock() + grace
        while alive(pid) and clock() < deadline:
            sleep(0.1)
        if alive(pid):
            send_signal(pid, KILL_SIGNAL)
    elif pid is None and proc.poll() is None:
        # No engine below samply: nothing else can be ended without losing the profile.
        stopped = True
        send_signal(proc.pid, signal.SIGINT)
    deadline = clock() + finalize_timeout
    while proc.poll() is None and clock() < deadline:
        sleep(0.2)
    if proc.poll() is None:
        send_signal(proc.pid, signal.SIGINT)
        deadline = clock() + 30.0
        while proc.poll() is None and clock() < deadline:
            sleep(0.2)
    if proc.poll() is None:
        _terminate_group(proc)
    return stopped


def metal_trace_dylib(run_dir, *, source=METAL_TRACE_SOURCE, cache_root=None, runner=None):
    """The compiled Metal trace observer, copied into the run directory.

    The dylib is compiled with the command from scripts/profiling/README.md into a cache keyed by the
    SHA-256 of the source, so an unchanged source is compiled once. The copy in the run directory
    is the one the run loads, which keeps the evidence directory self-contained.
    """
    runner = runner or _run
    source = Path(source)
    if not source.is_file():
        raise HarnessError("profiler_unavailable", f"{source} is missing")
    digest = hashlib.sha256(source.read_bytes()).hexdigest()[:16]
    cached = (Path(cache_root) if cache_root else cache_dir()) / "metal-trace" / digest / "libmudcrab_metal_trace.dylib"
    if not cached.is_file():
        cached.parent.mkdir(parents=True, exist_ok=True)
        partial = cached.with_name(cached.name + ".partial")
        command = ["clang", "-fobjc-arc", "-O2", "-Wall", "-Wextra", "-Werror", "-dynamiclib", str(source),
                   "-o", str(partial), "-framework", "Foundation", "-framework", "Metal",
                   "-framework", "QuartzCore", "-framework", "AppKit"]
        try:
            result = runner(command)
        except OSError as error:
            raise HarnessError("profiler_unavailable", f"clang could not be started: {error}") from error
        if result.returncode != 0:
            raise HarnessError("profiler_build_failed", (result.stderr or "").strip()[-2000:])
        os.replace(partial, cached)
    target = Path(run_dir) / cached.name
    shutil.copy2(cached, target)
    return target


def find_samply(explicit=None):
    candidates = [explicit] if explicit else [shutil.which("samply"), str(Path.home() / ".cargo" / "bin" / "samply")]
    for candidate in candidates:
        if candidate and os.access(candidate, os.X_OK):
            return str(candidate)
    raise HarnessError("profiler_unavailable", "samply was not found; `cargo install --locked samply` or pass --samply")


def plan_profiler(profiler, command, out_dir, *, samply=None, trace_start=METAL_TRACE_START_ACQUISITION,
                  dylib=None, environ=None):
    """The launch command, extra environment and output files of a run under `profiler`.

    Pure apart from locating samply: nothing is compiled or started here.
    """
    out_dir = Path(out_dir).resolve()
    plan = {"profiler": profiler, "launch_command": list(command), "env": {}, "outputs": {}}
    if profiler == "none":
        return plan
    if profiler == "samply":
        profile = out_dir / "cpu-profile.json.gz"
        plan["launch_command"] = [samply or "samply", "record", "--save-only", "--unstable-presymbolicate",
                                  "--rate", str(SAMPLY_RATE_HZ), "--output", str(profile), "--"] + list(command)
        plan["outputs"] = {"profile": str(profile), "symbols": str(profile.with_name(profile.name[:-3] + ".syms.json"))}
        return plan
    if profiler == "metal-trace":
        trace = out_dir / "native.ndjson"
        plan["env"] = {
            "DYLD_INSERT_LIBRARIES": str(dylib or out_dir / "libmudcrab_metal_trace.dylib"),
            # The observer opens the file exclusively and refuses to overwrite it.
            "MUDCRAB_METAL_TRACE_FILE": str(trace),
            "MUDCRAB_METAL_TRACE_START_ACQUISITION": str(int(trace_start)),
        }
        plan["outputs"] = {"trace": str(trace)}
        return plan
    raise HarnessError("bad_profiler", f"{profiler!r}; choose from {', '.join(PROFILERS)}")


# --------------------------------------------------------------------------------------------
# Metrics


def _mean_of(summary):
    return summary.get("mean") if isinstance(summary, dict) else None


def extract_metrics(report, bundle_dir=None):
    """Flatten a benchmark report into `name -> number`. Missing or zero-filled values are left out."""
    metrics = {}
    for key in FRAME_METRICS:
        value = report.get(key)
        if isinstance(value, (int, float)):
            metrics[key] = float(value)
    for name, summary in (report.get("gpu_passes", {}).get("passes") or {}).items():
        mean = _mean_of(summary)
        # Timestamp queries that read back all zero (Metal reports this) carry no information.
        if isinstance(mean, (int, float)) and mean > 0:
            metrics[f"gpu/{name}"] = float(mean)
    cpu = report.get("cpu") or {}
    for group in ("schedules", "spans"):
        for name, summary in (cpu.get(group) or {}).items():
            mean = _mean_of(summary)
            if isinstance(mean, (int, float)) and (summary.get("count") or 0) >= MIN_SPAN_SAMPLES:
                metrics[f"cpu/{name}"] = float(mean)
    peak = report.get("peak_process_memory_gib")
    if not isinstance(peak, (int, float)) or peak <= 0:
        peak = None
        if bundle_dir is not None:
            try:
                samples = json.loads((Path(bundle_dir) / "memory.json").read_text(encoding="utf-8"))["samples"]
                peak = max((s.get("process_gib") or 0.0) for s in samples) or None
            except (OSError, ValueError, KeyError):
                peak = None
    if peak:
        metrics["memory_peak_gib"] = float(peak)
    return metrics


# Threshold fields that say the run measured the renderer it was meant to. `passed` also folds in
# the FPS and p95 limits, so a real slowdown must not be hidden by refusing on it.
HEALTH_FIELDS = ("renderer_path_active", "no_streaming_failures", "commit_budget_respected",
                 "streaming_lifecycle_validated")


def report_health(report):
    """What is wrong with a report as a measurement: the health fields that are false or missing."""
    thresholds = report.get("thresholds")
    if not isinstance(thresholds, dict):
        return ["the report has no thresholds block, so the render path and streaming health are unknown"]
    problems = []
    for field in HEALTH_FIELDS:
        value = thresholds.get(field)
        if value is None:
            problems.append(f"thresholds.{field} is missing")
        elif value is not True:
            problems.append(f"thresholds.{field} is {json.dumps(value)}")
    return problems


def extract_scene(report):
    scene = report.get("scene") or {}
    return {key: scene.get(key) for key in SCENE_COUNTS}


def load_run(run_dir):
    """The run.json, report-derived metrics, fingerprint and scene counts of one run directory."""
    run = json.loads((Path(run_dir) / "run.json").read_text(encoding="utf-8"))
    entry = {"dir": str(run_dir), "run": run, "metrics": {}, "fingerprint": None, "scene": {},
             "health_problems": []}
    if run.get("valid") and run.get("report"):
        report = json.loads(Path(run["report"]).read_text(encoding="utf-8"))
        entry["metrics"] = extract_metrics(report, Path(run_dir) / "bundle")
        entry["fingerprint"] = report.get("fingerprint")
        entry["scene"] = extract_scene(report)
        entry["health_problems"] = report_health(report)
        entry["warmup_frames"] = report.get("warmup_frames")
    return entry


# --------------------------------------------------------------------------------------------
# Statistics


def percentile_sorted(values, fraction):
    """Linear-interpolated percentile of an already sorted list."""
    if not values:
        raise ValueError("no values")
    position = fraction * (len(values) - 1)
    low = int(math.floor(position))
    high = min(low + 1, len(values) - 1)
    return values[low] + (values[high] - values[low]) * (position - low)


def median(values):
    return statistics.median(values)


def bootstrap_ci(ratios, *, seed, samples=BOOTSTRAP_SAMPLES, alpha=0.05):
    """Percentile bootstrap interval of the median of per-pair ratios, resampling whole pairs."""
    rng = random.Random(seed)
    count = len(ratios)
    medians = []
    for _ in range(samples):
        draw = sorted(ratios[rng.randrange(count)] for _ in range(count))
        middle = count // 2
        medians.append(draw[middle] if count % 2 else (draw[middle - 1] + draw[middle]) / 2.0)
    medians.sort()
    return percentile_sorted(medians, alpha / 2), percentile_sorted(medians, 1 - alpha / 2)


def wilcoxon_signed_rank_p(differences):
    """Exact two-sided p-value of the Wilcoxon signed-rank test, conditional on the observed ranks.

    Zero differences are dropped; tied magnitudes get average ranks. Ranks are doubled so the
    distribution of the positive rank sum can be counted over integers.
    """
    values = [d for d in differences if d != 0]
    n = len(values)
    if n == 0:
        return 1.0
    order = sorted(range(n), key=lambda i: abs(values[i]))
    doubled = [0] * n
    start = 0
    while start < n:
        end = start
        while end + 1 < n and abs(values[order[end + 1]]) == abs(values[order[start]]):
            end += 1
        for k in range(start, end + 1):
            doubled[order[k]] = start + end + 2  # twice the average of 1-based ranks start+1..end+1
        start = end + 1
    total = sum(doubled)
    counts = [0] * (total + 1)
    counts[0] = 1
    for rank in doubled:
        for s in range(total, rank - 1, -1):
            counts[s] += counts[s - rank]
    observed = sum(r for r, v in zip(doubled, values) if v > 0)
    extreme = min(observed, total - observed)
    tail = sum(counts[s] for s in range(total + 1) if s <= extreme or s >= total - extreme)
    return min(1.0, tail / 2**n)


def verdict_for(ratio, p_value, *, min_effect, higher_is_better=False):
    """improved / regressed / noise. Significant only if the test rejects and the effect is big enough."""
    if p_value is None or p_value > SIGNIFICANCE or abs(ratio - 1.0) < min_effect:
        return "noise"
    worse = ratio < 1.0 if higher_is_better else ratio > 1.0
    return "regressed" if worse else "improved"


def compare_metric(name, base_values, cand_values, *, seed, min_effect, samples=BOOTSTRAP_SAMPLES):
    """Per-pair ratios candidate/base and their verdict. Lists are index-aligned pairs."""
    pairs = [(b, c) for b, c in zip(base_values, cand_values)
             if b is not None and c is not None and b > 0 and c >= 0]
    result = {"pairs": len(pairs), "higher_is_better": name in HIGHER_IS_BETTER}
    if not pairs:
        result["verdict"] = "no_data"
        return result
    ratios = [c / b for b, c in pairs]
    result.update({
        "base_median": median([b for b, _ in pairs]),
        "cand_median": median([c for _, c in pairs]),
        "ratios": ratios,
        "ratio_median": median(ratios),
    })
    lo, hi = bootstrap_ci(ratios, seed=seed, samples=samples) if len(pairs) >= 2 else (None, None)
    p_value = wilcoxon_signed_rank_p([math.log(r) for r in ratios if r > 0])
    result.update({"ci_lo": lo, "ci_hi": hi, "p_value": p_value})
    if len(pairs) < MIN_PAIRS_FOR_VERDICT:
        result["verdict"] = "insufficient_pairs"
        return result
    result["verdict"] = verdict_for(result["ratio_median"], p_value, min_effect=min_effect,
                                    higher_is_better=result["higher_is_better"])
    return result


# --------------------------------------------------------------------------------------------
# Guards


def fingerprint_diff(old, new, prefix=""):
    """`path: old -> new` lines for every field that differs, ignoring the build identity fields."""
    old, new = old or {}, new or {}
    lines = []
    for key in sorted(set(old) | set(new)):
        if not prefix and key in FINGERPRINT_IGNORED:
            continue
        path = f"{prefix}.{key}" if prefix else key
        a, b = old.get(key), new.get(key)
        if isinstance(a, dict) and isinstance(b, dict):
            lines += fingerprint_diff(a, b, path)
        elif a != b:
            lines.append(f"{path}: {json.dumps(a)} -> {json.dumps(b)}")
    return lines


def check_fingerprints(base_fingerprints, cand_fingerprints):
    """Every run, on both sides, must have been measured under the same conditions."""
    everything = [("base", i, f) for i, f in enumerate(base_fingerprints)] + \
                 [("cand", i, f) for i, f in enumerate(cand_fingerprints)]
    if not everything:
        return []
    reference_side, reference_index, reference = everything[0]
    problems = []
    for side, index, fingerprint in everything[1:]:
        for line in fingerprint_diff(reference, fingerprint):
            problems.append(f"{side}[{index}] vs {reference_side}[{reference_index}] {line}")
    return problems


def window_mode_of(run):
    """The window mode a run used. Runs recorded before the mode existed only say hidden or not."""
    return run.get("window_mode") or ("hidden" if run.get("hidden_window") else "visible")


def check_window_modes(base_runs, cand_runs):
    """Base and candidate must have run in one window mode: presentation cost differs between modes."""
    modes = {("base", window_mode_of(r)) for r in base_runs} | {("cand", window_mode_of(r)) for r in cand_runs}
    names = {mode for _, mode in modes}
    if len(names) <= 1:
        return []
    return [f"{side} ran with window mode {mode}" for side, mode in sorted(modes)]


def check_inventory(base_scenes, cand_scenes, *, tolerance=DEFAULT_INVENTORY_TOLERANCE):
    """The two sides must have drawn the same scene; a smaller scene is not an optimisation.

    A count the engine did not sample is not the same as a count that matches. A count missing from
    some runs, or from one side only, cannot be compared and is reported; a count missing from every
    run on both sides is skipped, but at least one count has to be comparable.
    """
    problems = []
    comparable = 0
    for key in SCENE_COUNTS:
        base = [s.get(key) for s in base_scenes]
        cand = [s.get(key) for s in cand_scenes]
        if all(v is None for v in base + cand):
            continue
        missing = [(side, sum(v is None for v in values), len(values))
                   for side, values in (("base", base), ("candidate", cand)) if any(v is None for v in values)]
        if missing:
            detail = ", ".join(f"{side} has no value in {count} of {total} runs" for side, count, total in missing)
            problems.append(f"{key}: the scene count is not available on both sides ({detail})")
            continue
        comparable += 1
        b, c = median(base), median(cand)
        allowed = max(tolerance * max(b, c), INVENTORY_ABSOLUTE_SLACK)
        if abs(b - c) > allowed:
            problems.append(f"{key}: base median {b:g} vs candidate median {c:g} "
                            f"(allowed difference {allowed:g})")
    if not comparable and not problems and (base_scenes or cand_scenes):
        problems.append("no scene count was sampled in any run, so the two scenes could not be compared")
    return problems


# --------------------------------------------------------------------------------------------
# Compare


def analyze_scenario(name, pairs, *, seed, min_effect, inventory_tolerance=DEFAULT_INVENTORY_TOLERANCE,
                     samples=BOOTSTRAP_SAMPLES, top_spans=TOP_CPU_SPANS):
    """Verdict for one scenario from its pairs: [{"base": loaded run, "cand": loaded run}, ...]."""
    usable = [p for p in pairs if p["base"]["run"].get("valid") and p["cand"]["run"].get("valid")]
    result = {
        "status": "ok",
        "pairs_requested": len(pairs),
        "pairs_used": len(usable),
        "dropped_pairs": [i for i, p in enumerate(pairs) if p not in usable],
        "classifications": {
            "base": [p["base"]["run"]["classification"] for p in pairs],
            "cand": [p["cand"]["run"]["classification"] for p in pairs],
        },
    }
    if len(usable) < 2:
        result.update(status="refused", error="too_few_valid_pairs",
                      details=[f"{len(usable)} of {len(pairs)} pairs had a valid run on both sides"])
        return result
    problems = [f"{side}[{i}] {text}" for i, p in enumerate(usable) for side in ("base", "cand")
                for text in p[side].get("health_problems", [])]
    if problems:
        result.update(status="refused", error="unhealthy_run", details=problems)
        return result
    problems = check_window_modes([p["base"]["run"] for p in usable], [p["cand"]["run"] for p in usable])
    if problems:
        result.update(status="refused", error="window_mode_mismatch", details=problems)
        return result
    problems = check_fingerprints([p["base"]["fingerprint"] for p in usable],
                                  [p["cand"]["fingerprint"] for p in usable])
    if problems:
        result.update(status="refused", error="fingerprint_mismatch", details=problems)
        return result
    problems = check_inventory([p["base"]["scene"] for p in usable], [p["cand"]["scene"] for p in usable],
                               tolerance=inventory_tolerance)
    if problems:
        result.update(status="refused", error="inventory_mismatch", details=problems)
        return result
    names = set()
    for p in usable:
        names |= set(p["base"]["metrics"]) & set(p["cand"]["metrics"])

    def base_median(name):
        # A span below the sample floor is dropped from the runs it was rare in, so rank it by
        # the runs that have it.
        values = [p["base"]["metrics"][name] for p in usable if name in p["base"]["metrics"]]
        return median(values) if values else 0.0

    cpu = sorted((n for n in names if n.startswith("cpu/")), key=lambda n: -base_median(n))
    chosen = [n for n in sorted(names) if not n.startswith("cpu/")] + cpu[:top_spans]
    metrics = {}
    for metric in chosen:
        metrics[metric] = compare_metric(
            metric, [p["base"]["metrics"].get(metric) for p in usable],
            [p["cand"]["metrics"].get(metric) for p in usable],
            seed=f"{seed}:{name}:{metric}", min_effect=min_effect, samples=samples)
    result["metrics"] = metrics
    verdicts = {metrics[m]["verdict"] for m in PRIMARY_METRICS if m in metrics}
    # Each primary metric needs enough usable pairs of its own; a metric the reports lack, or have
    # too few values for, says nothing and must not read as a quiet "noise".
    incomplete = [m for m in PRIMARY_METRICS
                  if m not in metrics or metrics[m]["verdict"] in ("no_data", "insufficient_pairs")]
    if len(usable) < MIN_PAIRS_FOR_VERDICT:
        result["summary_verdict"] = "insufficient_pairs"
    elif incomplete:
        result["summary_verdict"] = "inconclusive"
        result["details"] = [f"{m}: missing or too few usable pairs" for m in incomplete]
    elif {"improved", "regressed"} <= verdicts:
        result["summary_verdict"] = "mixed"
    elif "regressed" in verdicts:
        result["summary_verdict"] = "regressed"
    elif "improved" in verdicts:
        result["summary_verdict"] = "improved"
    else:
        result["summary_verdict"] = "noise"
    warnings = []
    for p in usable:
        for side in ("base", "cand"):
            warnings += p[side]["run"].get("warnings", [])
    result["run_warnings"] = sorted(set(warnings))
    return result


def overall_verdict(scenarios):
    if any(s["status"] == "refused" for s in scenarios.values()):
        return "refused"
    verdicts = {s["summary_verdict"] for s in scenarios.values()}
    if "regressed" in verdicts and "improved" in verdicts or "mixed" in verdicts:
        return "mixed"
    for candidate in ("regressed", "inconclusive", "improved", "insufficient_pairs"):
        if candidate in verdicts:
            return candidate
    return "noise"


def build_identity(binary, commit=None, dirty=False):
    """The commit and dirty state of a binary, for the verdict.

    The engine's own fingerprint carries only the label the harness passes it, so the source of a
    binary has to come from outside. `build` names its output `engine-<sha>` and compiles it from a
    clean detached worktree, so that name is trusted; any other binary has no known commit unless
    the caller says which one it is, and an unknown commit also leaves the dirty state unknown.
    """
    if commit:
        return {"commit": commit, "dirty": bool(dirty), "commit_source": "flag"}
    match = re.fullmatch(r"engine-([0-9a-f]{7,40})", Path(binary).name)
    if match:
        return {"commit": match.group(1), "dirty": False, "commit_source": "binary name"}
    return {"commit": None, "dirty": None, "commit_source": None}


def build_warnings(verdict):
    """One line per side whose binary may not match a clean commit."""
    warnings = []
    for side, label in (("base", "base"), ("cand", "candidate")):
        info = verdict.get(side, {})
        if info.get("dirty"):
            warnings.append(f"the {label} binary was built from a dirty working tree; its commit "
                            f"({info.get('commit') or 'unknown'}) does not describe what was measured")
        elif not info.get("commit"):
            warnings.append(f"the {label} binary's commit is unknown (name it `engine-<sha>` or pass "
                            f"--{'cand' if side == 'cand' else 'base'}-commit); the comparison cannot be tied to source")
    return warnings


def _describe_side(info):
    commit = info.get("commit") or "unknown"
    state = {True: "dirty", False: "clean", None: "dirty state unknown"}[info.get("dirty")]
    return f"commit {commit}, {state}"


def format_value(name, value):
    if value is None:
        return "n/a"
    if name == "average_fps":
        return f"{value:.1f}"
    return f"{value:.3f}"


def noise_caveat(pairs):
    return (f"`noise` means not distinguishable from run-to-run variation at {pairs} pairs, not "
            "\"no change\": a real slowdown of around 10% often reads as `noise` at this pair count.")


MULTIPLE_COMPARISON_NOTE = (
    "Only the four frame-time rows decide a verdict. Every other row is uncorrected for multiple "
    "comparisons (a run lists about twenty metrics, so one of them is often flagged by chance): "
    "treat it as a lead to investigate, not a verdict.")


def summary_markdown(verdict):
    lines = ["# Perf comparison", "",
             f"- base: `{verdict['base']['path']}` (sha256 {verdict['base']['sha256'][:12]}, "
             f"{_describe_side(verdict['base'])})",
             f"- candidate: `{verdict['cand']['path']}` (sha256 {verdict['cand']['sha256'][:12]}, "
             f"{_describe_side(verdict['cand'])})",
             f"- pairs requested: {verdict['pairs']}, min effect {verdict['min_effect'] * 100:.1f}%, "
             f"seed {verdict['seed']}",
             f"- overall: **{verdict['overall']}**"]
    lines += [f"- WARNING: {w}" for w in build_warnings(verdict)]
    lines.append(f"- note: {MULTIPLE_COMPARISON_NOTE}")
    if verdict.get("window_mode"):
        lines.append(f"- window mode: {verdict['window_mode']}")
    if verdict.get("window_mode") == "hidden" and verdict.get("platform") == "Darwin":
        lines.append(f"- WARNING: {HIDDEN_WINDOW_MACOS_WARNING}")
    if verdict["same_binary"]:
        lines.append("- same binary on both sides: this is a noise-floor run, every verdict should be `noise`")
    for check in verdict.get("preflight", []):
        if check["status"] in ("warn", "fail"):
            lines.append(f"- preflight {check['status']}: {check['name']}: {check['detail']}")
    for name, scenario in verdict["scenarios"].items():
        lines += ["", f"## {name}", ""]
        if scenario["status"] == "refused":
            lines.append(f"REFUSED `{scenario['error']}`")
            lines += [f"- {d}" for d in scenario["details"]]
            continue
        lines.append(f"{scenario['pairs_used']} of {scenario['pairs_requested']} pairs used; "
                     f"summary verdict **{scenario['summary_verdict']}**")
        lines += [f"- {d}" for d in scenario.get("details", [])]
        if scenario["summary_verdict"] == "noise":
            lines.append(f"- {noise_caveat(scenario['pairs_used'])}")
        for warning in scenario.get("run_warnings", []):
            lines.append(f"- warning: {warning}")
        lines += ["", "| metric | base median | cand median | ratio (95% CI) | p | verdict |", "|---|---:|---:|---:|---:|---|"]
        hidden = 0
        for metric, row in scenario["metrics"].items():
            if metric not in PRIMARY_METRICS and metric != "average_fps" and row["verdict"] == "noise":
                hidden += 1
                continue
            ratio = "n/a" if row.get("ratio_median") is None else f"{row['ratio_median']:.3f}"
            ci = "" if row.get("ci_lo") is None else f" ({row['ci_lo']:.3f} to {row['ci_hi']:.3f})"
            p_text = "n/a" if row.get("p_value") is None else f"{row['p_value']:.3f}"
            lines.append(f"| {metric} | {format_value(metric, row.get('base_median'))} | "
                         f"{format_value(metric, row.get('cand_median'))} | "
                         f"{ratio}{ci} | {p_text} | {row['verdict']} |")
        if hidden:
            lines.append("")
            lines.append(f"{hidden} other metrics (GPU passes, CPU spans, memory) were noise and are not listed; "
                         "see verdict.json.")
    return "\n".join(lines) + "\n"


def compare(args):
    scenarios_all = load_scenarios()
    names = [n.strip() for n in args.scenarios.split(",") if n.strip()]
    for name in names:
        if name not in scenarios_all:
            raise HarnessError("unknown_scenario", f"{name}; known: {', '.join(scenarios_all)}")
    for label, binary in (("base", args.base), ("cand", args.cand)):
        if not os.access(binary, os.X_OK):
            raise HarnessError("bad_binary", f"--{label} {binary} is not an executable file")
    out = Path(args.out)
    if out.exists() and any(out.iterdir()) and not args.force:
        raise HarnessError("out_not_empty", f"{out} is not empty; pick a new directory or pass --force")
    out.mkdir(parents=True, exist_ok=True)
    checks = preflight_checks(out)
    failures = [c for c in checks if c["status"] == "fail"]
    for check in checks:
        print(f"preflight {check['status']:5} {check['name']}: {check['detail']}", file=sys.stderr)
    if failures and not args.force:
        raise HarnessError("preflight_failed", "; ".join(f"{c['name']}: {c['detail']}" for c in failures)
                           + " (pass --force to measure anyway)")
    sides = {"base": args.base, "cand": args.cand}
    rows = {name: [] for name in names}
    window = resolve_window(args.window, [args.base, args.cand])
    print(f"window mode: {window} (requested {args.window})", file=sys.stderr)
    for pair in range(args.pairs):
        order = ("base", "cand") if pair % 2 == 0 else ("cand", "base")
        for name in names:
            loaded = {}
            for side in order:
                run_dir = out / name / f"pair-{pair:02d}" / side
                print(f"run {name} pair {pair} {side} ...", file=sys.stderr, flush=True)
                run = run_scenario(sides[side], name, scenarios_all[name], assets=args.assets, out_dir=run_dir,
                                   duration=args.duration, warmup_frames=args.warmup_frames,
                                   timeout=args.timeout, grace=args.grace, window=window,
                                   run_id=f"{name}-{pair:02d}-{side}", commit=side)
                print(f"  -> {run['classification']} in {run['wall_seconds']}s", file=sys.stderr, flush=True)
                loaded[side] = load_run(run_dir)
                time.sleep(args.cooldown)
            rows[name].append(loaded)
    verdict = {
        "tool": "perf.py compare",
        "base": {"path": str(args.base), "sha256": sha256_file(args.base),
                 **build_identity(args.base, args.base_commit, args.base_dirty)},
        "cand": {"path": str(args.cand), "sha256": sha256_file(args.cand),
                 **build_identity(args.cand, args.cand_commit, args.cand_dirty)},
        "pairs": args.pairs, "min_effect": args.min_effect, "seed": args.seed,
        "window_mode": window, "hidden_window": window == "hidden", "platform": platform.system(),
        "preflight": checks,
        "scenarios": {},
    }
    verdict["same_binary"] = verdict["base"]["sha256"] == verdict["cand"]["sha256"]
    for name in names:
        verdict["scenarios"][name] = analyze_scenario(
            name, rows[name], seed=args.seed, min_effect=args.min_effect,
            inventory_tolerance=args.inventory_tolerance)
    verdict["overall"] = overall_verdict(verdict["scenarios"])
    (out / "verdict.json").write_text(json.dumps(verdict, indent=2), encoding="utf-8")
    (out / "summary.md").write_text(summary_markdown(verdict), encoding="utf-8")
    print((out / "summary.md").read_text(encoding="utf-8"))
    return 3 if verdict["overall"] == "refused" else 0


# --------------------------------------------------------------------------------------------
# Preflight (macOS)


def parse_power_source(text):
    match = re.search(r"Now drawing from '([^']+)'", text)
    return match.group(1) if match else None


def parse_cpu_speed_limit(text):
    match = re.search(r"CPU_Speed_Limit\s*=\s*(\d+)", text)
    return int(match.group(1)) if match else None


def parse_low_power_mode(text):
    """True when Low Power Mode is on. Newer macOS prints `powermode` (0 normal, 1 low, 2 high)."""
    match = re.search(r"^\s*(?:lowpowermode|powermode)\s+(\d+)", text, re.MULTILINE)
    return None if match is None else int(match.group(1)) == 1


def _command_output(command):
    try:
        return subprocess.run(command, capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return None


def running_processes(names):
    """Names from `names` with at least one running process, by exact executable name.

    A name ending in `*` matches any executable that starts with the rest, which is how the
    `engine-<sha>` binaries the harness builds are recognised.
    """
    found = []
    for name in names:
        pattern = ["pgrep", f"^{re.escape(name[:-1])}"] if name.endswith("*") else ["pgrep", "-x", name]
        output = _command_output(pattern)
        if output and output.strip():
            found.append(name)
    return found


def preflight_checks(out_dir=None, *, system=None, command_output=_command_output, free_bytes=None,
                     processes=running_processes):
    """A list of {name, status: pass|warn|fail|skip, detail}."""
    system = system or platform.system()
    checks = []

    def add(name, status, detail):
        checks.append({"name": name, "status": status, "detail": detail})

    target = str(out_dir or ".")
    free = free_bytes if free_bytes is not None else shutil.disk_usage(
        target if os.path.exists(target) else os.path.dirname(os.path.abspath(target))).free
    add("disk", "pass" if free >= MIN_FREE_RUN_GIB * GIB else "fail",
        f"{free / GIB:.1f} GiB free, need {MIN_FREE_RUN_GIB}")
    if system != "Darwin":
        add("macos", "skip", f"not macOS ({system}); power, thermal and low-power checks skipped")
        return checks
    batt = command_output(["pmset", "-g", "batt"])
    source = parse_power_source(batt or "")
    if source is None:
        add("ac_power", "warn", "pmset gave no power source")
    elif source == "AC Power":
        add("ac_power", "pass", "on AC power")
    else:
        add("ac_power", "fail", f"drawing from {source}; plug in")
    therm = command_output(["pmset", "-g", "therm"])
    limit = parse_cpu_speed_limit(therm or "")
    if therm is None:
        add("thermal", "warn", "pmset -g therm failed")
    elif limit is not None and limit < 100:
        add("thermal", "fail", f"CPU_Speed_Limit {limit}: the machine is throttled, let it cool")
    else:
        add("thermal", "pass", "no thermal limit recorded")
    low = parse_low_power_mode(command_output(["pmset", "-g"]) or "")
    if low is None:
        add("low_power_mode", "warn", "pmset reports no lowpowermode setting")
    elif low:
        add("low_power_mode", "fail", "Low Power Mode is on")
    else:
        add("low_power_mode", "pass", "Low Power Mode is off")
    busy = processes(["cargo", "rustc"])
    add("no_builds", "fail" if busy else "pass", f"running: {', '.join(busy)}" if busy else "no cargo or rustc")
    engines = [name for name in processes(["engine", "engine-*"]) if name]
    add("no_other_engine", "fail" if engines else "pass",
        "another engine process is running" if engines else "no engine process running")
    return checks


def preflight(args):
    checks = preflight_checks(args.out or ".")
    for check in checks:
        print(f"{check['status']:5} {check['name']}: {check['detail']}")
    return 3 if any(c["status"] == "fail" for c in checks) else 0


# --------------------------------------------------------------------------------------------
# Screenshots: capture and PNG handling


def read_shot_names(shots_file):
    data = json.loads(Path(shots_file).read_text(encoding="utf-8"))
    return [shot["name"] for shot in data["shots"] if not shot.get("interior_cell_id")]


def capture_shots(engine, shots_file, assets, out_dir, *, worldspace="0x3c", timeout=300.0, grace=15.0,
                  extra_args=(), poll=0.2, window="visible"):
    """Capture screenshots. A hidden window gives black frames on macOS, so it is refused there."""
    if window == "hidden" and platform.system() == "Darwin":
        raise HarnessError("hidden_window_black_frames", "shots never use a hidden window on macOS")
    out_dir = Path(out_dir)
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    names = read_shot_names(shots_file)
    command = [str(engine), "--assets", str(assets), "--allow-incomplete-assets", "--worldspace", worldspace,
               "--shots", str(shots_file), "--shots-out", str(out_dir)]
    command += {"hidden": [HIDDEN_FLAG], "background": [BACKGROUND_FLAG]}.get(window, [])
    command += list(extra_args)

    def all_written():
        return all((out_dir / f"{n}.png").is_file() and (out_dir / f"{n}.png").stat().st_size > 0 for n in names)

    watch = watch_process(command, stdout_path=out_dir.parent / f"{out_dir.name}.stdout.log",
                          stderr_path=out_dir.parent / f"{out_dir.name}.stderr.log", is_done=all_written,
                          timeout=timeout, grace=grace, poll=poll)
    classification = classify(watch)
    run = {"classification": classification, "valid": classification in VALID_CLASSES, "command": command,
           "window_mode": window, "binary_sha256": sha256_file(engine), "shots": names, "exit_code": watch["exit_code"],
           "killed": watch["killed"], "wall_seconds": watch["wall_seconds"]}
    (out_dir.parent / f"{out_dir.name}.run.json").write_text(json.dumps(run, indent=2), encoding="utf-8")
    return run


class PngError(Exception):
    pass


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
_CHANNELS = {0: 1, 2: 3, 4: 2, 6: 4}


def _paeth_row(line, prev, bpp):
    out = bytearray(line)
    for i in range(len(out)):
        a = out[i - bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i - bpp] if i >= bpp else 0
        p = a + b - c
        pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
        predictor = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
        out[i] = (out[i] + predictor) & 255
    return out


def decode_png(path):
    """Decode an 8-bit, non-interlaced grey, grey+alpha, RGB or RGBA PNG: (width, height, channels, bytes)."""
    data = Path(path).read_bytes()
    if data[:8] != PNG_SIGNATURE:
        raise PngError(f"{path}: not a PNG")
    pos, header, idat = 8, None, []
    while pos + 8 <= len(data):
        length = int.from_bytes(data[pos:pos + 4], "big")
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            header = body
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    if header is None or not idat:
        raise PngError(f"{path}: truncated PNG")
    width, height = int.from_bytes(header[0:4], "big"), int.from_bytes(header[4:8], "big")
    depth, ctype, _, _, interlace = header[8], header[9], header[10], header[11], header[12]
    if depth != 8 or ctype not in _CHANNELS or interlace != 0:
        raise PngError(f"{path}: unsupported PNG (depth {depth}, colour type {ctype}, interlace {interlace})")
    channels = _CHANNELS[ctype]
    stride = width * channels
    try:
        raw = zlib.decompress(b"".join(idat))
    except zlib.error as error:
        raise PngError(f"{path}: bad image data ({error})") from error
    if len(raw) != (stride + 1) * height:
        raise PngError(f"{path}: image data is {len(raw)} bytes, expected {(stride + 1) * height}")
    pixels = bytearray(stride * height)
    prev = bytes(stride)
    for y in range(height):
        start = y * (stride + 1)
        kind = raw[start]
        line = raw[start + 1:start + 1 + stride]
        if kind == 0:
            row = bytearray(line)
        elif kind == 1:
            row = bytearray(line)
            for c in range(channels):
                row[c::channels] = bytes(itertools.accumulate(line[c::channels], lambda a, b: (a + b) & 255))
        elif kind == 2:
            row = bytearray((a + b) & 255 for a, b in zip(line, prev))
        elif kind == 3:
            row = bytearray(line)
            for i in range(stride):
                left = row[i - channels] if i >= channels else 0
                row[i] = (row[i] + ((left + prev[i]) >> 1)) & 255
        elif kind == 4:
            row = _paeth_row(line, prev, channels)
        else:
            raise PngError(f"{path}: unknown filter {kind} on row {y}")
        pixels[y * stride:(y + 1) * stride] = row
        prev = row
    return width, height, channels, bytes(pixels)


def write_png(path, width, height, channels, pixels):
    """Write an 8-bit non-interlaced PNG (grey, RGB or RGBA by channel count 1, 3, 4)."""
    ctype = {1: 0, 3: 2, 4: 6}[channels]
    stride = width * channels
    raw = b"".join(b"\x00" + pixels[y * stride:(y + 1) * stride] for y in range(height))

    def chunk(kind, body):
        return (len(body).to_bytes(4, "big") + kind + body
                + (zlib.crc32(kind + body) & 0xFFFFFFFF).to_bytes(4, "big"))

    header = width.to_bytes(4, "big") + height.to_bytes(4, "big") + bytes([8, ctype, 0, 0, 0])
    Path(path).write_bytes(PNG_SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 6))
                           + chunk(b"IEND", b""))


def _color_planes(image):
    """The colour planes (alpha dropped) of a decoded image as a list of bytes objects."""
    _, _, channels, pixels = image
    colour = 1 if channels in (1, 2) else 3
    return [pixels[c::channels] for c in range(colour)]


def blank_stats(image, stride=7):
    """Mean and standard deviation of the colour samples; a blank frame has almost no spread."""
    samples = bytes().join(plane[::stride] for plane in _color_planes(image))
    count = len(samples)
    mean = sum(samples) / count
    variance = sum((s - mean) ** 2 for s in samples) / count
    std = math.sqrt(variance)
    return {"mean": mean, "std": std, "blank": std < 0.5, "black": mean < 3.0 and std < 3.0}


def pixel_diff(a, b, threshold=8):
    """Mean abs difference (0-255 scale) and the fraction of colour samples differing by more than threshold."""
    if a[:2] != b[:2]:
        raise PngError(f"size mismatch: {a[0]}x{a[1]} vs {b[0]}x{b[1]}")
    total, over, count, worst = 0, 0, 0, 0
    for pa, pb in zip(_color_planes(a), _color_planes(b)):
        if pa == pb:
            count += len(pa)
            continue
        diffs = bytes(map(abs, map(int.__sub__, pa, pb)))
        total += sum(diffs)
        over += sum(1 for d in diffs if d > threshold)
        worst = max(worst, max(diffs))
        count += len(diffs)
    return {"mean_abs_diff": total / count, "fraction_over": over / count, "max_diff": worst,
            "threshold": threshold}


def flip_available():
    try:
        import flip_evaluator  # noqa: F401
        import numpy  # noqa: F401
    except ImportError:
        return False
    return True


def flip_diff(a, b, heatmap_path=None):
    """NVIDIA FLIP between two decoded images; needs numpy and flip-evaluator."""
    import numpy as np
    import flip_evaluator as flip

    def rgb(image):
        width, height, channels, pixels = image
        array = np.frombuffer(pixels, dtype=np.uint8).reshape(height, width, channels)
        array = array[:, :, :3] if channels >= 3 else np.repeat(array[:, :, :1], 3, axis=2)
        return array.astype(np.float32) / 255.0

    error_map, mean, _ = flip.evaluate(rgb(a), rgb(b), "LDR", inputsRGB=True, applyMagma=False,
                                       computeMeanError=True)
    error = np.asarray(error_map, dtype=np.float32).reshape(a[1], a[0])
    result = {"mean_flip": float(mean), "p99_flip": float(np.percentile(error, 99)),
              "fraction_over": float((error > 0.1).mean()), "threshold": 0.1}
    if heatmap_path is not None:
        ramp = np.clip(error * 2.0, 0, 1)
        heat = np.stack([ramp * 255, np.clip(error * 2.0 - 1.0, 0, 1) * 255, (1 - ramp) * 80], axis=2)
        write_png(heatmap_path, a[0], a[1], 3, heat.astype(np.uint8).tobytes())
    return result


def choose_metric(force_stdlib=False):
    """flip when it is installed and works, else the stdlib metric, with the reason it was not flip."""
    if force_stdlib:
        return "stdlib", None
    if not flip_available():
        return "stdlib", None
    try:
        probe = (4, 4, 3, bytes(range(48)))
        flip_diff(probe, probe)
    except Exception as error:  # the extra's API is outside this repo's control
        return "stdlib", f"flip-evaluator is installed but failed ({error}); used the stdlib metric"
    return "flip", None


DEFAULT_MARGINS = {
    "stdlib": {"mean": 0.5, "fraction": 0.005},
    "flip": {"mean": 0.005, "fraction": 0.005},
}


def visual_compare(base_dir, cand_dir, out_dir, *, noise_floor=None, margin_mean=None, margin_fraction=None,
                   force_stdlib=False):
    base_dir, cand_dir, out_dir = Path(base_dir), Path(cand_dir), Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    metric, metric_warning = choose_metric(force_stdlib)
    margins = dict(DEFAULT_MARGINS[metric])
    if margin_mean is not None:
        margins["mean"] = margin_mean
    if margin_fraction is not None:
        margins["fraction"] = margin_fraction
    floor = None
    if noise_floor:
        floor = json.loads(Path(noise_floor).read_text(encoding="utf-8"))
        if floor.get("metric") != metric:
            raise HarnessError("noise_floor_metric_mismatch",
                               f"the noise floor used {floor.get('metric')}, this run uses {metric}")
    names = sorted({p.stem for p in base_dir.glob("*.png")} | {p.stem for p in cand_dir.glob("*.png")})
    result = {"tool": "perf.py visual", "metric": metric, "margins": margins, "shots": {}, "review": []}
    if metric_warning:
        result["warnings"] = [metric_warning]
    sizes = {"base": {}, "cand": {}}
    images = {}
    for name in names:
        entry = {"status": "pass", "reasons": []}
        result["shots"][name] = entry
        paths = {"base": base_dir / f"{name}.png", "cand": cand_dir / f"{name}.png"}
        for side, path in paths.items():
            if not path.is_file():
                entry["reasons"].append(f"missing_{side}")
                continue
            sizes[side][name] = path.stat().st_size
            try:
                images[(side, name)] = decode_png(path)
            except PngError as error:
                entry["reasons"].append(f"undecodable_{side}: {error}")
                continue
            stats = blank_stats(images[(side, name)])
            entry[f"{side}_stats"] = {k: round(v, 3) if isinstance(v, float) else v for k, v in stats.items()}
            if stats["blank"] or stats["black"]:
                entry["reasons"].append(f"blank_frame_{side}")
        if ("base", name) in images and ("cand", name) in images:
            try:
                if metric == "flip":
                    heat = out_dir / f"{name}.flip.png"
                    measured = flip_diff(images[("base", name)], images[("cand", name)], heat)
                    measured["heatmap"] = str(heat)
                    mean, fraction = measured["mean_flip"], measured["fraction_over"]
                else:
                    measured = pixel_diff(images[("base", name)], images[("cand", name)])
                    mean, fraction = measured["mean_abs_diff"], measured["fraction_over"]
            except PngError as error:
                entry["reasons"].append(f"size_mismatch: {error}")
            else:
                entry["measured"] = measured
                base_floor = (floor or {}).get("measured", {}).get(name, {})
                limit_mean = base_floor.get("mean", 0.0) + margins["mean"]
                limit_fraction = base_floor.get("fraction", 0.0) + margins["fraction"]
                entry["limits"] = {"mean": limit_mean, "fraction": limit_fraction}
                entry["values"] = {"mean": mean, "fraction": fraction}
                if mean > limit_mean:
                    entry["reasons"].append(f"mean_difference {mean:.4f} > {limit_mean:.4f}")
                if fraction > limit_fraction:
                    entry["reasons"].append(f"fraction_over {fraction:.4f} > {limit_fraction:.4f}")
        if entry["reasons"]:
            entry["status"] = "fail"
    for side in ("base", "cand"):
        if len(sizes[side]) >= 2 and len(set(sizes[side].values())) == 1:
            result.setdefault("warnings", []).append(
                f"every {side} shot has the same file size ({next(iter(sizes[side].values()))} bytes): "
                "the signature of a black capture run")
            result["suspect_black_run"] = True
    result["measured"] = {n: e["values"] for n, e in result["shots"].items() if "values" in e}
    result["review"] = sorted(n for n, e in result["shots"].items() if e["status"] != "pass")
    if result.get("suspect_black_run"):
        result["review"] = sorted(names)
    result["overall"] = "fail" if result["review"] else "pass"
    (out_dir / "visual.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    return result


# --------------------------------------------------------------------------------------------
# Build


def _run(command, **kwargs):
    return subprocess.run(command, capture_output=True, text=True, check=False, **kwargs)


def git_toplevel(start):
    result = _run(["git", "-C", str(start), "rev-parse", "--show-toplevel"])
    if result.returncode != 0:
        raise HarnessError("not_a_repository", f"{start}: {result.stderr.strip()}")
    return Path(result.stdout.strip())


def resolve_sha(ref, repo, runner=_run):
    result = runner(["git", "-C", str(repo), "rev-parse", "--verify", f"{ref}^{{commit}}"])
    if result.returncode != 0:
        raise HarnessError("unknown_ref", f"{ref}: {result.stderr.strip()}")
    return result.stdout.strip()


def plan_build(ref, *, repo, worktree_root, bin_dir, runner=_run):
    sha = resolve_sha(ref, repo, runner)
    short = sha[:8]
    return {"ref": ref, "sha": sha, "short": short, "binary": Path(bin_dir) / f"engine-{short}",
            "worktree": Path(worktree_root) / f"perf-{short}"}


def build(ref, *, repo, worktree_root, bin_dir, runner=_run, free_bytes=None, keep_worktree=False,
          min_free_gib=MIN_FREE_BUILD_GIB, dry_run=False, environ=None):
    plan = plan_build(ref, repo=repo, worktree_root=worktree_root, bin_dir=bin_dir, runner=runner)
    plan["reused"] = plan["binary"].is_file()
    if plan["reused"] or dry_run:
        return plan
    Path(worktree_root).mkdir(parents=True, exist_ok=True)
    Path(bin_dir).mkdir(parents=True, exist_ok=True)
    free = free_bytes if free_bytes is not None else shutil.disk_usage(worktree_root).free
    if free < min_free_gib * GIB:
        raise HarnessError("insufficient_disk",
                           f"{free / GIB:.1f} GiB free under {worktree_root}, a build needs {min_free_gib}")
    worktree = plan["worktree"]
    if worktree.exists():
        head = runner(["git", "-C", str(worktree), "rev-parse", "HEAD"]).stdout.strip()
        if head != plan["sha"]:
            raise HarnessError("worktree_conflict", f"{worktree} exists at {head[:8]}, not {plan['short']}")
    else:
        added = runner(["git", "-C", str(repo), "worktree", "add", "--detach", str(worktree), plan["sha"]])
        if added.returncode != 0:
            raise HarnessError("worktree_failed", added.stderr.strip())
    try:
        env = dict(os.environ if environ is None else environ)
        # A shared target directory lets cargo reuse another tree's test and build outputs, and it
        # defeats sccache keys; each build gets the target directory inside its own worktree.
        env.pop("CARGO_TARGET_DIR", None)
        env["CARGO_INCREMENTAL"] = "0"
        built = runner(["cargo", "build", "--locked", "--release", "-p", "engine", "--bin", "engine"],
                       cwd=str(worktree), env=env)
        if built.returncode != 0:
            raise HarnessError("build_failed", (built.stderr or "").strip()[-2000:])
        produced = worktree / "target" / "release" / "engine"
        if not produced.is_file():
            raise HarnessError("binary_missing", f"{produced} was not produced")
        shutil.copy2(produced, plan["binary"])
        plan["binary"].chmod(0o755)
    finally:
        # A failed build leaves a target directory of 9-20 GiB; only --keep-worktree, which exists
        # for debugging the build, may keep it.
        if not keep_worktree:
            runner(["git", "-C", str(repo), "worktree", "remove", "--force", str(worktree)])
    plan["built"] = True
    return plan


# --------------------------------------------------------------------------------------------
# Analysing a run: summarize


def resolve_report_path(run_dir, run):
    """The report of a run directory. run.json may hold a path relative to where the run started."""
    run_dir = Path(run_dir)
    recorded = run.get("report")
    for candidate in ([Path(recorded)] if recorded else []) + [run_dir / "report.json"]:
        if candidate.is_file():
            return candidate
    raise HarnessError("report_missing", f"{run_dir} has no report.json (the run produced no complete report)")


def load_run_for_analysis(run_dir):
    run_dir = Path(run_dir)
    try:
        run = json.loads((run_dir / "run.json").read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise HarnessError("run_unreadable", f"{run_dir}/run.json: {error}") from error
    report = json.loads(resolve_report_path(run_dir, run).read_text(encoding="utf-8"))
    return run, report


def span_row(name, summary, frames):
    count = summary.get("count") or 0
    return {"name": name, "count": count, "mean_ms": summary.get("mean"), "p95_ms": summary.get("p95"),
            "worst_ms": summary.get("worst"), "total_ms": summary.get("total"),
            "ms_per_frame": (summary.get("total") or 0.0) / frames if frames else None}


def split_cpu_spans(report):
    """Split the report's CPU spans and schedules by how often they fire.

    A span that fires about once per frame (count at least half the measured frames) adds to the
    frame time, so it is ranked by its mean. The rest are asynchronous latencies: a database
    request or an asset becoming ready is measured from submission to completion across many
    frames, and its mean says nothing about frame cost. A span whose mean exceeds twice the frame
    time cannot be a per-frame cost whatever its count (`assets/model_ready` has about as many
    samples as the window has frames only because a scene has about as many assets). Counter-like
    spans (`assets_added/*` hold counts, not milliseconds) are listed apart.
    """
    frames = report.get("frames") or 0
    frame_ms = report.get("frame_ms_mean")
    cpu = report.get("cpu") or {}
    per_frame, asynchronous, counters = [], [], []
    for group in ("schedules", "spans"):
        for name, summary in sorted((cpu.get(group) or {}).items()):
            row = span_row(name, summary, frames)
            if name.startswith(COUNTER_SPAN_PREFIXES):
                counters.append(row)
            elif (frames and row["count"] >= PER_FRAME_SPAN_SHARE * frames
                  and (not frame_ms or (row["mean_ms"] or 0.0) <= 2 * frame_ms)):
                per_frame.append(row)
            else:
                asynchronous.append(row)
    per_frame.sort(key=lambda r: -(r["mean_ms"] or 0.0))
    asynchronous.sort(key=lambda r: -(r["mean_ms"] or 0.0))
    return per_frame, asynchronous, counters


def span_mean(report, name):
    summary = ((report.get("cpu") or {}).get("spans") or {}).get(name)
    return summary.get("mean") if isinstance(summary, dict) else None


def thread_budget(report):
    """Where the frame time goes: main world, extraction, the wait for the render thread, the render thread."""
    frame = report.get("frame_ms_mean")
    budget = {"frame_ms": frame,
              "main_world_ms": span_mean(report, "render_world/main_world"),
              "extract_ms": span_mean(report, "render_world/extract"),
              "wait_for_render_thread_ms": span_mean(report, "render_world/wait_for_render_thread"),
              "render_thread_ms": span_mean(report, "render_world/render_thread"),
              "render_prepare_ms": span_mean(report, "render_world/render/prepare"),
              "render_graph_and_present_ms": span_mean(report, "render_world/render/graph_and_present")}
    budget["shares"] = {key[:-3]: (value / frame if frame and value is not None else None)
                        for key, value in budget.items() if key.endswith("_ms") and key != "frame_ms"}
    return budget


def classify_bound(budget, gpu=None):
    """gpu-bound, render-thread-bound, main-thread-bound or unclear, with the numbers behind it."""
    frame = budget.get("frame_ms")
    shares = budget.get("shares", {})
    if gpu and gpu.get("busy_fraction") is not None and gpu["busy_fraction"] >= GPU_BUSY_BOUND:
        per_frame = gpu.get("gpu_ms_per_frame")
        numbers = f"GPU busy {gpu['busy_fraction'] * 100:.0f}% of wall"
        if per_frame is not None and frame:
            numbers += f" ({per_frame:.1f} ms of {frame:.1f} ms per frame)"
        verdict = {"class": "gpu-bound", "evidence": [numbers]}
        if gpu.get("acquisitions") == 0:
            verdict["evidence"].append("the trace has no drawable acquisitions: presentation is not included")
        return verdict
    if not frame or shares.get("render_thread") is None or shares.get("wait_for_render_thread") is None:
        return {"class": "unclear", "evidence": ["the report has no render_world thread spans"]}
    render, wait = shares["render_thread"], shares["wait_for_render_thread"]
    main = (shares.get("main_world") or 0.0) + (shares.get("extract") or 0.0)
    numbers = (f"render thread {budget['render_thread_ms']:.1f} ms = {render * 100:.0f}% of the "
               f"{frame:.1f} ms frame; main world + extract {main * 100:.0f}%; "
               f"main thread waits for the render thread {wait * 100:.0f}%")
    if render >= THREAD_BUSY_BOUND and wait >= WAIT_SHARE_RENDER_BOUND:
        evidence = [numbers]
        if gpu is None:
            evidence.append("render thread time includes waiting on the GPU and on presentation; "
                            "run gpu-busy on a Metal trace to tell CPU work from GPU wait")
        return {"class": "render-thread-bound", "evidence": evidence}
    if main >= THREAD_BUSY_BOUND and wait < WAIT_SHARE_MAIN_BOUND:
        return {"class": "main-thread-bound", "evidence": [numbers]}
    evidence = [numbers]
    if gpu and gpu.get("busy_fraction") is not None:
        evidence.append(f"GPU busy {gpu['busy_fraction'] * 100:.0f}% of wall, below {GPU_BUSY_BOUND * 100:.0f}%")
    return {"class": "unclear", "evidence": evidence}


def summarize_run(run_dir, gpu=None):
    """Everything `summarize` prints, as a dict."""
    run, report = load_run_for_analysis(run_dir)
    per_frame, asynchronous, counters = split_cpu_spans(report)
    budget = thread_budget(report)
    fingerprint = report.get("fingerprint") or {}
    system = (report.get("system") or {}).get("os") or ""
    warnings = list(run.get("warnings") or [])
    if run.get("classification") not in (None, "ok", "hung_after_report"):
        warnings.append(f"the run is classified {run['classification']}")
    mode = window_mode_of(run)
    if mode == "hidden" and (system.startswith("macOS") or platform.system() == "Darwin"):
        warnings.append(HIDDEN_WINDOW_MACOS_WARNING)
    if report.get("world_ready_reached") is False and not any("never became ready" in w for w in warnings):
        warnings.append("the world never became ready inside the window")
    ready, warmup = report.get("frames_to_world_ready"), report.get("warmup_frames")
    if ready is not None and warmup is not None and ready > warmup and not any("world ready at frame" in w for w in warnings):
        warnings.append(f"world ready at frame {ready}, after the {warmup}-frame warm-up")
    passes = (report.get("gpu_passes") or {}).get("passes") or {}
    if passes and not any((_mean_of(v) or 0) > 0 for v in passes.values()):
        warnings.append("every GPU pass timing is zero (Metal timestamp queries): the report has no GPU split")
    if gpu and gpu.get("never_acquired_drawable"):
        warnings.append(gpu["never_acquired_message"])
    frames = {key: report.get(key) for key in ("frames", "warmup_frames", "elapsed_seconds", "average_fps",
                                              "frame_ms_mean", "frame_ms_p50", "frame_ms_p95", "frame_ms_p99",
                                              "frame_ms_worst")}
    return {
        "run_dir": str(run_dir), "scenario": run.get("scenario") or report.get("scenario"),
        "classification": run.get("classification"), "window_mode": mode,
        "binary_sha256": run.get("binary_sha256"), "profiler": (run.get("profiler") or {}).get("name"),
        "resolution": fingerprint.get("resolution"), "frames": frames,
        "per_frame_spans": per_frame, "asynchronous_latencies": asynchronous, "counters": counters,
        "thread_budget": budget, "gpu": gpu, "bound": classify_bound(budget, gpu),
        "scene": {**extract_scene(report), "entity_count": report.get("entity_count"),
                  "world_ready_reached": report.get("world_ready_reached"),
                  "frames_to_world_ready": ready},
        "fingerprint": fingerprint, "warnings": warnings,
    }


def _ms(value, digits=2):
    return "n/a" if value is None else f"{value:.{digits}f}"


def format_summary(summary, top=8):
    f = summary["frames"]
    lines = [f"run {summary['run_dir']}",
             f"scenario {summary['scenario']}, window {summary['window_mode']}"
             + (f", profiler {summary['profiler']}" if summary.get("profiler") else "")
             + (f", resolution {summary['resolution'][0]}x{summary['resolution'][1]}" if summary.get("resolution") else ""),
             "",
             f"frames: {f['frames']} measured in {_ms(f['elapsed_seconds'], 1)} s, mean {_ms(f['frame_ms_mean'])} ms "
             f"({_ms(f['average_fps'], 1)} fps), p50 {_ms(f['frame_ms_p50'])}, p95 {_ms(f['frame_ms_p95'])}, "
             f"p99 {_ms(f['frame_ms_p99'])}, worst {_ms(f['frame_ms_worst'])} ms"]
    b = summary["thread_budget"]
    lines += ["", "thread budget (mean ms per frame; spans nest and threads overlap, so rows do not add up)"]
    for label, key in (("frame time", "frame_ms"), ("main world", "main_world_ms"), ("extract", "extract_ms"),
                       ("wait for render thread", "wait_for_render_thread_ms"),
                       ("render thread", "render_thread_ms"), ("  render/prepare", "render_prepare_ms"),
                       ("  render/graph_and_present", "render_graph_and_present_ms")):
        value = b.get(key)
        share = "" if key == "frame_ms" or value is None or not b.get("frame_ms") else f"  {value / b['frame_ms'] * 100:4.0f}% of frame"
        lines.append(f"  {label:28} {_ms(value):>8}{share}")
    gpu = summary.get("gpu")
    if gpu:
        lines.append(f"  {'gpu busy (trace)':28} {_ms(gpu.get('gpu_ms_per_frame')):>8}"
                     f"  {(gpu.get('busy_fraction') or 0) * 100:4.0f}% of wall")
    lines += ["", f"bound: {summary['bound']['class']}"] + [f"  {e}" for e in summary["bound"]["evidence"]]
    lines += ["", f"per-frame CPU spans (count >= half the frames; mean ms, ranked)"]
    lines += [f"  {r['name']:52} {_ms(r['mean_ms'], 3):>9} ms  p95 {_ms(r['p95_ms'], 3):>8}  n={r['count']}"
              for r in summary["per_frame_spans"][:top]] or ["  none"]
    lines += ["", "asynchronous latencies (submission to completion; not frame cost; mean ms, ranked)"]
    lines += [f"  {r['name']:52} {_ms(r['mean_ms'], 1):>9} ms  p95 {_ms(r['p95_ms'], 1):>8}  n={r['count']}"
              for r in summary["asynchronous_latencies"][:top]] or ["  none"]
    s = summary["scene"]
    lines += ["", f"scene: {s.get('mesh_entities')} mesh entities, {s.get('terrain_patches')} terrain patches, "
                  f"{s.get('water_surfaces')} water surfaces, {s.get('entity_count')} entities; world ready at frame "
                  f"{s.get('frames_to_world_ready')}"]
    if summary["warnings"]:
        lines += ["", "warnings"] + [f"  - {w}" for w in summary["warnings"]]
    return "\n".join(lines) + "\n"


def _delta(a, b):
    """(a, b, b - a, percent change) with None when either side is missing."""
    if a is None or b is None:
        return a, b, None, None
    return a, b, b - a, ((b - a) / a * 100.0 if a else None)


def compare_summaries(base, this):
    """Per-metric deltas between two summaries: `base` is the --against run, `this` the primary run."""
    rows = []

    def add(group, name, a, b):
        a, b, d, pct = _delta(a, b)
        rows.append({"group": group, "metric": name, "against": a, "this": b, "delta": d, "percent": pct})

    for key in ("frame_ms_mean", "frame_ms_p50", "frame_ms_p95", "frame_ms_p99", "average_fps"):
        add("frame", key, base["frames"].get(key), this["frames"].get(key))
    for key in ("main_world_ms", "extract_ms", "wait_for_render_thread_ms", "render_thread_ms",
                "render_prepare_ms", "render_graph_and_present_ms"):
        add("thread", key, base["thread_budget"].get(key), this["thread_budget"].get(key))
    if base.get("gpu") and this.get("gpu"):
        add("gpu", "busy_fraction", base["gpu"].get("busy_fraction"), this["gpu"].get("busy_fraction"))
        add("gpu", "gpu_ms_per_frame", base["gpu"].get("gpu_ms_per_frame"), this["gpu"].get("gpu_ms_per_frame"))
    for group, key in (("per-frame span", "per_frame_spans"), ("async span", "asynchronous_latencies")):
        left = {r["name"]: r["mean_ms"] for r in base[key]}
        right = {r["name"]: r["mean_ms"] for r in this[key]}
        for name in sorted(set(left) | set(right)):
            add(group, name, left.get(name), right.get(name))
    notes = []
    if base["window_mode"] != this["window_mode"]:
        notes.append(f"window modes differ ({base['window_mode']} vs {this['window_mode']}): presentation cost "
                     "is part of the difference")
    if base.get("scenario") != this.get("scenario"):
        notes.append(f"scenarios differ ({base.get('scenario')} vs {this.get('scenario')})")
    notes += [f"conditions differ: {line}" for line in fingerprint_diff(base.get("fingerprint"), this.get("fingerprint"))]
    for key in SCENE_COUNTS:
        a, b = base["scene"].get(key), this["scene"].get(key)
        if a is not None and b is not None and abs(a - b) > max(DEFAULT_INVENTORY_TOLERANCE * max(a, b), INVENTORY_ABSOLUTE_SLACK):
            notes.append(f"scene differs: {key} {a} vs {b}")
    return {"against": base["run_dir"], "this": this["run_dir"], "rows": rows, "notes": notes}


def format_comparison(comparison, top=8):
    lines = ["", f"changes: {comparison['this']} (this) against {comparison['against']}"]
    for note in comparison["notes"]:
        lines.append(f"  note: {note}")
    groups = collections.OrderedDict()
    for row in comparison["rows"]:
        groups.setdefault(row["group"], []).append(row)
    for group, rows in groups.items():
        if group.endswith("span"):
            rows = sorted((r for r in rows if r["delta"] is not None), key=lambda r: -abs(r["delta"]))[:top]
        lines.append(f"  {group}")
        for r in rows:
            pct = "" if r["percent"] is None else f" ({r['percent']:+.0f}%)"
            digits = 3 if r["metric"] == "busy_fraction" else 2
            delta = "n/a" if r["delta"] is None else f"{r['delta']:+.{digits}f}"
            lines.append(f"    {r['metric']:50} {_ms(r['against'], digits):>9} -> {_ms(r['this'], digits):>9}  {delta}{pct}")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------------------------
# Analysing a run: gpu-busy (Metal trace)

TRACE_PREFIX = b'{"type":"'
# The observer writes compact JSON; any other spacing still parses, just through the slower pattern.
TRACE_TYPE_PATTERN = re.compile(rb'\{\s*"type"\s*:\s*"([^"]*)"')
TRACE_LOSS_COUNTERS = ("dropped", "exceptions", "hook_failures", "thread_cpu_clock_failures",
                       "trace_config_errors", "capture_config_errors")
NEVER_ACQUIRED_MESSAGE = ("this run never acquired a drawable (hidden window?): timings exclude presentation")


def merge_intervals(intervals):
    """Sorted, non-overlapping union of (start, end) pairs."""
    merged = []
    for start, end in sorted(intervals):
        if merged and start <= merged[-1][1]:
            if end > merged[-1][1]:
                merged[-1][1] = end
        else:
            merged.append([start, end])
    return merged


def analyze_metal_trace(path, *, fps=None, crop=DEFAULT_TRACE_CROP_SECONDS):
    """Stream a native Metal trace (ndjson) and report GPU busy time, command buffers and drawables.

    The files are hundreds of megabytes, so lines are read as bytes and only those of a type the
    analysis needs are decoded. A truncated or malformed line, and a capture with dropped events,
    are reported rather than refused: the counts say how much to trust the numbers.
    """
    path = Path(path)
    counts = collections.Counter()
    bad_lines = 0
    begin = None
    intervals, invalid_completed = [], 0
    acquisitions, presents, present_encodes = [], [], []
    health_last, loss_max, rows_attempted_max = None, collections.Counter(), 0
    lines = 0
    with open(path, "rb") as handle:
        for raw in handle:
            if raw.startswith(TRACE_PREFIX):
                kind = raw[len(TRACE_PREFIX):raw.find(b'"', len(TRACE_PREFIX))]
            else:
                match = TRACE_TYPE_PATTERN.match(raw)
                if match is None:
                    if raw.strip():
                        bad_lines += 1
                    continue
                kind = match.group(1)
            lines += 1
            counts[kind] += 1
            if kind not in (b"completed", b"acquire_end", b"presented", b"present_encode_begin", b"health",
                            b"trace_begin"):
                continue
            try:
                row = json.loads(raw)
            except ValueError:
                bad_lines += 1
                continue
            if kind == b"completed":
                start, finish = row.get("gpu_start_s") or 0.0, row.get("gpu_end_s") or 0.0
                # Empty command buffers (transitions, pre-passes) complete with start == end.
                if row.get("gpu_time_valid") and finish >= start > 0:
                    intervals.append((start, finish))
                else:
                    invalid_completed += 1
            elif kind == b"acquire_end":
                acquisitions.append((row.get("end_ns") or row.get("host_ns") or 0) / 1e9)
            elif kind == b"presented":
                presents.append((row.get("host_ns") or 0) / 1e9)
            elif kind == b"present_encode_begin":
                present_encodes.append((row.get("host_ns") or 0) / 1e9)
            elif kind == b"health":
                health_last = row
                for counter in TRACE_LOSS_COUNTERS:
                    loss_max[counter] = max(loss_max[counter], row.get(counter) or 0)
                rows_attempted_max = max(rows_attempted_max, row.get("rows_attempted") or 0)
            else:
                begin = row
    result = {"path": str(path), "size_bytes": path.stat().st_size, "lines": lines, "bad_lines": bad_lines,
              "event_counts": {k.decode(): v for k, v in counts.most_common(12)}}
    warnings = []
    max_rows = (begin or {}).get("max_rows")
    row_cap = bool(max_rows and rows_attempted_max > max_rows)
    health = {"records": counts[b"health"], "rows_attempted": rows_attempted_max, "max_rows": max_rows,
              "row_cap_reached": row_cap, "loss_max": dict(loss_max),
              "last": None if health_last is None else {k: health_last.get(k) for k in
                                                       TRACE_LOSS_COUNTERS + ("enabled", "acquired",
                                                                              "present_encode_calls",
                                                                              "presented_callbacks",
                                                                              "commits", "completed")}}
    result["health"] = health
    for counter, value in sorted(loss_max.items()):
        if value:
            warnings.append(f"health: {counter} = {value}: the trace lost events; the numbers are a close "
                            "estimate, not an exact count" if counter == "dropped" else f"health: {counter} = {value}")
    if row_cap:
        warnings.append(f"the trace reached its row cap ({rows_attempted_max} of {max_rows} rows): the end of "
                        "the run is not recorded")
    if bad_lines:
        warnings.append(f"{bad_lines} lines could not be parsed and were skipped (a truncated file?)")
    if health_last is None:
        warnings.append("the trace has no health records: capture completeness is unknown")
    result["acquisitions"] = len(acquisitions)
    result["presents"] = len(presents)
    result["present_encodes"] = len(present_encodes)
    if not intervals:
        warnings.append("no completed command buffer has a valid GPU time")
        result.update(window=None, busy_fraction=None, busy_seconds=None, gpu_ms_per_frame=None,
                      frame_ms=None, command_buffers={"valid": 0, "invalid": invalid_completed, "in_window": 0,
                                                     "per_frame": None},
                      acquisitions_in_window=0, presents_in_window=0, never_acquired_drawable=False,
                      warnings=warnings)
        return result
    first, last = min(s for s, _ in intervals), max(e for _, e in intervals)
    lo, hi = first + crop, last - crop
    if hi <= lo:
        warnings.append(f"the trace covers {last - first:.1f} s, too short to crop {crop:g} s at both ends; "
                        "no crop was applied")
        lo, hi, crop = first, last, 0.0
    clipped = [(max(s, lo), min(e, hi)) for s, e in intervals if e > lo and s < hi]
    busy = sum(e - s for s, e in merge_intervals(clipped))
    seconds = hi - lo
    in_window = sum(1 for s, _ in intervals if lo <= s < hi)
    acquired_in_window = sum(1 for t in acquisitions if lo <= t < hi)
    presented_in_window = sum(1 for t in presents if lo <= t < hi)
    if fps:
        frames, frames_source = seconds * fps, "report fps"
    elif presented_in_window:
        frames, frames_source = float(presented_in_window), "presents in the window"
    elif acquired_in_window:
        frames, frames_source = float(acquired_in_window), "acquisitions in the window"
    else:
        frames, frames_source = None, None
    fraction = busy / seconds if seconds > 0 else None
    result.update(
        window={"start_s": lo, "end_s": hi, "seconds": seconds, "crop_seconds": crop,
                "trace_seconds": last - first},
        busy_seconds=busy, busy_fraction=fraction,
        frames_in_window=frames, frames_source=frames_source, fps=fps,
        frame_ms=(1000.0 / fps) if fps else None,
        gpu_ms_per_frame=(busy / frames * 1000.0) if frames else None,
        command_buffers={"valid": len(intervals), "invalid": invalid_completed, "in_window": in_window,
                         "per_frame": (in_window / frames) if frames else None},
        acquisitions_in_window=acquired_in_window, presents_in_window=presented_in_window)
    never = not acquisitions and bool(intervals)
    result["never_acquired_drawable"] = never
    if never:
        result["never_acquired_message"] = NEVER_ACQUIRED_MESSAGE
        warnings.append(NEVER_ACQUIRED_MESSAGE)
    result["warnings"] = warnings
    return result


def format_gpu_busy(r):
    lines = [f"trace {r['path']} ({r['size_bytes'] / 1e6:.0f} MB, {r['lines']} rows"
             + (f", {r['bad_lines']} unreadable" if r["bad_lines"] else "") + ")"]
    w = r.get("window")
    if w is None:
        lines.append("no GPU intervals: nothing to report")
    else:
        lines.append(f"window: {w['seconds']:.1f} s of {w['trace_seconds']:.1f} s recorded "
                     f"(cropped {w['crop_seconds']:g} s at both ends)")
        lines.append(f"gpu busy: {r['busy_fraction'] * 100:.1f}% of wall ({r['busy_seconds']:.2f} s, union of "
                     "command-buffer GPU intervals)")
        if r.get("gpu_ms_per_frame") is not None:
            frame = f" of a {r['frame_ms']:.2f} ms frame" if r.get("frame_ms") else ""
            lines.append(f"gpu ms/frame: {r['gpu_ms_per_frame']:.2f}{frame} (frames from {r['frames_source']}"
                         + (f", {r['fps']:.1f} fps" if r.get("fps") else "") + ")")
        cb = r["command_buffers"]
        per = f", {cb['per_frame']:.1f} per frame" if cb["per_frame"] is not None else ""
        lines.append(f"command buffers: {cb['in_window']} in the window{per} ({cb['valid']} with a valid GPU time "
                     f"in all, {cb['invalid']} without)")
    lines.append(f"drawable acquisitions: {r['acquisitions']} ({r['acquisitions_in_window']} in the window); "
                 f"presents: {r['presents']} presented callbacks, {r['present_encodes']} present encodes")
    h = r["health"]
    lost = {k: v for k, v in h["loss_max"].items() if v}
    lines.append("health: " + (", ".join(f"{k} {v}" for k, v in lost.items()) if lost else "no drops or errors")
                 + f"; rows {h['rows_attempted']}/{h['max_rows']}")
    for warning in r["warnings"]:
        lines.append(f"warning: {warning}")
    return "\n".join(lines) + "\n"


def gpu_from_trace(trace, report_path=None, crop=DEFAULT_TRACE_CROP_SECONDS, fps=None):
    if report_path and fps is None:
        try:
            fps = json.loads(Path(report_path).read_text(encoding="utf-8")).get("average_fps")
        except (OSError, ValueError) as error:
            raise HarnessError("report_unreadable", f"{report_path}: {error}") from error
    return analyze_metal_trace(trace, fps=fps, crop=crop)


# --------------------------------------------------------------------------------------------
# Analysing a run: samply-top

SAMPLY_UNIT_MS = {"ms": 1.0, "s": 1000.0, "µs": 1e-3, "us": 1e-3, "ns": 1e-6}
UNNAMED_THREAD = re.compile(r"^Thread <\d+>$")
ROLE_POOLS = (("Async Compute Task Pool", "async-compute"), ("Compute Task Pool", "compute"),
              ("IO Task Pool", "io"))
DEFAULT_THREAD_ROLES = ("main", "render", "compute")
# Leaf functions where a thread parks or waits. Their CPU is what the thread used since its previous
# sample, which includes work done just before it parked, so it is reported apart from execution.
PARKING_ENDPOINTS = re.compile(
    r"__psynch_cvwait|__psynch_mutexwait|mach_msg2?_trap|semaphore_wait_trap|__ulock_wait2?|kevent(?:_qos|64)?$|"
    r"__semwait_signal|_?pthread_cond_wait|__workq_kernreturn|__select|<parking::Inner>::park|nextDrawable")
# The render thread is the pipelined-rendering thread; it has no name of its own.
RENDER_THREAD_MARKER = "pipelined_rendering"
UNIVERSAL_SHARE = 0.995


def read_json_file(path):
    """JSON from a plain or gzip-compressed file, decided by the gzip magic number."""
    raw = Path(path).read_bytes()
    if raw[:2] == b"\x1f\x8b":
        raw = gzip.decompress(raw)
    return json.loads(raw)


def samply_sidecar_path(profile_path):
    """`cpu-profile.json.gz` is accompanied by `cpu-profile.json.syms.json`."""
    profile_path = Path(profile_path)
    stem = profile_path.name[:-3] if profile_path.name.endswith(".gz") else profile_path.name
    return profile_path.with_name(stem + ".syms.json")


class SymbolTable:
    """Address lookup for one library from the symbols sidecar."""

    def __init__(self, entry, strings):
        self.strings = strings
        self.table = entry.get("symbol_table") or []
        self.known = {address: index for address, index in entry.get("known_addresses") or []}
        self.rvas = [row["rva"] for row in self.table]

    def resolve(self, address):
        """(symbol name, start address) or None."""
        index = self.known.get(address)
        if index is None:
            position = bisect.bisect_right(self.rvas, address) - 1
            if position < 0:
                return None
            row = self.table[position]
            if not row["rva"] <= address < row["rva"] + max(row.get("size", 0), 1):
                return None
            index = position
        row = self.table[index]
        name = self.strings[row["symbol"]] if row["symbol"] < len(self.strings) else None
        if not name or name == "UNKNOWN":
            return None
        return name, row["rva"]


def debug_id_of(breakpad_id):
    """A breakpad id (32 hex digits and an age) as the dashed lower-case id the sidecar uses."""
    h = (breakpad_id or "")[:32].lower()
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}" if len(h) == 32 else h


def load_samply(profile_path):
    profile_path = Path(profile_path)
    try:
        profile = read_json_file(profile_path)
    except (OSError, ValueError) as error:
        raise HarnessError("profile_unreadable", f"{profile_path}: {error}") from error
    sidecar = samply_sidecar_path(profile_path)
    syms = None
    if sidecar.is_file():
        try:
            syms = read_json_file(sidecar)
        except (OSError, ValueError) as error:
            raise HarnessError("symbols_unreadable", f"{sidecar}: {error}") from error
    return profile, syms, sidecar


def symbol_tables(profile, syms):
    """One SymbolTable per profile library index (None when the sidecar has no entry for it)."""
    by_id = {}
    if syms:
        for entry in syms.get("data") or []:
            by_id[(entry.get("debug_name"), entry.get("debug_id"))] = SymbolTable(entry, syms.get("string_table") or [])
    return [by_id.get((lib.get("debugName"), debug_id_of(lib.get("breakpadId")))) for lib in profile.get("libs") or []]


def analyze_samply_thread(thread, libs, tables, *, cut_ms, unit_ms):
    """Self and inclusive CPU per function for one thread, from `samples.threadCPUDelta`.

    The delta is the CPU time the thread used since its previous sample, in the units named by
    `meta.sampleUnits`; it is attributed to the sample's stack as a sampling estimate and is not
    multiplied by the sample weight. A parked thread reports about zero, which keeps waits out.
    """
    samples, stacks, frames = thread["samples"], thread["stackTable"], thread["frameTable"]
    funcs, resources, names = thread["funcTable"], thread["resourceTable"], thread["stringArray"]
    cpu_by_stack = collections.defaultdict(float)
    samples_by_stack = collections.Counter()
    kept = 0
    first = last = None
    for stack, cpu, time_ms in zip(samples["stack"], samples["threadCPUDelta"], samples["time"]):
        if stack is None or time_ms < cut_ms:
            continue
        cpu_by_stack[stack] += (cpu or 0) * unit_ms
        samples_by_stack[stack] += 1
        kept += 1
        first = time_ms if first is None else first
        last = time_ms
    frame_cache = {}

    def frame_key(frame):
        cached = frame_cache.get(frame)
        if cached is None:
            func = frames["func"][frame]
            resource = funcs["resource"][func]
            lib_index = resources["lib"][resource] if resource is not None and resource >= 0 else None
            lib = libs[lib_index] if lib_index is not None and lib_index >= 0 else {}
            lib_name = lib.get("debugName") or lib.get("name") or "?"
            address = frames["address"][frame]
            table = tables[lib_index] if lib_index is not None and 0 <= lib_index < len(tables) else None
            hit = table.resolve(address) if table is not None and address is not None and address >= 0 else None
            if hit is not None:
                cached = ((lib_name, hit[0], hit[1]), True, address)
            else:
                text = names[funcs["name"][func]]
                already_named = not re.fullmatch(r"0x[0-9a-fA-F]+", text)
                shown = text if already_named else f"{lib_name}+0x{address:x}" if address is not None and address >= 0 else text
                cached = ((lib_name, shown, None), already_named, (lib_name, address))
            frame_cache[frame] = cached
        return cached

    self_ms, inclusive_ms = collections.defaultdict(float), collections.defaultdict(float)
    leaf_unresolved_ms = leaf_unresolved_samples = 0
    leaf_addresses, stack_addresses = set(), set()
    for stack, cpu in cpu_by_stack.items():
        seen = set()
        node, leaf = stack, True
        while node is not None:
            key, resolved, address = frame_key(stacks["frame"][node])
            if leaf:
                self_ms[key] += cpu
                if not resolved:
                    leaf_unresolved_ms += cpu
                    leaf_unresolved_samples += samples_by_stack[stack]
                    leaf_addresses.add(address)
                leaf = False
            if key not in seen:
                seen.add(key)
                inclusive_ms[key] += cpu
            if not resolved:
                stack_addresses.add(address)
            node = stacks["prefix"][node]
    return {"name": thread.get("name"), "tid": thread.get("tid"), "is_main": bool(thread.get("isMainThread")),
            "samples": kept, "cpu_ms": sum(cpu_by_stack.values()), "first_ms": first, "last_ms": last,
            "self_ms": dict(self_ms), "inclusive_ms": dict(inclusive_ms),
            "unresolved": {"leaf_samples": leaf_unresolved_samples, "leaf_cpu_ms": leaf_unresolved_ms,
                           "leaf_addresses": leaf_addresses, "stack_addresses": stack_addresses}}


def assign_thread_groups(analyses):
    """Group threads: main, render (the busiest unnamed thread), pools aggregated, others by name."""
    unnamed = [a for a in analyses if not a["is_main"] and UNNAMED_THREAD.match(a["name"] or "")]
    marked = [a for a in unnamed if any(RENDER_THREAD_MARKER in key[1] for key in a["inclusive_ms"])]
    render = max(marked or unnamed, key=lambda a: a["cpu_ms"], default=None)
    groups = collections.OrderedDict()
    for a in analyses:
        name = a["name"] or ""
        if a["is_main"]:
            group = "main"
        elif a is render and a["cpu_ms"] > 0:
            group = "render"
        else:
            group = next((role for prefix, role in ROLE_POOLS if name.startswith(prefix)), None)
            group = group or name or f"thread {a['tid']}"
        groups.setdefault(group, []).append(a)
    return groups


def merge_group(threads):
    merged = {"threads": [t["name"] for t in threads], "samples": sum(t["samples"] for t in threads),
              "cpu_ms": sum(t["cpu_ms"] for t in threads), "self_ms": collections.defaultdict(float),
              "inclusive_ms": collections.defaultdict(float)}
    for t in threads:
        for key, value in t["self_ms"].items():
            merged["self_ms"][key] += value
        for key, value in t["inclusive_ms"].items():
            merged["inclusive_ms"][key] += value
    return merged


def analyze_samply(profile_path, *, skip_seconds=DEFAULT_SAMPLY_SKIP_SECONDS):
    profile, syms, sidecar = load_samply(profile_path)
    meta = profile.get("meta") or {}
    unit = (meta.get("sampleUnits") or {}).get("threadCPUDelta", "µs")
    if unit not in SAMPLY_UNIT_MS:
        raise HarnessError("profile_units", f"threadCPUDelta is in {unit!r}; known units: {', '.join(SAMPLY_UNIT_MS)}")
    threads = profile.get("threads") or []
    starts = [t.get("processStartupTime") for t in threads if t.get("processStartupTime") is not None]
    origin = min(starts) if starts else 0.0
    cut_ms = origin + skip_seconds * 1000.0
    tables = symbol_tables(profile, syms)
    analyses = [analyze_samply_thread(t, profile.get("libs") or [], tables, cut_ms=cut_ms, unit_ms=SAMPLY_UNIT_MS[unit])
                for t in threads if t.get("samples") and t["samples"].get("length")]
    analyses = [a for a in analyses if a["samples"]]
    groups = assign_thread_groups(analyses)
    ends = [a["last_ms"] for a in analyses if a["last_ms"] is not None]
    window_s = (max(ends) - cut_ms) / 1000.0 if ends else 0.0
    total_samples = sum(a["samples"] for a in analyses)
    total_cpu = sum(a["cpu_ms"] for a in analyses)
    unresolved = {
        "samples": sum(a["unresolved"]["leaf_samples"] for a in analyses),
        "total_samples": total_samples,
        "cpu_ms": sum(a["unresolved"]["leaf_cpu_ms"] for a in analyses),
        "total_cpu_ms": total_cpu,
        "leaf_addresses": len(set().union(*[a["unresolved"]["leaf_addresses"] for a in analyses])) if analyses else 0,
        "stack_addresses": len(set().union(*[a["unresolved"]["stack_addresses"] for a in analyses])) if analyses else 0,
    }
    return {"profile": str(profile_path), "symbols": str(sidecar) if syms is not None else None,
            "skip_seconds": skip_seconds, "window_seconds": window_s, "unit": unit,
            "threads_with_samples": len(analyses), "groups": groups, "unresolved": unresolved,
            "merged": {name: merge_group(ts) for name, ts in groups.items()}}


def select_groups(analysis, thread):
    names = list(analysis["merged"])
    if thread is None:
        return [n for n in DEFAULT_THREAD_ROLES if n in analysis["merged"]]
    if thread == "all":
        return [n for n in names if analysis["merged"][n]["cpu_ms"] >= 50.0]
    wanted = thread.lower()
    chosen = [n for n in names if n.lower() == wanted]
    if not chosen:
        chosen = [n for n in names if wanted in n.lower()
                  or any(wanted in (t or "").lower() for t in analysis["merged"][n]["threads"])]
    if not chosen:
        raise HarnessError("no_such_thread", f"{thread}; available: {', '.join(names)}")
    return chosen


def function_label(key, width=110):
    lib, name, _ = key
    text = f"{name}  [{lib}]"
    return text if len(text) <= width else text[:width - 1] + "…"


def top_functions(values, n):
    return sorted(values.items(), key=lambda kv: -kv[1])[:n]


def format_samply_top(analysis, thread=None, top=25):
    lines = [f"profile {analysis['profile']}",
             f"window: after the first {analysis['skip_seconds']:g} s, {analysis['window_seconds']:.1f} s; "
             f"CPU from threadCPUDelta ({analysis['unit']} converted to ms), {analysis['threads_with_samples']} threads"]
    u = analysis["unresolved"]
    if analysis["symbols"] is None:
        lines.append("symbols: no .syms.json sidecar found, names are raw addresses")
    pct = (lambda a, b: f"{a / b * 100:.1f}%" if b else "n/a")
    lines.append(f"unsymbolicated: {u['samples']} of {u['total_samples']} samples ({pct(u['samples'], u['total_samples'])}), "
                 f"{u['cpu_ms']:.0f} of {u['total_cpu_ms']:.0f} CPU ms ({pct(u['cpu_ms'], u['total_cpu_ms'])}) at the leaf; "
                 f"{u['leaf_addresses']} distinct leaf addresses, {u['stack_addresses']} in all stacks")
    for name in select_groups(analysis, thread):
        g = analysis["merged"][name]
        count = len(g["threads"])
        window = analysis["window_seconds"] or 1.0
        lines += ["", f"== {name}: {count} thread{'s' if count != 1 else ''} ({g['threads'][0]}{', ...' if count > 1 else ''}), "
                      f"{g['cpu_ms']:.0f} CPU ms = {g['cpu_ms'] / window / 1000.0 * 100:.0f}% of one core over the window, "
                      f"{g['samples']} samples"]
        parking = sum(ms for fn, ms in g["self_ms"].items() if PARKING_ENDPOINTS.search(fn[1]))
        lines.append(f"  parking endpoints (marked *): {parking:.0f} CPU ms = "
                     f"{parking / g['cpu_ms'] * 100 if g['cpu_ms'] else 0:.0f}% of the thread's CPU; "
                     "it includes work done just before parking, so read it apart from function execution")
        for title, key in (("self CPU ms", "self_ms"), ("inclusive CPU ms (rows overlap)", "inclusive_ms")):
            lines.append(f"  {title:>8}   share  function")
            rows = top_functions(g[key], top + 40 if key == "inclusive_ms" else top)
            omitted = 0
            shown = 0
            for fn, ms in rows:
                if key == "inclusive_ms" and g["cpu_ms"] and ms >= UNIVERSAL_SHARE * g["cpu_ms"]:
                    omitted += 1
                    continue
                if shown >= top:
                    break
                shown += 1
                mark = "*" if PARKING_ENDPOINTS.search(fn[1]) else " "
                lines.append(f"  {ms:10.1f}  {ms / g['cpu_ms'] * 100 if g['cpu_ms'] else 0:5.1f}%{mark} {function_label(fn)}")
            if omitted:
                lines.append(f"  ({omitted} root frames on every sample omitted)")
    return "\n".join(lines) + "\n"


def compare_samply(this, other, thread=None, top=25):
    """Largest per-function changes in CPU ms per second of window, `other` being the baseline."""
    out = {}
    for name in select_groups(this, thread):
        if name not in other["merged"]:
            continue
        entry = {}
        for key in ("self_ms", "inclusive_ms"):
            a = {k: v / (other["window_seconds"] or 1.0) for k, v in other["merged"][name][key].items()}
            b = {k: v / (this["window_seconds"] or 1.0) for k, v in this["merged"][name][key].items()}
            rows = [(k, a.get(k, 0.0), b.get(k, 0.0), b.get(k, 0.0) - a.get(k, 0.0)) for k in set(a) | set(b)]
            entry[key] = sorted(rows, key=lambda r: -abs(r[3]))[:top]
        out[name] = entry
    return out


def format_samply_comparison(changes, this, other):
    lines = ["", f"changes in CPU ms per second of window: {this['profile']} (this) against {other['profile']}"]
    for name, entry in changes.items():
        lines += ["", f"== {name}"]
        for title, key in (("self", "self_ms"), ("inclusive", "inclusive_ms")):
            lines.append(f"  {title}: against -> this (ms/s)   change  function")
            for fn, a, b, d in entry[key]:
                lines.append(f"  {a:9.2f} -> {b:9.2f}  {d:+9.2f}  {function_label(fn, 100)}")
    return "\n".join(lines) + "\n"


def samply_json(analysis, thread, top):
    groups = {}
    for name in select_groups(analysis, thread):
        g = analysis["merged"][name]
        groups[name] = {"threads": g["threads"], "samples": g["samples"], "cpu_ms": g["cpu_ms"],
                        "self": [{"function": f"{k[1]}", "library": k[0], "ms": v} for k, v in top_functions(g["self_ms"], top)],
                        "inclusive": [{"function": f"{k[1]}", "library": k[0], "ms": v}
                                      for k, v in top_functions(g["inclusive_ms"], top)]}
    return {"profile": analysis["profile"], "symbols": analysis["symbols"], "skip_seconds": analysis["skip_seconds"],
            "window_seconds": analysis["window_seconds"], "unresolved": analysis["unresolved"], "groups": groups}


# --------------------------------------------------------------------------------------------
# Command line


def cmd_run(args):
    scenarios = load_scenarios()
    if args.scenario not in scenarios:
        raise HarnessError("unknown_scenario", f"{args.scenario}; known: {', '.join(scenarios)}")
    window = resolve_window(args.window, [args.engine])
    run = run_scenario(args.engine, args.scenario, scenarios[args.scenario], assets=args.assets, out_dir=args.out,
                       duration=args.duration, warmup_frames=args.warmup_frames, timeout=args.timeout,
                       grace=args.grace, window=window, dry_run=args.dry_run, profiler=args.profiler,
                       samply=args.samply, trace_start=args.metal_trace_start_acquisition,
                       finalize_timeout=args.samply_finalize_timeout)
    if args.dry_run:
        env = " ".join(f"{k}={v}" for k, v in sorted(run["env"].items()))
        print((env + " " if env else "") + " ".join(run["command"]))
        return 0
    print(json.dumps({k: run[k] for k in ("classification", "valid", "exit_code", "wall_seconds", "window_mode",
                                          "warnings")}))
    return 0 if run["valid"] else 4


def cmd_shots(args):
    window = resolve_window(args.window, [args.engine], for_shots=True)
    run = capture_shots(args.engine, args.shots, args.assets, args.out, worldspace=args.worldspace,
                        timeout=args.timeout, grace=args.grace, window=window)
    print(json.dumps({k: run[k] for k in ("classification", "valid", "exit_code", "wall_seconds", "window_mode")}))
    return 0 if run["valid"] else 4


def cmd_visual(args):
    result = visual_compare(args.base, args.cand, args.out, noise_floor=args.noise_floor,
                            margin_mean=args.margin_mean, margin_fraction=args.margin_fraction,
                            force_stdlib=args.stdlib)
    print(f"metric {result['metric']}: {result['overall']}")
    for name, entry in result["shots"].items():
        values = entry.get("values")
        detail = "" if not values else f" mean {values['mean']:.4f} fraction_over {values['fraction']:.4f}"
        print(f"  {name}: {entry['status']}{detail} {'; '.join(entry['reasons'])}")
    for warning in result.get("warnings", []):
        print(f"warning: {warning}")
    return 0 if result["overall"] == "pass" else 3


def cmd_build(args):
    repo = Path(args.repo) if args.repo else git_toplevel(Path.cwd())
    plan = build(args.ref, repo=repo, worktree_root=args.worktree_root, bin_dir=args.bin_dir,
                 keep_worktree=args.keep_worktree, dry_run=args.dry_run)
    action = "reused" if plan["reused"] else ("would build" if args.dry_run else "built")
    print(f"{action} {plan['binary']} ({plan['sha']})")
    return 0


def cmd_summarize(args):
    gpu = None
    if args.gpu_trace:
        fps = load_run_for_analysis(args.run_dir)[1].get("average_fps")
        gpu = gpu_from_trace(args.gpu_trace, crop=args.crop, fps=fps)
    summary = summarize_run(args.run_dir, gpu)
    text = format_summary(summary, top=args.top)
    result = {"summary": summary}
    if args.against:
        other = summarize_run(args.against)
        result["comparison"] = compare_summaries(other, summary)
        text += format_comparison(result["comparison"], top=args.top)
    print(text, end="")
    if args.json:
        Path(args.json).write_text(json.dumps(result, indent=2, default=str), encoding="utf-8")
    return 0


def cmd_gpu_busy(args):
    result = gpu_from_trace(args.trace, args.report, args.crop)
    print(format_gpu_busy(result), end="")
    if args.json:
        Path(args.json).write_text(json.dumps(result, indent=2), encoding="utf-8")
    return 0


def cmd_samply_top(args):
    analysis = analyze_samply(args.profile, skip_seconds=args.skip)
    text = format_samply_top(analysis, args.thread, args.top)
    result = {"this": samply_json(analysis, args.thread, args.top)}
    if args.against:
        other = analyze_samply(args.against, skip_seconds=args.skip)
        changes = compare_samply(analysis, other, args.thread, args.top)
        text += format_samply_comparison(changes, analysis, other)
        result["against"] = samply_json(other, args.thread, args.top)
        result["changes"] = {name: {key: [{"function": k[1], "library": k[0], "against_ms_per_s": a,
                                           "this_ms_per_s": b, "change": d} for k, a, b, d in rows]
                                    for key, rows in entry.items()} for name, entry in changes.items()}
    print(text, end="")
    if args.json:
        Path(args.json).write_text(json.dumps(result, indent=2), encoding="utf-8")
    return 0


def cmd_scenarios(_args):
    for name, scenario in load_scenarios().items():
        needs = "assets" if scenario.get("needs_assets") else "no assets"
        print(f"{name:14} {needs:10} {scenario.get('description', '')}")
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    def add_window_option(p):
        p.add_argument("--window", choices=WINDOW_MODES, default="auto",
                       help="auto (background when the engine lists --background-window, else visible), "
                            "background (visible, under other windows, no focus), visible, or hidden "
                            "(on macOS it never presents: not representative for timing, black for shots)")
        p.add_argument("--visible", action="store_const", const="visible", dest="window",
                       help="alias for --window visible")

    def timing_options(p):
        p.add_argument("--duration", type=float, help="seconds per window (scenario default, else 60)")
        p.add_argument("--warmup-frames", type=int, help="frames before measuring (scenario default)")
        p.add_argument("--timeout", type=float, help="wall-clock limit per run (default duration + 120)")
        p.add_argument("--grace", type=float, default=20.0,
                       help="seconds an engine gets to exit after writing its report before it is killed")
        add_window_option(p)

    p = sub.add_parser("run", help="run one scenario under the watchdog")
    p.add_argument("--engine", required=True)
    p.add_argument("--scenario", required=True)
    p.add_argument("--assets")
    p.add_argument("--out", required=True)
    p.add_argument("--dry-run", action="store_true", help="print the command and exit")
    p.add_argument("--profiler", choices=PROFILERS, default="none",
                   help="samply (CPU stacks), metal-trace (native GPU timeline) or none; a profiled run "
                        "is not comparable with an unprofiled one")
    p.add_argument("--samply", help="path of the samply binary (default: PATH, then ~/.cargo/bin)")
    p.add_argument("--samply-finalize-timeout", type=float, default=SAMPLY_FINALIZE_SECONDS,
                   help="seconds samply gets to write its profile after the engine has stopped")
    p.add_argument("--metal-trace-start-acquisition", type=int, default=METAL_TRACE_START_ACQUISITION,
                   help="native drawable acquisition number at which the Metal trace starts recording")
    timing_options(p)
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("compare", help="interleaved base/candidate comparison")
    p.add_argument("--base", required=True)
    p.add_argument("--cand", required=True)
    p.add_argument("--scenarios", required=True, help="comma-separated scenario names")
    p.add_argument("--pairs", type=int, default=MIN_PAIRS_FOR_VERDICT)
    p.add_argument("--assets")
    p.add_argument("--out", required=True)
    p.add_argument("--min-effect", type=float, default=DEFAULT_MIN_EFFECT)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument("--inventory-tolerance", type=float, default=DEFAULT_INVENTORY_TOLERANCE)
    p.add_argument("--cooldown", type=float, default=3.0, help="seconds between runs")
    for side in ("base", "cand"):
        p.add_argument(f"--{side}-commit", help=f"commit the {side} binary was built from "
                       "(default: taken from an engine-<sha> file name)")
        p.add_argument(f"--{side}-dirty", action="store_true",
                       help=f"the {side} binary was built from a working tree with uncommitted changes")
    p.add_argument("--force", action="store_true",
                   help="measure despite failed preflight checks or a non-empty --out")
    timing_options(p)
    p.set_defaults(func=compare)

    p = sub.add_parser("preflight", help="check the machine is fit for timing runs")
    p.add_argument("--out", help="directory whose disk is checked")
    p.set_defaults(func=preflight)

    p = sub.add_parser("shots", help="capture reference shots (never with a hidden window on macOS)")
    p.add_argument("--engine", required=True)
    p.add_argument("--shots", required=True)
    p.add_argument("--assets", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--worldspace", default="0x3c")
    p.add_argument("--timeout", type=float, default=300.0)
    p.add_argument("--grace", type=float, default=15.0)
    add_window_option(p)
    p.set_defaults(func=cmd_shots)

    p = sub.add_parser("visual", help="compare two directories of PNGs")
    p.add_argument("--base", required=True)
    p.add_argument("--cand", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--noise-floor", help="visual.json of two captures of the same binary")
    p.add_argument("--margin-mean", type=float)
    p.add_argument("--margin-fraction", type=float)
    p.add_argument("--stdlib", action="store_true", help="skip FLIP even when it is installed")
    p.set_defaults(func=cmd_visual)

    p = sub.add_parser("build", help="build the engine at a git ref into a binary directory")
    p.add_argument("ref")
    p.add_argument("--worktree-root", required=True)
    p.add_argument("--bin-dir", required=True)
    p.add_argument("--repo")
    p.add_argument("--keep-worktree", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(func=cmd_build)

    p = sub.add_parser("summarize", help="frame stats, thread budget, bound classification and CPU spans of a run")
    p.add_argument("run_dir")
    p.add_argument("--against", help="another run directory: print per-metric changes (this run against it)")
    p.add_argument("--gpu-trace", help="native.ndjson of this run: adds GPU busy time to the classification")
    p.add_argument("--crop", type=float, default=DEFAULT_TRACE_CROP_SECONDS, help="seconds cropped from each end of the trace")
    p.add_argument("--top", type=int, default=TOP_CPU_SPANS)
    p.add_argument("--json", metavar="OUT", help="also write the analysis as JSON")
    p.set_defaults(func=cmd_summarize)

    p = sub.add_parser("gpu-busy", help="GPU busy fraction and command buffers from a native Metal trace")
    p.add_argument("trace", help="native.ndjson written by --profiler metal-trace")
    p.add_argument("--report", help="report.json of the same run: its average_fps gives GPU ms per frame")
    p.add_argument("--crop", type=float, default=DEFAULT_TRACE_CROP_SECONDS, help="seconds cropped from each end")
    p.add_argument("--json", metavar="OUT", help="also write the result as JSON")
    p.set_defaults(func=cmd_gpu_busy)

    p = sub.add_parser("samply-top", help="top functions by self and inclusive CPU time of a Samply profile")
    p.add_argument("profile", help="cpu-profile.json.gz; cpu-profile.json.syms.json must sit beside it")
    p.add_argument("--thread", help="main, render, compute, io, async-compute, all, or part of a thread name "
                                    "(default: main, render and compute)")
    p.add_argument("--top", type=int, default=15)
    p.add_argument("--skip", type=float, default=DEFAULT_SAMPLY_SKIP_SECONDS,
                   help="seconds from process start to drop, so loading is not ranked")
    p.add_argument("--against", metavar="OTHER", help="another profile: the biggest per-function changes")
    p.add_argument("--json", metavar="OUT", help="also write the result as JSON")
    p.set_defaults(func=cmd_samply_top)

    p = sub.add_parser("scenarios", help="list scenarios")
    p.set_defaults(func=cmd_scenarios)
    return parser


def _exit_on_signal(signum, frame):
    raise SystemExit(128 + signum)


def install_termination_handlers(register=signal.signal):
    """Turn SIGTERM and SIGHUP into SystemExit and return the handlers they replaced.

    The engine runs in its own session, so a plain `kill` or a closed terminal reaches only this
    process; the default action would end it without unwinding and leave the engine running.
    SystemExit unwinds through the watchdog's cleanup, which stops and reaps the child.
    """
    previous = {}
    for name in ("SIGTERM", "SIGHUP"):
        number = getattr(signal, name, None)  # SIGHUP does not exist on Windows
        if number is not None:
            previous[number] = register(number, _exit_on_signal)
    return previous


def main(argv=None):
    args = build_parser().parse_args(argv)
    previous = install_termination_handlers()
    try:
        return args.func(args)
    except HarnessError as error:
        print(f"error: {error}", file=sys.stderr)
        return 3 if error.name in ("preflight_failed", "insufficient_disk") else 2
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)


if __name__ == "__main__":
    sys.exit(main())
