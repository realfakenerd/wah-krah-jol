//! Tests of the merged terrain path (#215): the merged cell mesh must reproduce today's four
//! quadrant meshes, weld like them, and hand the same buffers to the collider; a cell the merged
//! data cannot serve falls back, counted.

use super::*;
use crate::terrain_merge::{
    MergeState,
    classes::tests::{mesh_with_slots, vanilla_table},
    mesh::{
        TERRAIN_SLOT_IDS, TERRAIN_SLOT_WEIGHTS, build_merged_mesh, cell_to_grid, expand_slots,
        write_grid,
    },
};
use converter::esm::{
    cell_cache::write_cell_cache, records::RawRecord, terrain_mesh::build_cell_mesh,
};
use shared::{
    LAND_SIDE,
    terrain_mesh::{
        CELL_MESH_VERTICES, CellEdge, QUADRANT_SIDE, QUADRANT_VERTICES,
        TERRAIN_MESH_FORMAT_VERSION, TerrainChunkFile, edge_vertices, grid_position,
        terrain_chunk_path, vertex_index,
    },
};

const CELL: u32 = 0x0000_1234;

/// An ATXT layer to put in a synthetic LAND: (quadrant, layer index, LTEX, VTXT entries of
/// (vertex, opacity)).
type Atxt = (u8, u16, u32, Vec<(u16, f32)>);

fn layer_bytes(ltex: u32, quadrant: u8, layer: u16) -> Vec<u8> {
    let mut data = ltex.to_le_bytes().to_vec();
    data.extend([quadrant, 0]);
    data.extend(layer.to_le_bytes());
    data
}

/// A LAND record whose height at grid (x, y) is `8 * grid(x, y)` (VHGT stores whole multiples of
/// 8), with normals and colours that differ at every sample.
fn land(
    cell_id: u32,
    grid: impl Fn(usize, usize) -> i32,
    bases: [u32; 4],
    atxt: &[Atxt],
    with_colors: bool,
) -> RawRecord {
    let side = usize::from(LAND_SIDE);
    let mut vhgt = 0.0f32.to_le_bytes().to_vec();
    for y in 0..side {
        for x in 0..side {
            let previous = if x > 0 {
                grid(x - 1, y)
            } else if y > 0 {
                grid(0, y - 1)
            } else {
                0
            };
            let delta = grid(x, y) - previous;
            assert!(
                (-128..=127).contains(&delta),
                "delta {delta} does not fit a VHGT byte"
            );
            vhgt.push(delta as i8 as u8);
        }
    }
    vhgt.extend([0; 3]);
    let mut vnml = Vec::new();
    let mut vclr = Vec::new();
    for y in 0..side {
        for x in 0..side {
            vnml.extend([
                (x as i32 - 16) as i8 as u8,
                (y as i32 - 16) as i8 as u8,
                100,
            ]);
            vclr.extend([(7 * x) as u8, (7 * y) as u8, (x + y) as u8]);
        }
    }
    let mut subrecords = vec![(b"VHGT".to_vec(), vhgt), (b"VNML".to_vec(), vnml)];
    if with_colors {
        subrecords.push((b"VCLR".to_vec(), vclr));
    }
    for (quadrant, &ltex) in bases.iter().enumerate() {
        subrecords.push((b"BTXT".to_vec(), layer_bytes(ltex, quadrant as u8, 0)));
    }
    for (quadrant, layer, ltex, entries) in atxt {
        subrecords.push((b"ATXT".to_vec(), layer_bytes(*ltex, *quadrant, *layer)));
        let mut vtxt = Vec::new();
        for (vertex, opacity) in entries {
            vtxt.extend(vertex.to_le_bytes());
            vtxt.extend([0, 0]);
            vtxt.extend(opacity.to_le_bytes());
        }
        subrecords.push((b"VTXT".to_vec(), vtxt));
    }
    RawRecord {
        form_id: cell_id + 0x1000,
        record_type: *b"LAND",
        flags: 0,
        subrecords,
        cell_form_id: Some(cell_id),
        worldspace_form_id: None,
        load_order: 0,
    }
}

/// A rolling surface that never steps more than a few units between samples.
fn rolling(x: usize, y: usize) -> i32 {
    ((3 * x + 5 * y) % 17) as i32 + 20
}

fn known_ltex() -> std::collections::HashSet<u32> {
    (0..9)
        .map(|id| if id == 0 { 0 } else { 0xFF + id })
        .collect()
}

/// Today's snapshot of `record` as the converter's cell cache and the engine's cache read it.
fn cached_snapshot(record: &RawRecord) -> TerrainSnapshot {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cell_cache.rkyv");
    let records = HashMap::from([(record.form_id, record.clone())]);
    write_cell_cache(&records, &path).unwrap();
    CellCache::open(&path)
        .unwrap()
        .terrain(record.cell_form_id.unwrap())
        .expect("the cache holds the cell")
}

fn merged_cell(record: &RawRecord) -> TerrainCellMesh {
    build_cell_mesh(record.cell_form_id.unwrap(), record, &known_ltex(), [0; 32])
        .unwrap()
        .0
}

fn positions(mesh: &Mesh) -> &[[f32; 3]] {
    match mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap() {
        VertexAttributeValues::Float32x3(values) => values,
        other => panic!("positions are {other:?}"),
    }
}

