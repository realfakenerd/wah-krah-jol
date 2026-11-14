# Experimental native Metal profiling

These macOS tools instrument an owned, locally built development process. They
record drawable acquisition, native command-buffer and encoder boundaries,
GPU buffer timestamps, texture reuse and presentation. They complement Samply,
the Metal Performance HUD and full Xcode; they do not measure GPU shader
functions or active cycles. See the [profiling guide](../../docs/contributing/native-profiling.md)
and [measured results](../../docs/research/metal-native-timeline-20261009.md).

## Build and record

From the repository root:

```sh
profile_run="../mudcrab-profiles/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$profile_run"

clang -fobjc-arc -O2 -Wall -Wextra -Werror -dynamiclib \
  scripts/profiling/metal_trace.m -o "$profile_run/libmudcrab_metal_trace.dylib" \
  -framework Foundation -framework Metal -framework QuartzCore -framework AppKit

DYLD_INSERT_LIBRARIES="$PWD/$profile_run/libmudcrab_metal_trace.dylib" \
MUDCRAB_METAL_TRACE_FILE="$PWD/$profile_run/native.ndjson" \
MUDCRAB_METAL_TRACE_START_ACQUISITION=1200 \
  ./target/release/engine --assets "$PWD/modern_assets" \
  --worldspace 0x3c --grid-x 5 --grid-y -12 --stream-radius 2 \
  --benchmark-warmup-frames 1200 --benchmark-duration 20 \
  --benchmark-output "$profile_run/report.json" \
  --benchmark-frame-times "$profile_run/frames.csv" \
  --profile-output "$profile_run/profile" \
  --accept-min-fps 0 --accept-p95-ms 1000 \
  --accept-max-memory-growth-gib 4 \
  --acceptance-screenshot "$profile_run/frame.png" \
  --screenshot-camera-offset 0,6000,8000 \
  >"$profile_run/engine.stdout.log" 2>"$profile_run/engine.stderr.log"

python3 scripts/profiling/analyze_native.py "$profile_run/native.ndjson" \
  --output "$profile_run/native-summary.json"
python3 scripts/profiling/correlate_drawables.py \
  "$profile_run/native-summary-timeline.json" \
  --output "$profile_run/drawable-correlation.json"
```

These profiling thresholds allow captures below the normal 60 FPS target.
Streaming and renderer checks remain enforced. A passing capture verifies
those checks; it does not establish that the game meets the performance target.

Use an absolute, new trace-file path: the observer opens it exclusively and
refuses to overwrite it. Full observation can write hundreds of megabytes.
The ring holds 32,768 events and `MUDCRAB_METAL_TRACE_MAX_ROWS` defaults to one
million attempted rows. Inspect health records throughout the measured range:
enabled state, drops, hook failures, exceptions, clock failures and callback
coverage matter. A successful analyzer command does not certify a valid capture.
The final shutdown health record normally has `enabled=false`; inspect periodic
records inside the measured range to distinguish shutdown from an earlier cap.

Trace limits accept decimal integers through `18446744073709551615`. Set
`MUDCRAB_METAL_TRACE_MAX_ROWS` to a positive value; its default is `1000000`.
`MUDCRAB_METAL_TRACE_START_ACQUISITION` defaults to `0`, which starts observation
immediately, and accepts an explicit `0`. Empty values, signs, whitespace,
fractions, scientific notation and overflow are rejected. Invalid limits write
`trace_config_error` records and disabled health with `trace_config_errors`,
then stop the observer before installing hooks. The application continues.

Keep the game visible, preserve a scene image and actual surface dimensions,
and verify the final streaming/renderer checks. Check for other builds, tests
and GPU workloads before starting a comparison. Keep exact binaries, source
diffs, environment, raw logs and failed captures. Do not compare the frame rate
of an occluded window with a rendered scene.

The analyzers crop one second from each native acquisition range boundary.
Their native acquisition IDs are not engine frame IDs. Correlation expects one
stable drawable pool and the renderer's `upscaling` and
`(wgpu internal) Present` labels. Surface recreation, multiple layers or changed
render graph labels require additional identity checks. Empty captures, orphan
acquisition ends and incomplete pairs before the last complete acquisition fail
with an input error. The analyzer allows a trailing acquisition begin only when
its ID follows every complete pair and its timestamp falls after the cropped
measurement window. It reports the excluded count as
`trailing_incomplete_acquisition_count`; acquisition-only traces cannot produce
render/presentation correlation pairs.

## Controls and optional capture

Use `MUDCRAB_METAL_TRACE_ACQUIRE_ONLY=1` for a minimal control. It installs only
acquisition hooks, with no scheduled, completed or presented handlers. Its
startup still creates an uncommitted probe queue/buffer. Compare this mode, the
full observer and an uninstrumented run using the same scene and presentation
settings. Callback registration and the ring mutex can perturb scheduling.

Optional variables are independent experiments:

