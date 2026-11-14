# Mudcrab Point Lights from `LIGH` References

How the runtime turns a streamed Skyrim light reference into a Bevy point light, how bright it is, and
how many of them are enabled at once. The code is `crates/engine/src/lights.rs` (the conversion, the
scale and the budget), `crates/engine/src/world/database.rs` (the row it is built from) and
`crates/engine/src/streaming.rs` (where the light is spawned).

Lights are **opt-in**: nothing in this document happens unless the engine is run with `--lights`.

---

## 1. What is read

The cell query joins `lights` to the reference's canonical winning base record. It reads radius,
RGB, flags, FNAM fade/status and DATA falloff. `references.radius_override` retains the signed raw
XRDS offset; `radius_status` distinguishes absent (0), finite (1), and malformed/non-finite (2).
The legacy and in-house record readers write the same status (and `fade_status`) for the same
bytes, including a field too short for its float, which the in-house decoder rejects before export.
Despite the historical column name, XRDS is **not an absolute replacement radius**:

```text
offset_used = XRDS unless XRDS is finite and > 8192 (then ignored, as 0)
authored_radius = LIGH.DATA.radius + offset_used (absent offset = 0)
native_radius = authored_radius if authored_radius > 0 else 0.1
backend_range = min(native_radius, 8192)
```

An XRDS offset above 8192 is an outlier. The registered pack (`user-testing-main-b5259d33`) holds
ten such `LIGH` references. Nine are retail: seven in Skyrim.esm and two in Dragonborn.esm, with
offsets from 15,967 up to
3,736,737. One is Creation Club: 9,505.58 on a `BleakFallsSunlight01` reference in
`ccbgssse031-advcyrus.esm`. The engine ignored these before XRDS became additive, so each light kept its record
radius. That is kept: the offset stays queryable, the light keeps its base radius, and the
evaluation is marked `radius_offset_ignored` and backend-limited. Only a sum that passes 8192
through an in-range offset or a large base radius is clamped to the cap.

Negative offsets are valid. Zero offset preserves the base radius. The native constructor replaces
a nonpositive sum with 0.1; this adapter does the same. An invalid present offset is rejected instead of silently using the base radius. The range
limit applies to both base-only and offset radii; it does not overwrite the authored result.

The optional `reference_light_data` table projects XLIG size and its four floats: FOV offset, fade
offset, end-distance cap and shadow-depth bias. Valid shapes are 16/20 bytes. Missing XLIG has no
row; invalid sizes retain a row with NULL fields, and non-finite floats become NULL individually.
The canonical archive retains original bytes. These are raw inputs, **not effective multipliers**.

All optional columns/tables are probed once per connection. Older packages still load; missing
FNAM uses unit fade with a `legacy_fade` disposition. Missing reference projections cannot establish
that the source lacked XLIG. Reconvert into a separate pack to obtain them; never repair a registered
pack in place. No schema-version or asset-producer bump: these additive projections follow #278.

Older exporters already stored XRDS as its raw signed float. They did not convert it to an absolute
radius. The corrected additive evaluation therefore applies to those existing values too. The new
`radius_status` column adds presence/invalid diagnostics; it does not mark a different radius encoding.

## 2. How a row becomes a light

`point_light(row, radius_override)` returns `Some(PointLight)` for a record the engine may light the
world with, and `None` otherwise:

- `range` is the effective radius in Creation units. This engine renders one Creation unit as one
  Bevy world unit, so no conversion is applied.
- `color` is the record's colour, read as sRGB like every other colour of the game.
- `intensity` comes from unit-fade backend conversion times evaluated FNAM plus XLIG fade (§4).
- `shadow_maps_enabled` starts **false**. The existing rendering setting can enable Bevy shadow
  maps. This does not implement Skyrim's authored shadow modes or attenuation.
- the area `radius` stays 0, so there is no oversized specular highlight.

