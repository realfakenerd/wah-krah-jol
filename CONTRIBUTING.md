# Contributing to Mudcrab

Thank you for your interest in contributing to **Mudcrab**! We welcome contributions from developers, reverse engineers, 3D graphics enthusiasts, modders, and documentation writers of all experience levels.

---

## 📜 Code of Conduct

Please treat all community members with respect, patience, and empathy. We are all building a modern open-source engine reimplementation together.

---

## 🛠️ How Can You Help?

Here are some areas where you can make an immediate impact:

1. **Asset Converters (`crates/converter`)**
   - `.nif` parser improvements and glTF 2.0 export optimization (`mesh-tools`).
   - `.dds` to KTX2 / Basis Universal texture compression pipeline (`ddsfile`, `basis-universal`).
   - `.esm` binary parsing into SQLite database structures (`nom`, `rusqlite`, `rkyv`).
   - Papyrus (`.pex`) decompilation and transpilation to Luau (`mlua`).

2. **Engine Subsystems (`crates/engine`)**
   - Bevy 0.19+ rendering features (Vercidium instancing, HZB occlusion culling, custom mesh pipelines).
   - Physics integration (rigid bodies, collision meshes, character controllers).
   - Audio spatialization and music state machines.

3. **Launcher & UI (`crates/launcher`)**
   - Bevy UI setup wizard and path detection.
   - Built-in mod manager & load order drag-and-drop workflow.

4. **Documentation & Benchmarks**
   - Refining specs in the [`docs/specs/`](docs/specs/) directory.
   - Writing usage guides, benchmark tests, or API documentation.

---

## 🚀 Getting Started

### 1. Prerequisites
Ensure you have the following installed:
* **Rust** (2024 Edition)
* **Git**
* **A C and C++ compiler** for the bundled SQLite, Luau and Basis Universal sources: Visual Studio
  Build Tools on Windows, Xcode Command Line Tools on macOS, GCC or Clang on Linux

### 2. Fork and Clone
Fork [Mudcrab-Team/mudcrab](https://github.com/Mudcrab-Team/mudcrab) on GitHub, then clone
your fork:
```bash
git clone https://github.com/<your-username>/mudcrab.git
cd mudcrab
```

### 3. Check Workspace Compilation
```bash
cargo check --workspace
```

When Cargo runs inside this workspace, desktop Windows x64 MSVC builds select SQLite's private
heap through `CFLAGS_x86_64_pc_windows_msvc`. This keeps database allocation
separate from decoded records without changing SQL or transaction safety. Other
targets keep their existing allocator. These SQLite-prefixed defines also reach
other C dependencies built for that Windows target. If you supply your own
target-specific CFLAGS, retain `-DSQLITE_WIN32_MALLOC=1 -DSQLITE_WIN32_HEAP_CREATE=1`;
Cargo preserves environment overrides, including
the higher-priority hyphenated target spelling understood by `cc`.

### 4. No Game Installed? Generate Synthetic Assets
```bash
cargo run -p dummy-content -- gen Data
cargo run -p converter --bin converter -- Data modern_assets
```

This produces a deterministic, procedurally built `Data` directory (DDS, PEX, BSA, BA2) with no
copyrighted content, then converts it with the real pipeline. See
[`docs/specs/converters/dummy-content.md`](docs/specs/converters/dummy-content.md) for the CLI,
format coverage and library API.

To look at the generated world in the engine, point it at the converted folder and at the fixture's
worldspace (form id `1`; the engine's default worldspace is Skyrim's Tamriel, which the fixture does
not contain):
```bash
cargo run -p engine --bin engine -- --assets modern_assets --worldspace 1
```

How long a first build and a real conversion take, and how much disk they need, is in
[`docs/specs/meta/requirements.md`](docs/specs/meta/requirements.md).

### 5. Fast Optimised Builds
The engine is slow in a debug build and `--release` spends most of a rebuild on link-time
optimisation. For trying a change, use the `quick` profile (release without LTO, incremental):
```bash
cargo build --profile quick --bin engine
```
The binary lands in `target/quick/`. Use `--release` for anything you measure (benchmarks,
acceptance runs), since `quick` frame times are not comparable.

To see which system or render pass makes a frame slow, see the opt-in Tracy build in
[`docs/contributing/native-profiling.md`](docs/contributing/native-profiling.md).

## 🛠️ Development Workflow

The full path from idea to merge (checking for overlap, claiming an issue, drafts, review, merging) is in [`docs/contributing/workflow.md`](docs/contributing/workflow.md). The short version:

1. **Find or Create an Issue, and Claim It:**
   Search open issues *and* pull requests, including who is assigned, so no one else is working on the same thing. Assign yourself, or, if GitHub doesn't let you, comment "I'm taking this": the comment counts as your claim.
2. **Create a Feature Branch:**
   ```bash
   git checkout -b feature/nif-skinning-support
   ```
3. **Write Clean, Idiomatic Code:**
   Ensure all functions have doc comments (`///`) and pass clippy checks.
4. **Run What CI Runs:**
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo nextest run --workspace --all-targets --no-fail-fast --no-tests=warn
   ```
   CI uses [`cargo-nextest`](https://nexte.st) (`cargo install cargo-nextest --locked`); plain
   `cargo test --workspace --all-targets` runs the same test targets (neither runs doctests; add
   `cargo test --workspace --doc` for those). CI also runs `cargo audit` and a release-mode
   performance check; see `.github/workflows/run_tests.yml` for the exact commands.
5. **Submit a Pull Request:**
   - Target the `main` branch and fill in the PR template; [`docs/contributing/writing-prs.md`](docs/contributing/writing-prs.md) shows how to write it so it is quick to review.
   - Link the issue (`Closes #N`), apply the [labels](docs/contributing/labels.md) that fit (if a label there doesn't exist on the repository yet, use the closest one that does; without triage access, name them in the PR instead), and reference any relevant planning docs in `docs/specs/`.
   - Used an AI assistant? Follow [`docs/AI_POLICY.md`](docs/AI_POLICY.md): disclose it in one line, and own every line you submit.

---

## ⚖️ License & Legal

By contributing to Mudcrab, you agree that your contributions will be licensed under the **GNU General Public License v3 or later** (`GPL-3.0-or-later`).

### Legal Disclaimer
Mudcrab is a clean-room engine reimplementation. **Do NOT upload, distribute, or submit copyrighted game assets** (`.bsa`, `.esm`, `.nif`, `.dds`, etc.) owned by Bethesda Softworks / ZeniMax Media in PRs or issues. All test fixtures must be generated procedurally or extracted dynamically at runtime from the user's legally owned game files.