fn float2(mesh: &Mesh, attribute: bevy::mesh::MeshVertexAttribute) -> &[[f32; 2]] {
    match mesh.attribute(attribute).unwrap() {
        VertexAttributeValues::Float32x2(values) => values,
        other => panic!("{other:?}"),
    }
}

fn float3(mesh: &Mesh, attribute: bevy::mesh::MeshVertexAttribute) -> &[[f32; 3]] {
    match mesh.attribute(attribute).unwrap() {
        VertexAttributeValues::Float32x3(values) => values,
        other => panic!("{other:?}"),
    }
}

fn float4(mesh: &Mesh, attribute: bevy::mesh::MeshVertexAttribute) -> &[[f32; 4]] {
    match mesh.attribute(attribute).unwrap() {
        VertexAttributeValues::Float32x4(values) => values,
        other => panic!("{other:?}"),
    }
}

fn u32_indices(mesh: &Mesh) -> &[u32] {
    match mesh.indices().unwrap() {
        Indices::U32(indices) => indices,
        Indices::U16(_) => panic!("the merged mesh uses u32 indices"),
    }
}

fn slot_ids(mesh: &Mesh, group: usize) -> &[[u16; 4]] {
    match mesh.attribute(TERRAIN_SLOT_IDS[group].id).unwrap() {
        VertexAttributeValues::Uint16x4(values) => values,
        other => panic!("{other:?}"),
    }
}

fn slot_weights(mesh: &Mesh, group: usize) -> &[[u8; 4]] {
    match mesh.attribute(TERRAIN_SLOT_WEIGHTS[group].id).unwrap() {
        VertexAttributeValues::Unorm8x4(values) => values,
        other => panic!("{other:?}"),
    }
}

fn atxt_layers() -> Vec<Atxt> {
    vec![
        (
            0,
            1,
            0x103,
            vec![(0, 0.5), (1, 1.0), (40, 0.25), (288, 0.9)],
        ),
        (0, 2, 0x104, vec![(1, 0.2), (17, 0.7)]),
        (2, 1, 0x105, vec![(100, 0.6), (101, 0.4)]),
        (
            3,
            4,
            0x106,
            vec![(0, 1.0), (16, 0.33), (272, 0.5), (288, 0.01)],
        ),
        (3, 5, 0x107, vec![(16, 0.5)]),
        (3, 6, 0x101, vec![(16, 0.1), (17, 0.1)]),
    ]
}

// The vertex index walks six parallel buffers, so an index loop is the clearest form.
#[allow(clippy::needless_range_loop)]
#[test]
fn the_merged_mesh_reproduces_todays_four_quadrant_meshes_vertex_for_vertex() {
    let classes = TerrainClasses::from_table(vanilla_table(9)).unwrap();
    let record = land(
        CELL,
        rolling,
        [0x100, 0x101, 0x102, 0x103],
        &atxt_layers(),
        true,
    );
    let snapshot = cached_snapshot(&record);
    let cell = merged_cell(&record);
    let merged = build_merged_mesh(&cell, &classes.ids_by_form_id).unwrap();

    let merged_indices = u32_indices(&merged);
    assert_eq!(merged_indices.len(), 4 * 16 * 16 * 6);
    for quadrant in 0..4u8 {
        let today = build_terrain_quadrant_mesh(&snapshot, quadrant).unwrap();
        let offset = usize::from(quadrant) * QUADRANT_VERTICES;
        let layers = quadrant_layers(&snapshot, quadrant).unwrap();
        let overlay = quadrant_overlay_weights(&snapshot, quadrant).unwrap();
        for local in 0..QUADRANT_VERTICES {
            let vertex = offset + local;
            assert_eq!(
                positions(&merged)[vertex],
                positions(&today)[local],
                "position of quadrant {quadrant} vertex {local}"
            );
            assert_eq!(
                float3(&merged, Mesh::ATTRIBUTE_NORMAL)[vertex],
                float3(&today, Mesh::ATTRIBUTE_NORMAL)[local],
                "normal of quadrant {quadrant} vertex {local}"
            );
            assert_eq!(
                float2(&merged, Mesh::ATTRIBUTE_UV_0)[vertex],
                float2(&today, Mesh::ATTRIBUTE_UV_0)[local]
            );
            assert_eq!(
                float4(&merged, Mesh::ATTRIBUTE_COLOR)[vertex],
                float4(&today, Mesh::ATTRIBUTE_COLOR)[local]
            );
            // The texture slots: ids from the quadrant's layers, weights within 8-bit rounding.
            for slot in 0..8 {
                let id = slot_ids(&merged, slot / 4)[vertex][slot % 4];
                let weight = slot_weights(&merged, slot / 4)[vertex][slot % 4];
                match layers.get(slot) {
                    Some(layer) => {
                        assert_eq!(id, classes.ids_by_form_id[&layer.texture_form_id]);
                        let wanted = if slot == 0 {
                            0.0
                        } else {
                            overlay[slot - 1][local]
                        };
                        assert!(
                            (f32::from(weight) / 255.0 - wanted).abs() <= 1.0 / 510.0 + 1e-6,
                            "weight of quadrant {quadrant} slot {slot} vertex {local}: {weight} vs {wanted}"
                        );
                    }
                    None => assert_eq!((id, weight), (0, 0)),
                }
            }
        }
        let today_indices = u32_indices(&today);
        let range = usize::from(quadrant) * 16 * 16 * 6..(usize::from(quadrant) + 1) * 16 * 16 * 6;
        let local: Vec<u32> = merged_indices[range]
            .iter()
            .map(|index| index - offset as u32)
            .collect();
        assert_eq!(local, today_indices, "triangles of quadrant {quadrant}");
    }
    // Four 17 x 17 quadrants are the whole mesh, and nothing else is on it.
    assert_eq!(positions(&merged).len(), CELL_MESH_VERTICES);
    assert!(merged.attribute(Mesh::ATTRIBUTE_TANGENT).is_none());
    assert!(merged.attribute(Mesh::ATTRIBUTE_UV_1).is_none());
}

