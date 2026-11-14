//! Player movement physics: one controller for Riverwood and the regression fixture.
//! All units are Creation units.

use bevy::{
    ecs::system::{SystemId, SystemParam},
    input::mouse::MouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use bevy_rapier3d::prelude::*;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use shared::collision::CollisionBody;
use std::path::Path;

#[cfg(test)]
mod stair_tests;

use crate::{
    console::{AppConsoleExt, ConsoleCommand, ConsoleState, console_closed},
    profiling::ProfilingState,
    streaming::{StreamingMetrics, TerrainCollider},
    world::components::StreamingCamera,
};

/// Fixed physics step: 60 Hz Rapier simulation (V15, V18).
pub const PHYSICS_TIMESTEP: f32 = 1.0 / 60.0;
const PLAYER_CONTROLLER_OFFSET: f32 = 2.0;
const NPC_DEFAULT_MOVT_FORM_ID: u32 = 0x0003_580D;
/// Shared downward gravity in Creation units/s^2 for WALK and tankards (V11, V15).
pub const GRAVITY_CREATION_UNITS: f32 = 900.0;
/// Collision groups: world surfaces (V17).
pub const GROUP_WORLD: Group = Group::GROUP_1;
/// Collision groups: player capsule (V17).
pub const GROUP_PLAYER: Group = Group::GROUP_2;
/// Collision groups: dynamic debug tankards (V17).
pub const GROUP_TANKARD: Group = Group::GROUP_3;
/// Collision groups: placed dynamic clutter from authored physics data (#104 phase a).
pub const GROUP_CLUTTER: Group = Group::GROUP_4;
/// Rapier's default linear speed cap is 400 units/s, which clips free fall under 900 units/s^2
/// gravity after 0.44 s. Authored clutter limits (about 7000 units/s) are enforced per body by
/// [`clamp_dynamic_clutter_velocities`], so the global cap only has to stay out of the way. The cap
/// is global, so debug tankards fall faster too; they and clutter both use continuous collision.
pub const MAX_LINEAR_SPEED: f32 = 20_000.0;
/// Live-cap on simultaneous dynamic clutter bodies, like [`MAX_LIVE_TANKARDS`]. The cap bounds
/// the contact-pair cost a cell arrival can create; bodies over it keep no collider at all
/// for as long as their cell stays resident (a refusal is not retried); they are considered again
/// when their cell loads next, if earlier bodies have unloaded by then.
pub const MAX_LIVE_DYNAMIC_CLUTTER: usize = 256;

/// Provisional movement tuning, centralized (V10, V11, V12).
#[derive(Debug, Clone, Resource)]
pub struct MovementTuning {
    pub walk_speed: f32,
    pub run_speed: f32,
    /// Provisional sprint factor applied to the selected directional run speed.
    pub sprint_multiplier: f32,
    pub horizontal_acceleration: f32,
    pub gravity: f32,
    pub jump_launch: f32,
    pub capsule_radius: f32,
    pub capsule_standing_height: f32,
    pub eye_height: f32,
    pub slope_climb_degrees: f32,
    pub slope_slide_degrees: f32,
    pub autostep_height: f32,
    pub ground_snap: f32,
    /// Selected MOVT's left/right/forward/back walk/run speeds, when packaged.
    pub record_speeds: Option<DirectionalMovementSpeeds>,
}

#[derive(Debug, Clone)]
pub struct DirectionalMovementSpeeds {
    /// Each pair is [walk, run], ordered left/right/forward/back.
    pub directions: [[f32; 2]; 4],
    pub source: String,
}

impl Default for MovementTuning {
    fn default() -> Self {
        Self {
            walk_speed: 160.0,
            run_speed: 300.0,
            sprint_multiplier: 1.5,
            horizontal_acceleration: 1800.0,
            gravity: GRAVITY_CREATION_UNITS,
            jump_launch: 340.0,
            capsule_radius: 28.0,
            capsule_standing_height: 100.8,
            eye_height: 89.6,
            slope_climb_degrees: 50.0,
            slope_slide_degrees: 55.0,
            autostep_height: 36.0,
            ground_snap: 12.0,
            record_speeds: None,
        }
    }
}

impl MovementTuning {
    pub fn capsule_half_height(&self) -> f32 {
        (self.capsule_standing_height * 0.5 - self.capsule_radius).max(1.0)
    }

    /// Legacy packages lack `movement_types`. A package with the table must
    /// contain a valid selected profile; otherwise it cannot claim record speeds.
    pub fn from_world_database(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .wrap_err_with(|| format!("opening movement records in {}", path.display()))?;
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='movement_types')",
            [], |row| row.get(0),
        )?;
        if !has_table {
            return Ok(Self::default());
        }
        let race_id: u32 = conn
            .query_row("SELECT race_id FROM npcs WHERE id=7", [], |row| row.get(0))
            .wrap_err("record-backed movement needs Player NPC_ 00000007 RNAM")?;
        let race_type: String = conn
            .query_row(
                "SELECT record_type FROM records WHERE form_id=?1",
                [race_id],
                |row| row.get(0),
            )
            .wrap_err_with(|| format!("Player NPC_ 00000007 RNAM {race_id:08X} is missing"))?;
        ensure!(
            race_type == "RACE",
            "Player NPC_ 00000007 RNAM {race_id:08X} is {race_type}, expected RACE"
        );
        let (walk_link, run_link): (Option<u32>, Option<u32>) = conn
            .query_row(
                "SELECT walk_movt_id, run_movt_id FROM race_movement_links WHERE id=?1",
                [race_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .wrap_err_with(|| {
                format!("RACE {race_id:08X} movement links missing; reconvert assets")
            })?;
        ensure!(
            walk_link.is_none() && run_link.is_none(),
            "RACE {race_id:08X} has WKMV={walk_link:?} RNMV={run_link:?}; first-pass NPC_Default_MT selection supports only unset race links"
        );
        let selected: Option<(String, [f32; 8], String)> = conn
            .query_row(
                "SELECT m.editor_id, m.left_walk, m.left_run, m.right_walk, m.right_run, \
                    m.forward_walk, m.forward_run, m.back_walk, m.back_run, \
                    COALESCE(p.name, '<unknown plugin>') \
             FROM movement_types m LEFT JOIN plugins p ON p.id=m.load_order \
             WHERE m.id=?1",
                params![NPC_DEFAULT_MOVT_FORM_ID],
                |row| {
                    Ok((
                        row.get(0)?,
                        [
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ],
                        row.get(9)?,
                    ))
                },
            )
            .optional()?;
        let (editor_id, values, plugin) = selected.ok_or_else(|| color_eyre::eyre::eyre!(
            "movement_types exists but MOVT 0003580D NPC_Default_MT is missing; reconvert assets"
        ))?;
        ensure!(
            editor_id == "NPC_Default_MT",
            "MOVT 0003580D EDID is {editor_id}, expected NPC_Default_MT"
        );
        for (index, value) in values.iter().enumerate() {
            ensure!(
                value.is_finite() && *value > 0.0,
                "MOVT 0003580D SPED[{index}] is invalid: {value}"
            );
        }
        Ok(Self {
            walk_speed: values[4],
            run_speed: values[5],
            record_speeds: Some(DirectionalMovementSpeeds {
                directions: [
                    [values[0], values[1]],
                    [values[2], values[3]],
                    [values[4], values[5]],
                    [values[6], values[7]],
                ],
                source: format!("MOVT 0003580D NPC_Default_MT ({plugin})"),
            }),
            ..Self::default()
        })
    }

    fn target_speed(&self, strafe: Vec2, slow: bool, sprint: bool) -> f32 {
        let gait = if slow && !sprint { 0 } else { 1 };
        let Some(speeds) = &self.record_speeds else {
            let base = if gait == 0 {
                self.walk_speed
            } else {
                self.run_speed
            };
            return if sprint {
                base * self.sprint_multiplier
            } else {
                base
            };
        };
        let x = strafe.x.abs();
        let y = strafe.y.abs();
        let lateral = speeds.directions[if strafe.x < 0.0 { 0 } else { 1 }][gait];
        let longitudinal = speeds.directions[if strafe.y < 0.0 { 2 } else { 3 }][gait];
        let base = (lateral * x + longitudinal * y) / (x + y).max(1.0);
        if sprint {
            base * self.sprint_multiplier
        } else {
            base
        }
    }
}

/// Debug tankard fixture bounds in Creation units (V15).
pub const TANKARD_CUP_RADIUS: f32 = 14.0;
pub const TANKARD_CUP_HALF_HEIGHT: f32 = 18.0;
pub const TANKARD_HANDLE_RADIUS: f32 = 4.0;
pub const TANKARD_HANDLE_HALF_HEIGHT: f32 = 12.0;
/// P2+ cap on live spawned tankards (V15).
pub const MAX_LIVE_TANKARDS: usize = 32;

/// Marker for the asset-free debug tankard physics fixture (V15).
#[derive(Component, Debug, Clone, Copy)]
pub struct DebugTankard;

/// Marker for fixture arena geometry (P1 primitive hill/wall arena).
#[derive(Component, Debug, Clone, Copy)]
pub struct FixtureArena;

/// Groups every collider belongs to / collides with (V17).
pub fn world_collision_groups() -> CollisionGroups {
    CollisionGroups::new(GROUP_WORLD, GROUP_PLAYER | GROUP_TANKARD | GROUP_CLUTTER)
}

pub fn player_collision_groups() -> CollisionGroups {
    CollisionGroups::new(GROUP_PLAYER, GROUP_WORLD | GROUP_TANKARD | GROUP_CLUTTER)
}

pub fn tankard_collision_groups() -> CollisionGroups {
    CollisionGroups::new(
        GROUP_TANKARD,
        GROUP_WORLD | GROUP_PLAYER | GROUP_TANKARD | GROUP_CLUTTER,
    )
}

/// Placed dynamic clutter collides with the world, the player, tankards and other clutter.
pub fn clutter_collision_groups() -> CollisionGroups {
    CollisionGroups::new(
        GROUP_CLUTTER,
        GROUP_WORLD | GROUP_PLAYER | GROUP_TANKARD | GROUP_CLUTTER,
    )
}

/// A placed reference simulated as a dynamic Rapier body from its authored physics data (#104
/// phase a).
///
/// The object's stable ID is the [`crate::world::components::FormId`] already on the reference
/// entity. The resting pose is not persisted (#90), and the body despawns with its cell.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct DynamicClutter {
    /// Authored cap on linear speed, Creation units/s.
    pub max_linear_velocity: f32,
    /// Authored cap on angular speed, rad/s.
    pub max_angular_velocity: f32,
}

/// Mass properties for an authored body, or `None` when the mass or tensor is unusable (the
/// caller then falls back to collider density).
///
/// The tensor must be finite, symmetric and positive definite, the centre of mass finite and the
/// mass finite and above zero. Havok values come in the shape frame already (the converter bakes
/// the node and rigid-body transforms), in Creation units and kg x Creation units squared.
pub fn mass_properties_from_body(body: &CollisionBody) -> Option<MassProperties> {
    if !body.mass.is_finite() || body.mass <= 0.0 {
        return None;
    }
    if !body
        .inertia
        .iter()
        .chain(&body.center_of_mass)
        .all(|value| value.is_finite())
    {
        return None;
    }
    let i = body.inertia.map(f64::from);
    // Row-major 3x3. Sylvester's criterion on the leading minors plus a symmetry check.
    let symmetric = |a: f64, b: f64| (a - b).abs() <= 1e-4 * (a.abs() + b.abs()).max(1e-12);
    if !(symmetric(i[1], i[3]) && symmetric(i[2], i[6]) && symmetric(i[5], i[7])) {
        return None;
    }
    let minor2 = i[0] * i[4] - i[1] * i[3];
    let det = i[0] * (i[4] * i[8] - i[5] * i[7]) - i[1] * (i[3] * i[8] - i[5] * i[6])
        + i[2] * (i[3] * i[7] - i[4] * i[6]);
    if !(i[0] > 0.0 && minor2 > 0.0 && det > 0.0) {
        return None;
    }
    // Bevy's Mat3 is column-major; the tensor is symmetric, so rows and columns agree.
    let matrix = Mat3::from_cols_array(&body.inertia);
    Some(MassProperties::from_rapier(
        bevy_rapier3d::rapier::dynamics::MassProperties::with_inertia_matrix(
            Vec3::from_array(body.center_of_mass),
            body.mass,
            matrix,
        ),
    ))
}

/// Make `entity` a dynamic Rapier body from authored physics data (#104 phase a).
///
/// `parts` are the body's collider shapes and their translations in the entity's frame, already
/// built by the caller (a hull for a non-convex body). `mass` is the authored tensor; when it is
/// `None` (unusable values) collider density supplies the mass instead. `scale` is the
/// reference's uniform scale: Rapier scales the colliders from the `GlobalTransform` but not
/// [`AdditionalMassProperties`], so the authored centre of mass is scaled by it and the inertia by
/// its square, keeping them in step with the scaled shapes (the authored mass stays as it is). An
/// unusable scale falls back to collider density. Every collider carries
/// zero mass when the authored tensor is used, because collider mass is *added* to
/// [`AdditionalMassProperties`] rather than replacing it.
///
/// The body sits on the reference entity itself, which is a descendant of the cell root: it moves
/// with the root and despawns with the cell.
/// The character controller does not push dynamic bodies yet (`apply_impulse_to_dynamic_bodies`
/// is false), so the player collides with clutter but cannot shove it in phase (a).
pub fn spawn_dynamic_clutter<'a>(
    commands: &mut Commands,
    entity: Entity,
    body: &CollisionBody,
    parts: impl IntoIterator<Item = (Vec3, &'a Collider)>,
    mass: Option<MassProperties>,
    scale: f32,
) {
    let finite_or = |value: f32, default: f32| {
        if value.is_finite() && value >= 0.0 {
            value
        } else {
            default
        }
    };
    let mut entity_commands = commands.entity(entity);
    entity_commands.insert((
        RigidBody::Dynamic,
        DynamicClutter {
            max_linear_velocity: body.max_linear_velocity,
            max_angular_velocity: body.max_angular_velocity,
        },
        Damping {
            linear_damping: finite_or(body.linear_damping, 0.0),
            angular_damping: finite_or(body.angular_damping, 0.0),
        },
        Velocity::zero(),
        // Asleep until something touches it: each model becomes ready on its own, so the shelf or
        // terrain under a body may still be loading (or its collider disabled) on the first step,
        // and an awake body would fall through it. A contact, a grab or a set velocity wakes it.
        Sleeping {
            sleeping: true,
            ..default()
        },
        // The raised global speed cap lets a fast body cover more than a floor's thickness in
        // one 60 Hz step, so clutter gets continuous collision detection against the world.
        Ccd::enabled(),
    ));
    let collider_mass = match mass {
        Some(mut properties) if scale.is_finite() && scale > 0.0 => {
            properties.local_center_of_mass *= scale;
            properties.principal_inertia *= scale * scale;
            entity_commands.insert(AdditionalMassProperties::MassProperties(properties));
            ColliderMassProperties::Mass(0.0)
        }
        _ => ColliderMassProperties::Density(0.001),
    };
    // The Havok deactivator type is not mapped in phase (a): past the asleep start, Rapier's
    // default sleeping applies.
    let friction = Friction::coefficient(finite_or(body.friction, 0.5));
    let restitution = Restitution::coefficient(finite_or(body.restitution, 0.0));
    let parts: Vec<(Vec3, &Collider)> = parts.into_iter().collect();
    if let [(translation, collider)] = parts.as_slice()
        && *translation == Vec3::ZERO
    {
        commands.entity(entity).insert((
            (*collider).clone(),
            collider_mass,
            friction,
            restitution,
            clutter_collision_groups(),
        ));
        return;
    }
    for (translation, collider) in parts {
        commands.spawn((
            (*collider).clone(),
            collider_mass,
            friction,
            restitution,
            clutter_collision_groups(),
            Transform::from_translation(translation),
            ChildOf(entity),
        ));
    }
}

/// Clamp each dynamic clutter body to its authored max linear and angular speed.
///
/// Rapier has one global linear cap and no per-body or angular cap. Running after the Rapier
/// writeback means the corrected [`Velocity`] reaches the body at the next step. Clamps are
/// counted in `StreamingMetrics::dynamic_clutter_clamped`.
pub fn clamp_dynamic_clutter_velocities(
    mut bodies: Query<(&DynamicClutter, &mut Velocity)>,
    metrics: Option<ResMut<StreamingMetrics>>,
) {
    let mut clamped = 0u64;
    for (limits, mut velocity) in &mut bodies {
        let mut capped = *velocity;
        let linear = capped.linear.length();
        if limits.max_linear_velocity.is_finite()
            && limits.max_linear_velocity > 0.0
            && linear > limits.max_linear_velocity
        {
            capped.linear *= limits.max_linear_velocity / linear;
        }
        let angular = capped.angular.length();
        if limits.max_angular_velocity.is_finite()
            && limits.max_angular_velocity > 0.0
            && angular > limits.max_angular_velocity
        {
            capped.angular *= limits.max_angular_velocity / angular;
        }
        if capped != *velocity {
            *velocity = capped;
            clamped += 1;
        }
    }
    if clamped > 0
        && let Some(mut metrics) = metrics
    {
        metrics.dynamic_clutter_clamped = metrics.dynamic_clutter_clamped.saturating_add(clamped);
    }
}

/// Compound cup + handle collider for the debug tankard (R8, V15).
pub fn debug_tankard_collider() -> Collider {
    Collider::compound(vec![
        (
            Vect::ZERO,
            Rot::default(),
            Collider::cylinder(TANKARD_CUP_HALF_HEIGHT, TANKARD_CUP_RADIUS),
        ),
        (
            Vect::new(TANKARD_CUP_RADIUS + TANKARD_HANDLE_RADIUS, 0.0, 0.0),
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            Collider::cylinder(TANKARD_HANDLE_HALF_HEIGHT, TANKARD_HANDLE_RADIUS),
        ),
    ])
}

/// Rapier setup shared by the fixture and (later) streamed collision (V17).
pub struct PhysicsCorePlugin;

impl Plugin for PhysicsCorePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TimestepMode::Fixed {
            dt: PHYSICS_TIMESTEP,
            substeps: 1,
        })
        .insert_resource(Time::<Fixed>::from_hz(60.0))
        .init_resource::<MovementTuning>()
        .add_plugins(RapierPhysicsPlugin::<NoUserData>::default().in_fixed_schedule())
        .add_systems(Startup, (configure_physics_gravity, report_movement_tuning))
        .add_systems(
            FixedUpdate,
            clamp_dynamic_clutter_velocities.after(PhysicsSet::Writeback),
        );
    }
}

fn report_movement_tuning(tuning: Res<MovementTuning>) {
    let source = tuning
        .record_speeds
        .as_ref()
        .map_or("legacy provisional constants", |s| s.source.as_str());
    info!(
        "WALK speeds: forward walk={} run={} Creation units/s (first-pass factor 1); provisional sprint={} ({}x directional run); source={source}; jump_launch={} and gravity={} provisional",
        tuning.walk_speed,
        tuning.run_speed,
        tuning.target_speed(Vec2::new(0.0, -1.0), false, true),
        tuning.sprint_multiplier,
        tuning.jump_launch,
        tuning.gravity
    );
}

fn configure_physics_gravity(
    tuning: Res<MovementTuning>,
    mut configs: Query<&mut RapierConfiguration>,
    mut simulations: Query<&mut RapierContextSimulation>,
) {
    for mut config in &mut configs {
        config.gravity = Vect::new(0.0, -tuning.gravity, 0.0);
    }
    // Raise Rapier's global linear cap (default 400 units/s) so falling clutter is not clipped;
    // the authored per-body limits are enforced by `clamp_dynamic_clutter_velocities`.
    for mut simulation in &mut simulations {
        simulation
            .integration_parameters
            .normalized_max_linear_velocity = MAX_LINEAR_SPEED;
    }
}

/// Asset-free `--physics-fixture`: primitive arena + auto-spawned tankards.
pub struct PhysicsFixturePlugin;

impl Plugin for PhysicsFixturePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(PlayerControlsPlugin)
            .init_resource::<PhysicsFixtureState>()
            .add_systems(
                Startup,
                (setup_physics_fixture, setup_fixture_player).chain(),
            )
            .add_systems(FixedUpdate, validate_physics_fixture);
    }
}

/// Mouse and movement systems shared by Riverwood and the regression fixture.
pub struct PlayerControlsPlugin;

/// Complete camera/player update, before streaming reads or rebases their render positions.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PlayerControlsSystems;

impl Plugin for PlayerControlsPlugin {
    fn build(&self, app: &mut App) {
        register_move_mode_effect(app.world_mut());
        app.add_plugins(PhysicsCorePlugin)
            .init_resource::<MoveMode>()
            .init_resource::<WalkIntent>()
            .init_resource::<SprintLatch>()
            .init_resource::<LookIntent>()
            .init_resource::<WalkEntryStatus>()
            .init_resource::<WalkSpawnHold>()
            .init_resource::<WalkCameraFollowState>()
            .init_resource::<WalkCameraTick>()
            .init_resource::<CursorCapture>()
            .add_message::<TeleportPlayer>()
            .add_console_command(toggle_collision_command())
            .add_systems(
                Update,
                (
                    cursor_lifecycle_system,
                    apply_teleport_player,
                    look_input_system,
                    noclip_flight_system.run_if(crate::door_crossing::no_door_crossing),
                    // Nothing walks during a door crossing: the landing has no collision yet.
                    walk_intent_system.run_if(crate::door_crossing::no_door_crossing),
                    toggle_mode_system.run_if(console_closed),
                    walk_camera_follow_system,
                    sprint_fov_system,
                    overlay_system,
                )
                    .chain()
                    .after(crate::settings_ui::SettingsUiSet)
                    .in_set(PlayerControlsSystems),
            )
            .add_systems(
                FixedUpdate,
                // Input stays paused throughout a crossing. Placement and gravity settle behind
                // black once destination assets are ready, before the crossing may fade in.
                (release_walk_spawn_hold, walk_movement_system)
                    .chain()
                    .run_if(crate::door_crossing::walk_physics_allowed)
                    .run_if(console_closed)
                    .before(PhysicsSet::SyncBackend),
            )
            .add_systems(
                FixedUpdate,
                sample_walk_camera_tick.after(PhysicsSet::Writeback),
            );
    }
}

pub struct WorldPlayerPlugin;

impl Plugin for WorldPlayerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(PlayerControlsPlugin)
            .add_plugins(
                RapierDebugRenderPlugin {
                    default_collider_debug: ColliderDebug::NeverRender,
                    mode: DebugRenderMode::COLLIDER_SHAPES,
                    ..default()
                }
                .disabled(),
            )
            .init_resource::<HeldTankard>()
            .add_console_command(toggle_collision_geometry_command())
            .add_systems(PostStartup, setup_world_player)
            .add_systems(
                Update,
                update_collision_debug_visibility
                    .after(crate::settings_ui::SettingsUiSet)
                    .before(overlay_system),
            )
            .add_systems(
                Update,
                (
                    world_tankard_input.run_if(console_closed),
                    move_held_tankard,
                )
                    .chain()
                    .after(crate::settings_ui::SettingsUiSet),
            );
    }
}

#[derive(Resource, Default)]
struct HeldTankard(Option<Entity>);

#[derive(Resource)]
struct TankardVisuals {
    cup: Handle<Mesh>,
    handle: Handle<Mesh>,
    wood: Handle<StandardMaterial>,
}

