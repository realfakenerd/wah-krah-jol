# Water (WATR) record research

The converter projects a WATR record's colours, reflectivity factors and above-water fog from its
`DNAM` subrecord into the `waters` table (`docs/specs/converters/db-schema.md` section 13). This
file records the `DNAM` byte layout those columns come from.

## 1. DNAM layout

### 1.1 Lengths

Skyrim Legendary Edition writes a 228-byte `DNAM` (slots `0x00`–`0xE0`). Skyrim Special Edition
writes 232 bytes (`0xE8`), adding one trailing float, `flowmap_scale`, at offset `0xE4` (228).
Every other slot is identical between the two, so the offsets below are present in both lengths.

### 1.2 Offset table

Field names are CommonLibSSE-NG `WaterShaderData`
(`include/RE/T/TESWaterForm.h`); slots that source leaves unnamed are marked *unknown*. Colour
slots are four-byte `Color`s (RGBA) and read little-endian like the floats; `u32` slots are noted.
The fog distances are floats in world units, and `above_water_fog_amount` is the curve exponent
(0 keeps the body clear).

| offset | field | decoded by the converter |
| --- | --- | --- |
| 0x00 | unknown (`unk00`) | |
| 0x04 | unknown (`unk04`) | |
| 0x08 | unknown (`unk08`) | |
| 0x0C | unknown (`unk0C`) | |
| 0x10 | `sun_specular_power` | |
| 0x14 | `reflection_amount` (reflectivity) | ✓ |
| 0x18 | `fresnel_amount` | ✓ |
| 0x1C | unknown (`unk1C`, u32) | |
| 0x20 | `above_water_fog_dist_near` | ✓ |
| 0x24 | `above_water_fog_dist_far` | ✓ |
| 0x28 | `shallow_water_color` | ✓ |
| 0x2C | `deep_water_color` | ✓ |
| 0x30 | `reflection_water_color` | ✓ |
| 0x34 | unknown (`unk34`, u32) | |
| 0x38 | unknown (`unk38`) | |
| 0x3C | unknown (`unk3C`) | |
| 0x40 | unknown (`unk40`) | |
| 0x44 | unknown (`unk44`) | |
| 0x48 | `displacement_size` | |
| 0x4C | `displacement_force` | |
| 0x50 | `displacement_velocity` | |
| 0x54 | `displacement_falloff` | |
| 0x58 | `displacement_dampener` | |
| 0x5C | unknown (`unk5C`) | |
| 0x60 | `noise_falloff` | |
| 0x64 | `noise_wind_direction_a[0]` | |
| 0x68 | `noise_wind_direction_a[1]` | |
| 0x6C | `noise_wind_direction_a[2]` | |
| 0x70 | `noise_wind_speed_a[0]` | |
| 0x74 | `noise_wind_speed_a[1]` | |
| 0x78 | `noise_wind_speed_a[2]` | |
| 0x7C | unknown (`unk7C`) | |
| 0x80 | unknown (`unk80`) | |
| 0x84 | `above_water_fog_amount` | ✓ |
| 0x88 | unknown (`unk88`) | |
| 0x8C | `underwater_fog_amount` | |
| 0x90 | `underwater_fog_dist_near` | |
| 0x94 | `underwater_fog_dist_far` | |
| 0x98 | `refraction_magnitude` | |
| 0x9C | `specular_power` | |
| 0xA0 | unknown (`unkA0`) | |
| 0xA4 | `specular_radius` | |
| 0xA8 | `specular_brightness` | |
| 0xAC | `uv_scale_a[0]` | |
| 0xB0 | `uv_scale_a[1]` | |
| 0xB4 | `uv_scale_a[2]` | |
| 0xB8 | `amplitude_a[0]` | |
| 0xBC | `amplitude_a[1]` | |
| 0xC0 | `amplitude_a[2]` | |
| 0xC4 | `reflection_magnitude` | |
| 0xC8 | `sun_sparkle_magnitude` | |
| 0xCC | `sun_specular_magnitude` | |
| 0xD0 | `depth_properties.reflections` | |
| 0xD4 | `depth_properties.refraction` | |
| 0xD8 | `depth_properties.normals` | |
| 0xDC | `depth_properties.specular_lighting` | |
| 0xE0 | `sun_sparkle_power` | |
| 0xE4 | `flowmap_scale` (SSE only) | |

The converter decodes eight slots: +20 (reflectivity), +24 (fresnel), +32/+36 (above-water fog
near/far), +40/+44/+48 (shallow/deep/reflection colour) and +132 (above-water fog amount).

### 1.3 DefaultWater reference values

The engine falls back to Skyrim's DefaultWater fog per water when a record's `DNAM` does not carry
a complete fog triple. DefaultWater (Skyrim.esm, Update.esm) holds near −10.0, far 150.0, amount
0.7. Other observed retail values: `PuddleWater` far 10.0, `ClearInteriorWaterAnkleHeightStill`
amount 0.0, and `DLC1FrostreachWaterFlowNE` (Dawnguard.esm) −290.0 / 720.0 / 0.7.

Sources: xEdit `Core/wbDefinitionsTES5.pas` WATR `DNAM` struct (TES5Edit/TES5Edit);
CommonLibSSE-NG `include/RE/T/TESWaterForm.h` `WaterShaderData` byte offsets; UESP
"Skyrim Mod:Mod File Format/WATR".