#[test]
fn a_cell_without_vertex_colours_is_white_like_today() {
    let classes = TerrainClasses::from_table(vanilla_table(9)).unwrap();
    let record = land(CELL, rolling, [0x100; 4], &[], false);
    let snapshot = cached_snapshot(&record);
    let merged = build_merged_mesh(&merged_cell(&record), &classes.ids_by_form_id).unwrap();
    let today = build_terrain_quadrant_mesh(&snapshot, 1).unwrap();
    assert_eq!(
        float4(&merged, Mesh::ATTRIBUTE_COLOR)[QUADRANT_VERTICES],
        float4(&today, Mesh::ATTRIBUTE_COLOR)[0]
    );
    assert_eq!(float4(&merged, Mesh::ATTRIBUTE_COLOR)[0], [1.0; 4]);
}

#[test]
fn slot_attributes_expand_to_exact_values_at_stride_8() {
    let table = vanilla_table(9);
    let ids = table.ids_by_form_id();
    let mut cell = mesh_with_slots(8);
    // Quadrant 2: base 0x100 (id 1), then 0x105 (id 6) and 0x103 (id 4).
    cell.quadrant_slots[2 * 8..2 * 8 + 3].copy_from_slice(&[0x100, 0x105, 0x103]);
    cell.quadrant_slot_counts[2] = 3;
    let vertex = vertex_index(2, 4, 5);
    cell.weights[vertex * 8 + 1] = 200;
    cell.weights[vertex * 8 + 2] = 55;
    let slots = expand_slots(&cell, &ids).unwrap();
    assert_eq!(slots.ids.len(), 2);
    assert_eq!(slots.weights.len(), 2);
    assert_eq!(slots.ids[0][vertex], [1, 6, 4, 0]);
    assert_eq!(slots.ids[1][vertex], [0, 0, 0, 0]);
    assert_eq!(slots.weights[0][vertex], [0, 200, 55, 0]);
    assert_eq!(slots.weights[1][vertex], [0; 4]);
    // The other quadrants keep the default texture (FormID 0, id 0) in slot 0.
    assert_eq!(slots.ids[0][0], [0, 0, 0, 0]);
    // A stored slot-0 weight never reaches the shader.
    cell.weights[vertex * 8] = 99;
    assert_eq!(expand_slots(&cell, &ids).unwrap().weights[0][vertex][0], 0);
}

#[test]
fn slot_attributes_expand_to_exact_values_at_stride_16() {
    let table = vanilla_table(9);
    let ids = table.ids_by_form_id();
    let mut cell = mesh_with_slots(16);
    cell.quadrant_slots[16..16 + 10].copy_from_slice(&[
        0x100, 0x101, 0x102, 0x103, 0x104, 0x105, 0x106, 0x101, 0x102, 0x103,
    ]);
    cell.quadrant_slot_counts[1] = 10;
    let vertex = vertex_index(1, 8, 8);
    cell.weights[vertex * 16 + 5] = 10;
    cell.weights[vertex * 16 + 9] = 250;
    let slots = expand_slots(&cell, &ids).unwrap();
    assert_eq!(slots.ids.len(), 4);
    assert_eq!(slots.ids[0][vertex], [1, 2, 3, 4]);
    assert_eq!(slots.ids[1][vertex], [5, 6, 7, 2]);
    assert_eq!(slots.ids[2][vertex], [3, 4, 0, 0]);
    assert_eq!(slots.ids[3][vertex], [0, 0, 0, 0]);
    assert_eq!(slots.weights[1][vertex], [0, 10, 0, 0]);
    assert_eq!(slots.weights[2][vertex], [0, 250, 0, 0]);
    assert_eq!(slots.weights[3][vertex], [0; 4]);
}

#[test]
fn the_mesh_carries_two_attribute_groups_at_stride_8_and_four_at_stride_16() {
    let ids = vanilla_table(9).ids_by_form_id();
    let narrow = build_merged_mesh(&mesh_with_slots(8), &ids).unwrap();
    let wide = build_merged_mesh(&mesh_with_slots(16), &ids).unwrap();
    for group in 0..4 {
        assert_eq!(
            narrow.attribute(TERRAIN_SLOT_IDS[group].id).is_some(),
            group < 2
        );
        assert_eq!(
            narrow.attribute(TERRAIN_SLOT_WEIGHTS[group].id).is_some(),
            group < 2
        );
        assert!(wide.attribute(TERRAIN_SLOT_IDS[group].id).is_some());
        assert!(wide.attribute(TERRAIN_SLOT_WEIGHTS[group].id).is_some());
    }
}

