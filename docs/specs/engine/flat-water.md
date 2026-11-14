# Shared flat-water query

Implementation for [#397 candidate 1](https://github.com/Mudcrab-Team/mudcrab/issues/397),
sharing the CELL/WRLD boundary of [#399 candidate 3](https://github.com/Mudcrab-Team/mudcrab/issues/399).
No upstream code was copied.

`world::water::water_at_point` returns the covering CELL identity, flat surface height and optional
WATR FormID. Call it with `Query<&FlatWaterBody>::iter()`, a worldspace ID and a canonical
`WorldPosition` in Creation units (XY horizontal, Z up). It does not test submersion: a point above
the surface still returns the covering body. The caller can compare its Z with `surface.height`.

`FlatWaterBody` lives on the existing streamed `WaterSurface` entity. There is no separate mutable
registry, record resolver or catalog. Coverage begins at cell commit, including while textures
prepare, and ends with recursive cell despawn. Retiring cells remain queryable while still drawn.
Floating-origin shifts do not change the stored Creation-space grid or local height field.
`None` means no coverage among these committed bodies; unloaded or unsupported water is unknown.

## Coverage and selection

- Current support is exterior CELL water with a complete 33x33 LAND height field. The converter
  already writes the CELL's local height/type or WRLD default into `TerrainSnapshot`. Spawning,
  texture accounting and body construction share `FlatWater::from_terrain` and the existing WATR
  catalog. Missing type/catalog data keeps the existing rendering fallback.
- Cells own `[min, max)` on both horizontal axes. Negative coordinates use the existing floored
  `WorldPosition` grid. Prefer grid plus local coordinates when absolute floats would lose precision.
- Within a cell, coverage requires positive depth above the detailed LAND mesh. Interpolation uses
  the renderer's `[a,b,c]`, `[b,d,c]` triangles, including their diagonal. The shoreline itself is dry.
  Merged terrain uses its drawn, welded height field rather than an unwelded cache copy.
- Overlapping coverage chooses the highest surface; equal heights choose the highest CELL FormID.
  Selection is independent of iteration order and query altitude. This explicit policy has not been
  established as Skyrim's native overlap rule. The overlap fixture supplies coincident cells; the
  current cache does not describe several independently authored bodies within a CELL.

The reflection camera still chooses one mirror plane for its whole view. That is a different
question from point coverage: water may be visible while the camera stands on dry ground. It does
not supply gameplay water coverage. No actor water consumer exists on this branch.

## Limits and related work

The cache does not carry authored water polygons, interior pool bounds or coverage for water
without LAND. LAND alone cannot account for NIF geometry, bridges, overhangs or caves. These cases
must not be inferred as full authored-water correctness. Shader waves do not change this flat
surface, and WATR flow-normal textures do not establish physical flow velocity.

This slice consumes the existing converter projection. WRLD parent inheritance, missing/cyclic
parents and disabled-water semantics proposed in #399 still need a record-resolution contract,
fixtures and any required cache changes in that existing owner. They must feed this same query,
rather than introduce another runtime water authority.

[#261](https://github.com/Mudcrab-Team/mudcrab/pull/261) is merged reflection clipping work.
Active [#299](https://github.com/Mudcrab-Team/mudcrab/pull/299) owns depth colour, fog and ripple
shading; this change does not replace it. Carry/native springs, swimming, waves, preloading and
LOD handoff remain outside this slice.

## Validation

Four pure fixtures cover overlap/ties, triangle shorelines/dry ground, negative coordinates/cell
boundaries/worldspace isolation, and absent/invalid/unsupported data. A streaming fixture checks
the actual spawned surface, floating-origin independence and unload cleanup. The merged-terrain
fixture checks coverage against the drawn mesh when cached heights disagree.

On 2026-10-31, macOS 27.0.1 arm64 with Rust 1.98.1 passed workspace formatting, Clippy with all
targets/features, and **3,244 tests with 0 failures and 59 ignored**. All six water fixtures above
passed. Workspace doctests also passed (no doctests defined). Rust source remained unchanged
through validation; only this validation note was updated afterwards.

Commands ran inside managed `workspace.py exec --isolated issue397-flat-water` leases:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --lib --bins --tests --examples --no-fail-fast
cargo test --locked --workspace --doc
```

`cargo-nextest` was unavailable, so standard Cargo ran the same target categories. The test build
emitted nonfatal toolchain debug-stripping (`rust-objcopy` missing `libLLVM.dylib`) and dependency
linker warnings. No engine/game launch, installed-game conversion, registered-pack write,
performance measurement or visual-parity check ran; synthetic conversions are part of the tests.
