# Architecture Decision Records

This directory records the architecture decisions behind Mudcrab features. Each decision gets
its own numbered file using a lightweight MADR-style format: **Status**, **Date**, **Context**,
**Decision**, **Consequences**.

| ADR | Title | Status |
| :--- | :--- | :--- |
| [0001](0001-isolated-dummy-content-crate.md) | Isolated `dummy-content` crate | Accepted |
| [0002](0002-ground-truth-unmodded-install.md) | Ground truth is an unmodded game installation | Accepted |
| [0003](0003-archive-writer-layouts.md) | Archive writers emit converter-accepted layouts | Accepted |
| [0004](0004-dds-cubemap-legacy-caps2.md) | DDS cubemaps use the legacy `caps2` layout | Accepted |
| [0005](0005-security-and-performance-gates.md) | Security and performance gates are part of the deliverable | Accepted |
| [0006](0006-pure-rust-miniz-oxide.md) | `dummy-content` compresses with pure-Rust `miniz_oxide` | Accepted |
| [0007](0007-refuse-symlinked-path-components.md) | Fixture writes refuse symlinked path components | Accepted |
| [0008](0008-cargo-audit-quick-xml-ignores.md) | `cargo audit` ignores two build-time-only `quick-xml` advisories | Accepted |
| [0009](0009-cutout-vertex-alpha-normalization.md) | Normalize vertex alpha on Cutout shapes during conversion | Accepted |
| [0010](0010-lod-chunk-payload-format.md) | LOD chunk payloads are GLB files, indexed by manifest | Proposed |
| [0011](0011-lod-tier-selection-policy.md) | Fixed 4/8/16 tiers first; projected-size selection later | Proposed |
| [0012](0012-billboard-atlas-layout.md) | Per-worldspace billboard atlases with gutters and layout versioning | Proposed |
| [0014](0014-phase1-desktop-texture-contract.md) | Phase 1 desktop texture contract and portable-output deferral | Proposed; Route B scope approved by a maintainer on 2026-11-06 |

ADRs 0001–0008 were produced by the synthetic fixture generator work tracked in
[issue #2](https://github.com/Mudcrab-Team/mudcrab/issues/2). Decisions for the NIF/ESM writers
follow in the same series.
