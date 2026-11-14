# Performance loop: measure, change, compare

`scripts/perf/perf.py` runs the engine on fixed scenarios, compares a base binary with a candidate
binary statistically, and compares screenshots, so a "measure, then optimise" loop can run without
a person watching. It is Python 3.11, standard library only. It does not change the engine: the
reports it reads are the ones [the profiling bundle](../roadmap/02-profiling.md) already writes.

```sh
python3 scripts/perf/perf.py scenarios          # list scenarios
python3 scripts/perf/perf.py preflight          # is this machine fit for timing runs
```

Measuring is `run` and `compare`; understanding a run is `summarize`, `gpu-busy` and `samply-top`
([Analysing a run](#analysing-a-run)).

## Window mode

Every timing run, `compare` and `shots` takes `--window {auto,background,visible,hidden}`
(`--visible` is an alias for `--window visible`):

| mode | what it does | use it for |
| :-- | :-- | :-- |
| `background` | a visible window placed under all other windows that does not take focus (the engine's `--background-window`) | timing and profiling on macOS |
| `visible` | an ordinary window; it takes focus and pops up | timing and shots when `background` is not available |
| `hidden` | `--hidden-window`: a window with visibility off | nothing on macOS |
| `auto` (default) | `background` when the engine's `--help` lists `--background-window`, else `visible` | everything |

The `auto` probe runs `<engine> --help` once per binary SHA-256 and caches the answer in
`~/.cache/mudcrab-perf/engine-flags.json` (`MUDCRAB_PERF_CACHE` moves it). `compare` resolves `auto`
once for both binaries, so a base without the flag and a candidate with it both run `visible`;
asking for `background` when either lacks the flag is refused (`background_window_unsupported`).
The mode is recorded in `run.json` as `window_mode`.

**On macOS a hidden window is not representative for timing.** On the dense Riverwood scenario the
same binary took 10.45 ms/frame hidden (3200x1800) and 15.71 ms/frame visible (3024x1800). A Metal
trace of a hidden run recorded about 157,000 command buffers in 30 s and zero drawable
acquisitions: a hidden window renders but never acquires a drawable or presents, so its timings
exclude presentation, and the CPU cost of acquiring and handing over drawables with it. (In the
visible run `render/swapchain_acquire` averaged 4.5 ms and `render/specialize_and_views` 4.7 ms; in
the hidden run 0.03 ms and 0.27 ms.) A hidden window also renders black frames on macOS, so `shots`
refuses it there (`hidden_window_black_frames`). `compare` puts a warning in `summary.md` when it
ran hidden on macOS, and refuses a scenario whose base and candidate ran in different modes
(`window_mode_mismatch`). Hidden remains fine on platforms where it presents normally.

## Scenarios

`scripts/perf/scenarios.json` holds the scenarios of `scripts/phase2-baseline.ps1` with the same
flags and grids (`synthetic`, `rural`, `dense` = Riverwood at grid 3,-12, `dense-legacy`, `water`,
`stress`, `stability`, `travel`, `check`), plus `terrain-water` (`--terrain-water-fixture`) and
`stress-still` (the stress stream radius without the fly-through). `synthetic` and `terrain-water`
need no converted data; the others need `--assets`, and the harness adds
`--allow-incomplete-assets` because the real pack's manifest says `complete=false`. Route
scenarios run at least as long as the route takes plus 60 s, as the baseline script does. The
harness adds the benchmark, profile and acceptance flags itself and neutralises the acceptance
thresholds, so a slow frame never turns a measurement into a failed run.

World scenarios default to 300 warm-up frames: on this Mac Riverwood is ready after about 90
frames, and the default of 60 would put the world load in the measured window. A run whose world
became ready after the warm-up, or never did, carries that as a warning.

## One run

```sh
python3 scripts/perf/perf.py run --engine bins/engine-main-64108a1c --scenario dense \
  --assets modern_assets_real --out evidence/dense-1 --duration 20
```

The output directory gets `report.json` (the benchmark report), `bundle/` (the profile bundle),
`stdout.log`, `stderr.log` and `run.json`: the command line, the binary's SHA-256, the window mode,
how the run ended and any warnings.

### Profilers

`--profiler samply` or `--profiler metal-trace` runs the same scenario under a profiler, writing its
output into the run directory. A profiled run carries the profiler's overhead and says so in its
warnings: use it to look inside a run, never as one side of `compare`.

- `--profiler samply` wraps the engine as `samply record --save-only --unstable-presymbolicate
  --rate 1000 --output <run>/cpu-profile.json.gz --`, which leaves `cpu-profile.json.gz` and
  `cpu-profile.json.syms.json` (`--samply PATH` if it is not on `PATH` or in `~/.cargo/bin`). The
  engine can hang after writing its report (below). When the watchdog has to stop it, it signals the
  engine process, not samply, and then gives samply up to `--samply-finalize-timeout` (240 s) to
  write the profile; signalling samply would lose the capture. Build the engine with
  `CARGO_PROFILE_RELEASE_DEBUG=2` or the profile is mostly addresses.
- `--profiler metal-trace` compiles `scripts/profiling/metal_trace.m` with the command in
  [the profiling README](../../scripts/profiling/README.md) (cached by the SHA-256 of the source under
  `~/.cache/mudcrab-perf`, copied into the run directory) and runs the engine with
  `DYLD_INSERT_LIBRARIES`, `MUDCRAB_METAL_TRACE_FILE=<run>/native.ndjson` and
  `MUDCRAB_METAL_TRACE_START_ACQUISITION` (`--metal-trace-start-acquisition`, default 600: a start
  after the warm-up). The trace stops at one million rows, about 15 s of the dense scenario, so a
  30 s window is only partly covered; `gpu-busy` says when that happened. Use `--window background`
  or `visible`: a hidden window produces no acquisitions to trace.

`--dry-run` prints the full command, environment included.

The engine sometimes writes its report and then never exits (issue #525, a hang in Bevy's
pipelined-render teardown; the same run can also exit non-zero after a complete report). The
watchdog polls the process, treats a parseable report as "the run is done", gives the process
`--grace` seconds (20) to exit and then kills its process group. A run that never writes a report
is killed at `--timeout` (duration + 120 s). Classes:

| class | meaning | usable |
| :-- | :-- | :-- |
| `ok` | the process exited after a complete report | yes |
| `hung_after_report` | the report was complete, the process had to be killed | yes |
| `timeout_no_report` | killed at the timeout without a report | no |
| `failed` | exited, or could not start, without a report | no |

A non-zero exit after a complete report stays `ok` with a warning. The engine exits non-zero when
one of its own gates fails (for example `commit_budget_respected`), and the numbers are still a
measurement.

## Comparing two binaries

```sh
python3 scripts/perf/perf.py compare --base bins/engine-main-<sha> --cand bins/engine-<branch>-<sha> \
  --scenarios synthetic,dense --pairs 6 --assets modern_assets_real --duration 30 \
  --out evidence/perf/<topic>
```

Runs are interleaved in pairs and the side that goes first alternates (base, candidate; candidate,
base; ...), so thermal drift and background load land on both sides. For each metric the harness
takes the candidate/base ratio inside each pair, the median of those ratios, an exact two-sided
Wilcoxon signed-rank test on the log ratios (`p` in the table), and a seeded bootstrap 95% interval
of the median (10,000 resamples of whole pairs, `--seed`) for reading the spread.

A verdict is `improved` or `regressed` only when `p` is at most 0.05 **and** the median effect is
at least `--min-effect` (0.02). Otherwise it is `noise`. The bootstrap interval does not decide
anything: with few pairs it is roughly [smallest ratio, largest ratio], so with three pairs it
excludes 1.0 about a quarter of the time when nothing changed. Times are lower-is-better;
`average_fps` is higher-is-better. Metrics: frame time mean, p50, p95, p99 and fps; the mean of
every GPU pass that reports a non-zero time (Metal's timestamp queries often read all zero, and
those passes are skipped); the means of the eight most expensive CPU spans and schedules; process
memory peak when the report has one. The scenario's summary verdict is drawn from the four frame
time metrics only (checked: `analyze_scenario` reads `PRIMARY_METRICS` and nothing else); the others are
listed when they are not noise, are uncorrected for multiple comparisons (about twenty rows, so one is often
flagged by chance) and are leads to investigate, not verdicts. A pair is dropped when either
run is not usable, and a verdict needs at least six usable pairs (the default `--pairs`): with
five or fewer the test cannot reach 0.05 even when every pair moves the same way.

`verdict.json` has everything (including per-pair ratios and every run's class); `summary.md` is
the table to paste. Exit code 3 means refused.

### Refusals

The comparison refuses a scenario, with a named error in `verdict.json`, when:

- `fingerprint_mismatch`: any run's fingerprint differs from the others in anything except
  `commit` and `dirty_worktree` (resolution, backend, adapter, graphics settings, schema versions,
  build profile). Mixing a hidden and a visible window changes the resolution, so both sides always
  use the same mode.
- `inventory_mismatch`: the median mesh entities, terrain patches or water surfaces differ by more
  than 2% (`--inventory-tolerance`, with a slack of 2 for tiny counts). A candidate that draws less
  is not faster.
- `too_few_valid_pairs`: fewer than two pairs had a usable run on both sides.

`preflight` refuses the whole run (exit 3) unless `--force`: on battery, a thermal speed limit,
Low Power Mode, a running `cargo` or `rustc`, another engine process, or under 5 GiB free. On
other systems the machine checks are skipped. A non-empty `--out` also needs `--force`.

### Noise floor

Run `compare` with the same binary as base and candidate. Every verdict must be `noise`; the spread
of the per-pair ratios shows roughly how small an effect that setup can resolve. Do this once per machine state and
scenario, and again whenever the numbers look odd.

On an M4 Pro (macOS, Metal, hidden windows, which exclude presentation, 20 s windows, 300 warm-up frames), `synthetic` with the
same binary on both sides gave `noise` for every metric over 6 pairs; the frame-time ratios fell
between 1.011 and 1.099 with p from 0.062 to 0.156. The bootstrap interval for `frame_ms_p99` there
was 1.000 to 1.204, a hair from excluding 1.0, which is why it does not decide verdicts. As a positive
control, the same binary run under `taskpolicy -c background` (efficiency cores only, same
fingerprint) came out `regressed` on every frame metric with ratios around 6 and p = 0.031, the
smallest value six pairs allow. That shows the path end to end, not the smallest effect the setup
can resolve; for close calls use more pairs and longer windows rather than a smaller `--min-effect`.

Across those 24 runs, 5 were `hung_after_report` (the report was complete and the engine did not
exit; see #525) and were kept. Run warnings in `summary.md` also list runs that never reached
world-ready or failed the engine's own acceptance gate; check them before trusting a close call.

## Analysing a run

`compare` says whether something changed. These commands say where the time goes. Each prints a
short text and, with `--json OUT`, writes the same data as JSON. They read what a run already wrote
and start nothing, so they are safe while something else is being measured.

```sh
python3 scripts/perf/perf.py summarize evidence/dense-1                       # what kind of bound is this run
python3 scripts/perf/perf.py summarize evidence/dense-1 --against evidence/dense-0
python3 scripts/perf/perf.py run --engine bins/engine-main-<sha> --scenario dense --assets modern_assets_real \
  --window background --profiler metal-trace --out evidence/dense-trace
python3 scripts/perf/perf.py gpu-busy evidence/dense-trace/native.ndjson --report evidence/dense-trace/report.json
python3 scripts/perf/perf.py summarize evidence/dense-trace --gpu-trace evidence/dense-trace/native.ndjson
python3 scripts/perf/perf.py run ... --profiler samply --out evidence/dense-cpu
python3 scripts/perf/perf.py samply-top evidence/dense-cpu/cpu-profile.json.gz --top 25
```

The workflow is: `summarize` first, to learn which resource limits the frame; `gpu-busy` when the
answer depends on the GPU; `samply-top` when the answer is a thread, to name the functions.

### summarize

`summarize RUN_DIR [--against RUN_DIR] [--gpu-trace NATIVE.ndjson]` reads `run.json`, `report.json`
(the path in `run.json` may be relative to where the run started; the one in the directory is used
when it is not found) and prints:

- the frame statistics of the measured window;
- the **thread budget**: frame time, main world, extract, the main thread's wait for the render
  thread, the render thread and its two big parts. The spans nest and the threads overlap, so the
  rows are compared with the frame time, never added up;
- a one-line **bound** with the numbers behind it (below);
- the CPU spans, split into per-frame spans and asynchronous latencies (below);
- the scene inventory (mesh entities, terrain patches, water surfaces, entity count, the frame the
  world became ready) and the run's warnings, including a hidden window on macOS, a late
  world-ready, and GPU pass timings that are all zero.

With `--against OTHER` it adds the per-metric change of this run against `OTHER` and notes what
differs besides the binary: window mode, scenario, any fingerprint field (resolution, graphics
settings) and the scene counts. A change measured across a window-mode or resolution difference is
not a code change.

#### Reading the bound

| bound | rule | what to do |
| :-- | :-- | :-- |
| `gpu-bound` | a trace is given and the GPU was busy for at least 90% of the wall time | the CPU is waiting; look at GPU work (passes, draw count, resolution) |
| `render-thread-bound` | the render thread takes at least 85% of the frame and the main thread waits for it for at least 25% | `samply-top --thread render`; without a trace this may still be the GPU, because the render thread blocks in drawable acquisition and presentation |
| `main-thread-bound` | main world plus extract take at least 85% of the frame and the wait is under 10% | `samply-top --thread main` and the main-schedule spans |
| `unclear` | none of the above | read the numbers; pipelined frames can be limited by a mix |

Without a trace only the two thread rules can fire, and `render-thread-bound` says so. Examples
from the dense scenario on an M4 Pro, main at `64108a1c`: the hidden run (10.45 ms) has a render
thread of 9.9 ms (95%), a main-thread wait of 6.7 ms and is `render-thread-bound`. The visible
run's Metal trace shows the GPU busy 96% of the wall time, 14.6 ms of a 15.2 ms frame, so that run
is `gpu-bound`: its render thread is also 97% of the frame, but it is waiting on the GPU.

#### Per-frame spans and asynchronous latencies

The report's CPU spans mix two kinds of measurement. A **per-frame** span fires about once per frame
(its count is at least half the measured frames): `render_world/render_thread` 9.9 ms,
`render_world/render/prepare` 4.2 ms, `render_world/render/graph_and_present` 4.7 ms,
`render_world/wait_for_render_thread` 6.7 ms. These add to the frame time and are ranked by mean.
An **asynchronous latency** is measured from submission to completion across many frames, and its
count is the number of requests, not frames: `lod/db_request_total` 348 ms over three requests,
`assets/model_ready` 347 ms. These say how long loading takes, not what a frame costs. Ranking all
of them by mean puts the latencies on top, which is misleading. A span whose mean exceeds twice the
frame time is also treated as asynchronous, whatever its count: `assets/model_ready` fires 2,632
times in a 2,871-frame window because the scene has about that many assets. Counter-like spans
(`assets_added/*`) hold counts and are listed apart. Streaming latencies do change with the window
mode (the hidden dense run's database queue waits were about twice the visible run's), because the
world loads while frames are produced at a different rate; compare them only within one mode.

### gpu-busy

`gpu-busy NATIVE.ndjson [--report report.json] [--crop SECONDS]` streams a Metal trace written by
`--profiler metal-trace` (about 570 MB; one pass, a second) and reports, over the window between the
first and last GPU interval minus `--crop` (default 1 s) at both ends:

- **GPU busy**: the union of the command buffers' GPU intervals over the window. Overlapping buffers
  count once and so do idle gaps. It is elapsed time with work in flight, not shader utilisation;
- **GPU ms per frame**, from the report's `average_fps` (without `--report`, from the presents in
  the window);
- command buffers (those with a valid GPU time) in the window and per frame;
- drawable acquisitions and presents, and the health records: dropped events, hook failures, clock
  failures and whether the trace hit its row cap.

Unlike `scripts/profiling/analyze_native.py`, it does not refuse a capture with a few dropped
events: it prints the count and warns. A trace with command buffers but no drawable acquisitions
prints `this run never acquired a drawable (hidden window?): timings exclude presentation`.

Visible dense run: GPU busy 96.1% of the wall, 14.57 ms of a 15.16 ms frame, about 150 command
buffers per frame, 1,033 acquisitions, 18 dropped events, and the trace stopped at its row cap after
15.8 s. The same scenario hidden: 65% busy, 7.5 ms of an 11.6 ms frame, and zero acquisitions.

### samply-top

`samply-top PROFILE.json.gz [--thread NAME] [--top N] [--skip SECONDS] [--against OTHER.json.gz]`
loads the processed profile and the `.syms.json` sidecar with the same name, resolves every frame
address to a function (a frame already named in the profile keeps its name) and prints, per thread
group, the top functions by **self** and by **inclusive** CPU ms. The groups are `main`, `render`
(the unnamed thread that runs Bevy's pipelined-rendering loop) and `compute` (the Compute Task Pool
workers added together); `--thread` takes those names, `io`, `async-compute`, `all` or part of any
thread name.

CPU is the sum of `samples.threadCPUDelta` in the units `meta.sampleUnits` declares, never the
sample weight or the sample count, as in [native profiling](native-profiling.md). `--skip` (default
10 s, counted from process start) drops loading before ranking. The header says how many samples and
CPU ms stayed unsymbolicated, and how many distinct addresses.

Two things to keep in mind when reading it:

- A thread's CPU delta since its previous sample lands on the stack at the next sample, so a
  thread that parks right after working shows that work under `__psynch_cvwait` or
  `mach_msg2_trap`. Those rows carry a `*` and the group header states how much CPU they hold.
  Read them apart from function execution: on the dense capture 81% of the main thread's CPU, 24% of the
  render thread's and 67% of the compute workers' sat on parking endpoints.
- Inclusive rows overlap (a caller includes its callees), and the root frames present on every
  sample are omitted. Summed CPU over several threads is not wall-clock frame time.

`--against OTHER` prints, per group and for both self and inclusive CPU, the functions whose CPU
changed most, in ms per second of the window so that profiles of different lengths compare.
Compare profiles of the same scenario, window mode, resolution and build.

## Screenshots

```sh
python3 scripts/perf/perf.py shots --engine bins/engine-main-<sha> --shots scripts/perf/shots/riverwood.json \
  --assets modern_assets_real --out evidence/shots/main
python3 scripts/perf/perf.py shots --engine bins/engine-<branch>-<sha> ... --out evidence/shots/branch
python3 scripts/perf/perf.py visual --base evidence/shots/main --cand evidence/shots/branch \
  --out evidence/shots/diff [--noise-floor evidence/shots/noise/visual.json]
```

`shots` never uses a hidden window on macOS (`--window hidden` is refused there): `--hidden-window`
renders uniformly black frames. With the default `--window auto` it uses `--background-window` when
the engine has it, else a visible window, which appears for about ten seconds. `scripts/perf/shots/riverwood.json` has
four 1280x720 poses around Riverwood (Tamriel, grid 5,-12 and 5,-11). The file format is in
[reference shots](../specs/engine/reference-shots.md).

`visual` compares matching PNGs. It uses NVIDIA FLIP (mean, p99, fraction of pixels over 0.1, and a
heat map beside `visual.json`) when `flip-evaluator` and numpy are importable, and otherwise its
own metric: mean absolute difference on the 0-255 scale and the fraction of colour samples that
differ by more than 8, with the PNG decoded in pure Python (8-bit, non-interlaced). `visual.json`
records which metric ran. The pure-Python path takes about a second per 1280x720 pair. To run
FLIP without installing anything globally:

```sh
uv run --no-project --with flip-evaluator --with numpy \
  python scripts/perf/perf.py visual --base <dir> --cand <dir> --out <dir>
```

`visual` exits 3 when any shot fails, so a script or agent can stop on it.

A shot fails when a frame is blank (uniform, or near black), when it is missing or undecodable,
when the sizes differ, or when a metric exceeds its limit: the noise floor for that shot plus a
margin (0.5 and 0.005 for the fallback metric, 0.005 and 0.005 for FLIP; `--margin-mean`,
`--margin-fraction`). Capture the same binary twice and pass that `visual.json` as `--noise-floor`;
without one the margin is the whole limit. Identical file sizes across all shots of one side mark
a suspected black run and put every shot on the review list. `review` in `visual.json` is the list
to look at with your own eyes (or an image-reading agent) before accepting a change; a pass means
"within the noise", not "looks right". Water animates and has no fixed phase flag on this binary, so
expect water shots to need a noise floor.

Two captures of one binary on this Mac gave a mean difference of 0.001 to 0.005 on the 0-255 scale
and almost no pixels over the threshold.

## Building a candidate

```sh
python3 scripts/perf/perf.py build <ref> --worktree-root scratch/worktrees --bin-dir scratch/bins
```

Resolves the ref to a SHA and reuses `<bin-dir>/engine-<sha8>` when it exists. Otherwise it makes
a detached worktree, runs `CARGO_INCREMENTAL=0 cargo build --locked --release -p engine --bin
engine` there with the worktree's own target directory (any inherited `CARGO_TARGET_DIR` is
dropped), copies the binary and removes the worktree and its target (`--keep-worktree` keeps
them). It refuses with under 15 GiB free. `--dry-run` shows what it would do. A build takes a
while and `target/` is 9 to 20 GiB; check the disk first.

## Caveats

- Use AC power, nothing compiling, and the same Mac for base and candidate. `preflight` checks
  most of this.
- On macOS, time with `--window background` or `visible`. A hidden window never presents, so its
  frame times are too low and not representative; it also gives black pictures.
- Do not point two worktrees at one `CARGO_TARGET_DIR`.
- Compare binaries built the same way (release, same toolchain). The fingerprint's build profile
  is checked, the compiler is not.
- Each run is also a streaming run: a world scenario's first seconds load cells. The warm-up
  covers that; check the run warnings if you shorten it.

## Agent loop

1. Baseline. Build `main` into `bins/`, run the noise floor for the scenarios you care about, and
   keep `verdict.json`.
2. One hypothesis, from `summarize` (which resource limits the frame), `gpu-busy` and `samply-top`
   (which function), and the profile bundle (`bundle/cpu-spans.json`, `gpu-passes.json`): "the
   `render/prepare` span costs 1.4 ms because ...".
3. One change, in its own worktree and branch, with its own target directory.
4. Build it with `perf.py build`, then `compare` against the base binary, at least the scenario the
   change targets plus one it should not touch.
5. If `compare` refuses, read the error. A scene-inventory refusal usually means the change
   altered what is drawn.
6. If the verdict is `improved` or `noise` with a plausible win, run `shots` for both binaries and
   `visual` with a noise floor; look at everything on the `review` list.
7. Record the result: scenario, pairs, medians, p, interval, verdict, visual outcome, commit. Keep
   failed and noise results as well as wins; a note that something made no difference saves the
   next attempt.
8. Only then open a PR, with `summary.md` and the before/after images as evidence.