#[test]
fn an_unknown_form_id_cannot_be_expanded_and_fails_validation() {
    let table = vanilla_table(9);
    let ids = table.ids_by_form_id();
    let mut cell = mesh_with_slots(8);
    cell.quadrant_slots[0] = 0x0BAD;
    assert!(cell.validate(&ids).is_err());
    let message = expand_slots(&cell, &ids).unwrap_err();
    assert!(message.contains("00000BAD"), "{message}");
}

#[test]
fn seam_duplicates_that_disagree_are_not_one_height_field() {
    let record = land(CELL, rolling, [0x100; 4], &[], true);
    let mut cell = merged_cell(&record);
    cell_to_grid(&cell).unwrap();
    assert!(merged_geometry_check(&cell).is_ok());
    cell.heights[vertex_index(1, 0, 3)] += 1.0;
    assert!(
        cell_to_grid(&cell)
            .unwrap_err()
            .contains("seam vertex at grid (16, 3)")
    );
    assert!(merged_geometry_check(&cell).is_err());

    let mut flat = merged_cell(&record);
    flat.normals[0..3].copy_from_slice(&[0, 0, 0]);
    assert!(
        merged_geometry_check(&flat)
            .unwrap_err()
            .contains("zero-length")
    );
}

/// Two cells side by side: B's west edge differs from A's east edge by up to 24 units on every
/// fourth row, so the weld has points to move and points to leave.
fn neighbour_records() -> (RawRecord, RawRecord) {
    let a = land(0x10, rolling, [0x100; 4], &[], true);
    let b = land(
        0x11,
        |x, y| {
            if x == 0 {
                rolling(32, y) + if y % 4 == 0 { 3 } else { 0 }
            } else {
                rolling(x + 7, y + 2)
            }
        },
        [0x100; 4],
        &[],
        true,
    );
    (a, b)
}

fn key(grid_x: i32) -> CellKey {
    CellKey::Exterior {
        worldspace_id: 60,
        grid_x,
        grid_y: 0,
    }
}

#[test]
fn the_weld_on_merged_cells_matches_the_weld_on_todays_snapshots() {
    let (record_a, record_b) = neighbour_records();

    // Today: both snapshots through the quadrant path's weld.
    let mut continuity_today = TerrainContinuity::default();
    let mut metrics_today = StreamingMetrics::default();
    let mut snapshot_a = cached_snapshot(&record_a);
    let mut snapshot_b = cached_snapshot(&record_b);
    validate_and_register_terrain_edges(
        key(0),
        &mut snapshot_a,
        &mut continuity_today,
        &mut metrics_today,
    )
    .unwrap();
    validate_and_register_terrain_edges(
        key(1),
        &mut snapshot_b,
        &mut continuity_today,
        &mut metrics_today,
    )
    .unwrap();

    // Merged: the same two cells through the merged weld.
    let mut continuity_merged = TerrainContinuity::default();
    let mut metrics_merged = StreamingMetrics::default();
    let mut cell_a = merged_cell(&record_a);
    let mut cell_b = merged_cell(&record_b);
    let before_b = cell_b.clone();
    weld_merged_cell(
        key(0),
        &mut cell_a,
        &mut continuity_merged,
        &mut metrics_merged,
    )
    .unwrap();
    weld_merged_cell(
        key(1),
        &mut cell_b,
        &mut continuity_merged,
        &mut metrics_merged,
    )
    .unwrap();

    assert!(
        metrics_today.terrain_seam_points_welded > 0,
        "the fixture must make the weld move points"
    );
    assert_eq!(
        metrics_merged.terrain_seam_points_welded,
        metrics_today.terrain_seam_points_welded
    );
    assert_eq!(
        metrics_merged.terrain_seams_validated,
        metrics_today.terrain_seams_validated
    );
    assert_eq!(
        metrics_merged.terrain_edges_left_as_authored,
        metrics_today.terrain_edges_left_as_authored
    );
    // What the next cell would weld onto is the same too, so either path can be a neighbour.
    let registered_today = &continuity_today.edges[&key(1)];
    let registered_merged = &continuity_merged.edges[&key(1)];
    assert_eq!(registered_merged.west, registered_today.west);
    assert_eq!(registered_merged.east, registered_today.east);
    assert_eq!(registered_merged.south, registered_today.south);
    assert_eq!(registered_merged.north, registered_today.north);

    // Every vertex of the welded merged cell sits where today's welded snapshot puts it, and
    // carries the same recomputed normal.
    for quadrant in 0..4 {
        for y in 0..QUADRANT_SIDE {
            for x in 0..QUADRANT_SIDE {
                let (gx, gy) = grid_position(quadrant, x, y);
                let grid = gy * usize::from(LAND_SIDE) + gx;
                let vertex = vertex_index(quadrant, x, y);
                assert_eq!(cell_b.heights[vertex], snapshot_b.heights[grid]);
                assert_eq!(
                    cell_b.normals[vertex * 3..vertex * 3 + 3],
                    snapshot_b.normals[grid * 3..grid * 3 + 3]
                );
            }
        }
    }
    // Only the shared edge moved in height, and only vertices on it.
    let west: std::collections::HashSet<usize> =
        edge_vertices(CellEdge::West).into_iter().collect();
    let moved: Vec<usize> = (0..CELL_MESH_VERTICES)
        .filter(|&vertex| cell_b.heights[vertex] != before_b.heights[vertex])
        .collect();
    assert!(!moved.is_empty());
    assert!(
        moved.iter().all(|vertex| west.contains(vertex)),
        "{moved:?}"
    );
    // Seam duplicates still agree after the weld, so the cell is still one height field.
    cell_to_grid(&cell_b).unwrap();
    cell_to_grid(&cell_a).unwrap();
}

