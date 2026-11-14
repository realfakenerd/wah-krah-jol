# Native function-level profiling

Use a native sampling profiler alongside the engine's [profiling bundles](../roadmap/02-profiling.md)
when a slow frame needs function names, caller stacks or thread-wait analysis. Engine spans identify
stages; they do not explain every library or driver call inside those stages.

The Apple Silicon investigation on 2026-10-08 used **Samply 0.13.1**, **Firefox Profiler**, and the
**Metal Performance HUD**. Samply captured every engine thread at 1,000 Hz from an optimized release
build with line-table symbols. See [the investigation](../research/metal-function-profiling-20261008.md)
for its scene, results and source-level mechanisms. No graphics features were disabled.

## Where does the frame go?

1. Record an unprofiled release baseline for each scene. Save frame distributions and sample counts,
   the camera, settings, physical surface size, display refresh rate, requested present mode, source
   diff, executable checksum and asset provenance. Confirm that drawables are acquired and presented.
   A requested `AutoNoVsync` mode alone does not prove that presentation is unpaced.
2. On Metal, capture a short settled **Metal System Trace**. Compare CPU submission, GPU execution,
   drawable acquisition and presentation on the same timeline before calling the scene CPU- or
   GPU-bound. Record profiler overhead separately. GPU stages and CPU work can overlap; do not add
   their durations as a serial frame budget. Keep traces short and monitor free space: short captures
   can produce large temporary files.
3. Use engine spans and Samply 0.13.1 with Firefox Profiler to identify CPU functions in the same
   scene. Select the settled interval and inspect the render thread and worker threads. Distinguish
   sampled CPU time from off-CPU waits and unresolved symbols. For GPU shader or material costs,
   profile a Metal GPU capture in Xcode; a command-buffer interval is not a shader duration.
4. Form one change from a measured cost, then repeat the same scene and checks. Compare the same
   executable with a flag where possible. Keep the unprofiled before/after results separate from
   diagnostic captures, and preserve failed attempts and the exact commands.

On the M4 Pro with Xcode 27.1, the Command Line Tools selection needs this prefix for Instruments:
`DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcrun xctrace`. Verify both recording and
`xctrace export` on the host. Follow the checkout's managed storage and native-workload rules for
builds and captures. RenderDoc is a graphics debugger, not a timing profiler.

