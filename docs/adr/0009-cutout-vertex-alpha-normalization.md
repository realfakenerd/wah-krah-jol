# ADR-0009: Normalize vertex alpha on Cutout shapes during conversion

- **Status:** Accepted
- **Date:** 2026-09-26

## Context

Bethesda foliage NIFs carry per-vertex `COLOR_0` alpha as an artistic edge fade on
alpha-tested needles and leaves. The runtime cutout decision multiplies vertex alpha
into the alpha test, so minified distant foliage discards to sky at the authored
0.44 cutoff and pines render white at distance. PR #27 expands model extraction to
all placeable types, which makes the forest visible and the failure mode prominent.
A survey of converted assets found 244 of 25,391 GLBs carry sub-1 vertex alpha on
MASK primitives; the affected shapes are not only pine needles (e.g. wolf fur
cards).

## Decision

`normalize_cutout_vertex_alpha` in `crates/converter/src/mesh.rs` forces vertex
alpha to 1.0 on Cutout shapes on both NIF export paths, so the cutout decision
uses texture alpha alone. The change ships with the schema 14 to 15 migration
(GLBs only; textures and scripts are reused).

## Consequences

- Distant conifers render solid dark-green beside yellow aspens instead of
  stippled white, verified by matched before/after acceptance screenshots.
- Bethesda's per-vertex edge fade is lost on every touched Cutout shape (244
  files); fur cards and similar geometry render slightly fuller.
- Near-field renders already matched this behavior and look right, so the change
  is a fidelity tradeoff, not a regression in the common view.
- Revisit when engine LOD billboards exist: scope the normalization by material
  semantics, or replace the converter-side normalization with a renderer-side
  rule that preserves edge fade without the distance collapse.
