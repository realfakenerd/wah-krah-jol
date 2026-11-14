# ADR-0014: Proposed Phase 1 desktop texture contract

- **Status:** Proposed for repository adoption; Route B scope approved by a maintainer
- **Date:** 2026-10-31

## Context

[Phase 1 §1.4](../roadmap/01-asset-pipeline.md#14-texture-compressor-dds_to_ktx2)
promised desktop BC7, mobile ASTC and web ETC2 runtime transcoding. The current
converter emits a mixture of native GPU formats and UASTC, with no selectable
portable texture profile. [#274](https://github.com/Mudcrab-Team/mudcrab/issues/274)
tracks the decision under the parent [#162](https://github.com/Mudcrab-Team/mudcrab/issues/162).

## Scope decision

Taylor approved Route B during local review on 2026-10-31. A maintainer approved
Route B in [#162](https://github.com/Mudcrab-Team/mudcrab/issues/162#issuecomment-6694524381)
on 2026-11-06.

Route B selects the **desktop texture contract** below as Phase 1's texture scope
and defers the **portable texture profile**, including demonstrated mobile ASTC
and web ETC2 loading, to the multiplatform work. The portable profile is required
before mobile/browser delivery. This proposal changes no runtime behavior and
does not establish Phase 1 completion. Scope approval is separate from this ADR's
repository adoption and merge, which remain pending.

### Current desktop texture contract

| Path | Current output and limits |
| --- | --- |
| Native DDS preservation | Compatible BC1/BC2/BC3/BC7 and DXGI RGBA8 copy their authored mip payloads. BC4/BC5/BC6H and DXGI R8 copy only for linear slots; sRGB uses fallback. |
| CPU packed RGB(A) | Eligible single-layer 2D, whole-byte RGB(A) layouts encode to lossy native BC7, including normal/data slots. Native RGBA8 preservation takes precedence. |
| CPU fallback | Other decodable sources decode to RGBA8 and encode to UASTC. This does not promise HDR/signed-data precision preservation. Unsupported inputs fail. |
| GPU DDS encoding | Admitted inputs encode to UASTC. Native-preserved formats bypass the encoder; volumes use CPU. GPU failure returns to CPU routing, which can emit BC7. |
| Generated terrain LOD atlases | UASTC from generated RGBA mips through the separate LOD CPU/GPU policy, with CPU fallback. GLBs reference KTX2 through `KHR_texture_basisu` and the ordinary image source; no PNG/JPEG fallback ([ADR-0010](0010-lod-chunk-payload-format.md)). |
| Generated merged-terrain arrays | Preserve admitted BC classes or transcode/re-encode to BC7, generating missing mips as needed. DDS array rejection does not prohibit these generated arrays. |

DDS conversion supports 2D textures, one complete six-face cubemap and supported
volumes; it rejects arrays and cube arrays. Authored DDS mip levels and topology
are retained on successful conversion. Alpha passes through lossy encoding where
applicable. Slot semantics take precedence in this order: normal, data,
color/emissive, then unclassified linear data. Conflicting uses resolve by that
precedence, not by filenames or blanket rejection.

Packs containing native BC require a GPU supporting those BC formats. KTX2 is a
container, not a portability guarantee. `--texture-encoder gpu` does not select
a portable profile; serialized `texture_etc1s_quality` does not select ETC1S.
Bevy's UASTC loader has capability-dependent ASTC, BC7, ETC2 and RGBA branches;
those branches do not transcode native BC payloads into mobile formats.

### Evidence and limits

Source review at `64108a1c38b0a854d4f82d7031ea50b57c336a5a`:
[DDS routing and round-trip fixtures](../../crates/converter/src/texture.rs),
[GPU admission](../../crates/converter/src/texture_gpu/mod.rs),
[CPU fallback](../../crates/converter/src/pipeline.rs),
[configuration](../../crates/converter/src/config.rs),
[LOD encoding](../../crates/converter/src/lod/albedo.rs),
[terrain arrays](../../crates/converter/src/terrain_arrays.rs), and
[Bevy transcode selection](../../vendor/bevy_image/src/ktx2.rs).

Existing fixture code checks native mip bytes, UASTC round trips through BC7,
and [Bevy BC3 loading with/without Zstd](../../crates/engine/tests/native_ktx2_loads_in_bevy.rs).
These loader tests do not create a GPU device or render. No tests, conversion,
engine or browser were run for this documentation proposal. Source inspection
does not demonstrate ASTC/ETC2 target loading or a portable pack.

[#222](https://github.com/Mudcrab-Team/mudcrab/pull/222) is merged: its mip-layout
and producer-28 migration fixes are present. Its reported validation retains
its own source identity; independent container conformance and target loading
need separate evidence. #222 does not select a portable profile.

## Consequences and tracking

Parent decision: open #274, linked from #162. Route B scope was approved by a
maintainer on 2026-11-06. Implementation and target validation are tracked in the
open follow-up [#548](https://github.com/Mudcrab-Team/mudcrab/issues/548), which is
not a v0.1.0 blocker. Keep #274 open until this ADR is accepted and merged.
The follow-up must retain:

- An output profile independent of encoder selection across DDS, CPU fallback,
  generated LOD atlases and merged-terrain arrays; explicit topology and
  HDR/normal/data/alpha/mip policies, including unsupported combinations.
- Profile-aware cache identities, producer/manifests, CLI/launcher defaults,
  resume, repair, metadata recovery and standalone `texture-sync` /
  `asset-closure-textures` reuse.
- Separate evidence for independent KTX2 conformance, representative BC7/ASTC/ETC2
  transcoding with restricted capabilities, and real native/mobile/browser
  loading. Desktop loading or UASTC encoding alone is insufficient.
- A declared glTF image/extension compatibility policy and updated platform
  claims tied to the validated profile.
