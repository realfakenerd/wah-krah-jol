//! Point lights from Skyrim `LIGH` references: the torches, braziers, Dwemer lamps and glowing
//! fungus the game places around its spaces. `docs/specs/engine/point-lights.md` is the norm this
//! module implements.
//!
//! `streaming::spawn_cell` gives every reference whose base record has a row in the converter's
//! `lights` table a [`PointLight`] child, and only while the engine is run with `--lights`. The
//! light entity is a descendant of the cell root, so it follows the render-origin rebases and the
//! cell unloads that move the cell hierarchy, and it is despawned with the cell that placed it.
//!
//! This module owns the record-to-light conversion, the intensity scale, and the budget that bounds
//! how many lights are enabled at once.
//!
//! # The intensity scale: Creation units are not metres
//!
//! A `LIGH` row carries its radius in Creation units and this engine renders one Creation unit as
//! one Bevy world unit ([`CELL_SIZE`](crate::world::components::CELL_SIZE) is 4096 units across a
//! cell the game defines as 4096 Creation units), so `range` is the radius unchanged. Intensity is
//! what needs converting: Bevy's defaults are tuned for a metre-scale world, and in this world the
//! same distance is about 70 times larger, which loses a factor of 4900 to the inverse-square term.
//!
//! Bevy's point light contributes, at distance `d` from it,
//!
//! ```text
//! Lout = albedo * NdotL * (colour * intensity / (4*PI)) * window(d) / (PI * d^2)
//!      = albedo * NdotL * colour * intensity * window(d) / (4 * PI^2 * d^2)
//! window(d) = (1 - (d/range)^4)^2
//! ```
//!
//! The two `PI`s are easy to lose, and an earlier version of this derivation lost one of them.
//! `bevy_pbr/src/render/light.rs` divides a point light's intensity by `4*PI` on the CPU - Bevy's
//! `PointLight::intensity` is luminous power in lumens and the shader wants lumens per steradian
//! (`ExtractedPointLight`, `intensity: point_light.intensity / (4.0 * PI)`) - and `Fd_Burley` in
//! `bevy_pbr/src/render/pbr_lighting.wgsl` supplies the Lambert `1/PI`. Written out with only the
//! second one, as this module did, the light comes out `4*PI` too dim for the intensity asked for.
//!
//! The ambient light of `crate::app` contributes `albedo * ambient_colour * brightness` - no `NdotL`
//! and no `1/PI` (`bevy_pbr/src/render/light.rs`, `ambient_color: ... * ambient_light.brightness`,
//! and `pbr_ambient.wgsl`, `EnvBRDFApprox(diffuse_color, ..) * lights.ambient_color.rgb`). A
//! converted light is therefore given the intensity that makes the two equal at half its own
//! radius, times [`LIGHT_EXPOSURE`]:
//!
//! ```text
//! intensity = 4 * PI^2 * brightness * LIGHT_EXPOSURE * (radius/2)^2 / window(radius/2)
//! window(radius/2) = (1 - 1/16)^2 = 225/256
//! ```
//!
//! so `LIGHT_EXPOSURE` is the whole brightness knob for every converted light, in units of the
//! ambient the engine actually applies ([`AMBIENT_ILLUMINANCE`], the brightness the world path of
//! `crate::app` inserts into `GlobalAmbientLight`), and a 512-unit torch (a common `LIGH` radius)
//! gets about `4.7e9`. The light's own colour scales what a surface receives on top of that, as the
//! ambient's colour does on its side.
//!
//! # Why the reference distance stops at 256 units
//!
//! Sizing every light's intensity from its own `radius/2` reads the radius as brightness as well as
//! reach, and a `LIGH` radius is only reach: Skyrim's brightness scale is `FNAM` fade, and the
//! game's own attenuation is not inverse-square. One shipped record shows the difference at a
//! glance: a radius of 4334 is 71 times a 512-unit torch's reference radius, so a light sized from
//! its own radius came out 71 times a torch and washed out the space it hangs in - so the reference
//! distance is capped at [`INTENSITY_REFERENCE_RADIUS`]. Every radius up to twice that cap keeps
//! exactly the intensity the scale was written with, and a bigger light keeps its reach without a
//! brightness that grows with the square of it.
//!
//! # The budget
//!
//! [`budget_lights`] enables the [`ENABLED_LIGHT_BUDGET`] lights nearest the camera and hides the
//! rest. A whole hall of `LIGH` references is common, and Bevy's clustered forward renderer draws
//! every enabled light that reaches a cluster, so the far ones are switched off rather than paid
//! for.
//!
//! The choice is cached, and re-made when the camera has moved [`BUDGET_RECHOOSE_DISTANCE`], when a
//! light has spawned or despawned, or when the set of lights has changed in any other way - a cell
//! streaming in or out at an equal count ([`LightBudget::chosen`]). Keying the cache on the number
//! of spawned lights alone left a room walked into and stopped in dark until the player moved
//! another 256 units.
//!
//! # What is not done here
//!
//! A `LIGH` record's attenuation exponent, `FOV` and near clip are not applied: Bevy's point light
//! decays inverse-square and has neither a cone nor a near clip. Flicker, pulse, `FNAM` fade and
//! the `DATA` time field are not implemented either - a torch burns steadily. Shadows are off for
//! every converted light (`PointLight::shadow_maps_enabled` is one cube map per light, which a
//! 64-light budget cannot afford; the record's own shadow flags are left for later), so a light
//! also lights the far side of the wall it is mounted on. The reference's `XRDS` radius override *is* applied; see
//! [`radius_of`].