fn tankard_visuals(
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> TankardVisuals {
    TankardVisuals {
        cup: meshes.add(Cylinder::new(
            TANKARD_CUP_RADIUS,
            TANKARD_CUP_HALF_HEIGHT * 2.0,
        )),
        handle: meshes.add(Cylinder::new(
            TANKARD_HANDLE_RADIUS,
            TANKARD_HANDLE_HALF_HEIGHT * 2.0,
        )),
        wood: materials.add(fixture_material(Color::srgb(0.5, 0.32, 0.14))),
    }
}

fn spawn_debug_tankard(commands: &mut Commands, visuals: &TankardVisuals, position: Vec3) {
    commands
        .spawn((
            Name::new("Debug tankard"),
            DebugTankard,
            RigidBody::Dynamic,
            debug_tankard_collider(),
            ActiveEvents::COLLISION_EVENTS,
            CollidingEntities::default(),
            tankard_collision_groups(),
            ColliderMassProperties::Density(0.001),
            Velocity::zero(),
            // The raised global speed cap (`MAX_LINEAR_SPEED`) no longer clips a falling tankard
            // at 400 units/s, so it gets continuous collision detection like clutter does.
            Ccd::enabled(),
            Transform::from_translation(position),
            Visibility::default(),
        ))
        .with_children(|parent| {
            parent.spawn((
                Mesh3d(visuals.cup.clone()),
                MeshMaterial3d(visuals.wood.clone()),
                Transform::IDENTITY,
            ));
            parent.spawn((
                Mesh3d(visuals.handle.clone()),
                MeshMaterial3d(visuals.wood.clone()),
                Transform::from_xyz(TANKARD_CUP_RADIUS + TANKARD_HANDLE_RADIUS, 0.0, 0.0)
                    .with_rotation(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
            ));
        });
}

#[derive(Resource, Default)]
pub struct PhysicsFixtureState {
    pub ticks: u64,
    pub finished: bool,
    pub tankards_settled: bool,
}

fn fixture_material(base_color: Color) -> StandardMaterial {
    StandardMaterial {
        base_color,
        perceptual_roughness: 0.9,
        ..default()
    }
}

fn setup_physics_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<PhysicsFixtureMetrics>();
    let stone = materials.add(fixture_material(Color::srgb(0.42, 0.44, 0.48)));
    let grass = materials.add(fixture_material(Color::srgb(0.24, 0.5, 0.2)));
    let hazard = materials.add(fixture_material(Color::srgb(0.85, 0.3, 0.12)));

    // Flat ground pad plus a sloped hill, a step, and a blocking wall.
    let ground_mesh = meshes.add(Cuboid::new(2400.0, 40.0, 2400.0));
    commands.spawn((
        Name::new("Physics fixture ground"),
        FixtureArena,
        Mesh3d(ground_mesh),
        MeshMaterial3d(grass),
        Transform::from_xyz(0.0, -20.0, 0.0),
        RigidBody::Fixed,
        Collider::cuboid(1200.0, 20.0, 1200.0),
        world_collision_groups(),
    ));
    let hill_mesh = meshes.add(Cuboid::new(1200.0, 40.0, 600.0));
    commands.spawn((
        Name::new("Physics fixture hill"),
        FixtureArena,
        Mesh3d(hill_mesh),
        MeshMaterial3d(stone.clone()),
        Transform::from_xyz(-700.0, 190.0, -500.0).with_rotation(Quat::from_rotation_z(-0.32)),
        RigidBody::Fixed,
        Collider::cuboid(600.0, 20.0, 300.0),
        world_collision_groups(),
    ));
    let step_mesh = meshes.add(Cuboid::new(220.0, 24.0, 220.0));
    commands.spawn((
        Name::new("Physics fixture step"),
        FixtureArena,
        Mesh3d(step_mesh),
        MeshMaterial3d(stone.clone()),
        Transform::from_xyz(320.0, 12.0, 240.0),
        RigidBody::Fixed,
        Collider::cuboid(110.0, 12.0, 110.0),
        world_collision_groups(),
    ));
    let wall_mesh = meshes.add(Cuboid::new(60.0, 400.0, 1200.0));
    commands.spawn((
        Name::new("Physics fixture wall"),
        FixtureArena,
        Mesh3d(wall_mesh),
        MeshMaterial3d(hazard),
        Transform::from_xyz(760.0, 200.0, 0.0),
        RigidBody::Fixed,
        Collider::cuboid(30.0, 200.0, 600.0),
        world_collision_groups(),
    ));

    // Visible cup + handle meshes; collider stays compound convex (V15).
    let visuals = tankard_visuals(&mut meshes, &mut materials);
    for position in [
        Vec3::new(-700.0, 420.0, -500.0),
        Vec3::new(-640.0, 480.0, -420.0),
        Vec3::new(120.0, 320.0, 120.0),
    ] {
        spawn_debug_tankard(&mut commands, &visuals, position);
    }

    commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Transform::from_xyz(0.0, 900.0, 1900.0).looking_at(Vec3::new(0.0, 0.0, -200.0), Vec3::Y),
        StreamingCamera,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        brightness: 160.0,
        ..default()
    });
}

#[derive(Resource, Default)]
pub struct PhysicsFixtureMetrics {
    pub tankards_spawned: usize,
    pub tankards_in_contact: usize,
    pub settled_ticks: u64,
}

fn validate_physics_fixture(
    mut state: ResMut<PhysicsFixtureState>,
    mut metrics: ResMut<PhysicsFixtureMetrics>,
    mut profiler: ResMut<ProfilingState>,
    tankards: Query<(Entity, &Transform, Option<&CollidingEntities>), With<DebugTankard>>,
    mut streaming: ResMut<StreamingMetrics>,
) {
    if state.finished {
        return;
    }
    state.ticks = state.ticks.saturating_add(1);
    metrics.tankards_spawned = tankards.iter().count();
    metrics.tankards_in_contact = tankards
        .iter()
        .filter(|(_, _, contacts)| contacts.is_some_and(|contacts| !contacts.is_empty()))
        .count();
    let settled = tankards.iter().all(|(_, transform, _)| {
        transform.translation.is_finite() && transform.translation.y > -400.0
    });
    if settled && metrics.tankards_in_contact == metrics.tankards_spawned {
        metrics.settled_ticks = metrics.settled_ticks.saturating_add(1);
    } else {
        metrics.settled_ticks = 0;
    }
    if metrics.settled_ticks >= 60 {
        state.tankards_settled = true;
        state.finished = true;
        streaming.physics_fixture_validated = true;
        profiler.event("physics-fixture", "tankards_settled", None);
    } else if state.ticks >= 60 * 30 {
        state.finished = true;
        streaming.physics_fixture_failures = streaming.physics_fixture_failures.saturating_add(1);
        profiler.event("physics-fixture", "failed", None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn movement_tuning_preserves_spec_speed_ordering() {
        let tuning = MovementTuning::default();
        let forward = Vec2::new(0.0, -1.0);
        let walk = tuning.target_speed(forward, true, false);
        let run = tuning.target_speed(forward, false, false);
        let sprint = tuning.target_speed(forward, false, true);
        assert!(walk < run && run < sprint);
        assert_eq!(sprint, run * tuning.sprint_multiplier);
        assert!(tuning.horizontal_acceleration > 0.0);
        assert_eq!(tuning.gravity, GRAVITY_CREATION_UNITS);
        assert_eq!(tuning.capsule_standing_height, 100.8);
        assert_eq!(tuning.eye_height, 89.6);
        assert_eq!(tuning.capsule_radius, 28.0);
    }

    #[test]
    fn packaged_movement_profile_resolves_and_legacy_package_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("skyrim_world.db");
        let conn = Connection::open(&path).unwrap();
        let legacy = MovementTuning::from_world_database(&path).unwrap();
        assert_eq!(legacy.run_speed, 300.0);
        assert!(legacy.record_speeds.is_none());
        conn.execute_batch(
            "CREATE TABLE npcs(id INTEGER PRIMARY KEY, race_id INTEGER);
            CREATE TABLE records(form_id INTEGER PRIMARY KEY, record_type TEXT);
            CREATE TABLE plugins(id INTEGER PRIMARY KEY, name TEXT);
            CREATE TABLE race_movement_links(id INTEGER PRIMARY KEY, walk_movt_id INTEGER, run_movt_id INTEGER);
            CREATE TABLE movement_types(id INTEGER PRIMARY KEY, editor_id TEXT,
                left_walk REAL, left_run REAL, right_walk REAL, right_run REAL,
                forward_walk REAL, forward_run REAL, back_walk REAL, back_run REAL,
                load_order INTEGER);
            INSERT INTO npcs VALUES(7, 79686);
            INSERT INTO records VALUES(79686, 'RACE');
            INSERT INTO race_movement_links VALUES(79686, NULL, NULL);
            INSERT INTO plugins VALUES(0, 'Skyrim.esm');
            INSERT INTO movement_types VALUES(219149, 'NPC_Default_MT',
                80.09, 370, 79.75, 370, 80.1, 370, 71.93, 205.25, 0);",
        )
        .unwrap();
        let tuning = MovementTuning::from_world_database(&path).unwrap();
        assert_eq!(tuning.walk_speed, 80.1);
        assert_eq!(tuning.run_speed, 370.0);
        assert_eq!(
            tuning.target_speed(Vec2::new(0.0, -1.0), false, true),
            555.0
        );
        assert_eq!(
            tuning.target_speed(Vec2::new(0.0, 1.0), false, false),
            205.25
        );
        assert_eq!(
            tuning.target_speed(Vec2::new(-1.0, 0.0), true, false),
            80.09
        );
        assert_eq!(
            tuning.target_speed(Vec2::new(1.0, -1.0), false, false),
            370.0
        );
        assert!(tuning.record_speeds.unwrap().source.contains("Skyrim.esm"));
        conn.execute(
            "UPDATE records SET record_type='STAT' WHERE form_id=79686",
            [],
        )
        .unwrap();
        assert!(
            MovementTuning::from_world_database(&path)
                .unwrap_err()
                .to_string()
                .contains("expected RACE")
        );
        conn.execute(
            "UPDATE records SET record_type='RACE' WHERE form_id=79686",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE race_movement_links SET walk_movt_id=219149 WHERE id=79686",
            [],
        )
        .unwrap();
        assert!(
            MovementTuning::from_world_database(&path)
                .unwrap_err()
                .to_string()
                .contains("supports only unset race links")
        );
        conn.execute(
            "UPDATE race_movement_links SET walk_movt_id=NULL WHERE id=79686",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM movement_types", []).unwrap();
        assert!(
            MovementTuning::from_world_database(&path)
                .unwrap_err()
                .to_string()
                .contains("MOVT 0003580D")
        );
    }

    #[test]
    fn tankard_collider_is_compound_cup_and_handle() {
        let collider = debug_tankard_collider();
        let compound = collider.as_compound().expect("compound tankard");
        assert_eq!(compound.shapes().len(), 2);
    }

    #[test]
    fn collision_groups_keep_world_player_tankards_in_contact() {
        let world = world_collision_groups();
        let player = player_collision_groups();
        let tankard = tankard_collision_groups();
        assert!(world.filters.contains(GROUP_PLAYER));
        assert!(world.filters.contains(GROUP_TANKARD));
        assert!(player.filters.contains(GROUP_WORLD));
        assert!(tankard.filters.contains(GROUP_WORLD));
        assert!(tankard.filters.contains(GROUP_PLAYER));
    }

    #[test]
    fn clutter_group_collides_with_world_player_tankards_and_clutter() {
        let clutter = clutter_collision_groups();
        for group in [GROUP_WORLD, GROUP_PLAYER, GROUP_TANKARD, GROUP_CLUTTER] {
            assert!(clutter.filters.contains(group));
        }
        assert!(world_collision_groups().filters.contains(GROUP_CLUTTER));
        assert!(player_collision_groups().filters.contains(GROUP_CLUTTER));
        assert!(tankard_collision_groups().filters.contains(GROUP_CLUTTER));
        assert_eq!(clutter.memberships, GROUP_CLUTTER);
    }

    fn test_body(mass: f32, inertia: [f32; 9]) -> CollisionBody {
        CollisionBody {
            node: 0,
            target: String::new(),
            shapes: vec![0],
            kind: shared::collision::BodyKind::Dynamic,
            havok: shared::collision::HavokBodyInfo {
                motion_system: 3,
                quality_type: 4,
                deactivator_type: 1,
                collision_layer: 4,
            },
            mass,
            inertia,
            center_of_mass: [0.0, 2.0, 0.0],
            linear_damping: 0.0,
            angular_damping: 0.0,
            friction: 0.5,
            restitution: 0.0,
            max_linear_velocity: 100.0,
            max_angular_velocity: 5.0,
            convex: true,
        }
    }

    #[test]
    fn mass_properties_follow_the_authored_tensor_and_reject_unusable_ones() {
        let good = mass_properties_from_body(&test_body(
            2.0,
            [3.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 5.0],
        ))
        .expect("positive definite tensor");
        assert!((good.mass - 2.0).abs() < 1.0e-6);
        assert!((good.local_center_of_mass.y - 2.0).abs() < 1.0e-6);
        let mut principal = [
            good.principal_inertia.x,
            good.principal_inertia.y,
            good.principal_inertia.z,
        ];
        principal.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for (got, want) in principal.iter().zip([3.0, 4.0, 5.0]) {
            assert!((got - want).abs() < 1.0e-4, "{principal:?}");
        }
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        assert!(mass_properties_from_body(&test_body(0.0, identity)).is_none());
        assert!(mass_properties_from_body(&test_body(f32::NAN, identity)).is_none());
        assert!(mass_properties_from_body(&test_body(1.0, [0.0; 9])).is_none());
        // Not positive definite: a negative principal moment.
        assert!(
            mass_properties_from_body(&test_body(
                1.0,
                [1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 1.0]
            ))
            .is_none()
        );
        // Not symmetric.
        assert!(
            mass_properties_from_body(&test_body(
                1.0,
                [1.0, 0.5, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
            ))
            .is_none()
        );
    }

    #[test]
    fn clamp_caps_authored_max_velocity_and_counts_it() {
        let mut world = World::new();
        world.init_resource::<StreamingMetrics>();
        let limits = DynamicClutter {
            max_linear_velocity: 100.0,
            max_angular_velocity: 5.0,
        };
        let fast = world
            .spawn((
                limits,
                Velocity {
                    linear: Vec3::new(300.0, 400.0, 0.0),
                    angular: Vec3::new(0.0, 20.0, 0.0),
                },
            ))
            .id();
        let slow = world
            .spawn((
                limits,
                Velocity {
                    linear: Vec3::new(10.0, 0.0, 0.0),
                    angular: Vec3::new(0.0, 1.0, 0.0),
                },
            ))
            .id();
        world
            .run_system_once(clamp_dynamic_clutter_velocities)
            .unwrap();
        let fast = *world.get::<Velocity>(fast).unwrap();
        assert!((fast.linear.length() - 100.0).abs() < 1.0e-3);
        assert!((fast.angular.length() - 5.0).abs() < 1.0e-3);
        // Direction is kept.
        assert!((fast.linear.normalize() - Vec3::new(0.6, 0.8, 0.0)).length() < 1.0e-4);
        let slow = *world.get::<Velocity>(slow).unwrap();
        assert_eq!(slow.linear, Vec3::new(10.0, 0.0, 0.0));
        assert_eq!(
            world.resource::<StreamingMetrics>().dynamic_clutter_clamped,
            1
        );
    }

    #[test]
    fn physics_core_raises_the_global_linear_speed_cap() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PhysicsCorePlugin));
        app.update();
        let mut query = app.world_mut().query::<&RapierContextSimulation>();
        let simulation = query.single(app.world()).expect("rapier context");
        assert_eq!(
            simulation
                .integration_parameters
                .normalized_max_linear_velocity,
            MAX_LINEAR_SPEED
        );
    }

    #[test]
    fn physics_core_uses_fixed_sixty_hertz_step() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PhysicsCorePlugin));
        app.update();
        let mode = *app.world().resource::<TimestepMode>();
        assert_eq!(
            mode,
            TimestepMode::Fixed {
                dt: PHYSICS_TIMESTEP,
                substeps: 1,
            }
        );
        assert!((PHYSICS_TIMESTEP - 1.0 / 60.0).abs() < f32::EPSILON);
        assert_eq!(
            app.world().resource::<Time<Fixed>>().timestep(),
            std::time::Duration::from_secs_f64(1.0 / 60.0)
        );
    }
}

/// Placed dynamic clutter from authored physics data (#104 phase a), on the fixture arena.
#[cfg(test)]
mod clutter_tests {
    use super::headless;
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use shared::collision::{BodyKind, HavokBodyInfo};

    fn authored_body(mass: f32, inertia: [f32; 9], center_of_mass: [f32; 3]) -> CollisionBody {
        CollisionBody {
            node: 1,
            target: "Clutter01".to_owned(),
            shapes: vec![0],
            kind: BodyKind::Dynamic,
            havok: HavokBodyInfo {
                motion_system: 3,
                quality_type: 4,
                deactivator_type: 1,
                collision_layer: 4,
            },
            mass,
            inertia,
            center_of_mass,
            linear_damping: 0.1,
            angular_damping: 0.05,
            friction: 0.6,
            restitution: 0.3,
            max_linear_velocity: 7_000.0,
            max_angular_velocity: 31.0,
            convex: true,
        }
    }

    /// A 20-unit box whose collider sits 10 units above its reference origin, as the converter
    /// writes a NIF box shape.
    fn box_body() -> CollisionBody {
        authored_body(
            2.0,
            [30.0, 0.0, 0.0, 0.0, 40.0, 0.0, 0.0, 0.0, 50.0],
            [0.0, 10.0, 0.0],
        )
    }

    /// A 100-unit square platform, heavy enough that a tankard does not move it.
    fn platform_body() -> CollisionBody {
        authored_body(
            200.0,
            [20_000.0, 0.0, 0.0, 0.0, 20_000.0, 0.0, 0.0, 0.0, 20_000.0],
            [0.0, 10.0, 0.0],
        )
    }

    /// Spawn a placed clutter reference (optionally under a cell root) with one authored collider.
    fn spawn_clutter(
        app: &mut App,
        body: CollisionBody,
        collider: Collider,
        center: Vec3,
        position: Vec3,
        velocity: Vec3,
        parent: Option<Entity>,
    ) -> Entity {
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                let mut entity = commands.spawn(Transform::from_translation(position));
                if let Some(parent) = parent {
                    entity.insert(ChildOf(parent));
                }
                let entity = entity.id();
                let mass = mass_properties_from_body(&body);
                spawn_dynamic_clutter(
                    &mut commands,
                    entity,
                    &body,
                    [(center, &collider)],
                    mass,
                    1.0,
                );
                commands.entity(entity).insert(Velocity {
                    linear: velocity,
                    angular: Vec3::ZERO,
                });
                entity
            })
            .unwrap()
    }

    /// Clutter starts asleep, so it stays where it was placed (its support may still be loading);
    /// once woken it falls and settles on the floor.
    #[test]
    fn dynamic_clutter_body_falls_and_settles_on_the_fixture_arena() {
        let mut app = headless::fixture_app();
        let entity = spawn_clutter(
            &mut app,
            box_body(),
            Collider::cuboid(10.0, 10.0, 10.0),
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::new(-300.0, 400.0, 400.0),
            Vec3::ZERO,
            None,
        );
        for _ in 0..60 {
            app.update();
        }
        let y = app.world().get::<Transform>(entity).unwrap().translation.y;
        assert!((y - 400.0).abs() < 1.0e-3, "asleep clutter moved to {y}");
        app.world_mut()
            .get_mut::<Sleeping>(entity)
            .expect("clutter carries Sleeping")
            .sleeping = false;
        for _ in 0..600 {
            app.update();
        }
        let y = app.world().get::<Transform>(entity).unwrap().translation.y;
        // The collider's centre is 10 above the reference origin and the floor top is y = 0.
        assert!(y.abs() < 3.0, "clutter rests at {y}");
        let velocity = app.world().get::<Velocity>(entity).unwrap();
        assert!(
            velocity.linear.length() < 5.0,
            "clutter still moving: {velocity:?}"
        );
    }

    #[test]
    fn dynamic_clutter_body_takes_the_authored_mass_tensor_and_surface_values() {
        let mut app = headless::fixture_app();
        let entity = spawn_clutter(
            &mut app,
            box_body(),
            Collider::cuboid(10.0, 10.0, 10.0),
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::new(-300.0, 500.0, 400.0),
            Vec3::ZERO,
            None,
        );
        for _ in 0..3 {
            app.update();
        }
        app.world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                let context = context.single().expect("rapier context");
                let handle = context.entity2body()[&entity];
                let body = context.rigidbody_set.bodies.get(handle).expect("body");
                let props = body.mass_properties().local_mprops;
                assert!((props.mass() - 2.0).abs() < 1.0e-3, "mass {}", props.mass());
                let inertia = props.principal_inertia();
                let mut sorted = [inertia.x, inertia.y, inertia.z];
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                for (got, want) in sorted.iter().zip([30.0, 40.0, 50.0]) {
                    assert!((got - want).abs() < 0.01, "inertia {sorted:?}");
                }
                assert!((props.local_com.y - 10.0).abs() < 1.0e-3);
                assert!((body.linear_damping() - 0.1).abs() < 1.0e-6);
                assert!((body.angular_damping() - 0.05).abs() < 1.0e-6);
            })
            .unwrap();
        // The surface values sit on the colliders: the body entity itself for a single centred
        // shape, otherwise its collider children.
        let world = app.world_mut();
        let mut colliders = world.query::<(Entity, Option<&ChildOf>, &Friction, &Restitution)>();
        let surfaces: Vec<(f32, f32)> = colliders
            .iter(world)
            .filter(|(collider, parent, _, _)| {
                *collider == entity || parent.is_some_and(|parent| parent.parent() == entity)
            })
            .map(|(_, _, friction, restitution)| (friction.coefficient, restitution.coefficient))
            .collect();
        assert!(!surfaces.is_empty(), "the body has colliders");
        for (friction, restitution) in surfaces {
            assert_eq!(friction, 0.6);
            assert_eq!(restitution, 0.3);
        }
    }

    /// A reference placed at scale 2 keeps its authored mass; the centre of mass moves with the
    /// scaled shape and the inertia grows with the square of the scale.
    #[test]
    fn a_scaled_reference_scales_the_authored_centre_of_mass_and_inertia() {
        let mut app = headless::fixture_app();
        let entity = app
            .world_mut()
            .run_system_once(move |mut commands: Commands| {
                let entity = commands
                    .spawn(
                        Transform::from_translation(Vec3::new(-300.0, 500.0, 400.0))
                            .with_scale(Vec3::splat(2.0)),
                    )
                    .id();
                let body = box_body();
                let mass = mass_properties_from_body(&body);
                let collider = Collider::cuboid(10.0, 10.0, 10.0);
                spawn_dynamic_clutter(
                    &mut commands,
                    entity,
                    &body,
                    [(Vec3::new(0.0, 10.0, 0.0), &collider)],
                    mass,
                    2.0,
                );
                entity
            })
            .unwrap();
        for _ in 0..3 {
            app.update();
        }
        app.world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                let context = context.single().expect("rapier context");
                let handle = context.entity2body()[&entity];
                let body = context.rigidbody_set.bodies.get(handle).expect("body");
                let props = body.mass_properties().local_mprops;
                assert!((props.mass() - 2.0).abs() < 1.0e-3, "mass {}", props.mass());
                let inertia = props.principal_inertia();
                let mut sorted = [inertia.x, inertia.y, inertia.z];
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                for (got, want) in sorted.iter().zip([120.0, 160.0, 200.0]) {
                    assert!((got - want).abs() < 0.05, "inertia {sorted:?}");
                }
                assert!((props.local_com.y - 20.0).abs() < 1.0e-3);
            })
            .unwrap();
    }

    #[test]
    fn dynamic_clutter_body_collides_with_a_tankard() {
        let mut app = headless::fixture_app();
        let base = Vec3::new(-300.0, 0.0, 400.0);
        let platform = spawn_clutter(
            &mut app,
            platform_body(),
            Collider::cuboid(50.0, 10.0, 50.0),
            Vec3::new(0.0, 10.0, 0.0),
            base,
            Vec3::ZERO,
            None,
        );
        let tankard = app
            .world_mut()
            .spawn((
                DebugTankard,
                RigidBody::Dynamic,
                debug_tankard_collider(),
                tankard_collision_groups(),
                ColliderMassProperties::Density(0.001),
                Velocity::zero(),
                Transform::from_translation(base + Vec3::Y * 300.0),
            ))
            .id();
        for _ in 0..400 {
            app.update();
        }
        let platform_top = app
            .world()
            .get::<Transform>(platform)
            .unwrap()
            .translation
            .y
            + 20.0;
        let tankard_y = app.world().get::<Transform>(tankard).unwrap().translation.y;
        // A tankard that fell through the clutter would rest on the floor, about 14-18 above it.
        assert!(
            tankard_y > platform_top + 10.0,
            "tankard {tankard_y} passed through the clutter top {platform_top}"
        );
    }

    #[test]
    fn a_falling_tankard_knocks_over_a_light_clutter_body() {
        let mut app = headless::fixture_app();
        let base = Vec3::new(-300.0, 0.0, 400.0);
        // A tall light body (1 kg) balanced on one end; the tankard drops onto its top corner.
        let mut light = authored_body(
            1.0,
            [400.0, 0.0, 0.0, 0.0, 400.0, 0.0, 0.0, 0.0, 400.0],
            [0.0, 40.0, 0.0],
        );
        light.restitution = 0.0;
        let clutter = spawn_clutter(
            &mut app,
            light,
            Collider::cuboid(5.0, 40.0, 5.0),
            Vec3::new(0.0, 40.0, 0.0),
            base,
            Vec3::ZERO,
            None,
        );
        for _ in 0..30 {
            app.update();
        }
        // Control: left alone for as long as the tankard gets below, the body stays standing, so
        // the rotation the assertion sees comes from the tankard, not from solver jitter.
        let settled = app.world().get::<Transform>(clutter).unwrap().rotation;
        for _ in 0..240 {
            app.update();
        }
        let before = app.world().get::<Transform>(clutter).unwrap().rotation;
        assert!(
            settled.angle_between(before) < 0.05,
            "the body tipped over on its own (rotation changed {})",
            settled.angle_between(before)
        );
        app.world_mut().spawn((
            DebugTankard,
            RigidBody::Dynamic,
            debug_tankard_collider(),
            tankard_collision_groups(),
            ColliderMassProperties::Density(0.01),
            Velocity {
                linear: Vec3::new(120.0, -50.0, 0.0),
                angular: Vec3::ZERO,
            },
            Transform::from_translation(base + Vec3::new(-40.0, 70.0, 0.0)),
        ));
        for _ in 0..240 {
            app.update();
        }
        let after = app.world().get::<Transform>(clutter).unwrap().rotation;
        assert!(
            before.angle_between(after) > 0.2,
            "the tankard did not move the clutter body (rotation changed {})",
            before.angle_between(after)
        );
    }

    #[test]
    fn dynamic_clutter_body_collides_with_the_player_capsule() {
        let mut app = headless::fixture_app();
        let base = Vec3::new(-300.0, 0.0, 400.0);
        let platform = spawn_clutter(
            &mut app,
            platform_body(),
            Collider::cuboid(50.0, 10.0, 50.0),
            Vec3::new(0.0, 10.0, 0.0),
            base,
            Vec3::ZERO,
            None,
        );
        for _ in 0..120 {
            app.update();
        }
        let top = app
            .world()
            .get::<Transform>(platform)
            .unwrap()
            .translation
            .y
            + 20.0;
        headless::place_player(&mut app, base + Vec3::Y * (top + 150.0));
        for _ in 0..240 {
            app.update();
        }
        let (pose, grounded) = headless::player_pose(&mut app);
        let tuning = MovementTuning::default();
        let rest = top + tuning.capsule_half_height() + tuning.capsule_radius;
        assert!(grounded, "player not grounded at {pose:?}");
        assert!(
            pose.y > rest - 4.0,
            "player {pose:?} sank through the clutter (rest {rest})"
        );
    }
}

