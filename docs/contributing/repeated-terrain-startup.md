# Repeated terrain startup captures

Use `scripts/repeat-terrain-startup.py` to check intermittent terrain disappearance across fresh engine processes. The default campaign captures ten Riverwood launches at each of the 16 MiB and 1 MiB upload budgets. It preserves operating-system caches and records functional evidence; it does not compare performance.

```sh
python3 scripts/repeat-terrain-startup.py \
  --engine /path/to/pinned/engine \
  --assets /path/to/modern_assets \
  --output /path/to/new-campaign-directory \
  --commit SOURCE_REVISION
```

Each launch uses worldspace `0x3c`, cell `(5, -12)`, radius `2`, camera offset `(0, 6000, 8000)`, 1,200 warmup frames and 30 measured seconds. `--repeats`, `--upload-budgets`, `--warmup`, `--duration` and `--timeout` override the campaign defaults. The timeout defaults to 180 seconds per launch.

The engine must have its graphics and loader environment configured before launching. A prefix can provide a display or compositor:

```sh
python3 scripts/repeat-terrain-startup.py \
  --engine /path/to/pinned/engine \
  --assets /path/to/modern_assets \
  --output /path/to/new-campaign-directory \
  --launch-prefix "gamescope -W 1600 -H 900 -- /path/to/runtime-env.sh"
```

`--launch-prefix` is parsed into arguments with `shlex`; the runner never invokes a shell. A privileged compositor wrapper can strip `LD_LIBRARY_PATH`. In that case, put a runtime wrapper after the compositor's `--` separator. The wrapper must set the engine's required loader environment and finish with `exec "$@"`. Its path and arguments are preserved in every run manifest.

The runner supports Linux and macOS. It creates a new process group for each launch and cleans up that group on completion, timeout or interruption. It refuses an existing output directory.

Each `16mib-01`, `1mib-01` and subsequent run directory contains:

- `run.json`: arguments, camera settings, UTC start/end, exit status, safe environment fields and input hashes.
- `engine.stdout.log` and `engine.stderr.log`: complete raw launch logs.
- `report.json`, `frames.csv` and `profile/`: the engine's acceptance and profiling outputs.
- `frame.png`: the engine's post-warmup capture, taken after its readiness checks. It is captured once rather than at the end of the run.
- `validation.json`: functional gates, retry accounting and failures. An engine exit code of `3` means the GPU device was lost: the run fails with `device_lost: true` and a request to retry it, rather than the generic engine failure.

The manifests hash the engine, world database, cell cache, conversion manifest, integration report and LOD manifest before and after each launch. They also compare those inputs with the campaign baseline. Individual mesh and texture payload files are outside this hash scope. Pin the binary and asset directory for the campaign.

Validation requires 25 resident cells, 2,029 ready model instances, 100 validated terrain patches, matching request/completion counts, drained queues and zero asset, material, renderer and streaming failures. Reports must cover the requested measured duration; profile run ID, scenario and commit must match the invocation. The frame CSV count must match the report. The exact gamescope message `[gamescope] [Error] xwm: NO CURSOR IMPL XDG` is recorded as a known cursor warning; other error messages still fail the run.

The validator checks the initial logged camera target and offset, PNG chunk CRCs, decompressed scanline lengths and filter bytes. It compares dimensions with a recorded physical surface size when available, otherwise with the metadata's requested-resolution fallback. The fallback does not independently prove physical surface size. The capture's final camera pose is not recorded; keep the graphics session free of camera input. Cell/model/LOD timeline events must complete by warmup. Surface and upload gauges record final state without historical timestamps, so the report states that limit.

Truecolor and grayscale captures must contain a visible nonblack pixel; opaque alpha alone does not make a black image pass. Palette PNGs retain structural validation without this content check. Passing the check does not establish terrain or building coverage.

Acceptance captures now record the primary window's physical pixel dimensions when the screenshot is queued, in `metadata.json` under `window.physical_resolution`. This keeps Retina captures distinct from the requested logical resolution. Runs without a capture retain the existing fallback.

When retry logs exist, every accounting record must satisfy `tracked = resumed + canceled + pending`, and the last pending count must be zero. Missing retry logs are flagged for inspection; a startup that never reaches the retry path can still be valid.

`summary.json` is updated after every launch and finalized when the campaign stops. Interruptions during hashing, engine waits, cleanup or analysis retain the current `run.json` and `validation.json`; an interruption before the first run retains a summary with no launches. Repeated stop signals are deferred while the runner cleans up its owned group and saves evidence. Exit status `0` means every planned run passed the functional gates, `1` means a run failed, and `130` means the campaign was interrupted. **Visual inspection remains pending even when all counters pass.** Inspect every saved image for terrain holes and retain those findings with the campaign; counters and a valid PNG cannot establish pixel coverage.

Run the focused runner tests without opening a graphics session:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/tests -p test_repeat_terrain_startup.py -v
```