use crate::world::{components::StreamingCamera, database::LightRow};
use bevy::prelude::*;
use std::collections::HashSet;

/// `LIGH` `DATA` flag bit: the record is off until something turns it on, so a reference to it is
/// not lit by default (UESP, "Skyrim Mod:Mod File Format/LIGH").
///
/// The bit's meaning is UESP's, not measured: no `LIGH` record in the 506 of the shipped plugins
/// sets it (an unmodded install was scanned), so skipping these can never take a light out of the
/// game's own world.
pub const LIGHT_FLAG_OFF_BY_DEFAULT: u32 = 0x0000_0020;

/// `LIGH` `DATA` flag bit: the light removes light instead of adding it (UESP). Skyrim uses these
/// to darken a room; this engine has no way to subtract a clustered light, so they are skipped.
///
/// Like [`LIGHT_FLAG_OFF_BY_DEFAULT`], no shipped record sets it.
pub const LIGHT_FLAG_NEGATIVE: u32 = 0x0000_0004;

/// The ambient brightness the engine applies in its world path, in Bevy's ambient units: the
/// `brightness` the world startup of `crate::app` inserts beside its `GlobalAmbientLight` colour.
///
/// This is the illuminance a converted light is stated against, so the ratio
/// [`LIGHT_EXPOSURE`] names is the ratio a rendered surface really sees. A run that lights the
/// world differently - the fixture paths of `crate::app` each insert their own ambient - is not the
/// world this scale is written for, and the value has to be re-derived if per-space ambient lands.
pub const AMBIENT_ILLUMINANCE: f32 = 160.0;

/// How many times the ambient a converted light puts on a surface at half its own radius. This one
/// constant is the brightness knob for every converted light.
///
/// The delivery is per radius and not per record: a light delivers `LIGHT_EXPOSURE` times the
/// ambient at `radius/2`, falls to zero at `radius`, and is inverse-square in between, so the value
/// sets how bright a torch pool reads against the room around it. The intensity scale is chosen per
/// radius so that every `LIGH` record delivers the same surface brightness at half its own reach,
/// which is what makes one number usable for a 75-unit Dwarven lamp and a 3300-unit water light
/// alike.
///
/// Ten is a round default rather than a measured one: at half its radius a light delivers ten times
/// the ambient illuminance, an order of magnitude, so a lamp's pool reads clearly against the room.
/// It is not fitted to any image, and tuning it is a later pass - see
/// `docs/specs/engine/point-lights.md`.
pub const LIGHT_EXPOSURE: f32 = 10.0;

/// The largest reference distance [`intensity_for_radius`] sizes a light's intensity from: a light
/// with a radius up to `2 * INTENSITY_REFERENCE_RADIUS` is lit at its own `radius/2`, a bigger one
/// at this distance whatever its radius.
///
/// The cap is what keeps a radius from meaning brightness as well as reach (see the module
/// documentation). 512 units of radius is the widest common `LIGH` radius, so every light up to it
/// is byte-identical to the formula the scale was written with; a 4334-unit record drops from 71
/// times a torch to exactly one torch, at the same reach.
pub const INTENSITY_REFERENCE_RADIUS: f32 = 256.0;

/// The illuminance a converted light is tuned to deliver at half its own radius, in Bevy's ambient
/// units: [`LIGHT_EXPOSURE`] times the ambient the world path applies ([`AMBIENT_ILLUMINANCE`]).
const HALF_RADIUS_ILLUMINANCE: f32 =
    4.0 * core::f32::consts::PI * core::f32::consts::PI * AMBIENT_ILLUMINANCE * LIGHT_EXPOSURE;