// ---------------------------------------------------------------------------
// T2: WALK capsule, movement integration, collision-safe NOCLIP<->WALK toggle.
// ---------------------------------------------------------------------------

/// Player movement mode (V8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Resource)]
pub enum MoveMode {
    /// Flying camera, no collision.
    #[default]
    Noclip,
    /// Grounded kinematic capsule.
    Walk,
}

/// Reason the last NOCLIP->WALK entry failed (V8), surfaced in the overlay.
#[derive(Debug, Clone, Default, Resource)]
pub struct WalkEntryStatus {
    pub blocked_reason: Option<String>,
}

/// Bounded startup and destination protection while world collision reaches Rapier. A failed
/// load must leave debug flight available rather than freeze the player forever.
#[derive(Debug, Default, Resource)]
pub struct WalkSpawnHold {
    active: bool,
    elapsed: f32,
    sampled_ground_y: Option<f32>,
    destination: bool,
}

impl WalkSpawnHold {
    /// Whether safe placement still owns the walking body.
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }
}

/// Validated local terrain height for the initial interactive player pose.
#[derive(Debug, Default, Resource)]
pub struct WalkSpawnGroundHeight(pub Option<f32>);

const WALK_SPAWN_TIMEOUT_SECONDS: f32 = 30.0;
const WALK_SPAWN_MAX_PROBE_DISTANCE: f32 = 65_536.0;
const WALK_SPAWN_SURFACE_TOLERANCE: f32 = 64.0;

/// Read actual Rapier collision, not render/streaming completion counters. Run
/// before movement; colliders enabled by Update reach Rapier on a later tick.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn release_walk_spawn_hold(
    mut hold: ResMut<WalkSpawnHold>,
    mut mode: ResMut<MoveMode>,
    tuning: Res<MovementTuning>,
    context: ReadRapierContext,
    mut status: ResMut<WalkEntryStatus>,
    mut player: Query<(Entity, &mut Transform, &Collider), With<PlayerBody>>,
    mut reset: MotionReset,
    mut commands: Commands,
    crossing: Option<Res<crate::door_crossing::DoorCrossing>>,
) {
    if !hold.active || *mode != MoveMode::Walk {
        return;
    }
    hold.elapsed += PHYSICS_TIMESTEP;
    let Ok((entity, mut body, collider)) = player.single_mut() else {
        return;
    };
    let probe_distance =
        walk_spawn_probe_distance(body.translation.y, hold.sampled_ground_y, &tuning);
    let placement = context.single().ok().and_then(|context| {
        context
            .cast_ray(
                body.translation,
                Vec3::NEG_Y,
                probe_distance,
                true,
                QueryFilter::default().groups(CollisionGroups::new(GROUP_PLAYER, GROUP_WORLD)),
            )
            .filter(|hit| {
                hold.sampled_ground_y
                    .filter(|ground_y| ground_y.is_finite())
                    .is_none_or(|ground_y| {
                        // A loaded bridge or platform above the terrain is valid support too.
                        body.translation.y - hit.1 >= ground_y - WALK_SPAWN_SURFACE_TOLERANCE
                    })
            })
            .map(|_| {
                try_enter_walk(
                    &context,
                    &tuning,
                    body.translation + Vec3::Y * tuning.eye_height,
                    collider,
                )
            })
    });
    let result = match placement {
        Some(result) => result,
        None if hold.elapsed >= WALK_SPAWN_TIMEOUT_SECONDS => Err(if hold.destination {
            "WALK destination collision unavailable after 30 seconds; use V or tcl to retry WALK"
                .into()
        } else {
            "startup world collision unavailable after 30 seconds; use V or tcl to retry WALK"
                .into()
        }),
        None => return,
    };
    match result {
        Ok(position) => {
            body.translation = position;
            // Support from before placement cannot prove the new capsule is grounded.
            commands
                .entity(entity)
                .remove::<KinematicCharacterControllerOutput>();
            status.blocked_reason = None;
        }
        Err(reason) => {
            // A door owns its bounded rollback/recovery. Do not turn a failed WALK landing
            // into a successful NOCLIP arrival while the screen is still black.
            if crossing.is_some_and(|crossing| crossing.is_active()) {
                status.blocked_reason = Some(reason);
                return;
            }
            commands.entity(entity).insert(RigidBodyDisabled);
            *mode = MoveMode::Noclip;
            warn!("WALK collision fallback: {reason}");
            status.blocked_reason = Some(reason);
        }
    }
    *hold = WalkSpawnHold::default();
    reset.clear_input();
}

fn walk_spawn_probe_distance(
    body_y: f32,
    sampled_ground_y: Option<f32>,
    tuning: &MovementTuning,
) -> f32 {
    let fallback = tuning.capsule_standing_height * 2.0;
    let Some(ground_y) = sampled_ground_y.filter(|height| height.is_finite()) else {
        return fallback;
    };
    let distance_to_ground = body_y - ground_y;
    if !distance_to_ground.is_finite() {
        return 0.0;
    }
    (distance_to_ground + WALK_SPAWN_SURFACE_TOLERANCE).clamp(0.0, WALK_SPAWN_MAX_PROBE_DISTANCE)
}

/// Marker for the player body entity (V9).
#[derive(Component, Debug, Clone, Copy)]
pub struct PlayerBody;

/// Per-frame sampled movement intent; integrated once per physics tick (V18).
#[derive(Debug, Clone, Copy, Default, Resource)]
pub struct WalkIntent {
    /// Camera-yaw-relative wish direction, normalized, horizontal plane.
    pub wish_dir: Vec3,
    /// Desired top speed for current gait (walk/run/sprint).
    pub target_speed: f32,
    /// Fresh jump press this frame.
    pub jump_pressed: bool,
}

/// WALK sprint toggle; cleared whenever movement or controls stop.
#[derive(Debug, Clone, Copy, Default, Resource)]
pub(crate) struct SprintLatch(pub(crate) bool);

/// Live walk state: horizontal velocity + vertical velocity (V10, V11).
#[derive(Debug, Clone, Copy, Default, Component)]
pub struct WalkState {
    pub horizontal_velocity: Vec3,
    pub vertical_velocity: f32,
    pub grounded: bool,
    pub jump_apex: f32,
    pub jump_time: f32,
    pub jump_active: bool,
}

/// Upward search budget when entering WALK inside geometry (V8).
pub const WALK_ENTRY_SEARCH_STEPS: u32 = 12;
pub const WALK_ENTRY_STEP_HEIGHT: f32 = 28.0;

/// Build the upright player capsule + controller from tuning (V9, V12).
pub fn player_controller_bundle(tuning: &MovementTuning) -> impl Bundle {
    (
        PlayerBody,
        WalkState::default(),
        RigidBody::KinematicPositionBased,
        Collider::capsule_y(tuning.capsule_half_height(), tuning.capsule_radius),
        player_collision_groups(),
        KinematicCharacterController {
            up: Vect::Y,
            offset: CharacterLength::Absolute(PLAYER_CONTROLLER_OFFSET),
            slide: true,
            // WALK resolves steps with bounded capsule sweeps before SyncBackend.
            autostep: None,
            max_slope_climb_angle: tuning.slope_climb_degrees.to_radians(),
            min_slope_slide_angle: tuning.slope_slide_degrees.to_radians(),
            // NOTE: upstream Rapier 0.35 manifold-transfer panic (see T4 gate
            // hill test); re-enable with push validation once upgraded.
            apply_impulse_to_dynamic_bodies: false,
            snap_to_ground: Some(CharacterLength::Absolute(tuning.ground_snap)),
            ..default()
        },
    )
}

/// Pure movement integration: accelerate horizontal velocity toward wish dir,
/// apply gravity, handle jump from grounded + fresh press (V10, V11).
pub fn integrate_walk(
    state: &mut WalkState,
    intent: &WalkIntent,
    tuning: &MovementTuning,
    dt: f32,
    grounded: bool,
) {
    // Rapier's ground probe can remain true for several steps after takeoff.
    // Upward jump velocity must survive that proximity signal until ascent ends.
    let grounded = grounded && !(state.jump_active && state.vertical_velocity > 0.0);
    state.grounded = grounded;
    let wish = intent.wish_dir * intent.target_speed;
    let current = state.horizontal_velocity;
    let max_delta = tuning.horizontal_acceleration * dt;
    let delta = wish - current;
    state.horizontal_velocity = if delta.length() <= max_delta {
        wish
    } else {
        current + delta.normalize_or_zero() * max_delta
    };
    if grounded {
        state.vertical_velocity = 0.0;
        if state.jump_active {
            state.jump_active = false;
        }
        if intent.jump_pressed {
            state.vertical_velocity = tuning.jump_launch;
            state.jump_active = true;
            state.jump_apex = 0.0;
            state.jump_time = 0.0;
        }
    } else {
        state.vertical_velocity -= tuning.gravity * dt;
        if state.jump_active {
            state.jump_time += dt;
            state.jump_apex = state
                .jump_apex
                .max(state.vertical_velocity * state.jump_time);
        }
    }
}

type WalkBodyQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Transform,
        &'static Collider,
        &'static mut KinematicCharacterController,
        &'static mut WalkState,
        Option<&'static KinematicCharacterControllerOutput>,
    ),
>;

/// Fixed-step walk integration + controller feed (V10, V11, V18).
pub fn walk_movement_system(
    mode: Res<MoveMode>,
    hold: Option<Res<WalkSpawnHold>>,
    mut intent: ResMut<WalkIntent>,
    tuning: Res<MovementTuning>,
    context: ReadRapierContext,
    mut camera_tick: Option<ResMut<WalkCameraTick>>,
    mut player: WalkBodyQuery,
) {
    if *mode != MoveMode::Walk {
        return;
    }
    let Ok((entity, mut body, capsule, mut controller, mut state, output)) = player.single_mut()
    else {
        return;
    };
    controller.custom_shape = None;
    if hold.as_ref().is_some_and(|hold| hold.active) {
        *state = WalkState::default();
        intent.jump_pressed = false;
        // Even a zero move lets Rapier depenetrate an overlapping capsule. Only the bounded
        // placement search may move a held body; do not run the controller until it succeeds.
        controller.translation = None;
        return;
    }
    let grounded = output.map(|o| o.grounded).unwrap_or(false);
    integrate_walk(&mut state, &intent, &tuning, PHYSICS_TIMESTEP, grounded);
    intent.jump_pressed = false;
    let mut displacement =
        (state.horizontal_velocity + Vec3::Y * state.vertical_velocity) * PHYSICS_TIMESTEP;
    if state.grounded
        && !state.jump_active
        && let Ok(context) = context.single()
        && let Some(rise) = walk_step_rise(
            &context,
            entity,
            &body,
            capsule,
            &controller,
            &tuning,
            displacement.reject_from_normalized(Vec3::Y),
        )
    {
        if let Some(tick) = camera_tick.as_deref_mut() {
            tick.validated_rise = true;
        }
        // The upward sweep validated this segment. Rapier runs the controller
        // before syncing transforms, so override this tick's query pose too.
        // Do not subtract the lift from the horizontal tick: built-in autostep
        // does so, undoing the step at narrow treads and coplanar landing joins.
        body.translation.y += rise;
        controller.custom_shape = Some((
            capsule.clone(),
            body.rotation.inverse() * (Vec3::Y * rise),
            Quat::IDENTITY,
        ));
        state.grounded = false;
        state.vertical_velocity = 0.0;
        displacement.y = 0.0;
    }
    controller.translation = Some(displacement);
}

/// Find a supported step without changing the requested horizontal distance.
/// All casts use the full capsule and exclude dynamic bodies only for stepping.
#[allow(clippy::too_many_arguments)]
fn walk_step_rise(
    context: &RapierContext,
    entity: Entity,
    body: &Transform,
    capsule: &Collider,
    controller: &KinematicCharacterController,
    tuning: &MovementTuning,
    horizontal: Vec3,
) -> Option<f32> {
    let distance = horizontal.length();
    if distance <= f32::EPSILON {
        return None;
    }
    let direction = horizontal / distance;
    let filter = QueryFilter {
        flags: controller.filter_flags,
        groups: controller.filter_groups,
        ..QueryFilter::default().exclude_rigid_body(entity)
    };
    let static_filter = QueryFilter {
        flags: filter.flags | QueryFilterFlags::EXCLUDE_DYNAMIC,
        ..filter
    };
    let cast = |position, direction, distance, filter| {
        context
            .cast_shape(
                position,
                body.rotation,
                direction,
                &*capsule.raw,
                ShapeCastOptions {
                    target_distance: PLAYER_CONTROLLER_OFFSET,
                    stop_at_penetration: false,
                    max_time_of_impact: distance,
                    compute_impact_geometry_on_penetration: true,
                },
                filter,
            )
            .map(|(_, hit)| hit)
    };
    let blocking_hit = cast(body.translation, direction, distance, static_filter)?;
    let obstacle = blocking_hit.details?;
    let walkable = controller.max_slope_climb_angle.cos();
    if obstacle.normal1.y > walkable {
        // A zero-distance floor hit can consume all 20 move-and-slide iterations
        // while the default 1e-4 normal nudge cannot recover the lost skin gap.
        // Repair only measured skin overlap, with an upward clearance sweep;
        // ordinary slope contacts continue through Rapier without a step.
        let capsule_point = body.translation + body.rotation * obstacle.witness2;
        let gap = (capsule_point - obstacle.witness1).dot(obstacle.normal1);
        if blocking_hit.time_of_impact > f32::EPSILON
            || !(-PLAYER_CONTROLLER_OFFSET..PLAYER_CONTROLLER_OFFSET).contains(&gap)
        {
            // Recover at most one skin width of overlap. Deeper penetration
            // needs placement recovery, not a full autostep-height relocation.
            return None;
        }
        let lift = (PLAYER_CONTROLLER_OFFSET + 0.05 - gap) / obstacle.normal1.y;
        return (lift > 0.0
            && lift <= tuning.autostep_height
            && cast(body.translation, Vec3::Y, lift, filter).is_none())
        .then_some(lift);
    }
    if obstacle.normal1.y < 0.0 {
        return None;
    }
    let approach = -direction.dot(obstacle.normal1.with_y(0.0).normalize_or_zero());
    if approach <= f32::EPSILON {
        return None;
    }
    // Advance far enough to probe a walkable capsule-edge contact, including an
    // oblique approach. A radius-wide probe instead hits the next 24-unit riser.
    let corner_advance = tuning.capsule_radius * (1.0 - controller.max_slope_climb_angle.sin());
    let probe = (corner_advance / approach + PLAYER_CONTROLLER_OFFSET).max(distance);
    if probe > tuning.capsule_radius + distance + PLAYER_CONTROLLER_OFFSET {
        return None; // A glancing contact cannot borrow support beyond the capsule's reach.
    }
    let budget = tuning.autostep_height;
    if cast(body.translation, Vec3::Y, budget, filter).is_some() {
        return None;
    }
    let raised = body.translation + Vec3::Y * budget;
    if cast(raised, direction, probe, filter).is_some() {
        return None;
    }
    let landing = cast(
        raised + direction * probe,
        Vec3::NEG_Y,
        budget,
        static_filter,
    )?
    .details?;
    if landing.normal1.y < walkable {
        return None;
    }
    // Use the obstacle surface height, not the rounded capsule's hypothetical
    // position at the lookahead point. Rapier 0.35 predicts ground at offset+0.05;
    // that same slop keeps a new landing outside numerical skin penetration.
    let rise = landing.witness1.y + tuning.capsule_standing_height * 0.5 + PLAYER_CONTROLLER_OFFSET
        - body.translation.y;
    if rise <= 0.05 || rise > budget {
        return None;
    }
    let lift = (rise + 0.05).min(budget);
    if cast(
        body.translation + Vec3::Y * lift,
        direction,
        distance,
        filter,
    )
    .is_some()
    {
        return None;
    }
    Some(lift)
}

/// Camera follows the capsule at eye height; yaw on body, pitch on view (V9).
/// This response is Mudcrab presentation tuning. It is not a native Skyrim measurement.
const WALK_STEP_CAMERA_RESPONSE_SECONDS: f32 = 0.055;
const WALK_STEP_CAMERA_MAX_TICK_DELTA: f32 = 36.0;
const WALK_STEP_CAMERA_MIN_TICK_DELTA: f32 = 1.0;

#[derive(Debug, Default, Resource)]
pub struct WalkCameraTick {
    previous_body: Option<Vec3>,
    previous_supported: bool,
    validated_rise: bool,
    pending_y: f32,
    discontinuity: bool,
    airborne: bool,
    #[cfg(test)]
    last_sample_validated_rise: bool,
    #[cfg(test)]
    last_sample_collected_y: f32,
}

#[derive(Debug, Default, Resource)]
struct WalkCameraFollowState {
    offset_y: f32,
    initialized: bool,
    previous_origin: Option<IVec2>,
}

/// The steepest ground, in degrees, whose per-tick lowering the view follows directly.
/// Mudcrab presentation tuning, not a native Skyrim value. The controller itself climbs slopes up
/// to `slope_climb_degrees` (50); this is lower on purpose, because a stair flight descended at a
/// run slides over each edge and its per-tick lowering looks like a 25 to 45 degree slope.
const WALK_STEP_CAMERA_SLOPE_DEGREES: f32 = 30.0;

/// Whether one fixed tick's supported downward move is a stair drop rather than ordinary ground.
///
/// Walking down a ramp or hill lowers the body by `horizontal travel * tan(slope)` every tick.
/// That is continuous ground contact, so the eye follows it directly. A tick counts as a stair
/// drop only when the body lowers by more than that allowance for the tick's horizontal travel,
/// plus [`WALK_STEP_CAMERA_MIN_TICK_DELTA`] of slack for ground-snap jitter. A drop with no
/// horizontal travel (a lift or an edge) counts too. Ground steeper than the allowance, which is
/// rare, is smoothed like stairs.
fn is_stair_drop(tick_delta: Vec3, tuning: &MovementTuning) -> bool {
    let slope = WALK_STEP_CAMERA_SLOPE_DEGREES
        .min(tuning.slope_climb_degrees)
        .to_radians();
    let slope_allowance = tick_delta.xz().length() * slope.tan();
    -tick_delta.y > slope_allowance + WALK_STEP_CAMERA_MIN_TICK_DELTA
}

fn accumulate_camera_offset(offset: f32, delta_y: f32) -> f32 {
    // Keep the presented height continuous when step direction reverses.
    // Only explicit placement/reset paths discard the residual.
    offset + delta_y
}

fn exponential_camera_offset(offset: f32, delta_secs: f32) -> f32 {
    if !delta_secs.is_finite() || delta_secs <= 0.0 {
        return offset;
    }
    offset * (-delta_secs / WALK_STEP_CAMERA_RESPONSE_SECONDS).exp()
}

/// Runs after Rapier writeback so this records the controller's final physical pose per tick.
fn sample_walk_camera_tick(
    mode: Res<MoveMode>,
    tuning: Res<MovementTuning>,
    mut tick: ResMut<WalkCameraTick>,
    player: Query<
        (
            &Transform,
            &WalkState,
            Option<&KinematicCharacterControllerOutput>,
        ),
        With<PlayerBody>,
    >,
) {
    let Ok((body, walk, output)) = player.single() else {
        tick.previous_body = None;
        tick.previous_supported = false;
        tick.pending_y = 0.0;
        tick.validated_rise = false;
        tick.discontinuity = false;
        tick.airborne = false;
        return;
    };
    if *mode != MoveMode::Walk {
        tick.previous_body = Some(body.translation);
        tick.previous_supported = false;
        tick.pending_y = 0.0;
        tick.validated_rise = false;
        tick.airborne = false;
        return;
    }

    let supported = output.is_some_and(|result| result.grounded);
    let jumping_or_falling = walk.jump_active || (!supported && walk.vertical_velocity.abs() > 2.0);
    #[cfg(test)]
    {
        tick.last_sample_validated_rise = tick.validated_rise;
        tick.last_sample_collected_y = 0.0;
    }
    if let Some(previous) = tick.previous_body {
        let delta = body.translation - previous;
        let bounded = delta.y.abs() <= WALK_STEP_CAMERA_MAX_TICK_DELTA;
        if delta.xz().length() > 256.0 || !bounded {
            tick.discontinuity = true;
            tick.pending_y = 0.0;
        } else if tick.validated_rise && delta.y > 0.0 {
            // The explicit sweep signal remains valid when movement clears grounded.
            tick.pending_y += delta.y;
            #[cfg(test)]
            {
                tick.last_sample_collected_y = delta.y;
            }
        } else if tick.previous_supported
            && supported
            && !jumping_or_falling
            && !walk.jump_active
            && is_stair_drop(delta, &tuning)
        {
            // Smooth only bounded supported drops that a walkable slope cannot explain.
            // Walking down a ramp or hill is continuous contact and stays direct.
            tick.pending_y += delta.y;
            #[cfg(test)]
            {
                tick.last_sample_collected_y = delta.y;
            }
        }
        if jumping_or_falling || (!supported && !tick.validated_rise) {
            // Keep this state sticky until render consumes it. A jump and landing can
            // both occur between two rendered frames.
            tick.airborne = true;
            tick.pending_y = 0.0;
        }
    }
    tick.previous_body = Some(body.translation);
    tick.previous_supported = supported;
    tick.validated_rise = false;
}

