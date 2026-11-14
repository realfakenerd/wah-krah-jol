//! Resource identities for generated full-detail terrain and water surfaces.

use super::quadrant_layers;
use crate::{
    render::{TerrainMaterial, WaterMaterial},
    streaming_preparation::{PreparationDemand, PreparationKey},
    world::{
        cache::TerrainSnapshot,
        components::{TerrainPatch, WaterSurface},
        database::AssetCatalog,
        water::FlatWater,
    },
};
use bevy::{
    asset::{AssetId, VisitAssetDependencies},
    prelude::*,
};
use shared::streaming_costs::canonical_resource_key;
use std::{any::TypeId, collections::HashSet};

/// Follow the same layer selection and water-height conditions as `spawn_cell`.
/// The caller performs strict terrain validation before committing these loads.
pub(crate) fn surface_resource_paths(
    terrain: Option<&TerrainSnapshot>,
    water: bool,
    catalog: &AssetCatalog,
) -> Vec<String> {
    let Some(terrain) = terrain else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for quadrant in 0..4 {
        let Ok(layers) = quadrant_layers(terrain, quadrant) else {
            continue;
        };
        for layer in layers {
            // Zero is an unresolved slot for base and overlays alike: `TerrainExtension::from_quadrant`
            // binds no image there, so the catalog's row zero must not be reserved.
            if layer.texture_form_id == 0 {
                continue;
            }
            paths.extend(catalog.landscape_diffuse(layer.texture_form_id));
            paths.extend(catalog.landscape_normal(layer.texture_form_id));
        }
    }
    if water
        && let Some(surface) = FlatWater::from_terrain(terrain)
        && let Some(water_type) = surface.water_type_form_id
    {
        paths.extend(catalog.water_flow(water_type));
    }
    let mut paths: Vec<_> = paths
        .into_iter()
        // Preserve an unrecognized path for the runtime's unknown-cost fallback.
        // Dropping it would hide a load that spawn_cell still performs.
        .map(|path| canonical_resource_key(path).unwrap_or_else(|_| path.to_owned()))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

/// Observe only generated surface descendants. Placed models below the cell
/// have their own scene reservations and must not be counted again here.
pub(crate) fn collect_surface_demand(world: &mut World, root: Entity) -> PreparationDemand {
    let mut demand = PreparationDemand {
        key: PreparationKey::Cell(root),
        meshes: Vec::new(),
        images: Vec::new(),
        materials: Vec::new(),
    };
    let mut stack = vec![(root, false)];
    let mut visited = HashSet::new();
    while let Some((entity, parent_is_surface)) = stack.pop() {
        if !visited.insert(entity) {
            continue;
        }
        let Ok(entity) = world.get_entity(entity) else {
            continue;
        };
        let is_surface = parent_is_surface
            || entity.contains::<TerrainPatch>()
            || entity.contains::<WaterSurface>();
        if is_surface {
            if let Some(mesh) = entity.get::<Mesh3d>() {
                demand.meshes.push(mesh.0.id());
            }
            if let Some(material) = entity.get::<MeshMaterial3d<TerrainMaterial>>() {
                demand.materials.push(material.0.id().into());
                if let Some(material) = world
                    .get_resource::<Assets<TerrainMaterial>>()
                    .and_then(|materials| materials.get(&material.0))
                {
                    append_images(&material.base, &mut demand.images);
                    demand.images.extend(material.extension.image_ids());
                }
            }
            if let Some(material) = entity.get::<MeshMaterial3d<WaterMaterial>>() {
                demand.materials.push(material.0.id().into());
                if let Some(material) = world
                    .get_resource::<Assets<WaterMaterial>>()
                    .and_then(|materials| materials.get(&material.0))
                {
                    append_images(&material.base, &mut demand.images);
                    // Excludes the renderer's shared reflection target.
                    demand.images.extend(material.extension.image_ids());
                }
            }
            // Before CPU readiness these profiles also retain the exact IDs.
            // The material accessors remain authoritative after the profiles go.
            if let Some(pending) = entity.get::<super::PendingTerrainProfile>() {
                demand.images.extend(pending.images.iter().map(Handle::id));
                demand.images.extend(pending.normals.iter().map(Handle::id));
            }
            if let Some(pending) = entity.get::<super::PendingWaterProfile>() {
                demand
                    .images
                    .extend(pending.flow_normal.iter().map(Handle::id));
            }
        }
        if let Some(children) = entity.get::<Children>() {
            stack.extend(children.iter().map(|child| (child, is_surface)));
        }
    }
    demand.meshes.sort_unstable();
    demand.meshes.dedup();
    demand.images.sort_unstable();
    demand.images.dedup();
    demand.materials.sort_unstable();
    demand.materials.dedup();
    demand
}

fn append_images(material: &StandardMaterial, images: &mut Vec<AssetId<Image>>) {
    material.visit_dependencies(&mut |dependency| {
        if dependency.type_id() == TypeId::of::<Image>() {
            images.push(dependency.typed_debug_checked());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        render::{TerrainExtension, WaterExtension},
        world::cache::TerrainLayerSnapshot,
    };
    use rusqlite::Connection;

    fn catalog() -> (tempfile::TempDir, AssetCatalog) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);\
             CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT,normal_path TEXT);\
             CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);\
             INSERT INTO texture_sets VALUES(1,'Textures\\Shared.dds','Textures\\Shared_n.dds');\
             INSERT INTO landscape_textures VALUES(10,1);\
             INSERT INTO waters VALUES(20,'Textures\\Water_n.dds');",
            )
            .unwrap();
        (directory, AssetCatalog::open(&path).unwrap())
    }

    fn terrain() -> TerrainSnapshot {
        TerrainSnapshot {
            cell_id: 1,
            width: 33,
            height: 33,
            heights: vec![0.0; 33 * 33],
            normals: vec![0; 33 * 33 * 3],
            vertex_colors: vec![],
            layers: (0..4)
                .map(|quadrant| TerrainLayerSnapshot {
                    texture_form_id: 10,
                    quadrant,
                    layer: 0,
                    is_base: true,
                    weights: vec![],
                })
                .collect(),
            water_height: Some(0.0),
            water_type_form_id: Some(20),
        }
    }

    #[test]
    fn surface_paths_include_shared_normal_and_water_dependencies_once() {
        let (_directory, catalog) = catalog();
        let terrain = terrain();
        assert_eq!(
            surface_resource_paths(Some(&terrain), true, &catalog),
            vec![
                "textures/shared.ktx2",
                "textures/shared_n.ktx2",
                "textures/water_n.ktx2",
            ]
        );
        assert_eq!(
            surface_resource_paths(Some(&terrain), false, &catalog).len(),
            2
        );
        assert!(surface_resource_paths(None, true, &catalog).is_empty());
    }

    #[test]
    fn zero_overlay_reserves_nothing_while_a_valid_overlay_keeps_its_textures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);\
             CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT,normal_path TEXT);\
             CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);\
             INSERT INTO texture_sets VALUES(1,'Textures\\Zero_must_not_load.dds',NULL);\
             INSERT INTO texture_sets VALUES(2,'Textures\\Shared.dds','Textures\\Shared_n.dds');\
             INSERT INTO landscape_textures VALUES(0,1);\
             INSERT INTO landscape_textures VALUES(10,2);",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        // Row zero is a real catalog entry, so skipping it is the only thing that keeps it out.
        assert!(catalog.landscape_diffuse(0).is_some());
        let mut terrain = terrain();
        for quadrant in 0..4 {
            terrain.layers.push(TerrainLayerSnapshot {
                texture_form_id: 0,
                quadrant,
                layer: 1,
                is_base: false,
                weights: vec![(0, 0.5)],
            });
            terrain.layers.push(TerrainLayerSnapshot {
                texture_form_id: 10,
                quadrant,
                layer: 2,
                is_base: false,
                weights: vec![(18, 0.75)],
            });
        }
        assert_eq!(
            surface_resource_paths(Some(&terrain), false, &catalog),
            vec!["textures/shared.ktx2", "textures/shared_n.ktx2"]
        );
    }

    #[test]
    fn invalid_or_absent_water_height_has_no_flow_texture_reservation() {
        let (_directory, catalog) = catalog();
        for water_height in [None, Some(f32::NAN), Some(1.0e8)] {
            let mut terrain = terrain();
            terrain.water_height = water_height;
            assert_eq!(
                surface_resource_paths(Some(&terrain), true, &catalog).len(),
                2
            );
        }
    }

    #[test]
    fn surface_watch_ignores_placed_models_and_persistent_reflection_texture() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<TerrainMaterial>>();
        world.init_resource::<Assets<WaterMaterial>>();
        let surface_mesh = world.resource_mut::<Assets<Mesh>>().add(Cuboid::default());
        let placed_mesh = world.resource_mut::<Assets<Mesh>>().add(Cuboid::default());
        let diffuse = world.resource_mut::<Assets<Image>>().add(Image::default());
        let normal = world.resource_mut::<Assets<Image>>().add(Image::default());
        let reflection = world.resource_mut::<Assets<Image>>().add(Image::default());
        let terrain_material =
            world
                .resource_mut::<Assets<TerrainMaterial>>()
                .add(TerrainMaterial {
                    base: StandardMaterial::default(),
                    extension: TerrainExtension::fixture(
                        &terrain(),
                        0,
                        std::array::from_fn(|_| diffuse.clone()),
                    )
                    .unwrap(),
                });
        let water_material = world
            .resource_mut::<Assets<WaterMaterial>>()
            .add(WaterMaterial {
                base: StandardMaterial::default(),
                extension: WaterExtension::with_reflection(
                    reflection.clone(),
                    Some(normal.clone()),
                ),
            });
        let root = world.spawn_empty().id();
        world.spawn((
            TerrainPatch,
            Mesh3d(surface_mesh.clone()),
            MeshMaterial3d(terrain_material.clone()),
            ChildOf(root),
        ));
        world.spawn((
            WaterSurface,
            Mesh3d(surface_mesh.clone()),
            MeshMaterial3d(water_material.clone()),
            ChildOf(root),
        ));
        world.spawn((Mesh3d(placed_mesh.clone()), ChildOf(root)));
        let demand = collect_surface_demand(&mut world, root);
        assert_eq!(demand.meshes, vec![surface_mesh.id()]);
        assert_eq!(demand.images.len(), 2);
        assert!(demand.images.contains(&diffuse.id()));
        assert!(demand.images.contains(&normal.id()));
        assert!(!demand.images.contains(&reflection.id()));
        assert!(demand.materials.contains(&terrain_material.id().into()));
        assert!(demand.materials.contains(&water_material.id().into()));
    }
}
