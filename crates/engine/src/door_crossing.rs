//! Going through a load door: look at one, press E, fade to black, arrive at the other side.
//!
//! One state machine per crossing, with a single crossing at a time:
//!
//! 1. **Fade out** (0.25 s) under a full-screen black UI node.
//! 2. At full black the space switches in one step: every cell of the space left behind unloads at
//!    once (unpaced), the [`ActiveSpace`] and `CameraSpace` change, an exterior landing sets the
//!    render origin to the arrival's grid square (an interior landing leaves it alone), and the
//!    player is teleported to the door's `XTEL` arrival point.
//! 3. **Hold black** until the landing cell is resident, nothing under its root is pending (models, surfaces) and
//!    every mesh, material and texture under the cell's root has finished loading, then two more
//!    frames so render extraction and preparation run, for at most 10 s per landing. While black,
//!    the per-frame upload budget is lifted (a stutter behind black is invisible) and the configured
//!    value comes back when the fade-in starts. A landing cell that fails to load puts the player
//!    back where they were, with a warning. A destination timeout also returns to the saved space.
//!    If that return fails or times out, the screen fades in but movement stays paused until the
//!    player presses V or uses `tcl` to recover in noclip.
//!    WALK also waits for safe capsule placement and grounded, motionless controller output.
//! 4. **Fade in** (0.25 s). WALK keeps its settled pose; NOCLIP is placed on the landing point again.
//!
//! The player's walking and noclip systems pause for the whole crossing ([`no_door_crossing`]): nothing under
//! the landing point has collision until its cell is ready, so a WALK body would fall (and WASD
//! would move it) while the screen is black.
//! Only input-free WALK physics runs once the landing assets have settled, still behind black.
//!
//! There is no preload: the fade simply holds until the destination is resident and uploaded.

use crate::{
    doors::LoadDoor,
    nif_material::{EffectPaletteDepthSource, EffectPaletteMaterial},
    physics::{
        CursorCapture, MoveMode, MovementTuning, PlayerBody, TeleportPlayer, WalkSpawnHold,
        WalkState, body_and_camera_for_feet,
    },
    render::{TerrainMaterial, WaterMaterial},
    sky::CameraSpace,
    streaming::{
        ActiveSpace, PendingUnder, RenderOrigin, StreamingWorld, creation_rotation_to_bevy,
        creation_to_bevy, streaming_center, unload_all_cells_now,
    },
    world::{
        components::{CELL_SIZE, ExpectedModelBounds, StreamingCamera, WorldPosition},
        database::CellKey,
    },
};
use bevy::{
    asset::{LoadState, UntypedAssetId},
    ecs::system::SystemParam,
    prelude::*,
    render::render_asset::RenderAssetBytesPerFrame,
};
use bevy_rapier3d::prelude::{
    KinematicCharacterControllerOutput, QueryFilter, ReadRapierContext, RigidBodyDisabled,
};
use std::collections::HashSet;

/// How far from the camera a door can be and still be used, in Creation units.
pub const DOOR_REACH: f32 = 200.0;
/// The box a load door is activated through when its reference carries no model bounds, in the
/// door's local space: a door-sized box centred on the origin horizontally (160 wide and deep, so
/// a door hinged at the origin is still hit across its width) that starts at the floor and reaches
/// 200 up, a person's height and a little more.
pub const FALLBACK_DOOR_MIN: Vec3 = Vec3::new(-80.0, 0.0, -80.0);
pub const FALLBACK_DOOR_MAX: Vec3 = Vec3::new(80.0, 200.0, 80.0);
/// How much nearer than a door's bounds a collider may be before it counts as blocking the view
/// of the door, in Creation units: the door's own collider and its frame sit at the bounds.
pub const DOOR_BLOCKER_SLACK: f32 = 15.0;
/// Seconds to fade out, and to fade in.
pub const FADE_SECONDS: f32 = 0.25;
/// Longest the screen stays black waiting for the destination.
pub const LANDING_TIMEOUT_SECONDS: f32 = 10.0;
/// Frames to keep waiting once everything under the landing cell has loaded.
const SETTLE_FRAMES: u32 = 2;

pub struct DoorCrossingPlugin;

/// Door transitions and their queued scene switches must precede scene environment consumers.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DoorCrossingSet;

impl Plugin for DoorCrossingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DoorCrossing>()
            .init_resource::<CameraSpace>()
            .add_message::<TeleportPlayer>()
            .add_systems(Startup, spawn_fade_overlay)
            // After the streaming chain in `Update`, so the pending counts are this frame's.
            .add_systems(PostUpdate, drive_door_crossing.in_set(DoorCrossingSet));
    }
}

/// The full-screen black node the crossing fades.
#[derive(Component)]
pub struct FadeOverlay;

/// Where a crossing puts the player.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landing {
    space: ActiveSpace,
    /// The cell whose residency ends the black hold.
    key: CellKey,
    /// `Some(grid)` for an exterior landing; an interior landing leaves the origin alone.
    origin: Option<IVec2>,
    camera_space: CameraSpace,
    /// FEET position in render space (relative to `origin`, or to the unchanged origin), as
    /// [`TeleportPlayer`] takes it.
    position: Vec3,
    yaw: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    FadeOut {
        elapsed: f32,
    },
    /// Black; the switch is done and the landing cell is loading.
    Landing {
        waited: f32,
        restoring: bool,
        /// Consecutive frames the landing cell has been fully loaded; the fade-in starts after
        /// [`SETTLE_FRAMES`] more, so render extraction runs with the budget still lifted.
        ready_frames: u32,
    },
    FadeIn {
        elapsed: f32,
        recovery: bool,
    },
    Recovery,
}

/// The player's own place before a crossing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Restore {
    space: ActiveSpace,
    camera_space: CameraSpace,
    configured_worldspace: u32,
    /// Feet position. For an exterior start: absolute render-space coordinates (the render
    /// position plus the render origin's offset), so independent of whichever origin is in force.
    /// For an interior start: the raw render position, interiors being placed absolutely with the
    /// origin unused.
    feet: Vec3,
    yaw: f32,
}

/// The absolute feet position for render-space `feet` under `origin`.
fn absolute_feet(feet: Vec3, origin: IVec2) -> Vec3 {
    feet + Vec3::new(
        origin.x as f32 * CELL_SIZE,
        0.0,
        -(origin.y as f32) * CELL_SIZE,
    )
}

impl Restore {
    fn landing(&self) -> Landing {
        match self.space.interior {
            Some(cell_id) => Landing {
                space: self.space,
                key: CellKey::Interior(cell_id),
                origin: None,
                camera_space: self.camera_space,
                position: self.feet,
                yaw: self.yaw,
            },
            None => {
                let grid = streaming_center(self.feet, IVec2::ZERO);
                Landing {
                    space: self.space,
                    key: CellKey::Exterior {
                        worldspace_id: self.space.exterior_worldspace(self.configured_worldspace),
                        grid_x: grid.x,
                        grid_y: grid.y,
                    },
                    origin: Some(grid),
                    camera_space: self.camera_space,
                    position: absolute_feet(self.feet, -grid),
                    yaw: self.yaw,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Crossing {
    door: LoadDoor,
    target: Landing,
    /// Where the player was, to put them back if the target cannot load. Kept in world terms (the
    /// absolute feet position, see [`absolute_feet`]) so a render-origin rebase before the switch
    /// cannot move it; it becomes a [`Landing`] only when it is used.
    restore: Restore,
    /// Seconds since E was pressed.
    since_press: f32,
    stage: Stage,
}

/// The crossing in progress, if any.
#[derive(Resource, Default, Debug)]
pub struct DoorCrossing {
    active: Option<Crossing>,
}

impl DoorCrossing {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Permit placement and gravity only after the destination's assets have settled behind black.
    fn settling_walk(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            matches!(active.stage, Stage::Landing { ready_frames, .. } if ready_frames >= SETTLE_FRAMES)
        })
    }

    /// Whether the failed return is visible and the player can explicitly recover in noclip.
    pub(crate) fn needs_recovery(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| matches!(active.stage, Stage::Recovery))
    }

    /// Release the movement guard after the caller has disabled the walking body.
    pub(crate) fn finish_recovery(&mut self) {
        debug_assert!(self.needs_recovery());
        self.active = None;
    }
}

/// Run condition for the player's movement systems: true unless a door crossing is in progress (a
/// run without the crossing plugin never has one).
pub fn no_door_crossing(crossing: Option<Res<DoorCrossing>>) -> bool {
    crossing.is_none_or(|crossing| !crossing.is_active())
}

/// Input stays guarded for the whole crossing; placement and gravity may settle behind black.
pub(crate) fn walk_physics_allowed(crossing: Option<Res<DoorCrossing>>) -> bool {
    crossing.is_none_or(|crossing| !crossing.is_active() || crossing.settling_walk())
}

/// Actual controller support, rather than render residency, completes a WALK landing.
#[derive(SystemParam)]
struct WalkLanding<'w, 's> {
    mode: Option<Res<'w, MoveMode>>,
    hold: Option<Res<'w, WalkSpawnHold>>,
    player: WalkLandingPlayerQuery<'w, 's>,
}

type WalkLandingPlayerQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static WalkState,
        &'static KinematicCharacterControllerOutput,
    ),
    (With<PlayerBody>, Without<RigidBodyDisabled>),
>;

impl WalkLanding<'_, '_> {
    fn walking(&self) -> bool {
        self.mode.as_deref() == Some(&MoveMode::Walk)
    }

