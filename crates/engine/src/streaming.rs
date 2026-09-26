use crate::{
    config::EngineConfig,
    profiling::ProfilingState,
    render::{
        TerrainExtension, TerrainMaterial, WaterExtension, WaterMaterial, WaterReflectionTexture,
    },
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, FormId, InstanceBounds,
            MeshHandle, StreamedCellRoot, StreamingCamera, TerrainPatch, WaterSurface,
            WorldPosition, WorldTransform,
        },
        database::{AssetCatalog, CellKey, CellPayload, DatabaseRequest, WorldDatabase},
    },
};
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState, RenderAssetUsages},
    camera::primitives::MeshAabb,
    gltf::GltfExtras,
    image::{ImageFilterMode, ImageLoaderSettings, ImageSampler},
    math::Affine3A,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::time::Instant;

// Wall-clock spans can include a short OS scheduler preemption. Keep the raw maximum in metrics,
// but require a material overrun before classifying the frame as a commit-budget violation.
const COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS: u64 = 1_000;

fn commit_budget_exceeded(elapsed_micros: u64, budget_micros: u64) -> bool {
    elapsed_micros > budget_micros.saturating_add(COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS)
}

#[cfg(test)]
use bevy::mesh::VertexAttributeValues;

pub struct StreamingPlugin;

impl Plugin for StreamingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<DiagnosticFallbackAssets>()
            .init_resource::<TerrainContinuity>()
            .add_observer(mark_world_instance_ready)
            .add_systems(
                Update,
                (
                    plan_cells,
                    collect_cells,
                    track_asset_readiness,
                    track_surface_readiness,
                    update_render_origin,
                    validate_streaming_lifecycle,
                )
                    .chain(),
            );
    }
}

#[derive(Resource, Default)]
pub struct StreamingWorld {
    generation: u64,
    cells: HashMap<CellKey, CellStatus>,
}

#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct StreamingMetrics {
    pub requests_submitted: u64,
    pub responses_received: u64,
    pub stale_responses: u64,
    pub failed_cells: u64,
    pub unloaded_cells: u64,
    pub resident_cells: usize,
    pub loading_cells: usize,
    pub peak_resident_cells: usize,
    pub peak_loading_cells: usize,
    pub total_query_micros: u64,
    pub max_query_micros: u64,
    pub max_commit_micros: u64,
    pub total_frame_commit_micros: u64,
    pub max_frame_commit_micros: u64,
    pub commit_frames: u64,
    pub commit_budget_micros: u64,
    pub commit_budget_violations: u64,
    pub total_queue_wait_micros: u64,
    pub max_queue_wait_micros: u64,
    pub total_request_micros: u64,
    pub max_request_micros: u64,
    pub total_rows_loaded: u64,
    pub assets_ready: u64,
    pub asset_load_failures: u64,
    pub max_asset_ready_micros: u64,
    pub pending_asset_instances: usize,
    pub pending_surface_instances: usize,
    pub meshes_validated: u64,
    pub materials_validated: u64,
    pub images_validated: u64,
    pub material_validation_failures: u64,
    pub diagnostic_fallbacks: u64,
    pub canonical_fixture_validated: bool,
    pub terrain_patches_validated: u64,
    pub terrain_seams_validated: u64,
    /// Edge points of an arriving cell whose height had to move onto a resident neighbour's shared
    /// edge, so the two terrains meet exactly instead of the cell being rejected (see
    /// `validate_and_register_terrain_edges`).
    pub terrain_seam_points_welded: u64,
    /// Shared edges that differ past [`MAX_WELDABLE_EDGE_DELTA`] and are drawn as authored: a
    /// city's sculpted landscape meeting an unsculpted copy of the land around it, which Skyrim
    /// itself never stitches.
    pub terrain_edges_left_as_authored: u64,
    pub terrain_validation_failures: u64,
    pub water_surfaces_validated: u64,
    pub water_validation_failures: u64,
    pub terrain_water_fixture_validated: bool,
    pub transform_instances_validated: u64,
    pub transform_nodes_validated: u64,
    pub bounds_validated: u64,
    /// References whose converted model is an empty scene: a glTF scene with no node and no mesh,
    /// which is what the converter writes for a model whose NIF has no renderable geometry (an
    /// editor-marker-only model, for example). There is nothing to place, draw or bound, so such
    /// a reference is counted here rather than in [`Self::transform_bounds_validation_failures`],
    /// which is a hard gate and must count only real conversion defects. The tolerance stops
    /// there: a scene an exporter emptied by mistake looks exactly like one with nothing to
    /// export, so every empty scene is counted here, where the profiling reports can see it.
    pub empty_model_references: u64,
    pub transform_bounds_validation_failures: u64,
    pub transform_bounds_fixture_validated: bool,
    pub active_requests: usize,
    pub peak_active_requests: usize,
    pub resident_roots: usize,
    pub duplicate_cell_roots: u64,
    pub orphaned_cell_roots: u64,
    pub missing_cell_roots: u64,
    pub out_of_range_cell_roots: u64,
    pub streaming_invariant_failures: u64,
    pub origin_rebases: u64,
    pub streaming_fixture_validated: bool,
    pub streaming_fixture_failures: u64,
    pub asset_failures: Vec<AssetFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetFailure {
    pub model_path: String,
    pub reference_form_id: u32,
    pub base_form_id: u32,
    pub cell_id: u32,
    pub dependency_chain: Vec<String>,
}

#[derive(Resource, Default)]
struct DiagnosticFallbackAssets {
    mesh: Option<Handle<Mesh>>,
    material: Option<Handle<StandardMaterial>>,
}

#[derive(Resource, Default)]
struct TerrainContinuity {
    edges: HashMap<CellKey, TerrainEdges>,
}

#[derive(Clone)]
struct TerrainEdges {
    west: Vec<f32>,
    east: Vec<f32>,
    south: Vec<f32>,
    north: Vec<f32>,
}

impl TerrainEdges {
    /// The four edges of the heights `terrain` currently holds. Registering them after welding
    /// records what was actually drawn, so the next cell welds onto the same surface.
    fn of(terrain: &TerrainSnapshot) -> Self {
        let samples = |side: TerrainEdgeSide| {
            (0..side.len(terrain))
                .map(|position| terrain.heights[side.index(terrain, position)])
                .collect()
        };
        Self {
            west: samples(TerrainEdgeSide::West),
            east: samples(TerrainEdgeSide::East),
            south: samples(TerrainEdgeSide::South),
            north: samples(TerrainEdgeSide::North),
        }
    }

    /// The heights along `side` as they were registered.
    fn side(&self, side: TerrainEdgeSide) -> &[f32] {
        match side {
            TerrainEdgeSide::West => &self.west,
            TerrainEdgeSide::East => &self.east,
            TerrainEdgeSide::South => &self.south,
            TerrainEdgeSide::North => &self.north,
        }
    }
}

/// Which side of an exterior cell an edge belongs to, and which neighbour shares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerrainEdgeSide {
    West,
    East,
    South,
    North,
}

impl TerrainEdgeSide {
    /// Every side. This is also the order [`validate_and_register_terrain_edges`] welds them in,
    /// and for a corner the order in which its two neighbours are preferred.
    const ALL: [Self; 4] = [Self::West, Self::East, Self::South, Self::North];

    /// The side of the neighbour sharing this edge that lines up position for position with it.
    const fn opposite(self) -> Self {
        match self {
            Self::West => Self::East,
            Self::East => Self::West,
            Self::South => Self::North,
            Self::North => Self::South,
        }
    }

    /// The neighbour sharing this side in the same worldspace.
    fn neighbor_key(self, worldspace_id: u32, grid: IVec2) -> CellKey {
        let (grid_x, grid_y) = match self {
            Self::West => (grid.x - 1, grid.y),
            Self::East => (grid.x + 1, grid.y),
            Self::South => (grid.x, grid.y - 1),
            Self::North => (grid.x, grid.y + 1),
        };
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        }
    }

    /// How many height-field points lie along this side.
    fn len(self, terrain: &TerrainSnapshot) -> usize {
        match self {
            Self::West | Self::East => usize::from(terrain.height),
            Self::South | Self::North => usize::from(terrain.width),
        }
    }

    /// The height-field point at `position` along this side, numbered from the same end on both
    /// cells of a shared edge, so a cell's side lines up position for position with the
    /// neighbour's opposite side.
    fn index(self, terrain: &TerrainSnapshot, position: usize) -> usize {
        let width = usize::from(terrain.width);
        let height = usize::from(terrain.height);
        match self {
            Self::West => position * width,
            Self::East => position * width + width - 1,
            Self::South => position,
            Self::North => (height - 1) * width + position,
        }
    }
}

enum CellStatus {
    Loading { generation: u64 },
    Resident { root: Entity },
    Failed,
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct RenderOrigin(pub IVec2);

#[allow(clippy::too_many_arguments)]
fn plan_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut streaming: ResMut<StreamingWorld>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let plan_started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let global_x = camera.translation.x + origin.0.x as f32 * CELL_SIZE;
    let global_y = -camera.translation.z + origin.0.y as f32 * CELL_SIZE;
    let center = IVec2::new(
        (global_x / CELL_SIZE).floor() as i32,
        (global_y / CELL_SIZE).floor() as i32,
    );
    let mut wanted = HashSet::new();
    for y in -config.stream_radius..=config.stream_radius {
        for x in -config.stream_radius..=config.stream_radius {
            wanted.insert(CellKey::Exterior {
                worldspace_id: config.worldspace_id,
                grid_x: center.x + x,
                grid_y: center.y + y,
            });
        }
    }
    for key in &wanted {
        if !streaming.cells.contains_key(key) {
            streaming.generation = streaming.generation.wrapping_add(1);
            let generation = streaming.generation;
            if database
                .request(DatabaseRequest::Load {
                    generation,
                    key: *key,
                    queued_at: Instant::now(),
                })
                .is_ok()
            {
                metrics.requests_submitted += 1;
                profiler.increment("streaming/requests", 1);
                profiler.event(format!("{key:?}"), "requested", None);
                streaming
                    .cells
                    .insert(*key, CellStatus::Loading { generation });
            }
        }
    }
    streaming.cells.retain(|key, status| {
        let keep = cell_within_unload_radius(*key, center, config.unload_radius);
        if !keep {
            metrics.unloaded_cells += 1;
            continuity.edges.remove(key);
            profiler.event(format!("{key:?}"), "unloaded", None);
            if let CellStatus::Resident { root } = status {
                commands.entity(*root).despawn();
            }
        }
        keep
    });
    metrics.resident_cells = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Resident { .. }))
        .count();
    metrics.loading_cells = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Loading { .. }))
        .count();
    metrics.peak_resident_cells = metrics.peak_resident_cells.max(metrics.resident_cells);
    metrics.peak_loading_cells = metrics.peak_loading_cells.max(metrics.loading_cells);
    profiler.set_gauge("streaming/resident_cells", metrics.resident_cells as f64);
    profiler.set_gauge("streaming/loading_cells", metrics.loading_cells as f64);
    profiler.record_elapsed("streaming/plan_cells", plan_started);
}

