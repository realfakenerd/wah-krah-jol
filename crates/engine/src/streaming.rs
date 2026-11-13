use crate::nif_material::{
    EffectPaletteDepthSource, EffectPaletteMaterial, NifNativeMaterialSource,
};
use crate::physics::{DebugTankard, PlayerBody, PlayerControlsSystems};
use crate::{
    config::EngineConfig,
    profiling::ProfilingState,
    render::{
        PLACED_OBJECT_RENDER_LAYERS, QUADRANT_WEIGHT_SAMPLES, TerrainExtension, TerrainMaterial,
        WATER_LAYER, WaterExtension, WaterMaterial, WaterReflectionTexture,
    },
    swap_bridge::{SwapSurface, SwapWaiting},
    terrain_merge::{
        CellLookup, TerrainMerge, classes::TerrainClasses, material::MergedTerrainMaterial,
    },
    world::{
        actor_catalog::ActorCatalog,
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, FormId, InstanceBounds,
            MeshHandle, StreamedCellRoot, StreamingCamera, TerrainPatch, WaterSurface,
            WorldPosition, WorldTransform,
        },
        database::{AssetCatalog, CellKey, CellPayload, DatabaseRequest, WorldDatabase},
        water::{FlatWater, FlatWaterBody},
    },
};
use bevy::{
    app::{Propagate, SceneSpawnerSystems},
    asset::{LoadState, RecursiveDependencyLoadState, RenderAssetUsages},
    camera::{primitives::MeshAabb, visibility::RenderLayers},
    ecs::system::SystemParam,
    gltf::{GltfExtras, GltfMaterialName, GltfSceneExtras},
    image::{ImageAddressMode, ImageFilterMode, ImageLoaderSettings, ImageSampler},
    math::Affine3A,
    mesh::{Indices, PrimitiveTopology, VertexAttributeValues},
    prelude::*,
    world_serialization::{WorldInstance, WorldInstanceReady},
};
use bevy_rapier3d::prelude::{
    Collider, ColliderDisabled, MassProperties, RigidBody, WriteRapierContext,
};
use serde::{Deserialize, Serialize};
use shared::collision::{
    BodyKind, COLLISION_ASSET_VERSION, CollisionAsset, CollisionBody, CollisionShape,
};
use shared::lod::LodTier;
use shared::terrain_mesh::TerrainCellMesh;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Instant;

pub(crate) mod admission;
pub(crate) mod control;
pub(crate) mod lod;
#[cfg(test)]
mod merged_tests;
mod priority;
pub(crate) mod requests;
mod reservations;
pub(crate) mod runtime;
mod surface_costs;

// Wall-clock spans can include a short OS scheduler preemption. Keep the raw maximum in metrics,
// but require a material overrun before classifying the frame as a commit-budget violation.
const COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS: u64 = 1_000;

fn commit_budget_exceeded(elapsed_micros: u64, budget_micros: u64) -> bool {
    elapsed_micros > budget_micros.saturating_add(COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS)
}

pub struct StreamingPlugin;

/// Ready terrain collider attached to the same streamed quadrant as its mesh.
#[derive(Component, Debug, Clone, Copy)]
pub struct TerrainCollider;

/// Grid cell and quadrant covered by a terrain surface at one LOD tier.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerrainCoverage {
    grid: IVec2,
    quadrant: u8,
    tier: Option<LodTier>,
}

#[derive(Component)]
struct TerrainSurfaceReady;

/// The full-detail terrain of one exterior cell drawn as a single merged mesh (#215). It covers
/// all four quadrants of `grid`, so the LOD hand-off treats each of them as ready when this
/// entity is.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MergedTerrainCoverage {
    pub(crate) grid: IVec2,
}

/// A merged terrain mesh that has been spawned and waits for the readiness pass.
#[derive(Component)]
struct PendingMergedTerrain;

/// Marks the merged terrain mesh entity of a cell, for the scene gauges.
#[derive(Component)]
pub(crate) struct MergedTerrainCell;

/// The resources `collect_cells` fills a cell's entities from. Grouped because the system would
/// otherwise pass Bevy's limit of sixteen parameters.
#[derive(SystemParam)]
struct CellAssets<'w> {
    meshes: ResMut<'w, Assets<Mesh>>,
    terrain_materials: ResMut<'w, Assets<TerrainMaterial>>,
    water_materials: ResMut<'w, Assets<WaterMaterial>>,
    /// Present when the run installed the merged terrain plugin.
    merge: Option<ResMut<'w, TerrainMerge>>,
}

/// The merged data a cell is drawn from, once it has passed every check and been welded.
struct MergedSpawn {
    cell: Box<TerrainCellMesh>,
    classes: Arc<TerrainClasses>,
    material: Handle<MergedTerrainMaterial>,
    /// Every image `material` binds ([`TerrainMerge::material_images`]): the cell's declaration to
    /// the swap bridge.
    images: Arc<[AssetId<Image>]>,
}

/// A fixed placement's collision provenance.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticColliderSource {
    NifAuthored,
    RenderProxy,
}

impl Plugin for StreamingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<StreamingCommitBudget>()
            .init_resource::<lod::LodStreaming>()
            .init_resource::<DiagnosticFallbackAssets>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<SceneSpawnBatch>()
            .init_resource::<ActiveSpace>()
            .init_resource::<StaticCollisionCache>()
            .init_resource::<admission::SceneAdmission>()
            .init_resource::<requests::SceneSchedulingState>()
            .init_resource::<runtime::StreamingRuntime>()
            .add_plugins(crate::streaming_preparation::StreamingPreparationPlugin)
            .init_resource::<crate::pacing::PacingTracker>()
            .add_observer(mark_world_instance_ready)
            .add_observer(lod::mark_lod_world_instance_ready)
            .add_systems(
                Update,
                (
                    runtime::update_streaming_control,
                    plan_cells,
                    despawn_cells,
                    lod::plan_lod_chunks,
                    collect_cells,
                    lod::collect_lod_chunks,
                    requests::dispatch_scene_requests,
                    arm_pending_models,
                    track_asset_readiness,
                    track_surface_readiness,
                    track_merged_surface_readiness,
                    lod::track_lod_readiness,
                    lod::batch_ready_lod_chunks,
                    lod::update_terrain_lod_visibility,
                    finish_streaming_commit_budget,
                    update_render_origin,
                    validate_streaming_lifecycle,
                    runtime::observe_streaming_ownership,
                )
                    .chain()
                    .after(PlayerControlsSystems),
            )
            // Bevy instantiates every ready converted model in one unbudgeted pass inside
            // `SceneSpawnerSystems::WorldInstanceSpawn`; these two systems bracket that pass so the
            // profile can tell a frame that spawned a batch from one that spawned nothing.
            .add_systems(
                SpawnScene,
                (
                    begin_scene_spawn_batch.before(SceneSpawnerSystems::WorldInstanceSpawn),
                    end_scene_spawn_batch.after(SceneSpawnerSystems::WorldInstanceSpawn),
                ),
            );
        // The world-ready scan walks the whole stream window every frame and only feeds the pacing
        // report, so an ordinary play session installs none of it.
        if app
            .world()
            .get_resource::<crate::config::EngineConfig>()
            .is_some_and(crate::config::EngineConfig::measures_pacing)
        {
            app.add_systems(
                Update,
                crate::pacing::track_world_ready.after(validate_streaming_lifecycle),
            );
        }
    }
}

#[derive(Resource, Default)]
pub struct StreamingWorld {
    generation: u64,
    /// Spawn order for the models [`arm_pending_models`] has yet to arm, so a backlog drains oldest
    /// first whatever order the queries iterate in.
    next_model_sequence: u64,
    cells: HashMap<CellKey, CellStatus>,
    /// Keep each dispatched payload accounted for after its cell leaves the wanted window.
    /// An entry survives channel delivery and deferral until the response is consumed.
    outstanding_cells: HashMap<CellKey, u64>,
    /// The exterior grid square `plan_cells` measured this frame's window against, or `None` when
    /// it had no camera to plan from. The lifecycle check compares against it to tell a root the
    /// planner kept from one that only looks out of range because the camera moved since.
    planned_center: Option<IVec2>,
    /// Responses for exterior cells with terrain that arrived while the merged terrain arrays were
    /// still loading, in arrival order. They are replayed, ahead of new responses, once the merged
    /// path is ready or refused; every other cell is committed without waiting.
    held_responses: std::collections::VecDeque<crate::world::database::DatabaseResponse>,
}

impl StreamingWorld {
    /// The root entity of `key` while it is resident.
    pub(crate) fn resident_root(&self, key: CellKey) -> Option<Entity> {
        match self.cells.get(&key) {
            Some(CellStatus::Resident { root }) => Some(*root),
            _ => None,
        }
    }

    /// Whether the load of `key` failed.
    pub(crate) fn is_failed(&self, key: CellKey) -> bool {
        matches!(self.cells.get(&key), Some(CellStatus::Failed))
    }

    #[cfg(test)]
    pub(crate) fn set_resident_for_test(&mut self, key: CellKey, root: Entity) {
        self.cells.insert(key, CellStatus::Resident { root });
    }

    #[cfg(test)]
    pub(crate) fn set_failed_for_test(&mut self, key: CellKey) {
        self.cells.insert(key, CellStatus::Failed);
    }

    #[cfg(test)]
    pub(crate) fn cell_count_for_test(&self) -> usize {
        self.cells.len()
    }

    /// The number of cells in the stream window of `radius` around `center`, how many of them are
    /// resident (at full detail: a retiring cell inside the window is revived by the planner before
    /// this is read, so it counts as resident), and how many ended in [`CellStatus::Failed`]. A
    /// failed cell (map edge, missing data) never becomes resident and is never retried, so the
    /// world-ready predicate counts it as settled rather than waiting for it forever.
    pub(crate) fn window_residency(
        &self,
        worldspace_id: u32,
        center: IVec2,
        radius: i32,
    ) -> (usize, usize, usize) {
        let radius = radius.max(0);
        let side = 2 * radius as usize + 1;
        let mut resident = 0;
        let mut failed = 0;
        for y in -radius..=radius {
            for x in -radius..=radius {
                let key = CellKey::Exterior {
                    worldspace_id,
                    grid_x: center.x + x,
                    grid_y: center.y + y,
                };
                match self.cells.get(&key) {
                    Some(CellStatus::Resident { .. } | CellStatus::Retiring { .. }) => {
                        resident += 1
                    }
                    Some(CellStatus::Failed) => failed += 1,
                    _ => {}
                }
            }
        }
        (side * side, resident, failed)
    }

    /// Residency of the space the planner currently wants, including a single active interior.
    pub(crate) fn active_residency(
        &self,
        space: &ActiveSpace,
        configured_worldspace: u32,
        center: IVec2,
        radius: i32,
    ) -> (usize, usize, usize) {
        if let Some(cell) = space.interior {
            return match self.cells.get(&CellKey::Interior(cell)) {
                Some(CellStatus::Resident { .. } | CellStatus::Retiring { .. }) => (1, 1, 0),
                Some(CellStatus::Failed) => (1, 0, 1),
                _ => (1, 0, 0),
            };
        }
        self.window_residency(
            space.exterior_worldspace(configured_worldspace),
            center,
            radius,
        )
    }

    /// Submits one load for `key` unless it is already loading or resident, and records the
    /// request on the streaming metrics. This is the loader path every cell goes through, however
    /// the request is driven: the camera planner streams exteriors from it, and the streaming
    /// fixture and a load-door crossing load an interior from it by id.
    pub(crate) fn request_cell(
        &mut self,
        database: &WorldDatabase,
        key: CellKey,
        metrics: &mut StreamingMetrics,
        profiler: &mut ProfilingState,
    ) -> bool {
        if self.cells.contains_key(&key) || self.outstanding_cells.contains_key(&key) {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if database
            .try_request(DatabaseRequest::Load {
                generation,
                key,
                queued_at: Instant::now(),
            })
            .is_ok()
        {
            metrics.requests_submitted += 1;
            profiler.increment("streaming/requests", 1);
            profiler.event(format!("{key:?}"), "requested", None);
            self.cells.insert(key, CellStatus::Loading { generation });
            self.outstanding_cells.insert(key, generation);
            return true;
        }
        false
    }

    fn consume_cell_response(&mut self, key: CellKey, generation: u64) {
        if self.outstanding_cells.get(&key) == Some(&generation) {
            self.outstanding_cells.remove(&key);
        }
    }

    /// Whether a response is for the load still wanted under its generation. A cell that left the
    /// active space or the unload radius while its response was in flight is not.
    fn awaits(&self, key: CellKey, generation: u64) -> bool {
        matches!(
            self.cells.get(&key),
            Some(CellStatus::Loading { generation: current }) if *current == generation
        )
    }
}

#[derive(Resource)]
struct StreamingCommitBudget {
    frame_started: Instant,
    remaining: usize,
    commits: usize,
    lod_priority: bool,
    reserved_for_lod: bool,
    prefer_deferred_response: bool,
}

impl Default for StreamingCommitBudget {
    fn default() -> Self {
        Self {
            frame_started: Instant::now(),
            remaining: 0,
            commits: 0,
            lod_priority: false,
            reserved_for_lod: false,
            prefer_deferred_response: true,
        }
    }
}

impl StreamingCommitBudget {
    fn reserve_for_lod(&mut self, waiting: bool, frame_limit: usize) {
        self.reserved_for_lod = waiting && (self.lod_priority || frame_limit > 1);
    }

    fn remaining_for_cells(&self) -> usize {
        self.remaining
            .saturating_sub(usize::from(self.reserved_for_lod))
    }

    fn elapsed_limit_reached(&self, runtime: Option<&runtime::StreamingRuntime>) -> bool {
        self.commits != 0
            && runtime
                .and_then(|runtime| runtime.decision)
                .is_some_and(|decision| {
                    self.frame_started.elapsed().as_micros()
                        >= u128::from(decision.budgets.max_commit_micros)
                })
    }
}

#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct StreamingMetrics {
    pub requests_submitted: u64,
    pub responses_received: u64,
    pub stale_responses: u64,
    pub failed_cells: u64,
    pub unloaded_cells: u64,
    /// Cell roots despawned in the most recent frame, and the largest value that counter reached.
    pub despawns_this_frame: u64,
    pub max_despawns_per_frame: u64,
    /// Entities removed by those despawns, counted over each root's whole subtree.
    pub despawned_entities: u64,
    /// Converted models Bevy instantiated in the most recent frame, and the largest value that
    /// counter reached. An instance is counted on the frame its entities are written into the
    /// world, which is the frame the spawn batch's cost lands on.
    pub instances_spawned_this_frame: u64,
    pub max_instances_spawned_per_frame: u64,
    /// Models whose converted scene is loaded but which have not been handed to the spawner yet,
    /// waiting for their turn in the arming budget, and the largest backlog seen.
    pub arming_queue_depth: usize,
    pub peak_arming_queue_depth: usize,
    /// Models handed to Bevy's spawner in the most recent frame, and the largest value that counter
    /// reached. While the arming pacer is unlimited this is the frame a cell commits; once models
    /// are armed only when their asset is loaded, it is the frame the instance spawns.
    pub instances_armed_this_frame: u64,
    pub max_instances_armed_per_frame: u64,
    /// Model instances the readiness scan validated to completion in its most recent run, and the
    /// largest value that counter reached.
    pub instances_completed_this_scan: u64,
    pub max_instances_completed_per_scan: u64,
    /// Cells outside the unload radius that are waiting for their turn in the unload budget, and
    /// the largest backlog seen.
    pub retiring_cells: usize,
    pub peak_retiring_cells: usize,
    /// Retiring cells that came back into range before their turn and kept the root they had.
    pub revived_cells: u64,
    /// Frames where the backlog passed [`retire_backlog_bound`] and every retiring cell was
    /// unloaded at once.
    pub retire_backlog_overflows: u64,
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
    /// Exterior cells drawn as one merged mesh with the shared terrain material (#215).
    pub terrain_cells_merged: u64,
    /// Terrain cells drawn by the four-quadrant path: every cell when the merged path is absent
    /// or refused, and the cells the merged data could not serve otherwise.
    pub terrain_cells_fallback: u64,
    /// The subset of [`Self::terrain_cells_fallback`] where merged data was expected: the chunk
    /// file or the cell's entry is missing, or the entry failed validation.
    pub terrain_cells_fallback_expected_merged: u64,
    /// The subset of those whose merged data exists but failed validation, which is the one worth
    /// a look.
    pub terrain_cells_merged_invalid: u64,
    /// Where the merged terrain path stands: absent, awaiting device, loading arrays, ready or
    /// refused with its reason.
    pub terrain_merge_state: String,
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
    /// Fixed placements represented by NIF-authored Havok shapes.
    pub static_colliders_authored: u64,
    /// Authored colliders that also contain shape families this extractor could not decode.
    pub static_colliders_partial: u64,
    /// Fixed placements represented by a legacy render-triangle proxy.
    pub static_colliders_proxy: u64,
    pub resident_static_colliders: usize,
    /// NIF models that explicitly have no physical collision.
    pub static_colliders_authored_absent: u64,
    /// Placements whose record type is outside every collision tier while their model does carry
    /// authored collision. Counted per placement, not per model, so a repeated model counts each
    /// time it is placed.
    pub static_colliders_excluded_authored: u64,
    /// Eligible fixed placements for which no supported collision could be made.
    pub static_colliders_skipped: u64,
    /// Placed clutter references simulated as dynamic Rapier bodies from authored physics data.
    pub dynamic_clutter_bodies: u64,
    /// Dynamic clutter bodies whose non-convex shapes were replaced by one convex hull.
    pub dynamic_clutter_convexified: u64,
    /// Dynamic clutter placements that could not be built (no hull, invalid shape); the reference
    /// keeps no body and never falls back to a fixed collider.
    pub dynamic_clutter_skipped: u64,
    /// Dynamic clutter placements refused because [`crate::physics::MAX_LIVE_DYNAMIC_CLUTTER`]
    /// bodies were already live; they keep no collider, like movable types before this slice.
    pub dynamic_clutter_overflow: u64,
    /// Dynamic bodies beyond a model's first, which are not simulated (one body per reference).
    pub dynamic_clutter_extra_bodies_ignored: u64,
    /// Dynamic clutter bodies whose authored mass or inertia tensor was unusable, so collider
    /// density supplied the mass instead.
    pub dynamic_clutter_mass_fallbacks: u64,
    /// Dynamic clutter bodies clamped to their authored maximum speed, summed over frames (a body
    /// clamped on five frames counts five).
    pub dynamic_clutter_clamped: u64,
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
    pub lod_queries_submitted: u64,
    pub lod_query_responses: u64,
    pub lod_query_submission_failures: u64,
    pub stale_lod_query_responses: u64,
    pub failed_lod_queries: u64,
    pub unrecovered_lod_queries: usize,
    pub pending_lod_queries: usize,
    pub lod_chunks_requested: u64,
    pub lod_chunks_ready: u64,
    pub failed_lod_chunks: u64,
    pub unrecovered_lod_chunks: usize,
    pub resident_lod_chunks: usize,
    pub pending_lod_chunks: usize,
    pub ready_lod_terrain_patches: u64,
    pub visible_lod_terrain_patches: usize,
    pub total_lod_query_micros: u64,
    pub max_lod_query_micros: u64,
    pub resident_roots: usize,
    pub duplicate_cell_roots: u64,
    pub orphaned_cell_roots: u64,
    pub missing_cell_roots: u64,
    pub out_of_range_cell_roots: u64,
    /// Peak count of the roots `out_of_range_cell_roots` would count if it measured against the
    /// centre `plan_cells` used instead of the camera read at check time. Diagnostic only: it is
    /// not part of `streaming_invariant_failures` and gates nothing.
    pub out_of_range_vs_planned_roots: u64,
    /// Largest per-axis distance, in cells, between the planner's centre and the check's centre.
    pub max_plan_check_center_offset: i32,
    /// Lifecycle checks that ran without a centre from `plan_cells` because it had no camera.
    pub invariant_checks_without_plan: u64,
    pub streaming_invariant_failures: u64,
    pub origin_rebases: u64,
    pub streaming_fixture_validated: bool,
    pub streaming_fixture_failures: u64,
    pub physics_fixture_validated: bool,
    pub physics_fixture_failures: u64,
    pub asset_failures: AssetFailureLog,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetFailure {
    pub model_path: String,
    pub reference_form_id: u32,
    pub base_form_id: u32,
    pub cell_id: u32,
    pub dependency_chain: Vec<String>,
}

/// How many recent [`AssetFailure`] records [`StreamingMetrics`] retains.
///
/// A failure is recorded per failing reference per cell commit, and a cell committed again
/// records the same failure again, so the count grows with play time rather than with content.
/// The collector bounds what it keeps; [`AssetFailureLog::total`] keeps the exact number
/// recorded and the bug report exports the newest entries plus that total, so the bound hides
/// no count. The ring is well above the bug report's export limit so every export the report
/// can show has retained detail behind it.
pub const MAX_ASSET_FAILURES: usize = 256;

/// The newest [`MAX_ASSET_FAILURES`] asset failures, plus the exact number ever recorded.
///
/// [`Self::push`] is the only way in and applies the bound itself, so no call site can grow
/// the log without bound. Serialized as the retained failures alone (the shape the profiling
/// and comparison scripts read); the exact count is published by the bug report through
/// [`Self::total`].
#[derive(Debug, Clone, Default)]
pub struct AssetFailureLog {
    /// Newest last; `push` evicts the oldest, making this a ring.
    recent: VecDeque<AssetFailure>,
    total: u64,
}

impl AssetFailureLog {
    /// Records one failure, keeping the newest [`MAX_ASSET_FAILURES`] and the exact total.
    pub fn push(&mut self, failure: AssetFailure) {
        self.total = self.total.saturating_add(1);
        self.recent.push_back(failure);
        if self.recent.len() > MAX_ASSET_FAILURES {
            self.recent.pop_front();
        }
    }

    /// Every failure ever recorded through this log, including ones no longer retained.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Retained failures; never more than [`MAX_ASSET_FAILURES`].
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }

    /// Oldest retained failure first; `.rev()` yields newest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &AssetFailure> {
        self.recent.iter()
    }

    /// The oldest retained failure.
    pub fn first(&self) -> Option<&AssetFailure> {
        self.recent.front()
    }
}

impl Serialize for AssetFailureLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.recent.serialize(serializer)
    }
}

#[derive(Resource, Default)]
struct DiagnosticFallbackAssets {
    mesh: Option<Handle<Mesh>>,
    material: Option<Handle<StandardMaterial>>,
}

#[derive(Resource, Default)]
struct StaticCollisionCache(HashMap<(Option<String>, String), StaticCollisionDecision>);

#[derive(Clone)]
enum StaticCollisionDecision {
    Authored(Vec<AuthoredColliderPart>, Vec<String>),
    RenderProxy(Collider),
    AuthoredAbsent,
    LegacyExcluded,
    Skipped(String),
    /// A movable-type model with an authored dynamic body (#104 phase a).
    Clutter(Box<ClutterSpec>),
    /// A movable-type model with no dynamic body: it keeps no collision, as before.
    ClutterNone,
    /// An ineligible record type whose model does carry authored collision.
    ExcludedWithAuthored,
    /// A movable-type model whose dynamic body could not be built. It gets no collider at all.
    ClutterSkipped(String),
}

/// Colliders and dynamics for one placed dynamic clutter model.
#[derive(Clone)]
struct ClutterSpec {
    parts: Vec<AuthoredColliderPart>,
    body: CollisionBody,
    /// The body's non-convex shapes were replaced by one convex hull.
    convexified: bool,
    /// Further dynamic bodies in the same model, which phase (a) does not simulate.
    ignored_bodies: usize,
    /// `None` when the authored mass or tensor is unusable; density supplies the mass instead.
    mass: Option<MassProperties>,
}

#[derive(Clone)]
struct AuthoredColliderPart {
    translation: Vec3,
    collider: Collider,
}

#[derive(Resource, Default)]
pub(crate) struct TerrainContinuity {
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
    Loading {
        generation: u64,
    },
    Resident {
        root: Entity,
    },
    /// Outside the unload radius, waiting for its turn in the unload budget. The map entry and the
    /// root stay, so a cell that comes back into range before its turn is revived as it is instead
    /// of being despawned and requested again. A retiring cell is still drawn until its turn; that
    /// is harmless while the camera stays in one worldspace (retiring cells are the far ones), but a
    /// future runtime worldspace change must unload them at once rather than pace them.
    Retiring {
        root: Entity,
    },
    Failed,
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct RenderOrigin(pub IVec2);

/// The space the player is in. The planner streams only this space: the active interior when
/// there is one, otherwise the exterior cells around the camera in the active worldspace. A load
/// door crossing (`crate::door_crossing`) changes it; nothing else does, so the default (no
/// interior, the configured worldspace) is the behaviour before doors existed.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActiveSpace {
    /// `None` means the worldspace the engine was configured with.
    pub worldspace_id: Option<u32>,
    /// `Some(cell_id)` while the player is inside that interior cell.
    pub interior: Option<u32>,
}

impl ActiveSpace {
    pub fn exterior_worldspace(&self, configured: u32) -> u32 {
        self.worldspace_id.unwrap_or(configured)
    }
}

/// The cells the planner wants resident: the active interior alone, or the exterior window of
/// `radius` cells around `center` in the active worldspace.
fn wanted_cells(
    space: &ActiveSpace,
    configured_worldspace: u32,
    center: IVec2,
    radius: i32,
) -> HashSet<CellKey> {
    if let Some(cell_id) = space.interior {
        return HashSet::from([CellKey::Interior(cell_id)]);
    }
    let worldspace_id = space.exterior_worldspace(configured_worldspace);
    let mut wanted = HashSet::new();
    for y in -radius..=radius {
        for x in -radius..=radius {
            wanted.insert(CellKey::Exterior {
                worldspace_id,
                grid_x: center.x + x,
                grid_y: center.y + y,
            });
        }
    }
    wanted
}

/// Whether a loaded cell belongs to the active space at all. A cell of another space (left behind
/// by a crossing) is outside it whatever its grid square.
fn cell_in_active_space(key: CellKey, space: &ActiveSpace, configured_worldspace: u32) -> bool {
    match key {
        CellKey::Exterior { worldspace_id, .. } => {
            space.interior.is_none()
                && worldspace_id == space.exterior_worldspace(configured_worldspace)
        }
        CellKey::Interior(cell_id) => space.interior.is_none_or(|active| active == cell_id),
    }
}

/// Unloads every cell at once, unpaced: the space the player left must not be drawn or collide
/// for another frame. Entries still loading are dropped, so their responses turn stale.
pub(crate) fn unload_all_cells_now(world: &mut World) -> usize {
    let started = Instant::now();
    let cells: Vec<(CellKey, CellStatus)> = world
        .resource_mut::<StreamingWorld>()
        .cells
        .drain()
        .collect();
    let mut entities = 0usize;
    let mut unloaded = 0u64;
    for (key, status) in &cells {
        if let CellStatus::Resident { root } | CellStatus::Retiring { root } = status {
            entities = entities.saturating_add(despawn_subtree(world, *root));
            unloaded += 1;
        }
        world.resource_mut::<TerrainContinuity>().edges.remove(key);
    }
    {
        let mut metrics = world.resource_mut::<StreamingMetrics>();
        metrics.unloaded_cells = metrics.unloaded_cells.saturating_add(unloaded);
        metrics.despawned_entities = metrics.despawned_entities.saturating_add(entities as u64);
        metrics.resident_cells = 0;
        metrics.loading_cells = 0;
        metrics.retiring_cells = 0;
    }
    let mut profiler = world.resource_mut::<ProfilingState>();
    profiler.record_elapsed("streaming/space_switch_unload", started);
    profiler.increment("streaming/despawned_cells", unloaded);
    cells.len()
}

#[allow(clippy::too_many_arguments)]
fn plan_cells(
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    space: Res<ActiveSpace>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut streaming: ResMut<StreamingWorld>,
    mut commit_budget: ResMut<StreamingCommitBudget>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
    mut runtime: Option<ResMut<runtime::StreamingRuntime>>,
) {
    crate::perf_span::perf_span!("streaming/plan_cells");
    let plan_started = Instant::now();
    commit_budget.remaining = runtime
        .as_ref()
        .and_then(|runtime| runtime.decision)
        .map_or(config.max_cell_commits_per_frame, |decision| {
            decision.budgets.max_cell_commits
        });
    commit_budget.commits = 0;
    commit_budget.lod_priority = !commit_budget.lod_priority;
    commit_budget.reserved_for_lod = false;
    let Ok(camera) = camera.single() else {
        streaming.planned_center = None;
        return;
    };
    // Screenshot runs frame the configured start cell from an artistic
    // offset, so the camera can sit cells away from its target. Streaming
    // around the camera would then load the wrong neighborhood and leave
    // the framed cells empty; anchor on the start cell instead. Interactive
    // runs keep following the camera.
    let center = window_center(&config, camera.translation, origin.0);
    streaming.planned_center = Some(center);
    let wanted = wanted_cells(&space, config.worldspace_id, center, config.stream_radius);
    let mut ordered: Vec<_> = wanted.iter().copied().collect();
    let controlled = config.streaming_controls_enabled();
    if config.prioritize_streaming || controlled {
        ordered.sort_by_key(|key| match *key {
            CellKey::Exterior { grid_x, grid_y, .. } => {
                let dx = i64::from(grid_x) - i64::from(center.x);
                let dy = i64::from(grid_y) - i64::from(center.y);
                (dx * dx + dy * dy, cell_order_key(*key))
            }
            _ => (0, cell_order_key(*key)),
        });
    }
    let request_limit = if controlled {
        commit_budget.remaining
    } else {
        usize::MAX
    };
    let outstanding_limit = if controlled {
        match config.streaming_backlog_limit() {
            0 => 256,
            limit => limit,
        }
    } else {
        usize::MAX
    };
    let mut submitted = 0;
    for key in &ordered {
        if submitted >= request_limit || streaming.outstanding_cells.len() >= outstanding_limit {
            break;
        }
        // A retiring cell is still in the map, so `request_cell` never requests it a second time:
        // the retain pass below revives it with the root it kept.
        if !streaming.cells.contains_key(key) && !streaming.outstanding_cells.contains_key(key) {
            if let Some(runtime) = runtime.as_mut()
                && (!runtime.cell_requests_allowed()
                    || !runtime.reserve_cell(*key, config.interactive_world_physics()))
            {
                continue;
            }
            if streaming.request_cell(&database, *key, &mut metrics, &mut profiler) {
                submitted += 1;
            } else {
                // A full worker queue is retryable. Stop before reserving more unsubmitted cells.
                break;
            }
        }
    }
    let mut revived = 0u64;
    streaming.cells.retain(|key, status| {
        if cell_in_active_space(*key, &space, config.worldspace_id)
            && cell_within_unload_radius(*key, center, config.unload_radius)
        {
            if let CellStatus::Retiring { root } = status {
                *status = CellStatus::Resident { root: *root };
                revived += 1;
            }
            return true;
        }
        match status {
            CellStatus::Resident { root } => {
                *status = CellStatus::Retiring { root: *root };
                true
            }
            // Already waiting for its turn in the budget.
            CellStatus::Retiring { .. } => true,
            // Nothing was spawned, so there is no subtree to pace: drop the entry now and let the
            // in-flight response turn into a stale one.
            CellStatus::Loading { .. } | CellStatus::Failed => {
                continuity.edges.remove(key);
                profiler.event(format!("{key:?}"), "unload_dropped", None);
                false
            }
        }
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
    metrics.retiring_cells = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Retiring { .. }))
        .count();
    metrics.peak_resident_cells = metrics.peak_resident_cells.max(metrics.resident_cells);
    metrics.peak_loading_cells = metrics.peak_loading_cells.max(metrics.loading_cells);
    metrics.peak_retiring_cells = metrics.peak_retiring_cells.max(metrics.retiring_cells);
    if revived > 0 {
        metrics.revived_cells = metrics.revived_cells.saturating_add(revived);
        profiler.increment("streaming/revived_cells", revived);
    }
    profiler.set_gauge("streaming/resident_cells", metrics.resident_cells as f64);
    profiler.set_gauge("streaming/loading_cells", metrics.loading_cells as f64);
    profiler.set_gauge("streaming/retiring_cells", metrics.retiring_cells as f64);
    profiler.record_elapsed("streaming/plan_cells", plan_started);
}

/// How many cells may wait for their turn in the unload budget before the pacer gives up on pacing
/// and unloads every one of them in a single frame.
///
/// One whole window at the unload radius, `(2 * r + 1)^2` cells: the most a single crossing or
/// teleport can retire at once. Both are paced, so the overflow path only fires when sustained
/// flight keeps retiring cells faster than the budget unloads them, the case where holding on to
/// the backlog costs more than one larger frame.
fn retire_backlog_bound(unload_radius: i32) -> usize {
    let side = 2 * unload_radius.max(0) as usize + 1;
    side * side
}

/// Stable order for the unload budget, so a replayed crossing unloads the same cells on the same
/// frames whatever order the status map iterates in.
fn cell_order_key(key: CellKey) -> (u32, i32, i32, u32) {
    match key {
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => (0, grid_y, grid_x, worldspace_id),
        CellKey::Interior(cell_id) => (1, 0, 0, cell_id),
    }
}

/// Unloads retiring cells, at most `max_cell_unloads_per_frame` of them per frame.
///
/// This is an exclusive system on purpose: a command-buffered `despawn` only queues the removal and
/// the real work happens later at a schedule sync point, where no span can see it — and the whole
/// point of the budget is to keep that work off one frame.
fn despawn_cells(world: &mut World) {
    let (budget, backlog_bound) = {
        let config = world.resource::<EngineConfig>();
        (
            // `0` is unbudgeted: every retiring cell unloads at once, as before the budget.
            match config.max_cell_unloads_per_frame {
                0 => usize::MAX,
                budget => budget,
            },
            retire_backlog_bound(config.unload_radius),
        )
    };
    let mut retiring: Vec<(CellKey, Entity)> = world
        .resource::<StreamingWorld>()
        .cells
        .iter()
        .filter_map(|(key, status)| match status {
            CellStatus::Retiring { root } => Some((*key, *root)),
            _ => None,
        })
        .collect();
    if retiring.is_empty() {
        world.resource_mut::<StreamingMetrics>().despawns_this_frame = 0;
        world
            .resource_mut::<ProfilingState>()
            .set_gauge("streaming/despawns_this_frame", 0.0);
        return;
    }
    retiring.sort_by_key(|(key, _)| cell_order_key(*key));
    let backlog = retiring.len();
    let overflow = backlog > backlog_bound;
    let budget = if overflow {
        backlog
    } else {
        budget.min(backlog)
    };
    let started = Instant::now();
    let mut entities = 0usize;
    for (_, root) in retiring.iter().take(budget) {
        entities = entities.saturating_add(despawn_subtree(world, *root));
    }
    {
        let mut streaming = world.resource_mut::<StreamingWorld>();
        for (key, _) in retiring.iter().take(budget) {
            streaming.cells.remove(key);
        }
    }
    for (key, _) in retiring.iter().take(budget) {
        world.resource_mut::<TerrainContinuity>().edges.remove(key);
        world
            .resource_mut::<ProfilingState>()
            .event(format!("{key:?}"), "unloaded", None);
    }
    let cells = budget as u64;
    let remaining = backlog - budget;
    let max_despawns_per_frame = {
        let mut metrics = world.resource_mut::<StreamingMetrics>();
        metrics.retiring_cells = remaining;
        metrics.peak_retiring_cells = metrics.peak_retiring_cells.max(backlog);
        metrics.unloaded_cells = metrics.unloaded_cells.saturating_add(cells);
        metrics.despawns_this_frame = cells;
        metrics.despawned_entities = metrics.despawned_entities.saturating_add(entities as u64);
        metrics.max_despawns_per_frame = metrics.max_despawns_per_frame.max(cells);
        if overflow {
            metrics.retire_backlog_overflows = metrics.retire_backlog_overflows.saturating_add(1);
        }
        metrics.max_despawns_per_frame
    };
    let mut profiler = world.resource_mut::<ProfilingState>();
    profiler.record_elapsed("streaming/cell_despawn", started);
    profiler.increment("streaming/despawned_cells", cells);
    profiler.increment("streaming/despawned_entities", entities as u64);
    profiler.set_gauge("streaming/despawns_this_frame", cells as f64);
    profiler.set_gauge(
        "streaming/max_despawns_per_frame",
        max_despawns_per_frame as f64,
    );
    profiler.set_gauge("streaming/retiring_cells", remaining as f64);
    if overflow {
        profiler.increment("streaming/retire_backlog_overflows", 1);
        profiler.event("streaming", "retire_backlog_overflow", Some(backlog as f64));
        warn!(
            backlog,
            bound = backlog_bound,
            "retire backlog exceeded its bound; unloading every retiring cell at once"
        );
    }
}

/// Despawns a cell root and everything under it, returning the number of entities that removes.
///
/// The count walks `Children` first, so the batch span covers the accounting as well as the
/// recursive removal. The walk is only paid on frames that unload cells.
fn despawn_subtree(world: &mut World, root: Entity) -> usize {
    let mut entities = 0usize;
    let mut stack = vec![root];
    while let Some(entity) = stack.pop() {
        entities += 1;
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter());
        }
    }
    if let Ok(entity) = world.get_entity_mut(root) {
        entity.despawn();
    }
    entities
}