type WalkCameraFollowPlayerQuery<'w, 's> = Query<
    'w,
    's,
    (&'static Transform, &'static WalkState),
    (With<PlayerBody>, Without<StreamingCamera>),
>;

#[allow(clippy::too_many_arguments)]
fn walk_camera_follow_system(
    mode: Res<MoveMode>,
    tuning: Res<MovementTuning>,
    look: Res<LookIntent>,
    time: Res<Time>,
    camera_space: Option<Res<crate::sky::CameraSpace>>,
    render_origin: Option<Res<crate::streaming::RenderOrigin>>,
    mut teleports: MessageReader<TeleportPlayer>,
    mut tick: Option<ResMut<WalkCameraTick>>,
    mut follow: ResMut<WalkCameraFollowState>,
    player: WalkCameraFollowPlayerQuery,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
) {
    let mut teleported = false;
    for _ in teleports.read() {
        teleported = true;
    }
    let space_changed = camera_space
        .as_ref()
        .is_some_and(|space| space.is_changed());
    let current_origin = render_origin.as_ref().map(|origin| origin.0);
    let origin_changed = follow
        .previous_origin
        .is_some_and(|old| Some(old) != current_origin);
    follow.previous_origin = current_origin;

    if *mode != MoveMode::Walk {
        if mode.is_changed() || teleported || space_changed || origin_changed {
            follow.offset_y = 0.0;
            if let Some(tick) = tick.as_deref_mut() {
                tick.pending_y = 0.0;
                tick.discontinuity = false;
            }
        }
        return;
    }
    let (Ok((body, walk)), Ok(mut view)) = (player.single(), camera.single_mut()) else {
        return;
    };
    let target = body.translation + Vec3::Y * tuning.eye_height;
    if !follow.initialized {
        // The authored startup camera pose is authoritative for the first frame.
        follow.initialized = true;
        if let Some(tick) = tick.as_deref_mut() {
            tick.pending_y = 0.0;
            tick.discontinuity = false;
        }
        set_walk_camera_rotation(&mut view, body, look.pitch);
        return;
    }

    let reset = mode.is_changed() || teleported || space_changed || origin_changed;
    let camera_tick = tick.as_deref_mut();
    let discontinuity = camera_tick.as_ref().is_some_and(|tick| tick.discontinuity);
    let airborne = camera_tick.as_ref().is_some_and(|tick| tick.airborne) || walk.jump_active;
    if reset || discontinuity || airborne {
        view.translation = target;
        follow.offset_y = 0.0;
    } else {
        let pending = camera_tick.as_ref().map_or(0.0, |tick| tick.pending_y);
        follow.offset_y = accumulate_camera_offset(follow.offset_y, pending);
        if let Some(tick) = tick.as_deref_mut() {
            tick.pending_y = 0.0;
            tick.discontinuity = false;
        }
        follow.offset_y = exponential_camera_offset(follow.offset_y, time.delta_secs());
        view.translation = target - Vec3::Y * follow.offset_y;
    }
    if let Some(tick) = tick.as_deref_mut()
        && (reset || discontinuity || airborne)
    {
        tick.pending_y = 0.0;
        tick.discontinuity = false;
        tick.airborne = false;
        tick.previous_body = Some(body.translation);
    }
    set_walk_camera_rotation(&mut view, body, look.pitch);
}

fn set_walk_camera_rotation(view: &mut Transform, body: &Transform, pitch: f32) {
    let (yaw, _, _) = body.rotation.to_euler(EulerRot::YXZ);
    view.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
}

#[cfg(test)]
mod walk_camera_presentation_tests {
    use super::*;

    #[test]
    fn accumulated_step_offset_follows_the_exponential_recurrence() {
        let rise = 24.0_f32;
        let dt = 1.0 / 60.0;
        let decay = (-dt / WALK_STEP_CAMERA_RESPONSE_SECONDS).exp();
        let expected_offset = rise * decay;
        let offset = exponential_camera_offset(rise, dt);
        assert!((offset - expected_offset).abs() < 1.0e-5);
        assert_eq!(accumulate_camera_offset(5.0, -2.0), 3.0);
        assert_eq!(accumulate_camera_offset(-5.0, 2.0), -3.0);
        assert_eq!(accumulate_camera_offset(5.0, 2.0), 7.0);
        assert!((rise - offset) < rise);
        assert!(exponential_camera_offset(offset, dt) < offset);

        let slow_dt = 1.0 / 20.0;
        let after_three_ticks = rise * (-3.0 * dt / WALK_STEP_CAMERA_RESPONSE_SECONDS).exp();
        let accumulated_then_decayed = exponential_camera_offset(rise * 3.0, slow_dt);
        assert!((accumulated_then_decayed - 3.0 * after_three_ticks).abs() < 1.0e-5);
        let old_target = 100.0;
        let old_offset = 5.0;
        let reverse_step = -2.0;
        assert_eq!(
            old_target - old_offset,
            old_target + reverse_step - accumulate_camera_offset(old_offset, reverse_step),
            "step collection must not itself move the presented eye"
        );
    }

    /// Per-tick classification: ordinary ground is not a stair drop, a riser is.
    #[test]
    fn stair_drop_classification_separates_slopes_from_risers() {
        let tuning = MovementTuning::default();
        let tick = 1.0 / 60.0;
        let drop = |horizontal: f32, down: f32| Vec3::new(horizontal, -down, 0.0);
        for speed in [80.1_f32, 370.0] {
            let travel = speed * tick;
            // Ordinary ramps and hills, up to the allowance, at walking and running speed.
            for degrees in [5.0_f32, 10.0, 20.0, 29.0] {
                let down = travel * degrees.to_radians().tan();
                assert!(
                    !is_stair_drop(drop(travel, down), &tuning),
                    "{degrees} degree slope at {speed} u/s was read as a stair drop"
                );
            }
            // Stair risers lower the body by far more than the slope allowance for that travel.
            for riser in [12.0_f32, 24.0] {
                assert!(
                    is_stair_drop(drop(travel, riser), &tuning),
                    "{riser}-unit riser at {speed} u/s was not read as a stair drop"
                );
            }
        }
        // The slack covers ground-snap jitter; a drop in place beyond it is still a drop.
        assert!(!is_stair_drop(drop(0.0, 0.9), &tuning));
        assert!(is_stair_drop(drop(0.0, 1.5), &tuning));
        // Only lowering matters: a rise or a level move is never a drop.
        assert!(!is_stair_drop(drop(6.0, -10.0), &tuning));
        assert!(!is_stair_drop(drop(6.0, 0.0), &tuning));
        // A controller with a gentler climb limit lowers the allowance with it.
        let gentle = MovementTuning {
            slope_climb_degrees: 15.0,
            ..MovementTuning::default()
        };
        let down = 6.17 * 28.0_f32.to_radians().tan();
        assert!(!is_stair_drop(drop(6.17, down), &tuning));
        assert!(is_stair_drop(drop(6.17, down), &gentle));
    }

    #[test]
    fn camera_placement_resets_clear_pending_steps_and_follow_full_position() {
        let mut app = headless::fixture_app();
        let (body, camera) = {
            let mut bodies = app.world_mut().query_filtered::<Entity, With<PlayerBody>>();
            let body = bodies.single(app.world()).unwrap();
            let mut cameras = app
                .world_mut()
                .query_filtered::<Entity, With<StreamingCamera>>();
            (body, cameras.single(app.world()).unwrap())
        };
        app.insert_resource(MoveMode::Walk);
        app.world_mut().run_schedule(Update);
        for case in 0..5 {
            {
                let mut follow = app.world_mut().resource_mut::<WalkCameraFollowState>();
                follow.initialized = true;
                follow.offset_y = 12.0;
            }
            app.world_mut().resource_mut::<WalkCameraTick>().pending_y = 24.0;
            let destination = Vec3::new(500.0 + case as f32 * 100.0, 800.0, -900.0);
            app.world_mut()
                .get_mut::<Transform>(body)
                .unwrap()
                .translation = destination;
            match case {
                0 => {
                    app.world_mut().write_message(TeleportPlayer {
                        position: destination,
                        sampled_ground_y: Some(destination.y),
                        yaw: 0.4,
                    });
                }
                1 => {
                    app.insert_resource(crate::sky::CameraSpace::Interior);
                }
                2 => {
                    app.insert_resource(MoveMode::Noclip);
                    app.world_mut().run_schedule(Update);
                    app.insert_resource(MoveMode::Walk);
                }
                3 => {
                    app.insert_resource(crate::streaming::RenderOrigin(IVec2::ZERO));
                    app.world_mut().run_schedule(Update);
                    app.world_mut()
                        .resource_mut::<WalkCameraFollowState>()
                        .offset_y = 12.0;
                    app.world_mut().resource_mut::<WalkCameraTick>().pending_y = 24.0;
                    app.world_mut()
                        .resource_mut::<crate::streaming::RenderOrigin>()
                        .0 = IVec2::X;
                }
                _ => {
                    app.world_mut()
                        .get_mut::<WalkState>(body)
                        .unwrap()
                        .jump_active = true;
                }
            }
            app.world_mut().run_schedule(Update);
            let position = app.world().get::<Transform>(body).unwrap().translation;
            let view = app.world().get::<Transform>(camera).unwrap().translation;
            assert!(
                view.distance(position + Vec3::Y * MovementTuning::default().eye_height) < 1.0e-3,
                "reset case {case} retained an old camera coordinate: {view:?} vs {position:?}"
            );
            assert_eq!(
                app.world().resource::<WalkCameraFollowState>().offset_y,
                0.0
            );
            assert_eq!(app.world().resource::<WalkCameraTick>().pending_y, 0.0);
        }
    }
}

/// Attempt NOCLIP->WALK at the camera pose; overlap pushes the search upward
/// until the capsule fits or leaves NOCLIP active with a visible reason (V8).
pub fn try_enter_walk(
    context: &RapierContext,
    tuning: &MovementTuning,
    camera_position: Vec3,
    capsule: &Collider,
) -> Result<Vec3, String> {
    let shape = &*capsule.raw;
    let mut candidate = camera_position - Vec3::Y * tuning.eye_height;
    for _ in 0..=WALK_ENTRY_SEARCH_STEPS {
        let mut overlapping = false;
        context.intersect_shape(
            candidate,
            Quat::IDENTITY,
            shape,
            QueryFilter::default().groups(CollisionGroups::new(
                GROUP_PLAYER,
                GROUP_WORLD | GROUP_CLUTTER,
            )),
            |_| {
                overlapping = true;
                false
            },
        );
        if !overlapping {
            return Ok(candidate);
        }
        candidate.y += WALK_ENTRY_STEP_HEIGHT;
    }
    Err("no free capsule placement nearby".to_owned())
}

/// The pending-input state a pause drops, optional so the console works in partial apps.
#[derive(SystemParam)]
pub(crate) struct MotionReset<'w, 's> {
    intent: Option<ResMut<'w, WalkIntent>>,
    sprint: Option<ResMut<'w, SprintLatch>>,
    controllers: Query<'w, 's, &'static mut KinematicCharacterController>,
}

impl MotionReset<'_, '_> {
    /// Drops pending input only: walk intent, the sprint latch and the queued controller
    /// translation. Velocity and jump state stay, so a pause resumes the motion where it stopped.
    pub(crate) fn clear_input(&mut self) {
        if let Some(intent) = self.intent.as_deref_mut() {
            *intent = WalkIntent::default();
        }
        if let Some(sprint) = self.sprint.as_deref_mut() {
            sprint.0 = false;
        }
        for mut controller in &mut self.controllers {
            controller.translation = None;
        }
    }
}

/// Clear stale velocities + jump state on every toggle (V14).
fn clear_motion_state(
    intent: &mut WalkIntent,
    sprint: &mut SprintLatch,
    states: &mut Query<&mut WalkState>,
    controllers: &mut Query<&mut KinematicCharacterController>,
) {
    *intent = WalkIntent::default();
    sprint.0 = false;
    for mut state in states {
        *state = WalkState::default();
    }
    for mut controller in controllers {
        controller.translation = None;
    }
}

// ---------------------------------------------------------------------------
// Teleport: a one-shot placement of the player, used by load-door crossings.
// ---------------------------------------------------------------------------

/// Moves the player to `position` facing `yaw`.
///
/// `position` is where the player's FEET end up, in RENDER space (the same space as the player's
/// `Transform`, relative to the current render origin, not Creation or world coordinates). The
/// body's `Transform` goes one capsule half-extent above it and the camera one eye height above
/// the body, as in [`walk_camera_follow_system`]. The sender converts and rebases first.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct TeleportPlayer {
    pub position: Vec3,
    /// Known destination terrain height in render space, when the sender sampled it.
    pub sampled_ground_y: Option<f32>,
    /// Heading in radians about +Y, as `Quat::from_euler(EulerRot::YXZ, yaw, ..)`.
    pub yaw: f32,
}

/// How far the body's origin (the capsule centre) sits above the feet.
pub fn body_height_above_feet(tuning: &MovementTuning) -> f32 {
    tuning.capsule_half_height() + tuning.capsule_radius
}

/// Where the body goes and where the camera goes for feet at `feet`, in the feet's space.
pub fn body_and_camera_for_feet(tuning: &MovementTuning, feet: Vec3) -> (Vec3, Vec3) {
    let body = feet + Vec3::Y * body_height_above_feet(tuning);
    (body, body + Vec3::Y * tuning.eye_height)
}

type TeleportBodyQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Transform,
        Option<&'static mut WalkState>,
        Option<&'static mut KinematicCharacterController>,
        Option<&'static mut Velocity>,
    ),
    (With<PlayerBody>, Without<StreamingCamera>),
>;

/// Applies [`TeleportPlayer`]: places the body, zeroes its velocity and walk state, and holds WALK
/// until bounded destination collision is available. In NOCLIP the body stays disabled and only
/// the camera moves.
#[allow(clippy::too_many_arguments)]
fn apply_teleport_player(
    mut teleports: MessageReader<TeleportPlayer>,
    mut hold: ResMut<WalkSpawnHold>,
    mode: Res<MoveMode>,
    tuning: Res<MovementTuning>,
    mut look: ResMut<LookIntent>,
    mut intent: ResMut<WalkIntent>,
    mut body: TeleportBodyQuery,
    mut camera: Query<&mut Transform, (With<StreamingCamera>, Without<PlayerBody>)>,
    mut commands: Commands,
) {
    let Some(teleport) = teleports.read().last().copied() else {
        return;
    };
    *hold = WalkSpawnHold::default();
    if *mode == MoveMode::Walk {
        hold.active = true;
        hold.sampled_ground_y = teleport
            .sampled_ground_y
            .filter(|height| height.is_finite());
        hold.destination = true;
    }
    look.yaw = teleport.yaw;
    *intent = WalkIntent::default();
    let rotation = Quat::from_euler(EulerRot::YXZ, teleport.yaw, 0.0, 0.0);
    let (body_position, camera_position) = body_and_camera_for_feet(&tuning, teleport.position);
    if *mode == MoveMode::Walk
        && let Ok((entity, mut transform, state, controller, velocity)) = body.single_mut()
    {
        transform.translation = body_position;
        transform.rotation = rotation;
        commands
            .entity(entity)
            .remove::<KinematicCharacterControllerOutput>();
        if let Some(mut state) = state {
            *state = WalkState::default();
        }
        if let Some(mut controller) = controller {
            controller.translation = None;
        }
        if let Some(mut velocity) = velocity {
            *velocity = Velocity::zero();
        }
    }
    if let Ok(mut view) = camera.single_mut() {
        view.translation = camera_position;
        view.rotation = Quat::from_euler(EulerRot::YXZ, teleport.yaw, look.pitch, 0.0);
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn alt_sprint_latches_until_movement_stops_or_controls_leave_walk() {
        let mut app = App::new();
        app.insert_resource(MoveMode::Walk)
            .insert_resource(CursorCapture::Captured)
            .insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(MovementTuning::default())
            .insert_resource(LookIntent::default())
            .insert_resource(WalkIntent::default())
            .insert_resource(SprintLatch::default());
        app.world_mut().spawn((PlayerBody, Transform::default()));
        let sample = |app: &mut App| {
            app.world_mut().run_system_once(walk_intent_system).unwrap();
            app.world().resource::<WalkIntent>().target_speed
        };
        let tuning = app.world().resource::<MovementTuning>().clone();
        let sprint_speed = tuning.target_speed(Vec2::new(0.0, -1.0), false, true);

        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::KeyW);
            keys.press(KeyCode::AltLeft);
        }
        assert_eq!(sample(&mut app), sprint_speed);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.clear();
            keys.release(KeyCode::AltLeft);
        }
        assert_eq!(sample(&mut app), sprint_speed);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.clear();
            keys.press(KeyCode::AltLeft);
        }
        assert_eq!(sample(&mut app), tuning.run_speed);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.clear();
            keys.release(KeyCode::AltLeft);
            keys.press(KeyCode::AltRight);
        }
        assert_eq!(sample(&mut app), sprint_speed);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.clear();
            keys.release(KeyCode::AltRight);
            keys.release(KeyCode::KeyW);
        }
        assert_eq!(sample(&mut app), 0.0);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.clear();
            keys.press(KeyCode::KeyW);
        }
        assert_eq!(sample(&mut app), tuning.run_speed);

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::AltLeft);
        assert_eq!(sample(&mut app), sprint_speed);
        app.insert_resource(MoveMode::Noclip);
        sample(&mut app);
        app.insert_resource(MoveMode::Walk);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        assert_eq!(sample(&mut app), tuning.run_speed);

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::AltLeft);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::AltRight);
        assert_eq!(sample(&mut app), sprint_speed);
        app.insert_resource(CursorCapture::Released);
        sample(&mut app);
        app.insert_resource(CursorCapture::Captured);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        assert_eq!(sample(&mut app), tuning.run_speed);
    }

    #[test]
    fn walk_integrates_toward_wish_speed_with_bounded_acceleration() {
        let tuning = MovementTuning::default();
        let mut state = WalkState::default();
        let intent = WalkIntent {
            wish_dir: Vec3::X,
            target_speed: tuning.run_speed,
            jump_pressed: false,
        };
        integrate_walk(&mut state, &intent, &tuning, PHYSICS_TIMESTEP, true);
        let expected = tuning.horizontal_acceleration * PHYSICS_TIMESTEP;
        assert!((state.horizontal_velocity.x - expected).abs() < 0.01);
        for _ in 0..600 {
            integrate_walk(&mut state, &intent, &tuning, PHYSICS_TIMESTEP, true);
        }
        assert!((state.horizontal_velocity.length() - tuning.run_speed).abs() < 0.5);
    }

    #[test]
    fn jump_requires_grounded_fresh_press_and_applies_gravity() {
        let tuning = MovementTuning::default();
        let mut state = WalkState::default();
        let air_jump = WalkIntent {
            jump_pressed: true,
            ..default()
        };
        integrate_walk(&mut state, &air_jump, &tuning, PHYSICS_TIMESTEP, false);
        assert!(state.vertical_velocity < 0.0);
        let grounded_jump = WalkIntent {
            jump_pressed: true,
            ..default()
        };
        integrate_walk(&mut state, &grounded_jump, &tuning, PHYSICS_TIMESTEP, true);
        assert!((state.vertical_velocity - tuning.jump_launch).abs() < f32::EPSILON);
    }

    #[test]
    fn capsule_bundle_matches_provisional_spec_dimensions() {
        let tuning = MovementTuning::default();
        let app_bundle = player_controller_bundle(&tuning);
        let mut world = World::new();
        let entity = world.spawn(app_bundle).id();
        let collider = world.get::<Collider>(entity).expect("capsule collider");
        let capsule = collider.as_capsule().expect("capsule shape");
        assert!((capsule.radius() - tuning.capsule_radius).abs() < f32::EPSILON);
        let controller = world
            .get::<KinematicCharacterController>(entity)
            .expect("controller");
        assert!(
            (controller.max_slope_climb_angle - tuning.slope_climb_degrees.to_radians()).abs()
                < 1.0e-6
        );
        assert_eq!(
            controller.snap_to_ground,
            Some(CharacterLength::Absolute(tuning.ground_snap))
        );
    }
}

#[cfg(test)]
pub(crate) mod headless {
    use super::*;
    use bevy::mesh::MeshPlugin;
    use bevy::time::TimeUpdateStrategy;

    pub(crate) fn fixture_app() -> App {
        fixture_app_with(|_| {})
    }

    /// The fixture app, with `extra` run on it before plugins finish (to add more plugins).
    pub(crate) fn fixture_app_with(extra: impl FnOnce(&mut App)) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            bevy::input::InputPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            MaterialPlugin::<StandardMaterial>::default(),
        ));
        app.init_resource::<ProfilingState>()
            .init_resource::<StreamingMetrics>();
        app.add_plugins(PhysicsFixturePlugin);
        extra(&mut app);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f32(PHYSICS_TIMESTEP),
        ));
        app.finish();
        app.update();
        app
    }

    /// Place the fixture player body, enable its rigid body, enter WALK.
    pub(crate) fn place_player(app: &mut App, position: Vec3) {
        let mut query = app.world_mut().query_filtered::<(
            Entity,
            &mut Transform,
            Option<&RigidBodyDisabled>,
        ), With<PlayerBody>>();
        let (entity, mut transform, disabled) = query
            .single_mut(app.world_mut())
            .expect("fixture player body");
        transform.translation = position;
        if disabled.is_some() {
            app.world_mut()
                .entity_mut(entity)
                .remove::<RigidBodyDisabled>();
        }
        app.insert_resource(MoveMode::Walk);
        app.insert_resource(WalkIntent::default());
    }

    pub(crate) fn player_pose(app: &mut App) -> (Vec3, bool) {
        let mut query = app.world_mut().query_filtered::<(
            &Transform,
            &WalkState,
            Option<&KinematicCharacterControllerOutput>,
        ), With<PlayerBody>>();
        let (transform, state, output) = query.single(app.world()).expect("player body");
        (
            transform.translation,
            state.grounded || output.map(|o| o.grounded).unwrap_or(false),
        )
    }
}

#[cfg(test)]
mod world_startup_tests {
    use super::*;
    use bevy::{mesh::MeshPlugin, time::TimeUpdateStrategy};