#[allow(clippy::too_many_arguments)]
fn collect_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    cache: Res<CellCache>,
    origin: Res<RenderOrigin>,
    asset_server: Res<AssetServer>,
    catalog: Res<AssetCatalog>,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut streaming: ResMut<StreamingWorld>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let frame_commit_started = Instant::now();
    let mut commits_this_frame = 0u64;
    for _ in 0..config.max_cell_commits_per_frame {
        let Some(response) = database.try_response() else {
            break;
        };
        metrics.responses_received += 1;
        metrics.total_query_micros = metrics
            .total_query_micros
            .saturating_add(response.query_micros);
        metrics.max_query_micros = metrics.max_query_micros.max(response.query_micros);
        metrics.total_queue_wait_micros = metrics
            .total_queue_wait_micros
            .saturating_add(response.queue_wait_micros);
        metrics.max_queue_wait_micros = metrics
            .max_queue_wait_micros
            .max(response.queue_wait_micros);
        metrics.total_request_micros = metrics
            .total_request_micros
            .saturating_add(response.total_request_micros);
        metrics.max_request_micros = metrics
            .max_request_micros
            .max(response.total_request_micros);
        metrics.total_rows_loaded = metrics
            .total_rows_loaded
            .saturating_add(response.row_count as u64);
        profiler.record_micros("streaming/db_queue_wait", response.queue_wait_micros);
        profiler.record_micros("streaming/db_query", response.query_micros);
        profiler.record_micros("streaming/db_request_total", response.total_request_micros);
        let Some(CellStatus::Loading { generation }) = streaming.cells.get(&response.key) else {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_discarded", None);
            continue;
        };
        if *generation != response.generation {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_generation", None);
            continue;
        }
        let commit_started = std::time::Instant::now();
        match response.result {
            Ok(payload) => {
                let mut terrain = cache.terrain(payload.cell_id);
                if let Some(terrain) = &mut terrain {
                    let validation = validate_terrain_snapshot(terrain, &catalog).and_then(|()| {
                        validate_and_register_terrain_edges(
                            payload.key,
                            terrain,
                            &mut continuity,
                            &mut metrics,
                        )
                    });
                    if let Err(reason) = validation {
                        error!(cell = format_args!("{:08X}", payload.cell_id), %reason, "LAND failed strict validation");
                        metrics.failed_cells = metrics.failed_cells.saturating_add(1);
                        metrics.terrain_validation_failures =
                            metrics.terrain_validation_failures.saturating_add(1);
                        metrics.asset_failures.push(AssetFailure {
                            model_path: format!("terrain/{:08X}", payload.cell_id),
                            reference_form_id: 0,
                            base_form_id: 0,
                            cell_id: payload.cell_id,
                            dependency_chain: vec![reason],
                        });
                        profiler.increment("terrain/validation_failures", 1);
                        streaming.cells.insert(response.key, CellStatus::Failed);
                        continue;
                    }
                }
                let root = spawn_cell(
                    &mut commands,
                    &asset_server,
                    &catalog,
                    &reflection,
                    &mut meshes,
                    &mut terrain_materials,
                    &mut water_materials,
                    origin.0,
                    payload,
                    terrain,
                    &mut profiler,
                );
                streaming
                    .cells
                    .insert(response.key, CellStatus::Resident { root });
            }
            Err(error) => {
                debug!(?response.key, %error, "cell could not be streamed");
                streaming.cells.insert(response.key, CellStatus::Failed);
                metrics.failed_cells += 1;
                profiler.increment("streaming/failed_cells", 1);
            }
        }
        let commit_micros = commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.max_commit_micros = metrics.max_commit_micros.max(commit_micros);
        commits_this_frame = commits_this_frame.saturating_add(1);
        profiler.record_micros("streaming/cell_commit", commit_micros);
        profiler.event(
            format!("{:?}", response.key),
            "committed",
            Some(commit_micros as f64 / 1000.0),
        );
    }
    if commits_this_frame > 0 {
        let frame_micros = frame_commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.commit_frames = metrics.commit_frames.saturating_add(1);
        metrics.total_frame_commit_micros = metrics
            .total_frame_commit_micros
            .saturating_add(frame_micros);
        metrics.max_frame_commit_micros = metrics.max_frame_commit_micros.max(frame_micros);
        metrics.commit_budget_micros = config.max_commit_micros_per_frame;
        if commit_budget_exceeded(frame_micros, config.max_commit_micros_per_frame) {
            metrics.commit_budget_violations = metrics.commit_budget_violations.saturating_add(1);
            profiler.event(
                "streaming",
                "commit_budget_exceeded",
                Some(frame_micros as f64 / 1_000.0),
            );
        }
        profiler.set_gauge("streaming/commits_this_frame", commits_this_frame as f64);
        profiler.record_micros("streaming/frame_commit", frame_micros);
    }
}

fn cell_within_unload_radius(key: CellKey, center: IVec2, radius: i32) -> bool {
    match key {
        CellKey::Exterior { grid_x, grid_y, .. } => {
            (grid_x - center.x).abs() <= radius && (grid_y - center.y).abs() <= radius
        }
        CellKey::Interior(_) => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_cell(
    commands: &mut Commands,
    asset_server: &AssetServer,
    catalog: &AssetCatalog,
    reflection: &WaterReflectionTexture,
    meshes: &mut Assets<Mesh>,
    terrain_materials: &mut Assets<TerrainMaterial>,
    water_materials: &mut Assets<WaterMaterial>,
    origin: IVec2,
    payload: CellPayload,
    terrain: Option<TerrainSnapshot>,
    profiler: &mut ProfilingState,
) -> Entity {
    let spawn_started = Instant::now();
    let reference_count = payload.references.len();
    let root_translation = cell_translation(payload.key, origin);
    let mut root_commands = commands.spawn((
        Name::new(format!("Cell {:08X}", payload.cell_id)),
        CellRef(payload.cell_id),
        StreamedCellRoot,
        Transform::from_translation(root_translation),
        Visibility::default(),
    ));
    if let CellKey::Exterior { grid_x, grid_y, .. } = payload.key {
        root_commands.insert(ExteriorCellGrid(IVec2::new(grid_x, grid_y)));
    }
    let root = root_commands.id();
    commands.entity(root).with_children(|parent| {
        if let Some(terrain) = terrain {
            for quadrant in 0..4 {
                let started = Instant::now();
                let mesh = build_terrain_quadrant_mesh(&terrain, quadrant)
                    .expect("validated terrain must build");
                profiler.record_elapsed("streaming/terrain_mesh", started);
                let (extension, images) =
                    TerrainExtension::from_quadrant(&terrain, quadrant, catalog, asset_server)
                        .expect("validated terrain material must build");
                let material = terrain_materials.add(TerrainMaterial {
                    base: StandardMaterial {
                        base_color: Color::WHITE,
                        perceptual_roughness: 0.92,
                        cull_mode: None,
                        double_sided: true,
                        ..default()
                    },
                    extension,
                });
                parent.spawn((
                    Name::new(format!("Terrain quadrant {quadrant}")),
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::default(),
                    TerrainPatch,
                    Visibility::Hidden,
                    PendingTerrainProfile {
                        cell_id: terrain.cell_id,
                        quadrant,
                        images,
                    },
                ));
            }
            if let Some(height) = terrain
                .water_height
                .filter(|height| height.is_finite() && height.abs() < 1.0e7)
            {
                let water_mesh = meshes.add(Plane3d::default().mesh().size(CELL_SIZE, CELL_SIZE));
                let flow_normal = terrain
                    .water_type_form_id
                    .and_then(|form_id| catalog.water_flow(form_id))
                    .map(|path| {
                        asset_server
                            .load_builder()
                            .with_settings(|settings: &mut ImageLoaderSettings| {
                                settings.is_srgb = false;
                            })
                            .load(path.to_owned())
                    });
                let water_material = water_materials.add(WaterMaterial {
                    base: StandardMaterial {
                        base_color: Color::srgba(0.05, 0.2, 0.32, 0.68),
                        metallic: 0.15,
                        perceptual_roughness: 0.06,
                        reflectance: 0.9,
                        alpha_mode: AlphaMode::Blend,
                        ..default()
                    },
                    extension: WaterExtension::with_reflection(
                        reflection.0.clone(),
                        flow_normal.clone(),
                    ),
                });
                parent.spawn((
                    Name::new("Water"),
                    Mesh3d(water_mesh),
                    MeshMaterial3d(water_material),
                    Transform::from_translation(Vec3::new(
                        CELL_SIZE * 0.5,
                        height,
                        -CELL_SIZE * 0.5,
                    )),
                    WaterSurface,
                    Visibility::Hidden,
                    PendingWaterProfile {
                        cell_id: terrain.cell_id,
                        flow_normal,
                    },
                    bevy::camera::visibility::RenderLayers::layer(1),
                ));
            }
        }
        for reference in payload.references {
            let creation_position = Vec3::from_array(reference.position);
            let world_position = WorldPosition::from_creation_units(creation_position);
            let translation = match payload.key {
                CellKey::Exterior { grid_x, grid_y, .. } => {
                    let cell_origin = IVec2::new(grid_x, grid_y);
                    creation_to_bevy(world_position.relative_to(cell_origin))
                }
                CellKey::Interior(_) => creation_to_bevy(creation_position),
            };
            let rotation = creation_rotation_to_bevy(reference.rotation);
            let transform = Transform::from_translation(translation)
                .with_rotation(rotation)
                .with_scale(Vec3::splat(reference.scale));
            let model_bounds = reference.bounds_valid.then(|| {
                ExpectedModelBounds::new(
                    Vec3::from_array(reference.bounds_min),
                    Vec3::from_array(reference.bounds_max),
                )
            });
            let model_bounds = model_bounds.flatten();
            let bounds = model_bounds.map(|bounds| {
                InstanceBounds::transformed(bounds.min, bounds.max, transform.to_matrix())
            });
            let mut entity = parent.spawn((
                Name::new(format!("Reference {:08X}", reference.form_id)),
                FormId(reference.form_id),
                CellRef(reference.cell_id),
                world_position,
                WorldTransform(transform.to_matrix()),
                transform,
            ));
            if let Some(bounds) = bounds.zip(model_bounds) {
                entity.insert(bounds);
            }
            if let Some(path) = reference.model_path.and_then(converted_model_path) {
                entity.insert((
                    MeshHandle(path.clone()),
                    WorldAssetRoot(
                        asset_server.load(GltfAssetLabel::Scene(0).from_asset(path.clone())),
                    ),
                    PendingAssetProfile {
                        started: Instant::now(),
                        scene_spawned: false,
                        path,
                        form_id: reference.form_id,
                        base_form_id: reference.base_form_id,
                        cell_id: reference.cell_id,
                    },
                ));
            }
        }
    });
    profiler.increment("streaming/references_spawned", reference_count as u64);
    profiler.record_elapsed("streaming/spawn_cell", spawn_started);
    root
}

#[derive(Component)]
struct PendingAssetProfile {
    started: Instant,
    scene_spawned: bool,
    path: String,
    form_id: u32,
    base_form_id: u32,
    cell_id: u32,
}

#[derive(Component)]
struct PendingTerrainProfile {
    cell_id: u32,
    quadrant: u8,
    images: Vec<Handle<Image>>,
}

#[derive(Component)]
struct PendingWaterProfile {
    cell_id: u32,
    flow_normal: Option<Handle<Image>>,
}

type RenderPrimitiveQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static Mesh3d,
        Option<&'static MeshMaterial3d<StandardMaterial>>,
        Option<&'static GltfExtras>,
    ),
>;

type PendingAssetQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static WorldAssetRoot,
        &'static PendingAssetProfile,
        &'static Transform,
        &'static GlobalTransform,
        &'static WorldTransform,
        Option<&'static ExpectedModelBounds>,
    ),
>;

fn mark_world_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingAssetProfile>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn track_asset_readiness(
    mut commands: Commands,
    config: Res<EngineConfig>,
    asset_server: Res<AssetServer>,
    pending: PendingAssetQuery,
    children: Query<&Children>,
    primitives: RenderPrimitiveQuery,
    transforms: Query<(&Transform, &GlobalTransform)>,
    images: Res<Assets<Image>>,
    world_assets: Res<Assets<WorldAsset>>,
    mut fallback_assets: ResMut<DiagnosticFallbackAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    metrics.pending_asset_instances = pending.iter().count();
    let mut completed_this_scan = 0usize;
    for (entity, root, pending, local, global, world_transform, expected_bounds) in &pending {
        let load_failure =
            asset_server
                .get_load_states(root.0.id())
                .and_then(|(load, _, recursive)| match (load, recursive) {
                    (LoadState::Failed(error), _) => Some(error),
                    (_, RecursiveDependencyLoadState::Failed(error)) => Some(error),
                    _ => None,
                });
        if let Some(error) = load_failure {
            let chain = error_chain(error.as_ref());
            record_asset_failure(&mut metrics, &mut profiler, pending, chain, false);
            hide_partial_scene(&mut commands, entity, &children);
            if config.diagnostic_asset_fallbacks {
                spawn_diagnostic_fallback(
                    &mut commands,
                    entity,
                    &mut fallback_assets,
                    &mut meshes,
                    &mut materials,
                );
                metrics.diagnostic_fallbacks = metrics.diagnostic_fallbacks.saturating_add(1);
            }
            commands.entity(entity).remove::<PendingAssetProfile>();
            completed_this_scan += 1;
        } else if pending.scene_spawned && asset_server.is_loaded_with_dependencies(root.0.id()) {
            let transform_validation = validate_spawned_transforms_and_bounds(
                entity,
                local,
                global,
                world_transform,
                expected_bounds,
                world_assets.get(&root.0),
                &children,
                &transforms,
                &primitives,
                &meshes,
            );
            let transform_summary = match transform_validation {
                Ok(summary) => summary,
                Err(reason) => {
                    metrics.transform_bounds_validation_failures = metrics
                        .transform_bounds_validation_failures
                        .saturating_add(1);
                    profiler.increment("transforms/validation_failures", 1);
                    record_asset_failure(&mut metrics, &mut profiler, pending, vec![reason], false);
                    hide_partial_scene(&mut commands, entity, &children);
                    if config.diagnostic_asset_fallbacks {
                        spawn_diagnostic_fallback(
                            &mut commands,
                            entity,
                            &mut fallback_assets,
                            &mut meshes,
                            &mut materials,
                        );
                        metrics.diagnostic_fallbacks =
                            metrics.diagnostic_fallbacks.saturating_add(1);
                    }
                    commands.entity(entity).remove::<PendingAssetProfile>();
                    completed_this_scan += 1;
                    continue;
                }
            };
            // An empty converted model has nothing to place, draw or validate, so the reference is
            // skipped and counted on its own instead of failing the run's bounds gate. Everything
            // else about the reference stays: it keeps its transform, and its scene is left alone
            // (it is empty; there is nothing in it to hide).
            if transform_summary.empty_model {
                metrics.empty_model_references = metrics.empty_model_references.saturating_add(1);
                profiler.increment("assets/empty_model_references", 1);
                profiler.event(&pending.path, "asset_empty", None);
                commands.entity(entity).remove::<PendingAssetProfile>();
                completed_this_scan += 1;
                continue;
            }
            let validation = validate_spawned_asset(
                entity,
                &children,
                &primitives,
                &meshes,
                &materials,
                &images,
            );
            let summary = match validation {
                Ok(summary) => summary,
                Err(reason) => {
                    record_asset_failure(&mut metrics, &mut profiler, pending, vec![reason], true);
                    hide_partial_scene(&mut commands, entity, &children);
                    if config.diagnostic_asset_fallbacks {
                        spawn_diagnostic_fallback(
                            &mut commands,
                            entity,
                            &mut fallback_assets,
                            &mut meshes,
                            &mut materials,
                        );
                        metrics.diagnostic_fallbacks =
                            metrics.diagnostic_fallbacks.saturating_add(1);
                    }
                    commands.entity(entity).remove::<PendingAssetProfile>();
                    completed_this_scan += 1;
                    continue;
                }
            };
            let micros = pending
                .started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            metrics.assets_ready = metrics.assets_ready.saturating_add(1);
            metrics.meshes_validated = metrics
                .meshes_validated
                .saturating_add(summary.meshes as u64);
            metrics.materials_validated = metrics
                .materials_validated
                .saturating_add(summary.materials as u64);
            metrics.images_validated = metrics
                .images_validated
                .saturating_add(summary.images as u64);
            metrics.transform_instances_validated =
                metrics.transform_instances_validated.saturating_add(1);
            metrics.transform_nodes_validated = metrics
                .transform_nodes_validated
                .saturating_add(transform_summary.nodes as u64);
            metrics.bounds_validated = metrics.bounds_validated.saturating_add(1);
            metrics.max_asset_ready_micros = metrics.max_asset_ready_micros.max(micros);
            profiler.record_micros("assets/model_ready", micros);
            profiler.event(&pending.path, "asset_ready", Some(micros as f64 / 1000.0));
            commands.entity(entity).remove::<PendingAssetProfile>();
            completed_this_scan += 1;
        }
    }
    metrics.pending_asset_instances = metrics
        .pending_asset_instances
        .saturating_sub(completed_this_scan);
    profiler.set_gauge(
        "assets/pending_instances",
        metrics.pending_asset_instances as f64,
    );
    profiler.record_elapsed("assets/readiness_scan", started);
}