Bevy 0.19 omits command-encoder timestamp writes on macOS. Its GPU pass timing distributions can
therefore be all zero even when timestamp queries are reported as supported. Do not use those values
as GPU costs. For Bevy 0.20, recheck the timestamp implementation and diagnostic patch before using
this guidance. Metal Performance HUD encoder timing also conflicts with application-owned counter
sample buffers. For that method, use the measurement-only `native-hud-counters.patch` and its
[capture steps](../../scripts/profiling/README.md#controls-and-optional-capture). Its instrumented result is
not an unprofiled FPS comparison. The patch applies to `f9653bfd`, not `64108a1c` or `5840f49f`; run
`git apply --check` on the chosen source and update the patch before using it on a newer revision.
The [native Metal investigation](../research/metal-native-timeline-20261009.md)
explains the counter restriction and the earlier measurements.

## Build and capture CPU stacks

Install [Samply](https://github.com/mstange/samply/blob/main/README.md) using a prebuilt binary from
its [0.13.1 release](https://github.com/mstange/samply/releases/tag/samply-v0.13.1), or build that version:

```sh
cargo install --locked samply --version 0.13.1
```

Keep release optimization while enabling symbols. Full debug information is useful for source and
inline-stack inspection; the original capture used `CARGO_PROFILE_RELEASE_DEBUG=1` and reported
physical functions rather than assigning costs to inline source lines.

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 cargo build --locked --release -p engine --bin engine

profile_run="../mudcrab-profiles/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$profile_run"

samply record --save-only --unstable-presymbolicate --rate 1000 \
  --profile-name riverwood --output "$profile_run/cpu-profile.json.gz" -- \
  ./target/release/engine --assets "$PWD/modern_assets" \
  --worldspace 0x3c --grid-x 5 --grid-y -12 --stream-radius 2 \
  >"$profile_run/engine.stdout.log" 2>"$profile_run/engine.stderr.log"
```

This launches the ordinary game, including its default NOCLIP player mode. Let asset loading and
pipeline compilation settle, record at least a minute of steady rendering, then close the game.
Record the warmup and steady-range boundaries. Keep the window visible (`perf.py run --window background` keeps it under other windows without taking focus): macOS can return an occluded
surface and skip rendering while update loops continue; a hidden window never acquires a drawable at all. A high update FPS from that state is invalid
rendering evidence. Verify scene content as well as successful drawable acquisition.

Do not use `--main-thread-only`: render-thread work and task-pool drawable acquisition matter here.
Avoid comparing `quick` and `release` frame times; their optimization profiles differ. Profiling
overhead also means a profiled run is not an unprofiled baseline.

Samply can attach with `record --pid <pid>`. On macOS, its documentation requires `samply setup` for
attach support. Launching the locally built engine under Samply worked without that setup in this
investigation. Apple-signed system executables can reject its launch injection, so a failed system
Python probe does not establish that the engine cannot be profiled.

## Inspect and preserve a profile

```sh
samply load "$profile_run/cpu-profile.json.gz"
```

Use Firefox Profiler's thread timeline, call tree, inverted call tree and flame graph. Select the
steady range before ranking functions. Inspect the main thread, render thread and Compute Task Pool
workers separately; expensive work can move between workers.

Keep the `.json.gz` profile and the `.json.syms.json` sidecar together. The sidecar is important:
`--unstable-presymbolicate` stores symbols separately rather than embedding names into the raw
profile. `samply load` serves those symbols locally. Preserve the exact executable, debug information,
commit, dirty diff and command/environment too. If the browser blocks the local symbol server, import
a fully symbolicated download/export through Firefox Profiler's file picker; importing only the raw
profile can leave hexadecimal names. [Samply keeps data local](https://github.com/mstange/samply/blob/main/README.md)
until the user chooses to upload it.

Save profiles outside `target` when they must survive `cargo clean`. Raw game captures, symbols and
screenshots are investigation artifacts, not repository fixtures.

## Separate CPU work from waits

Samply records both on-CPU and off-CPU samples on macOS and Windows. A wide `parking`, `pthread_cond_wait`
or `nextDrawable` stack in a sample-count view is not proof of CPU time executing that function.

For custom analysis, sum each thread's `samples.threadCPUDelta` using the units in `meta`; do not
multiply it by `samples.weight`. Attribute that interval to the observed stack only as a sampling
estimate. Work may occur before the thread reaches a parked endpoint, so report those endpoint
observations separately from function execution estimates. Zero reported CPU between samples identifies
off-CPU residence, subject to timer resolution. Attribute it to blocking only when the sampled stack
supports that interpretation. Its duration is per thread, not a frame cost.

Self CPU is attributed to the leaf; inclusive CPU includes descendants and overlaps other rows.
Summed CPU ms/frame includes parallel threads and is not wall-clock frame time. Waits on different
threads overlap too. Retain unresolved addresses and symbol coverage. Distinguish same-named physical
symbols by library identity and function address; release/LTO can generate separate clones.

Compare identical camera, graphics settings, physical resolution, backend, presentation mode, build
profile and hardware. Format 2 bundles record observed physical and logical window dimensions and
scale factor; unavailable observations remain null. Earlier bundles wrote a nominal `[1600,900]`
resolution, which is unreliable on Retina displays. Read actual surface/window dimensions for those captures.
Do not equate model count, mesh assets, mesh entities, batch sets and native draw calls.

## Add Metal presentation and GPU evidence

On macOS, start a capture with the standard HUD and its logging enabled:

```sh
MTL_HUD_ENABLED=1 MTL_HUD_LOG_ENABLED=1 \
  samply record --save-only --unstable-presymbolicate --rate 1000 \
  --output "$profile_run/metal-cpu-profile.json.gz" -- \
  ./target/release/engine --assets "$PWD/modern_assets" \
  --worldspace 0x3c --grid-x 5 --grid-y -12 --stream-radius 2 \
  >"$profile_run/metal.stdout.log" 2>"$profile_run/metal.stderr.log"
```

The HUD supplies presented frame intervals and command-buffer GPU time. Apple explains the metrics
in [Understanding the Metal Performance HUD metrics](https://developer.apple.com/documentation/xcode/understanding-metal-performance-hud-metrics)
and [Monitoring your Metal app's graphics performance](https://developer.apple.com/documentation/xcode/monitoring-your-metal-apps-graphics-performance).
The GPU value can include idle periods between encoders; it is not active utilization or a shader's
cost. CPU and GPU durations overlap and must not be added. HUD log timestamps describe batches of
preceding presents; do not invent exact per-frame timestamps or engine frame IDs.

The 2026-10-08 HUD log contained exact adjacent duplicate interval/GPU pairs. Validate that pattern
before normalizing a capture; four equal records represent two observations, not one. Keep raw logs
and raw pairs with derived frame-time/FPS plots.

Bevy render diagnostics complement native sampling, but a supported timestamp feature or an
`elapsed_gpu` key does not validate the resulting values. The Apple capture's all-zero GPU pass
distributions were excluded. `elapsed_cpu` can cover recording deferred commands while the actual
wgpu/Metal encoding occurs later, so tiny CPU pass labels do not exclude expensive native encoding.

For a GPU timeline, use full Xcode's Instruments/Metal System Trace; for encoder, resource and shader
inspection, use a Metal GPU capture. Command Line Tools alone do not provide Instruments. Correlate
CPU submission, GPU execution, drawable acquisition and presentation in the same recording before
calling a scene GPU-bound. Do not enable `MTL_HUD_ENCODER_TIMING_ENABLED` while the app uses its own
counter-sample buffers; Apple's HUD documentation describes that incompatibility.

## Include engine stage and frame data

Use the existing benchmark/campaign when a bounded run needs engine frame percentiles, CSV frame
times, streaming state and render-stage distributions. Its `--benchmark-duration`,
`--benchmark-warmup-frames`, `--benchmark-frame-times` and `--profile-output` flags are documented in
[Phase 2 Profiling](../roadmap/02-profiling.md). Benchmark mode changes the camera/physics/presentation
path, so label it separately from ordinary play.

When `--profile-output` requests a bundle, the engine also logs primary drawable acquisition counts
every 300 render frames and at shutdown. Use those counts to identify occluded or interrupted
captures. A texture acquired before rendering does not prove that presentation completed, and the
shutdown summary can include teardown frames. Retain the last healthy periodic summary and crop
uncertain tails rather than treating a terminal missing drawable as measured rendering work.

A useful evidence bundle includes native profile and symbols, raw stdout/stderr, engine report and
frame CSV when available, warmup/crop boundaries, a verified scene image, machine/backend/settings,
actual physical dimensions, source/build identity, and the scripts used to derive charts and ranks.
Start with the slow frame's critical path, then explain the workload driving each function before
changing rendering or claiming a recoverable FPS gain.

See [measurement validity and optional draw counters](../research/profiling-measurement-validity-20261008.md)
for diagnostic timestamps, GPU validity and completed-frame submission counts, and
[the resulting fixes](../research/metal-performance-fixes-20261008.md) for implementation and validation.

## Check mesh preparation and missing terrain

CPU selection counts and successful upload acknowledgments do not prove that every surface reached
its render phase. Keep a scene image in each comparison and inspect it before accepting an FPS gain.

A bounded render-world audit compares cached mesh input offsets and counts against current allocator
ranges. Run it separately from timing captures:

```sh
MUDCRAB_MESH_RESIDENCY_AUDIT=1 MUDCRAB_MESH_RESIDENCY_AUDIT_FRAME=600 \
  ./target/release/engine --assets "$PWD/modern_assets" \
  --worldspace 0x3c --grid-x 5 --grid-y -12 --stream-radius 2
```

The log reports missing descriptors, missing or short allocations, absent input uniforms, and cached
offset/count mismatches. Empty indexed batches are counted separately. This reads CPU submission
metadata; it does not inspect GPU buffer contents or rendered pixels. This diagnostics split observes metadata only. It does not expose the historical
`MUDCRAB_REPAIR_MESH_RESIDENCY=1` re-extraction experiment. The audit adds measurement overhead
and runs separately from FPS comparisons.

An audit can pass while terrain remains absent: material specialization and phase membership are
separate from allocation and mesh input preparation. Inspect all three when an unchanged mesh fails
to appear after loading. A component refresh that restores the surface is a diagnostic clue, not a
reason to refresh every mesh on every frame.

For repeated camera, queue and coverage requirements, see the
[matched batching protocol](../research/terrain-batching-matched-protocol-20261009.md).

To compare two binaries or two sets of screenshots without a person watching, see the [performance loop](perf-loop.md). It also
runs Samply and the Metal trace for you (`perf.py run --profiler samply|metal-trace`) and reads their output
(`perf.py samply-top`, `perf.py gpu-busy`, `perf.py summarize`); see "Analysing a run" there.