`streaming::spawn_cell` places the light as a **child of its reference**, which is what makes it sit
where the reference is, follow a render-origin rebase with the rest of the cell, and be despawned
when the cell unloads. A reference whose base record is a `LIGH` often has no model at all, so the
reference also gets a `Visibility` component: without it Bevy warns (B0004) and the light child can
never become visible.

## 3. Flags that place no light

`LIGH` `DATA` carries a flag word. Two bits make the engine place nothing (UESP, "Skyrim Mod:Mod File
Format/LIGH"):

| Bit | Name | Why it is skipped |
| :--- | :--- | :--- |
| `0x0000_0004` | negative | the light removes light; this engine cannot subtract a clustered light |
| `0x0000_0020` | off by default | the record is dark until a script turns it on, which the runtime does not model |

Neither bit is set by any `LIGH` record in the shipped plugins, so skipping them cannot take a light
out of the game's own world; they exist for mods and for scripts.

## 4. Evaluation and bounded Bevy compatibility transform

`evaluate_light` retains the raw `LightRow`, signed offset, authored sum, backend range, base fade, authored/effective
fade, native falloff and dispositions as a `LightEvaluation` component on the reference. Immediate and deferred
spawning consume the same evaluated `PointLight`. `LightInputCoverage` counts dispositions once per
spawn, including rejected/zero-light results; reloads count again. Debug logs report cumulative counts.
The denominator includes only references that reach spawning with a usable projected `LIGH` row.
Disabled references, unloaded cells and records with no usable `DATA` row are outside that census.
The counters are overlapping dispositions, not unique FormIDs or full native feature coverage.
Malformed XRDS/XLIG fields remain queryable through projected status, size and NULL fields, with
original bytes in the archive. The converter's omission counters instead describe missing rows.
Source identity remains the containing reference's form/base IDs, linked to the converter's existing
records/formid map. This does not add missing field-level contributor history to old packages.

Finite nonnegative FNAM supplies base fade, including valid zero. Missing, malformed, non-finite
and negative FNAM use unit fade with explicit legacy/invalid dispositions; `fade_status` keeps
absent, valid and invalid distinct. Native Skyrim SE 1.7.104.0 instructions establish:

```text
reference_fade = XLIG.fade (absent = 0)
authored_fade = reference_fade if reference_fade == f32::MAX else base_fade + reference_fade
effective_fade = clamp(authored_fade, 0, f32::MAX)  // bounded Bevy adapter
```

The native sentinel bypasses addition. This adapter reads `f32::MAX` as "no fade": the Bevy light
has no unbounded power, and a maximum-intensity light is not what the sentinel means. An `f32::MAX`
in XLIG leaves the base fade as it is, and an `f32::MAX` in FNAM is read as an absent fade (unit
power). Both are marked `fade_sentinel`, and neither counts as malformed or backend-limited. `Fade 1.35+/-` is an editor label, not an extra 1.35 in the
runtime equation. Signed offsets can cancel base fade; zero remains zero. Malformed/non-finite
reference fade uses offset zero with `invalid_reference_fade`, never a fabricated multiplier.
Negative authored sums remain inspectable but clamp to zero because this backend cannot subtract
clustered light. This clamp and saturation are marked backend-limited, not native semantics.
With base FNAM 1, the observed house offsets 0.5 and 2 evaluate to 1.5 and 3; consuming them alone
cannot justify dimming those houses or claim their brightness is fixed.

Transform identity: `bevy-point-legacy-normalization-v1`:

```text
legacy_radiance = 12000 / (PI * 2.8) * 10  (frozen b5259d33 input normalization)
reference_distance = min(backend_range / 2, 256)
unit_fade_lumens = 4 * PI^2 * legacy_radiance * reference_distance^2 / (225/256)
intensity = min(unit_fade_lumens * effective_fade, f32::MAX)
```

The frozen coefficient preserves this checkout's baseline scale; it is not a new dimming control or
physical unit calibration. It no longer reads `AMBIENT_ILLUMINANCE`, `SUN_ILLUMINANCE` or environment
state. **The inherited unmeasured normalization remains a legacy fallback.** This slice cannot claim
to resolve the observed house overbrightness, close #264/#364, or establish a Skyrim rendered ratio.
The earlier ambient change multiplied configured inputs by 8.526157665637252; that is not a visual ratio.

Bevy divides lumens by `4*PI` and its diffuse BRDF supplies `1/PI`. Its attenuation remains
`window(d)/d²`, with `window(d) = max(1-(d/range)^4,0)^2`. The 256-unit anchor and 8192-unit range
cap are inherited backend safeguards, not native semantics. The formula matches its stated target
at half range only at ranges <=512; it does not match Skyrim's whole attenuation curve. Brightness
above `f32::MAX` is explicitly backend-limited; raw fade/radius remain available for diagnosis.

Native constructor flags `0x1c00` select the spot/hemi/omni shadow modes. Only that branch reads
DATA falloff; the ordinary point-light branch supplies 1 regardless of the stored exponent.
`native_falloff` exposes this evaluated selection. The Bevy adapter retains its inverse-square
attenuation: shadow-mode falloff remains counted unsupported rather than invented as a Bevy
exponent. Ordinary falloff changes are correctly inert and no longer counted as unsupported.
XLIG fade is consumed; nonzero/invalid FOV, end-distance cap and shadow-depth bias remain counted
unsupported. Malformed XLIG is counted separately. No global ambient policy, interior environment
resolver, raw-colour Skyrim profile, shadow, flicker, pulse or camera/exposure change is included.

## 5. Evidence and remaining gates

Pinned upstream sources (format/community implementation evidence, not a retail measurement):

- xEdit [`cce1c42403b279957e1e78bfe38100b29f39f72c`, wbDefinitionsCommon.pas,
  `wbREFRRadiusToStr`, lines 5404-5453](https://github.com/TES5Edit/TES5Edit/blob/cce1c42403b279957e1e78bfe38100b29f39f72c/Core/wbDefinitionsCommon.pas#L5404):
  adds signed XRDS to base radius for display/native values; editing subtracts base radius.
- Same revision, [`wbDefinitionsTES5.pas`, REFR/XLIG](https://github.com/TES5Edit/TES5Edit/blob/cce1c42403b279957e1e78bfe38100b29f39f72c/Core/wbDefinitionsTES5.pas):
  four float layout, optional tail, and `Fade 1.35+/-` label. The label does not prove an equation.
- [CommonLibSSE-NG `b93280e832f263dbef44e44cbe2936622a02f91a`, TESObjectLIGH.h](https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/T/TESObjectLIGH.h):
  separate base fade and DATA falloff; its ExtraLightData.h describes reference floats, not evaluation.
- [Light Placer `6b7c05894de17cb3e7f0b5169e82833c9244e1ed`, LightData.cpp](https://github.com/powerof3/LightPlacer/blob/6b7c05894de17cb3e7f0b5169e82833c9244e1ed/src/LightData.cpp):
  `GetFade` selects base fade; `UpdateLight` assigns scaled fade to NiLight. This mod bypasses the
  native placed-reference construction, so it does not establish XLIG precedence or composition.
- [Community Shaders `e5ae2d0082469f92ac08dda89d230dc2d9328ba0`, Attenuation.hlsli](https://github.com/community-shaders/skyrim-community-shaders/blob/e5ae2d0082469f92ac08dda89d230dc2d9328ba0/features/Light%20Limit%20Fix/Shaders/LightLimitFix/Attenuation.hlsli):
  ordinary versus inverse-square response. `LightLimitFix.cpp/CreatePointLight` calls the native
  constructor before reading runtime data; it does not recover the native XLIG equation.

Version-scoped native confirmation uses SkyrimSE.exe **1.7.104.0**, SHA-256
`846efccf0c1374d71f892907f46549560f2fcb0a75cb87a3eed438baa0f1402f`, image base `0x140000000`:

- `CreatePointLight`, RVA `0x274390`: base fade read at `0x274a6e`, evaluated by `0x300020`,
  written to NiLight fade at `0x274a82`. Radius <=0 selects 0.1 at `0x2746d2`.
- Fade getter `0x300020`: ExtraLightData getter `0x16c640` returns field +0x14 (zero if absent);
  comparison at `0x300047` selects the f32::MAX sentinel, otherwise `0x300059` adds base fade.
- Radius getter `0x2ffd70`, light branch `0x2ffe3c..0x2ffe60`: signed ExtraRadius plus base radius.
- Shadow-mode test `0x27483b`: only flags `0x1c00` read DATA falloff at `0x274847`; otherwise the
  constructor keeps the default 1 initialized at `0x274793`.
- XLIG loader `0x15b914..0x15ba09` maps serialized floats to FOV/fade/end-cap/bias setters.
  Native FOV getter `0x2fff70` also adds its reference offset; cone rendering remains outside scope.

These are bounded read-only static observations, corroborated against original PE bytes and LLVM
instruction decoding using the saved partially analyzed Ghidra project. Proprietary listings stay
in private shared evidence, outside Git. No executable was launched. Cobb API's pinned
[`GetFade`](https://github.com/DavidJCobb/tesv-cobb-api/blob/8b59a9052d5a77647013f3978b25f1a0c47fb3ad/SKSE%20Plugin/CobbAPI/Papyrus/Light.cpp)
independently uses base-plus-reference fade in classic Skyrim; it is supporting evidence, not the
version-matched authority.

Remaining qualification: independent attenuation/unit measurements and fixed-camera interior
comparison after the separate environment resolver lands. Tests prove authored field sensitivity,
storage and the actual CPU light consumer, not rendered brightness, performance or Skyrim parity.
The frozen normalization must be replaced only with that evidence.

## 6. The budget: 64 enabled lights by default

`LightsPlugin` runs one system, `budget_lights`, in `PostUpdate` after transforms are propagated. It
uses the existing budget setting, whose default is `ENABLED_LIGHT_BUDGET = 64`, and sets unselected
`SkyrimLight` children to `Visibility::Hidden`. Wider emitting lights are ranked by camera distance
first. Lights whose range is at or below the native 0.1 fallback use only remaining slots. Zero-power
or invalid lights use no enabled slot. This is backend selection policy; authored values, evaluated
fallbacks and child entities remain intact. Hidden is the switch that matters: Bevy extracts a light to the render world
only while it is visible. A `PointLight` without the `SkyrimLight` marker is never ranked, so the
budget cannot touch a light that a fixture or a future engine feature spawns.

The choice is cached in a resource and re-made when:

- the camera has moved more than `BUDGET_RECHOOSE_DISTANCE = 256.0` units, or
- the **set** of spawned lights has changed - a light that is not in the set the last choice was made
  from, or a count that no longer matches, which covers a cell streaming in or out, or
- a light changes between the wider, tiny and non-emitting budget classes.

The set is what is remembered, not the count: a cell that unloads as another streams in with the same
number of lights is invisible to a count, and a room walked into and stopped in would stay dark until
the player moved another 256 units. Re-choosing is a sort over every spawned light, so it is not done
every frame.

## 7. The `--lights` flag

`EngineConfig::lights` (`crates/engine/src/config.rs`) is false by default and set by `--lights`.
`LightsPlugin` is registered for every run - it owns the budget, not the spawning - but
`streaming::spawn_cell` creates the light child while the flag is on. Otherwise it retains the
evaluated light as deferred data and activates it when the flag is enabled in the resident cell.
A default run therefore renders
exactly as it did before this feature, and the acceptance, profiling and benchmark baselines that
`scripts/phase2-*.ps1` take do not move.

## 8. Scope limits

Native attenuation/units, XLIG end-distance cap, shadow modes, cone/near clip, flicker/pulse and full raw
colour/exposure handling remain open under #133/#264/#364. The legacy normalization is visible and
bounded, not qualified as a native solution. Interior environment selection is separately owned.