/// Bevy's range window at half a light's range: `(1 - (d/range)^4)^2` at `d = range/2`
/// (`bevy_pbr/src/render/pbr_lighting.wgsl`, `getRangeFalloff`).
const HALF_RADIUS_WINDOW: f32 = 225.0 / 256.0;

/// How many of the spawned lights are enabled at once. Bevy's clustered forward renderer draws
/// every enabled light in a cluster it reaches, and the game's spaces hold whole halls of `LIGH`
/// references, so the far ones are switched off rather than paid for.
pub const ENABLED_LIGHT_BUDGET: usize = 64;

/// How far the camera moves before the enabled lights are chosen again. Re-choosing is a sort of
/// every spawned light, so it is not done per frame.
pub const BUDGET_RECHOOSE_DISTANCE: f32 = 256.0;

/// The Bevy light a `LIGH` row becomes, or `None` for a record the engine must not light the world
/// with: a negative light ([`LIGHT_FLAG_NEGATIVE`]), one that is off by default
/// ([`LIGHT_FLAG_OFF_BY_DEFAULT`]), or one whose effective radius is not a positive, finite number
/// of Creation units.
///
/// `radius_override` is the reference's `XRDS` radius when it carries one; see [`radius_of`].
///
/// The colour is the record's RGB in the byte order the converter read it - `DATA`'s colour bytes
/// as sRGB, the way the engine treats every other colour of the game.
pub fn point_light(light: &LightRow, radius_override: Option<f32>) -> Option<PointLight> {
    if light.flags & (LIGHT_FLAG_NEGATIVE | LIGHT_FLAG_OFF_BY_DEFAULT) != 0 {
        return None;
    }
    let radius = radius_of(light, radius_override)?;
    Some(PointLight {
        color: Color::srgb_u8(light.color[0], light.color[1], light.color[2]),
        intensity: intensity_for_radius(radius),
        range: radius,
        shadow_maps_enabled: false,
        ..default()
    })
}

/// The radius a reference lights its space with.
///
/// `LIGH` records share their radius, and a reference places the same light at wildly different
/// sizes: 10,810 of the 12,148 `LIGH` references in `Skyrim.esm` carry an `XRDS` radius of their
/// own, and one placed candle is 850.8 units where another is 147.7. The reference's override
/// therefore wins over the record, and the record's own radius is the fallback.
///
/// An override that is not a positive, finite number is not used: `XRDS` has been seen negative,
/// and a radius is a size, not a switch - the flags carry the light's on/off state - so a nonsense
/// override leaves the record's radius in place instead of leaving the room dark. Neither is one
/// above [`MAX_RADIUS_OVERRIDE`].
fn radius_of(light: &LightRow, radius_override: Option<f32>) -> Option<f32> {
    let usable = |radius: f32| (radius.is_finite() && radius > 0.0).then_some(radius);
    radius_override
        .filter(|&radius| radius <= MAX_RADIUS_OVERRIDE)
        .and_then(usable)
        .or_else(|| usable(light.radius))
}

/// The largest `XRDS` radius a reference may give its light, in Creation units: two exterior
/// cells. A larger override is ignored and the record's own radius is used.
///
/// Across the base game's and the official add-ons' plugins, 13,677 of 15,167 `LIGH` references
/// carry an override. The largest `LIGH` record radius is 2,000; the positive overrides run
/// smoothly up to 6,919, and then jump: the nine above that are 15,967, four of 25,074, 456,444,
/// two of 844,567 and 3,736,737. Taken as a light's range, those reach across many cells (the
/// largest across the whole map) and put the light into every cluster of the view, while its
/// brightness is capped anyway ([`INTENSITY_REFERENCE_RADIUS`]). 8,192 sits in the gap.
pub const MAX_RADIUS_OVERRIDE: f32 = 8_192.0;

/// The intensity a `LIGH` radius is lit with; see the module documentation for the derivation.
///
/// The reference distance is the light's own half radius up to [`INTENSITY_REFERENCE_RADIUS`] and
/// that cap above it, so a big light keeps its reach without a brightness that grows with the
/// square of the radius it is only meant to reach. Every radius up to twice the cap is unchanged.
pub fn intensity_for_radius(radius: f32) -> f32 {
    let reference = (radius * 0.5).min(INTENSITY_REFERENCE_RADIUS);
    HALF_RADIUS_ILLUMINANCE * reference * reference / HALF_RADIUS_WINDOW
}

