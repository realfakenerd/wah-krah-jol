# Phase 2 Integration and Acceptance

The integration implementation is automated and does not redistribute Skyrim data. A legally
owned Skyrim Special Edition installation is required only for the final real-world runs.

## What is enforced

- Converter schema 14, cell cache version 3, and world database schema 4 are exact-version contracts; stale outputs are
  rejected by both launcher and engine.
- Every converted `STAT`, `MSTT`, and `FURN` GLB is inspected. POSITION accessor bounds are
  transformed through the glTF node hierarchy and stored with `bounds_valid=1`.
- `integration-report.json` records missing/invalid meshes, missing diffuse textures, database
  counts, and whether the converted set passed.
- Runtime instance bounds transform all eight local AABB corners by reference translation,
  rotation, and scale.
- LAND is split into four 17×17 quadrants. Each quadrant uses one `BTXT` base plus at most five
  ordered `ATXT` overlays; weights use tangent/secondary-UV channels while VCLR remains an
  independent vertex tint, with the base layer receiving the remaining normalized weight.
- Water resolves the converted WATR flow-normal texture when one is available.
- Streaming diagnostics record requests, stale responses, failures, load/unload peaks, query time,
  main-thread commit time, surface readiness, terrain seams, and terrain/water validation failures.
- Benchmark reports contain mean FPS, P50/P95/P99/worst frame time, process-memory peak and growth,
  entity count, system identity, streaming counters, thresholds, and a machine-readable result.

## Renderer decision

Phase 2 intentionally uses Bevy 0.19 GPU preprocessing, indirect drawing, `OcclusionCulling`, and
the depth-prepass HZB. A separate Vercidium-derived renderer is not maintained unless profiling
shows that Bevy's path fails the same acceptance scenarios. This avoids two competing render paths
without weakening the measurable acceptance gate.

## One-command acceptance

From PowerShell, run the fast asset-independent smoke suite:

```powershell
.\scripts\phase2-acceptance.ps1 -Quick
```

Run the full synthetic suite plus real rural, dense, and streaming-stress scenarios:

```powershell
.\scripts\phase2-acceptance.ps1 `
  -Assets D:\ConvertedSkyrim `
  -Worldspace 0x3c `
  -RuralGridX 0 -RuralGridY 0 `
  -DenseGridX 18 -DenseGridY -5
```

The coordinates must be selected from the user's converted load order. The full defaults are 120
seconds synthetic, 5 minutes for each representative world area, 10 minutes of fast streaming, and
a continuous 30-minute stability run.
Reports are written below `target/phase2-acceptance/`. Any quality gate, missing artifact, streaming
failure, average below 60 FPS, P95 above 16.67 ms, or memory growth above 0.5 GiB returns a non-zero
exit code.

For hardware unable to target 60 FPS, thresholds must be explicitly recorded rather than silently
relaxed:

```powershell
.\scripts\phase2-acceptance.ps1 -Assets D:\ConvertedSkyrim -MinimumFps 30 -MaximumP95Ms 33.34
```

## Manual visual sign-off

The automated result must be accompanied by a short visual pass over the chosen real-world areas:

- no terrain cracks or inverted cell edges;
- no premature object disappearance under rotation or scale;
- all six terrain layers match the source data;
- reflection remains stable and never recursively renders water;
- fast movement does not duplicate cells or leave orphaned entities;
- missing assets are explained by `integration-report.json`.

The executable acceptance campaign, screenshot capture, signed review format, regression policy and
final verdict are documented in [`02-acceptance.md`](02-acceptance.md). The repository cannot
manufacture the legally owned assets or substitute a hardware measurement that has not actually run.