fn track_surface_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    images: Res<Assets<Image>>,
    terrain: Query<(Entity, &PendingTerrainProfile)>,
    water: Query<(Entity, &PendingWaterProfile)>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    metrics.pending_surface_instances = terrain.iter().count() + water.iter().count();
    let mut completed = 0usize;
    for (entity, pending) in &terrain {
        match validate_surface_dependencies(&asset_server, &images, &pending.images, true) {
            SurfaceDependencyState::Pending => {}
            SurfaceDependencyState::Ready => {
                metrics.terrain_patches_validated =
                    metrics.terrain_patches_validated.saturating_add(1);
                metrics.materials_validated = metrics.materials_validated.saturating_add(1);
                metrics.images_validated = metrics
                    .images_validated
                    .saturating_add(pending.images.len() as u64);
                profiler.increment("terrain/patches_validated", 1);
                commands.entity(entity).insert(Visibility::Inherited);
                commands.entity(entity).remove::<PendingTerrainProfile>();
                completed += 1;
            }
            SurfaceDependencyState::Failed(reason) => {
                metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
                metrics.terrain_validation_failures =
                    metrics.terrain_validation_failures.saturating_add(1);
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!(
                        "terrain/{:08X}/quadrant-{}",
                        pending.cell_id, pending.quadrant
                    ),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: pending.cell_id,
                    dependency_chain: vec![reason],
                });
                profiler.increment("terrain/validation_failures", 1);
                commands.entity(entity).insert(Visibility::Hidden);
                commands.entity(entity).remove::<PendingTerrainProfile>();
                completed += 1;
            }
        }
    }
    for (entity, pending) in &water {
        let handles: Vec<_> = pending.flow_normal.iter().cloned().collect();
        match validate_surface_dependencies(&asset_server, &images, &handles, false) {
            SurfaceDependencyState::Pending => {}
            SurfaceDependencyState::Ready => {
                metrics.water_surfaces_validated =
                    metrics.water_surfaces_validated.saturating_add(1);
                metrics.materials_validated = metrics.materials_validated.saturating_add(1);
                metrics.images_validated = metrics
                    .images_validated
                    .saturating_add(handles.len() as u64);
                profiler.increment("water/surfaces_validated", 1);
                commands.entity(entity).insert(Visibility::Inherited);
                commands.entity(entity).remove::<PendingWaterProfile>();
                completed += 1;
            }
            SurfaceDependencyState::Failed(reason) => {
                metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
                metrics.water_validation_failures =
                    metrics.water_validation_failures.saturating_add(1);
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!("water/{:08X}", pending.cell_id),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: pending.cell_id,
                    dependency_chain: vec![reason],
                });
                profiler.increment("water/validation_failures", 1);
                commands.entity(entity).insert(Visibility::Hidden);
                commands.entity(entity).remove::<PendingWaterProfile>();
                completed += 1;
            }
        }
    }
    metrics.pending_surface_instances = metrics.pending_surface_instances.saturating_sub(completed);
    profiler.set_gauge(
        "assets/pending_surface_instances",
        metrics.pending_surface_instances as f64,
    );
}

enum SurfaceDependencyState {
    Pending,
    Ready,
    Failed(String),
}

fn validate_surface_dependencies(
    asset_server: &AssetServer,
    images: &Assets<Image>,
    handles: &[Handle<Image>],
    expects_srgb: bool,
) -> SurfaceDependencyState {
    for handle in handles {
        if let Some((load, _, recursive)) = asset_server.get_load_states(handle.id()) {
            let failed = match (load, recursive) {
                (LoadState::Failed(error), _) => Some(error),
                (_, RecursiveDependencyLoadState::Failed(error)) => Some(error),
                _ => None,
            };
            if let Some(error) = failed {
                return SurfaceDependencyState::Failed(error_chain(error.as_ref()).join(" -> "));
            }
        }
        if !asset_server.is_loaded_with_dependencies(handle.id()) {
            return SurfaceDependencyState::Pending;
        }
        let Some(image) = images.get(handle) else {
            return SurfaceDependencyState::Pending;
        };
        if image.texture_descriptor.format.is_srgb() != expects_srgb {
            return SurfaceDependencyState::Failed(format!(
                "image {:?} has wrong color space {:?}",
                handle.id(),
                image.texture_descriptor.format
            ));
        }
        if let Err(reason) = validate_image_sampler("surface", &image.sampler) {
            return SurfaceDependencyState::Failed(reason);
        }
    }
    SurfaceDependencyState::Ready
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AssetValidationSummary {
    meshes: usize,
    materials: usize,
    images: usize,
    excluded_materials: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TransformValidationSummary {
    nodes: usize,
    /// The reference's converted model is an empty scene, so it was counted in
    /// [`StreamingMetrics::empty_model_references`] rather than as a validated instance: there
    /// were no converted bounds to compare against and nothing to draw.
    empty_model: bool,
}

#[allow(clippy::too_many_arguments)]
fn validate_spawned_transforms_and_bounds(
    root: Entity,
    root_local: &Transform,
    root_global: &GlobalTransform,
    world_transform: &WorldTransform,
    expected: Option<&ExpectedModelBounds>,
    converted_model: Option<&WorldAsset>,
    children: &Query<&Children>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
) -> Result<TransformValidationSummary, String> {
    validate_transform("reference", root_local, root_global)?;
    let local_matrix = root_local.to_matrix();
    if matrix_max_difference(local_matrix, world_transform.0) > 1.0e-4 {
        return Err("WorldTransform differs from the spawned reference Transform".to_owned());
    }
    let Some(expected) = expected else {
        // A model the converter wrote no aggregate bounds for is an empty scene: a model with no
        // renderable geometry, so there is nothing to place, draw or bound. What makes it empty is
        // read from the converted model itself - the scene the converter wrote, which declares no
        // node and no mesh - rather than from the spawned instance, so a hierarchy that has not
        // spawned yet cannot pass as an empty model. A scene that does declare a node or a mesh
        // and still arrived without aggregate bounds is a conversion defect, and stays as fatal as
        // any other.
        let scene = converted_model.ok_or_else(|| {
            "the converted model is not loaded while validating its bounds; reconvert the asset"
                .to_owned()
        })?;
        let contents = converted_scene_contents(scene);
        return if contents.is_empty() {
            Ok(TransformValidationSummary {
                nodes: 0,
                empty_model: true,
            })
        } else {
            Err(format!(
                "converted model has no validated aggregate bounds; reconvert the asset (its converted scene is not empty: {} mesh primitives, {} nodes)",
                contents.meshes, contents.nodes
            ))
        };
    };
    ExpectedModelBounds::new(expected.min, expected.max)
        .ok_or_else(|| "converted model bounds are non-finite, empty, or inverted".to_owned())?;

    let mut actual_min = Vec3::splat(f32::INFINITY);
    let mut actual_max = Vec3::splat(f32::NEG_INFINITY);
    let mut nodes = 0usize;
    let mut bounded_meshes = 0usize;
    if let Ok(direct_children) = children.get(root) {
        for child in direct_children.iter() {
            accumulate_relative_bounds(
                child,
                Affine3A::IDENTITY,
                children,
                transforms,
                primitives,
                meshes,
                &mut nodes,
                &mut bounded_meshes,
                &mut actual_min,
                &mut actual_max,
            )?;
        }
    }
    if bounded_meshes == 0 {
        return Err("spawned hierarchy contains no bounded mesh".to_owned());
    }
    let extent = (expected.max - expected.min).abs().max_element().max(1.0);
    let tolerance = (extent * 1.0e-4).max(1.0e-3);
    let error = (actual_min - expected.min)
        .abs()
        .max((actual_max - expected.max).abs())
        .max_element();
    if !error.is_finite() || error > tolerance {
        return Err(format!(
            "spawned hierarchy bounds diverge from conversion: expected {:?}..{:?}, actual {:?}..{:?}, tolerance {tolerance}",
            expected.min, expected.max, actual_min, actual_max
        ));
    }
    Ok(TransformValidationSummary {
        nodes,
        empty_model: false,
    })
}

/// What a converted model holds: the scene the converter wrote for it, as the asset loader built
/// it, with the model's own node and mesh count. A model with no renderable geometry converts to
/// an empty scene, and the loader still gives that scene its own root entity, so emptiness is "no
/// mesh and no node", not "no entity".
#[derive(Debug, Default, PartialEq, Eq)]
struct ConvertedSceneContents {
    /// One per glTF primitive the converter exported: an entity carrying a [`Mesh3d`].
    meshes: usize,
    /// Every entity the loader attached below another one, which in a glTF scene is every node;
    /// the scene's own root is the only entity without a parent.
    nodes: usize,
}

impl ConvertedSceneContents {
    /// The converter's empty scene, which is what a model with no renderable geometry converts
    /// to: no node and no mesh to place, draw or bound.
    fn is_empty(&self) -> bool {
        self.meshes == 0 && self.nodes == 0
    }
}

/// Counts what a converted model holds. Taken from the loaded asset rather than from its spawned
/// instance, so what is read is the whole converted file: a scene whose entities have not spawned
/// yet, or one whose geometry an exporter dropped, cannot pass as an empty model.
fn converted_scene_contents(scene: &WorldAsset) -> ConvertedSceneContents {
    let mut contents = ConvertedSceneContents::default();
    for entity in scene.world.iter_entities() {
        contents.meshes += usize::from(entity.contains::<Mesh3d>());
        contents.nodes += usize::from(entity.contains::<ChildOf>());
    }
    contents
}

/// Walks the spawned hierarchy under a root, accumulating each descendant's transform
/// relative to the root by composing local `Transform`s along the path from the root.
///
/// This deliberately never forms the root's or a descendant's absolute `GlobalTransform`
/// matrix: at real-world placements (tens of thousands of units from the origin) building
/// that large-magnitude matrix and then multiplying by its inverse cancels lossily in f32,
/// losing more precision than the bounds-check tolerance allows for small models. Composing
/// only the local, mesh-scale transforms keeps every intermediate value small and exact
/// enough for the tolerance.
#[allow(clippy::too_many_arguments)]
fn accumulate_relative_bounds(
    entity: Entity,
    relative_to_root: Affine3A,
    children: &Query<&Children>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
    nodes: &mut usize,
    bounded_meshes: &mut usize,
    actual_min: &mut Vec3,
    actual_max: &mut Vec3,
) -> Result<(), String> {
    // An explicit stack, not recursion: a deeply nested model must not overflow the thread's
    // stack. Children are pushed in reverse so they are visited in order, as before.
    let mut stack = vec![(entity, relative_to_root)];
    while let Some((entity, parent_to_root)) = stack.pop() {
        let (local, global) = transforms
            .get(entity)
            .map_err(|_| format!("hierarchy node {entity:?} has no local/global transform"))?;
        validate_transform(&format!("hierarchy node {entity:?}"), local, global)?;
        *nodes += 1;
        let relative_to_root = parent_to_root * local.compute_affine();
        if let Ok((mesh_handle, _, _)) = primitives.get(entity) {
            let mesh = meshes.get(mesh_handle).ok_or_else(|| {
                format!(
                    "mesh {:?} is absent while validating bounds",
                    mesh_handle.id()
                )
            })?;
            let aabb = mesh.compute_aabb().ok_or_else(|| {
                format!("mesh {:?} has no finite POSITION bounds", mesh_handle.id())
            })?;
            let center = Vec3::from(aabb.center);
            let half_extents = Vec3::from(aabb.half_extents);
            let transformed = InstanceBounds::transformed(
                center - half_extents,
                center + half_extents,
                Mat4::from(relative_to_root),
            );
            *actual_min = actual_min.min(transformed.min);
            *actual_max = actual_max.max(transformed.max);
            *bounded_meshes += 1;
        }
        if let Ok(kids) = children.get(entity) {
            stack.extend(kids.iter().rev().map(|child| (child, relative_to_root)));
        }
    }
    Ok(())
}

fn validate_transform(
    label: &str,
    local: &Transform,
    global: &GlobalTransform,
) -> Result<(), String> {
    let local_matrix = local.to_matrix();
    let global_matrix = global.to_matrix();
    if !local_matrix.is_finite() || !global_matrix.is_finite() {
        return Err(format!("{label} contains a non-finite transform"));
    }
    if local.scale.abs().min_element() <= 1.0e-6
        || local_matrix.determinant().abs() <= 1.0e-8
        || global_matrix.determinant().abs() <= 1.0e-8
    {
        return Err(format!(
            "{label} contains a singular scale or hierarchy transform"
        ));
    }
    let rotation_length = local.rotation.length();
    if !rotation_length.is_finite() || (rotation_length - 1.0).abs() > 1.0e-3 {
        return Err(format!("{label} contains a non-normalized rotation"));
    }
    Ok(())
}

fn matrix_max_difference(left: Mat4, right: Mat4) -> f32 {
    left.to_cols_array()
        .into_iter()
        .zip(right.to_cols_array())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f32::max)
}

fn validate_spawned_asset(
    root: Entity,
    children: &Query<&Children>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
    materials: &Assets<StandardMaterial>,
    images: &Assets<Image>,
) -> Result<AssetValidationSummary, String> {
    let mut summary = AssetValidationSummary::default();
    for descendant in children.iter_descendants(root) {
        let Ok((mesh, material_handle, extras)) = primitives.get(descendant) else {
            continue;
        };
        if meshes.get(mesh).is_none() {
            return Err(format!(
                "mesh {:?} is absent after scene readiness",
                mesh.id()
            ));
        }
        summary.meshes += 1;
        let Some(material_handle) = material_handle else {
            if extras.is_some_and(has_explicit_material_exclusion) {
                summary.excluded_materials += 1;
                continue;
            }
            return Err(format!(
                "mesh entity {descendant:?} has no loaded material or explicit exclusion"
            ));
        };
        let material = materials.get(material_handle).ok_or_else(|| {
            format!(
                "material {:?} is absent after scene readiness",
                material_handle.id()
            )
        })?;
        summary.images += validate_standard_material(material, images)?;
        summary.materials += 1;
    }
    Ok(summary)
}

fn has_explicit_material_exclusion(extras: &GltfExtras) -> bool {
    serde_json::from_str::<serde_json::Value>(&extras.value)
        .ok()
        .and_then(|value| {
            value
                .pointer("/openSkyrim/materialExclusion")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some()
}

pub(crate) fn validate_standard_material(
    material: &StandardMaterial,
    images: &Assets<Image>,
) -> Result<usize, String> {
    match material.alpha_mode {
        AlphaMode::Opaque | AlphaMode::Blend => {}
        AlphaMode::Mask(cutoff) if cutoff.is_finite() && (0.0..=1.0).contains(&cutoff) => {}
        AlphaMode::Mask(cutoff) => return Err(format!("invalid alpha cutoff {cutoff}")),
        mode => return Err(format!("unsupported Skyrim material alpha mode {mode:?}")),
    }
    if material.double_sided != material.cull_mode.is_none() {
        return Err(format!(
            "inconsistent culling: double_sided={} cull_mode={:?}",
            material.double_sided, material.cull_mode
        ));
    }
    let emissive = material.emissive;
    if ![emissive.red, emissive.green, emissive.blue, emissive.alpha]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err("emissive contains a non-finite or negative channel".to_owned());
    }

    let slots = [
        ("base_color", material.base_color_texture.as_ref(), true),
        ("emissive", material.emissive_texture.as_ref(), true),
        (
            "metallic_roughness",
            material.metallic_roughness_texture.as_ref(),
            false,
        ),
        ("normal", material.normal_map_texture.as_ref(), false),
        ("occlusion", material.occlusion_texture.as_ref(), false),
        ("specular", material.specular_texture.as_ref(), false),
        (
            "specular_tint",
            material.specular_tint_texture.as_ref(),
            true,
        ),
    ];
    let mut validated = 0usize;
    for (slot, handle, expects_srgb) in slots {
        let Some(handle) = handle else {
            continue;
        };
        let image = images
            .get(handle)
            .ok_or_else(|| format!("{slot} image {:?} is not loaded", handle.id()))?;
        let descriptor = &image.texture_descriptor;
        if descriptor.size.width == 0
            || descriptor.size.height == 0
            || descriptor.mip_level_count == 0
        {
            return Err(format!("{slot} image has invalid dimensions or mip levels"));
        }
        if descriptor.format.is_srgb() != expects_srgb {
            return Err(format!(
                "{slot} image color space mismatch: {:?}",
                descriptor.format
            ));
        }
        validate_image_sampler(slot, &image.sampler)?;
        validated += 1;
    }
    Ok(validated)
}

fn validate_image_sampler(slot: &str, sampler: &ImageSampler) -> Result<(), String> {
    let ImageSampler::Descriptor(descriptor) = sampler else {
        return Ok(());
    };
    if descriptor.anisotropy_clamp == 0
        || !descriptor.lod_min_clamp.is_finite()
        || !descriptor.lod_max_clamp.is_finite()
        || descriptor.lod_min_clamp > descriptor.lod_max_clamp
    {
        return Err(format!("{slot} image has an invalid sampler descriptor"));
    }
    if descriptor.anisotropy_clamp > 1
        && (descriptor.mag_filter != ImageFilterMode::Linear
            || descriptor.min_filter != ImageFilterMode::Linear
            || descriptor.mipmap_filter != ImageFilterMode::Linear)
    {
        return Err(format!(
            "{slot} image requests anisotropy without linear filtering"
        ));
    }
    Ok(())
}

fn error_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        chain.push(error.to_string());
        current = error.source();
    }
    chain
}