#[test]
fn a_merged_cell_welds_onto_a_quadrant_path_neighbour_and_the_reverse() {
    let (record_a, record_b) = neighbour_records();
    let mut continuity = TerrainContinuity::default();
    let mut metrics = StreamingMetrics::default();
    // A arrives through the quadrant path, B through the merged path.
    let mut snapshot_a = cached_snapshot(&record_a);
    validate_and_register_terrain_edges(key(0), &mut snapshot_a, &mut continuity, &mut metrics)
        .unwrap();
    let mut cell_b = merged_cell(&record_b);
    weld_merged_cell(key(1), &mut cell_b, &mut continuity, &mut metrics).unwrap();
    for y in 0..usize::from(LAND_SIDE) {
        let east_of_a = snapshot_a.heights[y * usize::from(LAND_SIDE) + 32];
        let (quadrant, local_y) = if y < 16 { (0, y) } else { (2, y - 16) };
        assert_eq!(
            cell_b.heights[vertex_index(quadrant, 0, local_y)],
            east_of_a,
            "row {y}"
        );
    }
    // Reverse: B (quadrant path) arrives first, A (merged) welds onto it.
    let mut continuity = TerrainContinuity::default();
    let mut snapshot_b = cached_snapshot(&record_b);
    validate_and_register_terrain_edges(key(1), &mut snapshot_b, &mut continuity, &mut metrics)
        .unwrap();
    let mut cell_a = merged_cell(&record_a);
    weld_merged_cell(key(0), &mut cell_a, &mut continuity, &mut metrics).unwrap();
    for y in 0..usize::from(LAND_SIDE) {
        let west_of_b = snapshot_b.heights[y * usize::from(LAND_SIDE)];
        let (quadrant, local_y) = if y < 16 { (1, y) } else { (3, y - 16) };
        assert_eq!(
            cell_a.heights[vertex_index(quadrant, 16, local_y)],
            west_of_b,
            "row {y}"
        );
    }
}

#[test]
fn the_grid_round_trip_is_lossless() {
    let record = land(CELL, rolling, [0x100; 4], &[], true);
    let cell = merged_cell(&record);
    let (heights, normals) = cell_to_grid(&cell).unwrap();
    assert_eq!(heights.len(), 33 * 33);
    assert_eq!(normals.len(), 33 * 33 * 3);
    let mut copy = cell.clone();
    write_grid(&mut copy, &heights, &normals);
    assert_eq!(copy, cell);
}

#[test]
fn one_collider_per_merged_cell_is_the_render_mesh() {
    let classes = TerrainClasses::from_table(vanilla_table(9)).unwrap();
    let record = land(
        CELL,
        rolling,
        [0x100, 0x101, 0x102, 0x103],
        &atxt_layers(),
        true,
    );
    let mesh = build_merged_mesh(&merged_cell(&record), &classes.ids_by_form_id).unwrap();
    let collider = terrain_collider_from_mesh(&mesh).unwrap();
    let trimesh = collider.as_trimesh().expect("terrain trimesh");
    assert_eq!(trimesh.vertices().len(), CELL_MESH_VERTICES);
    for (vertex, position) in trimesh.vertices().zip(positions(&mesh)) {
        assert_eq!(vertex, Vec3::from_array(*position));
    }
    let indices = u32_indices(&mesh);
    assert_eq!(trimesh.indices().len() * 3, indices.len());
    assert_eq!(trimesh.indices().len(), 2048);
    for (triangle, indices) in trimesh.indices().iter().zip(indices.as_chunks::<3>().0) {
        assert_eq!(*triangle, [indices[0], indices[1], indices[2]]);
    }
}

fn chunk_with(root: &std::path::Path, cell: TerrainCellMesh, grid_x: i32) {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&TerrainChunkFile {
        format_version: TERRAIN_MESH_FORMAT_VERSION,
        worldspace_id: 60,
        cells: vec![cell],
    })
    .unwrap();
    let path = root.join(terrain_chunk_path(60, grid_x, 0));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn ready_merge(root: &std::path::Path) -> TerrainMerge {
    let classes = Arc::new(TerrainClasses::from_table(vanilla_table(9)).unwrap());
    TerrainMerge::with_state(
        MergeState::Ready {
            classes,
            material: Handle::default(),
            images: Arc::from([]),
        },
        root.to_path_buf(),
    )
}

