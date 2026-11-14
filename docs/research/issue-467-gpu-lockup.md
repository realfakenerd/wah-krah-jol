# Issue #467: Riverwood GPU lockup investigation

Owner: issue467-implementation-5e2edc11. Source base:
`b5259d33a73e1df730f0730866595fd4cdaae0e3`. Checkout:
`<worktree>`, branch
`t3/investigate-gpu-lockup`. The initial investigation commit is
`f5950727c7c88809869467e235a937828bdae6fb`. The follow-up below bypasses the
confirmed GPU bounds hazards on Metal; it does not reproduce #467 or establish
its freeze cause.

## Preserved evidence

Original evidence root:
`<shared storage>/evidence/20261031-user-testing-ec59a76c`.
Read `main/first-session-failure/observation.json`, its `settings.json`, the three
`.ips` reports, `main/interactive.log`, and `defaults-fix/investigation.json`.
The three reports identify `engine`, firmware-detected lockup, and
`2026-10-31 01:53:11 -0500`. Their command-buffer trace IDs are `14529302539`,
`14529302559`, and `14529304811`; they contain no shader name or GPU capture.
The clean exit does not refute Taylor's observed freeze.

The Z-slice warning appears at `2026-10-31T06:52:58.552907Z`, about 12.45 seconds
before the reports. It changes capacity from 1024 to 4096. This establishes that
readback observed overflow, not that this pass caused the later lockup. Original
camera inputs, per-view cluster dimensions, submitted shaders and GPU work are
missing. Later user closes are not reproductions. The defaults comparison changed
lights, lighting and interaction together; it cannot establish causality.

Active playtest `defaults-fix/native-20261031T081950Z` is excluded from experiments.
No game, desktop, user settings or registered pack was changed by this worker.

## Ownership and reuse