| Variable | Effect |
| --- | --- |
| `MUDCRAB_METAL_PROFILE_ACTIVATE_WINDOW=1` | Bring this process's Mudcrab window forward after startup. Changes focus. |
| `MUDCRAB_METAL_DIAGNOSTIC_DISPLAY_SYNC=1` | Force the native layer's display synchronization on. Changes presentation policy. |
| `MTL_CAPTURE_ENABLED=1` | Enable public GPU trace-document capture support. |
| `MUDCRAB_METAL_GPU_TRACE_FILE=/absolute/new.gputrace` | Request a GPU capture, scoped to the layer's actual Metal device. |
| `MUDCRAB_METAL_GPU_TRACE_START_ACQUISITION=1200` | Begin at a positive native acquisition ID. |
| `MUDCRAB_METAL_GPU_TRACE_ACQUISITIONS=3` | Stop asynchronously after the target Present buffer completes; count must be positive. |
| `MUDCRAB_METAL_HUD_MENU_PROBE=1` | Inspect existing HUD menu actions and request the selected seconds-duration report. |
| `MUDCRAB_METAL_HUD_REPORT_DURATION=5` | Select the existing **5 Seconds** report action. |

Capture boundaries bracket native acquisitions, not guaranteed whole engine
frames. Inspect the resulting file's metadata in full Xcode. Trace writing and
HUD report generation can affect timings; exclude them from ordinary FPS
comparisons. The HUD menu probe depends on English menu titles and current menu
structure. It uses public AppKit methods but is not a stable Apple automation
contract. `observation_only` in the trace describes the display-sync override,
not every optional capture or focus change.

GPU capture requires full observation, with tracing enabled no later than the
capture's start acquisition. Acquisition-only mode has no completion hooks and
falls back to the capture timeout, so keep it separate from GPU capture.

For native HUD encoder counters, use a separate build without application
timestamp-query features. `native-hud-counters.patch` contains the diagnostic
change tested against revision `f9653bfd`. It does not apply to `64108a1c` or `5840f49f`;
refresh it for those revisions before building. Check its application first:

```sh
git apply --check scripts/profiling/native-hud-counters.patch
git apply scripts/profiling/native-hud-counters.patch
CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --locked --release -p engine --bin engine
```

Run that diagnostic executable with `MUDCRAB_NATIVE_GPU_DIAGNOSTICS=1`,
`MTL_HUD_ENABLED=1`, `MTL_HUD_LOG_ENABLED=1`, and
`MTL_HUD_ENCODER_TIMING_ENABLED=1`. Set `MTL_HUD_REPORT_URL` to an absolute
filesystem path, then invoke the HUD's report menu action. The environment
variable selects the destination and does not start collection. Verify that
only the HUD owns native counter sample buffers. Preserve Bevy's macOS timestamp
workaround; do not reenable the suppressed timestamp writes.

## Interpretation and lifetime

GPU command-buffer elapsed spans overlap and can contain dependency waits;
neither their sum nor their union measures active utilization. Encoder wall
time includes other work while an encoder remains open. Thread CPU deltas need
valid clocks and matching thread IDs. `presentedTime` is onscreen time;
presented-handler arrival and compositor texture release are different events.
The observer does not expose the exact compositor release event.

The Objective-C class hooks remain installed for the process lifetime. **Never
unload the dylib with `dlclose`.** Use startup injection into a process that
exits normally. Concurrent swizzling and unsupported Metal implementations can
invalidate coverage. Teardown callbacks can outlive the writer's final drain;
retain the last healthy measured interval and crop uncertain shutdown tails.
Do not describe a zero-drop trace as proof that every native path was observed.

## Split provenance and repeat protocol

These native tools were untracked in the development checkout and are absent from
PR #200's pinned head. `source-manifest.json` preserves their original local hashes
and the split's source identity. The analyzer now rejects nonzero loss/error counters
throughout the health history, including an unhealthy record followed by clean
shutdown. Health and hook coverage still need review; zero counters alone do not
prove complete observation.

Use the [matched batching protocol](../../docs/research/terrain-batching-matched-protocol-20261009.md)
for both upload budgets, repeated camera/queue/image comparisons and established
memory limits. Its fixtures use camera coordinates only and contain no game assets.

`MUDCRAB_PROFILE_SCENE_EVIDENCE=1` plus `--profile-output` writes a bounded camera and
queue sidecar for both benchmark and shots-only runs. Add
`MUDCRAB_PROFILE_HANDOFF_IMAGES=1` for separate cell-crossing/reversal image requests.
Those images are asynchronous and carry request-pose metadata; they cannot certify
exact frame matching. Keep PNG capture outside timing comparisons.

Run the procedural analyzer checks with:

```sh
python3 -m unittest scripts.tests.test_native_metal_profiling -v
```

On macOS, the same command also compiles and loads the native observer in short
processes to check valid defaults, integer boundaries and rejected configuration.
Those checks need the Command Line Tools and use no renderer or game assets;
they are skipped on other platforms. They do not qualify a native capture.