fn record_asset_failure(
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
    pending: &PendingAssetProfile,
    dependency_chain: Vec<String>,
    material_validation: bool,
) {
    metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
    if material_validation {
        metrics.material_validation_failures =
            metrics.material_validation_failures.saturating_add(1);
    }
    profiler.increment("assets/load_failures", 1);
    profiler.event(&pending.path, "asset_failed", None);
    let mut full_chain = vec![
        format!("REFR {:08X}", pending.form_id),
        format!("base record {:08X}", pending.base_form_id),
        pending.path.clone(),
    ];
    full_chain.extend(dependency_chain);
    error!(
        reference = format_args!("{:08X}", pending.form_id),
        base = format_args!("{:08X}", pending.base_form_id),
        cell = format_args!("{:08X}", pending.cell_id),
        path = %pending.path,
        chain = ?full_chain,
        "model, material, or image dependency failed strict validation"
    );
    metrics.asset_failures.push(AssetFailure {
        model_path: pending.path.clone(),
        reference_form_id: pending.form_id,
        base_form_id: pending.base_form_id,
        cell_id: pending.cell_id,
        dependency_chain: full_chain,
    });
}

fn hide_partial_scene(commands: &mut Commands, root: Entity, children: &Query<&Children>) {
    for descendant in children.iter_descendants(root) {
        commands.entity(descendant).insert(Visibility::Hidden);
    }
}

fn spawn_diagnostic_fallback(
    commands: &mut Commands,
    root: Entity,
    fallback: &mut DiagnosticFallbackAssets,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let mesh = fallback
        .mesh
        .get_or_insert_with(|| meshes.add(Cuboid::new(96.0, 96.0, 96.0)))
        .clone();
    let material = fallback
        .material
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color: Color::srgb(1.0, 0.0, 0.8),
                emissive: LinearRgba::new(8.0, 0.0, 5.0, 1.0),
                unlit: true,
                ..default()
            })
        })
        .clone();
    commands.entity(root).with_child((
        Name::new("DIAGNOSTIC ASSET FAILURE"),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::default(),
    ));
}

fn cell_translation(key: CellKey, origin: IVec2) -> Vec3 {
    match key {
        CellKey::Exterior { grid_x, grid_y, .. } => Vec3::new(
            (grid_x - origin.x) as f32 * CELL_SIZE,
            0.0,
            -(grid_y - origin.y) as f32 * CELL_SIZE,
        ),
        CellKey::Interior(_) => Vec3::ZERO,
    }
}

fn creation_to_bevy(position: Vec3) -> Vec3 {
    Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        position.to_array(),
    ))
}

fn creation_rotation_to_bevy(rotation: [f32; 3]) -> Quat {
    Quat::from_array(shared::coordinates::creation_euler_to_runtime_quaternion(
        rotation,
    ))
}