/// Collect database responses, reject stale generations and commit cells within the shared budget.
#[allow(clippy::too_many_arguments)]
fn collect_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    cache: Res<CellCache>,
    origin: Res<RenderOrigin>,
    asset_server: Res<AssetServer>,
    catalog: Res<AssetCatalog>,
    actor_catalog: Res<ActorCatalog>,
    reflection: Res<WaterReflectionTexture>,
    mut assets: CellAssets,
    mut streaming: ResMut<StreamingWorld>,
    mut commit_budget: ResMut<StreamingCommitBudget>,
    mut continuity: ResMut<TerrainContinuity>,
    (mut metrics, mut runtime): (
        ResMut<StreamingMetrics>,
        Option<ResMut<runtime::StreamingRuntime>>,
    ),
    mut profiler: ResMut<ProfilingState>,
) {
    // Merged terrain decides cell by cell between its own mesh and the quadrant path, so an
    // exterior cell with terrain is not committed until the arrays it depends on are ready or
    // refused (the setup is bounded by `terrain_merge::SETUP_TIMEOUT`). Interiors and cells with
    // no LAND never wait.
    let holding = assets
        .merge
        .as_deref()
        .is_some_and(TerrainMerge::holds_cells);
    let frame_commit_started = Instant::now();
    commit_budget.frame_started = frame_commit_started;
    release_stale_held_responses(&mut streaming, &mut metrics, &mut profiler);
    let response_scan_limit = config.max_cell_commits_per_frame.saturating_mul(8).max(8);
    // Retry each previously deferred response once. Responses refused during
    // this scan wait until the next frame, leaving room for other arrivals.
    let mut deferred_remaining = runtime
        .as_ref()
        .map_or(0, |runtime| runtime.deferred_responses.len());
    for _ in 0..response_scan_limit {
        if commit_budget.remaining_for_cells() == 0 {
            break;
        }
        if commit_budget.elapsed_limit_reached(runtime.as_deref()) {
            break;
        }
        let mut take_deferred = || {
            if deferred_remaining == 0 {
                return None;
            }
            deferred_remaining -= 1;
            runtime
                .as_mut()
                .and_then(|runtime| runtime.deferred_responses.pop_front())
                .map(|response| (response, true))
        };
        // Held responses were counted when they first arrived, and replay ahead of new
        // responses once the merged path is no longer holding cells.
        let replayed = if holding {
            None
        } else {
            streaming.held_responses.pop_front()
        };
        let response = if let Some(response) = replayed {
            Some((response, true))
        } else if commit_budget.prefer_deferred_response {
            take_deferred().or_else(|| database.try_response().map(|response| (response, false)))
        } else {
            database
                .try_response()
                .map(|response| (response, false))
                .or_else(take_deferred)
        };
        let Some((response, was_deferred)) = response else {
            break;
        };
        // Persist the opposite source preference even when this frame can
        // commit only one cell. Empty sources fall back to the other queue.
        commit_budget.prefer_deferred_response = !was_deferred;
        if !was_deferred {
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
            profiler.record_completed_latency_micros(
                "streaming/db_queue_wait",
                response.queue_wait_micros,
            );
            profiler.record_completed_latency_micros("streaming/db_query", response.query_micros);
            profiler.record_completed_latency_micros(
                "streaming/db_request_total",
                response.total_request_micros,
            );
            // A response for a load no longer wanted is stale; it must not wait for setup.
            if !streaming.awaits(response.key, response.generation) {
                streaming.consume_cell_response(response.key, response.generation);
                metrics.stale_responses += 1;
                profiler.event(format!("{:?}", response.key), "stale_discarded", None);
                continue;
            }
            if holding
                && let Some(merge) = assets.merge.as_deref_mut()
                && should_hold_response(merge, &response.result, |id| cache.has_terrain(id))
            {
                profiler.increment("terrain/cells_held", 1);
                streaming.held_responses.push_back(response);
                continue;
            }
        }
        let Some(CellStatus::Loading { generation }) = streaming.cells.get(&response.key) else {
            streaming.consume_cell_response(response.key, response.generation);
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_discarded", None);
            continue;
        };
        if *generation != response.generation {
            streaming.consume_cell_response(response.key, response.generation);
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_generation", None);
            continue;
        }
        // Validate generation before reserving or retaining returned metadata.
        if let Some(runtime) = runtime.as_mut()
            && let Ok(payload) = &response.result
        {
            let terrain = cache.terrain(payload.cell_id);
            let paths = surface_costs::surface_resource_paths(terrain.as_ref(), true, &catalog);
            if !runtime.reserve_cell(response.key, config.interactive_world_physics())
                || !runtime.reserve_cell_textures(response.key, paths)
            {
                runtime.deferred_responses.push_back(response);
                continue;
            }
        }
        streaming.consume_cell_response(response.key, response.generation);
        commit_budget.remaining -= 1;
        commit_budget.commits = commit_budget.commits.saturating_add(1);
        let commit_started = std::time::Instant::now();
        match response.result {
            Ok(payload) => {
                let mut terrain = cache.terrain(payload.cell_id);
                // A cell with a snapshot is drawn from merged data when the merged path is ready
                // and the cell's own data passes every check; otherwise by quadrant, counted.
                let mut merged = terrain.as_ref().and_then(|_| {
                    choose_merged_terrain(
                        assets.merge.as_deref_mut(),
                        payload.key,
                        payload.cell_id,
                        &mut metrics,
                        &mut profiler,
                    )
                });
                if let Some(terrain) = &mut terrain {
                    let validation = settle_cell_terrain(
                        payload.key,
                        payload.cell_id,
                        merged.take(),
                        terrain,
                        &catalog,
                        &mut continuity,
                        &mut metrics,
                        &mut profiler,
                    );
                    let validation = validation.map(|settled| merged = settled);
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
                let cell_id = payload.cell_id;
                let drawn_merged = merged.is_some();
                let root = spawn_cell(
                    &mut commands,
                    &asset_server,
                    &catalog,
                    &actor_catalog,
                    &reflection,
                    &mut assets.meshes,
                    &mut assets.terrain_materials,
                    &mut assets.water_materials,
                    origin.0,
                    config.lights,
                    config.interactive_world_physics(),
                    requests::enabled(&config),
                    payload,
                    terrain,
                    merged,
                    &mut streaming.next_model_sequence,
                    &mut profiler,
                );
                match root {
                    Ok(root) => {
                        if drawn_merged {
                            metrics.terrain_cells_merged =
                                metrics.terrain_cells_merged.saturating_add(1);
                            profiler.increment("terrain/cells_merged", 1);
                        }
                        if let Some(runtime) = runtime.as_mut() {
                            runtime.bind_cell(response.key, root);
                        }
                        streaming
                            .cells
                            .insert(response.key, CellStatus::Resident { root });
                    }
                    Err(reason) => {
                        error!(cell = format_args!("{cell_id:08X}"), %reason, "terrain collider build failed");
                        metrics.failed_cells = metrics.failed_cells.saturating_add(1);
                        metrics.terrain_validation_failures =
                            metrics.terrain_validation_failures.saturating_add(1);
                        metrics.asset_failures.push(AssetFailure {
                            model_path: format!("terrain/{cell_id:08X}"),
                            reference_form_id: 0,
                            base_form_id: 0,
                            cell_id,
                            dependency_chain: vec![reason],
                        });
                        streaming.cells.insert(response.key, CellStatus::Failed);
                    }
                }
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
        profiler.record_micros("streaming/cell_commit", commit_micros);
        profiler.event(
            format!("{:?}", response.key),
            "committed",
            Some(commit_micros as f64 / 1000.0),
        );
    }
}

fn finish_streaming_commit_budget(
    config: Res<EngineConfig>,
    budget: Res<StreamingCommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
    mut runtime: Option<ResMut<runtime::StreamingRuntime>>,
) {
    if let Some(runtime) = runtime.as_mut() {
        runtime.previous_commit_ms = budget.frame_started.elapsed().as_secs_f64() * 1000.0;
    }
    if budget.commits == 0 {
        return;
    }
    let frame_micros = budget
        .frame_started
        .elapsed()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64;
    metrics.commit_frames = metrics.commit_frames.saturating_add(1);
    metrics.total_frame_commit_micros = metrics
        .total_frame_commit_micros
        .saturating_add(frame_micros);
    metrics.max_frame_commit_micros = metrics.max_frame_commit_micros.max(frame_micros);
    metrics.commit_budget_micros = config.max_commit_micros_per_frame;
    if commit_budget_exceeded(frame_micros, metrics.commit_budget_micros) {
        metrics.commit_budget_violations = metrics.commit_budget_violations.saturating_add(1);
        profiler.event(
            "streaming",
            "commit_budget_exceeded",
            Some(frame_micros as f64 / 1_000.0),
        );
    }
    profiler.set_gauge("streaming/commits_this_frame", budget.commits as f64);
    profiler.record_micros("streaming/frame_commit", frame_micros);
}

/// The centre of the stream window: the camera's grid square, or the start cell for screenshot
/// runs, which frame it from an offset (see `plan_cells`).
pub(crate) fn window_center(config: &EngineConfig, translation: Vec3, origin: IVec2) -> IVec2 {
    if config.acceptance_screenshot.is_some() {
        origin
    } else {
        streaming_center(translation, origin)
    }
}

/// The exterior grid square the camera is over: its rebased translation, put back through the
/// render origin. The planner streams from this square and the lifecycle checks measure against
/// it, so both read the same one.
pub(crate) fn streaming_center(translation: Vec3, origin: IVec2) -> IVec2 {
    let global_x = translation.x + origin.x as f32 * CELL_SIZE;
    let global_y = -translation.z + origin.y as f32 * CELL_SIZE;
    IVec2::new(
        (global_x / CELL_SIZE).floor() as i32,
        (global_y / CELL_SIZE).floor() as i32,
    )
}

/// Whole cells `update_render_origin` moves the origin by for a camera at `translation`.
fn rebase_shift(translation: Vec3) -> IVec2 {
    IVec2::new(
        (translation.x / CELL_SIZE).trunc() as i32,
        (-translation.z / CELL_SIZE).trunc() as i32,
    )
}

/// Whether a loaded cell stays loaded. Exteriors fall out of the radius the camera carries; an
/// interior has no grid square to fall out of, so it stays until something unloads it (a load door
/// crossing unloads every cell of the space it leaves, see [`unload_all_cells_now`]).
fn cell_within_unload_radius(key: CellKey, center: IVec2, radius: i32) -> bool {
    match key {
        CellKey::Exterior { grid_x, grid_y, .. } => {
            (grid_x - center.x).abs() <= radius && (grid_y - center.y).abs() <= radius
        }
        CellKey::Interior(_) => true,
    }
}

/// The grid squares of the non-retiring exterior roots more than `unload_radius` cells from
/// `center` on either axis: the roots the lifecycle check reports as out of range.
fn roots_outside_window<'a>(
    roots: &'a [(Entity, &'a CellRef, Option<&'a ExteriorCellGrid>)],
    retiring: &'a HashSet<Entity>,
    center: IVec2,
    unload_radius: i32,
) -> impl Iterator<Item = IVec2> + 'a {
    roots
        .iter()
        // A retiring root is outside the radius by construction: it is waiting for its turn in
        // the unload budget, which [`despawn_cells`] bounds, so its lag is not a violation.
        .filter(move |(entity, _, _)| !retiring.contains(entity))
        .filter_map(|(_, _, grid)| grid.map(|grid| grid.0))
        .filter(move |grid| {
            (grid.x - center.x).abs() > unload_radius || (grid.y - center.y).abs() > unload_radius
        })
}

/// Build a cell hierarchy and request its model scenes, recording their IDs for optional tracing.
#[allow(clippy::too_many_arguments)]
fn spawn_cell(
    commands: &mut Commands,
    asset_server: &AssetServer,
    catalog: &AssetCatalog,
    actor_catalog: &ActorCatalog,
    reflection: &WaterReflectionTexture,
    meshes: &mut Assets<Mesh>,
    terrain_materials: &mut Assets<TerrainMaterial>,
    water_materials: &mut Assets<WaterMaterial>,
    origin: IVec2,
    lights: bool,
    terrain_physics: bool,
    schedule_scenes: bool,
    payload: CellPayload,
    mut terrain: Option<TerrainSnapshot>,
    merged: Option<MergedSpawn>,
    model_sequence: &mut u64,
    profiler: &mut ProfilingState,
) -> Result<Entity, String> {
    crate::perf_span::perf_span!("streaming/spawn_cell");
    let spawn_started = Instant::now();
    let reference_count = payload
        .references
        .iter()
        .filter(|row| row.initially_enabled)
        .count();
    let suppressed_count = payload.references.len() - reference_count;
    let root_translation = cell_translation(payload.key, origin);
    let terrain_started = Instant::now();
    let terrain_grid = match payload.key {
        CellKey::Exterior { grid_x, grid_y, .. } => Some(IVec2::new(grid_x, grid_y)),
        CellKey::Interior(_) => None,
    };
    // The merged mesh may have welded edges different from the cached LAND. Query shorelines
    // against the height field actually drawn, while keeping the cache's CELL/WRLD water data.
    if let (Some(terrain), Some(merged)) = (&mut terrain, &merged) {
        terrain.heights = crate::terrain_merge::mesh::cell_to_grid(&merged.cell)?.0;
    }
    // The merged cell's one mesh and its one collider, built from the same buffers.
    let merged_cell = merged
        .map(|spawn| {
            let mesh = crate::terrain_merge::mesh::build_merged_mesh(
                &spawn.cell,
                &spawn.classes.ids_by_form_id,
            )?;
            let collider = terrain_physics
                .then(|| terrain_collider_from_mesh(&mesh))
                .transpose()?;
            Ok::<_, String>((mesh, collider, spawn.material, spawn.images))
        })
        .transpose()?;
    let terrain_quadrants = if let (Some(terrain), None) = (&terrain, &merged_cell) {
        (0..4)
            .map(|quadrant| {
                let mesh = build_terrain_quadrant_mesh(terrain, quadrant)?;
                let collider = terrain_physics
                    .then(|| terrain_collider_from_mesh(&mesh))
                    .transpose()?;
                Ok((quadrant, mesh, collider))
            })
            .collect::<Result<Vec<_>, String>>()?
    } else {
        Vec::new()
    };
    // The four quadrant meshes and their colliders, the cell's heaviest CPU work after the
    // references; a cell without terrain records nothing.
    if !terrain_quadrants.is_empty() {
        profiler.record_elapsed("streaming/terrain_mesh", terrain_started);
    }
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
            if let Some((mesh, collider, material, images)) = merged_cell {
                // Waiting for the render world from birth, like a quadrant: the grid's LOD tiles
                // stay drawn until the bridge confirms the merged mesh drawn
                // (`lod::update_terrain_lod_visibility`). The cell declares the shared material and
                // every land texture array it binds.
                let surface = SwapSurface::new(material.id().untyped(), images.iter().copied());
                let mut patch = parent.spawn((
                    Name::new("Terrain cell"),
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::default(),
                    TerrainPatch,
                    MergedTerrainCell,
                    Visibility::Hidden,
                    PendingMergedTerrain,
                    SwapWaiting,
                    surface,
                ));
                if let Some(grid) = terrain_grid {
                    patch.insert(MergedTerrainCoverage { grid });
                }
                if let Some(collider) = collider {
                    patch.insert((
                        TerrainCollider,
                        RigidBody::Fixed,
                        collider,
                        ColliderDisabled,
                        crate::physics::world_collision_groups(),
                    ));
                }
            }
            for (quadrant, mesh, collider) in terrain_quadrants {
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
                // The quadrant declares what its draw needs, for the render→main swap bridge
                // (`crate::swap_bridge`): the material it draws with and every image that material
                // binds. The images are the ones the readiness pass validates below, and the
                // declaration is re-made there if a normal map has to be dropped.
                let surface = SwapSurface::new(
                    material.id().untyped(),
                    images
                        .color
                        .iter()
                        .chain(images.normal.iter())
                        .map(|handle| handle.id()),
                );
                let mut patch = parent.spawn((
                    Name::new(format!("Terrain quadrant {quadrant}")),
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::default(),
                    TerrainPatch,
                    Visibility::Hidden,
                    PendingTerrainProfile {
                        cell_id: terrain.cell_id,
                        quadrant,
                        images: images.color,
                        normals: images.normal,
                    },
                    // Waiting for the render world from birth: the bridge removes the marker once
                    // the render world has drawn the quadrant. Nothing in this change reads the
                    // marker; the quadrant is still revealed by `track_surface_readiness`.
                    SwapWaiting,
                    surface,
                ));
                if let Some(grid) = terrain_grid {
                    patch.insert(TerrainCoverage {
                        grid,
                        quadrant,
                        tier: None,
                    });
                }
                if let Some(collider) = collider {
                    patch.insert((
                        TerrainCollider,
                        RigidBody::Fixed,
                        collider,
                        ColliderDisabled,
                        crate::physics::world_collision_groups(),
                    ));
                }
            }
            if let Some(surface) = FlatWater::from_terrain(&terrain) {
                let water_mesh = meshes.add(Plane3d::default().mesh().size(CELL_SIZE, CELL_SIZE));
                let water_colors = surface
                    .water_type_form_id
                    .and_then(|form_id| catalog.water_colors(form_id));
                let flow_normal = surface
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
                // Skyrim's DefaultWater deep colour after Update.esm, used when this water has no
                // decoded colours yet (a database converted before the WATR colour export). Skyrim
                // thins water to show the bed where it is shallow; without depth fog a 60% cover
                // keeps river beds visible.
                let base_color = water_colors.map_or(Color::srgba_u8(5, 14, 18, 153), |colors| {
                    let [r, g, b] = colors.deep;
                    Color::srgba_u8(r, g, b, 153)
                });
                let (fresnel, reflectivity) = water_colors.map_or(
                    (
                        crate::render::DEFAULT_WATER_FRESNEL,
                        crate::render::DEFAULT_WATER_REFLECTIVITY,
                    ),
                    |colors| (colors.fresnel, colors.reflectivity),
                );
                let water_material = water_materials.add(WaterMaterial {
                    base: StandardMaterial {
                        base_color,
                        metallic: 0.15,
                        perceptual_roughness: 0.06,
                        reflectance: 0.9,
                        alpha_mode: AlphaMode::Blend,
                        ..default()
                    },
                    extension: WaterExtension::with_reflection_and_factors(
                        reflection.0.clone(),
                        flow_normal.clone(),
                        fresnel,
                        reflectivity,
                    )
                    .with_reflection_color(
                        water_colors
                            .map_or(crate::render::DEFAULT_WATER_REFLECTION_COLOR, |colors| {
                                colors.reflection
                            }),
                    ),
                });
                let mut water = parent.spawn((
                    Name::new("Water"),
                    Mesh3d(water_mesh),
                    MeshMaterial3d(water_material),
                    Transform::from_translation(Vec3::new(
                        CELL_SIZE * 0.5,
                        surface.height,
                        -CELL_SIZE * 0.5,
                    )),
                    WaterSurface,
                    Visibility::Hidden,
                    PendingWaterProfile {
                        cell_id: terrain.cell_id,
                        flow_normal,
                    },
                    RenderLayers::layer(WATER_LAYER),
                ));
                if let Some(body) = FlatWaterBody::from_terrain(payload.key, &terrain) {
                    water.insert(body);
                }
            }
        }
        let references_started = Instant::now();
        let mut model_loads = 0u64;
        for reference in payload.references {
            if !reference.initially_enabled {
                continue;
            }
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
            // A referenced `NPC_` with a catalog row becomes one actor entity. A row the catalog
            // could not resolve, or one with no body to draw, skips the reference entirely: no
            // placeholder, no static mesh, and exactly one diagnostic naming the base FormID.
            let actor_plan = actor_catalog
                .get(reference.base_form_id)
                .map(|model| actor_catalog.spawn_plan(model));
            let actor_plan = match actor_plan {
                Some(Ok(plan)) => Some(plan),
                Some(Err(diagnostic)) => {
                    warn!(
                        form_id = format_args!("{:08X}", diagnostic.base_form_id),
                        reason = diagnostic.reason.as_str(),
                        "actor spawn skipped: {diagnostic}"
                    );
                    continue;
                }
                None => None,
            };
            let mut entity = parent.spawn((
                Name::new(format!("Reference {:08X}", reference.form_id)),
                FormId(reference.form_id),
                CellRef(reference.cell_id),
                world_position,
                WorldTransform(transform.to_matrix()),
                transform,
                // Every mesh this reference draws belongs to the placed-object layer, so the water
                // reflection pass - which renders the world layer only - leaves it out. The meshes
                // arrive later as descendants from the converted glb, so the layer is propagated
                // instead of inserted here; see `render::add_placed_object_layer_propagation`.
                Propagate(PLACED_OBJECT_RENDER_LAYERS),
            ));
            if let Some(bounds) = bounds.zip(model_bounds) {
                entity.insert(bounds);
            }
            if let Some(plan) = &actor_plan {
                entity.insert(plan.actor);
                if !plan.parts.is_empty() {
                    entity.insert(plan.parts.clone());
                }
                if let Some(head) = &plan.head {
                    entity.insert(head.clone());
                }
                if let Some(tone) = plan.skin_tone {
                    entity.insert(tone);
                }
                if let Some(weight) = plan.weight {
                    entity.insert(crate::actor_body_weight::ActorBodyWeight(
                        (1.0 - weight / 100.0).clamp(0.0, 1.0),
                    ));
                }
                if plan.uses_composed_body() {
                    entity.insert((
                        crate::actor_animation::ActorClips::load(asset_server, plan.actor.sex),
                        crate::actor_drive::ActorDrive::default(),
                    ));
                }
            }
            // The colour the reference's `XEMI` source lends its model's `External_Emittance`
            // surfaces once they are instantiated below it; see `crate::emittance`.
            if let Some(rgb) = reference.emittance {
                entity.insert(crate::emittance::ReferenceEmittance::from_srgb(rgb));
            }
            // A child of the reference, so the light sits where the reference is and follows it
            // through a render-origin rebase - and, because it is a descendant of the cell root,
            // through the cell unload that despawns the reference.
            if let Some(light) = reference
                .light
                .as_ref()
                .and_then(|row| crate::lights::point_light(row, reference.light_radius_override))
            {
                let marker = crate::lights::SkyrimLight {
                    form_id: reference.form_id,
                    cell_id: reference.cell_id,
                };
                if !lights {
                    entity.insert(crate::lights::DeferredPointLight { light, marker });
                } else {
                    // A reference without a model has no visibility components, so its light child
                    // could never become visible: Bevy warned (B0004) and extract_lights dropped every
                    // such light. The reference needs Visibility for the hierarchy to propagate.
                    entity.insert(Visibility::default());
                    entity.with_child((
                        Name::new(format!("Light {:08X}", reference.form_id)),
                        light,
                        marker,
                        // Not the reference's propagated placed-object layer: a light keeps
                        // `LIGHT_LAYERS` so it reaches the world layer and both cameras.
                        crate::render::light_render_layers(),
                    ));
                }
            }
            if let Some(door) = crate::doors::load_door(reference.form_id, reference.door.as_ref())
            {
                entity.insert(door);
            }
            // An actor draws its composed body (or its catalog body part before one exists); every
            // other reference keeps the model path it always used.
            let model_path = match &actor_plan {
                Some(plan) => Some(plan.scene_path.clone()),
                None => spawned_model_path(reference.model_path, reference.base_is_marker),
            };
            if let Some(path) = model_path {
                model_loads += 1;
                let sequence = *model_sequence;
                *model_sequence = model_sequence.saturating_add(1);
                if schedule_scenes {
                    let (center, radius) =
                        requests::reference_geometry(world_position, &transform, bounds);
                    entity.insert(requests::SceneRequest {
                        path: path.clone(),
                        cell: Some(payload.key),
                        coarse_terrain: false,
                        retry_cleanup: None,
                        content_identity: None,
                        sequence,
                        center,
                        radius,
                        collision_candidate: terrain_physics
                            && fixed_collision_record_eligible(
                                reference.base_record_type.as_deref(),
                                &path,
                            ),
                        handle: None,
                        started: Instant::now(),
                        allow_retry: false,
                    });
                } else {
                    let handle =
                        asset_server.load(GltfAssetLabel::Scene(0).from_asset(path.clone()));
                    profiler.observe_scene(handle.id(), asset_server);
                    entity.insert(PendingModel { handle, sequence });
                }
                entity.insert((
                    MeshHandle(path.clone()),
                    PendingAssetProfile {
                        started: Instant::now(),
                        scene_spawned: false,
                        path,
                        form_id: reference.form_id,
                        base_form_id: reference.base_form_id,
                        base_record_type: reference.base_record_type,
                        static_physics: terrain_physics,
                        cell_id: reference.cell_id,
                        actor_scene: actor_plan.is_some(),
                    },
                ));
            }
        }
        profiler.record_elapsed("streaming/spawn_references", references_started);
        // Per committed cell: what it carried and what it asked the loaders for; the cell's whole
        // commit time follows in the "committed" event.
        profiler.event(
            format!("{:?}", payload.key),
            format!("spawned references={reference_count} model_loads={model_loads}"),
            Some(references_started.elapsed().as_secs_f64() * 1000.0),
        );
    });
    profiler.increment("streaming/references_spawned", reference_count as u64);
    profiler.increment("streaming/references_suppressed", suppressed_count as u64);
    profiler.record_elapsed("streaming/spawn_cell", spawn_started);
    Ok(root)
}

/// Rapier consumes the exact vertex and triangle buffers rendered by this quadrant, or by this
/// whole cell when its terrain is merged.
fn terrain_collider_from_mesh(mesh: &Mesh) -> Result<Collider, String> {
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return Err("terrain mesh lacks Float32x3 positions".to_owned());
    };
    let Some(Indices::U32(indices)) = mesh.indices() else {
        return Err("terrain mesh lacks U32 triangle indices".to_owned());
    };
    if indices.len() % 3 != 0 {
        return Err("terrain mesh triangle indices are incomplete".to_owned());
    }
    let vertices = positions.iter().copied().map(Vec3::from_array).collect();
    let triangles = indices.as_chunks::<3>().0.to_vec();
    Collider::trimesh(vertices, triangles)
        .map_err(|error| format!("invalid terrain trimesh: {error}"))
}

/// Fixed Riverwood solids with a declared render-triangle proxy policy. Record type remains
/// authoritative: plant TREE records outside the pine family and movable clutter stay excluded.
fn static_proxy_eligible(record_type: Option<&str>, path: &str) -> bool {
    let pine = path.starts_with("meshes/landscape/trees/treepineforest");
    match record_type {
        Some("TREE") => pine,
        Some("STAT") => {
            path.starts_with("meshes/landscape/rocks/")
                || path.starts_with("meshes/architecture/")
                || pine
                || path.starts_with("meshes/clutter/firewood/firewoodpile")
                || (path.starts_with("meshes/landscape/roads/road") && path.contains("ramp"))
        }
        _ => false,
    }
}

/// How a fixed record type may receive static collision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FixedCollisionTier {
    /// Authored shapes, then the verified render proxy.
    Full,
    /// Authored fixed/keyframed bodies only; no render proxy; dynamic bodies ignored.
    AuthoredOnly,
    /// Audited container models with one intact, static-layer body on the root only.
    VerifiedContainer,
}

/// Container paths are a positive audited subset, not a filename prefix or a record-wide grant.
/// See `docs/riverwood-static-collision.md` for the source inventory and deferred moving lids.
fn fixed_collision_tier(record_type: Option<&str>, path: &str) -> Option<FixedCollisionTier> {
    match record_type {
        Some("STAT" | "TREE" | "FURN") => Some(FixedCollisionTier::Full),
        Some("ACTI") => Some(FixedCollisionTier::AuthoredOnly),
        Some("CONT")
            if !crate::collision_measure::disabled()
                && matches!(
                    path,
                    "meshes/clutter/barrel01.glb"
                        | "meshes/clutter/tgbarrel01.glb"
                        | "meshes/clutter/sovngarde/sovbarrel01.glb"
                        | "meshes/clutter/sburialurn01.glb"
                ) =>
        {
            Some(FixedCollisionTier::VerifiedContainer)
        }
        _ => None,
    }
}

/// Placed furniture is fixed in the world, but may use only its authored NIF shape.
/// Render proxies remain restricted to the verified STAT/TREE families above.
fn fixed_collision_record_eligible(record_type: Option<&str>, path: &str) -> bool {
    fixed_collision_tier(record_type, path).is_some()
}

/// Base record types whose placed references may become dynamic bodies (#104 phase a). These are
/// Skyrim's movable clutter records; the authored `kind` stays the classifier, so a fixed body on
/// one of them remains fixed.
///
/// `CONT`, `MSTT`, `ACTI`, `FLOR`, `DOOR` and `FURN` are deliberately left out of this slice:
/// barrels and chests (`CONT`) and ingredient plants (`FLOR`) stay fixed for now even when their
/// NIF carries simulated dynamics. `ACTI` keeps its fixed and keyframed bodies on the static path
/// instead, so only its dynamic bodies are left unsimulated.
fn dynamic_clutter_record_eligible(record_type: Option<&str>) -> bool {
    matches!(
        record_type,
        Some(
            "MISC" | "WEAP" | "ARMO" | "BOOK" | "AMMO" | "ALCH" | "INGR" | "SLGM" | "KEYM" | "SCRL"
        )
    )
}

fn static_proxy_material_allowed(
    path: &str,
    material_name: Option<&str>,
    material: &StandardMaterial,
) -> bool {
    matches!(material.alpha_mode, AlphaMode::Opaque)
        // Riverwood RockCliff GLBs put the large rock faces in BLEND primitives. Their masked
        // detail primitives stay excluded, as do blended materials on unrelated models.
        || (path.starts_with("meshes/landscape/rocks/rockcliff")
            && matches!(material.alpha_mode, AlphaMode::Blend))
        // The lumbermill's walkable ramp/boards are woodwalkway01 with MASK alpha. Rope and
        // roof cutouts remain excluded; the material name comes from the converted GLB.
        || (path == "meshes/architecture/farmhouse/lumbermill01.glb"
            && material_name.is_some_and(|name| name.starts_with("LumbermillMesh:19"))
            && matches!(material.alpha_mode, AlphaMode::Mask(_)))
}

/// Build one model-local proxy from the validated spawned scene. Bevy/Rapier apply the reference
/// instance translation, rotation, and scale when this collider is attached to its reference root.
/// Node transforms are baked into vertices, preserving mesh holes such as doorways.
fn static_proxy_from_hierarchy(
    path: &str,
    root: Entity,
    children: &Query<&Children>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
    materials: &Assets<StandardMaterial>,
) -> Result<Option<Collider>, String> {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    let mut stack = Vec::new();
    if let Ok(kids) = children.get(root) {
        stack.extend(kids.iter().map(|child| (child, Affine3A::IDENTITY)));
    }
    while let Some((entity, parent_to_root)) = stack.pop() {
        let (local, _) = transforms
            .get(entity)
            .map_err(|_| format!("static proxy node {entity:?} has no transform"))?;
        let node_to_root = parent_to_root * local.compute_affine();
        if let Ok((mesh_handle, material_handle, material_name, extras, native_source)) =
            primitives.get(entity)
            && !extras.is_some_and(has_explicit_material_exclusion)
        {
            let material = material_handle
                .map(|handle| &handle.0)
                .or_else(|| native_source.map(|source| &source.0))
                .and_then(|handle| materials.get(handle));
            if material.is_some_and(|material| {
                static_proxy_material_allowed(
                    path,
                    material_name.map(|name| name.0.as_str()),
                    material,
                )
            }) {
                let mesh = meshes.get(mesh_handle).ok_or_else(|| {
                    format!("static proxy mesh {:?} is missing", mesh_handle.id())
                })?;
                if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
                    return Err("static proxy mesh is not a triangle list".to_owned());
                }
                let Some(VertexAttributeValues::Float32x3(positions)) =
                    mesh.attribute(Mesh::ATTRIBUTE_POSITION)
                else {
                    return Err("static proxy mesh lacks Float32x3 positions".to_owned());
                };
                let indices: Vec<u32> = match mesh.indices() {
                    Some(Indices::U16(indices)) => {
                        indices.iter().map(|index| u32::from(*index)).collect()
                    }
                    Some(Indices::U32(indices)) => indices.clone(),
                    None => return Err("static proxy mesh lacks triangle indices".to_owned()),
                };
                if !indices.len().is_multiple_of(3) {
                    return Err("static proxy mesh has incomplete triangles".to_owned());
                }
                let offset = u32::try_from(vertices.len())
                    .map_err(|_| "static proxy vertex count exceeds u32".to_owned())?;
                for position in positions {
                    let position = node_to_root.transform_point3(Vec3::from_array(*position));
                    if !position.is_finite() {
                        return Err("static proxy has a non-finite vertex".to_owned());
                    }
                    vertices.push(position);
                }
                for index in indices.as_chunks::<3>().0 {
                    if index.iter().any(|index| *index as usize >= positions.len()) {
                        return Err("static proxy triangle index is out of range".to_owned());
                    }
                    triangles.push([offset + index[0], offset + index[1], offset + index[2]]);
                }
            }
        }
        if let Ok(kids) = children.get(entity) {
            stack.extend(kids.iter().map(|child| (child, node_to_root)));
        }
    }
    if triangles.is_empty() {
        return Ok(None);
    }
    Collider::trimesh(vertices, triangles)
        .map(Some)
        .map_err(|error| format!("invalid static proxy trimesh: {error}"))
}

/// Drop malformed entries from the collision extras' `bodies` array, in place, and note each in
/// `skipped`. A malformed body (a non-finite number written as `null`, an unknown `kind`) must
/// not fail the whole asset parse and take the model's fixed collision shapes down with it.
fn drop_malformed_bodies(collision: &mut serde_json::Value) {
    let Some(object) = collision.as_object_mut() else {
        return;
    };
    // Only an array is filtered; anything else is left for the parse to report as it did before.
    if !object
        .get("bodies")
        .is_some_and(serde_json::Value::is_array)
    {
        return;
    }
    let Some(serde_json::Value::Array(entries)) = object.remove("bodies") else {
        return;
    };
    let mut kept = Vec::with_capacity(entries.len());
    let mut notes = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        match serde_json::from_value::<CollisionBody>(entry.clone()) {
            Ok(_) => kept.push(entry),
            Err(error) => notes.push(serde_json::Value::String(format!(
                "rigid body {index} is malformed: {error}"
            ))),
        }
    }
    object.insert("bodies".to_owned(), serde_json::Value::Array(kept));
    if !notes.is_empty() {
        let skipped = object
            .entry("skipped")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let Some(skipped) = skipped.as_array_mut() {
            skipped.extend(notes);
        }
    }
}

fn authored_collision_from_hierarchy(
    root: Entity,
    children: &Query<&Children>,
    extras: &Query<&GltfSceneExtras>,
) -> Result<Option<CollisionAsset>, String> {
    let mut stack = vec![root];
    while let Some(entity) = stack.pop() {
        if let Ok(scene_extras) = extras.get(entity) {
            if scene_extras.value.len() > 64 * 1024 * 1024 {
                return Err("GLB collision extras exceed 64 MiB".to_owned());
            }
            let value: serde_json::Value = serde_json::from_str(&scene_extras.value)
                .map_err(|error| format!("invalid GLB scene extras: {error}"))?;
            let collision = value
                .get("mudcrabCollision")
                .or_else(|| value.get("openSkyrimCollision"))
                .cloned();
            if let Some(mut collision) = collision {
                drop_malformed_bodies(&mut collision);
                let asset: CollisionAsset = serde_json::from_value(collision)
                    .map_err(|error| format!("invalid GLB collision data: {error}"))?;
                if asset.version == 0 || asset.version > COLLISION_ASSET_VERSION || !asset.authored
                {
                    return Err("unsupported GLB collision contract".to_owned());
                }
                return Ok(Some(asset));
            }
        }
        if let Ok(kids) = children.get(entity) {
            stack.extend(kids.iter());
        }
    }
    Ok(None)
}

fn collider_parts_from_authored(
    asset: &CollisionAsset,
) -> Result<Vec<AuthoredColliderPart>, String> {
    asset.shapes.iter().map(collider_part_from_shape).collect()
}