    fn world_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            bevy::input::InputPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            MaterialPlugin::<StandardMaterial>::default(),
        ))
        .init_resource::<ProfilingState>()
        .init_resource::<StreamingMetrics>()
        // Rapier's debug-render system validates Gizmos even when disabled.
        .add_plugins(bevy::gizmos::GizmoPlugin)
        .add_plugins((WorldPlayerPlugin, crate::console::ConsolePlugin))
        .add_systems(Startup, |mut commands: Commands| {
            commands.spawn((
                StreamingCamera,
                Transform::from_translation(Vec3::new(0.0, 200.0, 0.0))
                    .with_rotation(Quat::from_euler(EulerRot::YXZ, 0.7, -0.2, 0.0)),
            ));
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f32(PHYSICS_TIMESTEP),
        ));
        app.finish();
        app.update();
        app
    }

    fn body(app: &mut App) -> Entity {
        app.world_mut()
            .query_filtered::<Entity, With<PlayerBody>>()
            .single(app.world())
            .unwrap()
    }

    fn tick(app: &mut App, count: usize) {
        for _ in 0..count {
            app.update();
        }
    }

    fn floor(app: &mut App, disabled: bool) -> Entity {
        let mut entity = app.world_mut().spawn((
            RigidBody::Fixed,
            Collider::cuboid(1000.0, 10.0, 1000.0),
            Transform::from_xyz(0.0, -10.0, 0.0),
            world_collision_groups(),
        ));
        if disabled {
            entity.insert(ColliderDisabled);
        }
        entity.id()
    }

    fn assert_mode(app: &mut App, expected: MoveMode) {
        assert_eq!(*app.world().resource::<MoveMode>(), expected);
        let entity = body(app);
        assert_eq!(
            app.world().get::<RigidBodyDisabled>(entity).is_some(),
            expected == MoveMode::Noclip
        );
    }

    #[test]
    fn world_starts_in_walk_with_active_body_and_copied_view() {
        let mut app = world_app();
        assert_mode(&mut app, MoveMode::Walk);
        let entity = body(&mut app);
        let transform = app.world().get::<Transform>(entity).unwrap();
        let tuning = app.world().resource::<MovementTuning>();
        assert!((transform.translation.y - (200.0 - tuning.eye_height)).abs() < 0.001);
        let (yaw, pitch, _) = transform.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - 0.7).abs() < 0.001);
        assert!(pitch.abs() < 0.001);
        let look = app.world().resource::<LookIntent>();
        assert!((look.yaw - 0.7).abs() < 0.001 && (look.pitch + 0.2).abs() < 0.001);
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, (With<StreamingCamera>, With<ControlledCamera>)>()
                .iter(app.world())
                .count(),
            1
        );
        let text = app
            .world_mut()
            .query_filtered::<&Text, With<NoclipOverlay>>()
            .single(app.world())
            .unwrap();
        assert!(text.0.starts_with("NOCLIP: OFF"));
        assert!(text.0.contains("waiting for WALK collision"));
    }

    #[test]
    fn hold_waits_for_enabled_rapier_collision_then_grounds() {
        let mut app = world_app();
        let entity = body(&mut app);
        let start = app.world().get::<Transform>(entity).unwrap().translation;
        let ground = floor(&mut app, true);
        {
            let mut state = app.world_mut().get_mut::<WalkState>(entity).unwrap();
            state.vertical_velocity = -900.0;
            state.horizontal_velocity = Vec3::X * 300.0;
        }
        tick(&mut app, 20);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_eq!(
            app.world().get::<Transform>(entity).unwrap().translation,
            start
        );
        app.world_mut()
            .entity_mut(ground)
            .remove::<ColliderDisabled>();
        tick(&mut app, 120);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
        let (pose, grounded) = headless::player_pose(&mut app);
        assert!(grounded, "{pose:?}");
        let view = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>()
            .single(app.world())
            .unwrap();
        assert!((view.translation.y - pose.y - MovementTuning::default().eye_height).abs() < 0.01);
        let (yaw, pitch, _) = view.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - 0.7).abs() < 0.001 && (pitch + 0.2).abs() < 0.001);
    }

    #[test]
    fn distant_world_floor_does_not_release_startup_hold() {
        let mut app = world_app();
        let ground = floor(&mut app, false);
        app.world_mut()
            .get_mut::<Transform>(ground)
            .unwrap()
            .translation
            .y = -10_000.0;
        tick(&mut app, 10);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
    }

    #[test]
    fn sampled_elevated_start_probe_releases_only_after_rapier_collision() {
        let mut app = world_app();
        let entity = body(&mut app);
        let authored_rotation = app.world().get::<Transform>(entity).unwrap().rotation;
        let camera = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>()
            .single(app.world())
            .unwrap()
            .rotation;
        let authored_camera_y = 5_000.0;
        app.world_mut()
            .get_mut::<Transform>(entity)
            .unwrap()
            .translation
            .y = authored_camera_y - MovementTuning::default().eye_height;
        app.world_mut()
            .resource_mut::<WalkSpawnHold>()
            .sampled_ground_y = Some(0.0);
        let ground = floor(&mut app, true);
        tick(&mut app, 10);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert!(
            (walk_spawn_probe_distance(
                authored_camera_y - MovementTuning::default().eye_height,
                Some(0.0),
                &MovementTuning::default(),
            ) - MovementTuning::default().capsule_standing_height * 2.0)
                > 4_000.0
        );

        app.world_mut()
            .entity_mut(ground)
            .remove::<ColliderDisabled>();
        tick(&mut app, 2);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
        let body_transform = app.world().get::<Transform>(entity).unwrap();
        assert!(
            (body_transform.translation.y + MovementTuning::default().eye_height
                - authored_camera_y)
                .abs()
                < 1.0
        );
        assert!(body_transform.rotation.dot(authored_rotation).abs() > 0.999_99);
        let current_camera_rotation = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>()
            .single(app.world())
            .unwrap()
            .rotation;
        assert!(current_camera_rotation.dot(camera).abs() > 0.999_99);
    }

    #[test]
    fn sampled_elevated_start_rejects_floor_far_below_local_surface() {
        let mut app = world_app();
        let entity = body(&mut app);
        app.world_mut()
            .get_mut::<Transform>(entity)
            .unwrap()
            .translation
            .y = 5_000.0;
        app.world_mut()
            .resource_mut::<WalkSpawnHold>()
            .sampled_ground_y = Some(0.0);
        let ground = floor(&mut app, false);
        app.world_mut()
            .get_mut::<Transform>(ground)
            .unwrap()
            .translation
            .y = -10_000.0;
        tick(&mut app, 10);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
    }

    #[test]
    fn sampled_elevated_start_accepts_loaded_platform_above_terrain() {
        let mut app = world_app();
        let entity = body(&mut app);
        app.world_mut()
            .get_mut::<Transform>(entity)
            .unwrap()
            .translation
            .y = 5_000.0;
        app.world_mut()
            .resource_mut::<WalkSpawnHold>()
            .sampled_ground_y = Some(0.0);
        let platform = floor(&mut app, true);
        app.world_mut()
            .get_mut::<Transform>(platform)
            .unwrap()
            .translation
            .y = 990.0;
        tick(&mut app, 5);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        app.world_mut()
            .entity_mut(platform)
            .remove::<ColliderDisabled>();
        tick(&mut app, 2);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
    }

    #[test]
    fn sampled_probe_keeps_tolerance_below_sample_and_stays_bounded() {
        let tuning = MovementTuning::default();
        assert_eq!(walk_spawn_probe_distance(-8.0, Some(0.0), &tuning), 56.0);
        assert_eq!(walk_spawn_probe_distance(-64.0, Some(0.0), &tuning), 0.0);
        assert_eq!(walk_spawn_probe_distance(-80.0, Some(0.0), &tuning), 0.0);
        assert_eq!(
            walk_spawn_probe_distance(100_000.0, Some(0.0), &tuning),
            65_536.0
        );
        assert_eq!(
            walk_spawn_probe_distance(100.0, Some(f32::NAN), &tuning),
            tuning.capsule_standing_height * 2.0
        );
        assert_eq!(walk_spawn_probe_distance(f32::NAN, Some(0.0), &tuning), 0.0);
    }

    #[test]
    fn sampled_start_below_sample_releases_on_loaded_lower_surface() {
        let mut app = world_app();
        let entity = body(&mut app);
        app.world_mut()
            .get_mut::<Transform>(entity)
            .unwrap()
            .translation
            .y = -10.0;
        app.world_mut()
            .resource_mut::<WalkSpawnHold>()
            .sampled_ground_y = Some(0.0);
        let ground = floor(&mut app, false);
        app.world_mut()
            .get_mut::<Transform>(ground)
            .unwrap()
            .translation
            .y = -50.0;
        tick(&mut app, 10);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
        assert!(
            app.world().get::<Transform>(entity).unwrap().translation.y
                >= -40.0 + body_height_above_feet(&MovementTuning::default()) - 2.0
        );
    }

    #[test]
    fn startup_overlap_uses_existing_failed_placement_fallback() {
        let mut app = world_app();
        app.world_mut().spawn((
            RigidBody::Fixed,
            Collider::cuboid(1000.0, 1000.0, 1000.0),
            Transform::default(),
            world_collision_groups(),
        ));
        tick(&mut app, 5);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_eq!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .as_deref(),
            Some("no free capsule placement nearby")
        );
    }

    #[test]
    fn missing_collision_times_out_safely_even_with_pending_streaming() {
        let mut app = world_app();
        app.world_mut()
            .resource_mut::<StreamingMetrics>()
            .loading_cells = 1;
        tick(&mut app, 10);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        app.world_mut().resource_mut::<WalkSpawnHold>().elapsed = WALK_SPAWN_TIMEOUT_SECONDS;
        tick(&mut app, 2);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .as_ref()
                .unwrap()
                .contains("startup world collision unavailable")
        );
    }

    #[test]
    fn hold_also_times_out_without_streaming_metrics() {
        let mut app = world_app();
        app.world_mut().remove_resource::<StreamingMetrics>();
        tick(&mut app, 5);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        app.world_mut().resource_mut::<WalkSpawnHold>().elapsed = WALK_SPAWN_TIMEOUT_SECONDS;
        tick(&mut app, 2);
        assert_mode(&mut app, MoveMode::Noclip);
    }

    #[test]
    fn explicit_walk_teleport_rearms_destination_hold() {
        let mut app = world_app();
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(20.0, 300.0, 40.0),
            sampled_ground_y: None,
            yaw: 1.0,
        });
        app.update();
        assert_mode(&mut app, MoveMode::Walk);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert!(app.world().resource::<WalkSpawnHold>().elapsed <= PHYSICS_TIMESTEP);
        assert!(app.world().resource::<WalkSpawnHold>().destination);
    }

    fn send_state(app: &mut App, state: bevy::input::ButtonState) {
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code: KeyCode::KeyV,
                logical_key: bevy::input::keyboard::Key::Character("v".into()),
                state,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        app.update();
    }

    #[test]
    fn v_cancels_hold_and_round_trips_without_rearming_it() {
        use bevy::input::ButtonState::{Pressed, Released};
        let mut app = world_app();
        app.insert_resource(CursorCapture::Captured);
        send_state(&mut app, Pressed);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        send_state(&mut app, Released);
        send_state(&mut app, Pressed);
        assert_mode(&mut app, MoveMode::Walk);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        let entity = body(&mut app);
        let state = app.world().get::<WalkState>(entity).unwrap();
        assert_eq!(state.horizontal_velocity, Vec3::ZERO);
        assert_eq!(state.vertical_velocity, 0.0);
        assert!(!app.world().resource::<SprintLatch>().0);
    }

    #[test]
    fn tcl_cancels_hold_and_round_trips() {
        let mut app = world_app();
        let entity = body(&mut app);
        app.world_mut()
            .get_mut::<WalkState>(entity)
            .unwrap()
            .vertical_velocity = -900.0;
        app.world_mut().resource_mut::<SprintLatch>().0 = true;
        crate::console::execute_line(app.world_mut(), "tcl");
        assert_eq!(
            app.world()
                .get::<WalkState>(entity)
                .unwrap()
                .vertical_velocity,
            0.0
        );
        assert!(!app.world().resource::<SprintLatch>().0);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        crate::console::execute_line(app.world_mut(), "tcl");
        assert_mode(&mut app, MoveMode::Walk);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
    }

    #[test]
    fn startup_hold_and_walk_pause_with_console_and_door_crossing_guards() {
        let mut app = world_app();
        let elapsed = WALK_SPAWN_TIMEOUT_SECONDS - PHYSICS_TIMESTEP * 0.5;
        app.world_mut().resource_mut::<WalkSpawnHold>().elapsed = elapsed;
        app.insert_resource(crate::console::ConsoleState {
            open: true,
            ..default()
        });
        tick(&mut app, 1);
        assert_mode(&mut app, MoveMode::Walk);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_eq!(app.world().resource::<WalkSpawnHold>().elapsed, elapsed);

        app.insert_resource(crate::console::ConsoleState::default());
        app.insert_resource(crate::door_crossing::DoorCrossing::holding_for_test());
        tick(&mut app, 1);
        assert_mode(&mut app, MoveMode::Walk);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_eq!(app.world().resource::<WalkSpawnHold>().elapsed, elapsed);

        app.insert_resource(crate::door_crossing::DoorCrossing::default());
        tick(&mut app, 1);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
    }

    #[test]
    fn tcl_cancels_a_walk_destination_hold() {
        let mut app = world_app();
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 1);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        crate::console::execute_line(app.world_mut(), "tcl");
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        crate::console::execute_line(app.world_mut(), "tcl");
        assert_mode(&mut app, MoveMode::Walk);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
    }

    #[test]
    fn walk_teleport_waits_for_destination_rapier_collision_then_grounds() {
        let mut app = world_app();
        let ground = floor(&mut app, true);
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 10);
        let entity = body(&mut app);
        let held_at = app.world().get::<Transform>(entity).unwrap().translation;
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert!(app.world().resource::<WalkSpawnHold>().destination);
        assert_eq!(
            held_at,
            body_and_camera_for_feet(&MovementTuning::default(), Vec3::new(0.0, 16.0, 0.0),).0
        );

        app.world_mut()
            .entity_mut(ground)
            .remove::<ColliderDisabled>();
        tick(&mut app, 120);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Walk);
        assert!(headless::player_pose(&mut app).1);
    }

    #[test]
    fn repeated_walk_teleport_replaces_sample_and_restarts_timeout() {
        let mut app = world_app();
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 1);
        app.world_mut().resource_mut::<WalkSpawnHold>().elapsed = 12.0;
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(100.0, 516.0, 100.0),
            sampled_ground_y: Some(500.0),
            yaw: 0.5,
        });
        tick(&mut app, 1);
        let hold = app.world().resource::<WalkSpawnHold>();
        assert!(hold.active && hold.destination);
        assert_eq!(hold.sampled_ground_y, Some(500.0));
        assert!(hold.elapsed <= PHYSICS_TIMESTEP);
        assert_mode(&mut app, MoveMode::Walk);
    }

    #[test]
    fn noclip_teleport_does_not_arm_destination_hold() {
        let mut app = world_app();
        crate::console::execute_line(app.world_mut(), "tcl");
        assert_mode(&mut app, MoveMode::Noclip);
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 1);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert_mode(&mut app, MoveMode::Noclip);
    }

    #[test]
    fn destination_hold_times_out_to_visible_noclip_fallback() {
        let mut app = world_app();
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 1);
        app.world_mut().resource_mut::<WalkSpawnHold>().elapsed = WALK_SPAWN_TIMEOUT_SECONDS;
        tick(&mut app, 2);
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
        assert!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("destination collision unavailable"))
        );
    }

    #[test]
    fn destination_hold_is_paused_during_door_crossing_and_recovery_clears_it() {
        let mut app = world_app();
        app.insert_resource(crate::door_crossing::DoorCrossing::recovery_for_test());
        app.world_mut().write_message(TeleportPlayer {
            position: Vec3::new(0.0, 16.0, 0.0),
            sampled_ground_y: Some(0.0),
            yaw: 0.0,
        });
        tick(&mut app, 1);
        let elapsed = app.world().resource::<WalkSpawnHold>().elapsed;
        tick(&mut app, 5);
        assert!(app.world().resource::<WalkSpawnHold>().active);
        assert_eq!(app.world().resource::<WalkSpawnHold>().elapsed, elapsed);

        crate::console::execute_line(app.world_mut(), "tcl");
        assert_mode(&mut app, MoveMode::Noclip);
        assert!(!app.world().resource::<WalkSpawnHold>().active);
    }
}

#[cfg(test)]
mod simulation_tests {
    use super::headless;
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn walk_falls_when_loaded_ground_is_far_below() {
        let mut app = headless::fixture_app();
        let start = Vec3::new(120.0, 1200.0, 120.0);
        headless::place_player(&mut app, start);
        for _ in 0..20 {
            app.update();
        }
        let (position, _) = headless::player_pose(&mut app);
        assert!(
            position.y < start.y - 10.0,
            "long fall froze at {position:?}"
        );
        assert!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .is_none()
        );
    }

    #[test]
    fn walk_falls_without_a_downward_ground_ray() {
        let mut app = headless::fixture_app();
        let start = Vec3::new(2000.0, 1200.0, 2000.0);
        headless::place_player(&mut app, start);
        for _ in 0..20 {
            app.update();
        }
        let (position, _) = headless::player_pose(&mut app);
        assert!(
            position.y < start.y - 10.0,
            "airborne WALK froze at {position:?}"
        );
        assert!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .is_none()
        );
    }

    #[test]
    fn noclip_can_enter_walk_and_fall_from_far_above_ground() {
        let mut app = headless::fixture_app();
        let camera_height = 8000.0;
        let start = Vec3::new(120.0, camera_height, 120.0);
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<ControlledCamera>>();
            query.single(app.world()).expect("controlled camera")
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = start;
        app.insert_resource(CursorCapture::Captured);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyV);
        app.world_mut().run_system_once(toggle_mode_system).unwrap();
        assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Walk);
        let initial = headless::player_pose(&mut app).0;
        assert!(
            (initial.y - (camera_height - app.world().resource::<MovementTuning>().eye_height))
                .abs()
                < 0.01
        );
        for _ in 0..20 {
            app.update();
        }
        let after = headless::player_pose(&mut app).0;
        assert!(
            after.y < initial.y - 10.0,
            "high-altitude WALK froze at {after:?}"
        );
    }

    #[test]
    fn noclip_stays_enabled_when_no_free_capsule_placement_exists() {
        let mut app = headless::fixture_app();
        let tuning = app.world().resource::<MovementTuning>().clone();
        let camera_position = Vec3::new(2000.0, 300.0 + tuning.eye_height, 2000.0);
        app.world_mut().spawn((
            RigidBody::Fixed,
            Collider::cuboid(500.0, 500.0, 500.0),
            world_collision_groups(),
            Transform::from_xyz(2000.0, 300.0, 2000.0),
        ));
        app.update();
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<ControlledCamera>>();
            query.single(app.world()).expect("controlled camera")
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = camera_position;
        app.insert_resource(CursorCapture::Captured);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyV);
        app.world_mut().run_system_once(toggle_mode_system).unwrap();
        assert_eq!(*app.world().resource::<MoveMode>(), MoveMode::Noclip);
        assert_eq!(
            app.world()
                .resource::<WalkEntryStatus>()
                .blocked_reason
                .as_deref(),
            Some("no free capsule placement nearby")
        );
    }

    #[test]
    fn sprint_increases_collision_resolved_speed_and_idle_reads_zero() {
        let mut app = headless::fixture_app();
        app.world_mut().resource_mut::<MovementTuning>().run_speed = 370.0;
        headless::place_player(&mut app, Vec3::new(0.0, 300.0, 700.0));
        for _ in 0..150 {
            app.update();
        }
        let measure = |app: &mut App| {
            let mut query = app
                .world_mut()
                .query_filtered::<&KinematicCharacterControllerOutput, With<PlayerBody>>();
            actual_walk_speed(query.single(app.world()).ok())
        };
        assert_eq!(measure(&mut app), 0.0);
        let tuning = app.world().resource::<MovementTuning>().clone();
        let forward = Vec2::new(0.0, -1.0);
        let run_target = tuning.target_speed(forward, false, false);
        let sprint_target = tuning.target_speed(forward, false, true);
        for target_speed in [run_target, sprint_target] {
            app.insert_resource(WalkIntent {
                wish_dir: Vec3::NEG_Z,
                target_speed,
                jump_pressed: false,
            });
            for _ in 0..45 {
                app.update();
            }
            let actual = measure(&mut app);
            assert!(
                (actual - target_speed).abs() < 2.0,
                "target {target_speed}, actual {actual}"
            );
        }
        assert_eq!(run_target, 370.0);
        assert_eq!(sprint_target, 555.0);
        app.insert_resource(WalkIntent::default());
        for _ in 0..30 {
            app.update();
        }
        assert!(measure(&mut app) < 0.5);
    }

    #[test]
    fn blocked_player_reports_zero_actual_speed_despite_sprint_target() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(500.0, 300.0, 0.0));
        for _ in 0..150 {
            app.update();
        }
        let sprint_target =
            app.world()
                .resource::<MovementTuning>()
                .target_speed(Vec2::X, false, true);
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: sprint_target,
            jump_pressed: false,
        });
        for _ in 0..120 {
            app.update();
        }
        let mut query = app
            .world_mut()
            .query_filtered::<&KinematicCharacterControllerOutput, With<PlayerBody>>();
        let actual = actual_walk_speed(query.single(app.world()).ok());
        assert!(sprint_target > 0.0 && actual < 1.0, "actual {actual}");
    }

    #[test]
    fn v1_physics_fixture_camera_has_explicit_hdr_output() {
        let mut app = headless::fixture_app();
        crate::color_pipeline::assert_scene_camera_output(app.world_mut(), 1);
    }

    #[test]
    fn tankards_fall_and_settle_on_fixture_ground() {
        let mut app = headless::fixture_app();
        let start: Vec<f32> = {
            let mut query = app
                .world_mut()
                .query_filtered::<&Transform, With<DebugTankard>>();
            query.iter(app.world()).map(|t| t.translation.y).collect()
        };
        assert_eq!(start.len(), 3);
        assert!(start.iter().all(|y| *y > 200.0));
        let continuous = app
            .world_mut()
            .query_filtered::<&Ccd, With<DebugTankard>>()
            .iter(app.world())
            .filter(|ccd| ccd.enabled)
            .count();
        assert_eq!(continuous, 3, "every tankard has continuous collision");
        for _ in 0..600 {
            app.update();
        }
        let settled: Vec<Vec3> = {
            let mut query = app
                .world_mut()
                .query_filtered::<&Transform, With<DebugTankard>>();
            query.iter(app.world()).map(|t| t.translation).collect()
        };
        assert_eq!(settled.len(), 3);
        for position in &settled {
            assert!(position.is_finite());
            // Ground top is y=0; cup half-height 18 + handle radius margin.
            assert!(
                position.y > -40.0 && position.y < 400.0,
                "tankard height {position:?}"
            );
        }
        assert!(
            app.world()
                .resource::<PhysicsFixtureState>()
                .tankards_settled
                || app
                    .world()
                    .resource::<PhysicsFixtureMetrics>()
                    .tankards_in_contact
                    > 0,
            "tankards never touched the arena"
        );
    }

    #[test]
    fn walk_capsule_spawns_grounds_and_steps_forward() {
        let mut app = headless::fixture_app();
        let tuning = app.world().resource::<MovementTuning>().clone();
        let spawn = Vec3::new(120.0, 300.0, 120.0);
        headless::place_player(&mut app, spawn);
        for _ in 0..240 {
            app.update();
        }
        let (rest, grounded) = headless::player_pose(&mut app);
        assert!(grounded, "capsule never grounded at {rest:?}");
        // Capsule center rests at half-height + radius above the ground pad.
        let expected_rest = tuning.capsule_half_height() + tuning.capsule_radius;
        assert!(
            (rest.y - expected_rest).abs() < 12.0,
            "rest height {} vs {expected_rest}",
            rest.y
        );
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: tuning.run_speed,
            jump_pressed: false,
        });
        for _ in 0..120 {
            app.update();
        }
        let moved = {
            let mut query = app.world_mut().query::<&Transform>();
            query
                .iter(app.world())
                .any(|t| (t.translation.x - spawn.x) > 100.0)
        };
        assert!(moved, "walk capsule did not advance under run intent");
    }

    #[test]
    fn jump_ascent_survives_ground_probe_and_lands_on_descent() {
        let tuning = MovementTuning::default();
        let mut state = WalkState::default();
        integrate_walk(
            &mut state,
            &WalkIntent {
                jump_pressed: true,
                ..default()
            },
            &tuning,
            PHYSICS_TIMESTEP,
            true,
        );
        for step in 1..=10 {
            integrate_walk(
                &mut state,
                &WalkIntent::default(),
                &tuning,
                PHYSICS_TIMESTEP,
                true,
            );
            assert!(state.jump_active);
            assert!(!state.grounded);
            assert!(
                (state.vertical_velocity
                    - (tuning.jump_launch - tuning.gravity * PHYSICS_TIMESTEP * step as f32))
                    .abs()
                    < 0.01
            );
        }
        // A new press cannot relaunch while the ground probe overlaps ascent.
        let before = state.vertical_velocity;
        integrate_walk(
            &mut state,
            &WalkIntent {
                jump_pressed: true,
                ..default()
            },
            &tuning,
            PHYSICS_TIMESTEP,
            true,
        );
        assert!(state.vertical_velocity < before);
        while state.vertical_velocity > 0.0 {
            integrate_walk(
                &mut state,
                &WalkIntent::default(),
                &tuning,
                PHYSICS_TIMESTEP,
                false,
            );
        }
        integrate_walk(
            &mut state,
            &WalkIntent::default(),
            &tuning,
            PHYSICS_TIMESTEP,
            true,
        );
        assert!(state.grounded);
        assert!(!state.jump_active);
        assert_eq!(state.vertical_velocity, 0.0);
    }

    #[test]
    fn jump_launches_and_returns_to_ground() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(120.0, 200.0, 120.0));
        for _ in 0..240 {
            app.update();
        }
        let (rest, grounded) = headless::player_pose(&mut app);
        assert!(grounded, "player did not settle before jump: {rest:?}");
        let rest_y = rest.y;
        app.insert_resource(WalkIntent {
            jump_pressed: true,
            ..default()
        });
        let mut apex = rest_y;
        for _ in 0..6 {
            app.update();
            apex = apex.max(headless::player_pose(&mut app).0.y);
        }
        app.insert_resource(WalkIntent::default());
        for _ in 0..240 {
            app.update();
            apex = apex.max(headless::player_pose(&mut app).0.y);
        }
        let (landing, grounded) = headless::player_pose(&mut app);
        // v^2/2g = 340^2/1800 ~ 64 units of apex above rest.
        assert!(
            apex - rest_y > 30.0,
            "jump apex {apex} too low above rest {rest_y}"
        );
        assert!(grounded, "player did not land: {landing:?}");
        assert!(
            (landing.y - rest_y).abs() < 2.0,
            "player landed at {} instead of {rest_y}",
            landing.y
        );
    }
}