    fn ready(&self) -> bool {
        if !self.walking() {
            return true;
        }
        self.hold.as_ref().is_some_and(|hold| !hold.is_active())
            && self.player.single().is_ok_and(|(state, output)| {
                state.grounded
                    && state.vertical_velocity == 0.0
                    && state.horizontal_velocity == Vec3::ZERO
                    && output.grounded
                    && output.effective_translation.length_squared() < 1e-6
            })
    }
}

#[cfg(test)]
impl DoorCrossing {
    pub(crate) fn recovery_for_test() -> Self {
        let mut crossing = Self::holding_for_test();
        crossing.active.as_mut().unwrap().stage = Stage::Recovery;
        crossing
    }

    /// A crossing held black on its landing, for tests of systems that must pause during one.
    pub(crate) fn holding_for_test() -> Self {
        let door = LoadDoor {
            ref_id: 1,
            destination: crate::doors::DoorDestination {
                destination_ref_id: 2,
                interior_cell_id: Some(3),
                worldspace_id: None,
                arrival_position: [0.0; 3],
                arrival_rotation: [0.0; 3],
            },
        };
        let space = ActiveSpace::default();
        let target = landing_for(&door, &space, 0);
        Self {
            active: Some(Crossing {
                door,
                target,
                restore: Restore {
                    space,
                    camera_space: CameraSpace::default(),
                    configured_worldspace: 0,
                    feet: Vec3::ZERO,
                    yaw: 0.0,
                },
                since_press: 0.0,
                stage: Stage::Landing {
                    waited: 0.0,
                    restoring: false,
                    ready_frames: 0,
                },
            }),
        }
    }
}

fn spawn_fade_overlay(mut commands: Commands) {
    commands.spawn((
        Name::new("Door fade"),
        FadeOverlay,
        Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.0)),
        GlobalZIndex(i32::MAX - 1),
    ));
}

fn fallback_bounds() -> ExpectedModelBounds {
    ExpectedModelBounds {
        min: FALLBACK_DOOR_MIN,
        max: FALLBACK_DOOR_MAX,
    }
}

/// The destination as the crossing's INFO line prints it: the interior cell id, or the
/// worldspace and grid square.
fn describe_destination(key: &CellKey) -> String {
    match key {
        CellKey::Interior(cell_id) => format!("interior cell {cell_id}"),
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => format!("worldspace {worldspace_id} grid ({grid_x}, {grid_y})"),
    }
}