/// Decide a fixed record's static collision from its asset alone.
///
/// `Full` keeps every authored shape, whatever its body's kind, which is what fixed record types
/// have always done. `AuthoredOnly` keeps a shape unless every body that owns it is dynamic, so a
/// prop's fixed and keyframed bodies stay solid while its simulated parts are left to nothing.
/// `VerifiedContainer` requires complete ownership by one static-layer root body; unowned shapes
/// and animated components cannot inherit ACTI's rest-pose fallback.
fn static_decision_from_asset(
    tier: FixedCollisionTier,
    asset: &CollisionAsset,
) -> StaticCollisionDecision {
    match tier {
        FixedCollisionTier::VerifiedContainer => verified_container_collision(asset),
        FixedCollisionTier::Full => match collider_parts_from_authored(asset) {
            Ok(parts) if !parts.is_empty() => {
                StaticCollisionDecision::Authored(parts, asset.skipped.clone())
            }
            Ok(_) if asset.skipped.is_empty() => StaticCollisionDecision::AuthoredAbsent,
            Ok(_) => StaticCollisionDecision::Skipped(asset.skipped.join("; ")),
            Err(reason) => StaticCollisionDecision::Skipped(reason),
        },
        FixedCollisionTier::AuthoredOnly => {
            let mut owned_by_fixed = vec![false; asset.shapes.len()];
            let mut owned_by_dynamic = vec![false; asset.shapes.len()];
            for body in &asset.bodies {
                let dynamic = body.kind == BodyKind::Dynamic;
                for shape_index in &body.shapes {
                    let Some(index) = usize::try_from(*shape_index)
                        .ok()
                        .filter(|index| *index < asset.shapes.len())
                    else {
                        return StaticCollisionDecision::Skipped(
                            "authored collision body lists a shape that does not exist".to_owned(),
                        );
                    };
                    if dynamic {
                        owned_by_dynamic[index] = true;
                    } else {
                        owned_by_fixed[index] = true;
                    }
                }
            }
            let mut parts = Vec::new();
            let mut kept = vec![false; asset.shapes.len()];
            for (index, shape) in asset.shapes.iter().enumerate() {
                if owned_by_dynamic[index] && !owned_by_fixed[index] {
                    continue;
                }
                match collider_part_from_shape(shape) {
                    Ok(part) => {
                        kept[index] = true;
                        parts.push(part);
                    }
                    Err(reason) => return StaticCollisionDecision::Skipped(reason),
                }
            }
            let lost_bodies = asset
                .bodies
                .iter()
                .filter(|body| {
                    body.kind == BodyKind::Dynamic
                        && body
                            .shapes
                            .iter()
                            .any(|index| !kept.get(*index as usize).copied().unwrap_or(false))
                })
                .count();
            if !parts.is_empty() {
                let mut notes = asset.skipped.clone();
                if lost_bodies > 0 {
                    notes.push(format!(
                        "{lost_bodies} dynamic bodies left unsimulated on a fixed record"
                    ));
                }
                StaticCollisionDecision::Authored(parts, notes)
            } else if asset.skipped.is_empty() {
                // Nothing was dropped and nothing failed to convert: the tier is the whole reason
                // this record has no shapes, which is a policy outcome rather than a failure.
                StaticCollisionDecision::AuthoredAbsent
            } else {
                StaticCollisionDecision::Skipped(asset.skipped.join("; "))
            }
        }
    }
}

/// The admitted container models were audited without node animation and with collision on their
/// root. Keep that narrow contract even when an existing pack has lost body bindings, or a model
/// override changes the authored body kind, layer, node or shape ownership.
fn verified_container_collision(asset: &CollisionAsset) -> StaticCollisionDecision {
    if !asset.skipped.is_empty() {
        return StaticCollisionDecision::Skipped(asset.skipped.join("; "));
    }
    if asset.shapes.is_empty() {
        return StaticCollisionDecision::AuthoredAbsent;
    }
    if asset.version < 2 {
        return StaticCollisionDecision::Skipped(
            "fixed container collision requires version 2 body ownership".to_owned(),
        );
    }
    let [body] = asset.bodies.as_slice() else {
        return StaticCollisionDecision::Skipped(
            "fixed container collision requires one intact root body".to_owned(),
        );
    };
    if body.kind != BodyKind::Fixed
        || body.node != 0
        || body.target.is_empty()
        || body.mass != 0.0
        || body.havok.collision_layer != 1
    {
        return StaticCollisionDecision::Skipped(
            "fixed container collision requires a massless fixed STATIC-layer root body".to_owned(),
        );
    }
    let mut owned = vec![false; asset.shapes.len()];
    for index in &body.shapes {
        let Some(shape) = usize::try_from(*index)
            .ok()
            .and_then(|index| owned.get_mut(index))
        else {
            return StaticCollisionDecision::Skipped(
                "fixed container body lists a shape that does not exist".to_owned(),
            );
        };
        *shape = true;
    }
    if owned.contains(&false) {
        return StaticCollisionDecision::Skipped(
            "fixed container collision has shapes without root-body ownership".to_owned(),
        );
    }
    match collider_parts_from_authored(asset) {
        Ok(parts) => StaticCollisionDecision::Authored(parts, Vec::new()),
        Err(reason) => StaticCollisionDecision::Skipped(reason),
    }
}

fn collider_part_from_shape(shape: &CollisionShape) -> Result<AuthoredColliderPart, String> {
    let (translation, collider) = {
        match shape {
            CollisionShape::Mesh {
                vertices,
                triangles,
            } => {
                if vertices.len() > 1_000_000 || triangles.len() > 2_000_000 {
                    return Err("authored mesh exceeds safety limit".to_owned());
                }
                if vertices.iter().flatten().any(|value| !value.is_finite())
                    || triangles
                        .iter()
                        .flatten()
                        .any(|index| *index as usize >= vertices.len())
                {
                    return Err("authored mesh contains invalid vertices or indices".to_owned());
                }
                let collider = Collider::trimesh(
                    vertices.iter().copied().map(Vec3::from_array).collect(),
                    triangles.clone(),
                )
                .map_err(|error| format!("invalid authored trimesh: {error}"))?;
                (Vec3::ZERO, collider)
            }
            CollisionShape::Capsule { a, b, radius } => {
                if !a.iter().chain(b).all(|value| value.is_finite())
                    || !radius.is_finite()
                    || *radius <= 0.0
                {
                    return Err("invalid authored capsule".to_owned());
                }
                (
                    Vec3::ZERO,
                    Collider::capsule(Vec3::from_array(*a), Vec3::from_array(*b), *radius),
                )
            }
            CollisionShape::Box {
                center,
                half_extents,
            } => {
                if !center
                    .iter()
                    .chain(half_extents)
                    .all(|value| value.is_finite())
                    || half_extents.iter().any(|value| *value <= 0.0)
                {
                    return Err("invalid authored box".to_owned());
                }
                (
                    Vec3::from_array(*center),
                    Collider::cuboid(half_extents[0], half_extents[1], half_extents[2]),
                )
            }
            CollisionShape::Hull { points } => {
                if points.len() > 1_000_000
                    || points.iter().flatten().any(|value| !value.is_finite())
                {
                    return Err("invalid authored hull".to_owned());
                }
                let collider = Collider::convex_hull(
                    &points
                        .iter()
                        .copied()
                        .map(Vec3::from_array)
                        .collect::<Vec<_>>(),
                )
                .ok_or_else(|| "invalid authored convex hull".to_owned())?;
                (Vec3::ZERO, collider)
            }
        }
    };
    Ok(AuthoredColliderPart {
        translation,
        collider,
    })
}

fn attach_authored_collision(
    commands: &mut Commands,
    entity: Entity,
    parts: &[AuthoredColliderPart],
) {
    if let [
        AuthoredColliderPart {
            translation,
            collider,
        },
    ] = parts
        && *translation == Vec3::ZERO
    {
        commands.entity(entity).insert((
            StaticColliderSource::NifAuthored,
            RigidBody::Fixed,
            collider.clone(),
            crate::physics::world_collision_groups(),
        ));
        return;
    }
    commands
        .entity(entity)
        .insert((StaticColliderSource::NifAuthored, RigidBody::Fixed));
    for part in parts {
        commands.spawn((
            part.collider.clone(),
            Transform::from_translation(part.translation),
            crate::physics::world_collision_groups(),
            ChildOf(entity),
        ));
    }
}

/// Most points one dynamic body may contribute after duplicate shape indices are removed.
const MAX_CLUTTER_HULL_POINTS: usize = 1_000_000;
/// Most collider parts one dynamic body may spawn. The point budget alone would permit
/// 125,000 distinct boxes; this cap bounds collider entities independently of geometry size.
const MAX_CLUTTER_COLLIDER_PARTS: usize = 256;

/// Every point of a shape, for the convex hull of a non-convex body, handed to `emit`.
///
/// The copy and the count both run through here, so the two cannot drift apart.
fn for_each_shape_hull_point(shape: &CollisionShape, mut emit: impl FnMut(Vec3)) {
    match shape {
        CollisionShape::Mesh { vertices, .. } => {
            for vertex in vertices {
                emit(Vec3::from_array(*vertex));
            }
        }
        CollisionShape::Hull { points: hull } => {
            for point in hull {
                emit(Vec3::from_array(*point));
            }
        }
        CollisionShape::Box {
            center,
            half_extents,
        } => {
            let center = Vec3::from_array(*center);
            let half = Vec3::from_array(*half_extents);
            for corner in 0..8u8 {
                let sign = Vec3::new(
                    if corner & 1 == 0 { -1.0 } else { 1.0 },
                    if corner & 2 == 0 { -1.0 } else { 1.0 },
                    if corner & 4 == 0 { -1.0 } else { 1.0 },
                );
                emit(center + sign * half);
            }
        }
        CollisionShape::Capsule { a, b, radius } => {
            // The 13 directions to a cube's neighbours (3 axes, 6 face diagonals, 4 corner
            // diagonals), each used as + and -: 26 points per end. The 6 axis points alone make an
            // octahedron, whose surface sits up to ~42% of the radius inside the capsule.
            const FACE: f32 = std::f32::consts::FRAC_1_SQRT_2;
            const CORNER: f32 = 0.577_350_26;
            const DIRECTIONS: [Vec3; 13] = [
                Vec3::X,
                Vec3::Y,
                Vec3::Z,
                Vec3::new(FACE, FACE, 0.0),
                Vec3::new(FACE, -FACE, 0.0),
                Vec3::new(FACE, 0.0, FACE),
                Vec3::new(FACE, 0.0, -FACE),
                Vec3::new(0.0, FACE, FACE),
                Vec3::new(0.0, FACE, -FACE),
                Vec3::new(CORNER, CORNER, CORNER),
                Vec3::new(CORNER, CORNER, -CORNER),
                Vec3::new(CORNER, -CORNER, CORNER),
                Vec3::new(-CORNER, CORNER, CORNER),
            ];
            for end in [Vec3::from_array(*a), Vec3::from_array(*b)] {
                for direction in DIRECTIONS {
                    emit(end + direction * *radius);
                    emit(end - direction * *radius);
                }
            }
        }
    }
}

/// Every point of a shape, for the convex hull of a non-convex body.
fn shape_hull_points(shape: &CollisionShape, points: &mut Vec<Vec3>) {
    for_each_shape_hull_point(shape, |point| points.push(point));
}

/// How many points [`shape_hull_points`] would append for `shape`, counted without allocating.
fn shape_hull_point_count(shape: &CollisionShape) -> usize {
    let mut count = 0usize;
    for_each_shape_hull_point(shape, |_| count += 1);
    count
}

/// The checked total of the points every listed shape contributes, repeats included, or `None`
/// when the running total passes `budget`. Bailing out early keeps a long list of indices from
/// being counted in full.
fn clutter_hull_point_total(shapes: &[&CollisionShape], budget: usize) -> Option<usize> {
    let mut total = 0usize;
    for shape in shapes {
        total = total.checked_add(shape_hull_point_count(shape))?;
        if total > budget {
            return None;
        }
    }
    Some(total)
}

/// Decide whether a movable-type model's authored data gives it a dynamic body (#104 phase a).
/// Takes the first dynamic body; a body whose shapes are not all convex primitives (or that lists
/// any trimesh) becomes one convex hull, counted by the caller.
fn clutter_decision_from_asset(asset: &CollisionAsset) -> StaticCollisionDecision {
    clutter_decision_from_asset_within(asset, MAX_CLUTTER_HULL_POINTS)
}

/// As [`clutter_decision_from_asset`], with the hull point limit injectable so a test does not
/// need a million points to reach it.
fn clutter_decision_from_asset_within(
    asset: &CollisionAsset,
    point_budget: usize,
) -> StaticCollisionDecision {
    let mut dynamic = asset
        .bodies
        .iter()
        .filter(|body| body.kind == BodyKind::Dynamic);
    let Some(body) = dynamic.next() else {
        return StaticCollisionDecision::ClutterNone;
    };
    let ignored_bodies = dynamic.count();
    let mut shapes = Vec::new();
    let mut seen = HashSet::new();
    for index in &body.shapes {
        // Repeated indices add no geometry, but duplicate colliders would multiply both
        // entities and density-derived mass. Keep the first occurrence's order.
        if !seen.insert(*index) {
            continue;
        }
        match asset.shapes.get(*index as usize) {
            Some(shape) => shapes.push(shape),
            None => {
                return StaticCollisionDecision::ClutterSkipped(
                    "dynamic body lists a shape that does not exist".to_owned(),
                );
            }
        }
    }
    if shapes.is_empty() {
        return StaticCollisionDecision::ClutterSkipped("dynamic body has no shapes".to_owned());
    }
    let convexify = !body.convex
        || shapes
            .iter()
            .any(|shape| matches!(shape, CollisionShape::Mesh { .. }));
    if !convexify && shapes.len() > MAX_CLUTTER_COLLIDER_PARTS {
        return StaticCollisionDecision::ClutterSkipped(
            "dynamic body has too many collider parts".to_owned(),
        );
    }
    // Check the total distinct geometry before copying points or building any collider. Both
    // branches keep the point budget; the convex-primitive branch also has a separate part cap.
    let Some(point_count) = clutter_hull_point_total(&shapes, point_budget) else {
        return StaticCollisionDecision::ClutterSkipped(
            "dynamic body has too many hull points".to_owned(),
        );
    };
    let parts = if convexify {
        let mut points = Vec::with_capacity(point_count);
        for shape in &shapes {
            shape_hull_points(shape, &mut points);
        }
        if points.iter().any(|point| !point.is_finite()) {
            return StaticCollisionDecision::ClutterSkipped(
                "dynamic body has invalid hull points".to_owned(),
            );
        }
        match Collider::convex_hull(&points) {
            Some(collider) => vec![AuthoredColliderPart {
                translation: Vec3::ZERO,
                collider,
            }],
            None => {
                return StaticCollisionDecision::ClutterSkipped(
                    "no convex hull for the dynamic body".to_owned(),
                );
            }
        }
    } else {
        match shapes
            .iter()
            .map(|shape| collider_part_from_shape(shape))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(parts) => parts,
            Err(reason) => return StaticCollisionDecision::ClutterSkipped(reason),
        }
    };
    StaticCollisionDecision::Clutter(Box::new(ClutterSpec {
        parts,
        mass: crate::physics::mass_properties_from_body(body),
        body: body.clone(),
        convexified: convexify,
        ignored_bodies,
    }))
}

fn clutter_collision_from_hierarchy(
    root: Entity,
    children: &Query<&Children>,
    scene_extras: &Query<&GltfSceneExtras>,
) -> StaticCollisionDecision {
    match authored_collision_from_hierarchy(root, children, scene_extras) {
        Ok(Some(asset)) => clutter_decision_from_asset(&asset),
        Ok(None) => StaticCollisionDecision::ClutterNone,
        Err(reason) => StaticCollisionDecision::ClutterSkipped(reason),
    }
}

#[allow(clippy::too_many_arguments)] // Scene queries and assets belong to this extraction pass.
fn static_collision_from_hierarchy(
    record_type: Option<&str>,
    path: &str,
    root: Entity,
    children: &Query<&Children>,
    scene_extras: &Query<&GltfSceneExtras>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
    materials: &Assets<StandardMaterial>,
) -> StaticCollisionDecision {
    match authored_collision_from_hierarchy(root, children, scene_extras) {
        // Only a tiered record type reaches this path, so a missing tier is a broken invariant
        // rather than a case to fall back on: the fallback would hand a record the shapes its
        // tier exists to withhold.
        Ok(Some(asset)) => static_decision_from_asset(
            fixed_collision_tier(record_type, path)
                .expect("static collision is only built for a tiered record type"),
            &asset,
        ),
        Ok(None) if static_proxy_eligible(record_type, path) => {
            match static_proxy_from_hierarchy(
                path, root, children, transforms, primitives, meshes, materials,
            ) {
                Ok(Some(collider)) => StaticCollisionDecision::RenderProxy(collider),
                Ok(None) => StaticCollisionDecision::Skipped(
                    "legacy render proxy has no eligible triangles".to_owned(),
                ),
                Err(reason) => StaticCollisionDecision::Skipped(reason),
            }
        }
        Ok(None) => StaticCollisionDecision::LegacyExcluded,
        Err(reason) => StaticCollisionDecision::Skipped(reason),
    }
}

/// Read a model once for a record type that no collision tier accepts, reporting whether the
/// exclusion silently drops authored collision. A malformed asset counts as no authored collision
/// so it cannot inflate the report.
fn exclusion_decision_from_hierarchy(
    root: Entity,
    children: &Query<&Children>,
    scene_extras: &Query<&GltfSceneExtras>,
) -> StaticCollisionDecision {
    match authored_collision_from_hierarchy(root, children, scene_extras) {
        Ok(Some(asset)) if !asset.shapes.is_empty() => {
            StaticCollisionDecision::ExcludedWithAuthored
        }
        Ok(Some(_)) | Ok(None) | Err(_) => StaticCollisionDecision::LegacyExcluded,
    }
}

/// A converted model [`spawn_cell`] found for a reference, waiting for its turn in the arming
/// budget.
///
/// Bevy instantiates every model it is handed in one unbudgeted pass, so handing it a whole cell's
/// models at once is what makes an arrival frame expensive. [`arm_pending_models`] replaces this
/// component with [`WorldAssetRoot`] once the converted scene is loaded and the budget allows it.
/// A model whose cell is unloaded before its turn needs no cleanup: this is a component on that
/// cell's subtree, not an entry in a queue of its own, so it disappears with the subtree.
#[derive(Component)]
struct PendingModel {
    handle: Handle<WorldAsset>,
    /// Where the reference was in spawn order, so a backlog is armed oldest first.
    sequence: u64,
}

#[derive(Component)]
pub(crate) struct PendingAssetProfile {
    started: Instant,
    scene_spawned: bool,
    path: String,
    form_id: u32,
    base_form_id: u32,
    base_record_type: Option<String>,
    static_physics: bool,
    cell_id: u32,
    /// An actor scene has no `statics` row, so its bounds come from the spawned hierarchy instead
    /// of the converter's aggregate bounds.
    actor_scene: bool,
}

#[cfg(test)]
impl PendingAssetProfile {
    pub(crate) fn test_actor() -> Self {
        Self {
            started: Instant::now(),
            scene_spawned: false,
            path: "meshes/actors/test.glb".to_owned(),
            form_id: 1,
            base_form_id: 1,
            base_record_type: Some("NPC_".to_owned()),
            static_physics: false,
            cell_id: 1,
            actor_scene: true,
        }
    }

    pub(crate) fn mark_instance_spawned_for_test(&mut self) {
        self.scene_spawned = true;
    }

    pub(crate) fn instance_spawned_for_test(&self) -> bool {
        self.scene_spawned
    }
}

#[derive(Component)]
struct PendingTerrainProfile {
    cell_id: u32,
    quadrant: u8,
    /// The layers' diffuse images, which must decode as sRGB.
    images: Vec<Handle<Image>>,
    /// The layers' normal maps, which must decode as linear.
    normals: Vec<Handle<Image>>,
}

#[derive(Component)]
struct PendingWaterProfile {
    cell_id: u32,
    flow_normal: Option<Handle<Image>>,
}

/// Counts the streamer's unfinished-work markers under one cell root: a model not yet armed or
/// instantiated, and terrain or water whose textures are still loading. Unlike the global
/// [`StreamingMetrics`] counts these cover only that cell, which is what a load door waits for.
#[derive(bevy::ecs::system::SystemParam)]
#[allow(clippy::type_complexity)]
pub(crate) struct PendingUnder<'w, 's> {
    children: Query<'w, 's, &'static Children>,
    pending: Query<
        'w,
        's,
        (),
        Or<(
            With<PendingModel>,
            With<PendingAssetProfile>,
            With<PendingTerrainProfile>,
            With<PendingWaterProfile>,
        )>,
    >,
}

impl PendingUnder<'_, '_> {
    /// How many entities at or under `root` still carry an unfinished-work marker.
    pub(crate) fn count(&self, root: Entity) -> usize {
        std::iter::once(root)
            .chain(self.children.iter_descendants(root))
            .filter(|entity| self.pending.contains(*entity))
            .count()
    }
}

/// A pending-work marker for tests in other modules: terrain whose textures are still loading.
#[cfg(test)]
pub(crate) fn pending_marker_for_test() -> impl Bundle {
    PendingTerrainProfile {
        cell_id: 0,
        quadrant: 0,
        images: Vec::new(),
        normals: Vec::new(),
    }
}

type RenderPrimitiveQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static Mesh3d,
        Option<&'static MeshMaterial3d<StandardMaterial>>,
        Option<&'static GltfMaterialName>,
        Option<&'static GltfExtras>,
        Option<&'static NifNativeMaterialSource>,
    ),
>;

/// The mesh and material stores, the camera, the pacing tracker and the dynamic clutter bodies the
/// readiness scan also uses.
type ReadinessExtras<'world, 'state> = (
    ResMut<'world, Assets<Mesh>>,
    ResMut<'world, Assets<StandardMaterial>>,
    Query<'world, 'state, &'static Transform, With<StreamingCamera>>,
    ResMut<'world, crate::pacing::PacingTracker>,
    Query<'world, 'state, (), With<crate::physics::DynamicClutter>>,
    EffectPalettePrimitives<'world, 'state>,
);

/// Converted effect surfaces drawn through their palette (`crate::nif_material`): these carry an
/// [`EffectPaletteMaterial`] in place of a `StandardMaterial`, either directly or, under an
/// authored depth override, as the source of the material that wraps it.
type EffectPalettePrimitives<'world, 'state> = (
    Query<'world, 'state, EffectPaletteSurface>,
    Option<Res<'world, Assets<EffectPaletteMaterial>>>,
);

type EffectPaletteSurface = AnyOf<(
    &'static MeshMaterial3d<EffectPaletteMaterial>,
    &'static EffectPaletteDepthSource,
)>;

fn effect_palette_handle<'a>(
    (material, depth_source): (
        Option<&'a MeshMaterial3d<EffectPaletteMaterial>>,
        Option<&'a EffectPaletteDepthSource>,
    ),
) -> Option<&'a Handle<EffectPaletteMaterial>> {
    material
        .map(|material| &material.0)
        .or_else(|| depth_source.map(|source| &source.0))
}

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

/// Wall-clock timing for the pass Bevy spends instantiating converted models.
#[derive(Resource, Default)]
struct SceneSpawnBatch {
    started: Option<Instant>,
}

fn begin_scene_spawn_batch(mut batch: ResMut<SceneSpawnBatch>) {
    batch.started = Some(Instant::now());
}

/// Records the cost of Bevy's world-instance spawn pass and counts what the pass did.
///
/// The counts read the world rather than Bevy's instance bookkeeping: a reference gains a child the
/// moment its converted scene is written below it, so `Changed<Children>` on a reference that owns
/// an instance is exactly "this model was instantiated this frame". `WorldInstance` itself is
/// inserted when the reference joins the spawner's queue
/// (`bevy_world_serialization::world_asset_spawner`, `world_instance_spawner`) - which happens
/// whether or not the asset has landed - so `Added<WorldInstance>` counts the models *handed to*
/// the spawner this frame, the same thing as spawned only once arming waits for the asset.
fn end_scene_spawn_batch(
    mut batch: ResMut<SceneSpawnBatch>,
    spawned: Query<(), (With<WorldInstance>, Changed<Children>)>,
    armed: Query<(), Added<WorldInstance>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
    mut runtime: Option<ResMut<runtime::StreamingRuntime>>,
) {
    if let Some(started) = batch.started.take() {
        if let Some(runtime) = runtime.as_mut() {
            runtime.previous_spawn_ms = started.elapsed().as_secs_f64() * 1000.0;
        }
        profiler.record_elapsed("scene/spawn_batch", started);
    }
    let spawned = spawned.iter().count() as u64;
    let armed = armed.iter().count() as u64;
    metrics.instances_spawned_this_frame = spawned;
    metrics.max_instances_spawned_per_frame = metrics.max_instances_spawned_per_frame.max(spawned);
    metrics.instances_armed_this_frame = armed;
    metrics.max_instances_armed_per_frame = metrics.max_instances_armed_per_frame.max(armed);
    profiler.set_gauge("scene/instances_spawned_this_frame", spawned as f64);
    profiler.set_gauge("scene/instances_armed_this_frame", armed as f64);
}

/// Hands loaded converted models to Bevy's scene spawner, at most `max_model_spawns_per_frame` of
/// them per frame.
///
/// Bevy instantiates every model whose converted scene is ready in the frame it is handed over, in
/// `SceneSpawnerSystems::WorldInstanceSpawn`, so the batch a frame pays to instantiate is the batch
/// this system lets through. The budget is spent on the models that are ready and skipped over the
/// ones that are still loading, so a slow load cannot hold the models behind it back; among the
/// ready ones the oldest reference is armed first in fixed mode. Optional priority scheduling
/// rechecks camera and cell demand and shares this quota with LOD scenes. `0` arms every ready model.
#[allow(clippy::too_many_arguments)]
fn arm_pending_models(
    config: Res<EngineConfig>,
    asset_server: Res<AssetServer>,
    world_assets: Res<Assets<WorldAsset>>,
    pending: Query<(Entity, &PendingModel, Option<&requests::SceneRequest>)>,
    cameras: Query<&Transform, With<StreamingCamera>>,
    origin: Option<Res<RenderOrigin>>,
    mut scheduling: Option<ResMut<requests::SceneSchedulingState>>,
    streaming: Option<Res<StreamingWorld>>,
    mut commands: Commands,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
    mut runtime: Option<ResMut<runtime::StreamingRuntime>>,
) {
    use bevy::math::DVec3;
    crate::perf_span::perf_span!("streaming/arm_models");
    let started = Instant::now();
    let backlog = pending.iter().count();
    let origin = origin.map_or(IVec2::ZERO, |origin| origin.0);
    let view = cameras
        .single()
        .ok()
        .map(|camera| priority::PriorityView {
            position: camera.translation.as_dvec3()
                + DVec3::new(
                    f64::from(origin.x) * f64::from(CELL_SIZE),
                    0.0,
                    -f64::from(origin.y) * f64::from(CELL_SIZE),
                ),
            forward: (camera.rotation * Vec3::NEG_Z).as_dvec3(),
        })
        .unwrap_or(priority::PriorityView {
            position: DVec3::NAN,
            forward: DVec3::NAN,
        });
    let mut ready: Vec<_> = pending
        .iter()
        .filter(|(_, model, request)| {
            request.is_none_or(|request| {
                streaming
                    .as_ref()
                    .is_none_or(|streaming| requests::relevant(request, streaming))
            }) && model_can_spawn(&world_assets, &asset_server, &model.handle)
        })
        .map(|(entity, model, request)| {
            let rank = request
                .map(|request| {
                    priority::DemandPriority::new(
                        view,
                        request.center,
                        request.radius,
                        request.collision_candidate,
                        model.sequence,
                    )
                    .with_coarse_terrain(request.coarse_terrain)
                })
                .unwrap_or_else(|| {
                    priority::DemandPriority::new(view, DVec3::NAN, None, false, model.sequence)
                });
            (model.sequence, entity, model.handle.clone(), rank)
        })
        .collect();
    ready.sort_by_key(|(sequence, entity, _, _)| (*sequence, entity.to_bits()));
    let decision = runtime.as_ref().and_then(|runtime| runtime.decision);
    let budget = decision.map_or(config.max_model_spawns_per_frame, |decision| {
        decision.budgets.max_model_activations
    });
    let armed = if budget == 0 && decision.is_none() {
        ready.len()
    } else {
        budget.min(ready.len())
    };
    let mut choices = scheduling
        .as_ref()
        .map_or(0, |state| state.activation_choices);
    let selected: Vec<_> = if config.prioritize_streaming {
        let indices = priority::ordered_choices(
            &ready
                .iter()
                .map(|(_, _, _, rank)| *rank)
                .collect::<Vec<_>>(),
            choices,
            armed,
        );
        indices
            .into_iter()
            .map(|index| ready[index].clone())
            .collect()
    } else {
        // Preserve the linear FIFO drain when priority scheduling is disabled.
        ready.drain(..armed).collect()
    };
    let mut actually_armed = 0usize;
    let activation_started = Instant::now();
    for (_, entity, handle, _) in selected {
        if actually_armed != 0
            && decision.is_some_and(|decision| {
                activation_started.elapsed().as_micros()
                    >= u128::from(decision.budgets.max_activation_micros)
            })
        {
            break;
        }
        if let Some(runtime) = runtime.as_mut() {
            let request = pending.get(entity).ok().and_then(|(_, _, request)| request);
            if !runtime.reserve_placement(
                entity,
                request,
                config.interactive_world_physics()
                    && request.is_some_and(|request| request.collision_candidate),
            ) {
                continue;
            }
        }
        commands
            .entity(entity)
            .insert(WorldAssetRoot(handle))
            .remove::<PendingModel>();
        actually_armed += 1;
        choices = choices.saturating_add(1);
    }
    if let Some(state) = scheduling.as_mut() {
        state.activation_choices = choices;
    }
    let armed = actually_armed;
    let depth = backlog.saturating_sub(armed);
    metrics.arming_queue_depth = depth;
    metrics.peak_arming_queue_depth = metrics.peak_arming_queue_depth.max(backlog);
    profiler.increment("streaming/models_armed", armed as u64);
    profiler.set_gauge("streaming/arming_queue_depth", depth as f64);
    profiler.set_gauge(
        "streaming/peak_arming_queue_depth",
        metrics.peak_arming_queue_depth as f64,
    );
    profiler.record_elapsed("streaming/arm_models", started);
}

/// Whether Bevy can instantiate this model now.
///
/// The converted scene being in `Assets<WorldAsset>` is exactly the condition the spawner retries
/// on, so arming on it hands a model over on the first frame it can actually spawn. A model whose
/// load failed will never arrive: it is armed too, so the readiness scan reports the failure
/// exactly as it did when every model was armed at commit, instead of leaving the reference
/// pending for as long as its cell lives.
fn model_can_spawn(
    world_assets: &Assets<WorldAsset>,
    asset_server: &AssetServer,
    handle: &Handle<WorldAsset>,
) -> bool {
    world_assets.contains(handle.id())
        || matches!(
            asset_server
                .get_load_states(handle.id())
                .map(|(load, _, _)| load),
            Some(LoadState::Failed(_))
        )
}

fn mark_world_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingAssetProfile>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

/// Validate spawned model instances, account for failures and record completed readiness latency.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn track_asset_readiness(
    mut commands: Commands,
    config: Res<EngineConfig>,
    asset_server: Res<AssetServer>,
    pending: PendingAssetQuery,
    unarmed: Query<(), (With<PendingAssetProfile>, Without<WorldAssetRoot>)>,
    children: Query<&Children>,
    scene_extras: Query<&GltfSceneExtras>,
    primitives: RenderPrimitiveQuery,
    transforms: Query<(&Transform, &GlobalTransform)>,
    images: Res<Assets<Image>>,
    world_assets: Res<Assets<WorldAsset>>,
    mut fallback_assets: ResMut<DiagnosticFallbackAssets>,
    mut static_cache: ResMut<StaticCollisionCache>,
    (mut meshes, mut materials, camera, mut pacing, clutter, palettes): ReadinessExtras,
    (mut metrics, mut runtime): (
        ResMut<StreamingMetrics>,
        Option<ResMut<runtime::StreamingRuntime>>,
    ),
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    // While flying, how far from the camera each model is when it finishes loading: the number
    // that says how far behind loading falls at speed. The camera is a root entity, so its
    // `Transform` is its world position; `GlobalTransform` only updates in PostUpdate, after
    // `fly_camera` moved the camera in Update, and would lag a frame.
    let fly_camera = config
        .records_fly_lag()
        .then(|| camera.single().ok().map(|camera| camera.translation))
        .flatten();
    // A model still waiting in the arming queue has no scene yet, so the scan below cannot see it;
    // count it here so the readiness gates keep waiting for every queued model.
    metrics.pending_asset_instances = pending.iter().count() + unarmed.iter().count();
    let mut completed_this_scan = 0usize;
    // Live dynamic clutter bodies, including the ones this scan spawns (their commands are
    // deferred, so the query cannot see them yet). The cap bounds what a cell arrival can create.
    let mut live_clutter = clutter.iter().count();
    for (entity, root, pending, local, global, world_transform, expected_bounds) in &pending {
        if completed_this_scan != 0
            && runtime
                .as_ref()
                .and_then(|runtime| runtime.decision)
                .is_some_and(|decision| {
                    started.elapsed().as_micros()
                        >= u128::from(decision.budgets.max_collision_micros)
                })
        {
            break;
        }
        let static_candidate = pending.static_physics
            && fixed_collision_record_eligible(pending.base_record_type.as_deref(), &pending.path);
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
            if static_candidate {
                note_static_proxy_skip(&mut metrics, &mut profiler);
            }
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
                pending.actor_scene,
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
                    if static_candidate {
                        note_static_proxy_skip(&mut metrics, &mut profiler);
                    }
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
                if static_candidate {
                    note_static_proxy_skip(&mut metrics, &mut profiler);
                }
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
                (&palettes.0, palettes.1.as_deref()),
                &images,
            );
            let summary = match validation {
                Ok(summary) => summary,
                Err(reason) => {
                    record_asset_failure(&mut metrics, &mut profiler, pending, vec![reason], true);
                    if static_candidate {
                        note_static_proxy_skip(&mut metrics, &mut profiler);
                    }
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
            // Movable references get exactly one of the two paths: a dynamic clutter body here,
            // or (for an ineligible type) nothing at all, which is what they had before this slice.
            if pending.static_physics
                && dynamic_clutter_record_eligible(pending.base_record_type.as_deref())
            {
                let key = (pending.base_record_type.clone(), pending.path.clone());
                let first_placement = !static_cache.0.contains_key(&key);
                let decision = static_cache.0.entry(key).or_insert_with(|| {
                    clutter_collision_from_hierarchy(entity, &children, &scene_extras)
                });
                match decision {
                    StaticCollisionDecision::Clutter(spec) => {
                        if live_clutter >= crate::physics::MAX_LIVE_DYNAMIC_CLUTTER {
                            metrics.dynamic_clutter_overflow =
                                metrics.dynamic_clutter_overflow.saturating_add(1);
                            profiler.increment("physics/dynamic_clutter_overflow", 1);
                            if first_placement {
                                warn!(
                                    model = %pending.path,
                                    limit = crate::physics::MAX_LIVE_DYNAMIC_CLUTTER,
                                    "dynamic clutter over the live cap; reference keeps no collider"
                                );
                            }
                        } else {
                            crate::physics::spawn_dynamic_clutter(
                                &mut commands,
                                entity,
                                &spec.body,
                                spec.parts
                                    .iter()
                                    .map(|part| (part.translation, &part.collider)),
                                spec.mass,
                                local.scale.x,
                            );
                            live_clutter += 1;
                            metrics.dynamic_clutter_bodies =
                                metrics.dynamic_clutter_bodies.saturating_add(1);
                            profiler.increment("physics/dynamic_clutter_bodies", 1);
                            if spec.convexified {
                                metrics.dynamic_clutter_convexified =
                                    metrics.dynamic_clutter_convexified.saturating_add(1);
                            }
                            if spec.ignored_bodies > 0 {
                                if first_placement {
                                    warn!(
                                        model = %pending.path,
                                        ignored = spec.ignored_bodies,
                                        "model has more than one dynamic body; only the first is simulated"
                                    );
                                }
                                metrics.dynamic_clutter_extra_bodies_ignored = metrics
                                    .dynamic_clutter_extra_bodies_ignored
                                    .saturating_add(spec.ignored_bodies as u64);
                            }
                            if spec.mass.is_none() {
                                metrics.dynamic_clutter_mass_fallbacks =
                                    metrics.dynamic_clutter_mass_fallbacks.saturating_add(1);
                            }
                        }
                    }
                    StaticCollisionDecision::ClutterSkipped(reason) => {
                        if first_placement {
                            warn!(model = %pending.path, %reason, "dynamic clutter skipped");
                        }
                        metrics.dynamic_clutter_skipped =
                            metrics.dynamic_clutter_skipped.saturating_add(1);
                        profiler.increment("physics/dynamic_clutter_skipped", 1);
                    }
                    _ => {}
                }
            }
            if static_candidate {
                let key = (pending.base_record_type.clone(), pending.path.clone());
                let first_placement = !static_cache.0.contains_key(&key);
                let decision = static_cache.0.entry(key).or_insert_with(|| {
                    static_collision_from_hierarchy(
                        pending.base_record_type.as_deref(),
                        &pending.path,
                        entity,
                        &children,
                        &scene_extras,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                });
                match decision {
                    StaticCollisionDecision::Authored(parts, skipped) => {
                        attach_authored_collision(&mut commands, entity, parts);
                        metrics.static_colliders_authored =
                            metrics.static_colliders_authored.saturating_add(1);
                        profiler.increment("physics/static_authored_placements", 1);
                        if !skipped.is_empty() {
                            metrics.static_colliders_partial =
                                metrics.static_colliders_partial.saturating_add(1);
                            profiler.increment("physics/static_authored_partial", 1);
                            if first_placement {
                                warn!(model = %pending.path, reason = %skipped.join("; "), "authored collision is partial");
                            }
                        }
                    }
                    StaticCollisionDecision::RenderProxy(collider) => {
                        commands.entity(entity).insert((
                            StaticColliderSource::RenderProxy,
                            RigidBody::Fixed,
                            collider.clone(),
                            crate::physics::world_collision_groups(),
                        ));
                        metrics.static_colliders_proxy =
                            metrics.static_colliders_proxy.saturating_add(1);
                        profiler.increment("physics/static_proxy_placements", 1);
                    }
                    StaticCollisionDecision::AuthoredAbsent => {
                        metrics.static_colliders_authored_absent =
                            metrics.static_colliders_authored_absent.saturating_add(1);
                        profiler.increment("physics/static_authored_absent", 1);
                    }
                    // A fixed record type never asks for the clutter decision, so these arms cannot
                    // be reached on this path; the movable path above owns them.
                    StaticCollisionDecision::LegacyExcluded
                    | StaticCollisionDecision::ExcludedWithAuthored
                    | StaticCollisionDecision::Clutter(_)
                    | StaticCollisionDecision::ClutterNone
                    | StaticCollisionDecision::ClutterSkipped(_) => {}
                    StaticCollisionDecision::Skipped(reason) => {
                        if first_placement {
                            warn!(model = %pending.path, %reason, "static collision skipped");
                        }
                        note_static_proxy_skip(&mut metrics, &mut profiler);
                    }
                }
            }
            // A record type outside every tier used to drop authored collision without a trace.
            // Report the placements that do carry it so the exclusion is visible in metrics.
            if pending.static_physics
                && fixed_collision_tier(pending.base_record_type.as_deref(), &pending.path)
                    .is_none()
                && !dynamic_clutter_record_eligible(pending.base_record_type.as_deref())
            {
                let key = (pending.base_record_type.clone(), pending.path.clone());
                let first_placement = !static_cache.0.contains_key(&key);
                let decision = static_cache.0.entry(key).or_insert_with(|| {
                    exclusion_decision_from_hierarchy(entity, &children, &scene_extras)
                });
                if matches!(decision, StaticCollisionDecision::ExcludedWithAuthored) {
                    metrics.static_colliders_excluded_authored =
                        metrics.static_colliders_excluded_authored.saturating_add(1);
                    profiler.increment("physics/static_excluded_authored", 1);
                    if first_placement {
                        debug!(
                            record_type = ?pending.base_record_type,
                            model = %pending.path,
                            "authored collision ignored: record type not eligible"
                        );
                    }
                }
            }
            let micros = pending
                .started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            metrics.assets_ready = metrics.assets_ready.saturating_add(1);
            if let Some(camera) = fly_camera {
                pacing.record_model_ready(crate::pacing::horizontal_distance(
                    global.translation(),
                    camera,
                ));
            }
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
            profiler.record_completed_latency_micros("assets/model_ready", micros);
            profiler.event(&pending.path, "asset_ready", Some(micros as f64 / 1000.0));
            commands.entity(entity).remove::<PendingAssetProfile>();
            completed_this_scan += 1;
        }
    }
    metrics.pending_asset_instances = metrics
        .pending_asset_instances
        .saturating_sub(completed_this_scan);
    metrics.instances_completed_this_scan = completed_this_scan as u64;
    metrics.max_instances_completed_per_scan = metrics
        .max_instances_completed_per_scan
        .max(metrics.instances_completed_this_scan);
    profiler.set_gauge(
        "assets/pending_instances",
        metrics.pending_asset_instances as f64,
    );
    profiler.set_gauge(
        "assets/instances_completed_this_scan",
        metrics.instances_completed_this_scan as f64,
    );
    profiler.record_elapsed("assets/readiness_scan", started);
    if let Some(runtime) = runtime.as_mut() {
        runtime.previous_validation_ms = started.elapsed().as_secs_f64() * 1000.0;
    }
}