// ---------------------------------------------------------------------------
// T3: mouse-look NOCLIP, V toggle, overlay, cursor lifecycle.
// ---------------------------------------------------------------------------

/// Noclip flight tuning (V6).
pub const NOCLIP_SPEED: f32 = 900.0;
pub const NOCLIP_FAST_MULTIPLIER: f32 = 4.0;
pub const MOUSE_SENSITIVITY: f32 = 0.0025;
pub const MAX_PITCH_RADIANS: f32 = 1.5533;
const SPRINT_FOV_BONUS_DEGREES: f32 = 8.0;
const SPRINT_FOV_HALF_LIFE_SECS: f32 = 0.12;
const SPEED_DISPLAY_DEADZONE: f32 = 5.0;

/// Sampled look intent (V6, V18).
#[derive(Debug, Clone, Copy, Default, Resource)]
pub struct LookIntent {
    pub yaw: f32,
    pub pitch: f32,
}

/// Marker for the NOCLIP status overlay text (V7).
#[derive(Component, Debug, Clone, Copy)]
pub struct NoclipOverlay;

/// Marker for the camera owned by the NOCLIP/WALK controller.
#[derive(Component, Debug, Clone, Copy)]
pub struct ControlledCamera;

/// Cursor capture state machine (V6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Resource)]
pub enum CursorCapture {
    #[default]
    Released,
    Captured,
}

/// Clamp pitch into the bounded look range (V6).
pub fn clamp_pitch(pitch: f32) -> f32 {
    pitch.clamp(-MAX_PITCH_RADIANS, MAX_PITCH_RADIANS)
}

/// Sample mouse motion into look intent; uncaptured pointer yields nothing (V6).
pub fn sample_look_intent(capture: &CursorCapture, mouse_delta: Vec2, intent: &mut LookIntent) {
    if *capture != CursorCapture::Captured {
        return;
    }
    intent.yaw -= mouse_delta.x * MOUSE_SENSITIVITY;
    intent.pitch = clamp_pitch(intent.pitch - mouse_delta.y * MOUSE_SENSITIVITY);
}

/// Noclip displacement from held keys relative to the view (V6).
pub fn noclip_displacement(
    forward: Vec3,
    right: Vec3,
    fly_up: bool,
    fly_down: bool,
    strafe: Vec2,
    fast: bool,
    dt: f32,
) -> Vec3 {
    let mut direction = forward * -strafe.y + right * strafe.x;
    if fly_up {
        direction += Vec3::Y;
    }
    if fly_down {
        direction -= Vec3::Y;
    }
    let speed = if fast {
        NOCLIP_SPEED * NOCLIP_FAST_MULTIPLIER
    } else {
        NOCLIP_SPEED
    };
    direction.normalize_or_zero() * speed * dt
}

fn noclip_overlay_text(mode: MoveMode, status: &WalkEntryStatus) -> String {
    let base = match mode {
        MoveMode::Noclip => "NOCLIP: ON",
        MoveMode::Walk => "NOCLIP: OFF",
    };
    match &status.blocked_reason {
        Some(reason) => format!("{base}  WALK blocked: {reason}"),
        None => base.to_owned(),
    }
}

/// Flip the Rapier collider outlines for the `tcg` command.
fn flip_collision_debug(debug: &mut DebugRenderContext) {
    debug.enabled = !debug.enabled;
}

/// Keep a collider when any part of its world-space AABB reaches the camera's
/// forward half-space. Large terrain pieces crossing the camera plane stay visible.
fn debug_collider_in_front(camera: &Transform, mins: Vec3, maxs: Vec3) -> bool {
    let forward = camera.forward().as_vec3();
    let center = (mins + maxs) * 0.5;
    let half_extents = (maxs - mins) * 0.5;
    forward.dot(center - camera.translation) + forward.abs().dot(half_extents) >= 0.0
}

fn update_collision_debug_visibility(
    debug: Res<DebugRenderContext>,
    camera: Query<&Transform, (With<StreamingCamera>, With<ControlledCamera>)>,
    context: ReadRapierContext,
    overrides: Query<Option<&ColliderDebug>, With<Collider>>,
    mut commands: Commands,
) {
    if !debug.enabled {
        return;
    }
    let (Ok(camera), Ok(context)) = (camera.single(), context.single()) else {
        return;
    };
    for (_, collider) in context.colliders.colliders.iter() {
        let entity = Entity::from_bits(collider.user_data as u64);
        let Ok(current) = overrides.get(entity) else {
            continue;
        };
        let bounds = collider.compute_aabb();
        let mins = Vec3::new(bounds.mins.x, bounds.mins.y, bounds.mins.z);
        let maxs = Vec3::new(bounds.maxs.x, bounds.maxs.y, bounds.maxs.z);
        let wanted = if debug_collider_in_front(camera, mins, maxs) {
            ColliderDebug::AlwaysRender
        } else {
            ColliderDebug::NeverRender
        };
        if current.copied() != Some(wanted) {
            commands.entity(entity).insert(wanted);
        }
    }
}

fn setup_fixture_player(
    mut commands: Commands,
    tuning: Res<MovementTuning>,
    mut look: ResMut<LookIntent>,
    camera: Query<(Entity, &Transform), With<StreamingCamera>>,
) {
    let Ok((camera, view)) = camera.single() else {
        return;
    };
    setup_controlled_player(
        &mut commands,
        &tuning,
        &mut look,
        camera,
        view,
        MoveMode::Noclip,
    );
}

fn setup_world_player(
    mut commands: Commands,
    tuning: Res<MovementTuning>,
    ground_height: Option<Res<WalkSpawnGroundHeight>>,
    mut look: ResMut<LookIntent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    camera: Query<(Entity, &Transform), With<StreamingCamera>>,
) {
    let Ok((camera, view)) = camera.single() else {
        return;
    };
    setup_controlled_player(
        &mut commands,
        &tuning,
        &mut look,
        camera,
        view,
        MoveMode::Walk,
    );
    commands.insert_resource(WalkSpawnHold {
        active: true,
        elapsed: 0.0,
        sampled_ground_y: ground_height.as_deref().and_then(|height| height.0),
        destination: false,
    });
    commands.insert_resource(tankard_visuals(&mut meshes, &mut materials));
}

fn setup_controlled_player(
    commands: &mut Commands,
    tuning: &MovementTuning,
    look: &mut LookIntent,
    camera: Entity,
    view: &Transform,
    initial_mode: MoveMode,
) {
    let (yaw, pitch, _) = view.rotation.to_euler(EulerRot::YXZ);
    *look = LookIntent { yaw, pitch };
    let body = commands
        .spawn((
            player_controller_bundle(tuning),
            Transform {
                translation: view.translation - Vec3::Y * tuning.eye_height,
                rotation: if initial_mode == MoveMode::Walk {
                    Quat::from_rotation_y(yaw)
                } else {
                    Quat::IDENTITY
                },
                ..default()
            },
        ))
        .id();
    commands.insert_resource(initial_mode);
    if initial_mode == MoveMode::Noclip {
        commands.entity(body).insert(RigidBodyDisabled);
    }
    commands.entity(camera).insert(ControlledCamera);
    commands.spawn((
        Name::new("Noclip overlay"),
        NoclipOverlay,
        Text::new(noclip_overlay_text(
            initial_mode,
            &WalkEntryStatus::default(),
        )),
        TextFont::from_font_size(18.0),
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(12.0),
            left: Val::Px(12.0),
            ..default()
        },
    ));
}

type PickupFilter = Or<(With<DebugTankard>, With<DynamicClutter>)>;

/// Resolve a compound collider child to the authored dynamic-body root.
fn pickup_root(
    mut entity: Entity,
    parents: &Query<&ChildOf>,
    objects: &Query<(), PickupFilter>,
) -> Option<Entity> {
    loop {
        if objects.get(entity).is_ok() {
            return Some(entity);
        }
        entity = parents.get(entity).ok()?.parent();
    }
}

/// T spawns a test tankard; E grabs or drops a tankard or authored dynamic clutter,
/// only while the cursor is captured. Debug tankards stay in world coordinates and are never
/// children of a streamed cell; authored bodies keep their streamed-cell parent.
#[allow(clippy::too_many_arguments)]
fn world_tankard_input(
    keyboard: Res<ButtonInput<KeyCode>>,
    capture: Res<CursorCapture>,
    camera: Query<&Transform, (With<StreamingCamera>, With<ControlledCamera>)>,
    terrain: Query<(), With<TerrainCollider>>,
    tankards: Query<(), With<DebugTankard>>,
    interactables: Query<(), PickupFilter>,
    parents: Query<&ChildOf>,
    player: Query<Entity, With<PlayerBody>>,
    visuals: Option<Res<TankardVisuals>>,
    mut held: ResMut<HeldTankard>,
    context: ReadRapierContext,
    mut commands: Commands,
) {
    if *capture != CursorCapture::Captured {
        return;
    }
    let (Ok(camera), Ok(context)) = (camera.single(), context.single()) else {
        return;
    };
    if keyboard.just_pressed(KeyCode::KeyT)
        && tankards.iter().count() < MAX_LIVE_TANKARDS
        && let Some(visuals) = visuals
    {
        let horizontal = camera.forward().as_vec3().with_y(0.0).normalize_or_zero();
        let origin = camera.translation + horizontal * 140.0 + Vec3::Y * 300.0;
        let is_terrain = |entity| terrain.get(entity).is_ok();
        if let Some((_, distance)) = context.cast_ray(
            origin,
            Vec3::NEG_Y,
            700.0,
            true,
            QueryFilter::default().predicate(&is_terrain),
        ) {
            spawn_debug_tankard(
                &mut commands,
                &visuals,
                origin - Vec3::Y * distance + Vec3::Y * 120.0,
            );
        }
    }
    if !keyboard.just_pressed(KeyCode::KeyE) {
        return;
    }
    if let Some(entity) = held.0.take() {
        if interactables.get(entity).is_ok() {
            commands
                .entity(entity)
                .remove::<(RigidBodyDisabled, ColliderDisabled)>();
            commands.entity(entity).insert((
                Velocity::zero(),
                Sleeping {
                    sleeping: false,
                    ..default()
                },
            ));
        }
        return;
    }
    let filter = if let Ok(player) = player.single() {
        QueryFilter::default().exclude_rigid_body(player)
    } else {
        QueryFilter::default()
    };
    let selected = context
        .cast_ray(
            camera.translation,
            camera.forward().as_vec3(),
            240.0,
            true,
            filter,
        )
        .and_then(|(entity, _)| pickup_root(entity, &parents, &interactables));
    if let Some(entity) = selected {
        commands
            .entity(entity)
            .insert((RigidBodyDisabled, ColliderDisabled, Velocity::zero()));
        held.0 = Some(entity);
    }
}

#[allow(clippy::type_complexity)]
fn move_held_tankard(
    mut held: ResMut<HeldTankard>,
    camera: Query<
        &Transform,
        (
            With<StreamingCamera>,
            With<ControlledCamera>,
            Without<DebugTankard>,
            Without<DynamicClutter>,
        ),
    >,
    mut tankards: Query<
        (&mut Transform, Option<&ChildOf>),
        (
            Or<(With<DebugTankard>, With<DynamicClutter>)>,
            Without<StreamingCamera>,
        ),
    >,
    parents: Query<&GlobalTransform>,
) {
    let Some(entity) = held.0 else { return };
    let (Ok(camera), Ok((mut tankard, parent))) = (camera.single(), tankards.get_mut(entity))
    else {
        held.0 = None;
        return;
    };
    let position = camera.translation + camera.forward().as_vec3() * 110.0;
    tankard.translation = match parent {
        Some(parent) => {
            let Ok(transform) = parents.get(parent.parent()) else {
                return;
            };
            transform.affine().inverse().transform_point3(position)
        }
        None => position,
    };
}

#[allow(clippy::too_many_arguments)]
fn cursor_lifecycle_system(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window>,
    mut capture: ResMut<CursorCapture>,
    mut cursor_options: Query<&mut CursorOptions>,
    mut intent: ResMut<WalkIntent>,
    mut sprint: ResMut<SprintLatch>,
    mut states: Query<&mut WalkState>,
    mut controllers: Query<&mut KinematicCharacterController>,
    console: Option<Res<ConsoleState>>,
    menu: Option<Res<crate::settings_ui::SettingsMenuState>>,
) {
    // Focus loss or F3 releases; click recaptures (V6). The console and settings menu own capture
    // while open and on their closing frame; the cursor options below still follow that state.
    let focused = windows.iter().all(|window| window.focused);
    let reporting_capture = console
        .as_ref()
        .is_some_and(|console| console.capture_suppressed);
    let console_owns_capture = console
        .is_some_and(|console| console.open || console.closing || console.capture_suppressed)
        || menu.is_some_and(|menu| menu.blocks_gameplay());
    if reporting_capture {
        *capture = CursorCapture::Released;
        clear_motion_state(&mut intent, &mut sprint, &mut states, &mut controllers);
    }
    if !console_owns_capture {
        if !focused || keyboard.just_pressed(KeyCode::F3) {
            if *capture == CursorCapture::Captured {
                *capture = CursorCapture::Released;
                clear_motion_state(&mut intent, &mut sprint, &mut states, &mut controllers);
            }
        } else if mouse_buttons.just_pressed(MouseButton::Left)
            && *capture == CursorCapture::Released
        {
            *capture = CursorCapture::Captured;
        }
    }
    let (grab_mode, visible) = match *capture {
        CursorCapture::Captured => (CursorGrabMode::Locked, false),
        CursorCapture::Released => (CursorGrabMode::None, true),
    };
    for mut options in &mut cursor_options {
        options.grab_mode = grab_mode;
        options.visible = visible;
    }
}

fn look_input_system(
    capture: Res<CursorCapture>,
    mut mouse_motion: MessageReader<MouseMotion>,
    mut intent: ResMut<LookIntent>,
) {
    let mut delta = Vec2::ZERO;
    for motion in mouse_motion.read() {
        delta += motion.delta;
    }
    sample_look_intent(&capture, delta, &mut intent);
}

fn noclip_flight_system(
    mode: Res<MoveMode>,
    capture: Res<CursorCapture>,
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    look: Res<LookIntent>,
    mut camera: Query<&mut Transform, (With<StreamingCamera>, With<ControlledCamera>)>,
) {
    if *mode != MoveMode::Noclip || *capture != CursorCapture::Captured {
        return;
    }
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };
    transform.rotation = Quat::from_euler(EulerRot::YXZ, look.yaw, look.pitch, 0.0);
    let forward = transform.forward().as_vec3();
    let right = transform.right().as_vec3();
    let strafe = Vec2::new(
        (keyboard.pressed(KeyCode::KeyD) as i8 - keyboard.pressed(KeyCode::KeyA) as i8) as f32,
        (keyboard.pressed(KeyCode::KeyS) as i8 - keyboard.pressed(KeyCode::KeyW) as i8) as f32,
    );
    transform.translation += noclip_displacement(
        forward,
        right,
        keyboard.pressed(KeyCode::Space),
        keyboard.pressed(KeyCode::ControlLeft) || keyboard.pressed(KeyCode::ControlRight),
        strafe,
        keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight),
        time.delta_secs(),
    );
}

#[allow(clippy::too_many_arguments)] // Bevy system parameters are supplied independently.
fn walk_intent_system(
    mode: Res<MoveMode>,
    capture: Res<CursorCapture>,
    keyboard: Res<ButtonInput<KeyCode>>,
    tuning: Res<MovementTuning>,
    look: Res<LookIntent>,
    mut intent: ResMut<WalkIntent>,
    mut sprint: ResMut<SprintLatch>,
    mut player: Query<&mut Transform, (With<PlayerBody>, Without<StreamingCamera>)>,
) {
    if *mode != MoveMode::Walk || *capture != CursorCapture::Captured {
        sprint.0 = false;
        return;
    }
    let Ok(mut body) = player.single_mut() else {
        sprint.0 = false;
        return;
    };
    // Yaw moves body, pitch moves view (V9).
    body.rotation = Quat::from_axis_angle(Vec3::Y, look.yaw);
    let forward = Vec3::new(-look.yaw.sin(), 0.0, -look.yaw.cos());
    let right = Vec3::new(look.yaw.cos(), 0.0, -look.yaw.sin());
    let strafe = Vec2::new(
        (keyboard.pressed(KeyCode::KeyD) as i8 - keyboard.pressed(KeyCode::KeyA) as i8) as f32,
        (keyboard.pressed(KeyCode::KeyS) as i8 - keyboard.pressed(KeyCode::KeyW) as i8) as f32,
    );
    let wish = (right * strafe.x - forward * strafe.y).normalize_or_zero();
    let slow = keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight);
    let moving = wish.length_squared() > 0.0;
    if !moving {
        sprint.0 = false;
    } else if keyboard.just_pressed(KeyCode::AltLeft) || keyboard.just_pressed(KeyCode::AltRight) {
        sprint.0 = !sprint.0;
    }
    intent.wish_dir = wish;
    intent.target_speed = if moving {
        tuning.target_speed(strafe, slow, sprint.0)
    } else {
        0.0
    };
    intent.jump_pressed |= keyboard.just_pressed(KeyCode::Space);
}

/// One-shot id of the NOCLIP/WALK switch, shared by the V key and the `tcl` command.
#[derive(Resource, Clone, Copy)]
struct MoveModeEffect(SystemId);

fn register_move_mode_effect(world: &mut World) {
    let id = world.register_system(toggle_move_mode_effect);
    world.insert_resource(MoveModeEffect(id));
}

/// V, only while the cursor is captured.
fn toggle_mode_system(
    keyboard: Res<ButtonInput<KeyCode>>,
    capture: Res<CursorCapture>,
    effect: Res<MoveModeEffect>,
    mut commands: Commands,
) {
    if keyboard.just_pressed(KeyCode::KeyV) && *capture == CursorCapture::Captured {
        commands.run_system(effect.0);
    }
}

/// Switch between NOCLIP and WALK, entering WALK only at a collision-free spot.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn toggle_move_mode_effect(
    mut hold: ResMut<WalkSpawnHold>,
    tuning: Res<MovementTuning>,
    mut crossing: Option<ResMut<crate::door_crossing::DoorCrossing>>,
    context: ReadRapierContext,
    mut mode: ResMut<MoveMode>,
    mut status: ResMut<WalkEntryStatus>,
    mut intent: ResMut<WalkIntent>,
    mut sprint: ResMut<SprintLatch>,
    mut look: ResMut<LookIntent>,
    camera: Query<&Transform, (With<StreamingCamera>, With<ControlledCamera>)>,
    mut player: Query<
        (Entity, &mut Transform, &Collider),
        (With<PlayerBody>, Without<StreamingCamera>),
    >,
    mut states: Query<&mut WalkState>,
    mut controllers: Query<&mut KinematicCharacterController>,
    mut commands: Commands,
) {
    // A reason from an earlier attempt must not outlive it: if this attempt stops early
    // (no camera, player or physics context), the caller reports "no player" instead.
    status.blocked_reason = None;
    if let Some(crossing) = crossing.as_mut().filter(|crossing| crossing.is_active()) {
        if crossing.needs_recovery() {
            let Ok((entity, _, _)) = player.single() else {
                return;
            };
            commands.entity(entity).insert(RigidBodyDisabled);
            *mode = MoveMode::Noclip;
            *hold = WalkSpawnHold::default();
            clear_motion_state(&mut intent, &mut sprint, &mut states, &mut controllers);
            crossing.finish_recovery();
            return;
        }
        status.blocked_reason = Some("door crossing in progress".to_owned());
        return;
    }
    match *mode {
        MoveMode::Noclip => {
            let (Ok(camera), Ok((entity, mut body, collider)), Ok(context)) =
                (camera.single(), player.single_mut(), context.single())
            else {
                return;
            };
            match try_enter_walk(&context, &tuning, camera.translation, collider) {
                Ok(spawn) => {
                    body.translation = spawn;
                    let (yaw, _, _) = camera.rotation.to_euler(EulerRot::YXZ);
                    body.rotation = Quat::from_axis_angle(Vec3::Y, yaw);
                    look.yaw = yaw;
                    commands.entity(entity).remove::<RigidBodyDisabled>();
                    status.blocked_reason = None;
                    *mode = MoveMode::Walk;
                }
                Err(reason) => {
                    status.blocked_reason = Some(reason);
                }
            }
        }
        MoveMode::Walk => {
            let Ok((entity, _, _)) = player.single() else {
                return;
            };
            commands.entity(entity).insert(RigidBodyDisabled);
            status.blocked_reason = None;
            *mode = MoveMode::Noclip;
        }
    }
    *hold = WalkSpawnHold::default();
    clear_motion_state(&mut intent, &mut sprint, &mut states, &mut controllers);
}

// ---------------------------------------------------------------------------
// Console commands for the debug keys, under Skyrim's names. Each runs the same
// effect as its key. The `T` (tankard) and `E` (grab) keys have no console
// command: Skyrim has neither.
// ---------------------------------------------------------------------------

fn no_args(args: &[&str], usage: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("usage: {usage}"))
    }
}

/// Skyrim's `ToggleCollision` / `tcl`; the `V` key runs the same effect.
fn toggle_collision_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "ToggleCollision",
        short: Some("tcl"),
        usage: "ToggleCollision",
        help: "toggle noclip flight and walking (key V)",
        handler: Box::new(|world, args| {
            no_args(args, "ToggleCollision")?;
            if !world.contains_resource::<MoveMode>() {
                return Err("noclip is not available in this run".to_owned());
            }
            let before = *world.resource::<MoveMode>();
            let recovering = world
                .get_resource::<crate::door_crossing::DoorCrossing>()
                .is_some_and(|crossing| crossing.needs_recovery());
            let Some(effect) = world.get_resource::<MoveModeEffect>().map(|e| e.0) else {
                return Err("noclip is not available in this run".to_owned());
            };
            world.run_system(effect).map_err(|e| e.to_string())?;
            let after = *world.resource::<MoveMode>();
            let recovered = recovering
                && world
                    .get_resource::<crate::door_crossing::DoorCrossing>()
                    .is_some_and(|crossing| !crossing.is_active());
            if after == before && !recovered {
                let reason = world
                    .get_resource::<WalkEntryStatus>()
                    .and_then(|status| status.blocked_reason.clone());
                return Err(match reason {
                    Some(reason) => format!("cannot walk here: {reason}"),
                    None => "no player to move in this run".to_owned(),
                });
            }
            Ok(match after {
                MoveMode::Noclip => "noclip ON".to_owned(),
                MoveMode::Walk => "noclip OFF (walking)".to_owned(),
            })
        }),
    }
}

/// Skyrim's `ToggleCollisionGeometry` / `tcg`; also available in the Graphics settings tab.
fn toggle_collision_geometry_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "ToggleCollisionGeometry",
        short: Some("tcg"),
        usage: "ToggleCollisionGeometry",
        help: "toggle the collision outlines (also in Graphics settings)",
        handler: Box::new(|world, args| {
            no_args(args, "ToggleCollisionGeometry")?;
            let Some(mut debug) = world.get_resource_mut::<DebugRenderContext>() else {
                return Err("collision outlines are not available in this run".to_owned());
            };
            flip_collision_debug(&mut debug);
            Ok(format!(
                "collision outlines {}",
                if debug.enabled { "ON" } else { "OFF" }
            ))
        }),
    }
}

