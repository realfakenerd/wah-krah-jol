# OpenSkyrim Point Lights from `LIGH` References

How the runtime turns a streamed Skyrim light reference into a Bevy point light, how bright it is, and
how many of them are enabled at once. The code is `crates/engine/src/lights.rs` (the conversion, the
scale and the budget), `crates/engine/src/world/database.rs` (the row it is built from) and
`crates/engine/src/streaming.rs` (where the light is spawned).

Lights are **opt-in**: nothing in this document happens unless the engine is run with `--lights`.

---

## 1. What is read

Two converter tables carry the data (`docs/specs/converters/db-schema.md`, §5 and §12). The engine
reads them through the cell query it already runs, so a cell's lights arrive with the cell's
references:

| Source | Column | Used as |
| :--- | :--- | :--- |
| `lights` (joined on the reference's base record) | `radius` | `PointLight::range` and the intensity's reference distance |
| `lights` | `color_r`, `color_g`, `color_b` | `PointLight::color`, as sRGB bytes |
| `lights` | `flags` | whether the light is placed at all (two flag bits; see §3) |
| `"references"` | `radius_override` | the reference's `XRDS` radius, which wins over the record's |

The reference's `radius_override` is the placement's own size, and 10,810 of the 12,148 `LIGH`
references in `Skyrim.esm` carry one. An override that is not a positive, finite number is ignored:
`XRDS` has been seen negative, and a radius is a size, not a switch - the flags carry the on/off
state - so a nonsense override leaves the record's radius in place rather than leaving the space dark.
An override above `MAX_RADIUS_OVERRIDE` (8,192 units, two exterior cells) is ignored too: the base
game's positive overrides run up to 6,919 and then jump to nine outliers (15,967 to 3,736,737) that
would reach across many cells. The record's own radius is used when the reference has no usable
override.

Both the table and the column are **probed for** once per connection, when the database worker opens it (`has_lights`, `has_radius_override`),
so a database converted before lights were exported still loads: every reference then reads as unlit
and no query fails. The `light` and `light_radius_override` fields of a reference row are `None` for
an unlit reference.

## 2. How a row becomes a light

`point_light(row, radius_override)` returns `Some(PointLight)` for a record the engine may light the
world with, and `None` otherwise:

- `range` is the effective radius in Creation units. This engine renders one Creation unit as one
  Bevy world unit, so no conversion is applied.
- `color` is the record's colour, read as sRGB like every other colour of the game.
- `intensity` comes from the effective radius (§4).
- `shadow_maps_enabled` is **false**: one cube map per light is unaffordable at 64 lights. Skyrim
  does have shadow-casting lights (the record's shadow flags); honouring them is left for later.
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

## 4. The brightness

Bevy's point light contributes, at distance `d`,

```text
Lout = albedo * NdotL * colour * intensity * window(d) / (4 * PI^2 * d^2)
window(d) = (1 - (d / range)^4)^2
```

The `4*PI^2` is Bevy's, not this engine's: `bevy_pbr/src/render/light.rs` divides a point light's
intensity by `4*PI` on the CPU (`ExtractedPointLight`) and `Fd_Burley` in
`bevy_pbr/src/render/pbr_lighting.wgsl` supplies the Lambert `1/PI`. The ambient light contributes
`albedo * ambient_colour * brightness`, with neither term (`bevy_pbr/src/render/light.rs`,
`ambient_color: ... * ambient_light.brightness`, and `pbr_ambient.wgsl`).

A converted light is given the intensity that makes the two equal at **half its own radius**, times
`LIGHT_EXPOSURE`:

```text
intensity = 4 * PI^2 * AMBIENT_ILLUMINANCE * LIGHT_EXPOSURE * (radius/2)^2 / window(radius/2)
window(radius/2) = (1 - 1/16)^2 = 225/256
```

with the constants of `crates/engine/src/lights.rs`:

| Constant | Value | Meaning |
| :--- | :--- | :--- |
| `AMBIENT_ILLUMINANCE` | `160.0` | the brightness the engine's world path inserts into `GlobalAmbientLight` (`crates/engine/src/app.rs`) |
| `LIGHT_EXPOSURE` | `10.0` | how many times the ambient a light delivers at half its radius |
| `HALF_RADIUS_ILLUMINANCE` | `4*PI^2 * 160 * 10` = 63165.6 | the delivered illuminance the intensity is solved for |
| `HALF_RADIUS_WINDOW` | `225/256` | Bevy's range window at half a range (`getRangeFalloff`) |

**Ten is a round default, not a fitted number.** At half its radius a light delivers ten times the
ambient illuminance, an order of magnitude, so a lamp's pool reads clearly against the room around it.
It is not fitted to any image, and tuning it is a later pass (§7). The scale is per radius rather than
per record, so one constant serves a 75-unit Dwarven lamp and a 3300-unit water light alike: every
light delivers the same surface brightness at half its own reach. A 512-unit torch gets about
`4.7e9`.

## 5. The 256-unit intensity reference cap

Sizing a light's intensity from its own `radius/2` treats the radius as brightness as well as reach,
and a `LIGH` radius is only reach. A shipped record shows the size of the error: a radius of 4334 is
71 times a 512-unit torch's reference radius, so a light sized from its own radius comes out 71 times
a torch and washes out the space it hangs in.

`INTENSITY_REFERENCE_RADIUS = 256.0` caps the reference distance: a light whose radius is up to
`2 * 256 = 512` is lit at its own `radius/2`, byte for byte as the formula above, and a bigger light
is lit at 256 units whatever its radius. The big light keeps its whole `range`, so it still *reaches*
as far as the record says; what it loses is a brightness that grows with the square of that reach.
512 units is the widest common `LIGH` radius, which is what makes the cap invisible to every ordinary
torch, lamp and brazier.

## 6. The budget: 64 enabled lights

`LightsPlugin` runs one system, `budget_lights`, in `PostUpdate` after transforms are propagated. It
enables the `ENABLED_LIGHT_BUDGET = 64` lights nearest the camera and sets every other `SkyrimLight`
to `Visibility::Hidden`. Hidden is the switch that matters: Bevy extracts a light to the render world
only while it is visible. A `PointLight` without the `SkyrimLight` marker is never ranked, so the
budget cannot touch a light that a fixture or a future engine feature spawns.

The choice is cached in a resource and re-made when:

- the camera has moved more than `BUDGET_RECHOOSE_DISTANCE = 256.0` units, or
- the **set** of spawned lights has changed - a light that is not in the set the last choice was made
  from, or a count that no longer matches, which covers a cell streaming in or out.

The set is what is remembered, not the count: a cell that unloads as another streams in with the same
number of lights is invisible to a count, and a room walked into and stopped in would stay dark until
the player moved another 256 units. Re-choosing is a sort over every spawned light, so it is not done
every frame.

## 7. The `--lights` flag

`EngineConfig::lights` (`crates/engine/src/config.rs`) is false by default and set by `--lights`.
`LightsPlugin` is registered for every run - it owns the budget, not the spawning - but
`streaming::spawn_cell` places a light only while the flag is on. A default run therefore renders
exactly as it did before this feature, and the acceptance, profiling and benchmark baselines that
`scripts/phase2-*.ps1` take do not move.

## 8. What is left for later

- **Tuning.** `LIGHT_EXPOSURE` is a documented round default; the exact value belongs with a
  comparison of rendered frames, as does any per-space ambient.
- **Skyrim's own attenuation.** The record's exponent column is published but not read, and Bevy's
  inverse-square curve is used instead of the game's own falloff curve.
- **Fade and flicker.** The `FNAM` fade, the `DATA` time field, flicker period and amplitude, the
  `FOV` cone and the near clip are all loaded by the converter and unused here: a converted torch
  burns steadily and lights in every direction.
- **Shadows.** Off for every converted light; a light therefore also lights the far side of the wall
  it is mounted on.
