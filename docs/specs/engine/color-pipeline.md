# L1 material and color pipeline

Tracks [L1 #131](https://github.com/Mudcrab-Team/mudcrab/issues/131), under [vanilla lighting parity #129](https://github.com/Mudcrab-Team/mudcrab/issues/129). L0 reference capture and L1 implementation can proceed concurrently. L0 evidence gates retail parity acceptance, not synthetic tests or implementation.

## Scope and evidence

Default target: unmodded Skyrim SE. Interior and exterior cases have equal priority. Mod research informs native behavior; addon lighting remains out of scope. L1 fixes output-domain inconsistencies and NIF surface inputs, with controlled probes. They do not establish Skyrim's image-space equations or select its final exposure/tone curve.

Code baseline: `a9f2310ccfc691eebb97fde18df1e8d334b7d744`; Bevy 0.19. The source trace below describes actual runtime behavior. Claims in older sky notes about encoded weather interpolation and fog equations still require the L0/L2 retail evidence; this change preserves those inputs.

## Pipeline trace and ownership

| Stage / owner | Input → output | Current limit / next owner |
|---|---|---|
| `converter/src/texture.rs`, `material.rs` | DDS channels → semantic KTX2 transfer format; diffuse/glow use sRGB aliases, normals/data use linear views | Shared source bytes may have distinct views; preserve alpha as data. Existing round-trip tests cover encoded channels. |
| `converter/src/material.rs` | Validated per-shape NIF values → glTF factors, extensions, source extras | #81: specular enable, normal-alpha mask and gloss exponent approximation. #82: static glow-slot eligibility and emission energy corrected; animated emission remains unsupported. #83: alpha/UV. #84: effect/editor surfaces. Remaining issues stay open; emission animation is outside #82. |
| Bevy glTF / `StandardMaterial` | sRGB textures decoded once; material factors and data textures remain linear → material response | Bevy's PBR BRDF is an approximation, not recovered Skyrim shading. Tangent/model-space normals require distinct treatment. |
| `shaders/terrain.wgsl` | Linear layer samples and normal data → weighted material response → PBR lighting | Authored normal conventions, layer semantics and specular response remain separate material probes. |
| Bevy `pbr_functions.wgsl` | Lights + material → exposure-scaled linear RGB | `Exposure.ev100` uses Bevy's `2^-EV100 / 1.2`. Current 9.7 is pinned from the prior default, not a Skyrim value. Emission's exposure weight is material-owned; default emission is exposure-independent. |
| `sky.rs`, `shaders/sky.wgsl` | Encoded weather-row mixing → one sRGB decode → linear palette brightness | Existing palette is a fixed clear-day approximation. Sky/fog palette values already occupy the composition domain; do not multiply them by camera exposure a second time. L2 owns authored state and its units. |
| `DistanceFog` | Exposed surface RGB + linear fog palette → fogged linear RGB | Existing Bevy visibility-based atmospheric fog toward Fog Far stays explicit. Interior fog removal is current behavior, not complete Skyrim interior semantics. L2 owns the correction. |
| `render.rs` reflection camera | Scene at main-view exposure → `Rgba16Float`, tone mapping disabled | Floating linear target preserves values above 1. Reflection gate copies exposure even while inactive. Existing reflected layers and geometry selection remain unchanged. |
| `shaders/water.wgsl` | Exposed water lighting + exposed linear reflection → blend → surface fog | Reflection receives neither a second lighting evaluation nor a display transform. Existing Fresnel/waves and reflection coverage are approximations. |
| `color_pipeline.rs` / scene cameras | HDR composition → selected Bevy transform (Tony by default) or explicit retail curve → selected post-tonemap grade → display encoding | Runtime starts with Cool Gray sky and the Sleeping Giant sRGB snapshot described below. The settings menu can select a Bevy transform on the Tony path. Explicit retail-curve selection owns camera tonemapping and stays active across unrelated settings changes. The saved `full` grade uses Bevy linear `post_saturation`, followed by sRGB brightness/contrast/tint from reported `ISSkyrimClearDAY` (0x00012F88, `Update.esm` winner) CNAM 1.5/1.135/1.4 and TNAM 0.42 toward 0.894/0.839/0.773. Contrast uses an artistic pivot of 0.18. EV100 remains a pinned Bevy baseline unless changed in the interactive settings. `HNAM` stays unapplied on the Tony path. The separate retail-curve experiment below uses White only. L4 owns adaptation and effective native state. |

The previous non-HDR mesh path tone-mapped in each material shader. The custom sky shader bypassed that operation, and reflection RGB passed through tone mapping in both the reflection and water passes. HDR composition moves the tone transform after composition. The experimental cinematic grade adds a fullscreen shader pass. It changes images and consumes more render-target memory; target-hardware performance remains an acceptance requirement.

## Experimental cinematic grade limits

The sRGB-domain brightness/contrast/tint pass preserves the Riverwood appearance
approved during local testing. It is an artistic interpretation, not verified
Skyrim shader parity. Native CPU curves, parameter packing and the final exponent
producer are now traced, but this grade still lacks native adaptation, its sampled
pivot, bloom and complete output transfer. One captured Skyrim frame now identifies
group 26 and its inputs, with a five-pixel arithmetic check against GPU replay;
this state is not yet reproduced in Mudcrab, and full retail pass order remains
unverified. See [captured GPU state](../../../research_notes/Skyrim%20native%20tone%20mapping/gpu_state.md)
and the [native packing trace](../../../research_notes/Skyrim%20native%20tone%20mapping/gpu_binding_trace.md).
Bevy saturation
runs in linear RGB with slightly different luminance weights from the custom tint
pass. Negative input and out-of-gamut output are clipped; alpha is preserved and
retail alpha/bloom-feed grading is omitted. `HNAM` is recorded but unapplied with
the default Tony transform. The explicit retail-curve experiment below uses
White only.

Scene cameras share the selected grade, including interiors. The experimental
record mode can load a direct image space or an explicitly selected authored
weather slot; it does not resolve effective runtime weather or interior state. All apps using
these camera settings must register `SkyrimCinematicPostPlugin`. Cameras must keep
`CompositingSpace::Linear`; overriding it with sRGB or Oklab is unsupported. Ordering
relative to AA and CAS remains unvalidated. Reflection currently incurs a disabled
pass-through copy. The color composition probe's `--legacy-output` path disables
both grades to preserve its original ungraded baseline. Probe reports identify the
grade and domains; material probes now register the full post pass.

## A/B controls and matched captures

### Sleeping Giant Inn snapshot experiment

`SceneGrade snapshot-srgb` and `SceneGrade snapshot-linear` start from the rounded
Photo Mode UI values supplied on 2026-10-13: saturation 1.66, brightness 1.15,
contrast 1.50, tint amount 0.56, and tint RGB `(0.88, 0.81, 0.72)`. Photo Mode
lists the tint sliders as Red, Blue, Green; the experiment stores RGB in normal order.
These are one scene's current base-grade values, not universal Skyrim defaults.

The sRGB mode now applies a user-requested 20% brightness increase, from 1.15 to
1.38. Saturation, contrast, tint, exposure, and fog remain unchanged by this tuning.
The original snapshot values and the linear mode retain brightness 1.15.
The approved look is named `approved-sleeping-giant` and stored in
`crates/engine/src/presets/approved-snapshot.json`: original rounded snapshot,
final grading values, sky colors, and fog overrides are separate data fields.
`--grade-preset <json>` loads the same format at startup. Its grade applies only
to `snapshot-srgb`; its sky/fog applies only to Cool Gray. It does not replace
loaded Skyrim records, Vivid, or `full` mode. Brightness 1.38 is final and is not
multiplied by 1.2 again.

Both modes disable Bevy post-saturation and evaluate saturation followed by tint
in the cinematic post pass. The original luminance feeds both operations; negative
channels produced by saturation are retained until tint, brightness, and contrast
have run, then the output is clipped. `snapshot-srgb` operates on sRGB-encoded
post-tonemap values; `snapshot-linear` operates directly on linear post-tonemap
values. Alpha passes through and reflections remain ungraded.

The modes keep the selected sky/fog preset, exposure, TonyMcMapface, and the fixed 0.18
contrast pivot. They do not reproduce Skyrim's native tone curve, adaptation,
bloom, final gamma, or modifier composition. The UI values are rounded, and the
correct native sampled domain remains a capture gate. These are controlled domain
experiments rather than a full retail renderer port.

Use `--color-grade snapshot-srgb|snapshot-linear` at startup. Return to
`SceneGrade full` to restore Vivid or Cool Gray's existing grading. `ScenePreset`
remains an independent sky/preset selector. Both saved presets retain their existing
values. The engine starts with the Cool Gray sky and `snapshot-srgb` grading. This
uses the snapshot's saturation 1.66; Cool Gray's saved saturation 1.25 applies when
`SceneGrade full` is selected. Restore the original Vivid look with
`--visual-preset vivid --color-grade full`, or run `ScenePreset vivid` followed by
`SceneGrade full` in the console. Shot receipts include the resolved grading values,
which saturation stage is active, and the post-pass domain flag. For a small GPU
composition check, run `color_pipeline_probe --gray --color-grade snapshot-srgb`
or `snapshot-linear` with a fresh `--output` directory.

### Authored image-space record experiment

`--color-grade record-srgb --image-space 0x12F88` reads the winning IMGS record
from the converted `skyrim_world.db`. Alternatively select a weather's authored
slot with `--image-space-weather 0x81A --image-space-slot day`; available slots
are `sunrise`, `day`, `sunset`, and `night`. FormIDs accept decimal or `0x` hex.
Direct and weather selectors cannot be combined, and the weather/slot flags must
be supplied together. No selector is guessed from the comparison screenshot.
Older archives may retain plugin-relative IMSP links without normalization
metadata. Weather lookup therefore accepts only verified identity cases:
slot-zero base-plugin forms with matching origin metadata and weather winner
priority 0 or 1. Later overrides and mod-owned weather links fail clearly; use a
direct canonical IMGS selection for those archives.

This mode sends raw CNAM saturation/brightness/contrast and TNAM tint into the
existing sRGB post pass without the approved snapshot overrides. With the default
tone-map selection it keeps TonyMcMapface, fixed EV100 9.7, the artistic 0.18 pivot, current lighting and the
independently selected sky/fog. It is a calibration experiment, not Skyrim's
native tone mapper. All nine HNAM values are retained in the loaded source and
receipts. With Tony selected they remain explicitly unapplied; adaptation, native bloom, native tone
curve, final transfer, and modifier composition are unresolved.

Loading a record without choosing `record-srgb` is supported for console A/B:
`SceneGrade record-srgb`, then `SceneGrade snapshot-srgb` restores the approved
grade. Switching to record mode without a loaded record fails and keeps the
previous mode. To restore the complete approved look after curve experiments,
select `SceneToneMap tony` and `SceneGrade snapshot-srgb`. Missing/malformed selected records fail startup rather than
substituting a preset. Fixtures and synthetic runs do not require retail data and
reject record selectors instead of opening a world database.

`SceneState` and automatic `shots.log` receipts include source selection, IMGS
and optional WTHR identity, winning plugin/load order, raw CNAM/TNAM/HNAM values,
approved override provenance, effective grade, and `hnam_applied: false` with
Tony selected. Retail-curve receipts identify White as the only applied HNAM field.
An authored weather slot is evidence of record data, not the reference frame's
effective weather, blended modifiers, or GPU constants. The composition probe
has no world database and rejects `record-srgb` explicitly.

Local verification on 2026-10-15 used native Metal captures of the approved
default, direct ClearDAY IMGS, and Cloudy weather's authored day slot. Source,
grade, camera, readiness, and raw-HNAM receipts are in the ignored
`data/visual-comparisons/record-loader-20261015/` directory. The default's
resolved grading values match the approved capture exactly. PNG identity is
reported separately: repeated captures with the unchanged executable also vary,
so these results do not establish bitwise image reproducibility. The cause of
that capture variation remains unverified.

### Explicit retail-curve experiment

`--tone-map retail-curve --tone-map-params <json> --image-space <FormID>`
selects a separate scalar-luminance curve in the existing fullscreen pass.
The approved default remains `--tone-map tony`. Grading stays independently
selected; this experiment supports `snapshot-srgb`, `record-srgb`, and `off`.
Legacy `full`/`saturation` and `snapshot-linear` are rejected for this path.

The parameter file requires `name`, `source`, `curve` (`reinhard` or `filmic`),
`reinhard_white_scale`, `filmic_white_scale`, `sampled_luminance`, and
`target_luminance`. Every numeric value must be finite and positive. It has no
implicit defaults. HNAM.White comes from the selected record, and the recovered
CPU formulas derive its curve coefficient. Native static PE initializers show
white scales 1.1/1.4 and filmic disabled. A later isolated Skyrim session also
observed those values in process memory and console evidence. The parameter file
still selects them explicitly; it does not inherit a running Skyrim session or
establish the settings of the historical reference screenshot. See
[the CPU evidence](../../../research_notes/Skyrim%20native%20tone%20mapping/next_step.md)
and [runtime observations](../../../research_notes/Skyrim%20native%20tone%20mapping/runtime_state.md).

The scene camera disables Bevy tonemapping so the scalar curve receives exposed
HDR exactly once. Bevy's tone-stage color grading and deband dither are skipped.
The existing sRGB grade, artistic contrast pivot, and display transfer remain.
`SceneGrade off` disables grading while retaining the selected curve. Reflections
keep their ungraded HDR output and never receive the curve independently.

Only HNAM.White contributes to this experiment. Other HDR fields stay unapplied.
Fixed sample/target luminance values stand in for P.x/P.y without adaptation
history; they do not change the grade pivot. Native parameter packing is now
traced, and the audited final exponent is `1 / fGamma`; this experiment does not
apply that exponent. Bloom, adaptation/history, effective modifiers, the sampled
numeric domain, and complete output transfer remain outside this reconstruction.
The historical reference's selected shader and inputs remain unobserved. This
path is a retail curve with Bevy inputs and sRGB grading, not a complete Skyrim
tone mapper. See [packing](../../../research_notes/Skyrim%20native%20tone%20mapping/gpu_binding_trace.md)
and [output evidence](../../../research_notes/Skyrim%20native%20tone%20mapping/output_transfer_trace.md).

Preload the parameter file and record while keeping Tony selected, then use
`SceneToneMap retail-curve` and `SceneToneMap tony` for console A/B. Invalid
selections leave the previous state intact. Automatic and live screenshot
receipts bind tone selection, coefficients, record source, and actual camera
tonemapping/exposure to the captured render frame.

Every tuning step uses the frozen approved and Skyrim references through
[the comparison workflow](tonemap-tuning.md). Original PNGs and receipts remain
separate from visual judgement. Screenshot byte identity is not a fidelity gate.

### Presets and diagnostic stages

For a visible tuning session, `--start-shot <file>` seeds the world camera from a
validated file containing one exterior pose. It sets the streaming cell, worldspace,
rotation and horizontal FOV without selecting the automated shots runner. Add
`--stdin-console` to control that process through its stdin using the same console
commands as the backtick UI. This bridge is opt-in and has no network listener.
`SceneState` reports grading and streaming/upload readiness. `SceneShot <png-path>`
captures the visible window only after readiness checks pass and writes a JSON
state receipt alongside the image. It refuses existing output paths. The receipt
records grading state at request and binds camera/readiness evidence to the captured
render frame; use `--shots` for strict automated pose and grading evidence. Existing interactive mouse and movement controls
remain available unless an automated option such as `--acceptance-screenshot`
selects a stationary camera session.

`--no-render-diagnostics` disables Bevy's GPU timing and pipeline-statistics queries
for troubleshooting Metal counter-buffer allocation failures. It preserves the
scene settings, CPU frame timing, streaming gauges, and capture-readiness checks.
GPU timing evidence is unavailable in runs using this option.

Two named visual presets preserve the approved look while allowing experiments:

| Preset | Bevy post-saturation in `full` mode | Sky upper / lower / horizon (sRGB bytes) |
|---|---|---|
| Vivid | 1.5 | `(21,77,117)` / `(60,135,183)` / `(125,163,183)` |
| Cool Gray | 1.25 | `(178,187,198)` / `(194,202,209)` / `(207,214,220)` |

Vivid retains the existing clear-day configuration exactly. Cool Gray uses a light,
slightly blue gray gradient and lower saturation. Both retain brightness 1.135,
contrast 1.4, tint amount 0.42 and its existing RGB, EV100 9.7, and TonyMcMapface.
Cool Gray uses one quarter of the original fog density, equivalent to visibility
increasing from 48,300 to 193,200 Creation units. Its Fog Far color is a subdued
snowy blue-white: `(210,222,235)` instead of `(116,168,203)` in sRGB bytes. Vivid retains the original
fog color and density. Lights, shadows and camera pose remain unchanged. Sky
colors are authored inputs before tone mapping and grading, not final display RGB.

Run `ScenePreset vivid`, `ScenePreset cool-gray`, or `ScenePreset toggle` in the
backtick console. With no argument, `ScenePreset` reports the active preset and
diagnostic grade mode. Switching is immediate and reversible. Use
`--visual-preset vivid|cool-gray` at startup; Cool Gray is the runtime default. Preset
selection does not change the independent `SceneGrade` mode below. Use full
grading to see each complete look. Presets are built into the engine; console
selection lasts for the current process. Shot receipts record `visual_preset`
and `preset_saturation` alongside the diagnostic grade mode.

Open the backtick console and run `SceneGrade off`, `SceneGrade saturation` or
`SceneGrade full`. With no argument, `SceneGrade` reports the current mode. `off`
disables both grading stages; `saturation` enables only Bevy post-saturation;
`full` enables the experimental sRGB brightness/contrast/tint pass too. Exposure,
tone mapping, camera pose, lighting and fog remain fixed. Reflections stay ungraded,
and newly spawned scene cameras inherit the current mode. The runtime default is
`snapshot-srgb` with Cool Gray.

Use `--color-grade off|saturation|full|snapshot-srgb|snapshot-linear|record-srgb`
for startup selection. The settings menu persists these six modes. Its optional
field mask distinguishes user tuning from the displayed resolved base: unedited
fields keep the active approved or loaded record value, and one edited field
overrides only that field. Source evidence records the base identity, raw record
and provenance, applied override mask/values, and effective grade. Camera
application and capture receipts use the same central resolver. For a repeatable
three-mode capture, run:

```bash
python3 scripts/capture_color_grade.py \
  --engine target/debug/engine \
  --assets /absolute/path/to/converted/assets \
  --output /absolute/path/to/new/capture-directory
```

The script uses `crates/engine/tests/fixtures/color-grade-riverwood-shots.json`:
three exact 1920×1080 Riverwood poses covering facades, roofs, shaded surfaces and
distant geometry. It runs each mode sequentially with the same assets, lighting,
fog, stream radius and poses. `--shots` pins water animation to 1.0 seconds; normal
interactive runs and matched movement routes keep animated water. Capture receipts
record grade selection and fixed water time. The script rejects failed runs,
timeout captures, missing receipts and inconsistent poses before accepting a set.
PNG files and logs stay outside the repository; this is Mudcrab A/B evidence,
not matched retail Skyrim or a performance benchmark.

## Invariants

- V1: Production scene cameras and visual fixtures use explicit `SceneColorPipeline`: HDR, EV100 9.7, TonyMcMapface, linear compositing, Bevy post-saturation and the registered cinematic post plugin. Reflection disables both grades. These settings are experimental; defaults are not evidence of vanilla parity.
- V2: Reflection storage preserves linear values above 1; no tone map or sRGB target view before water sampling. Reflection exposure matches the main camera before rendering, including after exposure changes while water is invisible. Missing reflection exposure ! restore before rendering; pose/visibility updates continue.
- V3: Sky, unlit mesh, fully fogged mesh, terrain emission and unit-reflecting water given equal composition-domain RGB produce matching output within 2/255 per channel. Exterior includes sky; interior has a black background. Test neutral gray and saturated HDR inputs.
- V4: Diagnostic inputs, camera and output settings, samples and verdict are recorded. Probe failure returns a nonzero status; stale reports are removed at startup. Synthetic consistency is not retail parity.
- V5: Preserve NIF source values and declared unsupported families. Do not compensate for pending material errors with global tint, exposure, ambient or emission changes.
- V6: Static emission → authored linear tint × multiplier × eligible glow sample; preserve zero, dim, HDR and black tint. Slot 2 glow ! Glow shader or Glow_Map; Own_Emit alone ≠ texture eligibility. Non-finite source emission or overflowing energy → contextual conversion error before clamping.
- V7: Converter schema 17 → rebuild GLBs from schemas 12–16; reuse verified unchanged texture/script/archive outputs only with matching source/configuration. Historical emission output retained its producer 17 identity; current compatibility is recorded in V11.

- V8: Finite signed NIF tint & static lighting multiplier ! convertible; glTF nonnegative projection retains raw signed color/multiplier and names lower-clamp approximation. Positive channel energy follows V6; signed shader parity remains gap.

- V9: NIF glossiness exponent → bounded monotonic `(2 / (n + 2))^0.25` perceptual roughness; GGX lobe approximation, not exact Skyrim BRDF.
- V10: Specular flag off or strength zero → explicit zero glTF factor. Enabled tangent normals → shared linear normal-alpha mask; model-space normals excluded. glTF specular factor ∈ [0,1]; only tagged loaded masks receive Bevy 0.19 compensation; generic glTF unchanged. F0 still squares scalar/mask inputs; native Skyrim intensity/BRDF parity remains gap.
- V11: Pruning/remapping ! both specular extension textures; removed mask → unchanged bounded factor and no native compensation. Current producer 27 combines native BC textures, emission/specular, source-surface fixes, corrected TXST slots and collision coverage. Rebuild all legacy GLB/KTX2/world outputs from schemas 12–24 and 26; producer 24 GLB compatibility ended with the collision data change. Verified unchanged scripts/archive ingestion remain reusable. Exact producer 27 required for staged meshes/textures; encoder mode and GPU quality participate in configuration identity. Runtime/launcher accept complete converter schemas 15–27 and world schemas 3–9. Schema 5 grass worlds need no LOD tables. Metadata-only retained meshes/textures preserve original producer/configuration and require normal conversion to repair old encoding and collision data.
- V12: Scene-only glTF loads ! reach and retain recursively loaded state after unused subassets release. Native material override retains stock hook's recorded source dependency; scene cloning preserves source and native handles. Probe ! no root-glTF or explicit material loads that mask dependency lifetime failures.

- V13: Converted tangent-space NIF normals ! DirectX Y convention exactly once at native material construction; generic glTF/model-space maps unchanged. Preserve linear RGB & source alpha. ±X/±Y/asymmetric GPU swatches ! match independent geometric normals ≤2/255; legacy no-flip control ! fail.
- V14: NiAlphaProperty owns test/blend enable; vertex-alpha/premultiplied/screendoor flags alone ! opaque. Preserve shader-enabled vertex alpha except native tree/LOD exclusions; prepass & shadow ! same discard as color pass.
- V15: Source UV offset/scale & four S/T clamp modes ! survive conversion. Conflicting wrap or transfer uses ! distinct asset paths; load order cannot change sampler. Alias source mapping ! cache/pruning/restoration consistency.
- V16: Declared EditorMarker & unsupported Fire_Refraction ! excluded with reason; ordinary visible controls remain. Unsupported shader features ! explicit compatibility inventory, no silent parity claim.

## Supported response and remaining material work

| Family / path | Current representation | L1 acceptance status |
|---|---|---|
| Ordinary lighting / opaque | glTF `StandardMaterial`, PBR lighting | Output consistency can be tested now; Specular enable, tangent mask and roughness corrected by T8; native BRDF/model-space parity remains open. Static emission corrected by T6. |
| Alpha-tested / blended | NiAlphaProperty mode, shader-enabled vertex channels, PR #99 prepass | Synthetic color/depth/shadow silhouettes verified; additive and other nonstandard blend factors still approximated. |
| Tangent-space normal maps | Linear RGB/alpha; native material applies DirectX Y convention once | Asymmetric mesh directions match geometric-normal references; NIF authored tangents still regenerated. Terrain owns a separate tangent frame and needs separate native-direction evidence. |
| Model-space normals | No established compatibility path in this slice | Named L1 gap; generic tangent interpretation cannot count as acceptance. |
| Environment-map / parallax / skin / hair / other NIF lighting variants | Raw source contract/extras plus generic approximation | [Compatibility inventory](../../lighting-l1-compatibility.md) names per-family gaps; retained metadata alone is not shader support. |
| Effect shader surfaces | Generic approximation with explicit exclusions; palette effects drawn through their palette (#289) | Declared EditorMarker and Fire_Refraction excluded with reasons; no general effect, refraction, particle or animation parity claim. |
| Water / sky | Custom Bevy shader paths | Output-domain test only; authored behavior and full-scene parity remain open. |

## Verification

- `cargo test -p engine --lib`: explicit scene settings, production reflection format, reflection exposure lifecycle, existing renderer/sky regressions.
- `cargo run -p engine --example color_pipeline_probe -- --output <dir> [--interior] [--gray]`: real GPU shader path and pixel comparison; uses headless GPU readback and requires a Vulkan adapter with at least 32 sampled-texture and sampler slots. The probe requests WebGPU features with terrain limits explicitly raised; it does not benchmark the full production device feature set. Software Vulkan is sufficient for functional validation, not performance acceptance.
- Run all four combinations: exterior/interior × gray/HDR. Each writes `probe.png` and `probe.json`. The probe fixes 800×600, orthographic camera `(0,0,10)`, EV100 9.7, TonyMcMapface, no dither/MSAA, full diagnostic fog on one swatch, unit reflectivity on water, and constant sky rows. All are isolated diagnostic settings, not shipping values.
- The probe uses the production reflection allocation and camera setup; its unlit source geometry, render layer and visibility are diagnostic. A clear-only reflection would not exercise material tone mapping. The HDR case includes blue = 2.0 to detect clipping and duplicate display transforms.
- `--legacy-output` is a negative control: restore the previous non-HDR cameras and 8-bit reflection target inside the probe. It must fail the consistency check, with a nonzero status and saved pixel differences. This option does not exist on the game CLI.
- Complete L1 acceptance additionally needs NIF-to-runtime material probes, integrated dependency fixes and matched vanilla neutral/material captures from L0. Leave #131 open until those gates pass.

## Static emission publication (historical schema-17 slice)

Historical packages and probes retain their original producer identities. Current producer 27 regeneration and compatibility are recorded in V11.

[Emission issue #82](https://github.com/Mudcrab-Team/mudcrab/issues/82): `Own_Emit` declares own emittance; `Glow_Map` declares third-slot glow (`vendor/project-wormhole-nif/src/nif_flags.rs`). Glow shader type also permits slot 2. Own_Emit alone retains slot 2 as unclassified source data; no slot-2 emissive texture sampling.

Publication: `peak = max(1, max(emissive_color))`; `emissiveFactor = max(0, emissive_color) / peak`; `emissiveStrength = max(0, emissive_multiple) * peak`. glTF factor remains in [0,1]; reconstructed linear RGB retains nonnegative authored energy. Extension emitted whenever strength ≠ 1, including 0 and values below 1. Black tint stays black with glow present. Bevy 0.19 glTF loader multiplies factor by strength into `StandardMaterial.emissive`; glow uses sRGB decode once, alpha does not scale opaque emission. No camera/ambient compensation.

For [#469](https://github.com/Mudcrab-Team/mudcrab/issues/469), the native handler modulates emission by diffuse colour on tagged Default lighting materials with `Own_Emit` and no `Glow_Map` or published emissive texture. It reuses the diffuse image, sampler, UV channel and transform, and multiplies the loaded emission by the linear base-colour factor. The source material, authored tint and multiplier remain intact. Existing packs need no reconversion for this runtime correction.

The shader-source basis is [Community Shaders e5ae2d0, Lighting.hlsl:2632–2657 and 2815–2821](https://github.com/community-shaders/skyrim-community-shaders/blob/e5ae2d0082469f92ac08dda89d230dc2d9328ba0/package/Shaders/Lighting.hlsl#L2632): the non-TRUE_PBR path adds emission to diffuse lighting before applying surface colour. This correction does not establish retail brightness, colour-space or vertex-colour parity. EnvironmentMap liquids, glow-map runes and effect surfaces retain their existing evaluation; the alchemy/enchanting station observations require separate native acceptance.

`material_own_emit_probe --output <new directory> [--legacy]` tests the native binding against two CPU-computed diffuse texels at fixed exposure, with no lights or ambient. It records PNG/JSON evidence; `--legacy` selects the stock additive material and must fail. Optional `horn_candle_emission` retail tests trace five horn fixtures and four station candidates into new task-owned output directories, preserving luminous liquid, rune and effect inputs.

Signed NIF tints remain valid. Negative channels retain previous glTF lower-clamp approximation; raw color/multiplier retained under `extras.openSkyrim.sourceEmission`, with explicit representation label. Signed-emission shader behavior remains unsupported; this projection does not claim Skyrim parity. Strength overflow → contextual conversion error. Source contract retains original valid values. Static lighting multipliers retain their raw sign in the contract and `sourceEmission`, including with nonnegative/black tint; only the glTF projection clamps negative values to zero. These are static shader-property values, not proven controller endpoints. NaN/±infinity rejected before publication; animation and shader-family compatibility remain L1 gaps. Contributor reference inspected at `BimingtonBill/mudcrab:ff96ac2b91bec0765d9ce59b2890449923f9d3ed`; emission publisher there retains old defects, so this slice uses current material owner directly.

`material_emission_probe --output <dir> [--interior] [--legacy-emission]`: converter-published synthetic NIF contracts → glTF/KTX2 → Bevy loader → GPU swatches. Nine cases: zero, dim, unit, HDR, dim HDR, black glow, untextured HDR, Own_Emit atlas, signed tint. Loaded factors checked against authored energy; glow view ! `Rgba8UnormSrgb`. Converted swatches compared with independently computed material RGB at tolerance 2/255; zero cases ! black, other references ! visible. 800×900, orthographic camera `(0,0,10)`, no lights/ambient/fog/dither/MSAA, pinned scene tone map/exposure. Interior/exterior here change diagnostic background only; no authored scene parity claim. Synthetic contract publication ≠ full NIF-file parse coverage. `--legacy-emission` restores old energy/eligibility defects inside probe and ! fail with exit 1. Every run records PNG, JSON, generated glTF/KTX2; removes stale verdicts before startup.

Historical emission migration (schema 17): schemas 12–16 retain verified non-GLB entries/archive ingestion only when source and original configuration hash match; GLBs/world data rebuilt. Configuration changes still invalidate cache. Stage journal schema check rejects old staged GLBs. Current runtime/launcher accept complete converter schemas 15–27 and world database schemas 3–9; cell-cache version unchanged.

For testing, reconvert to separate output directory with converter built from this branch, then run matching engine against that directory. Existing packs remain valid in engine but retain old emission until reconverted. Preserve old pack for rollback; older #137 engine rejects schema 17, so use new engine for new pack. Retail reconversion ! isolated matching package; delivery evidence recorded after successful conversion/startup.

Verification: `v6_emission_preserves_zero_dim_hdr_and_black_glow_energy`, `v6_own_emit_does_not_enable_slot_two_glow`, `v6_rejects_overflowing_emission_with_context`, `v8_signed_tint_keeps_source_and_clamps_only_gltf_negative_channels`, `recent_schema_migrations_reuse_only_unchanged_asset_kinds`, `v15_old_material_schemas_rebuild_meshes_and_reuse_compatible_assets`; engine runtime-schema acceptance tests; both probe backgrounds plus legacy negative control. Full converter/engine library suites, formatting and Clippy required before publication.

## External emittance (#242)

`SLSF1_External_Emittance` (bit 29) says a surface's emission colour comes from outside the model:
from the light or region its reference names in `XEMI`. Building windows use it. The Sleeping
Giant Inn's window (`architecture\farmhouse\inn01.nif`, shape `Inn01:14`) is a Glow shader with a
white tint, multiplier 3 and a neutral grey glow map; its Riverwood reference names
`FXLightRegionInvertWindowWhiterun`, whose weather's Effect Lighting colour is black by day and
amber at night. Publishing the static emission alone left the windows lit in daylight.

The GLB keeps the flag in `extras.openSkyrim.shaderFlags1`, so no mesh is reconverted. The
converter resolves each `XEMI` source's colour at every time of day into the `emittance` table
([db-schema §16](../converters/db-schema.md#16-reference-emittance-sources-emittance), an
optional table the engine probes for). At runtime:

- `NifMaterialPlugin` marks each primitive whose material has bit 29 with `ExternalEmittance`,
  carried with the scene into every instance.
- A reference whose source resolved gets `ReferenceEmittance`: the day colour (the engine's one
  time of day, `sky.rs`) decoded from 8-bit sRGB to linear, as light colours are.
- When the model is instantiated below the reference, each marked surface draws with a material
  whose emission is the model's own emission times that colour. Materials are shared per model
  material and colour, and the model's material stays unchanged for other references. The cache
  of shared materials holds ids, not handles, so a material is freed with the last surface that
  draws with it when its cell unloads.

Approximations: the colour multiplies the published emission instead of replacing the NIF tint,
because the glTF cannot separate tint from multiplier; the two agree for the white-tinted window
shaders. A region's likeliest weather stands in for its climate. A surface whose reference names
no source, or one the converter could not resolve, keeps the model's emission; Skyrim's behaviour
there is unverified. A database converted without the `emittance` table has no sources and keeps the
previous always-lit result until reconverted.

Verification: `resolves_reference_emittance_from_regions_and_lights`,
`the_sleeping_giant_inn_windows_are_dark_by_day` (opt-in, reads `Skyrim.esm`),
`returns_the_day_colour_of_a_reference_emittance_source`,
`external_emittance_needs_bit_29_of_shader_flags_1`,
`a_black_day_colour_darkens_only_the_external_emittance_surface` and the other `emittance` tests.

## Effect palette (#289)

A `BSEffectShaderProperty` with `Greyscale_To_PaletteColor` (flags 1, bit 4) or
`Greyscale_To_PaletteAlpha` (bit 5) does not draw its source texture. The texture's grey level
picks a colour, and its alpha an opacity, from the greyscale (palette) texture. Forge fires and
coals, braziers and many magic effects use it. Drawn as an ordinary material, the blacksmith
forge's coals (`furniture\blacksmithforgemarker.nif`, `BlacksmithForge01:2`, palette
`gradhotcoals`) came out as a flat white disc.

The converter already publishes the palette as the `greyscale` slot of `OPEN_SKYRIM_material` and
keeps the flags, so no mesh is reconverted. At runtime `NifMaterialPlugin` gives each such effect
an `EffectPaletteMaterial` (`StandardMaterial` extended by `shaders/effect_palette.wgsl`) in place
of its standard material. The lookup follows the retail effect pixel shader as reconstructed by
Community Shaders, revision `4d6ac751a05f2e0a10f8add4cc1ba5a0b80ea9e5`,
[`Effect.hlsl`](https://github.com/doodlum/skyrim-community-shaders/blob/4d6ac751a05f2e0a10f8add4cc1ba5a0b80ea9e5/package/Shaders/Effect.hlsl):

- colour = base colour scale × palette(texture.g, base colour.r × vertex.r).rgb;
- alpha = palette(texture.a, base colour.a × vertex.a).a;
- otherwise texture × base colour × vertex colour, as before. The result is unlit, like the game's
  effect shader, and goes through the usual fog and output transform.

Colour spaces: the game looks up the stored grey level, so the shader re-encodes the
sRGB-decoded source sample. Bevy loads a texture with no standard glTF slot names through an sRGB
view, so the palette colour is decoded once on sampling; spawn validation requires that view. The
base colour scale is recovered from the published emission (`emissive / base colour`, see the
emission publication above); a black base colour publishes none and falls back to 1.

The depth prepass and shadow passes use `shaders/effect_palette_prepass.wgsl`, which tests the
same alpha as the colour pass (`shaders/effect_palette_functions.wgsl` holds the shared lookup).
With Bevy's standard prepass, an alpha-tested effect whose palette alpha disagrees with its
source alpha would write depth and cast shadow where it draws nothing, or the reverse.

`ExtendedMaterial` reports none of its fields' handles as asset dependencies. The handler
therefore adds each palette image under a label of its own (`…/nif_palette_image`, an asset whose
image is a `#[dependency]`), and the palette material depends on that label. A scene then counts
as loaded only once its palettes have loaded; without this, spawn validation could run before a
palette arrived and reject the model for good.

A palette surface that also has an authored depth override (`crate::nif_depth`: decals, surfaces
that test but do not write depth, and surfaces with depth testing off) draws through
`EffectPaletteDepthMaterial`, the depth extension wrapped around the palette material
(`…/nif_palette_depth`). The wrapper keeps the palette shaders and adds the authored depth state,
the sorted colour pass, no prepass and the authored `Cast_Shadows` flag, exactly as
`NifDepthMaterial` does for standard surfaces. The surface keeps its palette material as
`EffectPaletteDepthSource`, which spawn validation and the door crossing's readiness check read.

Not covered: effect lighting influence, falloff, soft depth, membrane shaders, `IgnoreTexAlpha`,
nonstandard blend factors and every animation controller.

`material_palette_probe --output <dir> [--interior] [--standard-material]`: converter-published
synthetic effect contracts → glTF/KTX2 → production palette material → GPU swatches beside unlit
references computed on the CPU (bilinear filtering of the decoded 4×4 palette), with a depth
prepass as the engine's cameras have. Nine cases: mid grey, HDR scale with the row from base red
only, clamped corner, colour and alpha blended, alpha only, a depth-tested decal drawn
through the depth wrapper, and three alpha-tested cases over an unlit plane: a decal through the
wrapper whose palette alpha must discard, opaque source alpha with transparent palette alpha (must
leave the plane visible, not a prepass hole) and the reverse. A second glTF names a palette file that does not exist; its
palette material's recursive load must fail rather than finish. The palette view must be
`Rgba8UnormSrgb`; tolerance 2/255; references must be visible. `--standard-material` draws the
stock material the engine used before and must fail with exit 1.

Local Apple M4 Pro, Metal: both backgrounds match exactly (0/255). Negative controls fail:
`--standard-material` at up to 112/255; the standard prepass in place of the palette prepass
leaves a 104/255 hole in the discarded masked case; without the palette image label the missing
palette's material reports itself loaded. In the full Riverwood pack, the forge's coals render as
embers instead of the white disc. Unit regressions:
`palette_lookup_reads_effect_flags_and_the_greyscale_slot`,
`handler_hands_out_the_palette_by_gltf_texture_index`,
`base_color_scale_is_recovered_from_the_published_emission`,
`effect_palette_material_needs_its_palette_loaded_through_an_srgb_view`,
`the_fade_in_waits_for_an_effect_palette_materials_source_texture_and_palette`.

## Local verification, 2026-10-02

Headless Vulkan on llvmpipe / Mesa 26.2.2, LLVM 21.1.8. No renderer errors in the six final runs. These are functional shader checks, not target-hardware performance or Skyrim visual acceptance.

| Case | Largest channel difference (8-bit) | Expected verdict |
|---|---:|---|
| Exterior, HDR `(0.18, 0.4, 2.0)` | 1 | Pass |
| Interior, HDR | 1 | Pass |
| Exterior, gray `(0.18, 0.18, 0.18)` | 0 | Pass |
| Interior, gray | 0 | Pass |
| Previous output path, HDR | 39 | Fail (negative control) |
| Previous output path, gray | 3 | Fail (negative control) |

HDR mesh/sky/fog/water samples: `(114,151,239)`; terrain: `(114,152,239)`. Previous-path sky: `(118,170,255)`; water: `(107,136,200)`. All new-path gray samples: `(115,115,115)`. Interior background: black. Pixel tolerance was fixed at 2/255 before running; the negative controls returned exit code 1.

Static emission probe: exterior and interior both pass with maximum difference 1/255; all eight loaded material checks pass. Legacy negative control returns exit 1, maximum error 230/255, seven loaded-energy/eligibility checks fail. Zero/black/Own_Emit cases render `(0,0,0)`; dim `(68,21,73)`, unit `(123,49,130)`, textured HDR `(237,145,203)`. 353 converter tests pass (13 existing ignores); 230 engine library tests pass; formatting and converter/engine Clippy libraries/tests/examples pass with warnings denied. Same llvmpipe adapter as above. Evidence: `/home/dev/Projects/mudcrab-lighting-emission-evidence/{exterior,interior,legacy}`; functional synthetic proof, not Fiji performance or vanilla retail acceptance.

Signed-tint correction: 18 failing retail NIFs → 18 converted, zero skips; isolated private-fixture run. Nine-case exterior/interior GPU probes pass ≤1/255; raw signed metadata and nonnegative glTF projection checked. Full converter suite: 354 passed, 13 existing ignores; formatting and Clippy pass. Delivery/full-pack integration passed; T7 complete.

## Fiji delivery, 2026-10-02

Package: `/home/taylor/mudcrab-pr139-8db2a3d`; binaries ! commit `8db2a3d3ef07ef6dd92ad19f0c30d0c4c065da2e` (later documentation-only commits do not change deployed binaries). Separate schema-17 pack: complete; 18 converted, 257849 cache hits, zero skipped; inputs DDS 35663 / NIF 25388 / PEX 15162. World schema 4 integration passes; missing/invalid models 0. Converter quick check passes: 76213 files, 13.1 GB. Package hashes and bundled runtime dependency resolution pass.

RX 6700 XT / RADV NAVI22 / Mesa 26.2.2: nine-case exterior/interior probes both pass, maximum difference 0/255. Legacy control fails with exit 1, maximum error 231/255. Exact packaged `run-riverwood.sh --headless` exits 0 and records `Mudcrab runtime initialized`. Existing #137 package and original schema-16 asset pack preserved; original manifest/database/cell-cache hashes unchanged.

Evidence: package `DEPLOYMENT.json`, `conversion-report.json`, `material-probe/{exterior,interior,legacy}/probe.json`, `asset-check.log`, `smoke-new-assets.log`; local copies under `/home/dev/Projects/mudcrab-lighting-emission-evidence/signed-fix/`. Installed source NIFs stay private. Synthetic GPU/startup proof ≠ matched Skyrim scene acceptance or release performance; L0 comparison and remaining L1 families remain open.

## Specular integration plan

Sources: Bill's `5a116e79c1bc6327b4bd6bdf0c65635464701236` (exponent mapping) & `92d60dc64bcfa0cb210ec0fe4349668ec7c0ce97` (normal-alpha mask/pruning). Adapt owners; do not copy unrelated fork changes. NIF Specular bit 0 and Model_Space_Normals bit 12 traced in vendored flag definitions. Bevy 0.19 glTF loader scales factor by 0.5; fragment multiplies reflectance by mask alpha × 0.5; F0 ! `0.16 * reflectance^2`. Scalar/native response remains declared approximation.

Review: fork factor doubling exceeds glTF domain → reject; native glTF extension handler owns compensation. Tagged masks ! private `/nif` material label & scene binding; no duplicate standard label. Missing/pruned mask ! no compensation. Independent converter publication/pruning tests + runtime loader/GPU comparisons ! pass; legacy behavior ! fail. Gate: GO with V9–V11. No unrelated prepass/sampler integration in this slice.

cmd: `material_specular_probe --output <dir> [--interior] [--legacy-specular]` → PNG/JSON, deterministic paired PBR samples, nonzero failure. Existing camera/exposure settings unchanged; no retail parity claim. Converter marker: `extras.openSkyrim.specularMask = "normal_alpha"`; native handler consumes only marker and actual loaded mask. Native factor compensation ! 2× only at material construction; reloads do not compound.

## Specular local verification, 2026-10-03

358 converter tests pass (13 existing ignores); 231 engine library tests pass; formatting and converter/engine Clippy libraries/tests/examples pass with warnings denied. Nine-case exterior/interior `material_specular_probe`: loaded scene bindings, native factors and shared linear textures pass; maximum paired difference 0/255 on Vulkan llvmpipe / Mesa 26.2.2. Legacy control ! exit 1; maximum difference 248/255. Cases: disabled flag, zero strength, unmasked, full/thatch/zero mask, exponents 100/200, dim strength. Emission regression probe passes ≤1/255.

Evidence: `/home/dev/Projects/mudcrab-lighting-specular-evidence/{exterior,interior,legacy,emission-regression}`. Probe holds camera/exposure/tone map, directional illuminance 5000, zero ambient and no shadows; background varies only exterior/interior. Both native scene binding and independently computed scalar-response references exercised. Retail roof appearance and full-pack delivery remain separate gates; no Skyrim BRDF/parity claim.

## Specular target-hardware verification, 2026-10-03

Full Fiji schema-18 reconversion: 25388 NIFs converted, 232479 cache hits, zero skipped; inputs DDS 35663 / NIF 25388 / PEX 15162. Schema-4 integration passes with missing/invalid models 0; quick asset check passes (76213 files, 13.1 GB). Original schema-16 and prior #139 manifest/database/cell-cache hashes unchanged. Farmhouse audit: 24 thatch materials; `farmlonghouse01.glb` material 1 roughness 0.20 → 0.395188; normal-alpha mask now shares normal texture index 3.

Initial engine ! 224 pending Riverwood instances, no screenshot. Retained source-material dependency fixes scene readiness: 0 pending, 0 load/material validation failures, screenshot captured. Small synthetic fixtures passed even without retention; full Riverwood run supplies failing-first lifetime evidence. `v12_scene_cloning_retains_source_and_native_material_handles` checks reflected scene cloning; probe loads only Scene0, includes inverted-scale variant, and checks recursive readiness again after 90 rendered frames. Engine library 232 passed; updated Clippy/formatting/build pass.

RX 6700 XT / RADV NAVI22 / Mesa 26.2.2: final exterior/interior probes pass, paired difference 0/255; legacy specular control exits 1 with difference 248/255. Matched Riverwood captures: 2540×1375; camera `(2048,1176,452)`, target `(2048,-24,-2048)`, worldspace 60, grid (5,-12), stream radius 2, EV100 9.7, TonyMcMapface. Foreground thatch ROI `[110,785,430,1000]`: encoded-RGB display brightness 161.15 → 140.64 (12.73% decrease). Right thatch `[2090,632,2230,770]`: 120.00 → 112.05 (6.63%). Stable terrain control: mean channel difference 0.154/255; 94.92% pixels identical. Geometry/camera alignment inspected; animated water excluded.

Evidence: `/home/dev/Projects/mudcrab-lighting-specular-evidence/riverwood-comparison/{before-matched.png,after-candidate.png,roof-comparison.png,metrics.json}`, `roof-material-audit.json`, final scene-only probe JSON. Before/after compare #139 versus #141 Mudcrab; blue wash, remaining lighting work, native BRDF/model-space gaps remain. Test-profile 20-second smoke benchmarks pass their configured gates; no release performance or vanilla Skyrim parity acceptance.

## Source material completion (historical schema 19; current producer 27)

`NifMaterialPlugin` extends existing native glTF hook: retain source dependency,
apply tagged normal Y once, preserve mask compensation, apply common native UV
transform even without diffuse. Generic glTF remains unchanged. Alias identity
includes transfer space and S/T clamp mode so shared image loads cannot overwrite
another material's sampler. UV transform remains per material. Historical schema 19 rebuilt GLBs and reused
then-compatible nonmesh outputs. Current producer 27 rebuilds legacy meshes and
textures from known schemas 12–24 and 26, including the formerly compatible producer 24 GLBs, and
retains only verified scripts/archive ingestion.

Riverwood gate source basis: packed NIF tangent follows texture V; split tangent
components follow U. Bevy-generated tangents negate Mikk handedness. Native shader
samples `2 * rgb - 1` in source frame. Converted mesh frame therefore requires
`flip_normal_map_y`; no pixel rewrite, sun reversal or ambient adjustment.
Community Shaders source revision `2f2919a71bed6132b125e41781304c8f6f73d002`,
[`Lighting.hlsl`](https://github.com/doodlum/skyrim-community-shaders/blob/2f2919a71bed6132b125e41781304c8f6f73d002/package/Shaders/Lighting.hlsl),
TBN construction, `TransformNormal`, vertex color and tree/LOD alpha branches.
Direct retail shader/oracle acceptance remains L0-dependent.

`material_normal_probe`: six asymmetric linear DDS normal swatches → converter
material publication → KTX2/glTF → production scene-only native loading → rendered
comparison with geometric normals. Interior/exterior ≤2/255, source alpha preserved,
inverted-scale variant, scene lifetime, all four wrap modes and no-diffuse UV transforms checked. `--legacy-normal` ! fail.
`material_alpha_probe`: fading quad vs explicit half-quad geometry; discarded and
visible color, depth, cast shadow and lit receiver samples. Both backgrounds
≤2/255; `--legacy-prepass` ! fail. Interior diagnostic background ≠ authored
interior lighting verification.

Local llvmpipe Mesa 26.2.2: normal six cases and alpha five cases both backgrounds
match exactly (0/255). Negative controls fail: normal max 61/255, prepass max
154/255. Unit regressions cover modern NIF RGBA through GLB accessors, source
channel matrix, NiAlphaProperty modes, UV/wrap identity, alias pruning/restoration,
cache migration, and exclusion positive/negative controls.

## Historical Fiji GPU verification, 2026-10-03

RX 6700 XT, RADV, Mesa 26.2.2. Both diagnostic backgrounds: normal, alpha,
specular and emission maximum error 0/255; composition maximum 1/255. All ten
positive runs pass. Legacy controls fail with exit 1: normal 61/255, alpha
154/255, specular 248/255, emission 231/255, composition 39/255.
Recorded settings, loader assertions, samples and verdicts:
[`lighting-l1-20261003.json`](../../evidence/lighting-l1-20261003.json).
Synthetic [normal](../../images/l1-normal-comparison.png) and
[alpha](../../images/l1-alpha-comparison.png) comparisons contain no retail assets.

Final workspace verification: 844 passed, 0 failed, 18 existing ignores across
`cargo test --workspace --all-targets` plus `cargo test --workspace --doc`.
Strict workspace Clippy and formatting pass. Independent [CI run 37094801783](https://github.com/Mudcrab-Team/mudcrab/actions/runs/37094801783) passes format, Clippy, tests, security and performance for source commit `dbf048e`. Added sampler/UV assertions in normal
probe subsequently pass targeted Clippy/build and local/Fiji GPU runs.

## Historical L1 delivery, 2026-10-03

[PR #142](https://github.com/Mudcrab-Team/mudcrab/pull/142), source `dbf048e`;
Fiji launcher `/home/taylor/mudcrab-l1-20261003/run-riverwood.sh`.
Optimized test-profile engine/converter and probes, bundled runtime libraries.
Previous #141 pack's manifest/database/cell-cache hashes unchanged.

Schema 19 conversion complete: 25,402 converted, 232,465 cache hits, zero skipped.
25,388 GLBs; 527 absent-source texture references pruned and recorded. World
schema 4 integration passes: missing/invalid models 0, missing textures 0.
Existing source-coverage gaps remain: unavailable model sources 128, unbounded
models 28, unavailable texture sources 16. No claim that conversion creates
content absent from installed Data.

Trusted transferred ingestion/output cache reused with `--no-verify-cache`;
source/configuration checks remain. Post-publication Fiji full size/hash check:
76,213 primary files, 13.5 GB, all pass. Separately verify all 3,998 generated
texture aliases by hard-link identity or source hash; the converter manifest's
primary-file check does not enumerate those aliases. Six differing primary
textures and affected aliases copied privately after the first hash check caught
them. Read-only 1,121,325,056-byte SquashFS stores new meshes/world data inside
package; immutable unchanged textures/scripts share old files by hard link.
Mount helper remounts after reboot and does not pass its mount lock to the daemon.

Matched Fiji captures: six views of three Riverwood gates, before/after exact
1280×800 poses; all settle, none time out. Same engine, camera, exposure and sun;
old schema-18 vs corrected schema-19 material data. Wall normal highlights change
orientation without changing global light direction. Overview checks roofs and
foliage. Retail images stay in private evidence, not the repository.

20-second Riverwood smoke runs after 1,200 warmup frames, RX 6700 XT / RADV,
private Weston GL compositor, 1280×800: before 76.38 FPS / 15.18 ms p95; after
74.38 FPS / 15.56 ms p95. Both pass configured 60 FPS / 16.67 ms gates. New run:
25 resident cells, 2,029 ready assets, 3,872 validated materials; pending assets,
load/material/terrain/water failures and diagnostic fallbacks all 0. Single short
pair, not release performance acceptance or a causal speed comparison.

Private evidence root `/home/dev/Projects/mudcrab-lighting-l1-evidence`:
`retail/conversion-report.json`, `retail/material-inventory.json`,
`fiji/{wall-before,wall-after}`, `fiji/wall-comparison.png`,
`fiji/wall-comparison-metrics.json`, `fiji/riverwood-{before,after}.json`.
[Compatibility inventory](../../lighting-l1-compatibility.md) lists shader gaps.
Implementation T4 delivered; T5 and #131 remain open for L0 matched-reference
acceptance and required unsupported material cases. L2 owns remaining hardcoded
sun/ambient/weather behavior. Lighting addons remain out of scope.

## Emission review audit (2026-10-03)

[Aggregate report](../../evidence/lighting-l1-emission-impact-20261003.json): full schema-19
conversion, 25,388 GLBs; 58,133 published lighting-material instances. Parse winning
NIF bytes by manifest source hash (first hash precedes skeleton dependencies), join
source shader block to material extras, compare pre-#139 and corrected publication
on identical inputs. Reconstructed corrected energy matches all 58,133 published
factors/strengths. Effect shaders and excluded shapes outside count; this corpus
includes installed official DLC/Creation Club content, not a base-game-only sample.

- 3,599 instances lose slot-2 glow eligibility; 0 gain it. Eligibility precedes
  missing-texture pruning, so this is not a count of visible glowing surfaces.
- 41 retained glow-eligible instances have black tint; old white fallback removed.
- 4,177 change energy factor; 4,447 change factor or eligibility across 2,512 files.
- 3,631 lose nonzero emission factor; 0 gain it. Texture samples not evaluated.
- No non-finite source emission or strength overflow in counted instances; two
  finite negative static lighting multipliers keep a zero glTF projection.

Malformed emission still fails its asset with source/shape/shader context. Pipeline
records that failure; no successful complete publication with a skipped bad asset.
No clamp of non-finite/overflowing energy introduced: audited data does not justify
one. Finite signed tints and static lighting multipliers retain V8; signed native
emission remains unsupported. Earlier endpoint attribution was unverified. Review regression tests cover non-finite
multipliers before clamping, all non-finite tint channels and invalid negative
effect-material strength. Converter suite: 357 passed, 13 existing ignores; strict Clippy
and formatting pass. L0 acceptance remains open.

Signed-static follow-up: two Volendrung source meshes reconvert successfully in an
isolated mesh-only fixture. Both retain raw multiplier `-0.6013296842575073` and
publish zero glTF strength; 14 absent fixture texture references are pruned and
recorded. This checks source parsing/publication, not rendering or world coverage.
Synthetic regression covers positive, signed and black tint with and without glow.
Existing prereview packs retain their old metadata until rebuilt; use a separate
output or `--invalidate-cache` to refresh it. No new runtime shader behavior or
asset-format version is introduced by this metadata correction.

## Review follow-up (2026-10-03)

Reflection recovery: optional component query restores missing `Exposure` before
rendering; pose/visibility continue. Shared `DEFAULT_SCENE_EV100` owns baseline.
V1 tests execute five world/visual setup systems plus physics fixture startup;
V2 regression fails before fix, then passes with observer exposure and fallback.
232 engine tests pass; strict engine Clippy and formatting pass.

[Existing terrain/water fixture comparison](../../images/l1-color-fixture-comparison.png)
and [capture conditions](../../evidence/lighting-l1-color-fixture-20261003.json):
identical camera, materials, lighting, fixed water time; previous output path vs
HDR/linear reflection. Fixture has no sky; existing composition probe covers sky.
Private capture-only patch uses WebGPU features and 32 texture/sampler limits on
llvmpipe, omits production renderer gate from screenshot readiness. Both runs emit
Xvfb/Vulkan swapchain diagnostics and fail software performance gates. Images are
review evidence, not production-device or retail-parity acceptance. Capture-only
changes reverted; shipping renderer requirements unchanged.

## Tasks

id|status|task|cites
T1|x|Trace existing color/material owners and name unsupported paths|V5
T2|x|Use explicit HDR scene composition and linear reflection storage; synchronize exposure|V1,V2
T3|x|Render paired synthetic probes and record pixel evidence|V3,V4
T8|x|Adapt Bill roughness/mask corrections; verify native loader, pruning, cache, scene-only lifetime and GPU response|V5,V9,V10,V11,V12
T4|x|Integrate existing material/prepass/sampler fixes, add converted-NIF response probes and retail deployment|V5,V13,V14,V15,V16
T7|x|Restore signed-tint NIF compatibility; verify retail reconversion and delivered pack|V5,V6,V8
T6|x|Fix static emission publication; load converted materials and compare GPU swatches; migrate cache|V5,V6,V7
T5|.|Compare both scene types against L0 references; accept declared tolerances|V3,V5

## Bugs

id|date|cause|fix
B1|2026-10-02|Non-HDR mesh shaders tone-map while sky bypasses transform|V1,V3
B2|2026-10-02|Reflection is display-mapped before water applies another transform|V2,V3
B3|2026-10-02|Xvfb launch lacked `libxkbcommon-x11` runtime path; local software Vulkan rejected optional features; baseline WebGPU limits excluded terrain bindings|Headless readback; explicit 32 texture/sampler slots; no game renderer fallback change
B4|2026-10-02|Probe used unboxed `WgpuSettings` for Bevy 0.19 `RenderCreation::Automatic`|Box settings; compile-only correction, no new invariant
B5|2026-10-02|Own_Emit enables slot-2 glow; multiplier floor, HDR clipping and white fallback alter authored emission|V6,V7
B6|2026-10-02|Probe assumed glTF material handles were StandardMaterial in Bevy 0.19|Load production PBR /std labels; compiler catches type mismatch; no new invariant
B7|2026-10-02|Schema bump changes pinned manifest configuration hash|Snapshot diff reviewed: schema 17 and corresponding configuration hash only; V7
B8|2026-10-02|Rejecting signed NIF tint applies glTF domain to source data; Fiji reconversion skips 18 base/DLC/CC assets|V8; retain source tint; glTF lower clamp remains named approximation
B9|2026-10-02|Narrowed overflow regression left single-element test loop|Clippy catches mechanical shape; remove loop; no new invariant
B10|2026-10-02|Roof specularity ignores source enable/mask; glossiness treated as percentage|V9,V10,V11
B11|2026-10-02|Fork mask compensation publishes glTF factor >1; pruning can retain invalid texture indices|V10,V11; bounded glTF plus native loader compensation
B12|2026-10-02|Pinned snapshots predate schema 18 and specular response|Reviewed diff: schema/hash, flag-disabled factor 0, exponent-derived roughness; V9–V11
B13|2026-10-02|Bevy standard-material conversion helper public fn resides in private module|Clone stock `/std` labeled asset through public LoadContext API; preserve full field mapping; compiler/probe catch wiring, no new invariant
B14|2026-10-02|Native helper retained GltfMaterial parameter after switching to StandardMaterial clone|Correct parameter type; compiler catches mechanical mismatch, no new invariant
B15|2026-10-02|Probe used pre-0.19 scene/shadow names|Use WorldAsset and shadow_maps_enabled; compiler catches API mismatch, no new invariant
B16|2026-10-02|Probe held asset borrow while inserting reference and queried immutable scene guard|Clone normal handle before insertion; mutable WorldAsset guard; compiler catches borrowing, no new invariant
B17|2026-10-02|Adding scene-binding verification exceeded probe system argument lint|Group related glTF/WorldAsset resources; Clippy covers mechanical shape, no new invariant
B18|2026-10-03|Native scene override drops stock material handle while stock hook retains dependency ID; 224 Riverwood instances stay pending; small fixture missed streaming lifetime failure|V12; retain source handle in reflected scene component; scene-only probe, scene-clone regression and failing-first Riverwood gate
B19|2026-10-03|NIF texture Y follows UV V; omitted tangents make Bevy generate opposite bitangent, reversing vertical normal relief|V13; source gate/stonewall frame audit plus independent geometric-normal probe
B20|2026-10-03|Modern BSTriShape export drops COLOR_0; blanket cutout normalization also discards ordinary authored edge alpha|V14; restore RGBA then gate channels by shader flags/tree/LOD semantics
B21|2026-10-03|Normal probe's procedural rectangle lacks tangents; all six samples render flat|V13; generate tangent frame before GPU evaluation; geometric references expose omission
B22|2026-10-03|Exclusion test names Skyrim flags wrapper but parsed source property uses historical Fallout4-named wrapper|Use actual property type; compiler catches mismatch, no new invariant
B23|2026-10-03|Pinned snapshots and pruning test paths predate per-wrap aliases and schema 19|Reviewed schema/hash, GLB size, metadata, samplers and alias diff; V15 covers behavior, no new invariant
B24|2026-10-03|Alias helper appended after test module violates strict Clippy item ordering|Move helper before tests; structural lint, no new invariant
B25|2026-10-03|Desktop compositor changes capture frame then closes window; pixman compositor lacks Vulkan surface support|Private GL headless compositor; six baseline shots settle at exact 1280×800; capture environment only, no new invariant
B26|2026-10-03|Deployment assumes every old texture is reusable; full hash check finds six changed outputs; scp drops mount-helper executable mode; FUSE daemon inherits mount lock|Replace six files and affected aliases privately; chmod helper; close lock FD in daemon; repeat full check and mount invocation. Deployment-only corrections; V4/V15 verification catches failures
B27|2026-10-03|Required reflection Exposure query silently drops camera after component removal|V2; restore missing component; regression checks pose, activation and observer/default exposure
B28|2026-10-03|Review test helper followed test module; strict Clippy rejects item order|Move helper before module; mechanical, no new invariant
B29|2026-10-03|Lighting multiplier max(0) hid NaN and negative infinity before material validation|V6; reject non-finite source before publication
B30|2026-10-03|Static lighting multiplier clamped before metadata; mistaken attribution to controller endpoints|V8; preserve raw signed multiplier; clamp only glTF projection; metadata also for nonnegative tint
B31|2026-10-03|Reference-shot prose omits existing failure counters from settle predicate|Describe zero new failures and failed timeout capture; existing shot-settling tests, no new invariant
B32|2026-10-03|Emission migration prose retained latest-only launcher requirement after shared compatibility range landed|Align prose with existing 15–19 readiness gate and schema-range tests; documentation-only, no new invariant

Camera-mod research and remaining native calibration gates are recorded in [Skyrim camera comparison](skyrim-camera-comparison.md).