/// A [`PointLight`] that came from a Skyrim `LIGH` reference.
///
/// The budget and the tests tell the game's lights from the engine's own by this marker: a
/// `PointLight` without it is never enabled or budgeted by [`budget_lights`], which keeps the
/// budget from touching a light a fixture or a future engine feature adds.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkyrimLight {
    /// The reference that carries the light (`REFR` form id).
    pub form_id: u32,
    /// The cell that reference belongs to.
    pub cell_id: u32,
}

/// The lights the budget chose last, and where it chose them.
#[derive(Resource, Default)]
struct LightBudget {
    /// The camera position the current selection was made at; `None` before the first frame.
    chosen_at: Option<Vec3>,
    /// Every spawned light when the current selection was made - the whole set the ranking ran
    /// over, not only the [`ENABLED_LIGHT_BUDGET`] it enabled.
    ///
    /// The selection is re-made whenever that set changes, even while the camera stands still and
    /// even when the number of lights in the world does not: a light spawning or despawning, and a
    /// cell streaming in or out, both move lights in or out of it. A cache keyed on the *count*
    /// alone kept a room walked into and stopped in dark until the player moved another
    /// [`BUDGET_RECHOOSE_DISTANCE`].
    chosen: HashSet<Entity>,
}

/// Keeps the number of enabled [`SkyrimLight`]s to [`ENABLED_LIGHT_BUDGET`] nearest the camera.
///
/// Add it after [`StreamingPlugin`](crate::streaming::StreamingPlugin).
pub struct LightsPlugin;

impl Plugin for LightsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LightBudget>()
            .add_systems(PostUpdate, budget_lights.after(TransformSystems::Propagate));
    }
}

