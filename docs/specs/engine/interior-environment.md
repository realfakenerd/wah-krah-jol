# Authored interior environment (#264 / #364)

`EnvironmentCatalog` now resolves CELL/XCLL/LTMP/LGTM inputs into one `SceneEnvironment`.
`ActiveSpace.interior` selects the cell; the streamed world's marked directional light and
`GlobalAmbientLight` consume the selection in PostUpdate, after scene switches and interactive
preferences, before sky updates, transform propagation and render extraction. Fixtures retain
existing settings behavior. Point-light evaluation and color defaults are unchanged.
Door crossings also run in PostUpdate: `DoorCrossingSet` precedes `EnvironmentSet`, including
its deferred scene-switch commands, so arrival and failed-landing restoration apply light in
the same frame as `ActiveSpace` changes.

## Selection and inheritance

- CELL DATA bit `0x80` means Show Sky; `0x100` means Use Sky Lighting. These are independent.
  Showing a sky does not select exterior lighting. Use Sky Lighting selects the existing exterior
  lighting baseline and preferences even when the sky is hidden. Selecting weather from XCCM,
  region, climate and time remains outside this slice; that exterior baseline is still a fallback.
- An XCLL inheritance bit selects the linked LGTM field group, including valid black and zero.
  A clear bit selects the cell field. A missing selected template or field stays missing; it does
  not silently borrow the unselected cell/template value. The resolver retains the source per
  field, the original mask/link, and a list of missing fields.
- Groups: `0x1` ambient and ambient cube; `0x2` directional color; `0x4` both fog colors;
  `0x8` fog near; `0x10` fog far; `0x20` both rotations; `0x40` directional fade;
  `0x80` fog clip; `0x100` fog power; `0x200` fog max; `0x400` both light fade distances.
- Inherited ambient cubes use the separate LGTM DALC subrecord. The embedded LGTM DATA ambient
  block is marked **Unused** by xEdit and is not a fallback. A missing cube can use the selected
  scalar ambient color; a present black cube cannot. Specular color and the final DALC float
  (xEdit's Fresnel Power, historically named `scale` in this schema) are retained, not applied as
  ambient intensity.
- Compatibility policies, not verified native behavior: an LTMP-only row uses template groups;
  a short XCLL without an inheritance mask uses its local fields. A database-open failure or
  missing environment tables, cells or flags selects a closed interior with missing authored
  light, not the previous scene. The startup warning distinguishes an open failure.
  Missing colors become black, missing fade becomes 1, missing angles use identity rotation.
  Exiting restores exterior settings/baseline and the original exterior light rotation.
- Rejected XCLL blocks below 72 bytes retain their authored length (including zero), select no
  implicit template groups, and report `MalformedCellXcll` sources with missing lighting fields.
  An absent XCLL alone selects the LTMP-only policy. Older packs can have already collapsed these
  cases; recovering that distinction requires regeneration from source, not a runtime guess.
- Fog and light fade distances are resolved and inspectable, but their renderer consumers remain
  outside this slice. Interior weather fog remains disabled; showing sky alone does not add it.

## Bevy adapter and settings precedence

The adapter identifier is `bevy-interior-v1`; it is an approximation, not a Skyrim parity claim.

1. Interior ambient uses the equal linear-light mean of the six selected sRGB-decoded faces,
   or the selected sRGB scalar color when the cube is absent. This loses directional response.
   It does not invoke the historical affine DALC helper, assume native `G=0`, or apply specular
   scale. The native raw-domain matrix and live source selection remain separate research.
2. Interior direct color is the selected sRGB color. Bevy illuminance is the user's sun level
   (or existing startup level without settings) times `max(selected fade, 0)`. Missing fade
   uses 1. This fade/intensity mapping is an explicit uncalibrated backend policy. Zero stays zero;
   negative values remain in diagnostics and are suppressed at the backend.
3. Rotations use XY as bearing from Creation +Y toward +X and Z as elevation above XY. A
   unit direction is mapped by `(x,y,z) -> (x,z,-y)`; Bevy rays travel opposite that direction.
   This angle interpretation is provisional, not instruction-backed native evaluation.
4. Sun on/off, sun level, shadow enable and ambient brightness remain global user controls.
   RGB preferences apply outside and to Use Sky Lighting interiors. Ordinary interiors retain
   authored RGB when preferences are reapplied, including unrelated preference changes. Settings
   do not rewrite the catalog or persist cell colors. Returning outside applies current preferences.
5. Sky visibility composes Show Sky with the existing user sky toggle. A cell's lighting choice
   never forces the dome on. The clear color follows the selected sky visibility domain using the
   existing sky presentation policy. No image-space/default-color correction is included.

The existing fixed lux/ambient scales are not native units. No retail visual or performance
qualification is implied. In particular, using local fade 0 can suppress the directional light
while retaining ambient; it is not a rule that every interior disables directional lighting.

`SceneState` reports the selected values, sources and missing fields under `environment` and the
adapter name under `environment_adapter`; selection changes also emit one diagnostic log record.
Catalog/space changes refresh selection; preference changes affect the current scene immediately.

## Evidence

Format definitions inspected on 2026-10-31:

- [xEdit TES5 definitions](https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.6/Core/wbDefinitionsTES5.pas),
  Git blob `937db63510050e41b38f43434cfea306898b6dd5`: CELL DATA flags, XCLL inheritance groups,
  LGTM DATA unused embedded ambient, and separate DALC.
- [xEdit common definitions](https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.6/Core/wbDefinitionsCommon.pas),
  Git blob `b93975dab798f054b75497fedca0869fe80a2ffa`: six directional colors, specular and Fresnel Power.
- [CommonLibSSE-NG InteriorData.h](https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/I/InteriorData.h)
  and [TESObjectCELL.cpp](https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/src/RE/T/TESObjectCELL.cpp):
  matching inheritance names and independent Use Sky Lighting guard in cell fog setters. These
  declarations/wrappers establish neither retail light equations nor a native source-selection trace.
- Existing converter/schema: [environment.rs](../../../crates/converter/src/esm/environment.rs)
  and [db-schema.md](../converters/db-schema.md). No schema or asset conversion change is required.

Read-only inspection of registered `user-testing-main-b5259d33` found both cells 78791/78792 with
DATA 129 (`0x81`), LTMP 659862 (`FarmLightingTemplate`), inheritance 159 (`0x9f`). Thus ambient,
directional color and the selected fog groups inherit; rotation and fade stay local. The template
directional RGB is 65/70/80, local fade is 0, and local Z rotations are respectively 0/30 degrees.
The separate template DALC is nonzero; its unused DATA cube is zero. This is package evidence,
not a retail live-source trace. No registered pack, settings file or playtest checkout is modified.

## Validation scope

Procedural plugin overrides pass through converter publication, SQLite loading, resolution,
real settings application and actual ECS directional/ambient consumers. Additional CPU tests cover
exterior/interior/exterior, local/inherited/zero/missing values, absent masks, old databases,
separate template DALC, all sky flag combinations and actual dome visibility with preferences.
The opt-in `managed_farm_cells_reach_runtime_without_modifying_pack` test opens only the explicitly
provided `MUDCRAB_INTERIOR_TEST_DB` with read-only SQLite flags. It asserts package selection and
ECS application, not displayed output. Test logs and source identities belong in task evidence.

Remaining research: retail template/ambient selection, missing/short-block precedence, negative
fade and angle semantics, raw-domain light transport, dynamic weather/time and XCCM selection,
fog/image-space consumers, native render behavior, performance and matched Skyrim captures.