#[allow(clippy::too_many_arguments)] // Bevy system parameters are supplied independently.
fn overlay_system(
    mode: Res<MoveMode>,
    hold: Option<Res<WalkSpawnHold>>,
    status: Res<WalkEntryStatus>,
    sprint: Res<SprintLatch>,
    crossing: Option<Res<crate::door_crossing::DoorCrossing>>,
    collision_debug: Option<Res<DebugRenderContext>>,
    player: Query<Option<&KinematicCharacterControllerOutput>, With<PlayerBody>>,
    mut overlay: Query<&mut Text, With<NoclipOverlay>>,
) {
    let Ok(mut text) = overlay.single_mut() else {
        return;
    };
    let mut label = noclip_overlay_text(*mode, &status);
    if hold.as_ref().is_some_and(|hold| hold.active) {
        label.push_str("  waiting for WALK collision");
    }
    if *mode == MoveMode::Walk {
        let speed = actual_walk_speed(player.single().ok().flatten());
        let sprint_status = if sprint.0 { "ON" } else { "OFF" };
        label.push_str(&format!("  SPRINT: {sprint_status}  SPEED: {speed:.0} u/s"));
    }
    if let Some(debug) = collision_debug {
        label.push_str(if debug.enabled {
            "  COLLISION: ON"
        } else {
            "  COLLISION: OFF"
        });
    }
    if crossing.is_some_and(|crossing| crossing.needs_recovery()) {
        label.push_str(
            "\nDoor crossing failed. Movement paused. Press V or use tcl to recover in noclip.",
        );
    }
    **text = label;
}

/// Horizontal, collision-resolved player speed from the last fixed physics step.
fn actual_walk_speed(output: Option<&KinematicCharacterControllerOutput>) -> f32 {
    let Some(output) = output else { return 0.0 };
    let movement = output.effective_translation;
    let speed = Vec2::new(movement.x, movement.z).length() / PHYSICS_TIMESTEP;
    if speed < SPEED_DISPLAY_DEADZONE {
        0.0
    } else {
        speed
    }
}

fn exponential_fov_step(current: f32, target: f32, delta_secs: f32) -> f32 {
    let remaining = (-delta_secs / SPRINT_FOV_HALF_LIFE_SECS).exp2();
    target + (current - target) * remaining
}

fn sprint_fov_system(
    mode: Res<MoveMode>,
    sprint: Res<SprintLatch>,
    time: Res<Time>,
    player: Query<Option<&KinematicCharacterControllerOutput>, With<PlayerBody>>,
    mut camera: Query<&mut Projection, (With<StreamingCamera>, With<ControlledCamera>)>,
    settings: Option<Res<crate::settings::RenderingSettings>>,
) {
    let Ok(mut projection) = camera.single_mut() else {
        return;
    };
    let Projection::Perspective(perspective) = &mut *projection else {
        return;
    };
    let base_fov = settings
        .as_deref()
        .map_or(PerspectiveProjection::default().fov, |settings| {
            settings.fov_degrees.to_radians()
        });
    let moving = actual_walk_speed(player.single().ok().flatten()) > 1.0;
    let target_fov = if *mode == MoveMode::Walk && sprint.0 && moving {
        base_fov + SPRINT_FOV_BONUS_DEGREES.to_radians()
    } else {
        base_fov
    };
    perspective.fov = exponential_fov_step(perspective.fov, target_fov, time.delta_secs());
}

#[cfg(test)]
mod noclip_tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn authored_compound_clutter_grab_uses_parent_space_and_wakes_on_drop() {
        let mut app = headless::fixture_app();
        app.init_resource::<HeldTankard>();
        app.insert_resource(CursorCapture::Captured);
        let parent = app
            .world_mut()
            .spawn((
                Transform::from_xyz(500.0, 0.0, 0.0),
                GlobalTransform::default(),
            ))
            .id();
        let body = app
            .world_mut()
            .spawn((
                DynamicClutter {
                    max_linear_velocity: 2000.0,
                    max_angular_velocity: 20.0,
                },
                RigidBody::Dynamic,
                Velocity::zero(),
                Sleeping {
                    sleeping: true,
                    ..default()
                },
                Transform::from_xyz(0.0, 200.0, -160.0),
                GlobalTransform::default(),
                ChildOf(parent),
            ))
            .id();
        app.world_mut().spawn((
            Collider::cuboid(10.0, 10.0, 10.0),
            clutter_collision_groups(),
            Transform::default(),
            GlobalTransform::default(),
            ChildOf(body),
        ));
        let camera = app
            .world_mut()
            .query_filtered::<Entity, With<ControlledCamera>>()
            .single(app.world())
            .unwrap();
        *app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap() = Transform::from_xyz(500.0, 200.0, 0.0);
        app.update();
        app.insert_resource(CursorCapture::Captured);
        *app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap() = Transform::from_xyz(500.0, 200.0, 0.0);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        app.world_mut()
            .run_system_once(world_tankard_input)
            .unwrap();
        assert_eq!(app.world().resource::<HeldTankard>().0, Some(body));
        assert!(app.world().entity(body).contains::<RigidBodyDisabled>());
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation
            .x += 40.0;
        app.world_mut().run_system_once(move_held_tankard).unwrap();
        let position = app
            .world()
            .entity(body)
            .get::<Transform>()
            .unwrap()
            .translation;
        assert!(
            position.abs_diff_eq(Vec3::new(40.0, 200.0, -110.0), 0.01),
            "{position:?}"
        );
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.release(KeyCode::KeyE);
            keys.clear();
            keys.press(KeyCode::KeyE);
        }
        app.world_mut()
            .run_system_once(world_tankard_input)
            .unwrap();
        assert_eq!(app.world().resource::<HeldTankard>().0, None);
        assert!(!app.world().entity(body).contains::<RigidBodyDisabled>());
        assert!(!app.world().entity(body).get::<Sleeping>().unwrap().sleeping);
        for _ in 0..10 {
            app.update();
        }
        assert!(
            app.world()
                .entity(body)
                .get::<Transform>()
                .unwrap()
                .translation
                .y
                < position.y
        );
    }

    #[test]
    fn sprint_fov_interpolation_is_exponential_and_returns_to_base() {
        let base = PerspectiveProjection::default().fov;
        let sprint = base + SPRINT_FOV_BONUS_DEGREES.to_radians();
        let mut at_30_hz = base;
        let mut at_120_hz = base;
        for _ in 0..30 {
            at_30_hz = exponential_fov_step(at_30_hz, sprint, 1.0 / 30.0);
        }
        for _ in 0..120 {
            at_120_hz = exponential_fov_step(at_120_hz, sprint, 1.0 / 120.0);
        }
        assert!((at_30_hz - at_120_hz).abs() < 0.0001);
        assert!((at_30_hz - sprint).abs() < 0.001);
        for _ in 0..60 {
            at_30_hz = exponential_fov_step(at_30_hz, base, 1.0 / 60.0);
        }
        assert!((at_30_hz - base).abs() < 0.001);
    }

    #[test]
    fn sprint_fov_changes_only_the_controlled_camera_during_actual_motion() {
        let mut app = App::new();
        app.insert_resource(MoveMode::Walk)
            .insert_resource(SprintLatch(true))
            .insert_resource(Time::<()>::default());
        app.world_mut()
            .resource_mut::<Time<()>>()
            .advance_by(std::time::Duration::from_secs_f32(1.0 / 60.0));
        let base = PerspectiveProjection::default().fov;
        let controlled = app
            .world_mut()
            .spawn((
                StreamingCamera,
                ControlledCamera,
                Projection::Perspective(PerspectiveProjection::default()),
            ))
            .id();
        let unrelated = app
            .world_mut()
            .spawn(Projection::Perspective(PerspectiveProjection::default()))
            .id();
        app.world_mut().spawn((
            PlayerBody,
            KinematicCharacterControllerOutput {
                grounded: true,
                desired_translation: Vec3::ZERO,
                effective_translation: Vec3::new(0.0, 0.0, -10.0),
                collisions: Vec::new(),
                is_sliding_down_slope: false,
            },
        ));
        app.world_mut().run_system_once(sprint_fov_system).unwrap();
        let fov = |app: &App, entity| match app.world().get::<Projection>(entity).unwrap() {
            Projection::Perspective(perspective) => perspective.fov,
            _ => panic!("expected perspective camera"),
        };
        let widened = fov(&app, controlled);
        assert!(widened > base);
        assert_eq!(fov(&app, unrelated), base);
        app.insert_resource(MoveMode::Noclip);
        app.world_mut().run_system_once(sprint_fov_system).unwrap();
        assert!(fov(&app, controlled) < widened);
    }

    #[test]
    fn collision_debug_keeps_shapes_crossing_view_plane() {
        let camera = Transform::IDENTITY;
        assert!(debug_collider_in_front(
            &camera,
            Vec3::new(-2.0, -2.0, -10.0),
            Vec3::new(2.0, 2.0, -2.0)
        ));
        assert!(debug_collider_in_front(
            &camera,
            Vec3::new(-2.0, -2.0, -2.0),
            Vec3::new(2.0, 2.0, 2.0)
        ));
        assert!(!debug_collider_in_front(
            &camera,
            Vec3::new(-2.0, -2.0, 2.0),
            Vec3::new(2.0, 2.0, 10.0)
        ));
    }

    #[test]
    fn walk_view_pitch_tracks_mouse_look_without_tilting_body() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
        app.world_mut().resource_mut::<LookIntent>().pitch = 0.35;
        app.world_mut()
            .run_system_once(walk_camera_follow_system)
            .unwrap();
        let mut camera = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>();
        let view = camera.single(app.world()).unwrap();
        let (_, pitch, _) = view.rotation.to_euler(EulerRot::YXZ);
        assert!((pitch - 0.35).abs() < 1.0e-5);
        let mut player = app
            .world_mut()
            .query_filtered::<&Transform, With<PlayerBody>>();
        let body = player.single(app.world()).unwrap();
        assert!(body.rotation.to_euler(EulerRot::YXZ).1.abs() < 1.0e-5);
    }

    #[test]
    fn jump_press_survives_input_updates_until_fixed_tick() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
        app.insert_resource(CursorCapture::Captured);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Space);
        app.world_mut().run_system_once(walk_intent_system).unwrap();
        assert!(app.world().resource::<WalkIntent>().jump_pressed);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut().run_system_once(walk_intent_system).unwrap();
        assert!(app.world().resource::<WalkIntent>().jump_pressed);
    }

    #[test]
    fn pitch_clamps_inside_bounded_range() {
        assert_eq!(clamp_pitch(5.0), MAX_PITCH_RADIANS);
        assert_eq!(clamp_pitch(-5.0), -MAX_PITCH_RADIANS);
        assert_eq!(clamp_pitch(0.3), 0.3);
    }

    #[test]
    fn uncaptured_pointer_yields_no_look_intent() {
        let mut intent = LookIntent::default();
        sample_look_intent(&CursorCapture::Released, Vec2::new(40.0, 20.0), &mut intent);
        assert_eq!(intent.yaw, 0.0);
        assert_eq!(intent.pitch, 0.0);
        sample_look_intent(&CursorCapture::Captured, Vec2::new(40.0, 20.0), &mut intent);
        assert!(intent.yaw < 0.0);
        assert!(intent.pitch < 0.0);
    }

    #[test]
    fn noclip_flies_relative_to_view_with_fast_multiplier() {
        let slow = noclip_displacement(
            Vec3::NEG_Z,
            Vec3::X,
            false,
            false,
            Vec2::new(0.0, -1.0),
            false,
            1.0,
        );
        assert!((slow - Vec3::NEG_Z * NOCLIP_SPEED).length() < 0.01);
        let fast = noclip_displacement(
            Vec3::NEG_Z,
            Vec3::X,
            false,
            false,
            Vec2::new(0.0, -1.0),
            true,
            1.0,
        );
        assert!((fast.length() - NOCLIP_SPEED * NOCLIP_FAST_MULTIPLIER).abs() < 0.01);
        let rise = noclip_displacement(Vec3::NEG_Z, Vec3::X, true, false, Vec2::ZERO, false, 1.0);
        assert!((rise - Vec3::Y * NOCLIP_SPEED).length() < 0.01);
    }

    #[test]
    fn overlay_reports_mode_and_block_reason_without_control_hints() {
        let status = WalkEntryStatus::default();
        assert_eq!(noclip_overlay_text(MoveMode::Noclip, &status), "NOCLIP: ON");
        assert_eq!(noclip_overlay_text(MoveMode::Walk, &status), "NOCLIP: OFF");
        let blocked = WalkEntryStatus {
            blocked_reason: Some("no free capsule placement nearby".to_owned()),
        };
        assert!(noclip_overlay_text(MoveMode::Noclip, &blocked).contains("WALK blocked"));
    }
}

/// P1 gate evidence (T4, V16): slope/wall/step behavior, toggle state
/// agreement, fixed-step determinism, and render-origin rebase alignment.
#[cfg(test)]
mod gate_tests {
    use super::headless;
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn walk_crosses_narrow_raised_floor_join() {
        let mut app = headless::fixture_app();
        for (x, width, top) in [(0.0, 12.0, 15.0), (206.0, 400.0, 30.0)] {
            app.world_mut().spawn((
                RigidBody::Fixed,
                Collider::cuboid(width * 0.5, top * 0.5, 80.0),
                world_collision_groups(),
                Transform::from_xyz(x, top * 0.5, 850.0),
            ));
        }
        headless::place_player(&mut app, Vec3::new(-150.0, 200.0, 850.0));
        for _ in 0..150 {
            app.update();
        }
        let start = headless::player_pose(&mut app).0;
        assert!(start.y < 80.0, "player never grounded: {start:?}");
        let tuning = app.world().resource::<MovementTuning>();
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: tuning.run_speed,
            jump_pressed: false,
        });
        let mut peak = start.y;
        for _ in 0..180 {
            app.update();
            peak = peak.max(headless::player_pose(&mut app).0.y);
        }
        let (end, grounded) = headless::player_pose(&mut app);
        assert!(
            end.x > 120.0 && peak > start.y + 25.0 && grounded,
            "player stuck at raised floor join: {start:?} -> {end:?}, peak={peak}, grounded={grounded}"
        );
    }

    #[test]
    fn walk_camera_pitch_uses_current_mouse_intent() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
        app.world_mut().resource_mut::<LookIntent>().pitch = 0.6;
        app.update();
        let mut camera = app
            .world_mut()
            .query_filtered::<&Transform, With<ControlledCamera>>();
        let view = camera.single(app.world()).expect("controlled camera");
        let (_, pitch, _) = view.rotation.to_euler(EulerRot::YXZ);
        assert!((pitch - 0.6).abs() < 0.001, "WALK pitch was {pitch}");
        let mut body = app
            .world_mut()
            .query_filtered::<&Transform, With<PlayerBody>>();
        let pose = body.single(app.world()).expect("player body");
        assert!(pose.rotation.x.abs() < 0.001 && pose.rotation.z.abs() < 0.001);
    }

    #[test]
    fn riverwood_tankard_spawn_pickup_and_drop() {
        let mut app = headless::fixture_app();
        app.init_resource::<HeldTankard>();
        let visuals = {
            let world = app.world_mut();
            world.resource_scope(|world, mut meshes: Mut<Assets<Mesh>>| {
                tankard_visuals(
                    &mut meshes,
                    &mut world.resource_mut::<Assets<StandardMaterial>>(),
                )
            })
        };
        app.insert_resource(visuals);
        let ground = {
            let mut query = app
                .world_mut()
                .query_filtered::<(Entity, &Transform), With<FixtureArena>>();
            query
                .iter(app.world())
                .find(|(_, pose)| pose.translation.y == -20.0)
                .unwrap()
                .0
        };
        app.world_mut().entity_mut(ground).insert(TerrainCollider);
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<ControlledCamera>>();
            query.single(app.world()).unwrap()
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .clone_from(
                &Transform::from_xyz(0.0, 200.0, 0.0)
                    .looking_at(Vec3::new(0.0, 200.0, -1.0), Vec3::Y),
            );
        app.update();
        app.insert_resource(CursorCapture::Captured);
        // The Riverwood test tankards already exist; the one T spawns is the one not listed here.
        let existing: Vec<Entity> = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<DebugTankard>>();
            query.iter(app.world()).collect()
        };
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyT);
        let ground_hit = app
            .world_mut()
            .run_system_once(
                |context: ReadRapierContext, terrain: Query<(), With<TerrainCollider>>| {
                    let is_terrain = |entity| terrain.get(entity).is_ok();
                    context.single().unwrap().cast_ray(
                        Vec3::new(0.0, 500.0, -140.0),
                        Vec3::NEG_Y,
                        700.0,
                        true,
                        QueryFilter::default().predicate(&is_terrain),
                    )
                },
            )
            .unwrap();
        let any_hit = app
            .world_mut()
            .run_system_once(|context: ReadRapierContext| {
                context.single().unwrap().cast_ray(
                    Vec3::new(0.0, 500.0, -140.0),
                    Vec3::NEG_Y,
                    700.0,
                    true,
                    QueryFilter::default(),
                )
            })
            .unwrap();
        assert!(
            ground_hit.is_some(),
            "fixture terrain ray must hit; nearest={any_hit:?}, ground={ground:?}"
        );
        app.world_mut()
            .run_system_once(world_tankard_input)
            .unwrap();
        let spawned = {
            let mut query = app
                .world_mut()
                .query_filtered::<(Entity, &Transform), With<DebugTankard>>();
            query
                .iter(app.world())
                .find(|(entity, _)| !existing.contains(entity))
                .map(|(entity, pose)| (entity, pose.translation))
                .expect("spawned tankard above terrain")
        };
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<DebugTankard>>();
        assert_eq!(query.iter(app.world()).count(), 4);
        app.update();
        let spawned_pose = app
            .world()
            .entity(spawned.0)
            .get::<Transform>()
            .unwrap()
            .translation;
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .look_at(spawned_pose, Vec3::Y);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        app.world_mut()
            .run_system_once(world_tankard_input)
            .unwrap();
        assert_eq!(app.world().resource::<HeldTankard>().0, Some(spawned.0));
        assert!(
            app.world()
                .entity(spawned.0)
                .contains::<RigidBodyDisabled>()
        );
        assert!(app.world().entity(spawned.0).contains::<ColliderDisabled>());
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation
            .x += 100.0;
        app.world_mut().run_system_once(move_held_tankard).unwrap();
        let held_pose = app
            .world()
            .entity(spawned.0)
            .get::<Transform>()
            .unwrap()
            .translation;
        assert!(held_pose.x > 90.0, "held tankard did not follow the view");
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.release(KeyCode::KeyE);
            keys.clear();
            keys.press(KeyCode::KeyE);
        }
        app.world_mut()
            .run_system_once(world_tankard_input)
            .unwrap();
        assert_eq!(app.world().resource::<HeldTankard>().0, None);
        assert!(
            !app.world()
                .entity(spawned.0)
                .contains::<RigidBodyDisabled>()
        );
        assert!(!app.world().entity(spawned.0).contains::<ColliderDisabled>());
        app.update();
        let dropped_pose = app
            .world()
            .entity(spawned.0)
            .get::<Transform>()
            .unwrap()
            .translation;
        assert!(
            (dropped_pose - held_pose).length() < 10.0,
            "drop teleported tankard: {held_pose:?} -> {dropped_pose:?}"
        );
    }

    fn run_speed_intent(app: &mut App, wish_dir: Vec3) {
        let speed = app.world().resource::<MovementTuning>().run_speed;
        app.insert_resource(WalkIntent {
            wish_dir,
            target_speed: speed,
            jump_pressed: false,
        });
    }

    #[test]
    fn wall_blocks_step_mounts_slope_climbs() {
        let mut app = headless::fixture_app();
        // Wall face sits at x=730 (center 760, half-width 30).
        headless::place_player(&mut app, Vec3::new(500.0, 300.0, 0.0));
        for _ in 0..120 {
            app.update();
        }
        run_speed_intent(&mut app, Vec3::X);
        for _ in 0..600 {
            app.update();
        }
        let (blocked, _) = headless::player_pose(&mut app);
        let tuning = app.world().resource::<MovementTuning>().clone();
        assert!(
            blocked.x < 730.0 - tuning.capsule_radius,
            "capsule tunneled the wall: {blocked:?}"
        );

        // Step top is y=24; the 36-unit autostep budget includes it (V12).
        headless::place_player(&mut app, Vec3::new(100.0, 300.0, 240.0));
        for _ in 0..120 {
            app.update();
        }
        run_speed_intent(&mut app, Vec3::X);
        for _ in 0..600 {
            app.update();
        }
        let (stepped, step_grounded) = headless::player_pose(&mut app);
        assert!(step_grounded, "capsule never grounded near step");
        let rest = tuning.capsule_half_height() + tuning.capsule_radius;
        // Walk the step again tracking peak: autostep must lift the capsule
        // onto the 24-unit block, not stall against it (V12).
        headless::place_player(&mut app, Vec3::new(100.0, 300.0, 240.0));
        for _ in 0..120 {
            app.update();
        }
        run_speed_intent(&mut app, Vec3::X);
        let mut step_peak = 0.0f32;
        for _ in 0..600 {
            app.update();
            let (pose, _) = headless::player_pose(&mut app);
            step_peak = step_peak.max(pose.y);
        }
        assert!(
            step_peak > rest + 18.0,
            "capsule did not mount the 24-unit step: peak {step_peak}, rest {rest}, end {stepped:?}"
        );

        // Hill rises toward -x; climbing +x-to--x gains height (V12).
        // Slope is ~18 degrees, under the 50-degree climb limit.
        headless::place_player(&mut app, Vec3::new(100.0, 300.0, -500.0));
        for _ in 0..120 {
            app.update();
        }
        run_speed_intent(&mut app, Vec3::NEG_X);
        let mut peak = 0.0f32;
        for _ in 0..600 {
            app.update();
            let (pose, _) = headless::player_pose(&mut app);
            peak = peak.max(pose.y);
        }
        assert!(peak > 200.0, "capsule did not climb the hill: peak {peak}");
    }

    #[test]
    fn tankards_contact_wall_and_settle_without_penetration() {
        let mut app = headless::fixture_app();
        // Hurl one tankard at the wall; it must collide, not tunnel (V15).
        let hurled = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<DebugTankard>>();
            let first = query.iter(app.world()).next().expect("tankard");
            app.world_mut().entity_mut(first).insert((
                Velocity::linear(Vec3::X * 1500.0),
                Transform::from_xyz(0.0, 300.0, 0.0),
            ));
            first
        };
        for _ in 0..900 {
            app.update();
        }
        let position = app
            .world()
            .get::<Transform>(hurled)
            .expect("tankard transform")
            .translation;
        assert!(position.is_finite());
        // Wall spans x in [730, 790]; the cup radius keeps contact outside it.
        assert!(
            position.x < 730.0 - TANKARD_CUP_RADIUS * 0.5 || position.y > 420.0,
            "tankard tunneled the wall: {position:?}"
        );
        // Every tankard rests at or above the ground pad (no sinking).
        let mut query = app
            .world_mut()
            .query_filtered::<&Transform, With<DebugTankard>>();
        for transform in query.iter(app.world()) {
            assert!(
                transform.translation.y > -TANKARD_CUP_HALF_HEIGHT,
                "tankard sank through ground: {:?}",
                transform.translation
            );
        }
    }

    #[test]
    fn toggle_clears_state_and_overlay_matches_mode() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
        for _ in 0..120 {
            app.update();
        }
        // Simulate WALK->NOCLIP: stale intent must clear (V14).
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: 300.0,
            jump_pressed: true,
        });
        let _ = app.world_mut().run_system_once(
            |mut intent: ResMut<WalkIntent>,
             mut sprint: ResMut<SprintLatch>,
             mut states: Query<&mut WalkState>,
             mut controllers: Query<&mut KinematicCharacterController>| {
                clear_motion_state(&mut intent, &mut sprint, &mut states, &mut controllers);
            },
        );
        let intent = app.world().resource::<WalkIntent>();
        assert_eq!(intent.wish_dir, Vec3::ZERO);
        assert!(!intent.jump_pressed);
        // Overlay text tracks the mode resource every frame (V7).
        for _ in 0..5 {
            app.update();
        }
        let mut query = app
            .world_mut()
            .query_filtered::<&Text, With<NoclipOverlay>>();
        let text = query.single(app.world()).expect("overlay").clone();
        assert_eq!(text.as_str(), "NOCLIP: OFF  SPRINT: OFF  SPEED: 0 u/s");
        app.insert_resource(MoveMode::Noclip);
        for _ in 0..5 {
            app.update();
        }
        let mut query = app
            .world_mut()
            .query_filtered::<&Text, With<NoclipOverlay>>();
        let text = query.single(app.world()).expect("overlay").clone();
        assert_eq!(text.as_str(), "NOCLIP: ON");
    }

    #[test]
    fn a_failed_crossing_holds_the_walking_body_and_shows_how_to_recover() {
        let mut app = headless::fixture_app();
        let start = Vec3::new(0.0, 5_000.0, 0.0);
        headless::place_player(&mut app, start);
        app.insert_resource(crate::door_crossing::DoorCrossing::recovery_for_test());
        for _ in 0..60 {
            app.update();
        }
        let (held, _) = headless::player_pose(&mut app);
        assert!((held - start).length() < 1.0, "unguarded fall: {held}");
        let mut query = app
            .world_mut()
            .query_filtered::<&Text, With<NoclipOverlay>>();
        let text = query.single(app.world()).unwrap();
        assert!(text.contains("Movement paused"));
        assert!(text.contains("Press V or use tcl to recover in noclip"));
    }

    #[test]
    fn fixed_step_walk_is_frame_rate_independent() {
        // Two simulated seconds at 30/60/120 render fps must all run the same
        // 120 fixed physics ticks and reach the same point (V18).
        fn drive(render_hz: u32) -> Vec3 {
            let mut app = headless::fixture_app();
            headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
            for _ in 0..120 {
                app.update();
            }
            let speed = app.world().resource::<MovementTuning>().run_speed;
            app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(1.0 / render_hz as f64),
            ));
            for _ in 0..render_hz * 2 {
                app.insert_resource(WalkIntent {
                    wish_dir: Vec3::X,
                    target_speed: speed,
                    jump_pressed: false,
                });
                app.update();
            }
            headless::player_pose(&mut app).0
        }
        let reference = drive(60);
        for render_hz in [30, 120] {
            let pose = drive(render_hz);
            assert!(
                (pose - reference).length() < 10.0,
                "frame-rate divergence at {render_hz} fps: {pose:?} vs {reference:?}"
            );
        }
    }

    #[test]
    fn origin_rebase_keeps_body_camera_and_tankards_aligned() {
        use crate::streaming::RenderOrigin;
        use crate::world::components::CELL_SIZE;

        let mut app = headless::fixture_app();
        app.insert_resource(RenderOrigin(IVec2::ZERO));
        headless::place_player(&mut app, Vec3::new(120.0, 300.0, 120.0));
        for _ in 0..120 {
            app.update();
        }
        // Simulate render-origin rebase: every physics participant shifts
        // by the same cell offset, Rapier tracks Bevy transforms, velocity
        // preserved (V9).
        let shift = Vec3::new(CELL_SIZE, 0.0, CELL_SIZE);
        let before_velocity = {
            let mut query = app
                .world_mut()
                .query_filtered::<&Velocity, With<DebugTankard>>();
            query.iter(app.world()).next().cloned()
        };
        {
            let mut query = app.world_mut().query_filtered::<&mut Transform, Or<(
                With<PlayerBody>,
                With<DebugTankard>,
                With<StreamingCamera>,
                With<FixtureArena>,
            )>>();
            for mut transform in query.iter_mut(app.world_mut()) {
                transform.translation -= shift;
            }
        }
        app.world_mut().resource_mut::<RenderOrigin>().0 += IVec2::new(1, 1);
        for _ in 0..30 {
            app.update();
        }
        let (pose, grounded) = headless::player_pose(&mut app);
        assert!(grounded, "rebase ungrounded the capsule at {pose:?}");
        assert!(pose.is_finite());
        if let Some(before) = before_velocity {
            let after = {
                let mut query = app
                    .world_mut()
                    .query_filtered::<&Velocity, With<DebugTankard>>();
                query.iter(app.world()).next().cloned().unwrap_or_default()
            };
            assert!(
                (after.linear - before.linear).length() < 400.0,
                "rebase injected tankard velocity: {before:?} -> {after:?}"
            );
        }
    }
}