#[test]
fn every_terrain_cell_is_counted_as_merged_or_fallback_with_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    let (record_a, _) = neighbour_records();
    // Cell 0x10 at grid x 0 has good data; cell 0x11 at grid x 4 names a texture the table
    // lacks; grid x 8 has no chunk file at all.
    chunk_with(directory.path(), merged_cell(&record_a), 0);
    let mut bad = merged_cell(&land(0x11, rolling, [0x100; 4], &[], false));
    bad.quadrant_slots[0] = 0x0BAD;
    chunk_with(directory.path(), bad, 4);
    let mut merge = ready_merge(directory.path());
    let mut metrics = StreamingMetrics::default();
    let mut profiler = ProfilingState::default();

    let chosen = choose_merged_terrain(Some(&mut merge), key(0), 0x10, &mut metrics, &mut profiler);
    assert!(chosen.is_some());
    assert_eq!(metrics.terrain_cells_fallback, 0);

    assert!(
        choose_merged_terrain(Some(&mut merge), key(4), 0x11, &mut metrics, &mut profiler)
            .is_none()
    );
    assert_eq!(metrics.terrain_cells_fallback, 1);
    assert_eq!(metrics.terrain_cells_merged_invalid, 1);

    assert!(
        choose_merged_terrain(Some(&mut merge), key(8), 0x12, &mut metrics, &mut profiler)
            .is_none()
    );
    assert_eq!(metrics.terrain_cells_fallback, 2);
    assert_eq!(metrics.terrain_cells_fallback_expected_merged, 2);
    assert_eq!(metrics.terrain_cells_merged_invalid, 1);

    // A run without merged data (no table, or the plugin absent) falls back without calling it
    // a failure of the data.
    let mut absent = TerrainMerge::with_state(MergeState::Absent, directory.path().to_path_buf());
    assert!(
        choose_merged_terrain(Some(&mut absent), key(0), 0x10, &mut metrics, &mut profiler)
            .is_none()
    );
    assert!(choose_merged_terrain(None, key(0), 0x10, &mut metrics, &mut profiler).is_none());
    assert_eq!(metrics.terrain_cells_fallback, 4);
    assert_eq!(metrics.terrain_cells_fallback_expected_merged, 2);
    assert_eq!(
        metrics.terrain_cells_merged, 0,
        "merged cells are counted when they spawn"
    );
}

#[test]
fn a_cell_whose_seams_disagree_falls_back_as_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let mut cell = merged_cell(&land(0x10, rolling, [0x100; 4], &[], true));
    cell.heights[vertex_index(1, 0, 3)] += 1.0;
    chunk_with(directory.path(), cell, 0);
    let mut merge = ready_merge(directory.path());
    let mut metrics = StreamingMetrics::default();
    let mut profiler = ProfilingState::default();
    assert!(
        choose_merged_terrain(Some(&mut merge), key(0), 0x10, &mut metrics, &mut profiler)
            .is_none()
    );
    assert_eq!(metrics.terrain_cells_merged_invalid, 1);
}

#[allow(clippy::too_many_arguments)]
fn spawn_merged_cell(
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
    let record = land(
        CELL,
        rolling,
        [0x100, 0x101, 0x102, 0x103],
        &atxt_layers(),
        true,
    );
    let mut snapshot = cached_snapshot(&record);
    snapshot.water_height = Some(500.0);
    // Deliberately differ from the drawn merged height field: water coverage must use the mesh.
    snapshot.heights.fill(1000.0);
    let classes = Arc::new(TerrainClasses::from_table(vanilla_table(9)).unwrap());
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
        true,
        false,
        CellPayload {
            generation: 1,
            key: key(0),
            cell_id: CELL,
            references: Vec::new(),
        },
        Some(snapshot),
        Some(MergedSpawn {
            cell: Box::new(merged_cell(&record)),
            classes,
            material: Handle::default(),
            images: Arc::from([]),
        }),
        &mut 0,
        &mut profiler,
    )
    .expect("spawn cell");
}

#[test]
fn a_merged_cell_spawns_one_terrain_entity_with_one_collider() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<TerrainMaterial>()
        .init_asset::<WaterMaterial>()
        .init_resource::<AssetCatalog>()
        .init_resource::<ActorCatalog>()
        .init_resource::<StreamingMetrics>()
        .init_resource::<lod::LodStreaming>()
        .init_resource::<ProfilingState>()
        .insert_resource(WaterReflectionTexture(Handle::default()))
        .add_systems(
            Update,
            (spawn_merged_cell, track_merged_surface_readiness).chain(),
        );
    app.update();
    app.update();

    let world = app.world_mut();
    let patches = world
        .query_filtered::<Entity, With<TerrainPatch>>()
        .iter(world)
        .count();
    assert_eq!(patches, 1, "one terrain patch for the whole cell");
    let merged = world
        .query_filtered::<Entity, With<MergedTerrainCell>>()
        .iter(world)
        .count();
    assert_eq!(merged, 1);
    let quadrant_coverage = world
        .query_filtered::<Entity, With<TerrainCoverage>>()
        .iter(world)
        .count();
    assert_eq!(quadrant_coverage, 0, "no per-quadrant entities");
    let colliders = world
        .query_filtered::<Entity, With<TerrainCollider>>()
        .iter(world)
        .count();
    assert_eq!(colliders, 1, "one collider for the whole cell");
    let water = world
        .query_filtered::<Entity, With<WaterSurface>>()
        .iter(world)
        .count();
    assert_eq!(water, 1, "the cell's water still comes from its snapshot");
    let mut bodies = world.query::<&FlatWaterBody>();
    let sample = crate::world::water::water_at_point(
        bodies.iter(world),
        60,
        WorldPosition::from_creation_units(Vec3::new(64.0, 64.0, 0.0)),
    )
    .expect("the drawn merged LAND is below water, although the cached LAND is above it");
    assert_eq!(sample.cell_id, CELL);
    assert_eq!(sample.surface.height, 500.0);

    let mut ready = world.query_filtered::<(
        &MergedTerrainCoverage,
        &Visibility,
        Has<ColliderDisabled>,
        Has<PendingMergedTerrain>,
    ), (With<TerrainSurfaceReady>, With<TerrainPatch>)>();
    let (coverage, visibility, disabled, pending) = ready.single(world).unwrap();
    assert_eq!(coverage.grid, IVec2::new(0, 0));
    assert_eq!(*visibility, Visibility::Inherited);
    assert!(!disabled, "the collider is live once the surface is ready");
    assert!(!pending);
    assert_eq!(
        world
            .resource::<StreamingMetrics>()
            .terrain_patches_validated,
        1
    );
    assert!(world.resource::<lod::LodStreaming>().visibility_dirty);
}