fn converted_model_path(path: String) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let lowercase = normalized.to_ascii_lowercase();
    let filename = lowercase.rsplit('/').next().unwrap_or_default();
    if lowercase.starts_with("meshes/sky/")
        || lowercase.starts_with("sky/")
        || lowercase.starts_with("meshes/markers/")
        || lowercase.starts_with("markers/")
        || lowercase.starts_with("meshes/effects/")
        || lowercase.starts_with("effects/")
        || filename.contains("marker")
    {
        return None;
    }
    let without_prefix = normalized
        .strip_prefix("meshes/")
        .or_else(|| normalized.strip_prefix("Meshes/"))
        .unwrap_or(&normalized);
    if without_prefix.is_empty() {
        return None;
    }
    let mut converted = std::path::PathBuf::from("meshes").join(without_prefix);
    converted.set_extension("glb");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn quadrant_layers(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Vec<&TerrainLayerSnapshot>, String> {
    let mut layers: Vec<_> = terrain
        .layers
        .iter()
        .filter(|layer| layer.quadrant == quadrant)
        .collect();
    layers.sort_by_key(|layer| (!layer.is_base, layer.layer, layer.texture_form_id));
    if layers.is_empty() {
        return Ok(layers);
    }
    let base_count = layers.iter().filter(|layer| layer.is_base).count();
    if base_count != 1 {
        return Err(format!(
            "LAND {:08X} quadrant {quadrant} has {base_count} base layers; expected one",
            terrain.cell_id
        ));
    }
    if layers.len() > 6 {
        return Err(format!(
            "LAND {:08X} quadrant {quadrant} has {} layers; runtime supports six",
            terrain.cell_id,
            layers.len()
        ));
    }
    let mut layer_ids = HashSet::new();
    for layer in layers.iter().filter(|layer| !layer.is_base) {
        if !layer_ids.insert(layer.layer) {
            return Err(format!(
                "LAND {:08X} quadrant {quadrant} repeats ATXT layer {}",
                terrain.cell_id, layer.layer
            ));
        }
        let mut vertices = HashSet::new();
        for &(vertex, opacity) in &layer.weights {
            if usize::from(vertex) >= 17 * 17
                || !opacity.is_finite()
                || !(0.0..=1.0).contains(&opacity)
                || !vertices.insert(vertex)
            {
                return Err(format!(
                    "LAND {:08X} quadrant {quadrant} has invalid or duplicate VTXT data",
                    terrain.cell_id
                ));
            }
        }
    }
    Ok(layers)
}

fn validate_terrain_snapshot(
    terrain: &TerrainSnapshot,
    catalog: &AssetCatalog,
) -> Result<(), String> {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err(format!(
            "terrain dimensions/data mismatch: {width}x{height} with {} heights",
            terrain.heights.len()
        ));
    }
    if terrain.heights.iter().any(|height| !height.is_finite()) {
        return Err("terrain contains a non-finite height".to_owned());
    }
    if terrain.normals.len() != width * height * 3 {
        return Err(format!(
            "terrain has {} packed normal bytes",
            terrain.normals.len()
        ));
    }
    if terrain
        .normals
        .as_chunks::<3>()
        .0
        .iter()
        .any(|normal| normal == &[0, 0, 0])
    {
        return Err("terrain contains a zero-length packed normal".to_owned());
    }
    if !terrain.vertex_colors.is_empty() && terrain.vertex_colors.len() != width * height * 3 {
        return Err(format!(
            "terrain has {} packed vertex-color bytes",
            terrain.vertex_colors.len()
        ));
    }
    for quadrant in 0..4 {
        for layer in quadrant_layers(terrain, quadrant)? {
            if layer.is_base && layer.texture_form_id == 0 {
                continue;
            }
            if catalog.landscape_diffuse(layer.texture_form_id).is_none() {
                return Err(format!(
                    "quadrant {quadrant} texture {:08X} has no converted diffuse image",
                    layer.texture_form_id
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn build_terrain_quadrant_mesh(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Mesh, String> {
    let layers = quadrant_layers(terrain, quadrant)?;
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err("terrain must contain a complete 33x33 height field".to_owned());
    }
    let step_x = CELL_SIZE / (width - 1) as f32;
    let step_z = CELL_SIZE / (height - 1) as f32;
    let origin_x = usize::from(quadrant % 2) * 16;
    let origin_y = usize::from(quadrant / 2) * 16;
    let mut overlay_weights = vec![vec![0.0f32; 17 * 17]; layers.len().saturating_sub(1)];
    for (slot, layer) in layers.iter().skip(1).enumerate() {
        for &(vertex, opacity) in &layer.weights {
            overlay_weights[slot][usize::from(vertex)] = opacity;
        }
    }
    let mut positions = Vec::with_capacity(17 * 17);
    let mut normals = Vec::with_capacity(17 * 17);
    let mut uvs = Vec::with_capacity(17 * 17);
    let mut extra_weights = Vec::with_capacity(17 * 17);
    let mut packed_weights = Vec::with_capacity(17 * 17);
    let mut colors = Vec::with_capacity(17 * 17);
    for local_y in 0..17 {
        for local_x in 0..17 {
            let x = origin_x + local_x;
            let y = origin_y + local_y;
            let index = y * width + x;
            let local = local_y * 17 + local_x;
            positions.push([
                x as f32 * step_x,
                terrain.heights[index],
                -(y as f32 * step_z),
            ]);
            normals.push(
                Vec3::new(
                    terrain.normals[index * 3] as f32,
                    terrain.normals[index * 3 + 2] as f32,
                    -(terrain.normals[index * 3 + 1] as f32),
                )
                .normalize_or(Vec3::Y)
                .to_array(),
            );
            uvs.push([
                x as f32 / (width - 1) as f32,
                y as f32 / (height - 1) as f32,
            ]);
            let weight = |slot: usize| {
                overlay_weights
                    .get(slot)
                    .map_or(0.0, |values| values[local])
            };
            let first = Vec3::new(weight(0), weight(1), weight(2));
            let length = first.length();
            packed_weights.push(if length > 0.0 {
                let normalized = first / length;
                [normalized.x, normalized.y, normalized.z, length]
            } else {
                [0.0; 4]
            });
            extra_weights.push([weight(3), weight(4)]);
            colors.push(if terrain.vertex_colors.is_empty() {
                [1.0; 4]
            } else {
                [
                    terrain.vertex_colors[index * 3] as f32 / 255.0,
                    terrain.vertex_colors[index * 3 + 1] as f32 / 255.0,
                    terrain.vertex_colors[index * 3 + 2] as f32 / 255.0,
                    1.0,
                ]
            });
        }
    }
    let mut indices = Vec::with_capacity(16 * 16 * 6);
    for y in 0..16 {
        for x in 0..16 {
            let a = (y * 17 + x) as u32;
            let b = a + 1;
            let c = a + 17;
            let d = c + 1;
            indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, extra_weights);
    mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, packed_weights);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    Ok(mesh)
}

/// The largest difference between an arriving cell's edge and a resident neighbour's that is still
/// treated as a seam to weld.
///
/// Real seams are small: the worst measured on `Skyrim.esm` is 24 units, on one of 33 points of
/// Tamriel's (18,18) north edge against (18,19), and 16 units on one point of (18,20)'s east edge.
/// The bound, 64 units (half the height field's 128-unit sample spacing), leaves the measured seams
/// almost three times the room they need while staying small next to the terrain's own detail. A
/// larger difference is not a crack but two landscapes that were authored apart - the city
/// worldspaces' sculpted cells against the copies of Tamriel's land beside them (Markarth 520-2520
/// units, Solitude 560-2352) - which Skyrim draws as authored, so that edge is left alone.
const MAX_WELDABLE_EDGE_DELTA: f32 = 64.0;

/// Edge heights closer than this are already the same point: the tolerance the strict comparison
/// used before edges were welded.
const EDGE_MATCH_TOLERANCE: f32 = 0.01;

/// One side of the arriving cell that a resident neighbour can be welded onto.
struct WeldableEdge {
    /// Which of the arriving cell's four sides this is.
    side: TerrainEdgeSide,
    /// The neighbour sharing `side`.
    neighbor: CellKey,
    /// The neighbour's registered heights along the shared edge, position for position with `side`.
    heights: Vec<f32>,
    /// The largest difference the weld has to close along the edge.
    max_delta: f32,
}

/// Registers a cell's edge heights, welding the arriving cell onto the neighbours already drawn.
///
/// The resident neighbour is authoritative: its mesh is in the world, so where the two disagree
/// the arriving cell moves. This replaces the strict comparison that rejected the whole cell -
/// and with it the terrain and its references - over a single point of one edge, which is what
/// leaves a hole in the ground on real `Skyrim.esm` data. An edge that differs by more than
/// [`MAX_WELDABLE_EDGE_DELTA`] is not welded and not rejected either: it is drawn as authored and
/// counted, as Skyrim draws every `LAND` as its own mesh without comparing neighbours. What is
/// still rejected is data that cannot be drawn: an edge of a different length and a non-finite
/// height on either side.
///
/// The welded heights - not the loaded ones - are what gets registered, so a cell arriving later
/// welds onto the surface that is actually drawn and the block stays watertight.
fn validate_and_register_terrain_edges(
    key: CellKey,
    terrain: &mut TerrainSnapshot,
    continuity: &mut TerrainContinuity,
    metrics: &mut StreamingMetrics,
) -> Result<(), String> {
    let CellKey::Exterior {
        worldspace_id,
        grid_x,
        grid_y,
    } = key
    else {
        return Ok(());
    };
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    // `validate_terrain_snapshot` checks these on the loading path, but the point indices below are
    // computed here, and a non-finite height would be welded into the neighbours' shared edges while
    // `f32::max` kept quiet about it, so the field this function relies on is checked here too.
    if width < 2 || height < 2 || terrain.heights.len() != width * height {
        return Err(format!(
            "terrain must hold a height field of at least 2x2: {width}x{height} with {} heights",
            terrain.heights.len()
        ));
    }
    if terrain.heights.iter().any(|height| !height.is_finite()) {
        return Err("terrain contains a non-finite height".to_owned());
    }
    let grid = IVec2::new(grid_x, grid_y);
    // Every shared edge is read before any height moves, so a rejected cell is left exactly as it
    // was loaded even when an earlier edge turned out to be weldable.
    let mut weldable = Vec::new();
    for side in TerrainEdgeSide::ALL {
        let neighbor_key = side.neighbor_key(worldspace_id, grid);
        let Some(neighbor) = continuity.edges.get(&neighbor_key) else {
            continue;
        };
        let other = neighbor.side(side.opposite());
        if side.len(terrain) != other.len() {
            return Err(format!(
                "terrain edge {side:?} has {} points; neighbor {neighbor_key:?} has {}",
                side.len(terrain),
                other.len()
            ));
        }
        if other.iter().any(|height| !height.is_finite()) {
            return Err(format!(
                "terrain edge {side:?} of neighbor {neighbor_key:?} is not finite"
            ));
        }
        let max_delta = (0..side.len(terrain))
            .map(|position| {
                (terrain.heights[side.index(terrain, position)] - other[position]).abs()
            })
            .fold(0.0_f32, f32::max);
        if max_delta > MAX_WELDABLE_EDGE_DELTA {
            // Two landscapes authored apart, not a crack: rejecting the cell dropped its terrain
            // and every reference on it, the holes in Markarth's and Solitude's ground.
            warn!(
                ?key,
                neighbor = ?neighbor_key,
                side = ?side,
                max_delta,
                "terrain edge differs from its neighbour past the weld bound; drawn as authored"
            );
            metrics.terrain_edges_left_as_authored =
                metrics.terrain_edges_left_as_authored.saturating_add(1);
            continue;
        }
        weldable.push(WeldableEdge {
            side,
            neighbor: neighbor_key,
            heights: other.to_vec(),
            max_delta,
        });
    }
    let mut welded_points = Vec::new();
    for edge in &weldable {
        // The points at the two ends of a side are its corners, welded once each below.
        let mut moved = 0u64;
        for position in 1..edge.side.len(terrain) - 1 {
            let index = edge.side.index(terrain, position);
            let height = edge.heights[position];
            if (terrain.heights[index] - height).abs() > EDGE_MATCH_TOLERANCE {
                welded_points.push(index);
                moved += 1;
            }
            terrain.heights[index] = height;
        }
        if moved > 0 {
            debug!(
                cell = format_args!("{:08X}", terrain.cell_id),
                neighbor = ?edge.neighbor,
                max_delta = edge.max_delta,
                moved,
                "LAND edge welded onto the resident neighbor"
            );
        }
        metrics.terrain_seams_validated = metrics.terrain_seams_validated.saturating_add(1);
    }
    // A corner point is the end of two sides, so welding it with both would write it twice - the
    // later side winning - and count it twice. The two neighbours meeting there can disagree as
    // well, since they share only that point and never an edge, and then the arriving corner cannot
    // agree with both of them. Each corner is therefore welded last and once, to the first of its
    // two sides that has a resident neighbour: the corner follows a single neighbour, and what is
    // left where the two disagree is the difference they already had between them, which no
    // arriving cell can close.
    let corners = [
        (0, TerrainEdgeSide::West, TerrainEdgeSide::South),
        (width - 1, TerrainEdgeSide::East, TerrainEdgeSide::South),
        (
            (height - 1) * width,
            TerrainEdgeSide::West,
            TerrainEdgeSide::North,
        ),
        (
            height * width - 1,
            TerrainEdgeSide::East,
            TerrainEdgeSide::North,
        ),
    ];
    for (index, first, second) in corners {
        // The corner's row along a vertical side and its column along a horizontal one: how the
        // point sits on each of the two sides.
        let corner = [first, second].into_iter().find_map(|side| {
            let edge = weldable.iter().find(|edge| edge.side == side)?;
            let position = match side {
                TerrainEdgeSide::West | TerrainEdgeSide::East => index / width,
                TerrainEdgeSide::South | TerrainEdgeSide::North => index % width,
            };
            Some((side, edge.heights[position]))
        });
        let Some((side, height)) = corner else {
            continue;
        };
        if (terrain.heights[index] - height).abs() > EDGE_MATCH_TOLERANCE {
            welded_points.push(index);
            terrain.heights[index] = height;
            debug!(
                cell = format_args!("{:08X}", terrain.cell_id),
                corner = index,
                side = ?side,
                height,
                "LAND corner welded onto the resident neighbor"
            );
        }
    }
    if !welded_points.is_empty() {
        // A moved point no longer lies where its stored normal was computed, and the drawn
        // triangle is what gets shaded, so recompute it from the welded field. Real seams have
        // the two sides' normals already matching, so keeping the loaded ones would shade the
        // boundary exactly like the neighbour at the cost of a normal that disagrees with our
        // own geometry. The normals of the points beside a moved one are computed from its
        // height too, so they are recomputed as well.
        recompute_packed_normals(terrain, &points_and_neighbours(terrain, &welded_points));
        metrics.terrain_seam_points_welded = metrics
            .terrain_seam_points_welded
            .saturating_add(welded_points.len() as u64);
    }
    continuity.edges.insert(key, TerrainEdges::of(terrain));
    Ok(())
}

/// Recomputes the packed `VNML` bytes of the listed height-field points from the heights around
/// them, in the converter's own encoding (`crates/converter/src/esm/cell_cache.rs`,
/// `decode_normals`): `(h(left) - h(right), h(down) - h(up), 2 * step)`, normalized and scaled to
/// the `i8` range. `points` index a complete `width * height` field.
/// The given sample indices and their in-bounds cardinal neighbours, sorted and without
/// duplicates: every sample whose normal reads the height of a given one.
fn points_and_neighbours(terrain: &TerrainSnapshot, points: &[usize]) -> Vec<usize> {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    let mut all = Vec::with_capacity(points.len() * 5);
    for &index in points {
        let (x, y) = (index % width, index / width);
        all.push(index);
        if x > 0 {
            all.push(index - 1);
        }
        if x + 1 < width {
            all.push(index + 1);
        }
        if y > 0 {
            all.push(index - width);
        }
        if y + 1 < height {
            all.push(index + width);
        }
    }
    all.sort_unstable();
    all.dedup();
    all
}

fn recompute_packed_normals(terrain: &mut TerrainSnapshot, points: &[usize]) {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width < 2 || height < 2 || terrain.normals.len() != width * height * 3 {
        return;
    }
    let step = CELL_SIZE / (width - 1) as f32;
    for &index in points {
        let (x, y) = (index % width, index / width);
        let left = terrain.heights[y * width + x.saturating_sub(1)];
        let right = terrain.heights[y * width + (x + 1).min(width - 1)];
        let down = terrain.heights[y.saturating_sub(1) * width + x];
        let up = terrain.heights[(y + 1).min(height - 1) * width + x];
        let normal = Vec3::new(left - right, down - up, 2.0 * step).normalize_or(Vec3::Z);
        // A height field's own normal always has a positive up component, so the packed bytes can
        // never come out all zero - which is what the validation rejects.
        let byte = |component: f32| (component * 127.0).round().clamp(-127.0, 127.0) as i8;
        terrain.normals[index * 3..index * 3 + 3].copy_from_slice(&[
            byte(normal.x),
            byte(normal.y),
            byte(normal.z),
        ]);
    }
}

fn update_render_origin(
    mut origin: ResMut<RenderOrigin>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    let shift = IVec2::new(
        (camera.translation.x / CELL_SIZE).trunc() as i32,
        (-camera.translation.z / CELL_SIZE).trunc() as i32,
    );
    if shift == IVec2::ZERO {
        return;
    }
    origin.0 += shift;
    camera.translation.x -= shift.x as f32 * CELL_SIZE;
    camera.translation.z += shift.y as f32 * CELL_SIZE;
    for (grid, mut transform) in &mut roots {
        transform.translation = Vec3::new(
            (grid.0.x - origin.0.x) as f32 * CELL_SIZE,
            0.0,
            -(grid.0.y - origin.0.y) as f32 * CELL_SIZE,
        );
    }
    profiler.increment("streaming/origin_rebases", 1);
    metrics.origin_rebases = metrics.origin_rebases.saturating_add(1);
    profiler.event(
        format!("{},{}", origin.0.x, origin.0.y),
        "origin_rebased",
        None,
    );
    profiler.record_elapsed("streaming/render_origin_rebase", started);
}

fn validate_streaming_lifecycle(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    streaming: Res<StreamingWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<(Entity, &CellRef, Option<&ExteriorCellGrid>), With<StreamedCellRoot>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let active_requests = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Loading { .. }))
        .count();
    let resident_entities: HashSet<_> = streaming
        .cells
        .values()
        .filter_map(|status| match status {
            CellStatus::Resident { root } => Some(*root),
            _ => None,
        })
        .collect();
    let root_entries: Vec<_> = roots.iter().collect();
    let root_entities: HashSet<_> = root_entries.iter().map(|(entity, _, _)| *entity).collect();
    let mut roots_by_cell = HashMap::<u32, usize>::new();
    for (_, cell, _) in &root_entries {
        *roots_by_cell.entry(cell.0).or_default() += 1;
    }
    let duplicate_roots = roots_by_cell.values().filter(|count| **count > 1).count() as u64;
    let orphaned_roots = root_entities.difference(&resident_entities).count() as u64;
    let missing_roots = resident_entities.difference(&root_entities).count() as u64;
    let out_of_range_roots = camera.single().map_or(0, |camera| {
        let global_x = camera.translation.x + origin.0.x as f32 * CELL_SIZE;
        let global_y = -camera.translation.z + origin.0.y as f32 * CELL_SIZE;
        let center = IVec2::new(
            (global_x / CELL_SIZE).floor() as i32,
            (global_y / CELL_SIZE).floor() as i32,
        );
        root_entries
            .iter()
            .filter_map(|(_, _, grid)| *grid)
            .filter(|grid| {
                (grid.0.x - center.x).abs() > config.unload_radius
                    || (grid.0.y - center.y).abs() > config.unload_radius
            })
            .count() as u64
    });
    let violations = duplicate_roots + orphaned_roots + missing_roots + out_of_range_roots;

    metrics.active_requests = active_requests;
    metrics.peak_active_requests = metrics.peak_active_requests.max(active_requests);
    metrics.resident_roots = root_entries.len();
    metrics.duplicate_cell_roots = metrics.duplicate_cell_roots.max(duplicate_roots);
    metrics.orphaned_cell_roots = metrics.orphaned_cell_roots.max(orphaned_roots);
    metrics.missing_cell_roots = metrics.missing_cell_roots.max(missing_roots);
    metrics.out_of_range_cell_roots = metrics.out_of_range_cell_roots.max(out_of_range_roots);
    if violations > metrics.streaming_invariant_failures {
        error!(
            duplicate_roots,
            orphaned_roots,
            missing_roots,
            out_of_range_roots,
            "streaming lifecycle invariant failed"
        );
        profiler.event("streaming", "invariant_failed", None);
    }
    metrics.streaming_invariant_failures = metrics.streaming_invariant_failures.max(violations);
    profiler.set_gauge("streaming/active_requests", active_requests as f64);
    profiler.set_gauge("streaming/resident_roots", root_entries.len() as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_budget_ignores_only_the_documented_scheduler_tolerance() {
        assert!(!commit_budget_exceeded(16_670, 16_670));
        assert!(!commit_budget_exceeded(17_670, 16_670));
        assert!(commit_budget_exceeded(17_671, 16_670));
    }

    fn terrain_fixture(cell_id: u32, height: f32) -> TerrainSnapshot {
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![height; 33 * 33],
            normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
            vertex_colors: vec![255; 33 * 33 * 3],
            layers: (0..4)
                .map(|quadrant| TerrainLayerSnapshot {
                    texture_form_id: u32::from(quadrant) + 1,
                    quadrant,
                    layer: 0,
                    is_base: true,
                    weights: Vec::new(),
                })
                .collect(),
            water_height: None,
            water_type_form_id: None,
        }
    }

    #[test]
    fn maps_nif_paths_to_converted_glb_paths() {
        assert_eq!(
            converted_model_path("meshes\\architecture\\wall.nif".into()).as_deref(),
            Some("meshes/architecture/wall.glb")
        );
        assert_eq!(
            converted_model_path("meshes/Sky/CloudShape01.nif".into()),
            None
        );
        assert_eq!(converted_model_path("meshes/Marker_Map.nif".into()), None);
        assert_eq!(
            converted_model_path("Markers/CivilWarMarkers/CWAttSpawn02.nif".into()),
            None
        );
        assert_eq!(converted_model_path("Effects/FXRapids.nif".into()), None);
        assert_eq!(
            converted_model_path("meshes/Furniture/SitLedgeMarker.nif".into()),
            None
        );
    }

    #[test]
    fn unload_radius_removes_distant_exteriors_but_keeps_interiors() {
        let center = IVec2::new(4, -2);
        assert!(cell_within_unload_radius(
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 7,
                grid_y: -5,
            },
            center,
            3,
        ));
        assert!(!cell_within_unload_radius(
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 8,
                grid_y: -2,
            },
            center,
            3,
        ));
        assert!(cell_within_unload_radius(CellKey::Interior(99), center, 0));
    }

    #[test]
    fn repeated_rebasing_preserves_camera_and_cell_root_locality() {
        let mut app = App::new();
        app.insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, update_render_origin);
        let camera = app
            .world_mut()
            .spawn((Transform::default(), StreamingCamera))
            .id();
        let root = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::new(8, -3)), Transform::default()))
            .id();
        for shift in [IVec2::new(2, 1), IVec2::new(-3, 4), IVec2::new(7, -2)] {
            {
                let mut entity = app.world_mut().entity_mut(camera);
                let mut transform = entity.get_mut::<Transform>().unwrap();
                transform.translation.x = shift.x as f32 * CELL_SIZE + 12.0;
                transform.translation.z = -(shift.y as f32 * CELL_SIZE) - 20.0;
            }
            app.update();
            let camera_transform = app.world().entity(camera).get::<Transform>().unwrap();
            assert!(camera_transform.translation.x.abs() < CELL_SIZE);
            assert!(camera_transform.translation.z.abs() < CELL_SIZE);
        }
        assert_eq!(app.world().resource::<StreamingMetrics>().origin_rebases, 3);
        let origin = app.world().resource::<RenderOrigin>().0;
        let root_transform = app.world().entity(root).get::<Transform>().unwrap();
        assert_eq!(
            root_transform.translation,
            Vec3::new(
                (8 - origin.x) as f32 * CELL_SIZE,
                0.0,
                -(-3 - origin.y) as f32 * CELL_SIZE,
            )
        );
    }

    #[test]
    fn lifecycle_validator_detects_duplicate_and_orphaned_roots() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, validate_streaming_lifecycle);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        let resident = app.world_mut().spawn((CellRef(7), StreamedCellRoot)).id();
        app.world_mut().spawn((CellRef(7), StreamedCellRoot));
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                CellKey::Interior(7),
                CellStatus::Resident { root: resident },
            );
        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.duplicate_cell_roots, 1);
        assert_eq!(metrics.orphaned_cell_roots, 1);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 2);
    }

    #[test]
    fn maps_creation_position_and_rotation_through_the_same_basis() {
        assert_eq!(creation_to_bevy(Vec3::Y), Vec3::NEG_Z);
        assert_eq!(creation_to_bevy(Vec3::Z), Vec3::Y);

        let rotation = creation_rotation_to_bevy([0.0, 0.0, std::f32::consts::FRAC_PI_2]);
        let rotated = rotation * Vec3::X;
        // Creation yaw turns clockwise: east (+X) turns to south (-Y), runtime +Z.
        assert!(rotated.abs_diff_eq(Vec3::Z, 1.0e-5));
    }

    #[test]
    fn creates_upward_wound_quadrants_with_continuous_uvs() {
        let terrain = terrain_fixture(1, 0.0);
        let mesh = build_terrain_quadrant_mesh(&terrain, 3).unwrap();
        assert_eq!(mesh.count_vertices(), 17 * 17);
        assert_eq!(mesh.indices().unwrap().len(), 16 * 16 * 6);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        let [a, b, c] = [positions[0], positions[1], positions[17]];
        let normal = (Vec3::from(b) - Vec3::from(a)).cross(Vec3::from(c) - Vec3::from(a));
        assert!(normal.y > 0.0);
        let VertexAttributeValues::Float32x2(uvs) = mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap()
        else {
            panic!("terrain UVs must be Float32x2");
        };
        assert_eq!(uvs[0], [0.5, 0.5]);
        assert_eq!(uvs[16 * 17 + 16], [1.0, 1.0]);
    }

    fn exterior_cell(grid_x: i32, grid_y: i32) -> CellKey {
        CellKey::Exterior {
            worldspace_id: 60,
            grid_x,
            grid_y,
        }
    }

    /// The seam measured on real data - Tamriel (18,18) against (18,19), one point of the north
    /// edge 24 units off, across a sample step of 128 units - used to reject the whole cell and
    /// leave a hole where the player stands. It must weld, and nothing but that point may move.
    #[test]
    fn welds_one_point_of_a_shared_edge_below_the_tolerance() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let mut resident = terrain_fixture(1, 10.0);
        validate_and_register_terrain_edges(
            exterior_cell(0, 0),
            &mut resident,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();
        assert_eq!(
            metrics.terrain_seams_validated, 0,
            "the first cell has no registered neighbor to match"
        );

        // The arriving cell matches its resident neighbour along the shared edge except at one
        // point of it, and steps up one sample in from the edge so the weld is visible in the
        // normals as well as in the heights.
        let mut arriving = terrain_fixture(2, 10.0);
        for row in 0..33 {
            arriving.heights[row * 33 + 1] = 100.0;
        }
        arriving.heights[7 * 33] = 34.0;
        let loaded = arriving.heights.clone();
        validate_and_register_terrain_edges(
            exterior_cell(1, 0),
            &mut arriving,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();

        assert_eq!(metrics.terrain_seams_validated, 1);
        assert_eq!(metrics.terrain_seam_points_welded, 1);
        for (index, height) in arriving.heights.iter().enumerate() {
            if index % 33 == 0 {
                assert_eq!(*height, 10.0, "the shared edge is the resident's");
            } else {
                assert_eq!(
                    *height, loaded[index],
                    "a point that is not on the shared edge must not move"
                );
            }
        }
        // Both sides of the seam now hold the same heights, which is what keeps the block
        // watertight for the next cell to arrive.
        let registered = continuity.edges.get(&exterior_cell(1, 0)).unwrap();
        assert_eq!(registered.west, vec![10.0; 33]);
        assert_eq!(
            continuity.edges.get(&exterior_cell(0, 0)).unwrap().east,
            registered.west
        );
        // The moved point's normal was recomputed from the welded field: (left - right,
        // down - up, 2 * step) = (10 - 100, 0, 256) normalized and scaled to the `i8` range,
        // rather than the [0, 0, 127] it was loaded with.
        assert_eq!(
            &arriving.normals[7 * 33 * 3..7 * 33 * 3 + 3],
            &[-42, 0, 120]
        );
        assert_eq!(
            &arriving.normals[..3],
            &[0, 0, 127],
            "a point that did not move keeps its normal"
        );
    }

    /// A corner point is the end of two sides, so both of them would weld it; with the two
    /// residents disagreeing there the later side won, the point was counted twice, and one of the
    /// two neighbours was left with the crack. The corner follows the first of its two neighbours
    /// instead, which keeps the difference the residents already had between them and no arriving
    /// cell can close.
    #[test]
    fn welds_a_corner_once_to_the_first_resident_neighbour() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        // Two residents sharing only the corner of the arriving cell: (0,1) to its west and (1,0)
        // to its south, 24 units apart there - the worst seam measured on real data.
        for (key, cell_id, height) in [
            (exterior_cell(0, 1), 1, 10.0),
            (exterior_cell(1, 0), 2, 34.0),
        ] {
            validate_and_register_terrain_edges(
                key,
                &mut terrain_fixture(cell_id, height),
                &mut continuity,
                &mut metrics,
            )
            .unwrap();
        }
        assert_eq!(
            metrics.terrain_seams_validated, 0,
            "the two residents are diagonal and share no edge"
        );

        // The arriving cell matches the west resident along their whole shared edge except at the
        // corner, where it holds the south resident's height.
        let mut arriving = terrain_fixture(3, 34.0);
        for row in 0..33 {
            arriving.heights[row * 33] = 10.0;
        }
        arriving.heights[0] = 34.0;
        validate_and_register_terrain_edges(
            exterior_cell(1, 1),
            &mut arriving,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();

        assert_eq!(metrics.terrain_seams_validated, 2);
        assert_eq!(
            metrics.terrain_seam_points_welded, 1,
            "the corner is welded once, not once per side"
        );
        assert_eq!(
            arriving.heights[0], 10.0,
            "the corner follows West, its first side with a resident neighbour"
        );
        assert_eq!(
            arriving.heights[32], 34.0,
            "the other end of the south edge follows the south resident"
        );
        let registered = continuity.edges.get(&exterior_cell(1, 1)).unwrap();
        assert_eq!(registered.west, vec![10.0; 33]);
        let mut expected_south = vec![34.0; 33];
        expected_south[0] = 10.0;
        assert_eq!(
            registered.south, expected_south,
            "the south edge matches the south resident apart from the shared corner"
        );
        assert_eq!(
            continuity.edges.get(&exterior_cell(1, 0)).unwrap().north[0],
            34.0,
            "the residents do not move, so the corner keeps the difference they already had"
        );
        // The welded corner's normal comes from the welded field: (left - right, down - up,
        // 2 * step) = (10 - 34, 10 - 10, 256) over a sample step of 128 units, normalized
        // (length 257.122) and scaled by 127, rather than the [0, 0, 127] it was loaded with.
        assert_eq!(&arriving.normals[..3], &[-12, 0, 126]);
    }

    /// Past the bound the edge is not a seam: it is left as authored, the way Skyrim draws every
    /// LAND on its own. The cell is kept, its other edges within the bound are still welded, and
    /// every edge is registered.
    #[test]
    fn keeps_a_cell_whose_edge_differs_past_the_weld_bound_as_authored() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        for (key, cell_id) in [(exterior_cell(0, 0), 1), (exterior_cell(1, 1), 3)] {
            validate_and_register_terrain_edges(
                key,
                &mut terrain_fixture(cell_id, 10.0),
                &mut continuity,
                &mut metrics,
            )
            .unwrap();
        }
        assert_eq!(
            metrics.terrain_seams_validated, 0,
            "the two residents are diagonal and share no edge"
        );

        // The west edge is a weldable seam; the north edge is past the bound, as where a city's
        // sculpted landscape meets the unsculpted land beside it.
        let mut arriving = terrain_fixture(2, 10.0);
        arriving.heights[7 * 33] = 34.0;
        let authored = 10.0 + MAX_WELDABLE_EDGE_DELTA + 1.0;
        arriving.heights[32 * 33 + 15] = authored;
        let key = exterior_cell(1, 0);
        validate_and_register_terrain_edges(key, &mut arriving, &mut continuity, &mut metrics)
            .expect(
                "an edge past the weld bound is drawn as authored, not a reason to drop the cell",
            );
        assert_eq!(
            arriving.heights[7 * 33],
            10.0,
            "the weldable west seam is welded"
        );
        assert_eq!(
            arriving.heights[32 * 33 + 15],
            authored,
            "the edge past the bound keeps its authored heights"
        );
        assert!(
            continuity.edges.contains_key(&key),
            "the kept cell registers its edges for the cells that arrive after it"
        );
        assert_eq!(metrics.terrain_edges_left_as_authored, 1);
        assert_eq!(metrics.terrain_seam_points_welded, 1);
    }

    #[test]
    fn recomputes_packed_normals_from_the_surrounding_heights() {
        let mut terrain = terrain_fixture(1, 0.0);
        let index = 5 * 33 + 5;
        terrain.heights[index + 1] = 100.0;
        recompute_packed_normals(&mut terrain, &[index]);
        // (left - right, down - up, 2 * step) = (-100, 0, 256) over a sample step of 128 units,
        // normalized (length 274.838) and scaled by 127, as the converter's `decode_normals` does.
        assert_eq!(&terrain.normals[index * 3..index * 3 + 3], &[-46, 0, 118]);
        assert_eq!(
            &terrain.normals[..3],
            &[0, 0, 127],
            "a point that was not listed keeps its normal"
        );
    }

    #[test]
    fn a_moved_point_also_refreshes_its_neighbours_normals() {
        let mut terrain = terrain_fixture(1, 0.0);
        let index = 5 * 33 + 5;
        terrain.heights[index] = 100.0;
        let points = points_and_neighbours(&terrain, &[index]);
        assert_eq!(
            points,
            vec![index - 33, index - 1, index, index + 1, index + 33]
        );
        recompute_packed_normals(&mut terrain, &points);
        // Each neighbour's normal reads the moved height: (left - right, down - up, 2 * step)
        // with 100 on one side, (100, 0, 256) normalized and scaled by 127.
        assert_eq!(
            &terrain.normals[(index + 1) * 3..(index + 1) * 3 + 3],
            &[46, 0, 118]
        );
        assert_eq!(
            &terrain.normals[(index - 1) * 3..(index - 1) * 3 + 3],
            &[-46, 0, 118]
        );
        assert_eq!(
            &terrain.normals[(index + 33) * 3..(index + 33) * 3 + 3],
            &[0, 46, 118]
        );
        assert_eq!(
            &terrain.normals[(index - 33) * 3..(index - 33) * 3 + 3],
            &[0, -46, 118]
        );
        // The moved point itself sits between equal heights, so it stays flat.
        assert_eq!(&terrain.normals[index * 3..index * 3 + 3], &[0, 0, 127]);
        // A corner has only two neighbours.
        assert_eq!(points_and_neighbours(&terrain, &[0]), vec![0, 1, 33]);
    }

    #[test]
    fn rejects_more_than_six_layers_per_quadrant() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain
            .layers
            .extend((1..=6).map(|layer| TerrainLayerSnapshot {
                texture_form_id: u32::from(layer) + 10,
                quadrant: 0,
                layer,
                is_base: false,
                weights: Vec::new(),
            }));
        assert!(quadrant_layers(&terrain, 0).is_err());
    }

    #[test]
    fn accepts_textureless_official_land_quadrant() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain.layers.clear();
        assert!(quadrant_layers(&terrain, 0).unwrap().is_empty());
    }

    #[test]
    fn validates_loaded_material_images_and_rejects_missing_required_texture() {
        let mut images = Assets::<Image>::default();
        let base_color = images.add(Image::new_fill(
            bevy::render::render_resource::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            bevy::render::render_resource::TextureDimension::D2,
            &[255, 255, 255, 255],
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        ));
        let material = StandardMaterial {
            base_color_texture: Some(base_color),
            ..default()
        };
        assert_eq!(validate_standard_material(&material, &images), Ok(1));

        let missing = StandardMaterial {
            normal_map_texture: Some(Handle::default()),
            ..default()
        };
        assert!(
            validate_standard_material(&missing, &images)
                .unwrap_err()
                .contains("normal image")
        );
    }

    #[test]
    fn rejects_invalid_alpha_and_culling_semantics() {
        let images = Assets::<Image>::default();
        assert!(
            validate_standard_material(
                &StandardMaterial {
                    alpha_mode: AlphaMode::Mask(f32::NAN),
                    ..default()
                },
                &images
            )
            .is_err()
        );
        assert!(
            validate_standard_material(
                &StandardMaterial {
                    double_sided: true,
                    ..default()
                },
                &images
            )
            .is_err()
        );
    }

    use bevy::asset::{AssetApp, AssetPlugin};
    use bevy::world_serialization::WorldSerializationPlugin;

    /// The app the empty-model tests run in: the real readiness scan
    /// ([`track_asset_readiness`]) over an asset server and the world serialization spawner the
    /// engine uses, so a converted model is spawned and its reference becomes ready by the same
    /// route a converted glb takes.
    fn model_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            WorldSerializationPlugin,
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .insert_resource(EngineConfig::default())
        .init_resource::<StreamingMetrics>()
        .init_resource::<ProfilingState>()
        .init_resource::<DiagnosticFallbackAssets>()
        // The converted scene holds entities, and the spawner reads each of their components out
        // of the type registry.
        .register_type::<ChildOf>()
        .register_type::<Children>()
        .register_type::<GlobalTransform>()
        .register_type::<Mesh3d>()
        .register_type::<Name>()
        .register_type::<Transform>()
        .add_observer(mark_world_instance_ready)
        .add_systems(Update, track_asset_readiness);
        app
    }

    /// Adds `scene` to the asset server as the converted model a reference points at, and runs the
    /// frame the asset system needs to publish it, so `is_loaded_with_dependencies` is true for it
    /// exactly as it is for a loaded glb.
    fn add_converted_model(app: &mut App, scene: World) -> Handle<WorldAsset> {
        let handle = app
            .world()
            .resource::<AssetServer>()
            .add(WorldAsset::new(scene));
        // The `Loaded` event is applied in the asset schedule, before the spawner reads it.
        app.update();
        handle
    }

    /// A converted model as the loader builds one for the converter's empty scene: the scene's own
    /// root entity and nothing below it, so the model has no node and no mesh.
    fn empty_converted_scene() -> World {
        let mut world = World::new();
        world.spawn((Name::new("wispambush"), Transform::default()));
        world
    }

    /// A converted model with real geometry, as the loader builds one: the scene's root and the
    /// mesh primitive below it.
    fn converted_scene_with_mesh(mesh: Handle<Mesh>) -> World {
        let mut world = World::new();
        let root = world.spawn(Name::new("wispambush")).id();
        world.spawn((Mesh3d(mesh), Transform::default(), ChildOf(root)));
        world
    }

    /// A reference as [`spawn_cell`] spawns one for a model: the root components, the asset root
    /// pointing at the loaded scene, and the pending profile the readiness scan waits on. No
    /// `ExpectedModelBounds` is inserted, which is what `statics.bounds_valid = 0` produces - the
    /// component's absence is the whole signal.
    fn spawn_model_reference(
        app: &mut App,
        handle: Handle<WorldAsset>,
        expected_bounds: Option<ExpectedModelBounds>,
    ) -> Entity {
        let transform = Transform::from_translation(Vec3::new(3.0, -4.0, 5.0));
        let mut entity = app.world_mut().spawn((
            Name::new("Reference 000F9907"),
            FormId(0x00F9907),
            CellRef(0x02D4E0),
            transform,
            GlobalTransform::from(transform),
            WorldTransform(transform.to_matrix()),
            WorldAssetRoot(handle),
            PendingAssetProfile {
                started: Instant::now(),
                scene_spawned: false,
                path: "meshes/furniture/creatureexit/wispambush.glb".to_owned(),
                form_id: 0x00F9907,
                base_form_id: 0x00EF957,
                cell_id: 0x02D4E0,
            },
        ));
        if let Some(bounds) = expected_bounds {
            entity.insert(bounds);
        }
        entity.id()
    }

    /// Runs the readiness scan until the reference leaves the pending set, and fails the test
    /// rather than reading an unsettled metric. The world instance is spawned in `SpawnScene` and
    /// the scan runs in `Update`, so a reference settles over more than one frame.
    fn settle_readiness(app: &mut App) -> StreamingMetrics {
        for _ in 0..16 {
            app.update();
            let streaming = app.world().resource::<StreamingMetrics>();
            if streaming.pending_asset_instances == 0 {
                break;
            }
        }
        let metrics = app.world().resource::<StreamingMetrics>().clone();
        assert_eq!(
            metrics.pending_asset_instances, 0,
            "the reference never left the pending set"
        );
        metrics
    }

    /// What the emptiness rule reads: the converted model's own node and mesh count, taken from
    /// the asset rather than from anything the engine spawned.
    #[test]
    fn reads_a_converted_models_own_node_and_mesh_count() {
        let empty = WorldAsset::new(empty_converted_scene());
        assert!(
            converted_scene_contents(&empty).is_empty(),
            "the converter's empty scene is what an empty model looks like"
        );

        let mesh_model = WorldAsset::new(converted_scene_with_mesh(Handle::default()));
        let contents = converted_scene_contents(&mesh_model);
        assert_eq!(contents.meshes, 1, "one primitive: {contents:?}");
        assert_eq!(contents.nodes, 1, "one node: {contents:?}");
    }

    /// A model with no renderable geometry converts to an **empty scene**, so it has no converted
    /// bounds and nothing to draw. A streamed cell full of such models used to fail the bounds
    /// gate on a model with nothing to draw. Such a reference is skipped and counted on its own,
    /// and nothing fails.
    #[test]
    fn an_empty_scene_model_is_skipped_and_counted_instead_of_failing() {
        let mut app = model_app();
        let handle = add_converted_model(&mut app, empty_converted_scene());
        let reference = spawn_model_reference(&mut app, handle, None);

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.transform_bounds_validation_failures, 0,
            "an invisible marker is not a conversion failure: {:?}",
            metrics.asset_failures
        );
        assert_eq!(
            metrics.empty_model_references, 1,
            "the empty model is counted on its own"
        );
        assert!(
            metrics.asset_failures.is_empty(),
            "nothing is recorded as a failed asset: {:?}",
            metrics.asset_failures
        );
        assert_eq!(
            metrics.bounds_validated, 0,
            "there were no converted bounds to validate"
        );
        assert_eq!(
            metrics.assets_ready, 0,
            "an empty model is not counted as a ready asset either"
        );
        assert!(
            !app.world()
                .entity(reference)
                .contains::<PendingAssetProfile>(),
            "the reference is no longer pending"
        );
        assert!(
            app.world().entity(reference).contains::<Transform>(),
            "the empty reference keeps its own transform"
        );
        assert!(
            app.world()
                .entity(reference)
                .get::<Children>()
                .is_some_and(|children| !children.is_empty()),
            "the converted scene is spawned below the reference"
        );
    }

    /// The tolerated class is narrow: the model itself must be empty. A converted scene that
    /// declares a node but no mesh - a hierarchy whose geometry an exporter dropped - is not an
    /// empty model, so it keeps failing exactly as it did before the rule existed.
    #[test]
    fn a_model_whose_converted_scene_declares_only_a_node_still_fails() {
        let mut app = model_app();
        let mut scene = World::new();
        let root = scene.spawn(Transform::default()).id();
        scene.spawn((Transform::default(), ChildOf(root)));
        let handle = add_converted_model(&mut app, scene);
        spawn_model_reference(&mut app, handle, None);

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.empty_model_references, 0,
            "a scene with a node in it is not an empty model"
        );
        assert_eq!(
            metrics.transform_bounds_validation_failures, 1,
            "a model that declares a node and no mesh must still fail: {:?}",
            metrics.asset_failures
        );
        let failure = metrics
            .asset_failures
            .first()
            .expect("the failure is recorded as an asset failure");
        assert!(
            failure
                .dependency_chain
                .iter()
                .any(|reason| reason.contains("is not empty: 0 mesh primitives, 1 nodes")),
            "unexpected failure reason: {:?}",
            failure.dependency_chain
        );
    }

    /// The other side of the same evidence: a model whose converted scene really holds a mesh
    /// primitive, spawned from the asset by the engine's own spawner rather than hand-built, and
    /// which still arrives without converted bounds, is a conversion defect and stays fatal.
    #[test]
    fn a_model_whose_converted_scene_holds_a_mesh_still_fails_without_bounds() {
        let mut app = model_app();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(2.0, 4.0, 6.0));
        let handle = add_converted_model(&mut app, converted_scene_with_mesh(mesh));
        let reference = spawn_model_reference(&mut app, handle, None);

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.empty_model_references, 0,
            "a model with geometry is never counted as empty"
        );
        assert_eq!(
            metrics.transform_bounds_validation_failures, 1,
            "a model with geometry and no converted bounds must still fail: {:?}",
            metrics.asset_failures
        );
        let failure = metrics
            .asset_failures
            .first()
            .expect("the failure is recorded as an asset failure");
        assert!(
            failure
                .dependency_chain
                .iter()
                .any(|reason| reason.contains("is not empty: 1 mesh primitives")),
            "unexpected failure reason: {:?}",
            failure.dependency_chain
        );
        // The model's mesh is a descendant of the reference: the engine's own spawner put it
        // there, which is the shape every bounds check in this module reads.
        let mut primitives = app.world_mut().query::<(Entity, &Mesh3d)>();
        let (primitive, _) = primitives
            .iter(app.world())
            .next()
            .expect("the converted model's mesh primitive is spawned");
        let scene_root = app
            .world()
            .entity(reference)
            .get::<Children>()
            .and_then(|children| children.first().copied())
            .expect("the converted scene is spawned below the reference");
        assert_eq!(
            app.world()
                .entity(primitive)
                .get::<ChildOf>()
                .map(ChildOf::parent),
            Some(scene_root),
            "the mesh primitive hangs below the scene root the spawner attached"
        );
    }

    /// The other direction of the same rule: a model the converter *did* bound, whose spawned
    /// scene turns out to be empty, is not an empty model to wave through - the two disagree and
    /// the disagreement is fatal.
    #[test]
    fn a_bounded_model_whose_scene_is_empty_still_fails() {
        let mut app = model_app();
        let handle = add_converted_model(&mut app, empty_converted_scene());
        spawn_model_reference(
            &mut app,
            handle,
            ExpectedModelBounds::new(Vec3::splat(-1.0), Vec3::splat(1.0)),
        );

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.empty_model_references, 0,
            "only a model without converted bounds may be an empty model"
        );
        assert_eq!(
            metrics.transform_bounds_validation_failures, 1,
            "a bound model whose hierarchy holds no mesh must still fail: {:?}",
            metrics.asset_failures
        );
        let failure = metrics
            .asset_failures
            .first()
            .expect("the failure is recorded as an asset failure");
        assert!(
            failure
                .dependency_chain
                .iter()
                .any(|reason| reason.contains("no bounded mesh")),
            "unexpected failure reason: {:?}",
            failure.dependency_chain
        );
    }

    /// Composes the same basis-rotation / mesh-translation chain as `HumanSkull.glb`'s node
    /// hierarchy in f64, giving a ground-truth model-space (relative-to-root) bounding box
    /// that never touches the root's large world-space position. This stands in for the
    /// converter's own independent, exact recomputation of the model's bounds.
    fn f64_relative_bounds(
        local_min: Vec3,
        local_max: Vec3,
        basis_rotation: Quat,
        mesh_translation: Vec3,
        mesh_scale: Vec3,
    ) -> (Vec3, Vec3) {
        use bevy::math::{DAffine3, DQuat, DVec3};

        let basis = DAffine3::from_quat(DQuat::from_xyzw(
            basis_rotation.x as f64,
            basis_rotation.y as f64,
            basis_rotation.z as f64,
            basis_rotation.w as f64,
        ));
        let mesh = DAffine3::from_scale_rotation_translation(
            DVec3::new(
                mesh_scale.x as f64,
                mesh_scale.y as f64,
                mesh_scale.z as f64,
            ),
            DQuat::IDENTITY,
            DVec3::new(
                mesh_translation.x as f64,
                mesh_translation.y as f64,
                mesh_translation.z as f64,
            ),
        );
        let relative = basis * mesh;
        let mut min = DVec3::splat(f64::INFINITY);
        let mut max = DVec3::splat(f64::NEG_INFINITY);
        for x in [local_min.x, local_max.x] {
            for y in [local_min.y, local_max.y] {
                for z in [local_min.z, local_max.z] {
                    let point = relative.transform_point3(DVec3::new(x as f64, y as f64, z as f64));
                    min = min.min(point);
                    max = max.max(point);
                }
            }
        }
        (
            Vec3::new(min.x as f32, min.y as f32, min.z as f32),
            Vec3::new(max.x as f32, max.y as f32, max.z as f32),
        )
    }

    // Composing each mesh's world transform and then multiplying by the root's
    // inverse world transform (the old algorithm) loses precision at real-data world
    // placements. This fixture mirrors the failing acceptance run: reference 000F6031's
    // world position/rotation, and HumanSkull.glb's node hierarchy (a -90-degree X basis
    // node with a mesh child of scale ~1.14 and translation (0, -7.7, -150.7)).
    #[test]
    fn spawned_bounds_validate_within_tolerance_despite_large_world_placement() {
        use bevy::ecs::system::SystemState;

        let root_translation = Vec3::new(20790.89, -69970.35, 10984.74);
        let root_rotation = Quat::from_euler(EulerRot::XYZ, 2.0587, 0.6207, 1.3418);
        let basis_rotation = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
        let mesh_translation = Vec3::new(0.0, -7.7, -150.7);
        let mesh_scale = Vec3::splat(1.14);
        let local_min = Vec3::splat(-6.0);
        let local_max = Vec3::splat(6.0);

        let (expected_min, expected_max) = f64_relative_bounds(
            local_min,
            local_max,
            basis_rotation,
            mesh_translation,
            mesh_scale,
        );
        let expected_bounds = ExpectedModelBounds::new(expected_min, expected_max)
            .expect("fixture bounds must be finite and non-degenerate");
        let extent = (expected_max - expected_min).abs().max_element().max(1.0);
        let tolerance = (extent * 1.0e-4).max(1.0e-3);

        let mut world = World::new();
        let mut meshes = Assets::<Mesh>::default();
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        let corners: Vec<[f32; 3]> = [local_min.x, local_max.x]
            .into_iter()
            .flat_map(|x| {
                [local_min.y, local_max.y]
                    .into_iter()
                    .flat_map(move |y| [local_min.z, local_max.z].map(move |z| [x, y, z]))
            })
            .collect();
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, corners);
        let mesh_handle = meshes.add(mesh);
        world.insert_resource(meshes);

        let root_local = Transform {
            translation: root_translation,
            rotation: root_rotation,
            ..Default::default()
        };
        let root_global = GlobalTransform::from(root_local);
        let root = world.spawn((root_local, root_global)).id();

        let basis_local = Transform {
            rotation: basis_rotation,
            ..Default::default()
        };
        let basis_global = root_global.mul_transform(basis_local);
        let basis = world.spawn((basis_local, basis_global, ChildOf(root))).id();

        let mesh_local = Transform {
            translation: mesh_translation,
            scale: mesh_scale,
            ..Default::default()
        };
        let mesh_global = basis_global.mul_transform(mesh_local);
        world.spawn((mesh_local, mesh_global, ChildOf(basis), Mesh3d(mesh_handle)));

        #[allow(clippy::type_complexity)]
        let mut system_state: SystemState<(
            Query<&Children>,
            Query<(&Transform, &GlobalTransform)>,
            RenderPrimitiveQuery,
        )> = SystemState::new(&mut world);
        let (children, transforms, primitives) = system_state.get(&world).unwrap();
        let meshes = world.resource::<Assets<Mesh>>();

        // The old algorithm: compose each descendant's absolute GlobalTransform (which
        // bakes in the root's huge world position), then cancel that position back out by
        // multiplying by the root's inverted absolute GlobalTransform.
        let root_inverse = root_global.affine().inverse();
        let mut old_min = Vec3::splat(f32::INFINITY);
        let mut old_max = Vec3::splat(f32::NEG_INFINITY);
        for descendant in children.iter_descendants(root) {
            let Ok((mesh_handle, _, _)) = primitives.get(descendant) else {
                continue;
            };
            let mesh = meshes.get(mesh_handle).unwrap();
            let aabb = mesh.compute_aabb().unwrap();
            let center = Vec3::from(aabb.center);
            let half_extents = Vec3::from(aabb.half_extents);
            let (_, global) = transforms.get(descendant).unwrap();
            let relative = Mat4::from(root_inverse * global.affine());
            let transformed =
                InstanceBounds::transformed(center - half_extents, center + half_extents, relative);
            old_min = old_min.min(transformed.min);
            old_max = old_max.max(transformed.max);
        }
        let old_error = (old_min - expected_min)
            .abs()
            .max((old_max - expected_max).abs())
            .max_element();
        assert!(
            old_error > tolerance,
            "expected the old world-transform-and-invert composition to exceed tolerance \
             {tolerance} (it should reproduce the acceptance failure), got error \
             {old_error}"
        );

        // The fix, exercised through the real validation function: composing local
        // transforms along the path from the root never forms the large-magnitude matrix,
        // so it validates within tolerance.
        let world_transform = WorldTransform(root_local.to_matrix());
        let summary = validate_spawned_transforms_and_bounds(
            root,
            &root_local,
            &root_global,
            &world_transform,
            Some(&expected_bounds),
            None,
            &children,
            &transforms,
            &primitives,
            meshes,
        )
        .unwrap_or_else(|error| {
            panic!(
                "expected local-transform composition to validate within tolerance {tolerance}: \
                 {error}"
            )
        });
        assert_eq!(summary.nodes, 2);

        // Measure the new method's own error directly (mirroring what
        // `validate_spawned_transforms_and_bounds` computes internally) to report it
        // alongside the old method's.
        let mut new_min = Vec3::splat(f32::INFINITY);
        let mut new_max = Vec3::splat(f32::NEG_INFINITY);
        let mut new_nodes = 0usize;
        let mut new_bounded_meshes = 0usize;
        if let Ok(direct_children) = children.get(root) {
            for child in direct_children.iter() {
                accumulate_relative_bounds(
                    child,
                    Affine3A::IDENTITY,
                    &children,
                    &transforms,
                    &primitives,
                    meshes,
                    &mut new_nodes,
                    &mut new_bounded_meshes,
                    &mut new_min,
                    &mut new_max,
                )
                .unwrap();
            }
        }
        let new_error = (new_min - expected_min)
            .abs()
            .max((new_max - expected_max).abs())
            .max_element();
        assert!(
            new_error <= tolerance,
            "new method exceeded tolerance {tolerance}: error {new_error}"
        );
        eprintln!(
            "bounds precision fixture: tolerance={tolerance}, old_error={old_error}, new_error={new_error}"
        );
    }

    #[test]
    fn a_deeply_nested_model_is_bounded_without_overflowing_the_stack() {
        use bevy::ecs::system::SystemState;

        // 50,000 nested nodes with one unit cube at the leaf: a recursive walk would
        // overflow a test thread's stack long before the leaf.
        const DEPTH: usize = 50_000;
        let mut world = World::new();
        let mut meshes = Assets::<Mesh>::default();
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![[-0.5, -0.5, -0.5], [0.5, 0.5, 0.5], [0.5, -0.5, 0.5]],
        );
        let mesh_handle = meshes.add(mesh);
        world.insert_resource(meshes);

        let root = world
            .spawn((Transform::default(), GlobalTransform::default()))
            .id();
        let mut parent = root;
        for _ in 0..DEPTH {
            parent = world
                .spawn((
                    Transform::default(),
                    GlobalTransform::default(),
                    ChildOf(parent),
                ))
                .id();
        }
        world.spawn((
            Transform::default(),
            GlobalTransform::default(),
            ChildOf(parent),
            Mesh3d(mesh_handle),
        ));

        #[allow(clippy::type_complexity)]
        let mut system_state: SystemState<(
            Query<&Children>,
            Query<(&Transform, &GlobalTransform)>,
            RenderPrimitiveQuery,
        )> = SystemState::new(&mut world);
        let (children, transforms, primitives) = system_state.get(&world).unwrap();
        let meshes = world.resource::<Assets<Mesh>>();

        let mut nodes = 0usize;
        let mut bounded_meshes = 0usize;
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        accumulate_relative_bounds(
            root,
            Affine3A::IDENTITY,
            &children,
            &transforms,
            &primitives,
            meshes,
            &mut nodes,
            &mut bounded_meshes,
            &mut min,
            &mut max,
        )
        .unwrap();
        assert_eq!(nodes, DEPTH + 2);
        assert_eq!(bounded_meshes, 1);
        assert_eq!((min, max), (Vec3::splat(-0.5), Vec3::splat(0.5)));
    }
}