fn note_static_proxy_skip(metrics: &mut StreamingMetrics, profiler: &mut ProfilingState) {
    metrics.static_colliders_skipped = metrics.static_colliders_skipped.saturating_add(1);
    profiler.increment("physics/static_proxy_skipped", 1);
}
/// Opens the merged data of an exterior cell for drawing, or counts why the cell takes the
/// quadrant path. Every cell with terrain ends in exactly one of the two counters.
fn choose_merged_terrain(
    merge: Option<&mut TerrainMerge>,
    key: CellKey,
    cell_id: u32,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) -> Option<MergedSpawn> {
    let lookup = match merge {
        Some(merge) => match merge.lookup(key, cell_id) {
            CellLookup::Found(cell) => match merged_geometry_check(&cell) {
                Ok(()) => match (
                    merge.classes().cloned(),
                    merge.material().cloned(),
                    merge.material_images().cloned(),
                ) {
                    (Some(classes), Some(material), Some(images)) => {
                        return Some(MergedSpawn {
                            cell,
                            classes,
                            material,
                            images,
                        });
                    }
                    _ => CellLookup::Unavailable,
                },
                Err(reason) => CellLookup::Invalid(reason),
            },
            other => other,
        },
        None => CellLookup::Unavailable,
    };
    count_merged_fallback(metrics, profiler, cell_id, lookup);
    None
}

/// Counts one terrain cell drawn by the quadrant path and says why, once.
fn count_merged_fallback(
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
    cell_id: u32,
    lookup: CellLookup,
) {
    metrics.terrain_cells_fallback = metrics.terrain_cells_fallback.saturating_add(1);
    profiler.increment("terrain/cells_fallback", 1);
    match lookup {
        CellLookup::NoData => {
            metrics.terrain_cells_fallback_expected_merged = metrics
                .terrain_cells_fallback_expected_merged
                .saturating_add(1);
            // One line per cell would flood the log for a worldspace the converter never merged;
            // the counter above is what reports it.
            debug!(
                cell = format_args!("{cell_id:08X}"),
                "no merged terrain data for this cell; drawn by quadrant"
            );
        }
        CellLookup::Invalid(reason) => {
            metrics.terrain_cells_fallback_expected_merged = metrics
                .terrain_cells_fallback_expected_merged
                .saturating_add(1);
            metrics.terrain_cells_merged_invalid =
                metrics.terrain_cells_merged_invalid.saturating_add(1);
            warn!(
                cell = format_args!("{cell_id:08X}"),
                %reason,
                "merged terrain data failed validation; drawn by quadrant"
            );
        }
        CellLookup::Unavailable | CellLookup::Found(_) => {
            debug!(
                cell = format_args!("{cell_id:08X}"),
                "merged terrain is not in use; drawn by quadrant"
            );
        }
    }
}

/// What the merged path needs beyond [`TerrainCellMesh::validate`]: seam duplicates that agree,
/// so the cell has one height field, and no zero normal, which today's validation refuses.
fn merged_geometry_check(cell: &TerrainCellMesh) -> Result<(), String> {
    crate::terrain_merge::mesh::cell_to_grid(cell)?;
    if crate::terrain_merge::mesh::has_zero_normal(cell) {
        return Err("terrain contains a zero-length packed normal".to_owned());
    }
    Ok(())
}

/// Drops the held responses whose load is no longer wanted, releasing each one's outstanding slot.
/// Setup can hold cells for up to [`terrain_merge::SETUP_TIMEOUT`]; a held response that went stale
/// meanwhile must not keep its slot until then. Current held responses keep their place in order.
fn release_stale_held_responses(
    streaming: &mut StreamingWorld,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) {
    let held = std::mem::take(&mut streaming.held_responses);
    for response in held {
        if streaming.awaits(response.key, response.generation) {
            streaming.held_responses.push_back(response);
        } else {
            streaming.consume_cell_response(response.key, response.generation);
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_discarded", None);
        }
    }
}

/// Whether a database response waits for the merged terrain setup: only a loaded exterior cell
/// that has terrain, and only while the setup is in progress. Logs the first hold.
fn should_hold_response(
    merge: &mut TerrainMerge,
    result: &Result<CellPayload, String>,
    has_terrain: impl FnOnce(u32) -> bool,
) -> bool {
    let Ok(payload) = result else {
        return false;
    };
    if !merge.holds_cell(payload.key, has_terrain(payload.cell_id)) {
        return false;
    }
    if merge.note_hold() {
        info!(
            state = %merge.state.label(),
            "holding exterior terrain cells until the land texture arrays are ready"
        );
    }
    true
}

/// Welds a cell's terrain onto its resident neighbours and validates it, choosing its path.
///
/// A merged cell is welded with [`weld_merged_cell`]. If that fails, the cell is not a failed
/// cell: the quadrant path gets the same chance to draw it, and the cell is counted as a fallback
/// with the reason. A cell with no merged data validates and welds as the quadrant path always has.
/// The settled merged data, if the cell keeps it, is returned.
#[allow(clippy::too_many_arguments)]
fn settle_cell_terrain(
    key: CellKey,
    cell_id: u32,
    merged: Option<MergedSpawn>,
    terrain: &mut TerrainSnapshot,
    catalog: &AssetCatalog,
    continuity: &mut TerrainContinuity,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) -> Result<Option<MergedSpawn>, String> {
    let mut merged = merged;
    if let Some(spawn) = &mut merged {
        let weld_started = Instant::now();
        let welded = weld_merged_cell(key, &mut spawn.cell, continuity, metrics);
        profiler.record_elapsed("streaming/terrain_seam_weld", weld_started);
        if let Err(reason) = welded {
            merged = None;
            count_merged_fallback(
                metrics,
                profiler,
                cell_id,
                CellLookup::Invalid(format!("weld failed: {reason}")),
            );
        }
    }
    if merged.is_none() {
        let validation_started = Instant::now();
        let validation = validate_terrain_snapshot(terrain, catalog);
        profiler.record_elapsed("streaming/terrain_validation", validation_started);
        validation?;
        let weld_started = Instant::now();
        let welded = validate_and_register_terrain_edges(key, terrain, continuity, metrics);
        profiler.record_elapsed("streaming/terrain_seam_weld", weld_started);
        welded?;
    }
    Ok(merged)
}

/// Welds a merged cell's edges onto its resident neighbours with the same code the quadrant path
/// uses. The cell's 33 x 33 height and normal grid goes through
/// [`validate_and_register_terrain_edges`] unchanged, and the welded grid is written back to every
/// vertex, seam duplicates included, so the two paths agree sample for sample and either can be
/// the neighbour of the other.
fn weld_merged_cell(
    key: CellKey,
    cell: &mut TerrainCellMesh,
    continuity: &mut TerrainContinuity,
    metrics: &mut StreamingMetrics,
) -> Result<(), String> {
    let (heights, normals) = crate::terrain_merge::mesh::cell_to_grid(cell)?;
    let mut grid = TerrainSnapshot {
        cell_id: cell.cell_id,
        width: shared::LAND_SIDE,
        height: shared::LAND_SIDE,
        heights,
        normals,
        vertex_colors: Vec::new(),
        layers: Vec::new(),
        water_height: None,
        water_type_form_id: None,
    };
    validate_and_register_terrain_edges(key, &mut grid, continuity, metrics)?;
    crate::terrain_merge::mesh::write_grid(cell, &grid.heights, &grid.normals);
    Ok(())
}