/// Enables the [`ENABLED_LIGHT_BUDGET`] lights nearest the camera, and hides the rest.
///
/// Hidden is the switch: the lights are extracted to the render world only while they are visible
/// (`bevy_pbr/src/render/light.rs`), and a light set back to `Visibility::Inherited` still goes
/// dark whenever an ancestor is hidden. A light under a reference whose model failed validation is
/// hidden by that ancestor, not by this system.
///
/// This system owns the `Visibility` of a [`SkyrimLight`]. A light that another system hides and
/// this one re-enables - `streaming::hide_partial_scene`, which hides the descendants of a
/// reference whose model failed strict validation - comes back on: the budget cannot tell that
/// hiding from its own, and a light next to a missing model is not wrong. Hiding an *ancestor*
/// needs no such care, because `Visibility::Inherited` defers to it.
fn budget_lights(
    mut budget: ResMut<LightBudget>,
    camera: Query<&GlobalTransform, With<StreamingCamera>>,
    mut lights: Query<(Entity, &GlobalTransform, &PointLight, &mut Visibility), With<SkyrimLight>>,
) {
    let Ok(camera) = camera.single() else {
        return;
    };
    let camera_position = camera.translation();
    // One pass over every spawned light, allocating nothing: how many there are, and whether any of
    // them is not in the set the current selection was made from.
    //
    // The second half is what the count alone cannot answer. A swap that keeps the number of lights
    // in the world the same is invisible to a count - a cell unloading as another streams in with
    // as many lights - and the player standing still is the case the budget exists for: a room
    // walked into and stopped in has to light up.
    let mut spawned = 0usize;
    let mut joined = false;
    for (entity, _, _, _) in lights.iter() {
        spawned += 1;
        joined |= !budget.chosen.contains(&entity);
    }
    if !joined
        && let Some(chosen_at) = budget.chosen_at
        && budget.chosen.len() == spawned
        && chosen_at.distance(camera_position) <= BUDGET_RECHOOSE_DISTANCE
    {
        return;
    }

    let mut ranked: Vec<(Entity, f32)> = lights
        .iter()
        .map(|(entity, transform, _, _)| {
            (
                entity,
                transform.translation().distance_squared(camera_position),
            )
        })
        .collect();
    sort_by_distance(&mut ranked);
    let enabled: HashSet<Entity> = ranked
        .iter()
        .take(ENABLED_LIGHT_BUDGET)
        .map(|(entity, _)| *entity)
        .collect();

    for (entity, _, _, mut visibility) in &mut lights {
        let wanted = if enabled.contains(&entity) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    budget.chosen_at = Some(camera_position);
    budget.chosen = ranked.iter().map(|(entity, _)| *entity).collect();
    if let Some((nearest, distance_squared)) = ranked.first()
        && let Ok((_, _, light, _)) = lights.get(*nearest)
    {
        debug!(
            spawned,
            enabled = enabled.len(),
            nearest_distance = distance_squared.sqrt(),
            nearest_range = light.range,
            nearest_intensity = light.intensity,
            "lights: budget re-chosen"
        );
    }
}

/// Nearest first. The entity breaks ties, so two lights at the same distance are chosen between the
/// same way every frame they are re-ranked in.
fn sort_by_distance(ranked: &mut [(Entity, f32)]) {
    ranked.sort_by(|(left_entity, left), (right_entity, right)| {
        left.total_cmp(right)
            .then_with(|| left_entity.cmp(right_entity))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::components::CELL_SIZE;
    use bevy::{
        asset::AssetPlugin, camera::visibility::VisibilityPlugin, transform::TransformPlugin,
    };

    fn light_row(radius: f32, flags: u32) -> LightRow {
        LightRow {
            radius,
            color: [255, 200, 120],
            flags,
        }
    }

    /// Bevy's own point light attenuation, written out from the engine it runs in rather than from
    /// this module's derivation: `bevy_pbr/src/render/light.rs` divides a point light's intensity
    /// by `4*PI` (lumens to lumens per steradian) before it reaches the shader, and
    /// `Fd_Burley` in `bevy_pbr/src/render/pbr_lighting.wgsl` supplies the Lambert `1/PI`. The
    /// range window is `getRangeFalloff`'s.
    ///
    /// Both `PI`s belong here. This helper - and the derivation it restated - once carried only
    /// the Lambert one, which made the test pass while the lights delivered `4*PI` less than the
    /// constant they were aimed at.
    fn illuminance(light: &PointLight, distance: f32) -> f32 {
        let factor = distance * distance / (light.range * light.range);
        let window = (1.0 - factor * factor).max(0.0);
        light.intensity * window * window
            / (4.0 * core::f32::consts::PI * core::f32::consts::PI * distance * distance)
    }

    /// Relative luminance of a colour, the same Rec. 709 weights the pixels of a render are
    /// measured with: a light's colour scales its contribution linearly, so a warm light of a given
    /// intensity puts less luminance on a surface than a white one.
    fn luminance(color: Color) -> f32 {
        let linear = LinearRgba::from(color);
        0.2126 * linear.red + 0.7152 * linear.green + 0.0722 * linear.blue
    }

    /// The ambient of `crate::app` contributes `albedo * colour * brightness` to a surface; a
    /// converted light contributes what `illuminance` computes - already the light's colour times
    /// its intensity, since `illuminance` takes the colour out of the formula to keep the scale
    /// colour-free. A converted light has to put [`LIGHT_EXPOSURE`] times the ambient on a surface
    /// at half its radius, and that is the whole point of the intensity scale - so this is the test
    /// that catches a wrong formula.
    ///
    /// Above [`INTENSITY_REFERENCE_RADIUS`] that stops being true on purpose: a light bigger than
    /// twice the cap is lit like the largest calibrated one, so what reaches a surface at half its
    /// own reach falls off with the square of its radius. The reach is what a big radius buys.
    #[test]
    fn a_light_lights_a_surface_at_half_its_radius_like_the_ambient_does() {
        let wanted = LIGHT_EXPOSURE * AMBIENT_ILLUMINANCE;
        for radius in [128.0, 512.0] {
            let light = point_light(&light_row(radius, 0), None).unwrap();
            let from_light = illuminance(&light, radius * 0.5);
            assert!(
                (from_light - wanted).abs() < wanted * 1.0e-3,
                "a {radius}-unit light gives {from_light} at half its radius; {LIGHT_EXPOSURE} \
                 times the ambient brightness of the world path is {wanted}"
            );
        }
        // (512/radius)^2 of the 512-unit light, at that light's own half radius.
        let torch = illuminance(&point_light(&light_row(512.0, 0), None).unwrap(), 256.0);
        for radius in [1024.0, 4334.0] {
            let light = point_light(&light_row(radius, 0), None).unwrap();
            let from_light = illuminance(&light, radius * 0.5);
            let spread = (512.0 / radius).powi(2);
            assert!(
                (from_light - torch * spread).abs() < wanted * 1.0e-3,
                "a {radius}-unit light gives {from_light} at half its radius; a torch's own \
                 intensity spread over that reach gives {}",
                torch * spread
            );
        }
        assert!(
            (HALF_RADIUS_ILLUMINANCE
                - 4.0
                    * core::f32::consts::PI
                    * core::f32::consts::PI
                    * AMBIENT_ILLUMINANCE
                    * LIGHT_EXPOSURE)
                .abs()
                < 1.0,
            "the target illuminance is 4*PI^2 times the ambient the world path applies times \
             LIGHT_EXPOSURE, got {HALF_RADIUS_ILLUMINANCE}"
        );
    }

    /// The scale is per radius, so the surface brightness at half a light's radius does not depend
    /// on how big the record is: four times the radius is sixteen times the intensity.
    /// The light's own colour sits on the light's side of the comparison, exactly as the ambient's
    /// colour sits on its own: the intensity scale is colour-free, and a warm light of the same
    /// intensity puts less luminance on a surface than a white one of the same radius would.
    #[test]
    fn the_intensity_scale_ignores_the_lights_colour() {
        let warm = point_light(&light_row(512.0, 0), None).unwrap();
        let white = point_light(
            &LightRow {
                color: [255, 255, 255],
                ..light_row(512.0, 0)
            },
            None,
        )
        .unwrap();
        assert!(
            (warm.intensity - white.intensity).abs() < 1.0,
            "the scale is per radius, not per colour: {} and {}",
            warm.intensity,
            white.intensity
        );
        assert!(
            luminance(warm.color) < luminance(white.color),
            "and the warm light is the dimmer of the two on a surface"
        );
    }

    /// The inverse-square law, from the outside: twice the radius is four times the intensity, so a
    /// surface at half of either radius receives the same illuminance. It stops at the cap.
    #[test]
    fn intensity_follows_the_square_of_the_radius() {
        let quarter = intensity_for_radius(256.0);
        let half = intensity_for_radius(512.0);
        assert!((half / quarter - 4.0).abs() < 1.0e-4, "{quarter} -> {half}");
        assert!(
            intensity_for_radius(512.0) > 1.0e7,
            "a metre-scale intensity would not reach across a 512-unit room: {}",
            intensity_for_radius(512.0)
        );
        // And it stops at the cap: twice the radius is no longer four times the intensity.
        assert_eq!(intensity_for_radius(1024.0), intensity_for_radius(512.0));
    }

    /// The cap has to be invisible to every light up to the widest common radius: at or below
    /// `2 * INTENSITY_REFERENCE_RADIUS` the intensity is exactly the uncapped formula, to the bit,
    /// so no torch, lamp or brazier moves. Above it the light keeps the intensity of the largest
    /// uncapped one.
    #[test]
    fn the_reference_cap_leaves_every_radius_up_to_512_exactly_as_it_was() {
        let uncapped = |radius: f32| {
            let half_radius = radius * 0.5;
            HALF_RADIUS_ILLUMINANCE * half_radius * half_radius / HALF_RADIUS_WINDOW
        };
        for radius in [0.5, 64.0, 128.0, 147.7, 256.0, 330.0, 511.0, 512.0] {
            assert_eq!(
                intensity_for_radius(radius),
                uncapped(radius),
                "a {radius}-unit light is byte-identical to the uncapped formula"
            );
        }
        assert_eq!(
            INTENSITY_REFERENCE_RADIUS * 2.0,
            512.0,
            "the cap is half of the widest common radius, which is what makes the line above cover \
             every light that ever moved"
        );
        for radius in [512.5, 1024.0, 3300.0, 4334.0] {
            assert_eq!(
                intensity_for_radius(radius),
                intensity_for_radius(512.0),
                "a {radius}-unit light is lit like the largest uncapped one"
            );
            assert!(intensity_for_radius(radius) < uncapped(radius));
        }
    }

    /// The light the cap exists for. `000D9051` `FalmerCityLight02NS` is Blackreach's one huge
    /// light - radius 4334 at (2122,9014,4668), colour (216,128,39) - and with the intensity sized
    /// from its own radius it comes out 71 times a 512-unit torch, which washes out the cavern
    /// ceiling. It keeps its reach, and its colour is untouched: what it loses is the 71x.
    #[test]
    fn the_biggest_shipped_light_keeps_its_reach_without_the_brightness() {
        let falmer_city_light = LightRow {
            radius: 4334.0,
            color: [216, 128, 39],
            flags: 0,
        };
        let big = point_light(&falmer_city_light, None).unwrap();
        let torch = point_light(&light_row(512.0, 0), None).unwrap();
        assert_eq!(big.range, 4334.0, "the record's reach is unchanged");
        assert_eq!(
            big.intensity, torch.intensity,
            "and it is lit at a torch's intensity, not 71 of them"
        );
        assert!(
            big.range > torch.range * 8.0,
            "while still reaching eight times further than a torch: {}",
            big.range
        );
        assert_eq!(
            big.color,
            Color::srgb_u8(216, 128, 39),
            "its colour is the record's, orange as it is: what overwhelmed the space was the \
             intensity, not the colour"
        );
    }

    #[test]
    fn a_torch_becomes_a_range_coloured_unshadowed_light() {
        let light = point_light(&light_row(512.0, 0), None).unwrap();
        assert_eq!(light.range, 512.0);
        assert_eq!(light.color, Color::srgb_u8(255, 200, 120));
        assert!(!light.shadow_maps_enabled);
        assert_eq!(light.radius, 0.0, "no area, so no oversized specular");
        // A cell is 4096 Creation units across, about 58 metres: the range is in those units.
        assert!((CELL_SIZE / 512.0 - 8.0).abs() < 1.0e-3);
        // A metre-scale intensity would not reach across a 512-unit room.
        assert!(
            (light.intensity - intensity_for_radius(512.0)).abs() < 1.0,
            "{}",
            light.intensity
        );
    }

    #[test]
    fn negative_and_off_by_default_lights_spawn_nothing() {
        assert!(point_light(&light_row(512.0, LIGHT_FLAG_NEGATIVE), None).is_none());
        assert!(point_light(&light_row(512.0, LIGHT_FLAG_OFF_BY_DEFAULT), None).is_none());
        assert!(point_light(&light_row(512.0, 0x0001 | 0x0008), None).is_some());
        assert!(
            point_light(&light_row(0.0, 0), None).is_none(),
            "a light with no radius lights nothing"
        );
        assert!(point_light(&light_row(f32::NAN, 0), None).is_none());
        assert!(point_light(&light_row(-64.0, 0), None).is_none());
        assert!(
            point_light(&light_row(0.0, 0), Some(512.0)).is_some(),
            "an override rescues a record whose own radius is unusable"
        );
    }

    /// The reference's `XRDS` radius replaces the record's, and the intensity scale has to follow
    /// it: most `LIGH` references carry one (10,810 of the 12,148 in `Skyrim.esm`), and using the
    /// record default would light them at the wrong size.
    #[test]
    fn a_reference_radius_override_replaces_the_record_radius() {
        let record = light_row(256.0, 0);
        let overridden = point_light(&record, Some(850.8)).unwrap();
        assert_eq!(overridden.range, 850.8);
        assert!(
            (overridden.intensity / intensity_for_radius(850.8) - 1.0).abs() < 1.0e-5,
            "the intensity follows the radius that is used: {}",
            overridden.intensity
        );

        // A nonsense override is not a switch: the record's own radius stands.
        for override_radius in [-100.0, 0.0, f32::NAN, f32::INFINITY] {
            let light = point_light(&record, Some(override_radius)).unwrap();
            assert_eq!(light.range, 256.0, "override {override_radius}");
        }
    }

    /// The outliers the base game carries (15,967 up to 3,736,737 units) fall back to the record's
    /// radius; the largest ordinary override (6,919) and the cap itself are kept.
    #[test]
    fn an_override_above_the_cap_is_ignored() {
        let record = light_row(256.0, 0);
        for kept in [6_919.0, MAX_RADIUS_OVERRIDE] {
            assert_eq!(point_light(&record, Some(kept)).unwrap().range, kept);
        }
        for outlier in [15_967.0, 25_074.0, 3_736_737.5] {
            assert_eq!(
                point_light(&record, Some(outlier)).unwrap().range,
                256.0,
                "override {outlier}"
            );
        }
    }

    /// A light budgeted around a camera, as `budget_lights` sees them.
    fn budget_app(camera: Vec3, light_positions: impl IntoIterator<Item = Vec3>) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .init_asset::<Mesh>()
        .init_resource::<LightBudget>()
        .add_systems(PostUpdate, budget_lights.after(TransformSystems::Propagate));
        app.world_mut().spawn((
            Transform::from_translation(camera),
            GlobalTransform::from_translation(camera),
            StreamingCamera,
        ));
        for position in light_positions {
            app.world_mut().spawn((
                SkyrimLight {
                    form_id: 1,
                    cell_id: 2,
                },
                // The real bundle: `PointLight` is what gives a light its `Transform`, its
                // `Visibility` and its `GlobalTransform`.
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::from_translation(position),
            ));
        }
        app.update();
        app
    }

    fn enabled_lights(app: &mut App) -> Vec<Entity> {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, (With<SkyrimLight>, With<Visibility>)>();
        query
            .iter(app.world())
            .filter(|entity| {
                *app.world().entity(*entity).get::<Visibility>().unwrap() == Visibility::Inherited
            })
            .collect()
    }

    fn move_camera(app: &mut App, camera: Vec3) {
        let mut query = app
            .world_mut()
            .query_filtered::<&mut Transform, With<StreamingCamera>>();
        for mut transform in query.iter_mut(app.world_mut()) {
            transform.translation = camera;
        }
        app.update();
    }

    /// Hides nothing and enables nothing else: a `PointLight` the engine's own code spawns - a
    /// fixture, a future camera lantern - is never in the ranking.
    #[test]
    fn the_budget_leaves_a_point_light_that_is_not_a_skyrim_light_alone() {
        let mut app = budget_app(
            Vec3::ZERO,
            (0..ENABLED_LIGHT_BUDGET + 4).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );
        let ordinary = app
            .world_mut()
            .spawn((
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::from_translation(Vec3::new(1_000.0, 0.0, 0.0)),
            ))
            .id();
        app.update();
        assert_eq!(
            *app.world().entity(ordinary).get::<Visibility>().unwrap(),
            Visibility::Inherited,
            "a plain `PointLight` is not the budget's to switch"
        );
        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);
    }

    /// The budget is the 64 nearest and no more: the far ones are hidden, nearest first.
    #[test]
    fn the_budget_enables_the_64_nearest_lights() {
        let mut app = budget_app(
            Vec3::ZERO,
            (0..ENABLED_LIGHT_BUDGET + 16).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );

        let enabled = enabled_lights(&mut app);
        assert_eq!(
            enabled.len(),
            ENABLED_LIGHT_BUDGET,
            "the budget is 64 lights, not one per spawned reference"
        );
        let mut query = app
            .world_mut()
            .query_filtered::<(Entity, &Transform), With<SkyrimLight>>();
        let mut hidden: Vec<f32> = query
            .iter(app.world())
            .filter(|(entity, _)| !enabled.contains(entity))
            .map(|(_, transform)| transform.translation.x)
            .collect();
        hidden.sort_by(f32::total_cmp);
        assert_eq!(
            hidden.len(),
            16,
            "the sixteen farthest lights are the ones switched off"
        );
        assert!(
            hidden[0] >= 1000.0 + ENABLED_LIGHT_BUDGET as f32,
            "and no nearer light was switched off for a farther one: {hidden:?}"
        );

        // Moving the camera past `BUDGET_RECHOOSE_DISTANCE` re-chooses: the lights around its new
        // position come on and the ones left behind go off.
        move_camera(&mut app, Vec3::new(2000.0, 0.0, 0.0));
        let mut query = app
            .world_mut()
            .query_filtered::<(Entity, &Transform), With<SkyrimLight>>();
        let enabled = enabled_lights(&mut app);
        assert_eq!(enabled.len(), ENABLED_LIGHT_BUDGET);
        assert!(
            query
                .iter(app.world())
                .filter(|(entity, transform)| transform.translation.x < 1100.0
                    && !enabled.contains(entity))
                .count()
                > 0,
            "the lights by the camera's old position are switched off"
        );
    }

    /// The set-change re-choose, which a cache keyed on the count of lights cannot see: one cell
    /// streams out and another in with the same number of lights while the camera stands still, and
    /// the new lights have to be ranked.
    ///
    /// A cache that keyed on the count alone would leave the new light visible without hiding
    /// anything - 65 enabled lights - which is what the first assertion catches.
    #[test]
    fn the_budget_re_chooses_when_the_light_set_changes_without_the_camera_moving() {
        let mut app = budget_app(
            Vec3::ZERO,
            (0..ENABLED_LIGHT_BUDGET + 16).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );
        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);

        // One light of a distant cell goes away and one of a cell next to the camera arrives: the
        // same count, and the camera has not moved a unit.
        let gone = {
            let mut query = app
                .world_mut()
                .query_filtered::<(Entity, &Transform), With<SkyrimLight>>();
            query
                .iter(app.world())
                .max_by(|(_, left), (_, right)| left.translation.x.total_cmp(&right.translation.x))
                .map(|(entity, _)| entity)
                .unwrap()
        };
        let arrived = app
            .world_mut()
            .spawn((
                SkyrimLight {
                    form_id: 3,
                    cell_id: 4,
                },
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
            ))
            .id();
        app.world_mut().entity_mut(gone).despawn();
        app.update();

        let enabled = enabled_lights(&mut app);
        assert_eq!(
            enabled.len(),
            ENABLED_LIGHT_BUDGET,
            "a light that arrives while the camera stands still is ranked like any other"
        );
        assert!(
            enabled.contains(&arrived),
            "and the nearest light of all is one of them"
        );
        let mut query = app
            .world_mut()
            .query_filtered::<(Entity, &Transform), With<SkyrimLight>>();
        let enabled_far = query
            .iter(app.world())
            .filter(|(entity, transform)| {
                enabled.contains(entity) && transform.translation.x >= 1_000.0
            })
            .count();
        assert_eq!(
            enabled_far,
            ENABLED_LIGHT_BUDGET - 1,
            "the light the newcomer displaced is the farthest one"
        );
    }
}