#[test]
fn a_merged_cell_hides_its_lod_tiles_in_all_four_quadrants() {
    use shared::lod::LodTier;
    let mut app = App::new();
    app.insert_resource(EngineConfig::default())
        .insert_resource(RenderOrigin(IVec2::ZERO))
        .insert_resource(StreamingMetrics::default())
        .insert_resource(ProfilingState::default())
        .insert_resource(lod::LodStreaming::default())
        .add_systems(Update, lod::update_terrain_lod_visibility);
    let world = app.world_mut();
    world.spawn((
        StreamingCamera,
        Transform::from_xyz(CELL_SIZE * 0.5, 0.0, -CELL_SIZE * 0.5),
    ));
    let tiles: Vec<Entity> = (0..4u8)
        .map(|quadrant| {
            world
                .spawn((
                    TerrainCoverage {
                        grid: IVec2::ZERO,
                        quadrant,
                        tier: Some(LodTier::Tier4),
                    },
                    TerrainSurfaceReady,
                    Visibility::Hidden,
                ))
                .id()
        })
        .collect();
    app.update();
    for tile in &tiles {
        assert_eq!(
            *app.world().get::<Visibility>(*tile).unwrap(),
            Visibility::Inherited,
            "with no full-detail terrain the near LOD tile shows"
        );
    }
    // The merged mesh of the same cell becomes ready: it covers every quadrant at once.
    app.world_mut()
        .resource_mut::<lod::LodStreaming>()
        .visibility_dirty = true;
    let merged = app
        .world_mut()
        .spawn((
            TerrainPatch,
            MergedTerrainCoverage { grid: IVec2::ZERO },
            TerrainSurfaceReady,
            SwapWaiting,
        ))
        .id();
    app.update();
    for tile in &tiles {
        assert_eq!(
            *app.world().get::<Visibility>(*tile).unwrap(),
            Visibility::Inherited,
            "a merged cell still waiting for the render world keeps the LOD tiles drawn"
        );
    }
    // The swap bridge confirms the merged mesh drawn and removes the marker: the removal alone
    // triggers the refresh.
    app.world_mut().entity_mut(merged).remove::<SwapWaiting>();
    app.update();
    for tile in &tiles {
        assert_eq!(
            *app.world().get::<Visibility>(*tile).unwrap(),
            Visibility::Hidden,
            "one drawn merged cell hides the LOD tiles of all four quadrants"
        );
    }
    // Despawning it brings the tiles back: the removal alone triggers the refresh.
    app.world_mut().entity_mut(merged).despawn();
    app.update();
    for tile in &tiles {
        assert_eq!(
            *app.world().get::<Visibility>(*tile).unwrap(),
            Visibility::Inherited
        );
    }
}

fn spawn_for(cell: TerrainCellMesh) -> MergedSpawn {
    MergedSpawn {
        cell: Box::new(cell),
        classes: Arc::new(TerrainClasses::from_table(vanilla_table(9)).unwrap()),
        material: Handle::default(),
        images: Arc::from([]),
    }
}

/// A snapshot the quadrant path accepts: the cell's land with no texture layers, which the empty
/// test catalogue could not resolve.
fn plain_snapshot(record: &RawRecord) -> TerrainSnapshot {
    let mut snapshot = cached_snapshot(record);
    snapshot.layers.clear();
    snapshot
}

#[test]
fn a_merged_cell_whose_weld_fails_is_drawn_by_quadrant_and_counted() {
    let record = land(CELL, rolling, [0x100; 4], &[], true);
    let mut broken = merged_cell(&record);
    broken.heights[vertex_index(1, 0, 3)] += 1.0;
    let mut snapshot = plain_snapshot(&record);
    let mut continuity = TerrainContinuity::default();
    let mut metrics = StreamingMetrics::default();
    let mut profiler = ProfilingState::default();
    let settled = settle_cell_terrain(
        key(0),
        CELL,
        Some(spawn_for(broken)),
        &mut snapshot,
        &AssetCatalog::default(),
        &mut continuity,
        &mut metrics,
        &mut profiler,
    )
    .expect("the quadrant path accepts the cell");
    assert!(settled.is_none(), "the merged data is dropped");
    assert_eq!(metrics.terrain_cells_fallback, 1);
    assert_eq!(metrics.terrain_cells_fallback_expected_merged, 1);
    assert_eq!(metrics.terrain_cells_merged_invalid, 1);
    assert!(
        continuity.edges.contains_key(&key(0)),
        "the quadrant path registered the cell's edges for its neighbours"
    );
}

