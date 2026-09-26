# Architecture Decision Records

This directory records the architecture decisions behind OpenSkyrim features. Each decision gets
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

ADRs 0001–0008 were produced by the synthetic fixture generator work tracked in
[issue #2](https://github.com/realfakenerd/OpenSkyrim/issues/2). Decisions for the NIF/ESM writers
follow in the same series.
