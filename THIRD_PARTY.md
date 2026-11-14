# Third-party code

Mudcrab is licensed under the GNU General Public License v3 or later ([LICENSE](LICENSE)), except
for the files below, which are derived from other projects and keep those projects' terms. Each
derived file names its source in an SPDX header and a comment at the top. A bundled crate keeps
its own terms and license files; the ones that differ from Mudcrab's are noted below.

## Community Shaders

[Community Shaders](https://github.com/community-shaders/skyrim-community-shaders) re-implements
and extends Skyrim Special Edition's shaders in HLSL for Direct3D 11. Its code is licensed under
the GNU General Public License **version 3** (no "or later" grant), with a Modding Exception and a
GPL-3.0 Linking Exception (its `EXCEPTIONS.md`). Those exceptions cover linking against Skyrim
and SKSE, which Mudcrab does not do. As `EXCEPTIONS.md` allows, Mudcrab's derived files drop
them and are licensed under plain **GPL-3.0-only**.

Because these files are GPL-3.0-only, a Mudcrab build that includes them can be distributed
under GPL-3.0 only, not under a later version.

Community Shaders' base shaders (`package/Shaders/{DistantTree,Effect,Lighting,Particle,RunGrass,Sky,Water}.hlsl`)
started from Ilya Perapechka's shader code, which is MIT-licensed; only the Community Shaders
modifications are GPL-3.0. A file ported from one of them is `MIT AND GPL-3.0-only` and carries
the MIT notice below in its header.

| Mudcrab file | Derived from (Community Shaders path) | License |
|---|---|---|
| `crates/engine/src/shaders/cs_lighting_msn.wgsl` | `package/Shaders/Lighting.hlsl` (model-space normal decode, world normal, `_s` glossiness, `GetFacegenBaseColor`, `GetFacegenRGBTintBaseColor`, the `HAIR` tint vertex colour) | MIT AND GPL-3.0-only |
| `crates/engine/src/shaders/height_fog.wgsl` | `features/Exponential Height Fog/Shaders/ExponentialHeightFog/ExponentialHeightFog.hlsli`, `features/Exponential Height Fog/Shaders/ExponentialHeightFog/VolumetricFogCommon.hlsli`, `package/Shaders/ISSAOComposite.hlsl` | GPL-3.0-only |

## Bevy

The skin material shader also derives its mesh and PBR pipeline from Bevy 0.19.0. Bevy's pinned
`bevy_pbr` crate declares `MIT OR Apache-2.0`; Mudcrab keeps that SPDX expression on the derived
shader.

| Mudcrab file | Derived from (Bevy 0.19.0 path) | License |
|---|---|---|
| `crates/engine/src/shaders/skyrim_skin.wgsl` | `bevy_pbr/render/mesh.wgsl` and `bevy_pbr/render/pbr.wgsl` | MIT OR Apache-2.0 |

`vendor/bevy_image` is the published `bevy_image` 0.19.0 crate with one manifest change: its
optional `zstd` requirement moves from `0.13.3` to `0.14.3` for four RustSec advisories. The Rust
source is unchanged and keeps `MIT OR Apache-2.0`; both upstream license files are included. See
the [patch notes](vendor/bevy_image/README.mudcrab.md) for the source hash, the exact change and
the removal criteria.

`vendor/project-wormhole-nif` is a Mudcrab fork of the published `project-wormhole-nif` 0.1.0
crate (`GPL-3.0-or-later`, the same licence as Mudcrab). Unlike `vendor/bevy_image` it carries
Mudcrab's own parser changes. See its [fork notes](vendor/project-wormhole-nif/README.mudcrab.md)
for the source hash, what changed and how to compare it with the published crate.

The MIT notice for the original shader code (Community Shaders,
`package/SKSE/Plugins/CommunityShaders/Licenses/LICENSE-Perapechka-MIT.txt`):

```
MIT License

Copyright (c) 2022 Ilya Perapechka

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Code that Community Shaders itself took from elsewhere under other licences (see its
`package/SKSE/Plugins/CommunityShaders/Licenses/THIRD_PARTY.md`) is not used here. Mudcrab does
not use the Community Shaders logo or name it as an endorsement.

### Adding a file derived from Community Shaders

1. Start the file with `// SPDX-License-Identifier: GPL-3.0-only` (`MIT AND GPL-3.0-only` for a
   port of the base shaders listed above, with the MIT notice), then name the Community
   Shaders source files it ports, the copyright line
   `Copyright (C) the Community Shaders contributors`, and what Mudcrab changed.
2. Add a row to the table above.
3. Check the source file's own header and the Community Shaders `THIRD_PARTY.md` first: parts
   under MIT or Apache-2.0 keep that licence and its notice, and need their own row.
4. Do not port from `features/Upscaling` (NVIDIA Streamline and AMD FidelityFX have their own
   licences), `features/RenderDoc`, or the Community Shaders logo.