#[test]
fn a_merged_cell_that_welds_stays_merged_and_is_not_a_fallback() {
    let record = land(CELL, rolling, [0x100; 4], &[], true);
    let mut snapshot = plain_snapshot(&record);
    let mut continuity = TerrainContinuity::default();
    let mut metrics = StreamingMetrics::default();
    let mut profiler = ProfilingState::default();
    let settled = settle_cell_terrain(
        key(0),
        CELL,
        Some(spawn_for(merged_cell(&record))),
        &mut snapshot,
        &AssetCatalog::default(),
        &mut continuity,
        &mut metrics,
        &mut profiler,
    )
    .unwrap();
    assert!(settled.is_some());
    assert_eq!(metrics.terrain_cells_fallback, 0);
    assert!(continuity.edges.contains_key(&key(0)));
}

#[test]
fn a_cell_that_fails_both_paths_is_an_error_and_counted_once() {
    let record = land(CELL, rolling, [0x100; 4], &[], true);
    let mut broken = merged_cell(&record);
    broken.heights[vertex_index(1, 0, 3)] += 1.0;
    let mut snapshot = plain_snapshot(&record);
    snapshot.heights[0] = f32::NAN;
    let mut continuity = TerrainContinuity::default();
    let mut metrics = StreamingMetrics::default();
    let mut profiler = ProfilingState::default();
    let result = settle_cell_terrain(
        key(0),
        CELL,
        Some(spawn_for(broken)),
        &mut snapshot,
        &AssetCatalog::default(),
        &mut continuity,
        &mut metrics,
        &mut profiler,
    );
    assert!(
        result
            .err()
            .expect("both paths fail")
            .contains("non-finite")
    );
    assert_eq!(metrics.terrain_cells_fallback, 1);
}

fn payload_for(key: CellKey, cell_id: u32) -> Result<CellPayload, String> {
    Ok(CellPayload {
        generation: 1,
        key,
        cell_id,
        references: Vec::new(),
    })
}

#[test]
fn only_loaded_exterior_cells_with_terrain_are_held_while_the_setup_runs() {
    let classes = Arc::new(TerrainClasses::from_table(vanilla_table(9)).unwrap());
    let mut waiting = TerrainMerge::with_state(
        MergeState::AwaitingDevice(classes),
        std::path::PathBuf::new(),
    );
    assert!(should_hold_response(
        &mut waiting,
        &payload_for(key(0), 1),
        |_| true
    ));
    assert!(
        !should_hold_response(&mut waiting, &payload_for(key(0), 1), |_| false),
        "no LAND"
    );
    assert!(
        !should_hold_response(&mut waiting, &payload_for(CellKey::Interior(5), 5), |_| {
            true
        }),
        "an interior"
    );
    assert!(
        !should_hold_response(&mut waiting, &Err("no such cell".to_owned()), |_| true),
        "a failed load has nothing to wait for"
    );
    // Once the setup is over (ready here) nothing is held.
    let mut ready = ready_merge(std::path::Path::new(""));
    assert!(!should_hold_response(
        &mut ready,
        &payload_for(key(0), 1),
        |_| true
    ));
}

/// A chunk file as the converter's own writer produces it (Phase 1's `write_terrain_chunks`) is
/// read through the store: three cells in two chunk files, one of them west of the origin. The
/// store memory-maps the whole file, so the archive starts on a page boundary, which is what
/// rkyv's alignment needs; no sub-slice of a chunk is ever read.
#[test]
fn chunk_files_written_by_the_converter_are_read_through_the_store() {
    use converter::terrain_chunks::{PlacedCellMesh, write_terrain_chunks};
    let directory = tempfile::tempdir().unwrap();
    let a = merged_cell(&land(0x10, rolling, [0x100; 4], &atxt_layers(), true));
    let b = merged_cell(&land(0x11, rolling, [0x101; 4], &[], false));
    let c = merged_cell(&land(0x12, rolling, [0x102; 4], &[], true));
    let placed = |grid_x, grid_y, mesh: &TerrainCellMesh| PlacedCellMesh {
        worldspace_id: 60,
        grid_x,
        grid_y,
        mesh: mesh.clone(),
    };
    let report = write_terrain_chunks(
        directory.path(),
        vec![placed(1, 1, &a), placed(2, 3, &b), placed(-1, 0, &c)],
    )
    .unwrap();
    assert_eq!(report.cells, 3);
    assert_eq!(report.worldspaces[0].files, 2);

    let mut merge = ready_merge(directory.path());
    let exterior = |grid_x, grid_y| CellKey::Exterior {
        worldspace_id: 60,
        grid_x,
        grid_y,
    };
    for (key, cell_id, expected) in [
        (exterior(1, 1), 0x10, &a),
        (exterior(2, 3), 0x11, &b),
        (exterior(-1, 0), 0x12, &c),
    ] {
        match merge.lookup(key, cell_id) {
            CellLookup::Found(cell) => assert_eq!(&*cell, expected, "cell {cell_id:X}"),
            other => panic!("cell {cell_id:X}: {other:?}"),
        }
    }
    // A cell the chunk does not hold, and a chunk the writer did not write.
    assert!(matches!(
        merge.lookup(exterior(3, 3), 0x99),
        CellLookup::NoData
    ));
    assert!(matches!(
        merge.lookup(exterior(40, 40), 0x10),
        CellLookup::NoData
    ));
}