/// Makes a spawned merged terrain mesh visible and its collider live. Its arrays were validated
/// before any cell was committed, so there is nothing left to wait for.
fn track_merged_surface_readiness(
    mut commands: Commands,
    pending: Query<Entity, With<PendingMergedTerrain>>,
    mut lod_streaming: ResMut<lod::LodStreaming>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    for entity in &pending {
        metrics.terrain_patches_validated = metrics.terrain_patches_validated.saturating_add(1);
        profiler.increment("terrain/patches_validated", 1);
        commands
            .entity(entity)
            .insert((Visibility::Inherited, TerrainSurfaceReady))
            .remove::<(ColliderDisabled, PendingMergedTerrain)>();
        lod_streaming.visibility_dirty = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn track_surface_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    images: Res<Assets<Image>>,
    terrain: Query<(
        Entity,
        &PendingTerrainProfile,
        Option<&MeshMaterial3d<TerrainMaterial>>,
    )>,
    water: Query<(Entity, &PendingWaterProfile)>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    metrics.pending_surface_instances = terrain.iter().count() + water.iter().count();
    let mut completed = 0usize;
    for (entity, pending, material) in &terrain {
        let mut normals_dropped = false;
        let state = match validate_surface_dependencies(
            &asset_server,
            &images,
            &pending.images,
            SurfaceImageKind::Terrain,
        ) {
            SurfaceDependencyState::Ready => {
                match validate_surface_dependencies(
                    &asset_server,
                    &images,
                    &pending.normals,
                    SurfaceImageKind::TerrainNormal,
                ) {
                    // A normal map is optional detail: without it the quadrant is lit by its
                    // geometric normal, as a quadrant whose layers have none is.
                    SurfaceDependencyState::Failed(reason) => {
                        warn!(
                            cell = format_args!("{:08X}", pending.cell_id),
                            quadrant = pending.quadrant,
                            %reason,
                            "terrain normal map failed; drawing the quadrant without normal maps"
                        );
                        if let Some(mut material) =
                            material.and_then(|material| terrain_materials.get_mut(&material.0))
                        {
                            material.extension.drop_normal_maps();
                        }
                        // The swap bridge waits on the images the material binds, so the failed
                        // normal maps leave the quadrant's declaration with them: an image that
                        // will never reach the render world is one the surface could never stop
                        // waiting for, and a later change could fail a settle loudly.
                        let bound: Vec<_> =
                            pending.images.iter().map(|handle| handle.id()).collect();
                        commands
                            .entity(entity)
                            .entry::<SwapSurface>()
                            .and_modify(move |mut surface| surface.set_images(bound));
                        profiler.increment("terrain/normal_map_fallbacks", 1);
                        normals_dropped = true;
                        SurfaceDependencyState::Ready
                    }
                    other => other,
                }
            }
            other => other,
        };
        match state {
            SurfaceDependencyState::Pending => {}
            SurfaceDependencyState::Ready => {
                let normals_validated = if normals_dropped {
                    0
                } else {
                    pending.normals.len()
                };
                metrics.terrain_patches_validated =
                    metrics.terrain_patches_validated.saturating_add(1);
                metrics.materials_validated = metrics.materials_validated.saturating_add(1);
                metrics.images_validated = metrics
                    .images_validated
                    .saturating_add((pending.images.len() + normals_validated) as u64);
                profiler.increment("terrain/patches_validated", 1);
                commands
                    .entity(entity)
                    .insert((Visibility::Inherited, TerrainSurfaceReady));
                commands.entity(entity).remove::<ColliderDisabled>();
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
        match validate_surface_dependencies(
            &asset_server,
            &images,
            &handles,
            SurfaceImageKind::Water,
        ) {
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

#[derive(Clone, Copy)]
enum SurfaceImageKind {
    Terrain,
    TerrainNormal,
    Water,
}

fn terrain_sampler_repeats(sampler: &ImageSampler) -> bool {
    matches!(
        sampler,
        ImageSampler::Descriptor(descriptor)
            if descriptor.address_mode_u == ImageAddressMode::Repeat
                && descriptor.address_mode_v == ImageAddressMode::Repeat
    )
}

fn validate_surface_dependencies(
    asset_server: &AssetServer,
    images: &Assets<Image>,
    handles: &[Handle<Image>],
    kind: SurfaceImageKind,
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
        if image.texture_descriptor.format.is_srgb() != matches!(kind, SurfaceImageKind::Terrain) {
            return SurfaceDependencyState::Failed(format!(
                "image {:?} has wrong color space {:?}",
                handle.id(),
                image.texture_descriptor.format
            ));
        }
        if let Err(reason) = validate_image_sampler("surface", &image.sampler) {
            return SurfaceDependencyState::Failed(reason);
        }
        if matches!(
            kind,
            SurfaceImageKind::Terrain | SurfaceImageKind::TerrainNormal
        ) && !terrain_sampler_repeats(&image.sampler)
        {
            return SurfaceDependencyState::Failed(format!(
                "terrain image {:?} did not load with a repeating sampler",
                handle.id()
            ));
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
    actor_scene: bool,
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
    let expected = match expected {
        Some(expected) => Some(
            ExpectedModelBounds::new(expected.min, expected.max).ok_or_else(|| {
                "converted model bounds are non-finite, empty, or inverted".to_owned()
            })?,
        ),
        None => {
            // A model the converter wrote no aggregate bounds for is an empty scene: a model with
            // no renderable geometry, so there is nothing to place, draw or bound. What makes it
            // empty is read from the converted model itself - the scene the converter wrote, which
            // declares no node and no mesh - rather than from the spawned instance, so a hierarchy
            // that has not spawned yet cannot pass as an empty model. A scene that does declare a
            // node or a mesh and still arrived without aggregate bounds is a conversion defect,
            // unless it is an actor scene: actor references have no `statics` row, so their bounds
            // come from the spawned hierarchy itself.
            let scene = converted_model.ok_or_else(|| {
                "the converted model is not loaded while validating its bounds; reconvert the asset"
                    .to_owned()
            })?;
            let contents = converted_scene_contents(scene);
            if contents.is_empty() {
                return Ok(TransformValidationSummary {
                    nodes: 0,
                    empty_model: true,
                });
            }
            if !actor_scene {
                return Err(format!(
                    "converted model has no validated aggregate bounds; reconvert the asset (its converted scene is not empty: {} mesh primitives, {} nodes)",
                    contents.meshes, contents.nodes
                ));
            }
            None
        }
    };

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
    if let Some(expected) = expected {
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
        if let Ok((mesh_handle, _, _, _, _)) = primitives.get(entity) {
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
    (palettes, palette_materials): (
        &Query<EffectPaletteSurface>,
        Option<&Assets<EffectPaletteMaterial>>,
    ),
    images: &Assets<Image>,
) -> Result<AssetValidationSummary, String> {
    let mut summary = AssetValidationSummary::default();
    for descendant in children.iter_descendants(root) {
        let Ok((mesh, material_handle, _, extras, native_source)) = primitives.get(descendant)
        else {
            continue;
        };
        if meshes.get(mesh).is_none() {
            return Err(format!(
                "mesh {:?} is absent after scene readiness",
                mesh.id()
            ));
        }
        summary.meshes += 1;
        if let Some(palette_handle) = palettes
            .get(descendant)
            .ok()
            .and_then(effect_palette_handle)
        {
            let material = palette_materials
                .and_then(|materials| materials.get(palette_handle))
                .ok_or_else(|| {
                    format!(
                        "effect palette material {:?} is absent after scene readiness",
                        palette_handle.id()
                    )
                })?;
            summary.images += validate_effect_palette_material(material, images)?;
            summary.materials += 1;
            continue;
        }
        let Some(material_handle) = material_handle
            .map(|handle| &handle.0)
            .or_else(|| native_source.map(|source| &source.0))
        else {
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
        validate_material_image(slot, handle, expects_srgb, images)?;
        validated += 1;
    }
    Ok(validated)
}

/// Validates the standard half of an effect palette material, then its palette. The palette must
/// be sampled through an sRGB view: the palette shader takes its colour as decoded on sampling.
pub(crate) fn validate_effect_palette_material(
    material: &EffectPaletteMaterial,
    images: &Assets<Image>,
) -> Result<usize, String> {
    let validated = validate_standard_material(&material.base, images)?;
    validate_material_image("palette", material.extension.palette(), true, images)?;
    Ok(validated + 1)
}

fn validate_material_image(
    slot: &str,
    handle: &Handle<Image>,
    expects_srgb: bool,
    images: &Assets<Image>,
) -> Result<(), String> {
    let image = images
        .get(handle)
        .ok_or_else(|| format!("{slot} image {:?} is not loaded", handle.id()))?;
    let descriptor = &image.texture_descriptor;
    if descriptor.size.width == 0 || descriptor.size.height == 0 || descriptor.mip_level_count == 0
    {
        return Err(format!("{slot} image has invalid dimensions or mip levels"));
    }
    if descriptor.format.is_srgb() != expects_srgb {
        return Err(format!(
            "{slot} image color space mismatch: {:?}",
            descriptor.format
        ));
    }
    validate_image_sampler(slot, &image.sampler)
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

pub(crate) fn creation_to_bevy(position: Vec3) -> Vec3 {
    Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        position.to_array(),
    ))
}

pub(crate) fn creation_rotation_to_bevy(rotation: [f32; 3]) -> Quat {
    Quat::from_array(shared::coordinates::creation_euler_to_runtime_quaternion(
        rotation,
    ))
}

/// The converted model a reference draws, or `None` when it draws nothing. A base record flagged
/// `Is Marker` is editor-only and never drawn, whatever its model.
fn spawned_model_path(model_path: Option<String>, base_is_marker: bool) -> Option<String> {
    model_path
        .filter(|_| !base_is_marker)
        .and_then(converted_model_path)
}

pub(crate) fn converted_model_path(path: String) -> Option<String> {
    // Converted assets are published with lowercase canonical paths, so the
    // lookup must lowercase too (matching world-inspect's resolver).
    // The filename is no marker signal: forges and the Civil War map flags carry
    // "marker" and must draw. Editor markers are excluded by the base record's
    // `Is Marker` flag (`spawned_model_path`) and, inside a mesh, by the converter's
    // `EditorMarker` shape rule; the folder rules below stay as path-convention guards.
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    if normalized.starts_with("meshes/sky/")
        || normalized.starts_with("sky/")
        || normalized.starts_with("meshes/markers/")
        || normalized.starts_with("markers/")
        || normalized.starts_with("meshes/effects/")
        || normalized.starts_with("effects/")
    {
        return None;
    }
    let without_prefix = normalized.strip_prefix("meshes/").unwrap_or(&normalized);
    if without_prefix.is_empty() || !is_safe_relative_asset_path(without_prefix) {
        return None;
    }
    let mut converted = std::path::PathBuf::from("meshes").join(without_prefix);
    converted.set_extension("glb");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

/// Rejects a database-supplied relative path that could escape the assets
/// root once re-rooted under `meshes/` with `PathBuf::join`. A `..` segment
/// walks back out of the base directory, and a rooted or drive-prefixed path
/// makes `PathBuf::join` replace the base entirely instead of appending to it
/// (see the `std::path::PathBuf::push` docs). Every component must therefore
/// be a plain, non-empty path segment.
fn is_safe_relative_asset_path(path: &str) -> bool {
    std::path::Path::new(path)
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
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

/// Validates terrain geometry while retaining unresolved texture layers and their weights.
/// Missing diffuse images use the same material fallback as normalized zero-ID layers.
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
    let mut missing_diffuse = HashSet::new();
    for quadrant in 0..4 {
        for layer in quadrant_layers(terrain, quadrant)? {
            if layer.texture_form_id == 0 {
                continue;
            }
            if catalog.landscape_diffuse(layer.texture_form_id).is_none()
                && missing_diffuse.insert(layer.texture_form_id)
            {
                warn!(
                    cell_id = terrain.cell_id,
                    texture_form_id = layer.texture_form_id,
                    "LAND texture has no converted diffuse image; retaining its layer and weights with a missing-image fallback"
                );
            }
        }
    }
    Ok(())
}

/// The dense weight field of one quadrant: one [`QUADRANT_WEIGHT_SAMPLES`]-square grid per overlay
/// layer, indexed by the raw `VTXT` vertex value, in the same order as [`quadrant_layers`] (base
/// first, so slot 0 is the first overlay). A grid point no `VTXT` entry names is opacity 0. Both the
/// mesh's packed vertex weights and the material's uniform weight field are built from this, so they
/// cannot drift apart.
pub(crate) fn quadrant_overlay_weights(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Vec<Vec<f32>>, String> {
    let layers = quadrant_layers(terrain, quadrant)?;
    let samples = QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES;
    let mut overlay_weights = vec![vec![0.0f32; samples]; layers.len().saturating_sub(1)];
    for (slot, layer) in layers.iter().skip(1).enumerate() {
        for &(vertex, opacity) in &layer.weights {
            overlay_weights[slot][usize::from(vertex)] = opacity;
        }
    }
    Ok(overlay_weights)
}

pub(crate) fn build_terrain_quadrant_mesh(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Mesh, String> {
    let overlay_weights = quadrant_overlay_weights(terrain, quadrant)?;
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err("terrain must contain a complete 33x33 height field".to_owned());
    }
    let step_x = CELL_SIZE / (width - 1) as f32;
    let step_z = CELL_SIZE / (height - 1) as f32;
    let origin_x = usize::from(quadrant % 2) * 16;
    let origin_y = usize::from(quadrant / 2) * 16;
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
            // The packed vertex weights are the fallback for a material with no weight field, and
            // for a quadrant whose only layer is its base: weights 1-3 as a unit direction plus its
            // magnitude in `w`, weights 4-5 in the second UV set. Bevy re-normalizes
            // `world_tangent.xyz` in the vertex shader, so this carrier sharpens every transition
            // (`0.25` where the true interpolated weight is `0.5`); every other quadrant reads the
            // material's weight field instead.
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

/// Recomputes packed `VNML` bytes from the welded height field, normalized and scaled to `i8`,
/// with the converter's stencil (`crates/converter/src/esm/cell_cache.rs`, `decode_normals`):
/// each axis's height difference is divided by the distance it actually spans, two intervals
/// inside the field and one at an edge or corner, so a planar slope keeps the same normal
/// everywhere. `points` index a complete `width * height` field.
fn recompute_packed_normals(terrain: &mut TerrainSnapshot, points: &[usize]) {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width < 2 || height < 2 || terrain.normals.len() != width * height * 3 {
        return;
    }
    let step = CELL_SIZE / (width - 1) as f32;
    for &index in points {
        let (x, y) = (index % width, index / width);
        let (west, east) = (x.saturating_sub(1), (x + 1).min(width - 1));
        let (south, north) = (y.saturating_sub(1), (y + 1).min(height - 1));
        let left = terrain.heights[y * width + west];
        let right = terrain.heights[y * width + east];
        let down = terrain.heights[south * width + x];
        let up = terrain.heights[north * width + x];
        // `width` and `height` are at least 2, so every span covers at least one interval.
        let span_x = (east - west) as f32 * step;
        let span_y = (north - south) as f32 * step;
        let normal =
            Vec3::new((left - right) / span_x, (down - up) / span_y, 1.0).normalize_or(Vec3::Z);
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

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_render_origin(
    config: Res<EngineConfig>,
    space: Res<ActiveSpace>,
    mut origin: ResMut<RenderOrigin>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut roots: Query<
        (
            Option<&ExteriorCellGrid>,
            Option<&lod::LodChunkGridOrigin>,
            &mut Transform,
        ),
        (
            Or<(With<ExteriorCellGrid>, With<lod::LodChunkGridOrigin>)>,
            Without<StreamingCamera>,
            Without<PlayerBody>,
            Without<DebugTankard>,
        ),
    >,
    mut participants: Query<
        (Entity, &mut Transform),
        (
            Or<(With<PlayerBody>, With<DebugTankard>)>,
            Without<StreamingCamera>,
        ),
    >,
    terrain: Query<Entity, With<TerrainCollider>>,
    static_colliders: Query<Entity, With<StaticColliderSource>>,
    clutter: Query<Entity, With<crate::physics::DynamicClutter>>,
    mut physics: WriteRapierContext,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    // Screenshot runs keep the start-cell origin fixed: rebasing toward an
    // artistically offset camera would drag the streamed window (and the
    // screenshot target's cells) away from the framed view.
    if config.acceptance_screenshot.is_some() {
        return;
    }
    // An interior is placed in absolute coordinates around the origin it was entered with, so the
    // origin stays put while the player is inside one; leaving it sets the origin afresh.
    if space.interior.is_some() {
        return;
    }
    let started = Instant::now();
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    let shift = rebase_shift(camera.translation);
    if shift == IVec2::ZERO {
        return;
    }
    origin.0 += shift;
    let displacement = Vec3::new(
        shift.x as f32 * CELL_SIZE,
        0.0,
        -(shift.y as f32) * CELL_SIZE,
    );
    camera.translation -= displacement;
    let mut bodies = Vec::new();
    for (entity, mut transform) in &mut participants {
        transform.translation -= displacement;
        bodies.push(entity);
    }
    bodies.extend(terrain.iter());
    bodies.extend(static_colliders.iter());
    // Dynamic clutter is a cell-root descendant, so the root's move already shifts its Transform;
    // only its Rapier body is teleported here. Adding it to `participants` instead would shift the
    // Transform a second time and double the displacement. The teleport keeps its velocity.
    bodies.extend(clutter.iter());
    if let Ok(mut context) = physics.single_mut() {
        for entity in bodies {
            if let Some(handle) = context.entity2body().get(&entity).copied()
                && let Some(body) = context.rigidbody_set.bodies.get_mut(handle)
            {
                body.set_translation(body.translation() - displacement, false);
            }
        }
        context.propagate_modified_body_positions_to_colliders();
    }
    for (cell_grid, lod_grid, mut transform) in &mut roots {
        let Some(grid) = cell_grid
            .map(|grid| (i64::from(grid.0.x), i64::from(grid.0.y)))
            .or_else(|| lod_grid.map(|grid| (grid.grid_x, grid.grid_y)))
        else {
            continue;
        };
        transform.translation = Vec3::new(
            (grid.0 - i64::from(origin.0.x)) as f32 * CELL_SIZE,
            0.0,
            -((grid.1 - i64::from(origin.0.y)) as f32) * CELL_SIZE,
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

#[allow(clippy::too_many_arguments)]
fn validate_streaming_lifecycle(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    streaming: Res<StreamingWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<(Entity, &CellRef, Option<&ExteriorCellGrid>), With<StreamedCellRoot>>,
    static_colliders: Query<(), With<StaticColliderSource>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let active_requests = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Loading { .. }))
        .count();
    // A retiring cell still owns its root until the unload budget reaches it, so its root is
    // accounted for here rather than reading as orphaned.
    let resident_entities: HashSet<_> = streaming
        .cells
        .values()
        .filter_map(|status| match status {
            CellStatus::Resident { root } | CellStatus::Retiring { root } => Some(*root),
            _ => None,
        })
        .collect();
    let retiring_entities: HashSet<_> = streaming
        .cells
        .values()
        .filter_map(|status| match status {
            CellStatus::Retiring { root } => Some(*root),
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
    let unload_radius = config.unload_radius;
    let camera_translation = camera.single().ok().map(|camera| camera.translation);
    // Must match plan_cells: screenshot runs anchor streaming on the start cell, not the camera.
    let current_center =
        camera_translation.map(|translation| window_center(&config, translation, origin.0));
    let planned_center = streaming.planned_center;
    let out_of_range_roots = current_center.map_or(0, |center| {
        roots_outside_window(&root_entries, &retiring_entities, center, unload_radius).count()
            as u64
    });
    let out_of_range_vs_planned = planned_center.map_or(0, |center| {
        roots_outside_window(&root_entries, &retiring_entities, center, unload_radius).count()
            as u64
    });
    let plan_check_offset = planned_center
        .zip(current_center)
        .map(|(planned, current)| current - planned);
    let violations = duplicate_roots + orphaned_roots + missing_roots + out_of_range_roots;

    metrics.active_requests = active_requests;
    metrics.peak_active_requests = metrics.peak_active_requests.max(active_requests);
    metrics.resident_roots = root_entries.len();
    metrics.resident_static_colliders = static_colliders.iter().count();
    metrics.duplicate_cell_roots = metrics.duplicate_cell_roots.max(duplicate_roots);
    metrics.orphaned_cell_roots = metrics.orphaned_cell_roots.max(orphaned_roots);
    metrics.missing_cell_roots = metrics.missing_cell_roots.max(missing_roots);
    metrics.out_of_range_cell_roots = metrics.out_of_range_cell_roots.max(out_of_range_roots);
    metrics.out_of_range_vs_planned_roots = metrics
        .out_of_range_vs_planned_roots
        .max(out_of_range_vs_planned);
    if let Some(offset) = plan_check_offset {
        metrics.max_plan_check_center_offset = metrics
            .max_plan_check_center_offset
            .max(offset.abs().max_element());
    }
    if planned_center.is_none() {
        metrics.invariant_checks_without_plan =
            metrics.invariant_checks_without_plan.saturating_add(1);
    }
    if violations > metrics.streaming_invariant_failures {
        // The first offending roots measured against the check-time centre, each with its
        // distance from both centres: one trailing column is drift, scattered cells are a leak.
        let sample: Vec<_> = current_center
            .map(|center| {
                roots_outside_window(&root_entries, &retiring_entities, center, unload_radius)
                    .take(8)
                    .map(|grid| {
                        (
                            grid,
                            (grid - center).abs().max_element(),
                            planned_center.map(|planned| (grid - planned).abs().max_element()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        error!(
            duplicate_roots,
            orphaned_roots,
            missing_roots,
            out_of_range_roots,
            out_of_range_vs_planned,
            planned_center = ?planned_center,
            current_center = ?current_center,
            plan_check_offset = ?plan_check_offset,
            camera_translation = ?camera_translation,
            origin = ?origin.0,
            stream_radius = config.stream_radius,
            unload_radius,
            sample = ?sample,
            "streaming lifecycle invariant failed"
        );
        profiler.event("streaming", "invariant_failed", None);
    }
    metrics.streaming_invariant_failures = metrics.streaming_invariant_failures.max(violations);
    profiler.set_gauge("streaming/active_requests", active_requests as f64);
    profiler.set_gauge("streaming/resident_roots", root_entries.len() as f64);
}

impl TerrainCoverage {
    pub(crate) fn receipt(&self) -> serde_json::Value {
        serde_json::json!({
            "cell": self.grid.to_array(),
            "quadrant": self.quadrant,
            "tier": format!("{:?}", self.tier)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{
        Actor, ActorHead, ActorHeadSource, ActorParts, ActorSlot, ActorSpawnDiagnostic,
        ActorSpawnSkip,
    };
    use bevy::ecs::system::RunSystemOnce;
    use bevy_rapier3d::prelude::{
        AdditionalMassProperties, ColliderMassProperties, QueryFilter, ReadRapierContext, Velocity,
    };

    /// A broken pack re-records the same failure on every cell commit, so the collector, not
    /// the exporter, must hold the bound: the log keeps the newest entries and the exact total.
    #[test]
    fn asset_failures_keep_the_newest_entries_and_an_exact_total() {
        let mut metrics = StreamingMetrics::default();
        let recorded = MAX_ASSET_FAILURES + 68;
        for id in 0..recorded as u32 {
            metrics.asset_failures.push(AssetFailure {
                model_path: format!("meshes/model-{id}.nif"),
                reference_form_id: id,
                base_form_id: 0,
                cell_id: 7,
                dependency_chain: vec!["missing texture".to_owned()],
            });
        }
        assert_eq!(metrics.asset_failures.len(), MAX_ASSET_FAILURES);
        assert_eq!(
            metrics
                .asset_failures
                .iter()
                .next_back()
                .expect("the newest failure is retained")
                .reference_form_id,
            recorded as u32 - 1
        );
        assert_eq!(
            metrics
                .asset_failures
                .first()
                .expect("the oldest retained failure")
                .reference_form_id,
            68,
            "the ring drops only the oldest entries"
        );
        assert_eq!(metrics.asset_failures.total(), recorded as u64);
        assert!(!metrics.asset_failures.is_empty());
        // The reported streaming JSON keeps its array shape for the comparison scripts; the
        // exact count is published by the bug report.
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(
            json["asset_failures"].as_array().unwrap().len(),
            MAX_ASSET_FAILURES
        );
    }

    #[test]
    fn a_capsule_hull_covers_the_diagonals_of_its_end_spheres() {
        let radius = 2.0;
        let capsule = CollisionShape::Capsule {
            a: [0.0, 0.0, 0.0],
            b: [0.0, 0.0, 0.0],
            radius,
        };
        let mut points = Vec::new();
        shape_hull_points(&capsule, &mut points);
        assert_eq!(points.len(), 52, "26 points at each of the two ends");
        // The hull's reach along a direction is the largest dot product over its points. Along
        // any direction it must stay close to the sphere's radius; six axis points only reach
        // 0.577 of it along a corner diagonal.
        let mut worst = f32::MAX;
        for x in -4..=4 {
            for y in -4..=4 {
                for z in -4..=4 {
                    let direction = Vec3::new(x as f32, y as f32, z as f32);
                    if direction == Vec3::ZERO {
                        continue;
                    }
                    let direction = direction.normalize();
                    let reach = points
                        .iter()
                        .map(|point| point.dot(direction))
                        .fold(f32::MIN, f32::max);
                    worst = worst.min(reach / radius);
                }
            }
        }
        assert!(worst > 0.88, "the hull falls short of the sphere: {worst}");
    }

    #[test]
    fn one_commit_budget_alternates_priority_without_starving_cells_or_lod() {
        let mut budget = StreamingCommitBudget {
            remaining: 1,
            ..Default::default()
        };
        budget.reserve_for_lod(true, 1);
        assert_eq!(budget.remaining_for_cells(), 1);
        budget.lod_priority = true;
        budget.reserve_for_lod(true, 1);
        assert_eq!(budget.remaining_for_cells(), 0);
        budget.reserve_for_lod(false, 1);
        assert_eq!(budget.remaining_for_cells(), 1);
        budget.remaining = 2;
        budget.lod_priority = false;
        budget.reserve_for_lod(true, 2);
        assert_eq!(budget.remaining_for_cells(), 1);
        budget.remaining -= 1;
        assert_eq!(
            budget.remaining_for_cells(),
            0,
            "reservation survives a near-cell commit"
        );
        budget.remaining = 0;
        assert_eq!(budget.remaining_for_cells(), 0);
    }

    /// The converter writes a sphere as a zero-length capsule (a = b); Rapier must take it as a
    /// sphere of that radius.
    #[test]
    fn zero_length_authored_capsule_becomes_a_sphere() {
        let asset = CollisionAsset {
            version: COLLISION_ASSET_VERSION,
            authored: true,
            shapes: vec![CollisionShape::Capsule {
                a: [1.0, 2.0, 3.0],
                b: [1.0, 2.0, 3.0],
                radius: 5.0,
            }],
            skipped: Vec::new(),
            bodies: Vec::new(),
        };
        let parts = collider_parts_from_authored(&asset).unwrap();
        assert_eq!(parts.len(), 1);
        let aabb = parts[0].collider.raw.compute_local_aabb();
        let (mins, maxs) = (aabb.mins, aabb.maxs);
        assert!(
            (mins - Vec3::new(-4.0, -3.0, -2.0)).length() < 1e-4
                && (maxs - Vec3::new(6.0, 7.0, 8.0)).length() < 1e-4,
            "{mins:?} {maxs:?}"
        );
    }

    #[test]
    fn multiple_authored_meshes_attach_to_one_fixed_body_without_nested_composites() {
        let mut app = crate::physics::headless::fixture_app();
        let asset = CollisionAsset {
            version: COLLISION_ASSET_VERSION,
            authored: true,
            shapes: [0.0_f32, 100.0]
                .map(|x| CollisionShape::Mesh {
                    vertices: vec![
                        [x - 20.0, 0.0, -20.0],
                        [x - 20.0, 0.0, 20.0],
                        [x + 20.0, 0.0, 20.0],
                        [x + 20.0, 0.0, -20.0],
                    ],
                    triangles: vec![[0, 1, 2], [0, 2, 3]],
                })
                .into(),
            skipped: Vec::new(),
            bodies: Vec::new(),
        };
        let parts = collider_parts_from_authored(&asset).unwrap();
        let root = app
            .world_mut()
            .spawn(Transform::from_xyz(10_000.0, 0.0, 0.0))
            .id();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                attach_authored_collision(&mut commands, root, &parts);
            })
            .unwrap();
        for _ in 0..3 {
            app.update();
        }
        let hits = app
            .world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                let context = context.single().unwrap();
                let body = context
                    .rigidbody_set
                    .bodies
                    .get(context.entity2body()[&root])
                    .unwrap();
                assert_eq!(body.colliders().len(), 2);
                [0.0_f32, 100.0].map(|x| {
                    context.cast_ray(
                        Vec3::new(10_000.0 + x, 50.0, 0.0),
                        Vec3::NEG_Y,
                        100.0,
                        true,
                        QueryFilter::default(),
                    )
                })
            })
            .unwrap();
        for hit in hits {
            let (_, distance) = hit.expect("authored mesh must be queryable");
            assert!(
                (distance - 50.0).abs() < 0.01,
                "unexpected contact: {distance}"
            );
        }
    }

    #[test]
    fn authored_collision_and_absence_override_legacy_render_proxy_policy() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>();
        assert!(fixed_collision_record_eligible(Some("FURN"), ""));
        assert!(!fixed_collision_record_eligible(Some("MISC"), ""));
        assert!(!static_proxy_eligible(
            Some("FURN"),
            "meshes/furniture/clutter/milllogpile.glb"
        ));
        for (shapes, expected_solid) in [
            (
                vec![CollisionShape::Box {
                    center: [0.0, 5.0, 0.0],
                    half_extents: [10.0, 5.0, 10.0],
                }],
                true,
            ),
            (Vec::new(), false),
        ] {
            let root = app.world_mut().spawn_empty().id();
            let asset = CollisionAsset {
                version: COLLISION_ASSET_VERSION,
                authored: true,
                shapes,
                skipped: Vec::new(),
                bodies: Vec::new(),
            };
            app.world_mut().spawn((
                GltfSceneExtras {
                    value: serde_json::json!({"mudcrabCollision": asset}).to_string(),
                },
                ChildOf(root),
            ));
            for record_type in ["STAT", "FURN"] {
                let decision = app
                    .world_mut()
                    .run_system_once(
                        move |children: Query<&Children>,
                              scene_extras: Query<&GltfSceneExtras>,
                              transforms: Query<(&Transform, &GlobalTransform)>,
                              primitives: RenderPrimitiveQuery,
                              meshes: Res<Assets<Mesh>>,
                              materials: Res<Assets<StandardMaterial>>| {
                            static_collision_from_hierarchy(
                                Some(record_type),
                                "meshes/architecture/farmhouse/inn01.glb",
                                root,
                                &children,
                                &scene_extras,
                                &transforms,
                                &primitives,
                                &meshes,
                                &materials,
                            )
                        },
                    )
                    .unwrap();
                assert_eq!(
                    matches!(decision, StaticCollisionDecision::Authored(_, _)),
                    expected_solid
                );
                assert!(matches!(
                    decision,
                    StaticCollisionDecision::Authored(_, _)
                        | StaticCollisionDecision::AuthoredAbsent
                ));
            }
        }
    }

    fn authored_box_shape() -> CollisionShape {
        CollisionShape::Box {
            center: [0.0, 1.0, 0.0],
            half_extents: [1.0, 1.0, 1.0],
        }
    }

    fn authored_mesh_shape() -> CollisionShape {
        CollisionShape::Mesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2]],
        }
    }

    fn authored_hull_shape() -> CollisionShape {
        CollisionShape::Hull {
            points: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
            ],
        }
    }

    fn authored_asset(shapes: Vec<CollisionShape>, bodies: Vec<CollisionBody>) -> CollisionAsset {
        CollisionAsset {
            version: COLLISION_ASSET_VERSION,
            authored: true,
            shapes,
            skipped: Vec::new(),
            bodies,
        }
    }

    #[test]
    fn fixed_collision_tier_splits_fixed_and_authored_only_records() {
        assert_eq!(
            fixed_collision_tier(Some("ACTI"), ""),
            Some(FixedCollisionTier::AuthoredOnly)
        );
        for record in ["STAT", "TREE", "FURN"] {
            assert_eq!(
                fixed_collision_tier(Some(record), ""),
                Some(FixedCollisionTier::Full),
                "{record}"
            );
        }
        for record in ["DOOR", "MSTT", "FLOR", "CONT", "MISC"] {
            assert_eq!(fixed_collision_tier(Some(record), ""), None, "{record}");
        }
        assert_eq!(fixed_collision_tier(None, ""), None);
        assert!(fixed_collision_record_eligible(Some("ACTI"), ""));
    }

    fn verified_container_asset() -> CollisionAsset {
        let mut body = clutter_body(BodyKind::Fixed, vec![0], true);
        body.node = 0;
        body.target = "Barrel01".to_owned();
        body.mass = 0.0;
        body.havok.motion_system = 5;
        body.havok.quality_type = 0;
        body.havok.collision_layer = 1;
        authored_asset(vec![authored_box_shape()], vec![body])
    }

    #[test]
    fn container_collision_tier_requires_an_audited_model_and_cont_base() {
        for path in [
            "meshes/clutter/barrel01.glb",
            "meshes/clutter/tgbarrel01.glb",
            "meshes/clutter/sovngarde/sovbarrel01.glb",
            "meshes/clutter/sburialurn01.glb",
        ] {
            assert_eq!(
                fixed_collision_tier(Some("CONT"), path),
                Some(FixedCollisionTier::VerifiedContainer)
            );
            assert!(!dynamic_clutter_record_eligible(Some("CONT")));
            assert!(!static_proxy_eligible(Some("CONT"), path));
            for record in ["MISC", "MSTT", "FLOR", "DOOR", "TACT", "LIGH"] {
                assert_eq!(fixed_collision_tier(Some(record), path), None);
            }
        }
        for path in [
            "meshes/clutter/upperclass/upperchest01.glb",
            "meshes/clutter/ruins/ruinslargeurnwithstand01.glb",
            "meshes/clutter/barrel01extra.glb",
            "meshes/architecture/barrel01.glb",
        ] {
            assert_eq!(fixed_collision_tier(Some("CONT"), path), None);
            assert!(!static_proxy_eligible(Some("CONT"), path));
        }
    }

    #[test]
    fn container_collision_keeps_all_shapes_of_the_verified_root_body() {
        let mut asset = verified_container_asset();
        asset.shapes.push(authored_hull_shape());
        asset.bodies[0].shapes.push(1);
        match static_decision_from_asset(FixedCollisionTier::VerifiedContainer, &asset) {
            StaticCollisionDecision::Authored(parts, notes) => {
                assert_eq!(parts.len(), 2);
                assert!(notes.is_empty());
            }
            _ => panic!("an intact audited root body must be solid"),
        }
    }

    #[test]
    fn container_collision_defers_ambiguous_moving_or_nonphysical_assets() {
        let asset = verified_container_asset();
        let mut cases = Vec::new();
        let mut legacy = asset.clone();
        legacy.version = 1;
        legacy.bodies.clear();
        cases.push(("legacy", legacy));
        let mut unbound = asset.clone();
        unbound.bodies.clear();
        cases.push(("lost body", unbound));
        let mut unowned = asset.clone();
        unowned.shapes.push(authored_box_shape());
        cases.push(("unowned shape", unowned));
        let mut invalid = asset.clone();
        invalid.bodies[0].shapes.push(u32::MAX);
        cases.push(("missing shape", invalid));
        let mut mixed = asset.clone();
        mixed.shapes.push(authored_box_shape());
        mixed
            .bodies
            .push(clutter_body(BodyKind::Dynamic, vec![1], true));
        cases.push(("mixed bodies", mixed));
        for kind in [BodyKind::Dynamic, BodyKind::Keyframed] {
            let mut changed = asset.clone();
            changed.bodies[0].kind = kind;
            cases.push(("moving body", changed));
        }
        for layer in [2, 4, 12, 15] {
            let mut changed = asset.clone();
            changed.bodies[0].havok.collision_layer = layer;
            cases.push(("different layer", changed));
        }
        let mut child_body = asset.clone();
        child_body.bodies[0].node = 1;
        cases.push(("child body", child_body));
        let mut unnamed = asset.clone();
        unnamed.bodies[0].target.clear();
        cases.push(("unnamed target", unnamed));
        let mut massive = asset.clone();
        massive.bodies[0].mass = 1.0;
        cases.push(("massive body", massive));
        let mut skipped = asset;
        skipped.skipped.push("lost animated lid".to_owned());
        cases.push(("conversion skip", skipped));
        for (case, asset) in cases {
            assert!(
                matches!(
                    static_decision_from_asset(FixedCollisionTier::VerifiedContainer, &asset),
                    StaticCollisionDecision::Skipped(_)
                ),
                "{case} must stay excluded and counted as a skip"
            );
        }
    }

    #[test]
    fn container_collision_preserves_authored_absence_and_conversion_failures() {
        let mut asset = authored_asset(Vec::new(), Vec::new());
        assert!(matches!(
            static_decision_from_asset(FixedCollisionTier::VerifiedContainer, &asset),
            StaticCollisionDecision::AuthoredAbsent
        ));
        asset.skipped.push("unsupported shape".to_owned());
        match static_decision_from_asset(FixedCollisionTier::VerifiedContainer, &asset) {
            StaticCollisionDecision::Skipped(reason) => assert_eq!(reason, "unsupported shape"),
            _ => panic!("a conversion failure is not authored absence"),
        }
    }

    #[test]
    fn container_collision_readiness_attaches_once_and_counts_deferred_assets() {
        for (path, physics, valid, attached, skipped, excluded) in [
            ("meshes/clutter/barrel01.glb", true, true, 1, 0, 0),
            ("meshes/clutter/barrel01.glb", false, true, 0, 0, 0),
            ("meshes/clutter/barrel01.glb", true, false, 0, 1, 0),
            (
                "meshes/clutter/upperclass/upperchest01.glb",
                true,
                true,
                0,
                0,
                1,
            ),
        ] {
            let mut app = model_app();
            let mut asset = verified_container_asset();
            if !valid {
                asset.bodies.clear();
            }
            let (reference, metrics) =
                ready_clutter_reference_with(&mut app, "CONT", path, physics, &asset);
            assert_eq!(metrics.static_colliders_authored, attached);
            assert_eq!(metrics.static_colliders_skipped, skipped);
            assert_eq!(metrics.static_colliders_excluded_authored, excluded);
            assert_eq!(metrics.static_colliders_proxy, 0);
            assert_eq!(metrics.dynamic_clutter_bodies, 0);
            assert_eq!(
                app.world().get::<RigidBody>(reference),
                (attached == 1).then_some(&RigidBody::Fixed)
            );
            let mut colliders = app.world_mut().query_filtered::<Entity, With<Collider>>();
            assert_eq!(colliders.iter(app.world()).count(), attached as usize);
            settle_readiness(&mut app);
            assert_eq!(
                app.world()
                    .resource::<StreamingMetrics>()
                    .static_colliders_authored,
                attached,
                "a settled reference must not get another collider"
            );
        }
    }

    #[test]
    fn container_collision_without_metadata_has_no_render_proxy() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>();
        let root = app.world_mut().spawn_empty().id();
        let decision = app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      extras: Query<&GltfSceneExtras>,
                      transforms: Query<(&Transform, &GlobalTransform)>,
                      primitives: RenderPrimitiveQuery,
                      meshes: Res<Assets<Mesh>>,
                      materials: Res<Assets<StandardMaterial>>| {
                    static_collision_from_hierarchy(
                        Some("CONT"),
                        "meshes/clutter/barrel01.glb",
                        root,
                        &children,
                        &extras,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                },
            )
            .unwrap();
        assert!(matches!(decision, StaticCollisionDecision::LegacyExcluded));
    }

    #[test]
    fn container_collision_preserves_scaled_pose_rebase_unload_and_reload() {
        let mut app = crate::physics::headless::fixture_app();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
            .add_systems(Update, update_render_origin);
        let cell = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::ZERO), Transform::default()))
            .id();
        let mut asset = verified_container_asset();
        asset.shapes = vec![CollisionShape::Box {
            center: [0.0, 10.0, 0.0],
            half_extents: [10.0, 10.0, 20.0],
        }];
        let StaticCollisionDecision::Authored(parts, _) = static_decision_from_asset(
            fixed_collision_tier(Some("CONT"), "meshes/clutter/barrel01.glb").unwrap(),
            &asset,
        ) else {
            panic!("verified container must supply the fixture collider");
        };
        let transform = Transform::from_xyz(700.0, 100.0, 700.0)
            .with_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::splat(2.0));
        let root = app.world_mut().spawn((transform, ChildOf(cell))).id();
        let attach = |app: &mut App, entity, parts: Vec<AuthoredColliderPart>| {
            app.world_mut()
                .run_system_once(move |mut commands: Commands| {
                    attach_authored_collision(&mut commands, entity, &parts);
                })
                .unwrap();
        };
        attach(&mut app, root, parts.clone());
        for _ in 0..3 {
            app.update();
        }
        let hit = |app: &mut App, x: f32| {
            app.world_mut()
                .run_system_once(move |context: ReadRapierContext| {
                    context.single().unwrap().cast_ray(
                        Vec3::new(x, 200.0, 700.0),
                        Vec3::NEG_Y,
                        100.0,
                        true,
                        QueryFilter::default().groups(crate::physics::player_collision_groups()),
                    )
                })
                .unwrap()
        };
        // Rotation makes the 20-unit local Z half-extent the 40-unit world X half-extent.
        let (collider, distance) = hit(&mut app, 730.0).expect("scaled container has contact");
        assert!((distance - 60.0).abs() < 0.01, "contact at {distance}");
        assert!(
            hit(&mut app, 750.0).is_none(),
            "container grew beyond authored extent"
        );
        let camera = app
            .world_mut()
            .query_filtered::<Entity, With<StreamingCamera>>()
            .single(app.world())
            .unwrap();
        app.world_mut()
            .get_mut::<Transform>(camera)
            .unwrap()
            .translation
            .x += CELL_SIZE;
        app.update();
        let rebased_body_x = app
            .world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                let context = context.single().unwrap();
                context.rigidbody_set.bodies[context.entity2body()[&root]]
                    .translation()
                    .x
            })
            .unwrap();
        assert!((rebased_body_x - (700.0 - CELL_SIZE)).abs() < 0.01);
        assert!(
            (app.world()
                .get::<GlobalTransform>(root)
                .unwrap()
                .translation()
                .x
                - rebased_body_x)
                .abs()
                < 0.01
        );
        // Rebasing runs in Update, after the fixed physics step. The following fixed step
        // refreshes Rapier's broad phase before spatial queries use the new collider bounds.
        app.update();
        let (rebased, distance) = hit(&mut app, 730.0 - CELL_SIZE).expect("rebased contact");
        assert_eq!(rebased, collider);
        assert!((distance - 60.0).abs() < 0.01);
        assert!(
            hit(&mut app, 730.0).is_none(),
            "stale collider at the old origin"
        );
        app.world_mut().entity_mut(cell).despawn();
        app.update();
        assert!(app.world().get_entity(collider).is_err());
        assert!(
            hit(&mut app, 730.0 - CELL_SIZE).is_none(),
            "unloaded collider remains"
        );
        let reloaded = app.world_mut().spawn(transform).id();
        attach(&mut app, reloaded, parts);
        for _ in 0..3 {
            app.update();
        }
        let (_, distance) = hit(&mut app, 730.0).expect("reloaded contact");
        assert!((distance - 60.0).abs() < 0.01);
    }

    #[test]
    fn container_collision_blocks_the_fixture_player() {
        let mut app = crate::physics::headless::fixture_app();
        let mut asset = verified_container_asset();
        asset.shapes = vec![CollisionShape::Box {
            center: [0.0, 150.0, 0.0],
            half_extents: [10.0, 150.0, 250.0],
        }];
        let StaticCollisionDecision::Authored(parts, _) =
            static_decision_from_asset(FixedCollisionTier::VerifiedContainer, &asset)
        else {
            panic!("verified container must supply the blocking collider");
        };
        let root = app
            .world_mut()
            .spawn(Transform::from_xyz(250.0, 0.0, 750.0))
            .id();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                attach_authored_collision(&mut commands, root, &parts);
            })
            .unwrap();
        crate::physics::headless::place_player(&mut app, Vec3::new(0.0, 200.0, 900.0));
        app.insert_resource(crate::physics::WalkIntent {
            wish_dir: Vec3::X,
            target_speed: crate::physics::MovementTuning::default().run_speed,
            jump_pressed: false,
        });
        for _ in 0..180 {
            app.update();
        }
        let (player, _) = crate::physics::headless::player_pose(&mut app);
        assert!(
            player.x > 100.0,
            "player never approached the container: {player:?}"
        );
        assert!(player.x < 225.0, "player crossed the container: {player:?}");
        assert!(
            app.world()
                .get::<crate::physics::DynamicClutter>(root)
                .is_none()
        );
    }

    #[test]
    fn acti_never_receives_a_render_proxy() {
        let path = "meshes/architecture/whiterun/wrdrawbridge01.glb";
        assert!(static_proxy_eligible(Some("STAT"), path));
        assert!(!static_proxy_eligible(Some("ACTI"), path));
    }

    #[test]
    fn authored_only_keeps_every_shape_owned_by_fixed_bodies() {
        let asset = authored_asset(
            vec![authored_box_shape(), authored_mesh_shape()],
            vec![
                clutter_body(BodyKind::Fixed, vec![0], true),
                clutter_body(BodyKind::Fixed, vec![1], false),
                clutter_body(BodyKind::Fixed, vec![0, 1], true),
                clutter_body(BodyKind::Fixed, vec![1], false),
            ],
        );
        match static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset) {
            StaticCollisionDecision::Authored(parts, notes) => {
                assert_eq!(parts.len(), 2);
                assert!(notes.is_empty(), "{notes:?}");
            }
            _ => panic!("fixed bodies must keep their shapes"),
        }
    }

    #[test]
    fn authored_only_drops_shapes_owned_only_by_dynamic_bodies() {
        let asset = authored_asset(
            vec![authored_box_shape(), authored_hull_shape()],
            vec![
                clutter_body(BodyKind::Fixed, vec![0], true),
                clutter_body(BodyKind::Dynamic, vec![1], true),
            ],
        );
        match static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset) {
            StaticCollisionDecision::Authored(parts, notes) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(
                    notes,
                    vec!["1 dynamic bodies left unsimulated on a fixed record".to_owned()]
                );
            }
            _ => panic!("the fixed body must stay solid"),
        }
    }

    #[test]
    fn authored_only_ignores_a_dynamic_only_model_without_skipping_it() {
        let asset = authored_asset(
            vec![authored_hull_shape()],
            vec![clutter_body(BodyKind::Dynamic, vec![0], true)],
        );
        let decision = static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset);
        assert!(matches!(decision, StaticCollisionDecision::AuthoredAbsent));
    }

    #[test]
    fn authored_only_still_reports_a_converter_failure_on_a_dynamic_only_model() {
        let mut asset = authored_asset(
            vec![authored_hull_shape()],
            vec![clutter_body(BodyKind::Dynamic, vec![0], true)],
        );
        asset
            .skipped
            .push("block 4: collision target is not exported".to_owned());
        match static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset) {
            StaticCollisionDecision::Skipped(reason) => {
                assert!(reason.contains("not exported"), "{reason}");
            }
            _ => panic!("a converter failure must not be reported as an authored absence"),
        }
    }

    #[test]
    fn authored_only_keeps_every_shape_of_a_version_one_asset() {
        let mut asset = authored_asset(vec![authored_box_shape()], Vec::new());
        asset.version = 1;
        asset.shapes.push(authored_mesh_shape());
        match static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset) {
            StaticCollisionDecision::Authored(parts, notes) => {
                assert_eq!(parts.len(), 2);
                assert!(notes.is_empty(), "{notes:?}");
            }
            _ => panic!("a version one asset has no dynamic ownership"),
        }
    }

    #[test]
    fn authored_only_skips_a_body_that_names_a_missing_shape() {
        let asset = authored_asset(
            vec![authored_box_shape()],
            vec![clutter_body(BodyKind::Fixed, vec![0, 5], true)],
        );
        match static_decision_from_asset(FixedCollisionTier::AuthoredOnly, &asset) {
            StaticCollisionDecision::Skipped(reason) => {
                assert!(reason.contains("does not exist"), "{reason}");
            }
            _ => panic!("an out-of-range shape index must be reported"),
        }
    }

    #[test]
    fn full_tier_keeps_dynamic_body_shapes_on_a_fixed_record() {
        let asset = authored_asset(
            vec![authored_box_shape(), authored_hull_shape()],
            vec![
                clutter_body(BodyKind::Fixed, vec![0], true),
                clutter_body(BodyKind::Dynamic, vec![1], true),
            ],
        );
        match static_decision_from_asset(FixedCollisionTier::Full, &asset) {
            StaticCollisionDecision::Authored(parts, notes) => {
                assert_eq!(parts.len(), 2);
                assert!(notes.is_empty(), "{notes:?}");
            }
            _ => panic!("a full-tier fixed record keeps every authored shape"),
        }
    }

    #[test]
    fn collision_authored_under_the_pre_rename_key_still_loads() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>();
        let root = app.world_mut().spawn_empty().id();
        // Built from JSON rather than a struct literal, so a field added to `CollisionAsset`
        // with a serde default does not break this test.
        let mut asset: CollisionAsset = serde_json::from_value(serde_json::json!({
            "version": COLLISION_ASSET_VERSION,
            "authored": true,
            "shapes": [],
            "skipped": [],
        }))
        .unwrap();
        asset.shapes.push(CollisionShape::Box {
            center: [0.0, 5.0, 0.0],
            half_extents: [10.0, 5.0, 10.0],
        });
        // The key is built in two halves so scripts/rename-to-mudcrab.py leaves it alone.
        let old_key = ["openSkyrim", "Collision"].concat();
        app.world_mut().spawn((
            GltfSceneExtras {
                value: serde_json::json!({ old_key: asset }).to_string(),
            },
            ChildOf(root),
        ));
        let decision = app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      scene_extras: Query<&GltfSceneExtras>,
                      transforms: Query<(&Transform, &GlobalTransform)>,
                      primitives: RenderPrimitiveQuery,
                      meshes: Res<Assets<Mesh>>,
                      materials: Res<Assets<StandardMaterial>>| {
                    static_collision_from_hierarchy(
                        Some("STAT"),
                        "meshes/architecture/farmhouse/inn01.glb",
                        root,
                        &children,
                        &scene_extras,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                },
            )
            .unwrap();
        assert!(matches!(decision, StaticCollisionDecision::Authored(_, _)));
    }

    #[test]
    fn static_proxy_policy_excludes_movable_and_decorative_records() {
        assert!(static_proxy_eligible(
            Some("STAT"),
            "meshes/landscape/rocks/rockl01.glb"
        ));
        assert!(static_proxy_eligible(
            Some("STAT"),
            "meshes/architecture/farmhouse/inn01.glb"
        ));
        for path in [
            "meshes/clutter/firewood/firewoodpilelarge01.glb",
            "meshes/landscape/trees/treepineforestlog01.glb",
            "meshes/landscape/roads/roadstraightlongramp01.glb",
        ] {
            assert!(static_proxy_eligible(Some("STAT"), path), "{path}");
        }
        assert!(static_proxy_eligible(
            Some("TREE"),
            "meshes/landscape/trees/treepineforest01.glb"
        ));
        assert!(!static_proxy_eligible(
            Some("TREE"),
            "meshes/landscape/plants/clover01.glb"
        ));
        for kind in [None, Some("MISC"), Some("TREE"), Some("MSTT"), Some("DOOR")] {
            assert!(!static_proxy_eligible(
                kind,
                "meshes/architecture/farmhouse/inn01.glb"
            ));
        }
        assert!(!static_proxy_eligible(
            Some("STAT"),
            "meshes/landscape/plants/fern.glb"
        ));
        let blended = StandardMaterial {
            alpha_mode: AlphaMode::Blend,
            ..default()
        };
        let masked = StandardMaterial {
            alpha_mode: AlphaMode::Mask(0.5),
            ..default()
        };
        assert!(static_proxy_material_allowed(
            "meshes/landscape/rocks/rockcliff02.glb",
            None,
            &blended
        ));
        assert!(!static_proxy_material_allowed(
            "meshes/landscape/rocks/rockl01.glb",
            None,
            &blended
        ));
        assert!(!static_proxy_material_allowed(
            "meshes/architecture/farmhouse/ivy01.glb",
            None,
            &masked
        ));
        assert!(!static_proxy_material_allowed(
            "meshes/landscape/rocks/rockcliff02.glb",
            None,
            &masked
        ));
        assert!(static_proxy_material_allowed(
            "meshes/architecture/farmhouse/lumbermill01.glb",
            Some("LumbermillMesh:19 - L1_Posts01:19"),
            &masked
        ));
        assert!(!static_proxy_material_allowed(
            "meshes/architecture/farmhouse/lumbermill01.glb",
            Some("LumbermillMesh:20"),
            &masked
        ));
        assert!(!static_proxy_material_allowed(
            "meshes/architecture/farmhouse/inn01.glb",
            Some("LumbermillMesh:19"),
            &masked
        ));
    }

    #[test]
    fn static_proxy_bakes_nested_node_transforms_and_leaves_openings() {
        use bevy_rapier3d::rapier::parry::{math::Pose, query::Ray};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>();
        let post = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(20.0, 100.0, 10.0));
        let opaque = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let root = app.world_mut().spawn(Transform::default()).id();
        let node = app
            .world_mut()
            .spawn((Transform::from_xyz(30.0, 0.0, 0.0), ChildOf(root)))
            .id();
        for offset in [-40.0, 40.0] {
            app.world_mut().spawn((
                Mesh3d(post.clone()),
                MeshMaterial3d(opaque.clone()),
                Transform::from_xyz(offset, 0.0, 0.0),
                ChildOf(node),
            ));
        }
        app.update();
        let proxy = app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      transforms: Query<(&Transform, &GlobalTransform)>,
                      primitives: RenderPrimitiveQuery,
                      meshes: Res<Assets<Mesh>>,
                      materials: Res<Assets<StandardMaterial>>| {
                    static_proxy_from_hierarchy(
                        "meshes/architecture/farmhouse/inn01.glb",
                        root,
                        &children,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                },
            )
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(proxy.as_trimesh().is_some());
        let gap = Ray::new(Vec3::new(30.0, 0.0, 50.0), Vec3::NEG_Z);
        let post = Ray::new(Vec3::new(-10.0, 0.0, 50.0), Vec3::NEG_Z);
        assert!(
            proxy
                .raw
                .cast_ray(&Pose::IDENTITY, &gap, 100.0, true)
                .is_none()
        );
        assert!(
            proxy
                .raw
                .cast_ray(&Pose::IDENTITY, &post, 100.0, true)
                .is_some()
        );
        // Rapier applies the placement rotation and scale after node transforms are baked.
        let mut physics = crate::physics::headless::fixture_app();
        let placement = physics
            .world_mut()
            .spawn((
                RigidBody::Fixed,
                proxy,
                crate::physics::world_collision_groups(),
                Transform::from_xyz(200.0, 300.0, 200.0)
                    .with_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2))
                    .with_scale(Vec3::splat(2.0)),
            ))
            .id();
        for _ in 0..3 {
            physics.update();
        }
        let (post_hit, gap_hit) = physics
            .world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                let context = context.single().unwrap();
                let only_placement = |entity| entity == placement;
                let filter =
                    bevy_rapier3d::prelude::QueryFilter::default().predicate(&only_placement);
                (
                    context.cast_ray(
                        Vec3::new(500.0, 300.0, 220.0),
                        Vec3::NEG_X,
                        600.0,
                        true,
                        filter,
                    ),
                    context.cast_ray(
                        Vec3::new(500.0, 300.0, 140.0),
                        Vec3::NEG_X,
                        600.0,
                        true,
                        filter,
                    ),
                )
            })
            .unwrap();
        assert!(post_hit.is_some(), "scaled, rotated post had no contact");
        assert!(gap_hit.is_none(), "scaled, rotated opening was filled");
    }

    #[test]
    fn lumbermill_walkway_mask_contributes_without_roof_mask() {
        use bevy_rapier3d::rapier::parry::{math::Pose, query::Ray};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>();
        let slab = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(20.0, 4.0, 20.0));
        let masked = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial {
                alpha_mode: AlphaMode::Mask(0.5),
                ..default()
            });
        let root = app.world_mut().spawn(Transform::default()).id();
        for (x, name) in [
            (0.0, "LumbermillMesh:19 - L1_Posts01:19"),
            (100.0, "LumbermillMesh:20"),
        ] {
            app.world_mut().spawn((
                Mesh3d(slab.clone()),
                MeshMaterial3d(masked.clone()),
                GltfMaterialName(name.to_owned()),
                Transform::from_xyz(x, 10.0, 0.0),
                ChildOf(root),
            ));
        }
        app.update();
        let proxy = app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      transforms: Query<(&Transform, &GlobalTransform)>,
                      primitives: RenderPrimitiveQuery,
                      meshes: Res<Assets<Mesh>>,
                      materials: Res<Assets<StandardMaterial>>| {
                    static_proxy_from_hierarchy(
                        "meshes/architecture/farmhouse/lumbermill01.glb",
                        root,
                        &children,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                },
            )
            .unwrap()
            .unwrap()
            .unwrap();
        for (x, expected) in [(0.0, true), (100.0, false)] {
            let ray = Ray::new(Vec3::new(x, 50.0, 0.0), Vec3::NEG_Y);
            assert_eq!(
                proxy
                    .raw
                    .cast_ray(&Pose::IDENTITY, &ray, 100.0, true)
                    .is_some(),
                expected,
                "unexpected lumbermill proxy contact at x={x}"
            );
        }
    }

    #[test]
    fn walk_capsule_and_dynamic_tankard_contact_static_proxy() {
        use bevy_rapier3d::prelude::{ColliderMassProperties, Velocity};

        let mut app = crate::physics::headless::fixture_app();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(20.0, 200.0, 400.0));
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let wall = app
            .world_mut()
            .spawn(Transform::from_xyz(250.0, 100.0, 850.0))
            .id();
        app.world_mut().spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            ChildOf(wall),
        ));
        app.update();
        let proxy = app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      transforms: Query<(&Transform, &GlobalTransform)>,
                      primitives: RenderPrimitiveQuery,
                      meshes: Res<Assets<Mesh>>,
                      materials: Res<Assets<StandardMaterial>>| {
                    static_proxy_from_hierarchy(
                        "meshes/architecture/farmhouse/inn01.glb",
                        wall,
                        &children,
                        &transforms,
                        &primitives,
                        &meshes,
                        &materials,
                    )
                },
            )
            .unwrap()
            .unwrap()
            .unwrap();
        app.world_mut().entity_mut(wall).insert((
            StaticColliderSource::RenderProxy,
            RigidBody::Fixed,
            proxy,
            crate::physics::world_collision_groups(),
        ));
        crate::physics::headless::place_player(&mut app, Vec3::new(0.0, 200.0, 900.0));
        let tankard = app
            .world_mut()
            .spawn((
                DebugTankard,
                RigidBody::Dynamic,
                crate::physics::debug_tankard_collider(),
                crate::physics::tankard_collision_groups(),
                ColliderMassProperties::Density(0.001),
                Velocity::linear(Vec3::X * 500.0),
                Transform::from_xyz(60.0, 150.0, 750.0),
            ))
            .id();
        app.insert_resource(crate::physics::WalkIntent {
            wish_dir: Vec3::X,
            target_speed: crate::physics::MovementTuning::default().run_speed,
            jump_pressed: false,
        });
        let mut max_tankard_x = f32::NEG_INFINITY;
        for _ in 0..180 {
            app.update();
            max_tankard_x =
                max_tankard_x.max(app.world().get::<Transform>(tankard).unwrap().translation.x);
        }
        let (player, _) = crate::physics::headless::player_pose(&mut app);
        assert!(
            player.x < 225.0,
            "WALK capsule crossed static wall: {player:?}"
        );
        assert!(
            max_tankard_x < 270.0,
            "dynamic tankard tunneled through static wall: {max_tankard_x}"
        );
    }

    #[test]
    fn terrain_sampler_check_rejects_default_and_clamped_images() {
        assert!(!terrain_sampler_repeats(&ImageSampler::Default));
        assert!(!terrain_sampler_repeats(&ImageSampler::linear()));
        assert!(terrain_sampler_repeats(&ImageSampler::Descriptor(
            bevy::image::ImageSamplerDescriptor {
                address_mode_u: ImageAddressMode::Repeat,
                address_mode_v: ImageAddressMode::Repeat,
                ..bevy::image::ImageSamplerDescriptor::linear()
            }
        )));
    }

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
    fn terrain_colliders_match_each_render_quadrant() {
        let terrain = terrain_fixture(91, 120.0);
        for quadrant in 0..4 {
            let mesh = build_terrain_quadrant_mesh(&terrain, quadrant).unwrap();
            let collider = terrain_collider_from_mesh(&mesh).unwrap();
            let trimesh = collider.as_trimesh().expect("terrain trimesh");
            let VertexAttributeValues::Float32x3(positions) =
                mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap()
            else {
                panic!("terrain positions")
            };
            let Indices::U32(indices) = mesh.indices().unwrap() else {
                panic!("terrain indices")
            };
            assert_eq!(trimesh.vertices().len(), positions.len());
            for (vertex, position) in trimesh.vertices().zip(positions) {
                assert_eq!(vertex, Vec3::from_array(*position));
            }
            assert_eq!(trimesh.indices().len() * 3, indices.len());
            for (triangle, indices) in trimesh.indices().iter().zip(indices.as_chunks::<3>().0) {
                assert_eq!(*triangle, [indices[0], indices[1], indices[2]]);
            }
        }
    }

    #[test]
    fn maps_nif_paths_to_converted_glb_paths() {
        assert_eq!(
            converted_model_path("meshes\\architecture\\wall.nif".into()).as_deref(),
            Some("meshes/architecture/wall.glb")
        );
        assert_eq!(
            converted_model_path("Meshes\\Landscape\\Tundra\\TundraStreamStraight01.NIF".into())
                .as_deref(),
            Some("meshes/landscape/tundra/tundrastreamstraight01.glb")
        );
        assert_eq!(
            converted_model_path("meshes/Sky/CloudShape01.nif".into()),
            None
        );
        assert_eq!(
            converted_model_path("meshes/Markers/MapMarker.nif".into()),
            None
        );
        assert_eq!(
            converted_model_path("Markers/CivilWarMarkers/CWAttSpawn02.nif".into()),
            None
        );
        assert_eq!(converted_model_path("Effects/FXRapids.nif".into()), None);
    }

    #[test]
    fn keeps_visible_models_whose_filename_contains_marker() {
        assert_eq!(
            converted_model_path(r"meshes\Clutter\CivilWar\CWMapMarkers01.nif".into()).as_deref(),
            Some("meshes/clutter/civilwar/cwmapmarkers01.glb")
        );
        assert_eq!(
            converted_model_path("meshes/Furniture/BlacksmithForgeMarker.nif".into()).as_deref(),
            Some("meshes/furniture/blacksmithforgemarker.glb")
        );
    }

    #[test]
    fn a_marker_base_record_spawns_no_model() {
        assert_eq!(
            spawned_model_path(Some("Marker_Map.nif".into()), true),
            None
        );
        assert_eq!(
            spawned_model_path(Some("Marker_Map.nif".into()), false).as_deref(),
            Some("meshes/marker_map.glb")
        );
        assert_eq!(spawned_model_path(None, false), None);
    }

    #[test]
    fn rejects_traversal_and_rooted_model_paths() {
        assert_eq!(
            converted_model_path("meshes/../../secrets.nif".into()),
            None
        );
        assert_eq!(converted_model_path("meshes//etc/passwd".into()), None);
        assert_eq!(
            converted_model_path(r"meshes\..\..\secrets.nif".into()),
            None
        );
        // Windows treats a drive-prefixed path as absolute (and `PathBuf::join`
        // would let it replace the base path entirely); Rust's path parsing is
        // OS-native, so this case only bites on the Windows target this engine
        // ships for.
        #[cfg(windows)]
        assert_eq!(
            converted_model_path("meshes/C:/Windows/evil.nif".into()),
            None
        );
        // A plain relative path is unaffected.
        assert_eq!(
            converted_model_path("meshes/architecture/wall.nif".into()).as_deref(),
            Some("meshes/architecture/wall.glb")
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

    /// An app with the cell plan, the unload pacer and the lifecycle validator chained as they run
    /// in `StreamingPlugin`. `collect_cells` is left out, so a requested cell stays `Loading` and
    /// the test drives the resident cells itself through [`spawn_resident_cell`].
    fn streaming_test_app(config: EngineConfig) -> (App, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE schema_info(version INTEGER NOT NULL);
                 INSERT INTO schema_info VALUES({});",
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
        drop(connection);
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(WorldDatabase::open(&path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<StreamingCommitBudget>()
            .init_resource::<lod::LodStreaming>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<ActiveSpace>()
            .init_resource::<ProfilingState>()
            .add_systems(
                Update,
                (plan_cells, despawn_cells, validate_streaming_lifecycle).chain(),
            );
        (app, directory)
    }

    fn spawn_camera(app: &mut App, center: IVec2) {
        app.world_mut().spawn((
            Transform::from_xyz(
                center.x as f32 * CELL_SIZE,
                0.0,
                -(center.y as f32) * CELL_SIZE,
            ),
            StreamingCamera,
        ));
    }

    fn move_camera(app: &mut App, shift: IVec2) {
        let mut camera = app
            .world_mut()
            .query_filtered::<&mut Transform, With<StreamingCamera>>();
        let world = app.world_mut();
        let mut transforms = camera.query_mut(world);
        let mut transform = transforms.single_mut().unwrap();
        transform.translation.x += shift.x as f32 * CELL_SIZE;
        transform.translation.z -= shift.y as f32 * CELL_SIZE;
    }

    fn exterior_key(grid_x: i32, grid_y: i32) -> CellKey {
        CellKey::Exterior {
            worldspace_id: 0x3c,
            grid_x,
            grid_y,
        }
    }

    #[test]
    fn controlled_cell_intake_is_nearest_first_and_bounds_the_entire_outstanding_pipeline() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            max_streaming_backlog: 2,
            max_cell_commits_per_frame: 1,
            prioritize_streaming: false,
            stream_radius: 2,
            unload_radius: 2,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        app.update();
        let streaming = app.world().resource::<StreamingWorld>();
        assert_eq!(streaming.outstanding_cells.len(), 1);
        assert!(streaming.cells.contains_key(&exterior_key(0, 0)));
        app.update();
        let streaming = app.world().resource::<StreamingWorld>();
        assert_eq!(streaming.outstanding_cells.len(), 2);
        assert!(streaming.cells.keys().all(|key| {
            matches!(key, CellKey::Exterior { grid_x, grid_y, .. }
                if grid_x * grid_x + grid_y * grid_y <= 1)
        }));
        for _ in 0..12 {
            move_camera(&mut app, IVec2::new(100, 0));
            app.update();
        }
        let streaming = app.world().resource::<StreamingWorld>();
        assert!(streaming.cells.is_empty());
        assert_eq!(streaming.outstanding_cells.len(), 2);
        assert_eq!(streaming_metrics(&app).requests_submitted, 2);
    }

    #[test]
    fn default_cell_intake_still_requests_the_whole_wanted_window() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            stream_radius: 2,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        app.update();
        let streaming = app.world().resource::<StreamingWorld>();
        assert_eq!(streaming.cells.len(), 25);
        assert_eq!(streaming.outstanding_cells.len(), 25);
        assert_eq!(streaming_metrics(&app).requests_submitted, 25);
    }

    /// Spawns a cell root with a two-entity subtree under it and records the cell as resident or
    /// retiring, the state a committed cell is in.
    fn spawn_mock_cell(app: &mut App, grid_x: i32, grid_y: i32, resident: bool) -> Entity {
        let root = app
            .world_mut()
            .spawn((
                CellRef(cell_id_of(grid_x, grid_y)),
                StreamedCellRoot,
                ExteriorCellGrid(IVec2::new(grid_x, grid_y)),
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        let child = app
            .world_mut()
            .spawn((Transform::default(), ChildOf(root)))
            .id();
        app.world_mut()
            .spawn((Transform::default(), ChildOf(child)));
        let status = if resident {
            CellStatus::Resident { root }
        } else {
            CellStatus::Retiring { root }
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(exterior_key(grid_x, grid_y), status);
        root
    }

    /// A steady window around `center`: the 7x7 cells the camera wants plus the trailing column a
    /// crossing drops, which is the shape the resident set settles into while flying.
    fn spawn_steady_window(app: &mut App, center: IVec2) {
        for grid_y in (center.y - 3)..=(center.y + 3) {
            for grid_x in (center.x - 4)..=(center.x + 3) {
                spawn_mock_cell(app, grid_x, grid_y, true);
            }
        }
    }

    fn cell_id_of(grid_x: i32, grid_y: i32) -> u32 {
        ((grid_x + 64) as u32) * 256 + (grid_y + 64) as u32
    }

    fn resident_root_count(app: &mut App) -> usize {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<StreamedCellRoot>>();
        let world = app.world();
        query.iter(world).count()
    }

    fn streaming_metrics(app: &App) -> StreamingMetrics {
        app.world().resource::<StreamingMetrics>().clone()
    }

    /// A camera move to apply between the plan and the check, once.
    #[derive(Resource, Default)]
    struct MidFrameShift(Option<IVec2>);

    /// A resident root to commit between the plan and the check, once.
    #[derive(Resource, Default)]
    struct MidFrameLeak(Option<IVec2>);

    fn apply_mid_frame_shift(
        mut shift: ResMut<MidFrameShift>,
        mut camera: Query<&mut Transform, With<StreamingCamera>>,
    ) {
        let Some(shift) = shift.0.take() else {
            return;
        };
        let mut transform = camera.single_mut().unwrap();
        transform.translation.x += shift.x as f32 * CELL_SIZE;
        transform.translation.z -= shift.y as f32 * CELL_SIZE;
    }

    fn inject_mid_frame_leak(world: &mut World) {
        let Some(grid) = world.resource_mut::<MidFrameLeak>().0.take() else {
            return;
        };
        let root = world
            .spawn((
                CellRef(cell_id_of(grid.x, grid.y)),
                StreamedCellRoot,
                ExteriorCellGrid(grid),
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        world
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(exterior_key(grid.x, grid.y), CellStatus::Resident { root });
    }

    /// [`streaming_test_app`] with hooks that act after `plan_cells` and before the check, where
    /// no production system runs for a static interactive camera.
    fn mid_frame_test_app() -> (App, tempfile::TempDir) {
        let (mut app, directory) = streaming_test_app(EngineConfig {
            stream_radius: 2,
            unload_radius: 3,
            ..default()
        });
        app.init_resource::<MidFrameShift>()
            .init_resource::<MidFrameLeak>()
            .add_systems(
                Update,
                (apply_mid_frame_shift, inject_mid_frame_leak)
                    .after(plan_cells)
                    .before(validate_streaming_lifecycle),
            );
        (app, directory)
    }

    /// The radius-2 window around `center`, all resident.
    fn spawn_radius_two_window(app: &mut App, center: IVec2) {
        for grid_y in (center.y - 2)..=(center.y + 2) {
            for grid_x in (center.x - 2)..=(center.x + 2) {
                spawn_mock_cell(app, grid_x, grid_y, true);
            }
        }
    }

    #[test]
    fn a_static_camera_keeps_the_plan_and_check_centers_equal() {
        let start = IVec2::new(5, -12);
        let (mut app, _directory) = mid_frame_test_app();
        spawn_camera(&mut app, start);
        spawn_radius_two_window(&mut app, start);
        for _ in 0..3 {
            app.update();
        }

        assert_eq!(
            app.world().resource::<StreamingWorld>().planned_center,
            Some(start)
        );
        let metrics = streaming_metrics(&app);
        assert_eq!(metrics.out_of_range_cell_roots, 0);
        assert_eq!(metrics.out_of_range_vs_planned_roots, 0);
        assert_eq!(metrics.max_plan_check_center_offset, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
        assert_eq!(metrics.invariant_checks_without_plan, 0);
    }

    #[test]
    fn a_mid_frame_camera_crossing_is_measured_as_plan_drift() {
        let start = IVec2::new(5, -12);
        let (mut app, _directory) = mid_frame_test_app();
        spawn_camera(&mut app, start);
        spawn_radius_two_window(&mut app, start);
        app.update();

        move_camera(&mut app, IVec2::X);
        app.world_mut().resource_mut::<MidFrameShift>().0 = Some(IVec2::X);
        app.update();

        let metrics = streaming_metrics(&app);
        assert_eq!(metrics.out_of_range_cell_roots, 5);
        assert_eq!(metrics.out_of_range_vs_planned_roots, 0);
        assert_eq!(metrics.max_plan_check_center_offset, 1);
        assert_eq!(metrics.streaming_invariant_failures, 5);

        app.update();
        let settled = streaming_metrics(&app);
        assert_eq!(settled.out_of_range_cell_roots, 5);
        assert_eq!(settled.out_of_range_vs_planned_roots, 0);
    }

    #[test]
    fn a_root_committed_outside_the_planned_window_is_measured_as_a_leak() {
        let start = IVec2::new(5, -12);
        let (mut app, _directory) = mid_frame_test_app();
        spawn_camera(&mut app, start);
        spawn_radius_two_window(&mut app, start);
        app.update();

        app.world_mut().resource_mut::<MidFrameLeak>().0 = Some(IVec2::new(10, 0));
        app.update();

        let metrics = streaming_metrics(&app);
        assert_eq!(metrics.out_of_range_cell_roots, 1);
        assert_eq!(metrics.out_of_range_vs_planned_roots, 1);
        assert_eq!(metrics.max_plan_check_center_offset, 0);
        assert_eq!(metrics.streaming_invariant_failures, 1);
    }

    #[test]
    fn a_render_origin_rebase_does_not_move_the_streaming_center() {
        for origin in [IVec2::ZERO, IVec2::new(5, -12), IVec2::new(-40, 37)] {
            for (x, z) in [
                (2048.0, -2048.0),
                (4096.0, 0.0),
                (0.0, -4096.0),
                (-4096.0, 4096.0),
                (8192.0, -12288.0),
                (4095.999, -4095.999),
                (4096.001, 0.5),
                (-0.001, 0.001),
                (12.0, -20.0),
            ] {
                let translation = Vec3::new(x, 0.0, z);
                let shift = rebase_shift(translation);
                let rebased = translation
                    - Vec3::new(
                        shift.x as f32 * CELL_SIZE,
                        0.0,
                        -(shift.y as f32) * CELL_SIZE,
                    );
                assert_eq!(
                    streaming_center(rebased, origin + shift),
                    streaming_center(translation, origin),
                    "translation {translation:?} origin {origin:?}"
                );
            }
        }
    }

    #[test]
    fn a_zero_unload_budget_unloads_the_whole_crossing_in_one_frame() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            stream_radius: 3,
            unload_radius: 4,
            max_cell_unloads_per_frame: 0,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        spawn_steady_window(&mut app, IVec2::ZERO);
        let roots = resident_root_count(&mut app);
        app.update();

        move_camera(&mut app, IVec2::X);
        app.update();
        let metrics = streaming_metrics(&app);
        assert_eq!(
            metrics.despawns_this_frame, 7,
            "0 means unbudgeted, as before"
        );
        assert_eq!(metrics.retiring_cells, 0);
        assert_eq!(metrics.retire_backlog_overflows, 0);
        assert_eq!(resident_root_count(&mut app), roots - 7);
        assert_eq!(metrics.streaming_invariant_failures, 0);
    }

    #[test]
    fn a_crossing_unloads_cells_within_the_budget_and_finishes_within_ceil_frames() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            stream_radius: 3,
            unload_radius: 4,
            max_cell_unloads_per_frame: 2,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        spawn_steady_window(&mut app, IVec2::ZERO);
        let roots = resident_root_count(&mut app);
        app.update();
        let settled = streaming_metrics(&app);
        assert_eq!(
            settled.unloaded_cells, 0,
            "the window fits inside the radius"
        );
        assert_eq!(settled.retiring_cells, 0);
        assert_eq!(settled.streaming_invariant_failures, 0);

        // Cross into the next cell: the whole trailing column leaves the unload radius at once.
        move_camera(&mut app, IVec2::X);
        let mut unloads_per_frame = Vec::new();
        let mut roots_left = Vec::new();
        for frame in 1..=4 {
            app.update();
            let metrics = streaming_metrics(&app);
            unloads_per_frame.push(metrics.despawns_this_frame);
            assert!(
                metrics.despawns_this_frame <= 2,
                "frame {frame} unloaded {} cells against a budget of 2",
                metrics.despawns_this_frame
            );
            assert_eq!(metrics.streaming_invariant_failures, 0, "frame {frame}");
            roots_left.push(resident_root_count(&mut app));
        }
        assert_eq!(
            unloads_per_frame,
            vec![2, 2, 2, 1],
            "the column's 7 cells must spread over ceil(7 / 2) frames"
        );
        assert_eq!(roots_left, vec![roots - 2, roots - 4, roots - 6, roots - 7]);
        let metrics = streaming_metrics(&app);
        assert_eq!(metrics.unloaded_cells, 7);
        assert_eq!(metrics.retiring_cells, 0);
        assert_eq!(metrics.max_despawns_per_frame, 2);
        assert_eq!(metrics.streaming_invariant_failures, 0);
    }

    #[test]
    fn a_reversal_revives_retiring_cells_without_another_request() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            stream_radius: 3,
            unload_radius: 4,
            max_cell_unloads_per_frame: 2,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        spawn_steady_window(&mut app, IVec2::ZERO);
        app.update();
        move_camera(&mut app, IVec2::X);
        app.update();
        let after_crossing = streaming_metrics(&app);
        assert_eq!(
            after_crossing.retiring_cells, 5,
            "the crossing retires a column of 7 and the budget takes 2 in the same frame"
        );
        assert_eq!(after_crossing.unloaded_cells, 2);
        let roots_after_crossing = resident_root_count(&mut app);

        // Turn around before the budget has worked through the column.
        move_camera(&mut app, IVec2::NEG_X);
        app.update();
        let after_reversal = streaming_metrics(&app);
        assert_eq!(after_reversal.revived_cells, 5);
        assert_eq!(after_reversal.retiring_cells, 0);
        assert_eq!(after_reversal.despawns_this_frame, 0);
        assert_eq!(after_reversal.unloaded_cells, 2);
        assert_eq!(
            after_reversal.requests_submitted, after_crossing.requests_submitted,
            "a revived cell keeps its root, so it must not be requested again"
        );
        assert_eq!(resident_root_count(&mut app), roots_after_crossing);
        assert_eq!(after_reversal.streaming_invariant_failures, 0);
    }

    #[test]
    fn retire_backlog_bound_is_one_window_at_the_unload_radius() {
        assert_eq!(retire_backlog_bound(4), 81);
        assert_eq!(retire_backlog_bound(3), 49);
        assert_eq!(retire_backlog_bound(1), 9);
        assert_eq!(retire_backlog_bound(0), 1);
    }

    #[test]
    fn a_backlog_past_the_bound_unloads_every_retiring_cell_at_once() {
        let (mut app, _directory) = streaming_test_app(EngineConfig {
            stream_radius: 1,
            unload_radius: 1,
            max_cell_unloads_per_frame: 2,
            ..default()
        });
        spawn_camera(&mut app, IVec2::ZERO);
        // Under the bound the pacer keeps to the budget: 8 cells wait, 2 go per frame.
        for grid_x in 10..18 {
            spawn_mock_cell(&mut app, grid_x, 0, false);
        }
        app.update();
        let paced = streaming_metrics(&app);
        assert_eq!(paced.retire_backlog_overflows, 0);
        assert_eq!(paced.despawns_this_frame, 2);
        assert_eq!(paced.retiring_cells, 6);
        assert_eq!(
            resident_root_count(&mut app),
            6,
            "the waiters keep their roots"
        );

        // Past the bound the pacer gives up pacing and takes the whole backlog in one frame.
        for grid_x in 20..40 {
            spawn_mock_cell(&mut app, grid_x, 0, false);
        }
        app.update();
        let overflowed = streaming_metrics(&app);
        assert_eq!(overflowed.retire_backlog_overflows, 1);
        assert_eq!(overflowed.despawns_this_frame, 26);
        assert_eq!(overflowed.retiring_cells, 0);
        assert_eq!(overflowed.unloaded_cells, 28);
        assert_eq!(resident_root_count(&mut app), 0);
        assert_eq!(overflowed.streaming_invariant_failures, 0);
    }

    #[test]
    fn repeated_rebasing_preserves_camera_and_cell_root_locality() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
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
    fn rebase_moves_dynamic_rapier_pose_with_tankard_transform() {
        let mut app = crate::physics::headless::fixture_app();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
            .add_systems(Update, update_render_origin);
        let cell = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::ZERO), Transform::default()))
            .id();
        let static_entity = app
            .world_mut()
            .spawn((
                StaticColliderSource::RenderProxy,
                Transform::from_xyz(700.0, 100.0, 700.0),
                ChildOf(cell),
                RigidBody::Fixed,
                Collider::cuboid(20.0, 20.0, 20.0),
                crate::physics::world_collision_groups(),
            ))
            .id();
        for _ in 0..3 {
            app.update();
        }
        let pose = |world: &mut World| {
            world.run_system_once(
                |context: ReadRapierContext, tankards: Query<(Entity, &Transform), With<DebugTankard>>| {
                    let (entity, visual) = tankards.iter().next().unwrap();
                    let context = context.single().unwrap();
                    let handle = context.entity2body()[&entity];
                    (visual.translation, context.rigidbody_set.bodies.get(handle).unwrap().translation())
                },
            ).unwrap()
        };
        let before = pose(app.world_mut());
        let static_x = |world: &mut World| {
            world
                .run_system_once(move |context: ReadRapierContext| {
                    let context = context.single().unwrap();
                    let handle = context.entity2body()[&static_entity];
                    context
                        .rigidbody_set
                        .bodies
                        .get(handle)
                        .unwrap()
                        .translation()
                        .x
                })
                .unwrap()
        };
        let before_static_x = static_x(app.world_mut());
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<StreamingCamera>>();
            query.single(app.world()).unwrap()
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation
            .x += CELL_SIZE;
        app.update();
        let after = pose(app.world_mut());
        let after_static_x = static_x(app.world_mut());
        assert!((after.0.x - (before.0.x - CELL_SIZE)).abs() < 0.01);
        assert!((after.1.x - (before.1.x - CELL_SIZE)).abs() < 0.01);
        assert!((after.0.x - after.1.x).abs() < 0.01);
        assert!((after_static_x - (before_static_x - CELL_SIZE)).abs() < 0.01);
        app.world_mut().entity_mut(cell).despawn();
        app.update();
        let body_retained = app
            .world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                context
                    .single()
                    .unwrap()
                    .entity2body()
                    .contains_key(&static_entity)
            })
            .unwrap();
        assert!(!body_retained, "unloaded cell retained its fixed collider");
    }

    #[test]
    fn lifecycle_validator_detects_duplicate_and_orphaned_roots() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
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
    fn streaming_maps_creation_transforms_through_the_shared_contract() {
        assert_eq!(creation_to_bevy(Vec3::Y), Vec3::NEG_Z);
        assert_eq!(creation_to_bevy(Vec3::Z), Vec3::Y);

        // The shared contract decides the angle's sense, not the Bevy axes: this
        // Creation yaw takes +X to runtime +Z, where a Bevy +Y turn takes it to -Z.
        let rotation = creation_rotation_to_bevy([0.0, 0.0, std::f32::consts::FRAC_PI_2]);
        let rotated = rotation * Vec3::X;
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
        // The moved point's normal was recomputed from the welded field. On the west edge the
        // x stencil spans one interval (the point itself and its east neighbour):
        // ((10 - 100) / 128, 0, 1) normalized and scaled to the `i8` range, rather than the
        // [0, 0, 127] it was loaded with.
        assert_eq!(
            &arriving.normals[7 * 33 * 3..7 * 33 * 3 + 3],
            &[-73, 0, 104]
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
        // The welded corner's normal comes from the welded field. Both stencils span one
        // 128-unit interval at a corner: ((10 - 34) / 128, (10 - 10) / 128, 1) normalized and
        // scaled by 127, rather than the [0, 0, 127] it was loaded with.
        assert_eq!(&arriving.normals[..3], &[-23, 0, 125]);
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

    /// Welded edge and corner samples have one neighbour per axis, so their stencil spans one
    /// interval, not two. Dividing by the actual span keeps a planar slope's normal identical at
    /// the interior, every edge and every corner, matching the converter's `decode_normals`.
    #[test]
    fn welded_edge_and_corner_normals_match_the_interior_on_a_planar_slope() {
        let mut terrain = terrain_fixture(1, 0.0);
        let width = usize::from(terrain.width);
        // Rises 40 units per interval eastwards and 20 per interval northwards.
        for y in 0..width {
            for x in 0..width {
                terrain.heights[y * width + x] = 40.0 * x as f32 + 20.0 * y as f32;
            }
        }
        let last = width - 1;
        let interior = 16 * width + 16;
        let samples = [
            interior,
            16 * width,          // west edge
            16 * width + last,   // east edge
            16,                  // south edge
            last * width + 16,   // north edge
            0,                   // south-west corner
            last * width + last, // north-east corner
        ];
        recompute_packed_normals(&mut terrain, &samples);
        let expected = terrain.normals[interior * 3..interior * 3 + 3].to_vec();
        // (-40/128, -20/128, 1) normalized and scaled by 127.
        assert_eq!(expected, vec![-37, -19, 120]);
        for index in samples {
            assert_eq!(
                &terrain.normals[index * 3..index * 3 + 3],
                expected.as_slice(),
                "sample {index} disagrees with the interior normal"
            );
        }
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

    /// Zero and unresolved nonzero overlays retain ordered weights while malformed geometry
    /// still fails validation.
    #[test]
    fn terrain_unresolved_texture_overlays_preserve_weights_and_geometry_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("placeholder-catalogue.db");
        write_empty_catalogue(&path);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("INSERT INTO landscape_textures VALUES(2116,NULL);")
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        let mut terrain = terrain_fixture(91, 12.5);
        for layer in &mut terrain.layers {
            layer.texture_form_id = 0;
        }
        terrain.layers.extend([
            TerrainLayerSnapshot {
                texture_form_id: 0,
                quadrant: 0,
                layer: 4,
                is_base: false,
                weights: vec![(3, 0.625)],
            },
            TerrainLayerSnapshot {
                texture_form_id: 0,
                quadrant: 0,
                layer: 1,
                is_base: false,
                weights: vec![(0, 0.25), (18, 0.75)],
            },
        ]);
        validate_terrain_snapshot(&terrain, &catalog)
            .expect("normalized zero overlays retain their weights without image lookup");
        let layers = quadrant_layers(&terrain, 0).unwrap();
        assert_eq!(
            layers.iter().map(|layer| layer.layer).collect::<Vec<_>>(),
            vec![0, 1, 4]
        );
        let grids = quadrant_overlay_weights(&terrain, 0).unwrap();
        assert_eq!(grids.len(), 2);
        assert_eq!(grids[0][0].to_bits(), 0.25f32.to_bits());
        assert_eq!(grids[0][18].to_bits(), 0.75f32.to_bits());
        assert_eq!(grids[1][3].to_bits(), 0.625f32.to_bits());
        assert_eq!(grids[1][0], 0.0);
        assert!(build_terrain_quadrant_mesh(&terrain, 0).is_ok());

        terrain.layers[4].texture_form_id = 0x844;
        validate_terrain_snapshot(&terrain, &catalog)
            .expect("a missing diffuse image must not discard the cell and its objects");
        assert_eq!(quadrant_overlay_weights(&terrain, 0).unwrap(), grids);
        assert!(build_terrain_quadrant_mesh(&terrain, 0).is_ok());
        terrain.layers[4].texture_form_id = 0;
        assert!(validate_terrain_snapshot(&terrain, &catalog).is_ok());

        terrain.layers[4].weights.push((3, 0.5));
        assert!(
            validate_terrain_snapshot(&terrain, &catalog)
                .unwrap_err()
                .contains("duplicate VTXT")
        );
        terrain.layers[4].weights.pop();
        terrain.heights[0] = f32::NAN;
        assert!(
            validate_terrain_snapshot(&terrain, &catalog)
                .unwrap_err()
                .contains("non-finite height")
        );
    }

    #[test]
    fn overlay_weight_grids_hold_each_layers_vtxt_list_in_layer_order() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain.layers.extend([
            // Listed out of order: `quadrant_layers` sorts overlays by their ATXT layer number.
            TerrainLayerSnapshot {
                texture_form_id: 22,
                quadrant: 0,
                layer: 2,
                is_base: false,
                weights: vec![(3, 0.25)],
            },
            TerrainLayerSnapshot {
                texture_form_id: 21,
                quadrant: 0,
                layer: 1,
                is_base: false,
                weights: vec![(0, 1.0), (17 * 16 + 16, 0.5)],
            },
        ]);
        let grids = quadrant_overlay_weights(&terrain, 0).unwrap();
        assert_eq!(grids.len(), 2, "the base layer is not an overlay");
        assert_eq!(
            grids[0].len(),
            QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES,
            "a grid covers the quadrant's whole sample square"
        );
        assert_eq!(grids[0][0], 1.0);
        assert_eq!(grids[0][17 * 16 + 16], 0.5);
        assert_eq!(grids[0][3], 0.0, "an unnamed sample is opacity 0");
        assert_eq!(grids[1][3], 0.25, "layer 2 is the second overlay");
        assert_eq!(grids[1][0], 0.0);
        assert!(
            quadrant_overlay_weights(&terrain, 1).unwrap().is_empty(),
            "a quadrant with only a base layer has no overlays"
        );
    }

    #[test]
    fn overlay_weight_grids_reject_vtxt_outside_the_sample_square() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain.layers.push(TerrainLayerSnapshot {
            texture_form_id: 21,
            quadrant: 0,
            layer: 1,
            is_base: false,
            weights: vec![(17 * 17, 1.0)],
        });
        assert!(quadrant_overlay_weights(&terrain, 0).is_err());
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
    fn effect_palette_material_needs_its_palette_loaded_through_an_srgb_view() {
        let mut images = Assets::<Image>::default();
        let mut image = |format| {
            images.add(Image::new_fill(
                bevy::render::render_resource::Extent3d {
                    width: 2,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                bevy::render::render_resource::TextureDimension::D2,
                &[255, 255, 255, 255],
                format,
                RenderAssetUsages::default(),
            ))
        };
        let srgb = image(bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb);
        let linear = image(bevy::render::render_resource::TextureFormat::Rgba8Unorm);
        let material = |palette: Handle<Image>| EffectPaletteMaterial {
            base: StandardMaterial {
                base_color_texture: Some(srgb.clone()),
                ..default()
            },
            extension: crate::nif_material::EffectPalette::new(palette, true, true, 1.6),
        };
        assert_eq!(
            validate_effect_palette_material(&material(srgb.clone()), &images),
            Ok(2)
        );
        assert!(
            validate_effect_palette_material(&material(linear), &images)
                .unwrap_err()
                .contains("palette image color space mismatch")
        );
        assert!(
            validate_effect_palette_material(&material(Handle::default()), &images)
                .unwrap_err()
                .contains("palette image")
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

    use crate::render::add_placed_object_layer_propagation;
    use crate::world::database::ReferenceRow;
    use bevy::asset::{AssetApp, AssetPlugin};
    use bevy::world_serialization::WorldSerializationPlugin;

    /// The app the model tests run in: the unload pacer, the arming pacer and the real readiness
    /// scan ([`track_asset_readiness`]) over an asset server and the world serialization spawner
    /// the engine uses, so a converted model is spawned and its reference becomes ready by the same
    /// route a converted glb takes. The three run chained, in the order `StreamingPlugin` runs
    /// them, so a test can tell which frame of that chain a model answered on.
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
        .init_resource::<StreamingWorld>()
        .init_resource::<lod::LodStreaming>()
        .init_resource::<TerrainContinuity>()
        .init_resource::<ActiveSpace>()
        .init_resource::<ProfilingState>()
        .init_resource::<DiagnosticFallbackAssets>()
        .init_resource::<SceneSpawnBatch>()
        .init_resource::<StaticCollisionCache>()
        .init_resource::<crate::pacing::PacingTracker>()
        // The converted scene holds entities, and the spawner reads each of their components out
        // of the type registry.
        .register_type::<ChildOf>()
        .register_type::<Children>()
        .register_type::<GlobalTransform>()
        .register_type::<Mesh3d>()
        .register_type::<MeshMaterial3d<StandardMaterial>>()
        .register_type::<Name>()
        .register_type::<Transform>()
        .add_observer(mark_world_instance_ready)
        .add_systems(
            Update,
            (despawn_cells, arm_pending_models, track_asset_readiness).chain(),
        )
        .add_systems(
            SpawnScene,
            (
                begin_scene_spawn_batch.before(SceneSpawnerSystems::WorldInstanceSpawn),
                end_scene_spawn_batch.after(SceneSpawnerSystems::WorldInstanceSpawn),
            ),
        );
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

    /// A converted model whose primitive passes readiness: the mesh the converter exported and the
    /// material it points at, so the scan has something to validate rather than an empty model.
    fn converted_scene_with_material(
        mesh: Handle<Mesh>,
        material: Handle<StandardMaterial>,
    ) -> World {
        let mut world = World::new();
        // The loader gives every node a transform, the scene's own root included: the bounds walk
        // reads one from every node below the reference.
        let root = world
            .spawn((Name::new("wispambush"), Transform::default()))
            .id();
        world.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            ChildOf(root),
        ));
        world
    }

    /// The aggregate bounds the converter writes for a `Cuboid::new(2.0, 4.0, 6.0)` primitive in a
    /// reference's own frame, which is the frame the bounds check compares them in.
    fn cuboid_bounds() -> ExpectedModelBounds {
        ExpectedModelBounds::new(Vec3::new(-1.0, -2.0, -3.0), Vec3::new(1.0, 2.0, 3.0))
            .expect("the fixture bounds are finite and not degenerate")
    }

    /// A config whose arming budget is `budget` models per frame.
    fn paced_config(budget: usize) -> EngineConfig {
        EngineConfig {
            max_model_spawns_per_frame: budget,
            ..default()
        }
    }

    #[test]
    fn ready_fixed_reference_gets_proxy_but_movable_reference_does_not() {
        let mut app = model_app();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(2.0, 2.0, 2.0));
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let mut scene = World::new();
        let scene_root = scene.spawn(Transform::default()).id();
        scene.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            ChildOf(scene_root),
        ));
        let handle = add_converted_model(&mut app, scene);
        let bounds = ExpectedModelBounds::new(Vec3::splat(-1.0), Vec3::splat(1.0)).unwrap();
        let fixed = spawn_model_reference(&mut app, handle.clone(), Some(bounds));
        let movable = spawn_model_reference(&mut app, handle, Some(bounds));
        for (entity, kind) in [(fixed, "STAT"), (movable, "MISC")] {
            let mut entity_mut = app.world_mut().entity_mut(entity);
            let mut pending = entity_mut.get_mut::<PendingAssetProfile>().unwrap();
            pending.path = "meshes/landscape/rocks/rockl01.glb".to_owned();
            pending.base_record_type = Some(kind.to_owned());
            pending.static_physics = true;
        }
        let metrics = settle_readiness(&mut app);
        assert_eq!(metrics.static_colliders_proxy, 1);
        assert_eq!(metrics.static_colliders_skipped, 0);
        assert_eq!(
            app.world().get::<StaticColliderSource>(fixed),
            Some(&StaticColliderSource::RenderProxy)
        );
        assert!(app.world().get::<Collider>(fixed).is_some());
        assert!(app.world().get::<Collider>(movable).is_none());
    }

    /// The reference root components every model path shares, as [`spawn_cell`] spawns them: the
    /// placement, the ids, and the pending profile the readiness scan waits on.
    fn reference_root() -> impl Bundle {
        let transform = Transform::from_translation(Vec3::new(3.0, -4.0, 5.0));
        (
            Name::new("Reference 000F9907"),
            FormId(0x00F9907),
            CellRef(0x02D4E0),
            transform,
            GlobalTransform::from(transform),
            WorldTransform(transform.to_matrix()),
            Propagate(PLACED_OBJECT_RENDER_LAYERS),
            PendingAssetProfile {
                started: Instant::now(),
                scene_spawned: false,
                path: "meshes/furniture/creatureexit/wispambush.glb".to_owned(),
                form_id: 0x00F9907,
                base_form_id: 0x00EF957,
                base_record_type: None,
                static_physics: false,
                cell_id: 0x02D4E0,
                actor_scene: false,
            },
        )
    }

    /// A reference as [`spawn_cell`] spawns one for a model whose asset is already loaded: the root
    /// components, the asset root pointing at the loaded scene, and the pending profile the
    /// readiness scan waits on. No `ExpectedModelBounds` is inserted, which is what
    /// `statics.bounds_valid = 0` produces - the component's absence is the whole signal.
    fn spawn_model_reference(
        app: &mut App,
        handle: Handle<WorldAsset>,
        expected_bounds: Option<ExpectedModelBounds>,
    ) -> Entity {
        let mut entity = app
            .world_mut()
            .spawn((reference_root(), WorldAssetRoot(handle)));
        if let Some(bounds) = expected_bounds {
            entity.insert(bounds);
        }
        entity.id()
    }

    /// A reference as `spawn_cell` leaves one for [`arm_pending_models`]: the same root components
    /// and the model it found, but still pending, with `sequence` its place in spawn order. The
    /// cell path assigns that sequence from [`StreamingWorld::next_model_sequence`] as it walks the
    /// cell's references, so handing the sequence in is what a test does instead of committing a
    /// cell.
    fn spawn_pending_reference(app: &mut App, handle: Handle<WorldAsset>, sequence: u64) -> Entity {
        app.world_mut()
            .spawn((reference_root(), PendingModel { handle, sequence }))
            .id()
    }

    /// The references the arming pacer has handed to the spawner and the ones still waiting for
    /// their turn, which is the whole queue: arming removes the component that put a model in it.
    fn armed_and_pending(app: &mut App) -> (usize, usize) {
        let mut armed = app
            .world_mut()
            .query_filtered::<Entity, With<WorldAssetRoot>>();
        let armed = armed.iter(app.world()).count();
        let mut pending = app
            .world_mut()
            .query_filtered::<Entity, With<PendingModel>>();
        let pending = pending.iter(app.world()).count();
        (armed, pending)
    }

    /// Runs the readiness scan until the reference leaves the pending set, and fails the test
    /// rather than reading an unsettled metric. The world instance is spawned in `SpawnScene` and
    /// the scan runs in `Update`, so a reference settles over more than one frame. A model still
    /// waiting in the arming queue is not pending yet, so the queue has to drain too.
    fn settle_readiness(app: &mut App) -> StreamingMetrics {
        for _ in 0..16 {
            app.update();
            let streaming = app.world().resource::<StreamingMetrics>();
            if streaming.pending_asset_instances == 0 && streaming.arming_queue_depth == 0 {
                break;
            }
        }
        let metrics = app.world().resource::<StreamingMetrics>().clone();
        assert_eq!(
            metrics.pending_asset_instances, 0,
            "the reference never left the pending set"
        );
        assert_eq!(
            metrics.arming_queue_depth, 0,
            "the arming queue never drained"
        );
        metrics
    }

    /// The layer a reference draws on is what keeps it out of the water reflection pass
    /// (`render::REFLECTION_VIEW_LAYERS` renders the world layer only). Only `spawn_cell` spawns
    /// references, so the propagation has to be registered there and nowhere else - in particular
    /// not on the cell root, whose other children are terrain and water.
    #[test]
    fn spawn_cell_leaves_terrain_and_water_on_their_own_layers() {
        let mut app = model_app();
        add_placed_object_layer_propagation(&mut app);
        app.init_resource::<AssetCatalog>()
            .init_resource::<ActorCatalog>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .add_systems(Update, spawn_test_cell);
        app.update();
        app.update();

        let mut references = app
            .world_mut()
            .query_filtered::<(&RenderLayers, &Propagate<RenderLayers>), With<MeshHandle>>();
        let propagated: Vec<_> = references
            .iter(app.world())
            .map(|(layers, propagate)| (layers.clone(), propagate.0.clone()))
            .collect();
        assert_eq!(propagated.len(), 1, "the cell has one model reference");
        for layers in [&propagated[0].0, &propagated[0].1] {
            assert_eq!(
                layers,
                &RenderLayers::layer(crate::render::PLACED_OBJECT_LAYER),
                "a reference and everything below it draws the placed-object layer"
            );
        }

        let mut terrain = app
            .world_mut()
            .query_filtered::<Option<&RenderLayers>, With<TerrainPatch>>();
        let quadrants: Vec<_> = terrain
            .iter(app.world())
            .map(|layers| layers.cloned().unwrap_or_default())
            .collect();
        assert_eq!(quadrants.len(), 4, "the cell has four terrain quadrants");
        for layers in quadrants {
            assert!(
                layers.intersects(&RenderLayers::layer(crate::render::WORLD_LAYER)),
                "terrain is what the reflection pass draws"
            );
            assert!(
                !layers.intersects(&PLACED_OBJECT_RENDER_LAYERS),
                "propagating the placed-object layer from the cell root would drop the terrain \
                 out of the reflection pass"
            );
        }

        let mut water = app
            .world_mut()
            .query_filtered::<&RenderLayers, With<WaterSurface>>();
        let layers = water
            .single(app.world())
            .expect("the cell has one water surface");
        assert_eq!(layers, &RenderLayers::layer(WATER_LAYER));
    }

    #[test]
    fn flat_water_query_matches_spawned_surface_and_cell_unload() {
        use crate::world::water::water_at_point;

        let mut app = model_app();
        app.init_resource::<AssetCatalog>()
            .init_resource::<ActorCatalog>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .add_systems(Update, spawn_test_cell);
        app.update();

        let point = WorldPosition::from_creation_units(Vec3::new(64.0, 64.0, 100.0));
        let mut surfaces = app
            .world_mut()
            .query_filtered::<(&FlatWaterBody, &Transform, &ChildOf), With<WaterSurface>>();
        let (body, transform, parent) = surfaces.single(app.world()).unwrap();
        let root = parent.parent();
        let sample = water_at_point([body], 60, point).unwrap();
        assert_eq!(sample.cell_id, 0x02D4E0);
        assert_eq!(sample.surface.height, transform.translation.y);
        assert_eq!(sample.surface.water_type_form_id, None);

        // A floating-origin shift changes rendering, not this Creation-space query.
        app.world_mut()
            .get_mut::<Transform>(root)
            .unwrap()
            .translation += Vec3::new(4096.0, 0.0, -8192.0);
        let mut bodies = app.world_mut().query::<&FlatWaterBody>();
        assert_eq!(
            water_at_point(bodies.iter(app.world()), 60, point),
            Some(sample)
        );

        // Retiring meshes remain queryable until the real unload removes their cell subtree.
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 0,
                    grid_y: 0,
                },
                CellStatus::Retiring { root },
            );
        assert_eq!(
            water_at_point(bodies.iter(app.world()), 60, point),
            Some(sample)
        );
        assert_eq!(unload_all_cells_now(app.world_mut()), 1);
        let mut bodies = app.world_mut().query::<&FlatWaterBody>();
        assert!(water_at_point(bodies.iter(app.world()), 60, point).is_none());
        app.update();
        assert!(water_at_point(bodies.iter(app.world()), 60, point).is_none());
    }

    /// A model reference's meshes arrive from the converted glb, frames after the reference itself,
    /// and they are what the reflection pass must not draw.
    #[test]
    fn a_reference_glb_lands_on_the_placed_object_layer() {
        let mut app = model_app();
        add_placed_object_layer_propagation(&mut app);
        let handle = add_converted_model(&mut app, converted_scene_with_mesh(Handle::default()));
        let reference = spawn_model_reference(&mut app, handle, None);
        settle_readiness(&mut app);

        let mut primitives = app.world_mut().query_filtered::<Entity, With<Mesh3d>>();
        let meshes: Vec<_> = primitives.iter(app.world()).collect();
        assert_eq!(meshes.len(), 1, "the converted model has one primitive");
        for entity in [reference, meshes[0]] {
            assert_eq!(
                app.world().entity(entity).get::<RenderLayers>(),
                Some(&PLACED_OBJECT_RENDER_LAYERS),
                "the mesh below a reference must carry the placed-object layer"
            );
        }
    }

    /// A committed cell times its real work: the terrain mesh and collider build used to be
    /// recorded against an `Instant` made on the line before, so it always read zero. The
    /// reference loop has a span of its own, and the cell reports what it carried and loaded.
    #[test]
    fn spawn_cell_times_the_terrain_build_and_the_reference_loop() {
        let mut app = model_app();
        add_placed_object_layer_propagation(&mut app);
        app.init_resource::<AssetCatalog>()
            .init_resource::<ActorCatalog>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .add_systems(Update, spawn_test_cell);
        app.update();

        let profiler = app.world().resource::<ProfilingState>();
        let terrain = profiler.span_samples("streaming/terrain_mesh");
        assert_eq!(terrain.len(), 1, "one terrain build for the one cell");
        assert!(terrain[0] > 0.0, "the terrain build took measurable time");
        assert_eq!(profiler.span_samples("streaming/spawn_references").len(), 1);
        assert_eq!(profiler.span_samples("streaming/spawn_cell").len(), 1);
        assert!(
            profiler.has_event_stage("spawned references=1 model_loads=1"),
            "the cell reports its reference and model-load counts"
        );
    }

    /// Spawns the cell the two layer tests above read: a textureless LAND with water, and one
    /// reference with a model path, through the same [`spawn_cell`] a streamed cell goes through.
    #[allow(clippy::too_many_arguments)]
    fn spawn_test_cell(
        mut commands: Commands,
        mut spawned: Local<bool>,
        asset_server: Res<AssetServer>,
        catalog: Res<AssetCatalog>,
        actor_catalog: Res<ActorCatalog>,
        reflection: Res<WaterReflectionTexture>,
        mut meshes: ResMut<Assets<Mesh>>,
        mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
        mut water_materials: ResMut<Assets<WaterMaterial>>,
        mut profiler: ResMut<ProfilingState>,
    ) {
        if *spawned {
            return;
        }
        *spawned = true;
        let mut terrain = terrain_fixture(0x02D4E0, 0.0);
        terrain.layers.clear();
        terrain.water_height = Some(12.0);
        spawn_cell(
            &mut commands,
            &asset_server,
            &catalog,
            &actor_catalog,
            &reflection,
            &mut meshes,
            &mut terrain_materials,
            &mut water_materials,
            IVec2::ZERO,
            false,
            false,
            false,
            CellPayload {
                generation: 1,
                key: CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 0,
                    grid_y: 0,
                },
                cell_id: 0x02D4E0,
                references: vec![ReferenceRow {
                    form_id: 0x00F9907,
                    initially_enabled: true,
                    cell_id: 0x02D4E0,
                    base_form_id: 0x00EF957,
                    base_record_type: None,
                    base_is_marker: false,
                    model_path: Some("meshes\\furniture\\creatureexit\\wispambush.nif".to_owned()),
                    position: [0.0, 0.0, 0.0],
                    rotation: [0.0; 3],
                    scale: 1.0,
                    bounds_min: [0.0; 3],
                    bounds_max: [0.0; 3],
                    bounds_valid: false,
                    light: None,
                    light_radius_override: None,
                    door: None,
                    emittance: None,
                }],
            },
            Some(terrain),
            None,
            &mut 0,
            &mut profiler,
        )
        .expect("spawn cell");
    }
    /// The spawn-batch measurement reads the world rather than the spawner's bookkeeping: three
    /// models whose assets are already loaded are reported as three instances on the frame Bevy
    /// writes them into the world, and as none on the frame after, while the readiness scan
    /// completes the three of them on its own, later, scan.
    #[test]
    fn the_spawn_batch_counts_the_instances_bevy_instantiated_this_frame() {
        let mut app = model_app();
        // Every model is published before any reference points at it, so all three references are
        // handed to the spawner on the same frame.
        let handles: Vec<_> = (0..3)
            .map(|_| add_converted_model(&mut app, empty_converted_scene()))
            .collect();
        for handle in handles {
            spawn_model_reference(&mut app, handle, None);
        }

        app.update();
        let arrival = streaming_metrics(&app);
        assert_eq!(
            arrival.instances_spawned_this_frame, 3,
            "three references were handed to the spawner with their assets loaded"
        );
        assert_eq!(
            arrival.instances_armed_this_frame, 3,
            "the references joining the spawner's queue are the models armed this frame"
        );
        assert_eq!(
            arrival.instances_completed_this_scan, 0,
            "the scan validates an instance on a later frame than the one it spawns on"
        );

        app.update();
        let quiet = streaming_metrics(&app);
        assert_eq!(
            quiet.instances_spawned_this_frame, 0,
            "an instance spawns once, so the frame after reports none"
        );
        assert_eq!(
            quiet.instances_armed_this_frame, 0,
            "a model arms once, so the frame after reports none"
        );
        assert_eq!(quiet.max_instances_spawned_per_frame, 3);
        assert_eq!(
            quiet.instances_completed_this_scan, 3,
            "the scan completes the three instances it accepts"
        );
        assert_eq!(quiet.max_instances_completed_per_scan, 3);
        assert_eq!(
            quiet.empty_model_references, 3,
            "the empty converted scenes are counted, not failed"
        );
    }

    /// Pacing is what a frame instantiates: a cell's models wait in the arming queue and are handed
    /// over a budget at a time, so the spawn batch is the armed batch and a drained queue stays
    /// drained.
    #[test]
    fn the_arming_budget_spreads_a_cells_models_over_frames() {
        let mut app = model_app();
        app.insert_resource(paced_config(2));
        let handles: Vec<_> = (0..5)
            .map(|_| add_converted_model(&mut app, empty_converted_scene()))
            .collect();
        for (sequence, handle) in handles.into_iter().enumerate() {
            spawn_pending_reference(&mut app, handle, sequence as u64);
        }

        app.update();
        let first = streaming_metrics(&app);
        assert_eq!(
            armed_and_pending(&mut app),
            (2, 3),
            "a budget of two hands two models over and leaves the rest queued"
        );
        assert_eq!(first.arming_queue_depth, 3);
        assert_eq!(
            first.peak_arming_queue_depth, 5,
            "the peak is the cell's whole backlog"
        );
        assert_eq!(
            first.instances_spawned_this_frame, 2,
            "the frame instantiates the models the budget armed, not the whole cell"
        );

        app.update();
        assert_eq!(armed_and_pending(&mut app), (4, 1));
        app.update();
        assert_eq!(armed_and_pending(&mut app), (5, 0));
        assert_eq!(streaming_metrics(&app).arming_queue_depth, 0);

        app.update();
        assert_eq!(
            armed_and_pending(&mut app),
            (5, 0),
            "a model left the queue when it was armed, so none is armed twice"
        );
        assert_eq!(streaming_metrics(&app).instances_spawned_this_frame, 0);
    }

    /// A backlog drains oldest first, whatever order the references were spawned in: the entities
    /// here are created newest-first, so arming in entity order would arm the wrong model.
    #[test]
    fn the_arming_budget_arms_the_oldest_reference_first() {
        let mut app = model_app();
        app.insert_resource(paced_config(1));
        let handles: Vec<_> = (0..4)
            .map(|_| add_converted_model(&mut app, empty_converted_scene()))
            .collect();
        let mut oldest_first = Vec::new();
        for (index, handle) in handles.into_iter().enumerate() {
            // The reference spawned first is the newest model: the sequences run the other way.
            oldest_first.push(spawn_pending_reference(&mut app, handle, 3 - index as u64));
        }
        oldest_first.reverse();

        for (frame, reference) in oldest_first.iter().enumerate() {
            app.update();
            assert!(
                app.world().entity(*reference).contains::<WorldAssetRoot>(),
                "frame {frame} armed a newer model while the oldest one was still waiting"
            );
            assert_eq!(armed_and_pending(&mut app), (frame + 1, 3 - frame));
        }
    }

    fn add_priority_request(
        app: &mut App,
        entity: Entity,
        center: bevy::math::DVec3,
        radius: Option<f64>,
        collision_candidate: bool,
        coarse_terrain: bool,
        cell: Option<CellKey>,
    ) {
        let model = app.world().get::<PendingModel>(entity).unwrap();
        let request = requests::SceneRequest {
            path: "fixture.glb".to_owned(),
            cell,
            coarse_terrain,
            retry_cleanup: None,
            content_identity: None,
            sequence: model.sequence,
            center,
            radius,
            collision_candidate,
            handle: Some(model.handle.clone()),
            started: Instant::now(),
            allow_retry: false,
        };
        app.world_mut().entity_mut(entity).insert(request);
    }

    #[test]
    fn priority_activation_rechecks_turns_and_shares_the_quota_with_coarse_lod() {
        use bevy::math::DVec3;
        let mut app = model_app();
        let handle = add_converted_model(&mut app, empty_converted_scene());
        app.insert_resource(EngineConfig {
            prioritize_streaming: true,
            max_model_spawns_per_frame: 1,
            ..default()
        })
        .init_resource::<requests::SceneSchedulingState>()
        .insert_resource(RenderOrigin(IVec2::new(-4, 7)));
        let camera = app
            .world_mut()
            .spawn((StreamingCamera, Transform::default()))
            .id();
        let origin = DVec3::new(
            -4.0 * f64::from(CELL_SIZE),
            0.0,
            -7.0 * f64::from(CELL_SIZE),
        );
        let coarse = spawn_pending_reference(&mut app, handle.clone(), 0);
        add_priority_request(&mut app, coarse, origin, Some(40000.0), false, true, None);
        let front = spawn_pending_reference(&mut app, handle.clone(), 1);
        add_priority_request(
            &mut app,
            front,
            origin + DVec3::NEG_Z * 1000.0,
            Some(100.0),
            false,
            false,
            None,
        );
        let back = spawn_pending_reference(&mut app, handle.clone(), 2);
        add_priority_request(
            &mut app,
            back,
            origin + DVec3::Z * 1000.0,
            Some(100.0),
            false,
            false,
            None,
        );
        let front_clutter = spawn_pending_reference(&mut app, handle, 3);
        add_priority_request(
            &mut app,
            front_clutter,
            origin + DVec3::NEG_Z * 500.0,
            Some(2.0),
            false,
            false,
            None,
        );

        app.update();
        assert!(app.world().get::<WorldAssetRoot>(front).is_some());
        assert!(app.world().get::<WorldAssetRoot>(coarse).is_none());
        assert_eq!(armed_and_pending(&mut app), (1, 3));
        app.world_mut()
            .get_mut::<Transform>(camera)
            .unwrap()
            .rotation = Quat::from_rotation_y(std::f32::consts::PI);
        app.update();
        assert!(app.world().get::<WorldAssetRoot>(back).is_some());
        assert!(app.world().get::<WorldAssetRoot>(front_clutter).is_none());
        assert_eq!(armed_and_pending(&mut app), (2, 2));
        // The eighth actual activation reserves service for the old coarse request.
        app.world_mut()
            .resource_mut::<requests::SceneSchedulingState>()
            .activation_choices = 7;
        app.update();
        assert!(app.world().get::<WorldAssetRoot>(coarse).is_some());
        assert_eq!(armed_and_pending(&mut app), (3, 1));
    }

    #[test]
    fn priority_activation_protects_collision_and_holds_retiring_work_until_revival() {
        use bevy::ecs::system::RunSystemOnce;
        use bevy::math::DVec3;
        let mut app = model_app();
        let handle = add_converted_model(&mut app, empty_converted_scene());
        app.insert_resource(EngineConfig {
            prioritize_streaming: true,
            max_model_spawns_per_frame: 1,
            ..default()
        })
        .init_resource::<requests::SceneSchedulingState>();
        app.world_mut()
            .spawn((StreamingCamera, Transform::default()));
        let cell = app.world_mut().spawn_empty().id();
        let key = exterior_key(-2, -3);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(key, CellStatus::Retiring { root: cell });
        let retired = spawn_pending_reference(&mut app, handle.clone(), 0);
        add_priority_request(
            &mut app,
            retired,
            DVec3::NEG_Z * 20.0,
            Some(10.0),
            true,
            false,
            Some(key),
        );
        let building = spawn_pending_reference(&mut app, handle.clone(), 1);
        add_priority_request(
            &mut app,
            building,
            DVec3::NEG_Z * 1000.0,
            Some(100.0),
            false,
            false,
            None,
        );
        let collision = spawn_pending_reference(&mut app, handle, 2);
        add_priority_request(
            &mut app,
            collision,
            DVec3::Z * 100.0,
            Some(10.0),
            true,
            false,
            None,
        );

        app.world_mut().run_system_once(arm_pending_models).unwrap();
        assert!(app.world().get::<WorldAssetRoot>(collision).is_some());
        assert!(app.world().get::<WorldAssetRoot>(building).is_none());
        assert!(app.world().get::<WorldAssetRoot>(retired).is_none());
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(key, CellStatus::Resident { root: cell });
        app.world_mut().run_system_once(arm_pending_models).unwrap();
        assert!(app.world().get::<WorldAssetRoot>(retired).is_some());
        assert!(app.world().get::<WorldAssetRoot>(building).is_none());
    }

    /// A budget is spent on the models that can spawn now: a model whose converted scene has not
    /// arrived keeps its place in the queue without holding up the models behind it, and takes its
    /// turn as soon as it can.
    #[test]
    fn a_model_still_loading_does_not_use_a_budget_slot() {
        let mut app = model_app();
        app.insert_resource(paced_config(2));
        // A handle `Assets<WorldAsset>` has no entry for is the state a loading model is in: the
        // loader has not published the scene yet. The default handle is exactly that - identity
        // without storage - and this one gets its scene before the test ends.
        let loading = Handle::<WorldAsset>::default();
        let oldest = spawn_pending_reference(&mut app, loading.clone(), 0);
        let handles: Vec<_> = (0..4)
            .map(|_| add_converted_model(&mut app, empty_converted_scene()))
            .collect();
        for (sequence, handle) in handles.into_iter().enumerate() {
            spawn_pending_reference(&mut app, handle, sequence as u64 + 1);
        }

        app.update();
        assert!(
            !app.world().entity(oldest).contains::<WorldAssetRoot>(),
            "a model whose scene has not arrived is not armed"
        );
        assert_eq!(
            armed_and_pending(&mut app),
            (2, 3),
            "the budget went to the two models that are ready, not to the loading one"
        );
        assert_eq!(streaming_metrics(&app).arming_queue_depth, 3);

        app.world_mut()
            .resource_mut::<Assets<WorldAsset>>()
            .insert(&loading, WorldAsset::new(empty_converted_scene()))
            .expect("publishing the model's scene cannot fail");
        app.update();
        assert!(
            app.world().entity(oldest).contains::<WorldAssetRoot>(),
            "the loading model is armed once its scene arrives"
        );
        assert_eq!(armed_and_pending(&mut app), (4, 1));
    }

    /// A model whose cell is unloaded before its turn needs no cleanup, and the pacer cannot trip
    /// over it: the queue is the component set, and the despawn takes it with the subtree.
    #[test]
    fn a_pending_model_whose_cell_is_despawned_leaves_nothing_queued() {
        let mut app = model_app();
        app.insert_resource(paced_config(1));
        let handle = add_converted_model(&mut app, empty_converted_scene());
        let cell = app
            .world_mut()
            .spawn((
                Name::new("Cell"),
                StreamedCellRoot,
                ExteriorCellGrid(IVec2::ZERO),
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        for sequence in 0..2 {
            let reference = spawn_pending_reference(&mut app, handle.clone(), sequence);
            app.world_mut().entity_mut(reference).insert(ChildOf(cell));
        }
        // The cell is outside the unload radius and its turn in the unload budget has come.
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(exterior_key(4, 4), CellStatus::Retiring { root: cell });

        app.update();

        assert_eq!(
            armed_and_pending(&mut app),
            (0, 0),
            "the cell took its pending models with it; none was armed and none is queued"
        );
        let metrics = streaming_metrics(&app);
        assert_eq!(metrics.arming_queue_depth, 0);
        assert_eq!(metrics.unloaded_cells, 1, "the cell itself was unloaded");
        assert_eq!(metrics.instances_spawned_this_frame, 0);
    }

    /// `0` is the unbudgeted behaviour: every model whose scene is ready is handed over at once.
    #[test]
    fn an_arming_budget_of_zero_arms_every_ready_model() {
        let mut app = model_app();
        app.insert_resource(paced_config(0));
        let handles: Vec<_> = (0..5)
            .map(|_| add_converted_model(&mut app, empty_converted_scene()))
            .collect();
        for (sequence, handle) in handles.into_iter().enumerate() {
            spawn_pending_reference(&mut app, handle, sequence as u64);
        }

        app.update();
        assert_eq!(armed_and_pending(&mut app), (5, 0));
        assert_eq!(
            streaming_metrics(&app).instances_spawned_this_frame,
            5,
            "without a budget the whole batch lands on one frame, as it did before the pacer"
        );
    }

    /// Pacing moves the frame a model is spawned on, not the totals: every queued model is armed
    /// once, spawned once, and validated exactly once, and no frame instantiates more than the
    /// budget.
    #[test]
    fn every_paced_model_is_spawned_and_validated_exactly_once() {
        let mut app = model_app();
        app.insert_resource(paced_config(2));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let mesh = app
                .world_mut()
                .resource_mut::<Assets<Mesh>>()
                .add(Cuboid::new(2.0, 4.0, 6.0));
            let material = app
                .world_mut()
                .resource_mut::<Assets<StandardMaterial>>()
                .add(StandardMaterial::default());
            handles.push(add_converted_model(
                &mut app,
                converted_scene_with_material(mesh, material),
            ));
        }
        for (sequence, handle) in handles.into_iter().enumerate() {
            let reference = spawn_pending_reference(&mut app, handle, sequence as u64);
            app.world_mut()
                .entity_mut(reference)
                .insert(cuboid_bounds());
        }

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.assets_ready, 5,
            "every instance is validated once: {:?}",
            metrics.asset_failures
        );
        assert_eq!(metrics.meshes_validated, 5);
        assert_eq!(metrics.materials_validated, 5);
        assert_eq!(metrics.transform_instances_validated, 5);
        assert_eq!(metrics.empty_model_references, 0);
        assert_eq!(metrics.peak_arming_queue_depth, 5);
        assert_eq!(
            metrics.max_instances_spawned_per_frame, 2,
            "no frame instantiated more than the budget allowed"
        );
        assert_eq!(armed_and_pending(&mut app), (5, 0));
        let mut instances = app
            .world_mut()
            .query_filtered::<Entity, With<WorldInstance>>();
        assert_eq!(
            instances.iter(app.world()).count(),
            5,
            "one instance per reference, paced or not"
        );
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

    /// Actor references have no `statics` row, so an actor scene never has converted aggregate
    /// bounds. It still finishes readiness: its transforms and render assets are validated and the
    /// spawned hierarchy supplies its own bounds.
    #[test]
    fn an_actor_scene_without_static_bounds_becomes_ready() {
        let mut app = model_app();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(2.0, 4.0, 6.0));
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let handle = add_converted_model(&mut app, converted_scene_with_material(mesh, material));
        let reference = spawn_model_reference(&mut app, handle, None);
        app.world_mut()
            .entity_mut(reference)
            .get_mut::<PendingAssetProfile>()
            .expect("the pending profile")
            .actor_scene = true;

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.transform_bounds_validation_failures, 0,
            "an actor has no static bounds to compare: {:?}",
            metrics.asset_failures
        );
        assert!(
            metrics.asset_failures.is_empty(),
            "nothing is recorded as a failed asset: {:?}",
            metrics.asset_failures
        );
        assert_eq!(metrics.assets_ready, 1);
        assert_eq!(metrics.transform_instances_validated, 1);
        assert_eq!(metrics.bounds_validated, 1);
        assert_eq!(metrics.meshes_validated, 1);
        assert_eq!(metrics.materials_validated, 1);
        assert_eq!(metrics.empty_model_references, 0);
        assert!(
            !app.world()
                .entity(reference)
                .contains::<PendingAssetProfile>(),
            "the actor scene is no longer pending"
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
            let Ok((mesh_handle, _, _, _, _)) = primitives.get(descendant) else {
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
            false,
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

    use crate::world::database::LightRow;

    fn light_row(radius: f32, color: [u8; 3], flags: u32) -> LightRow {
        LightRow {
            radius,
            color,
            flags,
        }
    }

    /// A reference in interior cell 99 with a light row and, optionally, an `XRDS` radius of its
    /// own, as the database hands one to `spawn_cell`. The model path is empty: the light does not
    /// depend on the reference having a model - that is the case it has to work for.
    fn lit_reference(
        form_id: u32,
        light: Option<LightRow>,
        radius_override: Option<f32>,
    ) -> ReferenceRow {
        ReferenceRow {
            form_id,
            initially_enabled: true,
            cell_id: 99,
            base_form_id: 0x200 + form_id,
            base_record_type: None,
            base_is_marker: false,
            model_path: None,
            position: [100.0, 50.0, -200.0],
            rotation: [0.0; 3],
            scale: 1.0,
            bounds_min: [0.0; 3],
            bounds_max: [0.0; 3],
            bounds_valid: false,
            light,
            light_radius_override: radius_override,
            door: None,
            emittance: None,
        }
    }

    #[derive(Resource, Default)]
    struct QueuedReferences(Vec<ReferenceRow>);

    /// The cell root the last [`spawn_queued_references`] produced.
    #[derive(Resource, Default)]
    struct SpawnedCellRoot(Option<Entity>);

    #[allow(clippy::too_many_arguments)]
    fn spawn_queued_references(
        mut commands: Commands,
        config: Res<EngineConfig>,
        queued: Res<QueuedReferences>,
        mut root: ResMut<SpawnedCellRoot>,
        asset_server: Res<AssetServer>,
        catalog: Res<AssetCatalog>,
        actor_catalog: Res<ActorCatalog>,
        reflection: Res<WaterReflectionTexture>,
        mut meshes: ResMut<Assets<Mesh>>,
        mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
        mut water_materials: ResMut<Assets<WaterMaterial>>,
        mut profiler: ResMut<ProfilingState>,
    ) {
        root.0 = Some(
            spawn_cell(
                &mut commands,
                &asset_server,
                &catalog,
                &actor_catalog,
                &reflection,
                &mut meshes,
                &mut terrain_materials,
                &mut water_materials,
                IVec2::ZERO,
                config.lights,
                false,
                false,
                CellPayload {
                    generation: 1,
                    key: CellKey::Interior(99),
                    cell_id: 99,
                    references: queued.0.clone(),
                },
                None,
                None,
                &mut 0,
                &mut profiler,
            )
            .expect("spawn cell"),
        );
    }

    /// An empty catalogue database: the three tables [`AssetCatalog::open`] reads, with no rows.
    fn write_empty_catalogue(path: &std::path::Path) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
                 CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
                 CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);",
            )
            .unwrap();
    }

    /// An app that spawns one interior cell holding `references` through the real [`spawn_cell`],
    /// with the engine's lights flag set as `lights`.
    fn spawn_reference_cell_app(references: Vec<ReferenceRow>, lights: bool) -> App {
        spawn_reference_cell_app_with_catalog(references, lights, ActorCatalog::default())
    }

    /// The same app with a populated actor catalog.
    fn spawn_reference_cell_app_with_catalog(
        references: Vec<ReferenceRow>,
        lights: bool,
        actor_catalog: ActorCatalog,
    ) -> App {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalogue.db");
        write_empty_catalogue(&path);
        let config = EngineConfig {
            lights,
            ..default()
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            // A reference with a model path asks the asset server for its converted scene.
            .init_asset::<WorldAsset>()
            // A composed-body actor asks for its idle clip.
            .init_asset::<bevy::gltf::Gltf>()
            .insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
            .insert_resource(AssetCatalog::open(&path).unwrap())
            .insert_resource(actor_catalog)
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .insert_resource(QueuedReferences(references))
            .init_resource::<SpawnedCellRoot>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, spawn_queued_references);
        app.update();
        app
    }

    /// The documented actor catalog DDL (db-schema.md §18), as an in-memory fixture.
    fn actor_catalog_connection() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE actor_models (
                    id INTEGER PRIMARY KEY,
                    race_id INTEGER NOT NULL,
                    sex TEXT NOT NULL,
                    skin_armo_id INTEGER,
                    head_model TEXT,
                    head_source TEXT,
                    resolution TEXT NOT NULL
                 );
                 CREATE TABLE actor_model_parts (
                    npc_id INTEGER NOT NULL,
                    slot TEXT NOT NULL,
                    model_path TEXT NOT NULL,
                    PRIMARY KEY (npc_id, slot)
                 );",
            )
            .unwrap();
        connection
    }

    /// Alvor's catalog row and three authored parts, as the converter publishes them.
    fn insert_alvor(connection: &rusqlite::Connection) {
        connection
            .execute(
                "INSERT INTO actor_models(id, race_id, sex, skin_armo_id, head_model, head_source, resolution)
                 VALUES (0x13475, 79686, 'male', NULL,
                         'meshes/actors/character/facegendata/facegeom/skyrim.esm/00013475.nif',
                         'facegen', 'body_head')",
                [],
            )
            .unwrap();
        for (slot, path) in [
            (
                "body",
                r"Meshes\Actors\Character\Character Assets\MaleBody_1.NIF",
            ),
            (
                "hands",
                r"Meshes\Actors\Character\Character Assets\MaleHands_1.nif",
            ),
            (
                "feet",
                r"Meshes\Actors\Character\Character Assets\MaleFeet_1.nif",
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO actor_model_parts(npc_id, slot, model_path) VALUES (0x13475, ?1, ?2)",
                    rusqlite::params![slot, path],
                )
                .unwrap();
        }
    }

    // ---- Dynamic clutter from authored physics data (#104 phase a) ----

    fn clutter_body(kind: BodyKind, shapes: Vec<u32>, convex: bool) -> CollisionBody {
        CollisionBody {
            node: 1,
            target: "Clutter01".to_owned(),
            shapes,
            kind,
            havok: shared::collision::HavokBodyInfo {
                motion_system: 3,
                quality_type: 4,
                deactivator_type: 1,
                collision_layer: 4,
            },
            mass: 2.0,
            inertia: [30.0, 0.0, 0.0, 0.0, 40.0, 0.0, 0.0, 0.0, 50.0],
            center_of_mass: [0.0, 1.0, 0.0],
            linear_damping: 0.1,
            angular_damping: 0.05,
            friction: 0.6,
            restitution: 0.3,
            max_linear_velocity: 7_000.0,
            max_angular_velocity: 31.0,
            convex,
        }
    }

    fn clutter_box_asset(version: u32, kind: BodyKind) -> CollisionAsset {
        CollisionAsset {
            version,
            authored: true,
            shapes: vec![CollisionShape::Box {
                center: [0.0, 10.0, 0.0],
                half_extents: [10.0, 10.0, 10.0],
            }],
            skipped: Vec::new(),
            bodies: if version >= 2 {
                vec![clutter_body(kind, vec![0], true)]
            } else {
                Vec::new()
            },
        }
    }

    fn clutter_spec(asset: &CollisionAsset) -> ClutterSpec {
        match clutter_decision_from_asset(asset) {
            StaticCollisionDecision::Clutter(spec) => *spec,
            _ => panic!("expected a dynamic clutter body"),
        }
    }

    /// Readies one reference of `record_type` whose model carries `asset` in its scene extras.
    fn ready_clutter_reference_with(
        app: &mut App,
        record_type: &str,
        path: &str,
        static_physics: bool,
        asset: &CollisionAsset,
    ) -> (Entity, StreamingMetrics) {
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(2.0, 2.0, 2.0));
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let mut scene = World::new();
        let scene_root = scene.spawn(Transform::default()).id();
        scene.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            ChildOf(scene_root),
        ));
        let handle = add_converted_model(app, scene);
        let bounds = ExpectedModelBounds::new(Vec3::splat(-1.0), Vec3::splat(1.0)).unwrap();
        let reference = spawn_model_reference(app, handle, Some(bounds));
        {
            let mut entity_mut = app.world_mut().entity_mut(reference);
            let mut pending = entity_mut.get_mut::<PendingAssetProfile>().unwrap();
            pending.path = path.to_owned();
            pending.base_record_type = Some(record_type.to_owned());
            pending.static_physics = static_physics;
        }
        // The extras node carries a transform like every node of a converted scene: the
        // readiness scan validates that every node below the reference has one, and would
        // otherwise reject the whole model before the collision decision.
        app.world_mut().spawn((
            GltfSceneExtras {
                value: serde_json::json!({ "mudcrabCollision": asset }).to_string(),
            },
            Transform::default(),
            ChildOf(reference),
        ));
        let metrics = settle_readiness(app);
        (reference, metrics)
    }

    fn ready_clutter_reference(
        record_type: &str,
        static_physics: bool,
        asset: &CollisionAsset,
    ) -> (App, Entity, StreamingMetrics) {
        let mut app = model_app();
        let (reference, metrics) = ready_clutter_reference_with(
            &mut app,
            record_type,
            "meshes/clutter/test/clutter01.glb",
            static_physics,
            asset,
        );
        (app, reference, metrics)
    }

    /// The collider a body owns: the body entity itself for a single centred shape, otherwise its
    /// collider children (the shape's authored centre becomes a child translation).
    fn body_collider(app: &mut App, body: Entity) -> Option<Entity> {
        let world = app.world_mut();
        let mut query = world.query_filtered::<(Entity, Option<&ChildOf>), With<Collider>>();
        query
            .iter(world)
            .find(|(entity, parent)| {
                *entity == body || parent.is_some_and(|parent| parent.parent() == body)
            })
            .map(|(entity, _)| entity)
    }

    #[test]
    fn only_dynamic_bodies_on_movable_records_become_dynamic() {
        let dynamic = clutter_box_asset(2, BodyKind::Dynamic);
        let (mut app, misc, metrics) = ready_clutter_reference("MISC", true, &dynamic);
        assert_eq!(
            app.world().get::<RigidBody>(misc),
            Some(&RigidBody::Dynamic)
        );
        assert!(
            body_collider(&mut app, misc).is_some(),
            "the dynamic body has a collider"
        );
        assert!(
            app.world()
                .get::<crate::physics::DynamicClutter>(misc)
                .is_some()
        );
        assert!(app.world().get::<StaticColliderSource>(misc).is_none());
        assert_eq!(metrics.dynamic_clutter_bodies, 1);
        assert_eq!(metrics.dynamic_clutter_convexified, 0);
        assert_eq!(metrics.dynamic_clutter_skipped, 0);
        assert_eq!(metrics.dynamic_clutter_mass_fallbacks, 0);
        // The stable id is the FormId the reference already carries.
        assert_eq!(app.world().get::<FormId>(misc), Some(&FormId(0x00F9907)));

        // A fixed body on a movable record gets no body and no collider (as before).
        let fixed = clutter_box_asset(2, BodyKind::Fixed);
        let (app, misc, metrics) = ready_clutter_reference("MISC", true, &fixed);
        assert!(app.world().get::<RigidBody>(misc).is_none());
        assert!(app.world().get::<Collider>(misc).is_none());
        assert_eq!(metrics.dynamic_clutter_bodies, 0);

        // A dynamic body on a fixed record type stays fixed, with today's authored collider.
        let (app, stat, metrics) = ready_clutter_reference("STAT", true, &dynamic);
        assert_eq!(app.world().get::<RigidBody>(stat), Some(&RigidBody::Fixed));
        assert_eq!(
            app.world().get::<StaticColliderSource>(stat),
            Some(&StaticColliderSource::NifAuthored)
        );
        assert!(
            app.world()
                .get::<crate::physics::DynamicClutter>(stat)
                .is_none()
        );
        assert_eq!(metrics.dynamic_clutter_bodies, 0);

        // Containers (barrels, chests) and the other deferred record types stay fixed for now.
        for record in ["CONT", "MSTT", "ACTI", "FLOR", "DOOR"] {
            let (app, entity, metrics) = ready_clutter_reference(record, true, &dynamic);
            assert!(
                app.world().get::<RigidBody>(entity).is_none(),
                "{record} became a body"
            );
            assert_eq!(metrics.dynamic_clutter_bodies, 0, "{record}");
        }
    }

    fn parse_collision_extras(value: &serde_json::Value) -> CollisionAsset {
        let mut value = value.clone();
        drop_malformed_bodies(&mut value);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_malformed_body_is_dropped_without_losing_the_models_shapes() {
        let mut value = serde_json::to_value(clutter_box_asset(2, BodyKind::Dynamic)).unwrap();
        // A non-finite mass is written as null, and an unknown kind is not a BodyKind.
        let mut bad_mass = value["bodies"][0].clone();
        bad_mass["mass"] = serde_json::Value::Null;
        let mut bad_kind = value["bodies"][0].clone();
        bad_kind["kind"] = serde_json::json!("sideways");
        value["bodies"] = serde_json::json!([bad_mass, bad_kind, value["bodies"][0].clone()]);
        let asset = parse_collision_extras(&value);
        assert_eq!(asset.shapes.len(), 1);
        assert_eq!(asset.bodies.len(), 1);
        assert_eq!(asset.skipped.len(), 2);
        assert!(asset.skipped[0].contains("rigid body 0"));
        value["bodies"] = serde_json::json!([bad_mass]);
        let asset = parse_collision_extras(&value);
        assert_eq!(asset.shapes.len(), 1);
        assert!(asset.bodies.is_empty());
    }

    #[test]
    fn extra_dynamic_bodies_are_counted_not_silently_dropped() {
        let mut asset = clutter_box_asset(2, BodyKind::Dynamic);
        asset
            .bodies
            .push(clutter_body(BodyKind::Dynamic, vec![0], true));
        asset
            .bodies
            .push(clutter_body(BodyKind::Fixed, vec![0], true));
        assert_eq!(clutter_spec(&asset).ignored_bodies, 1);
        let single = clutter_box_asset(2, BodyKind::Dynamic);
        assert_eq!(clutter_spec(&single).ignored_bodies, 0);
    }

    #[test]
    fn version_one_asset_gives_no_dynamic_bodies() {
        let legacy = clutter_box_asset(1, BodyKind::Dynamic);
        let (app, misc, metrics) = ready_clutter_reference("MISC", true, &legacy);
        assert!(app.world().get::<RigidBody>(misc).is_none());
        assert_eq!(metrics.dynamic_clutter_bodies, 0);
        assert_eq!(metrics.dynamic_clutter_skipped, 0);
    }

    /// What today's converter writes for a model with no authored dynamic body: version 2 with an
    /// empty `bodies` list. It must behave like a version 1 asset.
    #[test]
    fn version_two_asset_with_no_bodies_gives_no_dynamic_bodies() {
        let mut empty = clutter_box_asset(2, BodyKind::Dynamic);
        empty.bodies.clear();
        assert!(matches!(
            clutter_decision_from_asset(&empty),
            StaticCollisionDecision::ClutterNone
        ));
        let (app, misc, metrics) = ready_clutter_reference("MISC", true, &empty);
        assert!(app.world().get::<RigidBody>(misc).is_none());
        assert_eq!(metrics.dynamic_clutter_bodies, 0);
        assert_eq!(metrics.dynamic_clutter_skipped, 0);
    }

    #[test]
    fn non_convex_dynamic_body_becomes_one_counted_hull() {
        let asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![CollisionShape::Mesh {
                vertices: vec![
                    [0.0, 0.0, 0.0],
                    [20.0, 0.0, 0.0],
                    [0.0, 20.0, 0.0],
                    [0.0, 0.0, 20.0],
                    [5.0, 5.0, 5.0],
                ],
                triangles: vec![[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]],
            }],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0], false)],
        };
        let (app, misc, metrics) = ready_clutter_reference("MISC", true, &asset);
        assert_eq!(metrics.dynamic_clutter_bodies, 1);
        assert_eq!(metrics.dynamic_clutter_convexified, 1);
        let collider = app.world().get::<Collider>(misc).expect("hull collider");
        assert!(collider.as_convex_polyhedron().is_some());
        assert_eq!(
            app.world().get::<RigidBody>(misc),
            Some(&RigidBody::Dynamic)
        );
    }

    #[test]
    fn dynamic_body_without_a_hull_is_skipped_never_fixed() {
        // Collinear points have no 3D hull.
        let asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![CollisionShape::Hull {
                points: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]],
            }],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0], false)],
        };
        let (app, misc, metrics) = ready_clutter_reference("MISC", true, &asset);
        assert_eq!(metrics.dynamic_clutter_skipped, 1);
        assert_eq!(metrics.dynamic_clutter_bodies, 0);
        assert!(app.world().get::<RigidBody>(misc).is_none());
        assert!(app.world().get::<Collider>(misc).is_none());
    }

    /// A pyramid mesh: five points with a 3D hull, and a body listing it is always convexified.
    fn pyramid_mesh() -> CollisionShape {
        CollisionShape::Mesh {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [20.0, 0.0, 0.0],
                [0.0, 20.0, 0.0],
                [0.0, 0.0, 20.0],
                [5.0, 5.0, 5.0],
            ],
            triangles: vec![[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]],
        }
    }

    /// The point limit is injectable, so these tests reach it without millions of points.
    fn clutter_decision_within(asset: &CollisionAsset, budget: usize) -> StaticCollisionDecision {
        clutter_decision_from_asset_within(asset, budget)
    }

    fn skip_reason(decision: StaticCollisionDecision) -> String {
        match decision {
            StaticCollisionDecision::ClutterSkipped(reason) => reason,
            _ => panic!("expected a skipped dynamic body"),
        }
    }

    #[test]
    fn repeated_primitive_indices_create_one_collider_and_density_contribution() {
        let mut asset = clutter_box_asset(2, BodyKind::Dynamic);
        asset.bodies[0].shapes = vec![0; 32];
        asset.bodies[0].inertia = [0.0; 9];
        assert_eq!(clutter_spec(&asset).parts.len(), 1);

        let (mut app, reference, metrics) = ready_clutter_reference("MISC", true, &asset);
        assert_eq!(metrics.dynamic_clutter_bodies, 1);
        assert_eq!(metrics.dynamic_clutter_mass_fallbacks, 1);
        let world = app.world_mut();
        let mut colliders = world.query::<(Entity, &ColliderMassProperties, Option<&ChildOf>)>();
        let owned: Vec<_> = colliders
            .iter(world)
            .filter(|(entity, _, parent)| {
                *entity == reference || parent.is_some_and(|parent| parent.parent() == reference)
            })
            .collect();
        assert_eq!(owned.len(), 1);
        assert!(
            matches!(owned[0].1, ColliderMassProperties::Density(density) if *density == 0.001)
        );
    }

    /// A corrupt body may list one in-range index thousands of times. The reviewer's fixture
    /// packs 4_096 occurrences of a 4_096-point hull into a 72 KB file; resolve it only once.
    #[test]
    fn a_body_repeating_one_shape_index_builds_one_budgeted_hull() {
        let asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![CollisionShape::Hull {
                points: (0..4_096)
                    .map(|index| {
                        let index = index as f32;
                        [index, index % 64.0, index % 17.0]
                    })
                    .collect(),
            }],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0; 4_096], false)],
        };
        assert_eq!(shape_hull_point_count(&asset.shapes[0]), 4_096);
        // The distinct shape still exceeds this smaller budget, before any hull is built.
        let reason = skip_reason(clutter_decision_within(&asset, 1_000));
        assert!(reason.contains("too many hull points"), "{reason}");
        // Repeats would exceed the shipped limit, but the single 4_096-point shape fits exactly.
        let spec = match clutter_decision_within(&asset, 4_096) {
            StaticCollisionDecision::Clutter(spec) => *spec,
            _ => panic!("the distinct geometry fits the budget"),
        };
        assert!(spec.convexified);
        assert_eq!(spec.parts.len(), 1);
    }

    /// Each shape alone is well under the budget; what is counted is the total of all of them.
    #[test]
    fn distinct_shapes_over_the_budget_together_are_rejected() {
        let asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![
                CollisionShape::Box {
                    center: [0.0, 0.0, 0.0],
                    half_extents: [1.0, 2.0, 3.0],
                },
                CollisionShape::Capsule {
                    a: [0.0, 0.0, 0.0],
                    b: [4.0, 0.0, 0.0],
                    radius: 1.0,
                },
                pyramid_mesh(),
            ],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0, 1, 2], false)],
        };
        assert_eq!(shape_hull_point_count(&asset.shapes[0]), 8);
        assert_eq!(shape_hull_point_count(&asset.shapes[1]), 52);
        assert_eq!(shape_hull_point_count(&asset.shapes[2]), 5);
        // 8 + 52 + 5 = 65, so 64 is one point short and 65 fits exactly.
        let reason = skip_reason(clutter_decision_within(&asset, 64));
        assert!(reason.contains("too many hull points"), "{reason}");
        assert!(matches!(
            clutter_decision_within(&asset, 65),
            StaticCollisionDecision::Clutter(_)
        ));
    }

    /// Repeated convex shape indices also resolve to one collider and one point-budget charge.
    #[test]
    fn a_convex_body_repeating_one_hull_index_builds_one_collider() {
        let asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![CollisionShape::Hull {
                points: (0..64)
                    .map(|index| {
                        let index = index as f32;
                        [index, index % 8.0, index % 5.0]
                    })
                    .collect(),
            }],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0; 4_096], true)],
        };
        let reason = skip_reason(clutter_decision_within(&asset, 63));
        assert!(reason.contains("too many hull points"), "{reason}");
        let spec = match clutter_decision_within(&asset, 64) {
            StaticCollisionDecision::Clutter(spec) => *spec,
            _ => panic!("the single convex shape fits the budget"),
        };
        assert!(!spec.convexified);
        assert_eq!(spec.parts.len(), 1);
    }

    #[test]
    fn distinct_primitive_parts_keep_first_seen_order_and_have_a_separate_cap() {
        let mut asset = clutter_box_asset(2, BodyKind::Dynamic);
        asset.shapes.push(CollisionShape::Box {
            center: [40.0, 10.0, 0.0],
            half_extents: [10.0; 3],
        });
        asset.bodies[0].shapes = vec![1, 0, 1, 0];
        let spec = clutter_spec(&asset);
        let translations: Vec<_> = spec.parts.iter().map(|part| part.translation).collect();
        assert_eq!(
            translations,
            [Vec3::new(40.0, 10.0, 0.0), Vec3::new(0.0, 10.0, 0.0)]
        );

        asset.shapes = vec![asset.shapes[0].clone(); MAX_CLUTTER_COLLIDER_PARTS + 1];
        asset.bodies[0].shapes = (0..MAX_CLUTTER_COLLIDER_PARTS as u32).collect();
        assert_eq!(clutter_spec(&asset).parts.len(), MAX_CLUTTER_COLLIDER_PARTS);
        asset.bodies[0]
            .shapes
            .push(MAX_CLUTTER_COLLIDER_PARTS as u32);
        let reason = skip_reason(clutter_decision_from_asset(&asset));
        assert!(reason.contains("too many collider parts"), "{reason}");
    }

    /// A body inside the budget is unaffected: it still gets its one convex hull.
    #[test]
    fn a_body_just_under_the_budget_still_produces_a_hull() {
        let mut asset = CollisionAsset {
            version: 2,
            authored: true,
            shapes: vec![pyramid_mesh()],
            skipped: Vec::new(),
            bodies: vec![clutter_body(BodyKind::Dynamic, vec![0], false)],
        };
        // One occurrence costs five points: comfortably inside a budget of ten.
        assert!(matches!(
            clutter_decision_within(&asset, 10),
            StaticCollisionDecision::Clutter(_)
        ));
        // Listed twice it still costs five, which fits exactly.
        asset.bodies[0].shapes = vec![0, 0];
        let spec = match clutter_decision_within(&asset, 5) {
            StaticCollisionDecision::Clutter(spec) => *spec,
            _ => panic!("a body inside the budget must keep its hull"),
        };
        assert!(spec.convexified);
        assert_eq!(spec.parts.len(), 1);
        assert!(spec.parts[0].collider.as_convex_polyhedron().is_some());
        let reason = skip_reason(clutter_decision_within(&asset, 4));
        assert!(reason.contains("too many hull points"), "{reason}");
    }

    /// The count a budget is checked against is the number of points the copy appends, for every
    /// shape kind.
    #[test]
    fn the_counted_hull_points_match_the_appended_points() {
        let shapes = [
            pyramid_mesh(),
            CollisionShape::Hull {
                points: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            },
            CollisionShape::Box {
                center: [1.0, 2.0, 3.0],
                half_extents: [1.0, 2.0, 3.0],
            },
            CollisionShape::Capsule {
                a: [0.0, 0.0, 0.0],
                b: [4.0, 0.0, 0.0],
                radius: 1.0,
            },
        ];
        // 5 mesh vertices, 3 hull points, 8 box corners, 26 points at each of two capsule ends.
        let expected = [5, 3, 8, 52];
        for (shape, expected) in shapes.iter().zip(expected) {
            let mut points = Vec::new();
            shape_hull_points(shape, &mut points);
            assert_eq!(points.len(), expected);
            assert_eq!(shape_hull_point_count(shape), points.len());
        }
        // A repeat costs its points again, the same way the copy would.
        let repeated = [&shapes[0], &shapes[0], &shapes[1]];
        assert_eq!(clutter_hull_point_total(&repeated, 13), Some(13));
        assert_eq!(clutter_hull_point_total(&repeated, 12), None);
    }

    #[test]
    fn unusable_inertia_falls_back_to_density_and_is_counted() {
        let mut asset = clutter_box_asset(2, BodyKind::Dynamic);
        asset.bodies[0].inertia = [0.0; 9];
        let (mut app, misc, metrics) = ready_clutter_reference("MISC", true, &asset);
        assert_eq!(metrics.dynamic_clutter_bodies, 1);
        assert_eq!(metrics.dynamic_clutter_mass_fallbacks, 1);
        assert!(app.world().get::<AdditionalMassProperties>(misc).is_none());
        let collider = body_collider(&mut app, misc).expect("the body's collider");
        assert!(matches!(
            app.world().get::<ColliderMassProperties>(collider),
            Some(ColliderMassProperties::Density(_))
        ));
    }

    #[test]
    fn the_live_cap_refuses_bodies_and_counts_the_overflow() {
        let dynamic = clutter_box_asset(2, BodyKind::Dynamic);
        let mut app = model_app();
        for _ in 0..crate::physics::MAX_LIVE_DYNAMIC_CLUTTER {
            app.world_mut().spawn(crate::physics::DynamicClutter {
                max_linear_velocity: 0.0,
                max_angular_velocity: 0.0,
            });
        }
        let (reference, metrics) = ready_clutter_reference_with(
            &mut app,
            "MISC",
            "meshes/clutter/test/clutter01.glb",
            true,
            &dynamic,
        );
        assert_eq!(metrics.dynamic_clutter_bodies, 0);
        assert_eq!(metrics.dynamic_clutter_overflow, 1);
        assert!(app.world().get::<RigidBody>(reference).is_none());
        assert!(app.world().get::<Collider>(reference).is_none());
    }

    #[test]
    fn non_interactive_runs_spawn_no_dynamic_bodies() {
        // `static_physics` is `EngineConfig::interactive_world_physics()` at spawn time.
        let dynamic = clutter_box_asset(2, BodyKind::Dynamic);
        let (app, misc, metrics) = ready_clutter_reference("MISC", false, &dynamic);
        assert!(app.world().get::<RigidBody>(misc).is_none());
        assert_eq!(metrics.dynamic_clutter_bodies, 0);
        for config in [
            EngineConfig {
                benchmark_only: true,
                ..default()
            },
            EngineConfig {
                acceptance_screenshot: Some(default()),
                ..default()
            },
            EngineConfig {
                headless: true,
                ..default()
            },
        ] {
            assert!(!config.interactive_world_physics());
        }
    }

    #[test]
    fn rebase_keeps_a_moving_clutter_body_pose_and_velocity() {
        let mut app = crate::physics::headless::fixture_app();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<ActiveSpace>()
            .add_systems(Update, update_render_origin);
        let cell = app
            .world_mut()
            .spawn((
                ExteriorCellGrid(IVec2::ZERO),
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        let spec = clutter_spec(&clutter_box_asset(2, BodyKind::Dynamic));
        let entity = app
            .world_mut()
            .run_system_once(move |mut commands: Commands| {
                let entity = commands
                    .spawn((
                        Transform::from_translation(Vec3::new(-300.0, 20_000.0, 400.0)),
                        ChildOf(cell),
                    ))
                    .id();
                crate::physics::spawn_dynamic_clutter(
                    &mut commands,
                    entity,
                    &spec.body,
                    spec.parts
                        .iter()
                        .map(|part| (part.translation, &part.collider)),
                    spec.mass,
                    1.0,
                );
                commands.entity(entity).insert(Velocity {
                    linear: Vec3::new(40.0, 0.0, 0.0),
                    angular: Vec3::ZERO,
                });
                entity
            })
            .unwrap();
        for _ in 0..5 {
            app.update();
        }
        let body = |world: &mut World| {
            world
                .run_system_once(move |context: ReadRapierContext| {
                    let context = context.single().expect("rapier context");
                    let handle = context.entity2body()[&entity];
                    let body = context.rigidbody_set.bodies.get(handle).expect("body");
                    (body.translation(), body.linvel())
                })
                .unwrap()
        };
        let (before_pose, before_velocity) = body(app.world_mut());
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<StreamingCamera>>();
            query.single(app.world()).unwrap()
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation
            .x += CELL_SIZE;
        app.update();
        let (after_pose, after_velocity) = body(app.world_mut());
        // One 60 Hz step of motion (40 u/s sideways, a gravity-sized fall) separates the samples.
        assert!(
            (after_pose.x - (before_pose.x - CELL_SIZE)).abs() < 2.0,
            "x {before_pose:?} -> {after_pose:?}"
        );
        assert!((after_pose.y - before_pose.y).abs() < 40.0);
        assert!((after_velocity.x - before_velocity.x).abs() < 1.0);
        // Moved once, not twice: the entity's world pose follows the body.
        let global = app.world().get::<GlobalTransform>(entity).unwrap();
        assert!(
            (global.translation().x - after_pose.x).abs() < 2.0,
            "entity {} body {}",
            global.translation().x,
            after_pose.x
        );

        // The body despawns with its cell.
        app.world_mut().entity_mut(cell).despawn();
        app.update();
        let retained = app
            .world_mut()
            .run_system_once(move |context: ReadRapierContext| {
                context
                    .single()
                    .expect("rapier context")
                    .entity2body()
                    .contains_key(&entity)
            })
            .unwrap();
        assert!(!retained, "unloaded cell retained its clutter body");
    }

    /// Every reference the cell spawned, by the form id it was placed with.
    fn reference_entity(app: &App, form_id: u32) -> Entity {
        let root = app
            .world()
            .resource::<SpawnedCellRoot>()
            .0
            .expect("the cell spawned");
        app.world()
            .entity(root)
            .get::<Children>()
            .expect("the cell root has the references as children")
            .iter()
            .find(|child| {
                app.world()
                    .entity(*child)
                    .get::<FormId>()
                    .is_some_and(|id| id.0 == form_id)
            })
            .expect("the reference spawned")
    }

    #[test]
    fn disabled_references_spawn_no_roots_models_lights_or_collider_work() {
        let mut hidden = lit_reference(1, Some(light_row(256.0, [255; 3], 0)), None);
        hidden.initially_enabled = false;
        hidden.model_path = Some("architecture/hidden-wall.nif".to_owned());
        hidden.base_record_type = Some("STAT".to_owned());
        let visible = lit_reference(2, Some(light_row(256.0, [255; 3], 0)), None);
        let mut app = spawn_reference_cell_app(vec![hidden, visible], true);
        let mut ids = app.world_mut().query::<&FormId>();
        assert_eq!(
            ids.iter(app.world()).map(|id| id.0).collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            app.world_mut()
                .query::<&PointLight>()
                .iter(app.world())
                .count(),
            1
        );
        assert_eq!(
            app.world_mut()
                .query::<&PendingModel>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world_mut()
                .query::<&PendingAssetProfile>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world_mut()
                .query::<&Collider>()
                .iter(app.world())
                .count(),
            0
        );
        let directory = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            profile_output_dir: Some(directory.path().to_owned()),
            ..default()
        };
        let profiler = app.world().resource::<ProfilingState>();
        profiler
            .write_bundle(
                &config,
                &serde_json::json!({}),
                None,
                &default(),
                None,
                None,
            )
            .unwrap();
        let profile: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("cpu-spans.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(profile["counters"]["streaming/references_spawned"], 1);
        assert_eq!(profile["counters"]["streaming/references_suppressed"], 1);
    }

    #[test]
    fn a_cell_with_only_disabled_references_still_loads() {
        let mut hidden = lit_reference(1, None, None);
        hidden.initially_enabled = false;
        let mut app = spawn_reference_cell_app(vec![hidden], false);
        let root = app.world().resource::<SpawnedCellRoot>().0.unwrap();
        assert_eq!(
            app.world()
                .entity(root)
                .get::<Children>()
                .map_or(0, |children| children.len()),
            0
        );
        assert_eq!(
            app.world_mut().query::<&FormId>().iter(app.world()).count(),
            0
        );
    }

    /// A spawned reference with a resolvable `door_links` row carries `LoadDoor`; one whose link
    /// cannot be resolved, and a plain reference, do not.
    #[test]
    fn a_spawned_door_reference_carries_load_door() {
        let link = |cell| crate::world::database::DoorLinkRow {
            destination_ref_id: 0x700,
            destination_cell_id: cell,
            destination_worldspace_id: None,
            arrival_position: [10.0, 20.0, 30.0],
            arrival_rotation: [0.0, 0.0, 1.0],
        };
        let mut door = lit_reference(0x110, None, None);
        door.door = Some(link(Some(500)));
        let mut unresolved = lit_reference(0x111, None, None);
        unresolved.door = Some(link(None));
        let app = spawn_reference_cell_app(
            vec![door, unresolved, lit_reference(0x112, None, None)],
            false,
        );

        let loaded = app
            .world()
            .entity(reference_entity(&app, 0x110))
            .get::<crate::doors::LoadDoor>()
            .cloned()
            .expect("the door reference carries LoadDoor");
        assert_eq!(loaded.ref_id, 0x110);
        assert_eq!(loaded.destination.destination_ref_id, 0x700);
        assert_eq!(loaded.destination.interior_cell_id, Some(500));
        assert_eq!(loaded.destination.arrival_position, [10.0, 20.0, 30.0]);
        for form_id in [0x111, 0x112] {
            assert!(
                app.world()
                    .entity(reference_entity(&app, form_id))
                    .get::<crate::doors::LoadDoor>()
                    .is_none()
            );
        }
    }

    /// Two references to the same model, one whose base record carries `Is Marker`: only the
    /// other loads a model, though both are still spawned as references.
    #[test]
    fn spawn_cell_loads_no_model_for_a_marker_base_record() {
        let mut marker = lit_reference(0x120, None, None);
        marker.model_path = Some("Architecture/Wall.nif".to_owned());
        marker.base_is_marker = true;
        let mut drawn = lit_reference(0x121, None, None);
        drawn.model_path = Some("Furniture/BlacksmithForgeMarker.nif".to_owned());
        let app = spawn_reference_cell_app(vec![marker, drawn], false);

        let marker = app.world().entity(reference_entity(&app, 0x120));
        assert!(marker.get::<MeshHandle>().is_none());
        assert!(marker.get::<PendingModel>().is_none());
        let drawn = app.world().entity(reference_entity(&app, 0x121));
        assert_eq!(
            drawn.get::<MeshHandle>().map(|handle| handle.0.as_str()),
            Some("meshes/furniture/blacksmithforgemarker.glb")
        );
        assert!(drawn.get::<PendingModel>().is_some());
    }

    /// A referenced `NPC_` with a catalog row becomes one actor entity at the reference's authored
    /// transform. Its visual root is the composed body, and its hands/feet are components rather
    /// than scenes.
    #[test]
    fn a_referenced_npc_spawns_one_actor_at_its_authored_transform() {
        let directory = tempfile::tempdir().unwrap();
        let composed = directory
            .path()
            .join("meshes/actors/character/composed/default/male/body.glb");
        std::fs::create_dir_all(composed.parent().unwrap()).unwrap();
        std::fs::write(&composed, b"glTF").unwrap();
        let connection = actor_catalog_connection();
        insert_alvor(&connection);
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());

        let mut reference = lit_reference(0x13475, None, None);
        reference.base_form_id = 0x13475;
        reference.base_record_type = Some("NPC_".to_owned());
        reference.model_path = Some(r"Actors\Character\Character Assets\MaleBody_1.NIF".to_owned());
        reference.position = [128.0, 64.0, -256.0];
        reference.rotation = [0.0, 1.5, 0.0];
        let mut app =
            spawn_reference_cell_app_with_catalog(vec![reference.clone()], false, catalog);

        let actors: Vec<Entity> = {
            let mut query = app.world_mut().query_filtered::<Entity, With<Actor>>();
            query.iter(app.world()).collect()
        };
        assert_eq!(
            actors.len(),
            1,
            "one NPC_ reference becomes exactly one actor"
        );
        let entity = app.world().entity(actors[0]);
        let actor = entity.get::<Actor>().expect("the actor component");
        assert_eq!(actor.base_form_id, 0x13475);
        assert_eq!(actor.race_id, 79686);
        assert_eq!(actor.sex, crate::actors::ActorSex::Male);
        assert_eq!(actor.resolution, crate::actors::ActorResolution::BodyHead);

        let transform = *entity.get::<Transform>().expect("the reference transform");
        assert_eq!(
            transform.translation,
            creation_to_bevy(Vec3::from_array(reference.position))
        );
        assert_eq!(
            transform.rotation,
            creation_rotation_to_bevy(reference.rotation)
        );
        assert_eq!(transform.scale, Vec3::ONE);

        let parts = entity.get::<ActorParts>().expect("the catalog parts");
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts
                .get(ActorSlot::Body)
                .map(|part| part.model_path.as_str()),
            Some(r"Meshes\Actors\Character\Character Assets\MaleBody_1.NIF")
        );
        assert!(parts.get(ActorSlot::Hands).is_some());
        assert!(parts.get(ActorSlot::Feet).is_some());

        let head = entity.get::<ActorHead>().expect("the catalog head");
        assert_eq!(head.source, ActorHeadSource::Facegen);
        assert_eq!(
            head.model_path,
            "meshes/actors/character/facegendata/facegeom/skyrim.esm/00013475.nif"
        );

        assert_eq!(
            entity.get::<MeshHandle>().map(|handle| handle.0.as_str()),
            Some("meshes/actors/character/composed/default/male/body.glb"),
            "the visual root is the composed body, not a part scene"
        );
        assert!(entity.get::<PendingModel>().is_some());
        assert_eq!(entity.get::<FormId>(), Some(&FormId(0x13475)));
    }

    /// Before a composed body is published, the same spawn draws the catalog's authored body part.
    #[test]
    fn an_actor_without_a_composed_body_draws_its_catalog_body_part() {
        let directory = tempfile::tempdir().unwrap();
        let connection = actor_catalog_connection();
        insert_alvor(&connection);
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());
        let mut reference = lit_reference(0x13475, None, None);
        reference.base_form_id = 0x13475;
        let app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        let entity = app.world().entity(reference_entity(&app, 0x13475));
        assert_eq!(
            entity.get::<MeshHandle>().map(|handle| handle.0.as_str()),
            Some("meshes/actors/character/character assets/malebody_1.glb")
        );
        assert!(entity.get::<Actor>().is_some());
        assert!(
            entity
                .get::<crate::actor_skin_tone::ActorSkinTone>()
                .is_none(),
            "a catalog without the skin tone columns gives no tone"
        );
    }

    /// The catalog's `QNAM` skin tone travels to the actor entity, where
    /// `crate::actor_skin_tone` applies it to the body's `skin_tint` surfaces.
    #[test]
    fn an_actor_carries_its_catalog_skin_tone() {
        let directory = tempfile::tempdir().unwrap();
        let connection = actor_catalog_connection();
        connection
            .execute_batch(
                "ALTER TABLE actor_models ADD COLUMN skin_tone_r REAL;
                 ALTER TABLE actor_models ADD COLUMN skin_tone_g REAL;
                 ALTER TABLE actor_models ADD COLUMN skin_tone_b REAL;",
            )
            .unwrap();
        insert_alvor(&connection);
        connection
            .execute(
                "UPDATE actor_models SET skin_tone_r=0.75, skin_tone_g=0.5, skin_tone_b=0.25",
                [],
            )
            .unwrap();
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());
        let mut reference = lit_reference(0x13475, None, None);
        reference.base_form_id = 0x13475;
        let app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        let entity = app.world().entity(reference_entity(&app, 0x13475));
        assert_eq!(
            entity.get::<crate::actor_skin_tone::ActorSkinTone>(),
            Some(&crate::actor_skin_tone::ActorSkinTone(Vec3::new(
                0.75, 0.5, 0.25
            )))
        );
    }

    /// The catalog's `NAM7` body weight becomes the morph value `1 - NAM7 / 100`.
    #[test]
    fn an_actor_carries_its_catalog_body_weight() {
        let directory = tempfile::tempdir().unwrap();
        let connection = actor_catalog_connection();
        connection
            .execute("ALTER TABLE actor_models ADD COLUMN weight REAL", [])
            .unwrap();
        insert_alvor(&connection);
        connection
            .execute("UPDATE actor_models SET weight=20.0", [])
            .unwrap();
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());
        let mut reference = lit_reference(0x13475, None, None);
        reference.base_form_id = 0x13475;
        let app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        let entity = app.world().entity(reference_entity(&app, 0x13475));
        let weight = entity
            .get::<crate::actor_body_weight::ActorBodyWeight>()
            .expect("the body weight component");
        assert!((weight.0 - 0.8).abs() < 1e-6, "{weight:?}");
    }

    /// A race with its own body keeps that body and receives no shared human gait, even when the
    /// default composed body exists.
    #[test]
    fn a_custom_race_actor_keeps_its_body_and_gets_no_shared_gait() {
        let directory = tempfile::tempdir().unwrap();
        let composed = directory
            .path()
            .join("meshes/actors/character/composed/default/male/body.glb");
        std::fs::create_dir_all(composed.parent().unwrap()).unwrap();
        std::fs::write(&composed, b"glTF").unwrap();
        let connection = actor_catalog_connection();
        connection
            .execute(
                "INSERT INTO actor_models(id, race_id, sex, resolution)
                 VALUES (0x2000, 0x1000, 'male', 'body')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO actor_model_parts(npc_id, slot, model_path)
                 VALUES (0x2000, 'body',
                         'Meshes\\Actors\\CustomRace\\Character Assets\\CustomBody.NIF')",
                [],
            )
            .unwrap();
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());

        let mut reference = lit_reference(0x2000, None, None);
        reference.base_form_id = 0x2000;
        reference.base_record_type = Some("NPC_".to_owned());
        let app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        let entity = app.world().entity(reference_entity(&app, 0x2000));
        assert_eq!(
            entity.get::<MeshHandle>().map(|handle| handle.0.as_str()),
            Some("meshes/actors/customrace/character assets/custombody.glb"),
            "the custom race draws its own body"
        );
        assert!(
            entity.get::<crate::actor_animation::ActorClips>().is_none(),
            "the shared gait clips only fit the shared humanoid body"
        );
        assert!(entity.get::<crate::actor_drive::ActorDrive>().is_none());
    }

    /// An unresolved catalog row is skipped entirely: no actor, no placeholder and no static mesh,
    /// with one typed diagnostic naming the base FormID.
    #[test]
    fn an_unresolved_actor_is_skipped_with_a_typed_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let connection = actor_catalog_connection();
        connection
            .execute(
                "INSERT INTO actor_models(id, race_id, sex, resolution)
                 VALUES (0x13475, 79686, 'male', 'unresolved')",
                [],
            )
            .unwrap();
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());
        let model = catalog.get(0x13475).expect("the unresolved catalog row");
        assert_eq!(
            catalog.spawn_plan(model).unwrap_err(),
            ActorSpawnDiagnostic {
                base_form_id: 0x13475,
                reason: ActorSpawnSkip::Unresolved,
            }
        );

        let mut reference = lit_reference(0x13475, None, None);
        reference.base_form_id = 0x13475;
        reference.model_path = Some("Architecture/Wall.nif".to_owned());
        let mut app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        assert_eq!(
            app.world_mut().query::<&FormId>().iter(app.world()).count(),
            0,
            "the skipped actor spawns no entity at all"
        );
        assert_eq!(
            app.world_mut()
                .query::<&MeshHandle>()
                .iter(app.world())
                .count(),
            0,
            "the skipped actor never falls back to its static model path"
        );
        assert_eq!(
            app.world_mut().query::<&Actor>().iter(app.world()).count(),
            0
        );
    }

    /// A reference whose base has no catalog row keeps the exact spawn path it had before actors.
    #[test]
    fn a_non_actor_reference_spawns_its_model_exactly_as_before() {
        let directory = tempfile::tempdir().unwrap();
        let connection = actor_catalog_connection();
        insert_alvor(&connection);
        let catalog = ActorCatalog::from_connection_with_assets_root(&connection, directory.path());
        let mut reference = lit_reference(0x200, None, None);
        reference.model_path = Some("Architecture/Wall.nif".to_owned());
        let app = spawn_reference_cell_app_with_catalog(vec![reference], false, catalog);

        let entity = app.world().entity(reference_entity(&app, 0x200));
        assert!(entity.get::<Actor>().is_none());
        assert!(entity.get::<ActorParts>().is_none());
        assert_eq!(
            entity.get::<MeshHandle>().map(|handle| handle.0.as_str()),
            Some("meshes/architecture/wall.glb")
        );
        assert!(entity.get::<PendingModel>().is_some());
    }

    /// One lit reference, one negative light, one flagged off by default and one plain reference:
    /// exactly one `PointLight` comes out, carrying the reference's own radius and colour.
    #[test]
    fn spawns_one_point_light_for_a_lit_reference() {
        let mut app = spawn_reference_cell_app(
            vec![
                lit_reference(
                    0x100,
                    Some(light_row(256.0, [255, 200, 120], 0)),
                    Some(850.8),
                ),
                lit_reference(0x101, Some(light_row(512.0, [80, 80, 90], 0x4)), None),
                lit_reference(0x102, Some(light_row(512.0, [80, 80, 90], 0x20)), None),
                lit_reference(0x103, None, None),
            ],
            true,
        );

        let lights: Vec<(Entity, PointLight, crate::lights::SkyrimLight)> = {
            let mut query = app
                .world_mut()
                .query::<(Entity, &PointLight, &crate::lights::SkyrimLight)>();
            query
                .iter(app.world())
                .map(|(entity, light, marker)| (entity, *light, *marker))
                .collect()
        };
        assert_eq!(
            lights.len(),
            1,
            "the negative and off-by-default lights spawn none"
        );
        let (light_entity, light, marker) = lights[0];
        assert_eq!(marker.form_id, 0x100);
        assert_eq!(marker.cell_id, 99);
        assert_eq!(
            light.range, 850.8,
            "the reference's XRDS radius wins over the record's 256"
        );
        assert_eq!(light.color, Color::srgb_u8(255, 200, 120));
        assert!(
            (light.intensity - crate::lights::intensity_for_radius(850.8)).abs() < 1.0e3,
            "{}",
            light.intensity
        );
        assert!(!light.shadow_maps_enabled);
        // The light opts out of its reference's propagated placed-object layer; see
        // `render::a_light_below_a_reference_keeps_the_light_layers`.
        assert_eq!(
            app.world().entity(light_entity).get::<RenderLayers>(),
            Some(&RenderLayers::from_layers(crate::render::LIGHT_LAYERS))
        );
        assert!(
            app.world()
                .entity(light_entity)
                .contains::<bevy::app::PropagateOver<RenderLayers>>()
        );

        // The light has to be inside the cell root: that hierarchy is what a render-origin rebase
        // moves and what a cell unload despawns.
        let root = app
            .world()
            .resource::<SpawnedCellRoot>()
            .0
            .expect("the cell spawned");
        assert!(app.world().entity(root).get::<StreamedCellRoot>().is_some());
        let reference = app
            .world()
            .entity(light_entity)
            .get::<ChildOf>()
            .expect("the light is a child of its reference")
            .parent();
        assert_eq!(
            app.world().entity(reference).get::<FormId>(),
            Some(&FormId(0x100))
        );
        assert!(
            app.world().entity(reference).get::<Visibility>().is_some(),
            "the reference of a light needs `Visibility`, or the light child can never be visible"
        );
        assert_eq!(
            app.world()
                .entity(reference)
                .get::<ChildOf>()
                .expect("the reference is a child of the cell root")
                .parent(),
            root
        );
        // The cell's other references are spawned exactly as before, and the unlit one gets no
        // child at all.
        assert!(
            app.world()
                .entity(reference_entity(&app, 0x103))
                .get::<Children>()
                .is_none(),
            "a reference with no light row spawns no light child"
        );
    }

    /// The flag is the whole switch: the same cell places no light at all without `--lights`, and
    /// the references themselves are spawned exactly as before.
    #[test]
    fn places_no_light_without_the_lights_flag() {
        let mut app = spawn_reference_cell_app(
            vec![lit_reference(
                0x100,
                Some(light_row(256.0, [255, 200, 120], 0)),
                Some(850.8),
            )],
            false,
        );

        let mut query = app
            .world_mut()
            .query_filtered::<Entity, (With<PointLight>, With<crate::lights::SkyrimLight>)>();
        assert_eq!(
            query.iter(app.world()).count(),
            0,
            "a lit reference places nothing while the flag is off"
        );
        let root = app
            .world()
            .resource::<SpawnedCellRoot>()
            .0
            .expect("the cell spawned");
        assert!(app.world().entity(root).get::<StreamedCellRoot>().is_some());
        assert_eq!(
            app.world()
                .entity(reference_entity(&app, 0x100))
                .get::<FormId>(),
            Some(&FormId(0x100)),
            "and the reference is still spawned"
        );
    }
}

#[cfg(test)]
mod collision_placement_probe;