/// The distance along a ray (unit `direction`) to a door's model bounds, if within `reach`.
///
/// The bounds are the converter's model-space box; the ray is taken into the door's local space
/// so a rotated or scaled door is tested as the oriented box it is.
fn ray_hits_door(
    origin: Vec3,
    direction: Vec3,
    door: &GlobalTransform,
    bounds: &ExpectedModelBounds,
    reach: f32,
) -> Option<f32> {
    let inverse = door.affine().inverse();
    // A reference whose `XSCL` is zero, infinite or NaN has a singular affine, whose inverse is
    // NaN/infinite. The slab test below would then report a hit at distance 0 from any origin and
    // direction, because `f32::min`/`max` drop a NaN argument; glam's `try_inverse` does not help,
    // as it returns `Some(NaN)` when the determinant is NaN. Such a door is not activatable.
    if !inverse.is_finite() {
        return None;
    }
    let local_origin = inverse.transform_point3(origin);
    // Not renormalised, so the ray parameter stays a world distance.
    let local_direction = inverse.transform_vector3(direction);
    let mut near = 0.0f32;
    let mut far = reach;
    for axis in 0..3 {
        let (start, step) = (local_origin[axis], local_direction[axis]);
        if step.abs() < 1e-9 {
            if start < bounds.min[axis] || start > bounds.max[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = (
            (bounds.min[axis] - start) / step,
            (bounds.max[axis] - start) / step,
        );
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    Some(near)
}

/// Whether a collider the camera ray met first hides a door `door_distance` away: it must be
/// nearer than the door by more than [`DOOR_BLOCKER_SLACK`]. No collider (`None`) never blocks.
fn door_view_blocked(nearest_collider: Option<f32>, door_distance: f32) -> bool {
    nearest_collider.is_some_and(|hit| hit < door_distance - DOOR_BLOCKER_SLACK)
}

/// The landing a door's destination describes, from the space the player is in now.
fn landing_for(door: &LoadDoor, current: &ActiveSpace, configured_worldspace: u32) -> Landing {
    let destination = &door.destination;
    let creation = Vec3::from_array(destination.arrival_position);
    let yaw = creation_rotation_to_bevy(destination.arrival_rotation)
        .to_euler(EulerRot::YXZ)
        .0;
    if let Some(cell_id) = destination.interior_cell_id {
        // Interiors are placed in absolute coordinates and the origin stays as it is.
        return Landing {
            space: ActiveSpace {
                worldspace_id: Some(current.exterior_worldspace(configured_worldspace)),
                interior: Some(cell_id),
            },
            key: CellKey::Interior(cell_id),
            origin: None,
            camera_space: CameraSpace::Interior,
            position: creation_to_bevy(creation),
            yaw,
        };
    }
    let worldspace_id = destination
        .worldspace_id
        .unwrap_or_else(|| current.exterior_worldspace(configured_worldspace));
    let at = WorldPosition::from_creation_units(creation);
    Landing {
        space: ActiveSpace {
            worldspace_id: Some(worldspace_id),
            interior: None,
        },
        key: CellKey::Exterior {
            worldspace_id,
            grid_x: at.grid.x,
            grid_y: at.grid.y,
        },
        origin: Some(at.grid),
        camera_space: CameraSpace::Exterior,
        position: creation_to_bevy(at.relative_to(at.grid)),
        yaw,
    }
}

/// Switches the world to `landing` in one step. Runs as a command, so it applies before the next
/// system that reads the cells and the old space is gone the same frame.
fn switch_space(world: &mut World, landing: Landing) {
    unload_all_cells_now(world);
    *world.resource_mut::<ActiveSpace>() = landing.space;
    if let Some(grid) = landing.origin {
        world.resource_mut::<RenderOrigin>().0 = grid;
    }
    *world.resource_mut::<CameraSpace>() = landing.camera_space;
    let (_, eye) = body_and_camera_for_feet(world.resource::<MovementTuning>(), landing.position);
    // Move the camera now as well as through the message: the planner runs before the teleport is
    // applied and must already see the player in the new space.
    let mut cameras = world.query_filtered::<&mut Transform, With<StreamingCamera>>();
    for mut camera in cameras.iter_mut(world) {
        camera.translation = eye;
    }
    world.write_message(TeleportPlayer {
        position: landing.position,
        sampled_ground_y: None,
        yaw: landing.yaw,
    });
}

/// Places the player on `landing` again as the fade-in starts. The walking systems paused during
/// the crossing, so this only undoes anything else that moved the body or the camera while black.
fn place_again(commands: &mut Commands, landing: &Landing) {
    let landing = *landing;
    commands.queue(move |world: &mut World| {
        // A saved exterior pose belongs to its landing origin, which may have changed during
        // the hold. Interiors use absolute coordinates and do not follow the render origin.
        let position = match landing.origin {
            Some(saved) => {
                absolute_feet(landing.position, saved - world.resource::<RenderOrigin>().0)
            }
            None => landing.position,
        };
        world.write_message(TeleportPlayer {
            position,
            sampled_ground_y: None,
            yaw: landing.yaw,
        });
    });
}

/// What a landing cell is made of, for the readiness check.
#[derive(SystemParam)]
#[allow(clippy::type_complexity)]
struct LandingAssets<'w, 's> {
    server: Option<Res<'w, AssetServer>>,
    children: Query<'w, 's, &'static Children>,
    parts: Query<
        'w,
        's,
        (
            Option<&'static Mesh3d>,
            Option<&'static MeshMaterial3d<StandardMaterial>>,
            Option<&'static MeshMaterial3d<TerrainMaterial>>,
            Option<&'static MeshMaterial3d<WaterMaterial>>,
            Option<&'static MeshMaterial3d<EffectPaletteMaterial>>,
            Option<&'static EffectPaletteDepthSource>,
        ),
    >,
    meshes: Option<Res<'w, Assets<Mesh>>>,
    images: Option<Res<'w, Assets<Image>>>,
    standard: Option<Res<'w, Assets<StandardMaterial>>>,
    terrain: Option<Res<'w, Assets<TerrainMaterial>>>,
    water: Option<Res<'w, Assets<WaterMaterial>>>,
    palette: Option<Res<'w, Assets<EffectPaletteMaterial>>>,
}

/// What the readiness check found under a landing root.
#[derive(Debug, Default)]
struct LandingReport {
    /// Assets still loading.
    waiting: usize,
    /// The first of them, by asset path when the server knows one.
    first_waiting: Option<String>,
    /// Assets whose load failed. They count as done, so a missing texture cannot hold the screen.
    failed: HashSet<UntypedAssetId>,
}

impl LandingAssets<'_, '_> {
    /// Notes an asset that is still loading, or failed, in `report`. A finished asset is loaded,
    /// or (no load tracked, as for an asset added directly) present in its store.
    fn check(
        &self,
        id: UntypedAssetId,
        present: impl FnOnce() -> bool,
        report: &mut LandingReport,
    ) {
        let Some(server) = &self.server else {
            return;
        };
        let waiting = match server.get_load_state(id) {
            Some(LoadState::Loaded) => false,
            Some(LoadState::Failed(_)) => {
                report.failed.insert(id);
                false
            }
            Some(_) => true,
            None => !present(),
        };
        if waiting {
            report.waiting += 1;
            if report.first_waiting.is_none() {
                report.first_waiting = Some(
                    server
                        .get_path(id)
                        .map_or_else(|| format!("{id:?}"), |path| path.to_string()),
                );
            }
        }
    }

    fn check_image(&self, id: UntypedAssetId, report: &mut LandingReport) {
        self.check(
            id,
            || match (&self.images, id.try_typed::<Image>()) {
                (Some(images), Ok(typed)) => images.contains(typed),
                _ => true,
            },
            report,
        );
    }

    /// Checks every dependency (the textures) of a material.
    fn check_dependencies<A: Asset>(&self, material: &A, report: &mut LandingReport) {
        material.visit_dependencies(&mut |id| self.check_image(id, report));
    }

    /// Checks a material handle and its textures.
    fn check_material<A: Asset>(
        &self,
        handle: &Handle<A>,
        store: &Option<Res<Assets<A>>>,
        textures_of: impl FnOnce(&Self, &A, &mut LandingReport),
        report: &mut LandingReport,
    ) {
        let present = || {
            store
                .as_ref()
                .is_none_or(|assets| assets.contains(handle.id()))
        };
        let before = report.waiting;
        self.check(handle.id().untyped(), present, report);
        if report.waiting != before {
            return;
        }
        if let Some(material) = store.as_ref().and_then(|assets| assets.get(handle.id())) {
            textures_of(self, material, report);
        }
    }

    /// Inspects every mesh, material and texture under `root`. Covered: `Mesh3d`,
    /// `MeshMaterial3d<StandardMaterial>` and the effect palette material (directly or as the
    /// source of its depth wrapper) with all their texture dependencies, and the terrain and water materials with their base-material textures. The
    /// terrain and water extensions' own layer and reflection images are private to `render.rs`
    /// and are not checked.
    fn inspect_under(&self, root: Entity) -> LandingReport {
        let mut report = LandingReport::default();
        for entity in std::iter::once(root).chain(self.children.iter_descendants(root)) {
            let Ok((mesh, standard, terrain, water, palette, palette_depth)) =
                self.parts.get(entity)
            else {
                continue;
            };
            if let Some(Mesh3d(handle)) = mesh {
                self.check(
                    handle.id().untyped(),
                    || {
                        self.meshes
                            .as_ref()
                            .is_none_or(|meshes| meshes.contains(handle.id()))
                    },
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = standard {
                self.check_material(
                    handle,
                    &self.standard,
                    |this, m, report| this.check_dependencies(m, report),
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = terrain {
                self.check_material(
                    handle,
                    &self.terrain,
                    |this, m, report| this.check_dependencies(&m.base, report),
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = water {
                self.check_material(
                    handle,
                    &self.water,
                    |this, m, report| this.check_dependencies(&m.base, report),
                    &mut report,
                );
            }
            // A palette surface under a depth override draws through a wrapper; its palette
            // material is the source the wrapper was cloned from.
            let palette = palette
                .map(|material| &material.0)
                .or_else(|| palette_depth.map(|source| &source.0));
            if let Some(handle) = palette {
                self.check_material(
                    handle,
                    &self.palette,
                    // `ExtendedMaterial` reports no dependencies itself: its fields carry
                    // no `#[dependency]`, so each half is visited on its own.
                    |this, m, report| {
                        this.check_dependencies(&m.base, report);
                        this.check_dependencies(&m.extension, report);
                    },
                    &mut report,
                );
            }
        }
        report
    }
}

/// Sets the per-frame render-asset upload budget, if the engine has one.
fn set_upload_budget(
    budget: &mut Option<ResMut<RenderAssetBytesPerFrame>>,
    max_bytes: Option<usize>,
) {
    if let Some(budget) = budget {
        budget.max_bytes = max_bytes;
    }
}

fn set_alpha(overlay: &mut Query<&mut BackgroundColor, With<FadeOverlay>>, alpha: f32) {
    for mut colour in overlay.iter_mut() {
        colour.0 = Color::srgba(0.0, 0.0, 0.0, alpha.clamp(0.0, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_door_crossing(
    keyboard: Res<ButtonInput<KeyCode>>,
    capture: Res<CursorCapture>,
    time: Res<Time>,
    config: Res<crate::config::EngineConfig>,
    tuning: Res<MovementTuning>,
    space: Res<ActiveSpace>,
    origin: Res<RenderOrigin>,
    camera_space: Res<CameraSpace>,
    streaming: Res<StreamingWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    doors: Query<(&LoadDoor, &GlobalTransform, Option<&ExpectedModelBounds>)>,
    (physics, player): (ReadRapierContext, Query<Entity, With<PlayerBody>>),
    mut crossing: ResMut<DoorCrossing>,
    mut overlay: Query<&mut BackgroundColor, With<FadeOverlay>>,
    (mut budget, landing_assets, pending_under, walk_landing): (
        Option<ResMut<RenderAssetBytesPerFrame>>,
        LandingAssets,
        PendingUnder,
        WalkLanding,
    ),
    mut commands: Commands,
) {
    let delta = time.delta_secs();
    let Some(active) = crossing.active.as_mut() else {
        // Idle: E at a door starts a crossing.
        if !keyboard.just_pressed(KeyCode::KeyE) || *capture != CursorCapture::Captured {
            return;
        }
        let Ok(view) = camera.single() else {
            return;
        };
        let direction = view.forward().as_vec3();
        let Some((door, distance)) = doors
            .iter()
            .filter_map(|(door, transform, bounds)| {
                let bounds = bounds.copied().unwrap_or_else(fallback_bounds);
                ray_hits_door(view.translation, direction, transform, &bounds, DOOR_REACH)
                    .map(|distance| (door, distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return;
        };
        // The same camera-ray test the tankard grab uses: a door behind a wall is not reachable.
        // Without a physics world (a run with no colliders) nothing can block.
        if let Ok(physics) = physics.single() {
            let filter = match player.single() {
                Ok(player) => QueryFilter::default().exclude_rigid_body(player),
                Err(_) => QueryFilter::default(),
            };
            let nearest = physics
                .cast_ray(view.translation, direction, DOOR_REACH, true, filter)
                .map(|(_, distance)| distance);
            if door_view_blocked(nearest, distance) {
                return;
            }
        }
        let (yaw, _, _) = view.rotation.to_euler(EulerRot::YXZ);
        // The camera sits an eye height above the body, and the body a capsule half-extent above
        // the feet.
        let (body_offset, _) = body_and_camera_for_feet(&tuning, Vec3::ZERO);
        let feet = view.translation - Vec3::Y * tuning.eye_height - body_offset;
        let restore = Restore {
            space: *space,
            camera_space: *camera_space,
            configured_worldspace: config.worldspace_id,
            feet: if space.interior.is_some() {
                feet
            } else {
                absolute_feet(feet, origin.0)
            },
            yaw,
        };
        let target = landing_for(door, &space, config.worldspace_id);
        debug!(
            door = format_args!("{:08X}", door.ref_id),
            "door crossing started"
        );
        crossing.active = Some(Crossing {
            door: door.clone(),
            target,
            restore,
            since_press: 0.0,
            stage: Stage::FadeOut { elapsed: 0.0 },
        });
        set_alpha(&mut overlay, 0.0);
        return;
    };
    active.since_press += delta;
    match active.stage {
        Stage::FadeOut { elapsed } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                let target = active.target;
                commands.queue(move |world: &mut World| switch_space(world, target));
                // Behind black a stutter is invisible: lift the upload budget until the fade-in.
                set_upload_budget(&mut budget, None);
                active.stage = Stage::Landing {
                    waited: 0.0,
                    restoring: false,
                    ready_frames: 0,
                };
            } else {
                active.stage = Stage::FadeOut { elapsed };
            }
        }
        Stage::Landing {
            waited,
            restoring,
            ready_frames,
        } => {
            set_alpha(&mut overlay, 1.0);
            let waited = waited + delta;
            let landing = if restoring {
                active.restore.landing()
            } else {
                active.target
            };
            if streaming.is_failed(landing.key) {
                if restoring {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        "door crossing: the player's own space could not be reloaded; movement stays paused; press V or use tcl to recover in noclip"
                    );
                    info!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        destination = %describe_destination(&landing.key),
                        restored = true,
                        milliseconds = (active.since_press * 1000.0) as u32,
                        "door crossing: return failed; fade-in starts with movement paused"
                    );
                    set_upload_budget(&mut budget, config.max_upload_bytes_per_frame());
                    place_again(&mut commands, &landing);
                    active.stage = Stage::FadeIn {
                        elapsed: 0.0,
                        recovery: true,
                    };
                } else {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        destination = %describe_destination(&active.target.key),
                        "door crossing: the destination could not be loaded; putting the player back"
                    );
                    let restore = active.restore.landing();
                    commands.queue(move |world: &mut World| switch_space(world, restore));
                    active.stage = Stage::Landing {
                        waited: 0.0,
                        restoring: true,
                        ready_frames: 0,
                    };
                }
                return;
            }
            // Only the landing cell's own work counts: other cells of a stream window may stay
            // busy for a long time without affecting what the player sees.
            let root = streaming.resident_root(landing.key);
            let pending = root.map_or(0, |root| pending_under.count(root));
            let report = root.map(|root| landing_assets.inspect_under(root));
            let loaded = report
                .as_ref()
                .is_some_and(|report| pending == 0 && report.waiting == 0);
            // Once loaded, a few more frames let extraction and preparation upload with the budget
            // still lifted: the fade-in starts on the SETTLE_FRAMES-th loaded frame.
            let ready_frames = if loaded { ready_frames + 1 } else { 0 };
            let ready = ready_frames >= SETTLE_FRAMES && walk_landing.ready();
            if !ready && waited >= LANDING_TIMEOUT_SECONDS && !restoring {
                warn!(
                    door = format_args!("{:08X}", active.door.ref_id),
                    "door crossing: the destination was not ready in time; putting the player back"
                );
                let restore = active.restore.landing();
                commands.queue(move |world: &mut World| switch_space(world, restore));
                active.stage = Stage::Landing {
                    waited: 0.0,
                    restoring: true,
                    ready_frames: 0,
                };
                return;
            }
            if ready || waited >= LANDING_TIMEOUT_SECONDS {
                if !ready {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        waited_seconds = waited,
                        resident = root.is_some(),
                        pending_components = pending,
                        assets_loading = report.as_ref().map_or(0, |report| report.waiting),
                        first_not_loaded = report
                            .as_ref()
                            .and_then(|report| report.first_waiting.as_deref())
                            .unwrap_or("none"),
                        "door crossing: the return was not ready in time; movement stays paused; press V or use tcl to recover in noclip"
                    );
                }
                if let Some(report) = report.as_ref().filter(|report| !report.failed.is_empty()) {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        failed_assets = report.failed.len(),
                        "door crossing: some assets under the landing cell failed to load"
                    );
                }
                info!(
                    door = format_args!("{:08X}", active.door.ref_id),
                    destination = %describe_destination(&landing.key),
                    restored = restoring,
                    milliseconds = (active.since_press * 1000.0) as u32,
                    "door crossing: {} fade-in starts",
                    if !ready {
                        "return timed out with movement paused;"
                    } else if restoring {
                        "restored the player's own place;"
                    } else {
                        "arrived;"
                    }
                );
                set_upload_budget(&mut budget, config.max_upload_bytes_per_frame());
                // Re-teleporting WALK would discard the settled capsule and rearm its hold.
                // On a failed return the existing recovery guard still owns the unsafe pose.
                if !ready || !walk_landing.walking() {
                    place_again(&mut commands, &landing);
                }
                active.stage = Stage::FadeIn {
                    elapsed: 0.0,
                    recovery: !ready,
                };
            } else {
                active.stage = Stage::Landing {
                    waited,
                    restoring,
                    ready_frames,
                };
            }
        }
        Stage::FadeIn { elapsed, recovery } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, 1.0 - elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                if recovery {
                    active.stage = Stage::Recovery;
                } else {
                    crossing.active = None;
                }
            } else {
                active.stage = Stage::FadeIn { elapsed, recovery };
            }
        }
        Stage::Recovery => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doors::DoorDestination,
        physics::MoveMode,
        profiling::ProfilingState,
        streaming::{StreamingMetrics, StreamingWorld, pending_marker_for_test},
    };
    use bevy::ecs::system::RunSystemOnce;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    const DT: f32 = 0.05;

    #[derive(Resource, Default)]
    struct Teleports(Vec<TeleportPlayer>);

    fn collect(mut reader: MessageReader<TeleportPlayer>, mut out: ResMut<Teleports>) {
        out.0.extend(reader.read().copied());
    }

    fn door(destination: DoorDestination) -> LoadDoor {
        LoadDoor {
            ref_id: 0x10,
            destination,
        }
    }

    fn interior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x20,
            interior_cell_id: Some(77),
            worldspace_id: None,
            arrival_position: [100.0, 200.0, 300.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    fn exterior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x21,
            interior_cell_id: None,
            worldspace_id: Some(60),
            // Grid (3, -2) in Creation units, 100 into the cell on each axis.
            arrival_position: [3.0 * 4096.0 + 100.0, -2.0 * 4096.0 + 100.0, 50.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    /// A headless world with the crossing registered, the camera 100 units in front of a door
    /// whose bounds are a 100-unit cube, and one resident old-space cell.
    fn app_with(door: LoadDoor) -> (App, Entity) {
        app_with_extra(door, |_| {})
    }

    fn app_with_environment(
        door: LoadDoor,
        environment: Option<crate::world::environment::EnvironmentCatalog>,
    ) -> (App, Entity) {
        app_with_extra(door, |app| {
            if let Some(environment) = environment {
                app.insert_resource(environment)
                    .insert_resource(GlobalAmbientLight {
                        color: Color::srgb(0.1, 0.2, 0.3),
                        brightness: 160.0,
                        ..default()
                    })
                    .add_plugins(crate::world::environment::interior::InteriorEnvironmentPlugin);
                app.world_mut().spawn((
                    crate::world::environment::interior::WorldEnvironmentLight,
                    DirectionalLight {
                        illuminance: 12_000.0,
                        ..default()
                    },
                    Transform::from_rotation(Quat::from_rotation_y(0.4)),
                ));
            }
        })
    }

    fn app_with_extra(door: LoadDoor, extra: impl FnOnce(&mut App)) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin, AssetPlugin::default()));
        app.init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .init_asset::<EffectPaletteMaterial>()
            .insert_resource(RenderAssetBytesPerFrame {
                max_bytes: crate::config::EngineConfig::default().max_upload_bytes_per_frame(),
            });
        app.init_resource::<ProfilingState>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<StreamingWorld>()
            .init_resource::<crate::streaming::TerrainContinuity>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ActiveSpace>()
            .init_resource::<MovementTuning>()
            .init_resource::<Teleports>()
            .insert_resource(MoveMode::Noclip)
            .insert_resource(CursorCapture::Captured)
            .insert_resource(RenderOrigin(IVec2::new(1, 1)))
            .insert_resource(crate::config::EngineConfig::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
                DT,
            )))
            .add_plugins(DoorCrossingPlugin)
            .add_systems(Last, collect);
        app.world_mut().spawn((
            StreamingCamera,
            Transform::from_xyz(0.0, 0.0, 0.0).looking_to(Vec3::NEG_Z, Vec3::Y),
            GlobalTransform::default(),
        ));
        app.world_mut().spawn((
            door,
            ExpectedModelBounds::new(Vec3::splat(-50.0), Vec3::splat(50.0)).unwrap(),
            Transform::from_xyz(0.0, 0.0, -150.0),
            GlobalTransform::default(),
        ));
        let old_root = app.world_mut().spawn_empty().id();
        let old = CellKey::Exterior {
            worldspace_id: 1,
            grid_x: 1,
            grid_y: 1,
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(old, old_root);
        extra(&mut app);
        app.finish();
        app.update();
        (app, old_root)
    }

    /// Use the real player/physics schedules without a window, GPU, assets or settings writes.
    fn walking_app(door: LoadDoor, source_interior: bool) -> App {
        let (app, _) = app_with_extra(door, |app| {
            app.init_resource::<ButtonInput<MouseButton>>()
                .add_message::<bevy::input::mouse::MouseMotion>()
                .add_plugins(crate::physics::PlayerControlsPlugin)
                .insert_resource(MoveMode::Walk);
            let tuning = app.world().resource::<MovementTuning>().clone();
            app.world_mut().spawn((
                crate::physics::player_controller_bundle(&tuning),
                Transform::from_xyz(0.0, -tuning.eye_height, 0.0),
            ));
            if source_interior {
                app.insert_resource(ActiveSpace {
                    worldspace_id: Some(60),
                    interior: Some(5),
                })
                .insert_resource(CameraSpace::Interior);
            }
        });
        app
    }

    fn current_landing(app: &App) -> Landing {
        let crossing = app
            .world()
            .resource::<DoorCrossing>()
            .active
            .as_ref()
            .unwrap();
        if matches!(
            crossing.stage,
            Stage::Landing {
                restoring: true,
                ..
            }
        ) {
            crossing.restore.landing()
        } else {
            crossing.target
        }
    }

    fn player_position(app: &mut App) -> Vec3 {
        app.world_mut()
            .query_filtered::<&Transform, With<PlayerBody>>()
            .single(app.world())
            .unwrap()
            .translation
    }

    fn register_floor(app: &mut App, landing: Landing, disabled: bool, blocked: bool) -> Entity {
        use bevy_rapier3d::prelude::*;
        let root = app.world_mut().spawn(Transform::default()).id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(landing.key, root);
        // Eight units of initial overlap exercises the existing 28-unit safe-placement search.
        let top = landing.position.y + 8.0;
        let (half_height, center_y) = if blocked {
            (1000.0, top)
        } else {
            (10.0, top - 10.0)
        };
        let mut floor = app.world_mut().spawn((
            RigidBody::Fixed,
            Collider::cuboid(1000.0, half_height, 1000.0),
            crate::physics::world_collision_groups(),
            Transform::from_xyz(landing.position.x, center_y, landing.position.z),
            ChildOf(root),
        ));
        if disabled {
            floor.insert(ColliderDisabled);
        }
        floor.id()
    }

    #[test]
    fn walk_arrival_settles_before_visibility_for_destinations_and_rollbacks() {
        use bevy_rapier3d::prelude::ColliderDisabled;
        for exterior in [false, true] {
            for restoring in [false, true] {
                let door = if exterior {
                    exterior_door()
                } else {
                    interior_door()
                };
                let mut app = walking_app(door, exterior);
                press_e(&mut app);
                // Interior starts differ from ActiveSpace::default(), so inspect the stage.
                for _ in 0..20 {
                    if matches!(
                        app.world()
                            .resource::<DoorCrossing>()
                            .active
                            .as_ref()
                            .unwrap()
                            .stage,
                        Stage::Landing { .. }
                    ) {
                        break;
                    }
                    app.update();
                }
                if restoring {
                    let target = current_landing(&app);
                    app.world_mut()
                        .resource_mut::<StreamingWorld>()
                        .set_failed_for_test(target.key);
                    app.update();
                }
                let landing = current_landing(&app);
                let floor = register_floor(&mut app, landing, true, false);
                for _ in 0..8 {
                    app.update();
                    assert_eq!(
                        alpha(&mut app),
                        1.0,
                        "render residency cannot prove support"
                    );
                    assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Walk);
                }
                if landing.origin.is_some() {
                    // Model the streamer's exterior rebase while collision is still disabled.
                    // A successful WALK fade must preserve this live pose, not replay XTEL.
                    let shift = IVec2::new(2, -1);
                    let displacement = absolute_feet(Vec3::ZERO, shift);
                    app.world_mut().resource_mut::<RenderOrigin>().0 += shift;
                    let mut participants = app.world_mut().query_filtered::<&mut Transform,
                        Or<(With<PlayerBody>, With<StreamingCamera>)>>();
                    for mut transform in participants.iter_mut(app.world_mut()) {
                        transform.translation -= displacement;
                    }
                    app.world_mut()
                        .get_mut::<Transform>(floor)
                        .unwrap()
                        .translation -= displacement;
                    let body = app
                        .world_mut()
                        .query_filtered::<Entity, With<PlayerBody>>()
                        .single(app.world())
                        .unwrap();
                    // The real streamer also shifts Rapier immediately; transform edits alone
                    // can be overwritten by the backend's writeback before propagation.
                    app.world_mut()
                        .run_system_once(
                            move |mut physics: bevy_rapier3d::prelude::WriteRapierContext| {
                                let mut context = physics.single_mut().unwrap();
                                for entity in [body, floor] {
                                    let handle =
                                        context.entity2body().get(&entity).copied().unwrap();
                                    let body =
                                        context.rigidbody_set.bodies.get_mut(handle).unwrap();
                                    body.set_translation(body.translation() - displacement, false);
                                }
                                context.propagate_modified_body_positions_to_colliders();
                            },
                        )
                        .unwrap();
                    app.update();
                }
                let held = player_position(&mut app);
                app.world_mut()
                    .entity_mut(floor)
                    .remove::<ColliderDisabled>();
                let mut visible_pose = None;
                let mut highest = held.y;
                for frame in 0..60 {
                    app.update();
                    let pose = player_position(&mut app);
                    let opacity = alpha(&mut app);
                    highest = highest.max(pose.y);
                    let hold = app.world().resource::<WalkSpawnHold>().is_active();
                    println!(
                        "issue468_trace {}",
                        serde_json::json!({
                            "exterior": exterior, "restoring": restoring, "frame": frame,
                            "alpha": opacity, "body": pose.to_array(), "hold": hold,
                        })
                    );
                    if opacity < 1.0 {
                        assert!(!hold, "visible arrivals must not rearm placement");
                        let first = *visible_pose.get_or_insert(pose);
                        assert!(
                            (pose - first).length() < 0.001,
                            "visible correction: {first:?} -> {pose:?}"
                        );
                        let mut views = app
                            .world_mut()
                            .query_filtered::<&Transform, With<StreamingCamera>>();
                        let eye = views.single(app.world()).unwrap().translation;
                        assert!(
                            (eye - pose
                                - Vec3::Y * app.world().resource::<MovementTuning>().eye_height)
                                .length()
                                < 0.001
                        );
                    }
                }
                assert!(
                    visible_pose.is_some(),
                    "WALK must finish settling and fade in"
                );
                assert!(
                    highest > held.y + 10.0,
                    "fixture must exercise upward safe placement"
                );
                assert!(!app.world().resource::<DoorCrossing>().is_active());
                assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Walk);
                let pose = player_position(&mut app);
                let expected = match landing.origin {
                    Some(saved) => absolute_feet(
                        landing.position,
                        saved - app.world().resource::<RenderOrigin>().0,
                    ),
                    None => landing.position,
                };
                assert!(
                    (Vec2::new(pose.x, pose.z) - Vec2::new(expected.x, expected.z)).length()
                        < 0.001
                );
                assert_eq!(
                    app.world().resource::<Teleports>().0.len(),
                    if restoring { 2 } else { 1 }
                );
            }
        }
    }

    #[test]
    fn walk_collision_failure_rolls_back_then_requires_explicit_recovery() {
        for blocked in [false, true] {
            let mut app = walking_app(interior_door(), false);
            press_e(&mut app);
            run_until_black(&mut app);
            let landing = current_landing(&app);
            register_floor(&mut app, landing, !blocked, blocked);
            let held = body_and_camera_for_feet(
                app.world().resource::<MovementTuning>(),
                landing.position,
            )
            .0;
            for _ in 0..((LANDING_TIMEOUT_SECONDS + 0.5) / DT) as usize {
                app.update();
                if app
                    .world()
                    .resource::<DoorCrossing>()
                    .active
                    .as_ref()
                    .is_some_and(|active| {
                        matches!(
                            active.stage,
                            Stage::Landing {
                                restoring: false,
                                ..
                            }
                        )
                    })
                {
                    assert!(
                        (player_position(&mut app) - held).length() < 0.001,
                        "a pending hold must not depenetrate outside the bounded placement search"
                    );
                }
            }
            assert!(
                app.world().resource::<DoorCrossing>().is_active(),
                "blocked={blocked}; mode={:?}; hold={:?}; status={:?}",
                app.world().resource::<MoveMode>(),
                app.world().resource::<WalkSpawnHold>(),
                app.world().resource::<crate::physics::WalkEntryStatus>()
            );
            assert!(matches!(
                app.world()
                    .resource::<DoorCrossing>()
                    .active
                    .as_ref()
                    .unwrap()
                    .stage,
                Stage::Landing {
                    restoring: true,
                    ..
                }
            ));
            assert_eq!(alpha(&mut app), 1.0);
            assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Walk);
            // A resident return with no enabled support must also remain guarded.
            let restore = current_landing(&app);
            register_floor(&mut app, restore, true, false);
            for _ in 0..((LANDING_TIMEOUT_SECONDS + 0.5) / DT) as usize {
                app.update();
            }
            assert_eq!(alpha(&mut app), 0.0);
            assert!(app.world().resource::<DoorCrossing>().needs_recovery());
            assert!(
                !app.world_mut()
                    .run_system_once(walk_physics_allowed)
                    .unwrap()
            );
            assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Walk);
        }
    }

    fn press_e(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::KeyE);
    }

    fn alpha(app: &mut App) -> f32 {
        let mut query = app
            .world_mut()
            .query_filtered::<&BackgroundColor, With<FadeOverlay>>();
        query.single(app.world()).unwrap().0.to_srgba().alpha
    }

    fn run_until_black(app: &mut App) {
        for _ in 0..20 {
            if app.world().resource::<ActiveSpace>() != &ActiveSpace::default() {
                return;
            }
            app.update();
        }
        panic!("the space never switched");
    }

    #[test]
    fn e_at_a_door_starts_a_crossing_and_e_elsewhere_does_not() {
        let (mut app, _) = app_with(interior_door());
        // Turn away from the door: nothing starts.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.look_to(Vec3::Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        // Face it, but from beyond reach.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.translation = Vec3::new(0.0, 0.0, 400.0);
            camera.look_to(Vec3::NEG_Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            query.single_mut(app.world_mut()).unwrap().translation = Vec3::ZERO;
        }
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
        // E while it runs does not start another.
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_space_switches_at_full_black_and_the_old_cells_go_the_same_frame() {
        let (mut app, old_root) = app_with(interior_door());
        press_e(&mut app);
        assert!(alpha(&mut app) < 1.0);
        run_until_black(&mut app);
        assert!(alpha(&mut app) >= 0.99, "the switch happens at full black");
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.interior, Some(77));
        assert!(app.world().get_entity(old_root).is_err());
        assert_eq!(
            app.world()
                .resource::<StreamingWorld>()
                .cell_count_for_test(),
            0
        );
        assert_eq!(
            *app.world().resource::<CameraSpace>(),
            CameraSpace::Interior
        );
        // An interior landing leaves the render origin alone.
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 1);
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 200.0, 300.0))
        );
    }

    #[test]
    fn crossing_and_timed_out_landing_restore_render_lighting_in_the_scene_switch_frame() {
        check_crossing_restores_lighting(false);
    }

    #[test]
    fn crossing_and_failed_cell_restore_render_lighting_in_the_scene_switch_frame() {
        check_crossing_restores_lighting(true);
    }

    fn check_crossing_restores_lighting(fail_destination: bool) {
        use crate::world::environment::{
            EnvironmentCatalog,
            interior::{SceneEnvironment, WorldEnvironmentLight},
        };
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        converter::esm::exporter::create_tables(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO cell_environment(cell_id,data_flags) VALUES(77,1);
             INSERT INTO cell_lighting(cell_id,xcll_size,inherit_flags,
                ambient_r,ambient_g,ambient_b,directional_r,directional_g,directional_b,
                directional_rotation_xy,directional_rotation_z,directional_fade)
             VALUES(77,92,0,10,20,30,40,50,60,90,30,0.5);",
            )
            .unwrap();
        let (mut app, _) = app_with_environment(
            interior_door(),
            Some(EnvironmentCatalog::from_connection(&connection)),
        );
        let sun = app
            .world_mut()
            .query_filtered::<Entity, With<WorldEnvironmentLight>>()
            .single(app.world())
            .unwrap();
        let exterior_rotation = app.world().get::<Transform>(sun).unwrap().rotation;
        press_e(&mut app);
        let mut saw_interior = false;
        let mut saw_restore = false;
        for _ in 0..300 {
            app.update();
            let cell = app.world().resource::<ActiveSpace>().interior;
            let scene = app.world().resource::<SceneEnvironment>();
            assert_eq!(
                scene.interior.as_ref().map(|lighting| lighting.cell_id),
                cell
            );
            let light = app.world().get::<DirectionalLight>(sun).unwrap();
            let ambient = app.world().resource::<GlobalAmbientLight>();
            if cell.is_some() {
                saw_interior = true;
                assert_eq!(light.color, Color::srgb_u8(40, 50, 60));
                assert_eq!(light.illuminance, 6_000.0);
                assert_eq!(ambient.color, Color::srgb_u8(10, 20, 30));
                if fail_destination {
                    app.world_mut()
                        .resource_mut::<StreamingWorld>()
                        .set_failed_for_test(CellKey::Interior(77));
                }
            } else if saw_interior {
                // The real failure or timeout path queues the exterior restoration.
                saw_restore = true;
                assert_eq!(light.color, DirectionalLight::default().color);
                assert_eq!(light.illuminance, 12_000.0);
                assert_eq!(ambient.color, Color::srgb(0.1, 0.2, 0.3));
                assert_eq!(
                    app.world().get::<Transform>(sun).unwrap().rotation,
                    exterior_rotation
                );
                break;
            }
        }
        assert!(saw_interior && saw_restore);
    }

    #[test]
    fn an_exterior_landing_sets_the_render_origin_to_the_arrival_grid() {
        let (mut app, _) = app_with(exterior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(3, -2));
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.worldspace_id, Some(60));
        assert_eq!(space.interior, None);
        let teleports = &app.world().resource::<Teleports>().0;
        // 100 units into the cell on each axis, relative to the new origin: no cell offset.
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 100.0, 50.0))
        );
    }

    #[test]
    fn the_fade_in_waits_for_the_landing_cell_and_starts_within_a_frame_of_it() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        for _ in 0..30 {
            app.update();
        }
        assert!(
            alpha(&mut app) >= 0.99,
            "still black while nothing is resident"
        );
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        // Pending work under the landing root keeps it black.
        let marker = app
            .world_mut()
            .spawn((pending_marker_for_test(), ChildOf(root)))
            .id();
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99, "pending work under the root holds");
        app.world_mut().entity_mut(marker).despawn();
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the fade-in starts once it is ready");
        for _ in 0..10 {
            app.update();
        }
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        assert_eq!(alpha(&mut app), 0.0);
    }

    #[test]
    fn the_fade_in_places_the_player_on_the_landing_point_again() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the fade-in started");
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 2, "{teleports:?}");
        assert_eq!(teleports[1], teleports[0], "the same landing point twice");
    }

    #[test]
    fn exterior_landing_and_restore_survive_a_rebase_during_the_black_hold() {
        for restoring in [false, true] {
            let (mut app, _) = app_with(exterior_door());
            press_e(&mut app);
            run_until_black(&mut app);
            if restoring {
                let target = app
                    .world()
                    .resource::<DoorCrossing>()
                    .active
                    .as_ref()
                    .unwrap()
                    .target;
                app.world_mut()
                    .resource_mut::<StreamingWorld>()
                    .set_failed_for_test(target.key);
                app.update();
            }
            let crossing = app
                .world()
                .resource::<DoorCrossing>()
                .active
                .as_ref()
                .unwrap();
            let landing = if restoring {
                crossing.restore.landing()
            } else {
                crossing.target
            };
            let saved_origin = landing.origin.unwrap();
            let rebased = saved_origin + IVec2::new(2, -1);
            app.world_mut().resource_mut::<RenderOrigin>().0 = rebased;
            let root = app.world_mut().spawn_empty().id();
            app.world_mut()
                .resource_mut::<StreamingWorld>()
                .set_resident_for_test(landing.key, root);
            for _ in 0..4 {
                app.update();
            }
            assert!(
                alpha(&mut app) < 1.0,
                "restoring={restoring}: fade-in starts"
            );
            let teleports = &app.world().resource::<Teleports>().0;
            let final_pose = teleports.last().unwrap();
            assert_eq!(
                absolute_feet(final_pose.position, rebased),
                absolute_feet(landing.position, saved_origin)
            );
            assert_eq!(final_pose.yaw, landing.yaw);
        }
    }

    #[test]
    fn walking_pauses_only_while_a_crossing_is_in_progress() {
        let mut world = World::new();
        assert!(world.run_system_once(no_door_crossing).unwrap());
        world.insert_resource(DoorCrossing::default());
        assert!(world.run_system_once(no_door_crossing).unwrap());
        world.insert_resource(DoorCrossing::holding_for_test());
        assert!(!world.run_system_once(no_door_crossing).unwrap());
    }

    #[test]
    fn pending_work_in_another_cell_does_not_hold_the_fade_in() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        // The global counts and a marker under some other cell's root are not the landing cell's.
        {
            let mut metrics = app.world_mut().resource_mut::<StreamingMetrics>();
            metrics.pending_asset_instances = 50;
            metrics.pending_surface_instances = 50;
        }
        let other_root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .spawn((pending_marker_for_test(), ChildOf(other_root)));
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the fade-in started");
    }

    fn budget(app: &App) -> Option<usize> {
        app.world().resource::<RenderAssetBytesPerFrame>().max_bytes
    }

    fn configured_budget() -> Option<usize> {
        crate::config::EngineConfig::default().max_upload_bytes_per_frame()
    }

    #[test]
    fn the_upload_budget_is_lifted_at_the_switch_and_restored_at_the_fade_in() {
        let (mut app, _) = app_with(interior_door());
        assert!(configured_budget().is_some());
        press_e(&mut app);
        assert_eq!(budget(&app), configured_budget(), "kept while fading out");
        run_until_black(&mut app);
        assert_eq!(budget(&app), None, "lifted from the switch");
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        app.update();
        assert_eq!(budget(&app), None, "still lifted while the frames settle");
        for _ in 0..10 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0);
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn the_upload_budget_is_restored_when_the_destination_fails() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        assert_eq!(budget(&app), None);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        assert_eq!(
            budget(&app),
            None,
            "still black, still lifted while restoring"
        );
        // The player's own space fails too: fade in with movement still guarded.
        let own = CellKey::Exterior {
            worldspace_id: crate::config::EngineConfig::default().worldspace_id,
            grid_x: 1,
            grid_y: 1,
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(own);
        app.update();
        app.update();
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn the_fade_in_waits_for_a_landing_texture_and_a_mesh_then_settles_two_frames() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let texture = app.world().resource::<Assets<Image>>().reserve_handle();
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial {
                base_color_texture: Some(texture.clone()),
                ..default()
            });
        let mesh = app.world().resource::<Assets<Mesh>>().reserve_handle();
        let root = app.world_mut().spawn_empty().id();
        app.world_mut().spawn((
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material),
            ChildOf(root),
        ));
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        for _ in 0..20 {
            app.update();
        }
        assert!(
            alpha(&mut app) >= 0.99,
            "the texture and mesh are still loading"
        );
        assert_eq!(budget(&app), None);
        app.world_mut()
            .resource_mut::<Assets<Image>>()
            .insert(texture.id(), Image::default())
            .unwrap();
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99, "the mesh is still loading");
        app.world_mut()
            .resource_mut::<Assets<Mesh>>()
            .insert(
                mesh.id(),
                Mesh::new(
                    bevy::mesh::PrimitiveTopology::TriangleList,
                    bevy::asset::RenderAssetUsages::default(),
                ),
            )
            .unwrap();
        // Loaded on the first of these frames; the fade-in starts on the SETTLE_FRAMES-th loaded
        // frame (the second), and the fade itself shows from the frame after.
        app.update();
        assert!(alpha(&mut app) >= 0.99, "settling");
        assert_eq!(budget(&app), None);
        app.update();
        assert_eq!(budget(&app), configured_budget(), "the fade-in started");
        app.update();
        assert!(alpha(&mut app) < 1.0, "the fade shows");
    }

    #[test]
    fn the_fade_in_waits_for_an_effect_palette_materials_source_texture_and_palette() {
        fade_in_waits_for_effect_palette_images(false);
    }

    #[test]
    fn the_fade_in_waits_for_a_depth_wrapped_effect_palette_through_its_source() {
        fade_in_waits_for_effect_palette_images(true);
    }

    /// `depth_wrapped`: the surface carries only the wrapper's source, as a palette surface
    /// under an authored depth override does; the wrapper itself is not inspected.
    fn fade_in_waits_for_effect_palette_images(depth_wrapped: bool) {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let source = app.world().resource::<Assets<Image>>().reserve_handle();
        let palette = app.world().resource::<Assets<Image>>().reserve_handle();
        let material = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteMaterial>>()
            .add(EffectPaletteMaterial {
                base: StandardMaterial {
                    base_color_texture: Some(source.clone()),
                    ..default()
                },
                extension: crate::nif_material::EffectPalette::new(
                    palette.clone(),
                    true,
                    true,
                    1.0,
                ),
            });
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Mesh::new(
                bevy::mesh::PrimitiveTopology::TriangleList,
                bevy::asset::RenderAssetUsages::default(),
            ));
        let root = app.world_mut().spawn_empty().id();
        let part = app.world_mut().spawn((Mesh3d(mesh), ChildOf(root))).id();
        if depth_wrapped {
            app.world_mut()
                .entity_mut(part)
                .insert(EffectPaletteDepthSource(material));
        } else {
            app.world_mut()
                .entity_mut(part)
                .insert(MeshMaterial3d(material));
        }
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        for (image, waiting) in [(source, "the palette"), (palette, "")] {
            for _ in 0..20 {
                app.update();
            }
            assert!(alpha(&mut app) >= 0.99, "an image is still loading");
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .insert(image.id(), Image::default())
                .unwrap();
            if !waiting.is_empty() {
                for _ in 0..20 {
                    app.update();
                }
                assert!(alpha(&mut app) >= 0.99, "{waiting} is still loading");
            }
        }
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(budget(&app), configured_budget(), "the fade-in started");
    }

    #[test]
    fn a_destination_timeout_returns_to_the_saved_space_before_resuming() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let frames = (LANDING_TIMEOUT_SECONDS / DT) as usize;
        for _ in 0..frames - 5 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99);
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99, "the return is still loading");
        assert_eq!(
            *app.world().resource::<ActiveSpace>(),
            ActiveSpace::default()
        );
        assert!(!app.world_mut().run_system_once(no_door_crossing).unwrap());
        let own = app
            .world()
            .resource::<DoorCrossing>()
            .active
            .as_ref()
            .unwrap()
            .restore
            .landing()
            .key;
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(own, root);
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) < 0.01, "the loaded return fades in");
        assert!(app.world_mut().run_system_once(no_door_crossing).unwrap());
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn a_return_timeout_fades_in_without_releasing_the_movement_guard() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        for _ in 0..((2.0 * LANDING_TIMEOUT_SECONDS + 1.0) / DT) as usize {
            app.update();
        }
        assert!(
            alpha(&mut app) < 0.01,
            "failure does not leave a black screen"
        );
        assert!(app.world().resource::<DoorCrossing>().needs_recovery());
        assert!(!app.world_mut().run_system_once(no_door_crossing).unwrap());
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn a_failed_return_stays_guarded_after_the_fade_in() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let own = app
            .world()
            .resource::<DoorCrossing>()
            .active
            .as_ref()
            .unwrap()
            .restore
            .landing()
            .key;
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(own);
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) < 0.01);
        assert!(app.world().resource::<DoorCrossing>().needs_recovery());
        assert!(!app.world_mut().run_system_once(no_door_crossing).unwrap());
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn a_destination_that_fails_puts_the_player_back_where_they_were() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space, ActiveSpace::default());
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 2);
        let back = teleports[1].position;
        assert!(back.x.abs() < 0.01 && back.z.abs() < 0.01);
    }

    #[test]
    fn the_restore_pose_survives_a_render_origin_rebase_during_the_fade_out() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        // A rebase while fading out: the origin moves, the stored pose must not.
        app.world_mut().resource_mut::<RenderOrigin>().0 = IVec2::new(5, 5);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let origin = app.world().resource::<RenderOrigin>().0;
        let teleports = &app.world().resource::<Teleports>().0;
        let back = teleports.last().unwrap().position;
        // The player stood at render (0, _, 0) under origin (1, 1): the same world place now.
        let world_x = back.x + origin.x as f32 * CELL_SIZE;
        let world_z = back.z - origin.y as f32 * CELL_SIZE;
        assert!((world_x - CELL_SIZE).abs() < 0.01, "x {world_x}");
        assert!((world_z + CELL_SIZE).abs() < 0.01, "z {world_z}");
        let tuning = MovementTuning::default();
        let feet_y = -tuning.eye_height - (tuning.capsule_half_height() + tuning.capsule_radius);
        assert!((back.y - feet_y).abs() < 0.01);
    }

    #[test]
    fn a_failed_crossing_from_an_interior_lands_where_the_player_stood() {
        // Start inside interior 5 with a non-zero render origin that interiors do not use.
        let (mut app, _) = app_with(exterior_door());
        *app.world_mut().resource_mut::<ActiveSpace>() = ActiveSpace {
            worldspace_id: Some(60),
            interior: Some(5),
        };
        *app.world_mut().resource_mut::<CameraSpace>() = CameraSpace::Interior;
        app.world_mut().resource_mut::<RenderOrigin>().0 = IVec2::new(7, -3);
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            query.single_mut(app.world_mut()).unwrap().translation = Vec3::new(10.0, 0.0, 0.0);
        }
        press_e(&mut app);
        let door_target = CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 3,
            grid_y: -2,
        };
        for _ in 0..20 {
            if app.world().resource::<ActiveSpace>().interior.is_none() {
                break;
            }
            app.update();
        }
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(door_target);
        app.update();
        app.update();
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.interior, Some(5), "back in the interior");
        let tuning = MovementTuning::default();
        let (body_offset, _) = body_and_camera_for_feet(&tuning, Vec3::ZERO);
        let expected = Vec3::new(10.0, 0.0, 0.0) - Vec3::Y * tuning.eye_height - body_offset;
        let back = app
            .world()
            .resource::<Teleports>()
            .0
            .last()
            .unwrap()
            .position;
        assert!(
            (back - expected).length() < 0.01,
            "restored to {back}, expected {expected}"
        );
    }

    #[test]
    fn a_door_without_model_bounds_can_still_be_activated() {
        let (mut app, _) = app_with(interior_door());
        let mut doors = app.world_mut().query_filtered::<Entity, With<LoadDoor>>();
        let door = doors.single(app.world()).unwrap();
        app.world_mut()
            .entity_mut(door)
            .remove::<ExpectedModelBounds>();
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_teleport_target_is_the_arrival_feet_position() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let tuning = MovementTuning::default();
        let (_, eye) =
            body_and_camera_for_feet(&tuning, creation_to_bevy(Vec3::new(100.0, 200.0, 300.0)));
        let mut view = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>();
        assert!((view.single(app.world()).unwrap().translation - eye).length() < 1e-3);
    }

    #[test]
    fn a_nearer_collider_blocks_the_view_of_a_door() {
        // A wall well in front of the door hides it.
        assert!(door_view_blocked(Some(40.0), 100.0));
        // The door's own collider or frame at its bounds does not.
        assert!(!door_view_blocked(Some(100.0), 100.0));
        assert!(!door_view_blocked(Some(90.0), 100.0));
        // Something behind the door, or nothing at all, does not either.
        assert!(!door_view_blocked(Some(180.0), 100.0));
        assert!(!door_view_blocked(None, 100.0));
    }

    /// A ray hits a rotated door's bounds only toward the door and within reach, and never
    /// hits a door whose scale is zero, flat or not finite.
    #[test]
    fn a_ray_tests_the_oriented_bounds() {
        let bounds =
            ExpectedModelBounds::new(Vec3::new(-10.0, 0.0, -2.0), Vec3::new(10.0, 40.0, 2.0))
                .unwrap();
        let door = GlobalTransform::from(
            Transform::from_xyz(0.0, 0.0, -100.0).with_rotation(Quat::from_rotation_y(0.5)),
        );
        let hit = ray_hits_door(
            Vec3::new(0.0, 20.0, 0.0),
            Vec3::NEG_Z,
            &door,
            &bounds,
            200.0,
        );
        assert!(hit.is_some_and(|distance| (distance - 98.0).abs() < 5.0));
        assert!(ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::Z, &door, &bounds, 200.0).is_none());
        assert!(
            ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Z, &door, &bounds, 50.0).is_none()
        );

        // A load door whose reference scale (`XSCL`) is zero or not finite has a singular or NaN
        // affine: no ray may use it. Without the finiteness check the slab test hits it at 0 for
        // every origin and direction, which makes it the nearest door from anywhere.
        for scale in [
            Vec3::ZERO,
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::splat(f32::NAN),
            Vec3::splat(f32::INFINITY),
        ] {
            let degenerate =
                GlobalTransform::from(Transform::from_xyz(0.0, 0.0, -100.0).with_scale(scale));
            for direction in [Vec3::NEG_Z, Vec3::Z, Vec3::X] {
                assert!(
                    ray_hits_door(
                        Vec3::new(0.0, 20.0, 0.0),
                        direction,
                        &degenerate,
                        &bounds,
                        200.0,
                    )
                    .is_none(),
                    "scale {scale:?} direction {direction:?}"
                );
            }
        }
    }
}
