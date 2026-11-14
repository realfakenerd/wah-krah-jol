# Build and Conversion Requirements

What a contributor's machine spends on Mudcrab, measured on a Windows desktop with NVMe drives and
Skyrim Special Edition with its free Creation Club content. Treat the times as a guide: they depend on the
CPU, the disk and what else is running.

## Building

| Step | Time | Notes |
| :--- | :--- | :--- |
| `cargo check --workspace`, first time | about 5 min | compiles Bevy and every dependency once |
| `cargo build --release --bin engine`, first time | longer than a check | optimised code generation plus link-time optimisation |
| a one-file engine change, `--release` | about 4-5 min | mostly link-time optimisation |
| a one-file engine change, `--profile quick` | about 40 s | release without link-time optimisation (`cargo build --profile quick --bin engine`); not for measuring |
| the build folder (`target/`) | 20-30 GB | debug and release profiles together |

## Converting a Skyrim SE install

| Item | Size or time | Notes |
| :--- | :--- | :--- |
| the game's `Data` folder | about 15 GB | read only; the converter never writes into it |
| a first (fresh) conversion | not re-measured | the desktop default preserves native BC1–BC7 blocks, so textures take minutes; the 5.1 h figure was a full UASTC run |
| a reconversion with unchanged inputs | about 30 minutes | converted assets are reused through `conversion-manifest.json` |
| the converted output | not re-measured | only runtime artifacts are published (textures, meshes, scripts, databases, manifest); `vfs/` and `.ingestion-cache/` stay in staging; the persistent cache is `<output>.assets-cache`, hard-linked where the filesystem allows |
| free space during a reconversion | about the size of the output again, plus the cache | the runtime pack is linked from staging and published over the old output at the end; staging is removed after a fresh run publishes |

Without Skyrim, the `dummy-content` fixture (see [CONTRIBUTING](../../../CONTRIBUTING.md)) builds and
converts a small synthetic game in a couple of minutes.

## Running the engine

A desktop GPU needs Vulkan, DirectX 12 or Metal support and the BC formats used by its converted
textures. KTX2 output includes preserved native BC, newly encoded BC7 and UASTC fallback. See the
[desktop texture contract and proposed portable-output deferral](../../adr/0014-phase1-desktop-texture-contract.md).
