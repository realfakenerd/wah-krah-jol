# Wah Krah Jol

![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/realfakenerd/wah-krah-jol?utm_source=oss&utm_medium=github&utm_campaign=realfakenerd%2Fwah-krah-jol&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)
[![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://www.rust-lang.org/)
[![Engine](https://img.shields.io/badge/Engine-Bevy_0.19-blue.svg)](https://bevyengine.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE-APACHE)
[![CI Status](https://img.shields.io/badge/CI-passing-brightgreen.svg)](#-quick-start-development-setup)
[![Platforms](https://img.shields.io/badge/Platforms-Windows_%7C_Linux_%7C_Android-purple.svg)](docs/specs/meta/platforms.md)

An open-source, high-performance game engine reimplementation compatible with **The Elder Scrolls V: Skyrim (Special Edition)** assets, built from the ground up in **Rust** using the **Bevy Engine**.

Inspired by open-source engine reimplementations like OpenMW, **Wah Krah Jol** modernizes legacy Bethesda game formats (`.esm`, `.bsa`, `.nif`, `.dds`, `.pex`) into GPU-native, stream-friendly formats (`glTF 2.0`, `KTX2 Basis`, `SQLite 3 / libSQL`, `Luau`) to deliver **60+ FPS high-performance gameplay without loading screens**.

> [!IMPORTANT]
> **Legal Disclaimer:** Wah Krah Jol is an independent open-source project and is **not** affiliated with, endorsed by, or connected to Bethesda Softworks LLC, ZeniMax Media Inc., or Microsoft Corporation. "The Elder Scrolls" and "Skyrim" are registered trademarks of Bethesda Softworks / ZeniMax Media. This repository distributes **no proprietary game assets**. A legally purchased copy of the original game is required to supply game data. See [LEGAL.md](LEGAL.md) for full legal notices.

---

## ✨ Features & Architecture Highlights

- **🚀 Zero Loading Screens:** Interior and exterior city transitions load 100% seamlessly in real time using libSQL spatial R-Tree indexing and `rkyv` zero-copy memory mapping.
- **⚡ High-FPS Vercidium Rendering:** Batched instanced indirect draw calls (`DrawMeshInstancedIndirect`) reduce 5,000+ individual draw calls to under 100 per frame.
- **📜 Luau Scripting Engine (`mlua`):** Replaces single-threaded Papyrus with sandboxed, JIT-compiled Luau scripts. Includes async HTTP networking for live web APIs (e.g. real-world weather sync mods).
- **📱 Cross-Platform (Desktop & Mobile):** Native support for Windows, Linux, macOS (Apple Silicon), and Android (ARM64 / POCO F5).
- **🎨 Modern UI (Bevy 0.19 `bsn!`):** Transpiles obsolete Flash (`.gfx`) menus into hardware-accelerated declarative Bevy UI node trees.
- **🎮 Integrated Mod Manager:** Built-in launcher with drag-and-drop archive installation and priority-weighted `.esp`/`.esl` plugin load orders.

---

## 🏗️ Workspace Crate Architecture

Wah Krah Jol is organized into a modular Cargo workspace:

```
wah-krah-jol/
├── Cargo.toml                  # Workspace Root Manifest
├── LEGAL.md                    # Legal & Trademark Disclaimers
├── crates/
│   ├── launcher/      # GUI Setup Wizard & Built-in Mod Manager
│   ├── converter/     # Asset Converter Pipeline (.nif ➔ .glb, .dds ➔ KTX2, .esm ➔ libSQL)
│   ├── shared/        # Versioned coordinate, database, and cell-cache contracts
│   ├── engine/        # Bevy Game Engine Binary (Render, Physics, Luau, Audio)
│   └── dummy-content/ # Synthetic mock asset generation for tests & CI
```

| Crate           | Responsibilities                                                                                                                |
| :-------------- | :------------------------------------------------------------------------------------------------------------------------------ |
| **`launcher`**  | First-run setup wizard, game path detection, built-in mod manager UI, triggers converter progress bar, and launches the engine. |
| **`converter`** | Heavy offline asset converter (`mesh-tools`, `basis-universal`, `ddsfile`, `nom` binary parsers).                               |
| **`shared`**    | Versioned contracts shared by conversion and runtime, including coordinates and the terrain cell cache.                      |
| **`engine`**    | Lightweight, hyper-fast game binary (Bevy 0.19+, `wgpu`, `libsql`, `mlua` Luau JIT).                                            |
| **`dummy-content`** | Procedural, synthetic asset fixtures for automated testing without proprietary game files.                                 |

---

## 🗺️ Project Roadmap

Wah Krah Jol is being built systematically across 5 core phases. Explore the full roadmap specs in [`docs/roadmap/`](docs/roadmap/README.md).

- [x] **[Phase 1: Asset Modernization Pipeline (`converter`)](docs/roadmap/01-asset-pipeline.md)** — Transpile legacy `.esm`, `.nif`, `.dds`, and `.pex` into `SQLite 3`, `glTF 2.0`, `KTX2`, and `Luau`.
- [ ] **[Phase 2: Core Engine Runtime & Vercidium Renderer (`engine`)](docs/roadmap/02-core-engine.md)** — Runtime, integration, profiling, and acceptance infrastructure implemented; complete real-asset sign-off remains pending.
- [ ] **[Phase 3: Luau Runtime, Declarative UI & Launcher (`launcher`)](docs/roadmap/03-luau-and-ui.md)** — Sandboxed Luau JIT with async web APIs, Flash-to-Bevy `bsn!` UI conversion, and setup wizard.
- [ ] **[Phase 4: Gameplay Mechanics, Physics & Persistence](docs/roadmap/04-gameplay-and-physics.md)** — Rapier 3D physics, animation blending, combat state machine, and sub-second save state snapshots.
- [ ] **[Phase 5: Hardware Ray-Tracing, Multiplatform & Networking](docs/roadmap/05-multiplatform-and-networking.md)** — WebGPU/Vulkan RTGI, DLSS/FSR3 frame gen, native co-op multiplayer, Android ARM64, and OpenXR VR.

---

## 🚀 Quick Start (Development Setup)

### Prerequisites

- **Rust** (2024 Edition)
- **CMake** & **Ninja** / **GCC** (for native libSQL / SQLite compilation)

### Building & Running

1. **Clone the repository:**

   ```bash
   git clone https://github.com/your-username/wah-krah-jol.git
   cd wah-krah-jol
   ```

2. **Check workspace compilation:**

   ```bash
   cargo check --workspace
   ```

3. **Run the Launcher App:**
   ```bash
   cargo run -p launcher
   ```

### Faster rebuilds with kache

This repo uses [kache](https://kunobi.ninja/docs/kache) in CI, and it also
speeds up local builds and git worktrees by sharing compiled dependencies:

```bash
cargo install kache
export RUSTC_WRAPPER=kache  # add to your shell profile to keep it
```

kache is opt-in locally: unset `RUSTC_WRAPPER` for a plain Cargo build.
Each worktree keeps its own `target/` directory; do not share one
`CARGO_TARGET_DIR` across worktrees.

---

## 📚 Technical Specifications (`docs/specs/`)

For detailed technical specifications, format breakdowns, and architectural guidelines, explore the [`docs/specs/`](docs/specs/README.md) directory:

- **[`architecture.md`](docs/specs/engine/architecture.md)** — Core engine architecture overview & phase milestones.
- **[`nif-to-gltf.md`](docs/specs/converters/nif-to-gltf.md)** — 3D mesh converter specification (`mesh-tools`).
- **[`dds-to-ktx2.md`](docs/specs/converters/dds-to-ktx2.md)** — Texture compressor specification (`basis-universal` + `ddsfile`).
- **[`esm-to-sqlite.md`](docs/specs/converters/esm-to-sqlite.md)** — Master database specification (`libSQL` + `rkyv`).
- **[`pex-to-lua.md`](docs/specs/converters/pex-to-lua.md)** — Papyrus bytecode to Luau transpilation spec (`mlua`).
- **[`mods-and-ui.md`](docs/specs/modding/mods-and-ui.md)** — Integrated Mod Manager & Flash-to-Bevy UI strategy.
- **[`launcher.md`](docs/specs/modding/launcher.md)** — First-run setup wizard & launcher workflow.
- **[`vercidium-optimizations.md`](docs/specs/engine/vercidium-optimizations.md)** — High-FPS GPU instancing & HZB culling.
- **[`platforms.md`](docs/specs/meta/platforms.md)** — Target matrix for Desktop, Mobile ARM64, and WebGPU.
- **[`requirements.md`](docs/specs/meta/requirements.md)** — Hardware system specs and optimization analysis.
- **[`features.md`](docs/specs/modding/features.md)** — Unlocked capabilities (Zero-loading screens, AI dialogue, native co-op).
- **[`bevy-examples.md`](docs/specs/engine/bevy-examples.md)** — Mapping official Bevy 0.19 patterns to engine subsystems.

---

## 🤝 Contributing

We welcome community contributions! Whether you're fixing bugs in asset converters, enhancing Bevy rendering pipelines, or writing documentation, check out our guidelines before submitting a PR:

- 📖 **[CONTRIBUTING.md](CONTRIBUTING.md)** — Guide on development workflow, code style (`cargo fmt`/`clippy`), and PR guidelines.
- 📜 **[LEGAL.md](LEGAL.md)** — Intellectual property policy and compatibility guidelines.
- 🐛 **[Issue Tracker](../../issues)** — Search existing issues or report a new bug using our template.

---

## ⚖️ Legal Disclaimer & License

### Licensing

Wah Krah Jol is dual-licensed under either of the following licenses at your option:

- **MIT License** ([`LICENSE-MIT`](LICENSE-MIT))
- **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE))

### Legal Notice & Trademark Disclaimer

Wah Krah Jol is an independent open-source game engine reimplementation. It does **not** contain or distribute any copyrighted game assets, artwork, 3D models, audio, or game data belonging to Bethesda Softworks LLC or ZeniMax Media Inc. Users must supply their own legally owned copy of _The Elder Scrolls V: Skyrim_ to extract game data.

All trademarks are the property of their respective owners and are used strictly under nominative fair use for compatibility description. Read the full statement in [LEGAL.md](LEGAL.md).