Live GitHub read: [#467](https://github.com/Mudcrab-Team/mudcrab/issues/467) is open,
unassigned, with zero comments. Searches for `467`, `clustering`, `cluster_allocate`
and `"GPU lockup"` found no PR claiming this freeze. The `467` search returned
unrelated terrain PR #236; it is not a fix for this issue.

[PR #253](https://github.com/Mudcrab-Team/mudcrab/pull/253), by TaylorTurnerIT, is
open at `f34a694d0e5d795bb40268a430f7987845c55c58`. Its `gpu_tuning.rs` sets
`GlobalClusterSettings::gpu_clustering = None` during Startup. Local commit
`5c7086db6daa223df0e2f2811b288fd525130081` supplies that approach. Reuse that
single assignment for the CPU experiment; do not import the whole tiled profile.
It also changes render scale, shadows, reflections, drawing and visibility, which
would confound the comparison. Its historical performance claims are not #467
reproduction evidence. No GitHub text or refs were written.

## Pinned source findings

Cargo.lock pins Bevy 0.19.0 and wgpu/wgpu-core/wgpu-hal/Naga 29.0.4. The audit
compares selected registry sources with cached `.crate` contents and verifies each
archive against Cargo.lock. It reads archives in place and uses no GPU.

1. **Allocation tail lacks a guard.** In `bevy_pbr`'s
   `src/cluster/cluster_allocate.wgsl:100`, the global pass launches 256 lanes and
   writes `offsets_and_counts.data[i + local_id.x][0].x` for every lane, including
   the last partial block. `gpu.rs:1642` reserves only `cluster_count` elements;
   `mod.rs:689` adds exactly that many entries and the bind group binds the buffer.
   The buffer is newly created per view preparation, rather than a reused rounded
   allocation. A count of 3672 accesses indices 3672..3839 beyond its logical
   array: 168 lanes. Aligned controls 256 and 4096 have no tail. The initial
   default 16:9 grid can be 17×9×24 = 3672; that is an example, not a measurement of
   the failed frame. Dynamic resizing can change it.
2. **Z-slice overflow leaves the indirect draw count unbounded.**
   `cluster_z_slice.wgsl:144` increments `instance_count` before checking capacity.
   `cluster_raster.wgsl:103` reads `z_slices[instance_id]` without a guard, and
   `gpu.rs:1071` draws from that metadata without clamping. With capacity 1024
   and 1025 requested slices, instance 1024 reads beyond the list. CPU readback
   grows the list afterward. The warning confirms this overflow path was reached;
   the exact request count and failing view were not retained.
3. **Do not assume wgpu inserts bounds checks.** `bevy_shader/src/shader.rs:98`
   defaults to `ValidateShader::Disabled`. The disabled pipeline-cache branch calls
   `create_shader_module_trusted(..., ShaderRuntimeChecks::unchecked())` through
   `RenderDevice`. Pinned `wgpu-hal/src/metal/device.rs:161` therefore selects
   Naga's `BoundsCheckPolicy::Unchecked`. This is a source path, not inspection of
   the failed binary's compiled Metal shader.

These are concrete source hazards and discriminating experiment targets. Neither
has been linked to a recorded failing GPU command. These findings do not justify
changing lighting defaults, disabling point lights globally or claiming the freeze fixed. The local Metal CPU fallback below
avoids the confirmed unsafe shader paths without modifying Bevy.

## Metal safety workaround

The renderer selects Bevy's supported CPU clustering path when the actual selected adapter is Metal.
It reuses #253's `GlobalClusterSettings::gpu_clustering = None` assignment without
importing its other renderer tuning. The policy runs in `PreStartup`, after
`PbrPlugin::finish` initializes capabilities and before first light assignment
or render-world extraction. Both GPU bounds hazards above are excluded from the
Metal render workload. Point lights, shadows, storage-buffer support and other
cluster capabilities remain unchanged; settings files are not touched.

CPU-only schedule tests check first-Startup ordering, preservation of light and
shadow state, all other backends, an already selected CPU path, and apps without
renderer/PBR resources. No GPU is requested by those tests. CPU clustering can
change CPU time and cluster assignments; no performance or visual equivalence is
claimed. The original freeze still needs the coordinated native experiment below.
Remove or narrow the workaround only after the pinned GPU paths are bounded and
native validation supports enabling them on Metal.

Validation: `cargo check --locked -p engine --tests`, `rustfmt --check` and
`git diff --check` passed. All six focused `renderer_init::tests` passed at
`4ca9d4168d3ae4348aa9f9327f265ae84f97f6a7` with a clean tracked tree. They
exercise CPU schedules and synthetic adapter metadata; they request no GPU.
The first full-build attempt was blocked by the 60 GiB storage budget. Taylor
approved a process-local 68 GiB allowance for the successful run, retaining the
20 GiB reserve and two-build limit. No persistent storage policy changed.
Detailed commands, output and the test-binary checksum are retained at
`<shared storage>/evidence/20261031-issue467-metal-cpu-safety`,
owned by `issue467-coordination-metal-cpu-safety`. Native freeze confirmation,
performance and Skyrim visual parity remain unvalidated.

## Executed check

```sh
python3 tools/diagnose_cluster_bounds.py --registry "$CARGO_HOME/registry"
```

`CARGO_HOME` defaults to `~/.cargo`.

The audit checks source premises and enumerates allocation counts
1/255/256/257/3672/4096 and Z-slice counts 1024/1025/1536/3072 at capacity 1024.
Guarded allocation controls stay within the logical array. Output records source
hashes and exact premise lines. This is CPU/source validation only. No Cargo
build, engine test suite, native execution, performance or Skyrim parity claim.

## Prepared bounded native experiment

Run only after Taylor's active playtest ends and the coordinator approves the
native window. Do not stop or relaunch the current game. First isolate each hazard
without a game window, proprietary assets or true GPU out-of-bounds access:

- Use the pinned allocation shader with imports resolved to the same storage
  layout: two `vec4<u32>` per cluster, 32 bytes. For each count above, bind
  `round_up(count, 256) + 256` entries. Fill valid cluster counts with one point
  light each; initialize local offsets to `index % 256`. Fill trailing entries
  with offset `0x12340000` and zero counts. Dispatch only the global pass, one
  256-lane workgroup. Read back the whole buffer after completion.
- Compare original with a single diagnostic guard around the offset addition:
  `if (i + local_id.x < cluster_count) { ... }`. Keep `storageBarrier()` outside
  that condition so all lanes execute every barrier. Each variant uses a fresh
  buffer. The original should change 255 trailing entries at count 257 and 168
  at count 3672; the guard should change none. At counts 1 and 255 the extra
  addition is zero, so unchanged canaries do not refute the source access hazard.
  The complete 256-entry trailing block must remain untouched in every case.
- Separately run the Z-slicing/count path with requested slices
  1024/1025/1536/3072 and logical capacity 1024, but physically bind 4096 initialized
  slices. Record generated `instance_count` and the number of vertex instances
  addressing the padded region; do not read outside the physical buffer. Compare
  with a diagnostic vertex guard that emits a degenerate primitive when
  `instance_id >= logical_capacity`. Keep the requested-count readback separate
  from any clamped draw count. This isolates over-dispatch without provoking a
  driver fault. These synthetic cases do not establish Riverwood causality.

Bound this probe to ten submissions (six allocation cases, four Z-slice cases)
per variant, one process per variant, at most 30 seconds per process, with a
5-second queue-completion timeout. Stop the campaign on the first timeout, device
loss or validation error; preserve the first receipt, with no automatic retry.
Capture source SHA, dirty diff, resolved shader text, buffer sizes, adapter, exact
command, binary hash and readbacks. A future harness must implement these bounds
before execution; no native harness is supplied or claimed validated here.

If the safe probe supports the hazards, coordinate a separate Riverwood test:
hold the original settings snapshot, pack `user-testing-main-b5259d33`,
worldspace `0x3c`, grid `(5,-12)`, exact camera pose/input schedule, dimensions,
point-light budget/shadows, reflections, culling and rendering workload fixed.
Use baseline GPU clustering, this Metal CPU-fallback candidate, and baseline GPU
clustering with only `initial_z_slice_list_capacity = 4096` as three separate
arms. Build GPU arms from the pre-workaround source in isolated checkouts; the
production Metal policy deliberately cannot be re-enabled by a saved setting. Preserve the same frozen defaults patch on every arm only if needed; it was
not needed for this source audit. Preallocation does not address the tail hazard.

Use an explicit **session-only** settings snapshot path; the current macOS path
reads HOME, and hidden/benchmark modes skip interactive saved settings. Do not
change HOME, overwrite Taylor's settings or assume `--hidden-window` preserves
them. An experiment harness must load/apply the snapshot and serialize resolved
settings for equality before comparison. Start each arm from a fresh process;
bound each to 45 seconds, preserve first failure, and schedule no more than one
run per arm without further coordination. A time-based route does not guarantee
identical per-frame GPU work: record per-frame camera, cluster dimensions,
clusterable counts and buffer capacities. Capture labeled GPU work with Metal
validation/Xcode capture to identify the failing pass. If Taylor must supply the
missing input sequence, collect it during that coordinated session.

Even a stable CPU arm or a warning-free preallocated arm is only supporting
evidence. A causal fix needs a matched failing workload and a change confined to
the implicated path, followed by native validation. This investigation leaves
#467 open.

## Retention and settlement

Retain audit JSON, input hashes, ownership snapshot, source diff and settlement
receipts under
`<shared storage>/evidence/20261031-issue467-5e2edc11`.
Owner is `issue467-implementation-5e2edc11`; purpose is independent source review
and preparation of the next bounded experiment. Original evidence remains in
place. No build lease, worker process, pack conversion or task-owned large scratch
was created. Run installed `workspace.py settle` and inspect `prune` without
`--apply`; retain their receipts. No cleanup or branch retirement is authorized.