#[cfg(test)]
mod console_tests {
    use super::headless;
    use super::*;
    use crate::console::{ConsolePlugin, execute_line};
    use bevy::ecs::system::RunSystemOnce;

    fn console_fixture() -> App {
        let mut app = headless::fixture_app_with(|app| {
            // `ToggleCollision`/`tcl` comes in with `PlayerControlsPlugin` through the fixture.
            app.add_plugins(ConsolePlugin)
                .add_console_command(toggle_collision_geometry_command())
                .insert_resource(DebugRenderContext::default());
        });
        app.update();
        app
    }

    fn scrollback(app: &App) -> String {
        app.world().resource::<ConsoleState>().scrollback.join("\n")
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
    }

    /// Send a key press the way winit does; `InputPlugin` turns it into `ButtonInput` on update.
    fn send_key(app: &mut App, key_code: KeyCode) {
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code,
                logical_key: bevy::input::keyboard::Key::Character("x".into()),
                state: bevy::input::ButtonState::Pressed,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
    }

    fn mode(app: &App) -> MoveMode {
        *app.world().resource::<MoveMode>()
    }

    #[test]
    fn help_lists_every_physics_command() {
        let mut app = console_fixture();
        execute_line(app.world_mut(), "Help");
        let text = scrollback(&app);
        for line in [
            "Help [text] - ",
            "ClearConsole - ",
            "ToggleCollision [tcl] - toggle noclip flight and walking (key V)",
            "ToggleCollisionGeometry [tcg] - toggle the collision outlines (also in Graphics settings)",
        ] {
            assert!(text.contains(line), "Help is missing {line}: {text}");
        }
        // The filter finds a command by its short form and by its description.
        execute_line(app.world_mut(), "ClearConsole");
        execute_line(app.world_mut(), "help TCG");
        let text = scrollback(&app);
        assert!(text.contains("ToggleCollisionGeometry [tcg]"), "{text}");
        assert!(!text.contains("ToggleCollision [tcl]"), "{text}");
        execute_line(app.world_mut(), "ClearConsole");
        execute_line(app.world_mut(), "help outlines");
        let text = scrollback(&app);
        assert!(text.contains("ToggleCollisionGeometry [tcg]"), "{text}");
        assert!(!text.contains("ToggleCollision [tcl]"), "{text}");
    }

    #[test]
    fn tcl_twice_flips_and_flips_back() {
        let mut app = console_fixture();
        assert_eq!(mode(&app), MoveMode::Noclip);
        execute_line(app.world_mut(), "tcl");
        assert_eq!(mode(&app), MoveMode::Walk, "{}", scrollback(&app));
        execute_line(app.world_mut(), "TCL");
        assert_eq!(mode(&app), MoveMode::Noclip);
        execute_line(app.world_mut(), "togglecollision");
        assert_eq!(mode(&app), MoveMode::Walk);
    }

    #[test]
    fn crossing_blocks_both_mode_switch_entry_points_in_both_modes() {
        for initial in [MoveMode::Noclip, MoveMode::Walk] {
            let mut app = console_fixture();
            app.insert_resource(initial);
            app.insert_resource(CursorCapture::Captured);
            app.insert_resource(crate::door_crossing::DoorCrossing::holding_for_test());
            press(&mut app, KeyCode::KeyV);
            app.world_mut().run_system_once(toggle_mode_system).unwrap();
            assert_eq!(mode(&app), initial, "V must preserve mode during crossing");
            execute_line(app.world_mut(), "tcl");
            assert_eq!(
                mode(&app),
                initial,
                "tcl must preserve mode during crossing"
            );
            assert!(scrollback(&app).contains("door crossing in progress"));
            app.insert_resource(crate::door_crossing::DoorCrossing::default());
            execute_line(app.world_mut(), "tcl");
            assert_ne!(mode(&app), initial, "switching resumes after crossing");
        }
    }

    #[test]
    fn failed_crossing_recovery_uses_noclip_from_both_modes_and_entry_points() {
        for initial in [MoveMode::Noclip, MoveMode::Walk] {
            for keyed in [false, true] {
                let mut app = console_fixture();
                app.insert_resource(initial);
                app.insert_resource(CursorCapture::Captured);
                app.insert_resource(crate::door_crossing::DoorCrossing::recovery_for_test());
                app.world_mut().resource_mut::<WalkSpawnHold>().active = true;
                if keyed {
                    press(&mut app, KeyCode::KeyV);
                    app.world_mut().run_system_once(toggle_mode_system).unwrap();
                } else {
                    execute_line(app.world_mut(), "tcl");
                    assert!(scrollback(&app).contains("noclip ON"));
                }
                assert_eq!(mode(&app), MoveMode::Noclip);
                assert!(!app.world().resource::<WalkSpawnHold>().active);
                assert!(
                    !app.world()
                        .resource::<crate::door_crossing::DoorCrossing>()
                        .is_active()
                );
                let mut players = app
                    .world_mut()
                    .query_filtered::<&RigidBodyDisabled, With<PlayerBody>>();
                assert_eq!(players.iter(app.world()).count(), 1);
                assert!(
                    app.world_mut()
                        .run_system_once(crate::door_crossing::no_door_crossing)
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn tcl_without_a_player_does_not_repeat_an_old_block_reason() {
        let mut app = console_fixture();
        app.world_mut()
            .resource_mut::<WalkEntryStatus>()
            .blocked_reason = Some("no free capsule placement nearby".to_owned());
        let players: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<PlayerBody>>()
            .iter(app.world())
            .collect();
        assert!(!players.is_empty(), "the fixture spawns a player");
        for player in players {
            app.world_mut().despawn(player);
        }
        execute_line(app.world_mut(), "tcl");
        let text = scrollback(&app);
        assert!(text.contains("no player to move in this run"), "{text}");
        assert!(!text.contains("cannot walk here"), "{text}");
        assert_eq!(mode(&app), MoveMode::Noclip);
    }

    #[test]
    fn key_and_command_agree_on_move_mode() {
        let mut keyed = console_fixture();
        keyed.insert_resource(CursorCapture::Captured);
        press(&mut keyed, KeyCode::KeyV);
        keyed
            .world_mut()
            .run_system_once(toggle_mode_system)
            .unwrap();
        let mut commanded = console_fixture();
        // The removed `v` alias is unknown; `tcl` runs the key's effect.
        execute_line(commanded.world_mut(), "v");
        assert!(scrollback(&commanded).contains("unknown command \"v\""));
        assert_eq!(mode(&commanded), MoveMode::Noclip);
        execute_line(commanded.world_mut(), "tcl");
        assert_eq!(mode(&keyed), MoveMode::Walk);
        assert_eq!(mode(&keyed), mode(&commanded));
        execute_line(commanded.world_mut(), "ToggleCollision");
        assert_eq!(mode(&commanded), MoveMode::Noclip);
    }

    #[test]
    fn command_toggles_collision_outlines() {
        let mut commanded = console_fixture();
        let before = commanded.world().resource::<DebugRenderContext>().enabled;
        // F3 releases the mouse; Skyrim's `tcg` controls collision outlines.
        execute_line(commanded.world_mut(), "f3");
        assert!(scrollback(&commanded).contains("unknown command \"f3\""));
        assert_eq!(
            commanded.world().resource::<DebugRenderContext>().enabled,
            before
        );
        execute_line(commanded.world_mut(), "tcg");
        assert_ne!(
            commanded.world().resource::<DebugRenderContext>().enabled,
            before
        );
        execute_line(commanded.world_mut(), "ToggleCollisionGeometry");
        assert_eq!(
            commanded.world().resource::<DebugRenderContext>().enabled,
            before
        );
    }

    #[test]
    fn tankard_and_grab_have_no_console_command() {
        let mut app = console_fixture();
        for line in ["tankard", "grab", "t", "e"] {
            execute_line(app.world_mut(), line);
        }
        let text = scrollback(&app);
        for line in ["tankard", "grab", "t", "e"] {
            assert!(
                text.contains(&format!("unknown command \"{line}\"")),
                "unknown missing for {line}: {text}"
            );
            assert!(
                !text.contains(&format!("unknown command \"{line}\", did you mean")),
                "unexpected suggestion for {line}: {text}"
            );
        }
    }

    #[test]
    fn open_console_blocks_the_debug_keys_look_and_recapture() {
        let mut app = console_fixture();
        // Open the console the way a player does, from a captured cursor.
        app.insert_resource(CursorCapture::Captured);
        send_key(&mut app, KeyCode::Backquote);
        app.update();
        assert!(app.world().resource::<ConsoleState>().open);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        let camera_before = {
            let mut query = app
                .world_mut()
                .query_filtered::<&Transform, With<ControlledCamera>>();
            *query.single(app.world()).unwrap()
        };
        // V, a movement key, a click and a mouse move while open.
        send_key(&mut app, KeyCode::KeyV);
        send_key(&mut app, KeyCode::KeyW);
        app.world_mut()
            .write_message(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state: bevy::input::ButtonState::Pressed,
                window: Entity::PLACEHOLDER,
            });
        app.world_mut().write_message(MouseMotion {
            delta: Vec2::new(40.0, 10.0),
        });
        app.update();
        assert_eq!(mode(&app), MoveMode::Noclip);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert_eq!(app.world().resource::<LookIntent>().yaw, 0.0);
        let mut query = app
            .world_mut()
            .query_filtered::<&Transform, With<ControlledCamera>>();
        assert_eq!(
            query.single(app.world()).unwrap().translation,
            camera_before.translation
        );
        // F3 is inert too.
        let before = app.world().resource::<DebugRenderContext>().enabled;
        send_key(&mut app, KeyCode::F3);
        app.update();
        assert_eq!(app.world().resource::<DebugRenderContext>().enabled, before);
    }

    /// Type a character into the console the way winit delivers it, then run one frame.
    fn type_text(app: &mut App, key_code: KeyCode, c: &str) {
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code,
                logical_key: bevy::input::keyboard::Key::Character(c.into()),
                state: bevy::input::ButtonState::Pressed,
                text: Some(c.into()),
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        app.update();
    }

    #[test]
    fn typed_tcl_runs_through_the_schedule() {
        // Drives the real path: the line is typed, Enter is pressed and the exclusive
        // dispatch system runs the handler, which runs the move-mode effect as a system.
        let mut app = console_fixture();
        app.insert_resource(CursorCapture::Captured);
        send_key(&mut app, KeyCode::Backquote);
        app.update();
        assert!(app.world().resource::<ConsoleState>().open);
        for (code, c) in [
            (KeyCode::KeyT, "t"),
            (KeyCode::KeyC, "c"),
            (KeyCode::KeyL, "l"),
        ] {
            type_text(&mut app, code, c);
        }
        assert_eq!(app.world().resource::<ConsoleState>().buffer, "tcl");
        assert_eq!(mode(&app), MoveMode::Noclip);
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code: KeyCode::Enter,
                logical_key: bevy::input::keyboard::Key::Enter,
                state: bevy::input::ButtonState::Pressed,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        app.update();
        app.update();
        assert_eq!(mode(&app), MoveMode::Walk, "{}", scrollback(&app));
        assert!(app.world().resource::<ConsoleState>().buffer.is_empty());
    }

    #[test]
    fn keys_still_work_through_the_schedule_with_the_console_closed() {
        let mut app = console_fixture();
        // The real schedule reads the capture state, so the cursor must be captured.
        app.insert_resource(CursorCapture::Captured);
        let before = app.world().resource::<DebugRenderContext>().enabled;
        send_key(&mut app, KeyCode::KeyV);
        app.update();
        assert_eq!(mode(&app), MoveMode::Walk, "{}", scrollback(&app));
        send_key(&mut app, KeyCode::F3);
        app.update();
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert_eq!(app.world().resource::<DebugRenderContext>().enabled, before);
    }

    fn send_state(app: &mut App, key_code: KeyCode, state: bevy::input::ButtonState) {
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code,
                logical_key: bevy::input::keyboard::Key::Character("x".into()),
                state,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        app.update();
    }

    #[test]
    fn keys_in_the_closing_frame_do_not_reach_the_game() {
        use bevy::input::ButtonState::{Pressed, Released};
        let mut app = console_fixture();
        app.insert_resource(CursorCapture::Captured);
        send_state(&mut app, KeyCode::Backquote, Pressed);
        send_state(&mut app, KeyCode::Backquote, Released);
        assert!(app.world().resource::<ConsoleState>().open);
        // V and the closing backtick land in the same update.
        send_key(&mut app, KeyCode::KeyV);
        send_key(&mut app, KeyCode::Backquote);
        // A selection click delivered with the closing key must not recapture the cursor.
        app.world_mut()
            .write_message(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state: bevy::input::ButtonState::Pressed,
                window: Entity::PLACEHOLDER,
            });
        app.update();
        assert!(!app.world().resource::<ConsoleState>().open);
        // The capture that movement and look read stays released for the closing frame, so they
        // ignore that frame's keys and mouse motion (this checks the gate, not a moving body).
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert_eq!(mode(&app), MoveMode::Noclip, "{}", scrollback(&app));
        // One update later the capture is back and V works again.
        send_state(&mut app, KeyCode::KeyV, Released);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        send_state(&mut app, KeyCode::Backquote, Released);
        send_state(&mut app, KeyCode::KeyV, Pressed);
        assert_eq!(mode(&app), MoveMode::Walk);
    }

    #[test]
    fn bug_report_capture_keeps_input_blocked_after_console_dismissal() {
        use bevy::input::ButtonState::{Pressed, Released};
        let mut app = console_fixture();
        app.insert_resource(CursorCapture::Captured);
        send_state(&mut app, KeyCode::Backquote, Pressed);
        send_state(&mut app, KeyCode::Backquote, Released);
        app.world_mut()
            .resource_mut::<ConsoleState>()
            .capture_suppressed = true;
        send_state(&mut app, KeyCode::Backquote, Pressed);
        send_state(&mut app, KeyCode::Backquote, Released);
        assert!(!app.world().resource::<ConsoleState>().open);
        assert!(!app.world().resource::<Time<Virtual>>().is_paused());
        let camera_before = {
            let mut query = app
                .world_mut()
                .query_filtered::<&Transform, With<ControlledCamera>>();
            *query.single(app.world()).unwrap()
        };
        send_key(&mut app, KeyCode::KeyW);
        send_key(&mut app, KeyCode::KeyV);
        app.world_mut()
            .write_message(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state: Pressed,
                window: Entity::PLACEHOLDER,
            });
        app.world_mut().write_message(MouseMotion {
            delta: Vec2::new(40.0, 10.0),
        });
        app.update();
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert_eq!(mode(&app), MoveMode::Noclip);
        let mut query = app
            .world_mut()
            .query_filtered::<&Transform, With<ControlledCamera>>();
        assert_eq!(*query.single(app.world()).unwrap(), camera_before);
        send_state(&mut app, KeyCode::KeyW, Released);
        send_state(&mut app, KeyCode::KeyV, Released);
        app.world_mut()
            .resource_mut::<ConsoleState>()
            .capture_suppressed = false;
        app.update();
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        assert!(!app.world().resource::<ConsoleState>().open);
    }

    #[test]
    fn opening_clears_held_motion() {
        let mut app = console_fixture();
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: 300.0,
            jump_pressed: true,
        });
        send_key(&mut app, KeyCode::Backquote);
        app.update();
        let intent = *app.world().resource::<WalkIntent>();
        assert_eq!(intent.wish_dir, Vec3::ZERO);
        assert_eq!(intent.target_speed, 0.0);
    }
}

#[cfg(test)]
mod teleport_tests {
    use super::*;

    #[test]
    fn noclip_flight_pauses_during_a_door_crossing_and_resumes_afterward() {
        let mut app = headless::fixture_app();
        app.insert_resource(MoveMode::Noclip);
        app.insert_resource(CursorCapture::Captured);
        app.insert_resource(crate::door_crossing::DoorCrossing::holding_for_test());
        let camera_position = |app: &mut App| {
            app.world_mut()
                .query_filtered::<&Transform, With<StreamingCamera>>()
                .single(app.world())
                .unwrap()
                .translation
        };
        let start = camera_position(&mut app);
        app.world_mut()
            .write_message(bevy::input::keyboard::KeyboardInput {
                key_code: KeyCode::KeyW,
                logical_key: bevy::input::keyboard::Key::Character("w".into()),
                state: bevy::input::ButtonState::Pressed,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        for _ in 0..360 {
            app.update();
        }
        assert_eq!(camera_position(&mut app), start);
        app.insert_resource(crate::door_crossing::DoorCrossing::default());
        for _ in 0..360 {
            app.update();
        }
        assert!((camera_position(&mut app) - start).length() > 4096.0);
    }

    /// A WALK body over nothing does not fall, or walk, while a door crossing holds the screen
    /// black, and falls again once the crossing is over.
    #[test]
    fn walking_pauses_during_a_door_crossing() {
        let mut app = headless::fixture_app();
        let start = Vec3::new(0.0, 5_000.0, 0.0);
        headless::place_player(&mut app, start);
        app.insert_resource(crate::door_crossing::DoorCrossing::holding_for_test());
        {
            let mut intent = app.world_mut().resource_mut::<WalkIntent>();
            intent.wish_dir = Vec3::NEG_Z;
            intent.target_speed = 300.0;
        }
        for _ in 0..30 {
            app.update();
        }
        let (held, _) = headless::player_pose(&mut app);
        assert!(
            (held - start).length() < 1.0,
            "moved during the crossing: {held}"
        );
        app.insert_resource(crate::door_crossing::DoorCrossing::default());
        for _ in 0..30 {
            app.update();
        }
        let (after, _) = headless::player_pose(&mut app);
        assert!(
            after.y < start.y - 10.0,
            "did not fall after the crossing: {after}"
        );
    }

    #[test]
    fn teleport_player_moves_the_body_and_zeroes_its_velocity() {
        let mut app = headless::fixture_app();
        headless::place_player(&mut app, Vec3::new(0.0, 500.0, 0.0));
        let mut query = app
            .world_mut()
            .query_filtered::<&mut WalkState, With<PlayerBody>>();
        {
            let mut state = query.single_mut(app.world_mut()).unwrap();
            state.vertical_velocity = -900.0;
            state.horizontal_velocity = Vec3::new(300.0, 0.0, 0.0);
            state.grounded = true;
        }
        let target = Vec3::new(40_000.0, 900.0, -2_000.0);
        app.world_mut().write_message(TeleportPlayer {
            position: target,
            sampled_ground_y: None,
            yaw: 1.0,
        });
        app.update();
        let state = *query.single(app.world()).unwrap();
        assert_eq!(state.vertical_velocity, 0.0);
        assert_eq!(state.horizontal_velocity, Vec3::ZERO);
        assert!(!state.grounded);
        let mut body = app
            .world_mut()
            .query_filtered::<&Transform, With<PlayerBody>>();
        let at = body.single(app.world()).unwrap().translation;
        // One physics tick of gravity may already have moved it; it is where it was sent.
        assert!((at.x - target.x).abs() < 1.0 && (at.z - target.z).abs() < 1.0);
        let tuning = MovementTuning::default();
        // The target is the feet: the body's origin is a capsule half-extent above them.
        assert!((at.y - (target.y + body_height_above_feet(&tuning))).abs() < 20.0);
        let look = app.world().resource::<LookIntent>();
        assert_eq!(look.yaw, 1.0);
    }

    #[test]
    fn teleport_puts_the_feet_at_the_target_and_the_camera_at_eye_height() {
        let tuning = MovementTuning::default();
        let feet = Vec3::new(10.0, 300.0, -20.0);
        let (body, camera) = body_and_camera_for_feet(&tuning, feet);
        // The capsule's lowest point is the feet.
        let lowest = body.y - tuning.capsule_half_height() - tuning.capsule_radius;
        assert!((lowest - feet.y).abs() < 1e-3);
        assert!((camera.y - body.y - tuning.eye_height).abs() < 1e-3);
        // Noclip moves only the camera, to the same place the walk follow would put it.
        let mut app = headless::fixture_app();
        app.insert_resource(MoveMode::Noclip);
        app.world_mut().write_message(TeleportPlayer {
            position: feet,
            sampled_ground_y: None,
            yaw: 0.0,
        });
        app.update();
        let mut view = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>();
        let at = view.single(app.world()).unwrap().translation;
        assert!((at - camera).length() < 1e-3);
    }
}
