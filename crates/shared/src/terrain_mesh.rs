//! Merged terrain: one mesh per exterior cell, written by the converter as derived data (#215).
//!
//! A cell mesh keeps its 4 quadrants unwelded: 4 x 17 x 17 = 1,156 vertices, laid out by
//! [`vertex_index`]. Every vertex of a quadrant carries the same slot list
//! ([`TerrainCellMesh::quadrant_slots`]): slot 0 is the quadrant's base texture, then its alpha
//! layers in ATXT layer order. A cell has 8 or 16 slots per quadrant
//! ([`TerrainCellMesh::slot_stride`]); 16 only when one of its quadrants needs more than 8.
//! Per-vertex weights are stored for slots 1 and up; the slot 0 weight is implied as
//! `max(0, 1 - sum of the others)`, as the shader computes it today.
//!
//! Slots name land textures by LTEX FormID in load-order numbering (0 = the default texture),
//! not by table id, so a cell depends only on its own records: adding or removing a landscape
//! texture elsewhere never makes it stale. The engine maps FormIDs to the u16 ids of the
//! game-wide [`LandTextureTable`] at load.
//!
//! Units and basis are the plugin's: cell-local Creation units, x east and y north in
//! `0..=4096`, z the absolute height decoded from VHGT (offset plus cumulative deltas, before
//! any engine edge weld), and normals and colours as the VNML and VCLR bytes store them. The
//! engine converts with [`crate::coordinates`] at load. Triangles are not stored:
//! [`quadrant_triangles`] gives the fixed split every reader uses.
//!
//! Land textures live in texture arrays, one per class (usage, block format, size). A source
//! keeps its own block format (copied block by block, never re-encoded) when the format is
//! one its usage keeps ([`DIFFUSE_FORMATS`], [`NORMAL_FORMATS`]), and goes into the smallest
//! size class that holds it without downscaling. Only a source format outside those lists is
//! transcoded, into the BC7 class of its usage, and reported. The table lists only the classes
//! the content uses; the engine binds them through a binding array, or specialises the shader
//! to the listed classes.

use rkyv::{Archive, Deserialize, Serialize};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use std::collections::HashMap;

/// Bumped whenever the layout below changes; part of every derived-data key.
pub const TERRAIN_MESH_FORMAT_VERSION: u32 = 1;
/// The slot strides a cell may use: 8 normally, 16 when a quadrant needs more than 8.
pub const TERRAIN_SLOT_STRIDES: [u8; 2] = [8, 16];
/// The most slots a quadrant can carry; the converter refuses a quadrant that needs more.
pub const MAX_TERRAIN_SLOTS: usize = 16;
/// Vertices along one side of a quadrant.
pub const QUADRANT_SIDE: usize = 17;
/// Vertices in one quadrant.
pub const QUADRANT_VERTICES: usize = QUADRANT_SIDE * QUADRANT_SIDE;
/// Vertices in one cell mesh (4 unwelded quadrants).
pub const CELL_MESH_VERTICES: usize = 4 * QUADRANT_VERTICES;
/// Creation units between neighbouring vertices (4096 / 32).
pub const TERRAIN_VERTEX_SPACING: f32 = 128.0;
/// The slot value and table entry of the default texture (a quadrant with no BTXT).
pub const DEFAULT_LAND_TEXTURE_FORM_ID: u32 = 0;
/// Land texture repeats across one cell: the one UV constant both sides use.
pub const TERRAIN_UV_REPEATS_PER_CELL: f32 = crate::LAND_TEXTURE_REPEATS_PER_CELL;
/// The texture-array size classes, in texels per side. A source goes into the smallest class
/// at least as large as it; a larger source is never downscaled without the user's choice.
pub const LAND_TEXTURE_SIZE_CLASSES: [u32; 3] = [1024, 2048, 4096];

/// Path of the land-texture table, relative to the converted output root.
pub const LAND_TEXTURE_TABLE_PATH: &str = "terrain/land-textures.json";
/// The default layer limit per texture array, written into every table as
/// `max_layers_per_array`. Desktop GPUs allow 2048 array layers; the engine checks the
/// device's real limit against the table.
pub const DEFAULT_MAX_LAND_TEXTURE_ARRAY_LAYERS: u32 = 2048;
/// Cells per side of one terrain chunk file. Chunks are anchored at cell (0, 0) of every
/// worldspace, so any worldspace works, with or without LOD settings.
pub const TERRAIN_CHUNK_SIDE_CELLS: i32 = 4;

/// The chunk (`x`, `y`) holding exterior cell (`grid_x`, `grid_y`), flooring toward negative
/// infinity so cells west or south of the origin land in their own chunk.
pub const fn terrain_chunk_for_cell(grid_x: i32, grid_y: i32) -> (i32, i32) {
    (
        grid_x.div_euclid(TERRAIN_CHUNK_SIDE_CELLS),
        grid_y.div_euclid(TERRAIN_CHUNK_SIDE_CELLS),
    )
}

/// Path of the chunk file holding exterior cell (`grid_x`, `grid_y`) of `worldspace_id`
/// (load-order numbering), relative to the converted output root.
pub fn terrain_chunk_path(worldspace_id: u32, grid_x: i32, grid_y: i32) -> String {
    let (chunk_x, chunk_y) = terrain_chunk_for_cell(grid_x, grid_y);
    format!("terrain/{worldspace_id:08x}/chunk_{chunk_x}_{chunk_y}.rkyv")
}
/// The block formats diffuse arrays keep as-is: BC1 and BC3 (vanilla) and BC7.
pub const DIFFUSE_FORMATS: [LandTextureFormat; 3] = [
    LandTextureFormat::Bc1,
    LandTextureFormat::Bc3,
    LandTextureFormat::Bc7,
];
/// The block formats normal arrays keep as-is: BC3 (vanilla), BC5 and BC7.
pub const NORMAL_FORMATS: [LandTextureFormat; 3] = [
    LandTextureFormat::Bc3,
    LandTextureFormat::Bc5,
    LandTextureFormat::Bc7,
];

/// The vertex index of quadrant-local vertex (`x`, `y`) of `quadrant`.
///
/// Quadrants follow xEdit's order (`wbQuadrantEnum`): 0 bottom left (south-west), 1 bottom
/// right, 2 top left, 3 top right. Rows run south to north, columns west to east.
pub const fn vertex_index(quadrant: usize, x: usize, y: usize) -> usize {
    quadrant * QUADRANT_VERTICES + y * QUADRANT_SIDE + x
}

/// The cell-grid position (0..=32 on each axis) of quadrant-local vertex (`x`, `y`).
pub const fn grid_position(quadrant: usize, x: usize, y: usize) -> (usize, usize) {
    ((quadrant % 2) * 16 + x, (quadrant / 2) * 16 + y)
}

/// The cell-local Creation-unit x and y of vertex `index`.
pub fn vertex_xy(index: usize) -> [f32; 2] {
    let quadrant = index / QUADRANT_VERTICES;
    let local = index % QUADRANT_VERTICES;
    let (gx, gy) = grid_position(quadrant, local % QUADRANT_SIDE, local / QUADRANT_SIDE);
    [
        gx as f32 * TERRAIN_VERTEX_SPACING,
        gy as f32 * TERRAIN_VERTEX_SPACING,
    ]
}

/// One of the four outer edges of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellEdge {
    West,
    East,
    South,
    North,
}

/// The vertex indices on `edge`, ordered along it (south to north, or west to east). The
/// vertex where two quadrants meet on the edge appears twice, once per quadrant, so an edge
/// lists 34 indices for its 33 grid positions.
pub fn edge_vertices(edge: CellEdge) -> Vec<usize> {
    let last = QUADRANT_SIDE - 1;
    let (quadrants, fixed_x, fixed_y): ([usize; 2], Option<usize>, Option<usize>) = match edge {
        CellEdge::West => ([0, 2], Some(0), None),
        CellEdge::East => ([1, 3], Some(last), None),
        CellEdge::South => ([0, 1], None, Some(0)),
        CellEdge::North => ([2, 3], None, Some(last)),
    };
    let mut indices = Vec::with_capacity(2 * QUADRANT_SIDE);
    for quadrant in quadrants {
        for step in 0..QUADRANT_SIDE {
            let x = fixed_x.unwrap_or(step);
            let y = fixed_y.unwrap_or(step);
            indices.push(vertex_index(quadrant, x, y));
        }
    }
    indices
}

/// The triangles of one quadrant as vertex indices into the cell mesh: today's split, two per
/// quad, `[a, b, c]` and `[b, d, c]` with `a` = (x, y), `b` = (x+1, y), `c` = (x, y+1),
/// `d` = (x+1, y+1), counter-clockwise seen from above. 512 triangles per quadrant.
pub fn quadrant_triangles(quadrant: usize) -> Vec<[u16; 3]> {
    let mut triangles = Vec::with_capacity(2 * (QUADRANT_SIDE - 1) * (QUADRANT_SIDE - 1));
    for y in 0..QUADRANT_SIDE - 1 {
        for x in 0..QUADRANT_SIDE - 1 {
            let a = vertex_index(quadrant, x, y) as u16;
            let b = a + 1;
            let c = a + QUADRANT_SIDE as u16;
            let d = c + 1;
            triangles.push([a, b, c]);
            triangles.push([b, d, c]);
        }
    }
    triangles
}

/// One exterior cell's merged terrain.
#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct TerrainCellMesh {
    /// The CELL FormID, in load-order numbering.
    pub cell_id: u32,
    /// The derived-data key this mesh was built from (#185): a hash of the LAND record, its
    /// plugin's master list, the LTEX and TXST records it uses and
    /// [`TERRAIN_MESH_FORMAT_VERSION`].
    pub key: [u8; 32],
    /// [`CELL_MESH_VERTICES`] absolute heights in Creation units, decoded from VHGT (offset
    /// plus cumulative deltas), before any edge weld.
    pub heights: Vec<f32>,
    /// [`CELL_MESH_VERTICES`] x 3 normal bytes, as VNML stores them.
    pub normals: Vec<i8>,
    /// Empty (white), or [`CELL_MESH_VERTICES`] x 3 colour bytes as VCLR stores them.
    pub vertex_colors: Vec<u8>,
    /// Slots per quadrant: one of [`TERRAIN_SLOT_STRIDES`].
    pub slot_stride: u8,
    /// 4 x `slot_stride` LTEX FormIDs (load-order numbering), quadrant by quadrant; slot 0 is
    /// the base, 0 the default texture. Slots past a quadrant's count are 0.
    pub quadrant_slots: Vec<u32>,
    /// Per quadrant: how many slots are in use (1..=`slot_stride`).
    pub quadrant_slot_counts: [u8; 4],
    /// [`CELL_MESH_VERTICES`] x `slot_stride` weights, 0..=255 for 0..=1, vertex by vertex.
    /// Slot 0 is written as 0 and ignored (implied); unused slots are 0.
    pub weights: Vec<u8>,
}

/// A file of cell meshes: one LOD-chunk group of a worldspace.
#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct TerrainChunkFile {
    pub format_version: u32,
    /// The WRLD FormID, in load-order numbering.
    pub worldspace_id: u32,
    pub cells: Vec<TerrainCellMesh>,
}

/// What a texture array holds, and so its colour space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerdeSerialize, SerdeDeserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandTextureUsage {
    /// Diffuse colour, sampled as sRGB (BC1, BC3 or BC7; see [`DIFFUSE_FORMATS`]).
    DiffuseSrgb,
    /// Normal map, sampled as linear (BC3, BC5 or BC7; see [`NORMAL_FORMATS`]).
    NormalLinear,
}

/// A block format kept in a land texture array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerdeSerialize, SerdeDeserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandTextureFormat {
    Bc1,
    Bc3,
    Bc5,
    Bc7,
}

/// One texture array file: every member of one class, with full mip chains. Diffuse arrays
/// carry the sRGB variant of their format, normal arrays the linear one.
#[derive(Debug, Clone, PartialEq, SerdeSerialize, SerdeDeserialize)]
pub struct LandTextureArray {
    /// Path relative to the converted output root, e.g. `terrain/land-diffuse-bc1-1024.ktx2`.
    /// [`LandTextureTable::validate`] refuses any other shape. The check reads the path string
    /// only: a symlink at this path inside the pack can still resolve outside the pack root.
    pub path: String,
    pub usage: LandTextureUsage,
    pub format: LandTextureFormat,
    /// One of [`LAND_TEXTURE_SIZE_CLASSES`].
    pub size: u32,
    /// Layers in the file, at least 2 (a single member is padded with a copy), at most
    /// [`LandTextureTable::max_layers_per_array`].
    pub layers: u32,
}

/// A layer of one array in [`LandTextureTable::arrays`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerdeSerialize, SerdeDeserialize)]
pub struct ArrayLayer {
    pub array: u8,
    pub layer: u16,
}

/// Where one land texture lives.
#[derive(Debug, Clone, PartialEq, SerdeSerialize, SerdeDeserialize)]
pub struct LandTextureEntry {
    /// The id the engine puts in the per-vertex slot attribute: the entry's index.
    pub id: u16,
    /// LTEX FormID in load-order numbering; [`DEFAULT_LAND_TEXTURE_FORM_ID`] for the default.
    pub ltex_form_id: u32,
    pub diffuse: ArrayLayer,
    /// None when the texture set has no normal map.
    pub normal: Option<ArrayLayer>,
}

/// The default texture a conversion was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerdeSerialize, SerdeDeserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandTextureRequest {
    White,
    SkyrimIni,
}

/// What entry 0 of a table holds once the arrays are built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerdeSerialize, SerdeDeserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandTextureDefault {
    /// The generated 4 x 4 white diffuse: requested, or the only option.
    GeneratedWhite,
    /// The `[Landscape]` default diffuse (and normal, when one was named and converted).
    SkyrimIni,
    /// `skyrim-ini` was requested, but its diffuse could not be converted: entry 0 is white.
    SkyrimIniFallbackWhite,
}

/// The settings a table was built from, as the converter's terrain stage resolved them. Entry 0's
/// source, the downscale allowance and the Skyrim.ini bytes it was read from change the arrays
/// and entries, so a stored table records them.
///
/// `default_diffuse` and `default_normal` are converted `.ktx2` paths, the textures entry 0 holds,
/// and only when entry 0 holds the ini's own textures (`skyrim_ini`). They are absent for
/// `generated_white` and `skyrim_ini_fallback_white`, where entry 0 is the white texture; the DDS
/// paths a run asked for are reported elsewhere, not here. `default_normal` is absent when the ini
/// named no normal map or its normal did not convert.
///
/// `terrain_ini_sha256` is the SHA-256 of the Skyrim.ini bytes the defaults were parsed from, taken
/// from that same read (`None` for the built-in Dirt02 paths and for `white`).
#[derive(Debug, Clone, PartialEq, Eq, SerdeSerialize, SerdeDeserialize)]
pub struct LandTextureSettings {
    pub request: LandTextureRequest,
    pub default: LandTextureDefault,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_diffuse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_normal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain_ini_sha256: Option<String>,
    pub allow_downscale: bool,
}

/// `terrain/land-textures.json`: the land-texture arrays and the lookup table.
/// `entries` is indexed by id (`entries[i].id == i`, dense from 0); entry 0 is the default
/// texture.
#[derive(Debug, Clone, PartialEq, SerdeSerialize, SerdeDeserialize)]
pub struct LandTextureTable {
    pub format_version: u32,
    /// The layer limit both sides check: the converter never writes more, and the engine
    /// refuses the table if the device allows fewer.
    pub max_layers_per_array: u32,
    pub arrays: Vec<LandTextureArray>,
    pub entries: Vec<LandTextureEntry>,
    /// The settings the converter built this table from. Absent in tables written before the
    /// field existed, and then the table does not say which default texture it holds: it must not
    /// be taken as proof of any particular setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<LandTextureSettings>,
}

/// Why a cell mesh or texture table is refused. The engine falls back to the runtime build
/// for a refused cell and reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum TerrainMeshError {
    FormatVersion {
        found: u32,
    },
    Length {
        field: &'static str,
        found: usize,
        expected: usize,
    },
    NonFiniteHeight {
        vertex: usize,
    },
    SlotStride {
        found: u8,
    },
    SlotCount {
        quadrant: usize,
        count: u8,
    },
    UnusedSlotSet {
        quadrant: usize,
        slot: usize,
    },
    UnknownTexture {
        quadrant: usize,
        slot: usize,
        ltex_form_id: u32,
    },
    ImpliedWeightSet {
        vertex: usize,
    },
    TableNotDense {
        index: usize,
        id: u16,
    },
    TableDuplicate {
        ltex_form_id: u32,
    },
    TableArray {
        id: u16,
        array: u8,
    },
    ArrayShape {
        array: usize,
        size: u32,
        layers: u32,
    },
    ArrayClass {
        array: usize,
    },
    /// An array file's `path` is not a canonical relative path: absolute, above the root, URI
    /// syntax, a control character or empty.
    ArrayPath {
        array: usize,
        path: String,
    },
    /// An array file's `path` is relative, but not a `terrain/*.ktx2` file: the only shape the
    /// converter writes, and so the only one the engine will open.
    ArrayPathOutsideTerrain {
        array: usize,
        path: String,
    },
}

impl std::fmt::Display for TerrainMeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FormatVersion { found } => write!(
                f,
                "terrain format version {found}, expected {TERRAIN_MESH_FORMAT_VERSION}"
            ),
            Self::Length {
                field,
                found,
                expected,
            } => write!(f, "{field} has {found} values, expected {expected}"),
            Self::NonFiniteHeight { vertex } => {
                write!(f, "height at vertex {vertex} is not finite")
            }
            Self::SlotStride { found } => write!(f, "slot stride {found}, expected 8 or 16"),
            Self::SlotCount { quadrant, count } => {
                write!(
                    f,
                    "quadrant {quadrant} uses {count} slots, more than its stride or none"
                )
            }
            Self::UnusedSlotSet { quadrant, slot } => {
                write!(
                    f,
                    "quadrant {quadrant} slot {slot} is past the count but not empty"
                )
            }
            Self::UnknownTexture {
                quadrant,
                slot,
                ltex_form_id,
            } => write!(
                f,
                "quadrant {quadrant} slot {slot} names land texture {ltex_form_id:08X}, which the table lacks"
            ),
            Self::ImpliedWeightSet { vertex } => {
                write!(
                    f,
                    "vertex {vertex} stores a slot-0 weight, which is implied"
                )
            }
            Self::TableNotDense { index, id } => {
                write!(f, "land texture entry {index} has id {id}")
            }
            Self::TableDuplicate { ltex_form_id } => {
                write!(
                    f,
                    "land texture {ltex_form_id:08X} appears twice in the table"
                )
            }
            Self::TableArray { id, array } => {
                write!(
                    f,
                    "land texture {id} names a missing array or layer ({array})"
                )
            }
            Self::ArrayShape {
                array,
                size,
                layers,
            } => write!(
                f,
                "texture array {array}: size {size} or {layers} layers is outside the format"
            ),
            Self::ArrayClass { array } => write!(
                f,
                "texture array {array}: its usage and format are not a class, or the class repeats"
            ),
            Self::ArrayPath { array, path } => write!(
                f,
                "texture array {array}: path {path:?} is not a relative path inside the converted pack"
            ),
            Self::ArrayPathOutsideTerrain { array, path } => write!(
                f,
                "texture array {array}: path {path:?} is not a terrain/*.ktx2 file"
            ),
        }
    }
}

impl std::error::Error for TerrainMeshError {}

/// Checks one array file's `path` is confined to the pack: a canonical relative path under
/// `terrain/` with a `.ktx2` suffix, the only shape the converter writes. Without this the
/// engine's `root.join(&path)` would follow `..` or an absolute path out of the asset root
/// (`join` discards the root for a drive path), and the asset server would be handed the same
/// escaping path. [`crate::streaming_costs::canonical_resource_key`] normalizes resource
/// identities; require that it leaves the path unchanged because readers use the original
/// string. The `terrain/*.ktx2` shape is this table's own: one nonempty filename directly
/// under `terrain/`, with no intermediate directories for the OS to interpret.
fn validate_array_path(array: usize, path: &str) -> Result<(), TerrainMeshError> {
    let key = crate::streaming_costs::canonical_resource_key(path).map_err(|_| {
        TerrainMeshError::ArrayPath {
            array,
            path: path.to_owned(),
        }
    })?;
    if key != path {
        return Err(TerrainMeshError::ArrayPath {
            array,
            path: path.to_owned(),
        });
    }
    let terrain_ktx2 = key
        .strip_prefix("terrain/")
        .and_then(|file| file.strip_suffix(".ktx2"))
        .is_some_and(|stem| !stem.is_empty() && !stem.contains('/'));
    if !terrain_ktx2 {
        return Err(TerrainMeshError::ArrayPathOutsideTerrain {
            array,
            path: path.to_owned(),
        });
    }
    Ok(())
}

impl LandTextureTable {
    /// Checks the version, the array shapes, that every array path has the one shape the
    /// converter writes (a lexical check, see [`LandTextureArray::path`]), that entries are dense by id with no repeated LTEX, and that
    /// every entry names an existing array layer.
    pub fn validate(&self) -> Result<(), TerrainMeshError> {
        if self.format_version != TERRAIN_MESH_FORMAT_VERSION {
            return Err(TerrainMeshError::FormatVersion {
                found: self.format_version,
            });
        }
        let mut classes = std::collections::HashSet::new();
        for (array, file) in self.arrays.iter().enumerate() {
            validate_array_path(array, &file.path)?;
            let formats = match file.usage {
                LandTextureUsage::DiffuseSrgb => DIFFUSE_FORMATS,
                LandTextureUsage::NormalLinear => NORMAL_FORMATS,
            };
            if !formats.contains(&file.format)
                || !classes.insert((file.usage, file.format, file.size))
            {
                return Err(TerrainMeshError::ArrayClass { array });
            }
            if !LAND_TEXTURE_SIZE_CLASSES.contains(&file.size)
                || file.layers < 2
                || file.layers > self.max_layers_per_array
            {
                return Err(TerrainMeshError::ArrayShape {
                    array,
                    size: file.size,
                    layers: file.layers,
                });
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if usize::from(entry.id) != index {
                return Err(TerrainMeshError::TableNotDense {
                    index,
                    id: entry.id,
                });
            }
            if !seen.insert(entry.ltex_form_id) {
                return Err(TerrainMeshError::TableDuplicate {
                    ltex_form_id: entry.ltex_form_id,
                });
            }
            let layers = std::iter::once((entry.diffuse, LandTextureUsage::DiffuseSrgb)).chain(
                entry
                    .normal
                    .map(|layer| (layer, LandTextureUsage::NormalLinear)),
            );
            for (layer, usage) in layers {
                // The array must be the entry's own usage: a diffuse layer in a normal array
                // would sample the wrong texture class and colour space with nothing to report it.
                let known = self
                    .arrays
                    .get(usize::from(layer.array))
                    .is_some_and(|file| {
                        file.usage == usage && u32::from(layer.layer) < file.layers
                    });
                if !known {
                    return Err(TerrainMeshError::TableArray {
                        id: entry.id,
                        array: layer.array,
                    });
                }
            }
        }
        Ok(())
    }

    /// LTEX FormID to table id, built once per load.
    pub fn ids_by_form_id(&self) -> HashMap<u32, u16> {
        self.entries
            .iter()
            .map(|entry| (entry.ltex_form_id, entry.id))
            .collect()
    }
}

impl TerrainCellMesh {
    /// Checks lengths, slot lists and weights against the layout, and that every slot's
    /// texture is in the table (`ids` from [`LandTextureTable::ids_by_form_id`]). The converter
    /// calls it before writing a cell, and the engine before using one.
    pub fn validate(&self, ids: &HashMap<u32, u16>) -> Result<(), TerrainMeshError> {
        if !TERRAIN_SLOT_STRIDES.contains(&self.slot_stride) {
            return Err(TerrainMeshError::SlotStride {
                found: self.slot_stride,
            });
        }
        let stride = usize::from(self.slot_stride);
        let lengths = [
            ("heights", self.heights.len(), CELL_MESH_VERTICES),
            ("normals", self.normals.len(), 3 * CELL_MESH_VERTICES),
            ("quadrant_slots", self.quadrant_slots.len(), 4 * stride),
            ("weights", self.weights.len(), stride * CELL_MESH_VERTICES),
        ];
        for (field, found, expected) in lengths {
            if found != expected {
                return Err(TerrainMeshError::Length {
                    field,
                    found,
                    expected,
                });
            }
        }
        if !self.vertex_colors.is_empty() && self.vertex_colors.len() != 3 * CELL_MESH_VERTICES {
            return Err(TerrainMeshError::Length {
                field: "vertex_colors",
                found: self.vertex_colors.len(),
                expected: 3 * CELL_MESH_VERTICES,
            });
        }
        if let Some(vertex) = self.heights.iter().position(|height| !height.is_finite()) {
            return Err(TerrainMeshError::NonFiniteHeight { vertex });
        }
        for quadrant in 0..4 {
            let count = self.quadrant_slot_counts[quadrant];
            if count == 0 || usize::from(count) > stride {
                return Err(TerrainMeshError::SlotCount { quadrant, count });
            }
            let slots = &self.quadrant_slots[quadrant * stride..(quadrant + 1) * stride];
            for (slot, &ltex_form_id) in slots.iter().enumerate() {
                if slot >= usize::from(count) {
                    let weighted = (0..QUADRANT_VERTICES).any(|local| {
                        self.weights[(quadrant * QUADRANT_VERTICES + local) * stride + slot] != 0
                    });
                    if ltex_form_id != 0 || weighted {
                        return Err(TerrainMeshError::UnusedSlotSet { quadrant, slot });
                    }
                } else if !ids.contains_key(&ltex_form_id) {
                    return Err(TerrainMeshError::UnknownTexture {
                        quadrant,
                        slot,
                        ltex_form_id,
                    });
                }
            }
        }
        if let Some(vertex) =
            (0..CELL_MESH_VERTICES).find(|vertex| self.weights[vertex * stride] != 0)
        {
            return Err(TerrainMeshError::ImpliedWeightSet { vertex });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(textures: u16) -> LandTextureTable {
        LandTextureTable {
            format_version: TERRAIN_MESH_FORMAT_VERSION,
            max_layers_per_array: 2048,
            settings: None,
            arrays: vec![LandTextureArray {
                path: "terrain/land-diffuse-bc1-1024.ktx2".into(),
                usage: LandTextureUsage::DiffuseSrgb,
                format: LandTextureFormat::Bc1,
                size: 1024,
                layers: u32::from(textures.max(2)),
            }],
            entries: (0..textures)
                .map(|id| LandTextureEntry {
                    id,
                    // Entry 0 is the default texture; the rest stand for LTEX 0x100, 0x101...
                    ltex_form_id: if id == 0 { 0 } else { 0xFF + u32::from(id) },
                    diffuse: ArrayLayer {
                        array: 0,
                        layer: id,
                    },
                    normal: None,
                })
                .collect(),
        }
    }

    fn flat_mesh(stride: u8) -> TerrainCellMesh {
        let stride_len = usize::from(stride);
        TerrainCellMesh {
            cell_id: 1,
            key: [0; 32],
            heights: vec![0.0; CELL_MESH_VERTICES],
            normals: vec![0; 3 * CELL_MESH_VERTICES],
            vertex_colors: Vec::new(),
            slot_stride: stride,
            quadrant_slots: vec![0; 4 * stride_len],
            quadrant_slot_counts: [1; 4],
            weights: vec![0; stride_len * CELL_MESH_VERTICES],
        }
    }

    #[test]
    fn layout_covers_the_cell_grid_with_duplicated_inner_seams() {
        let mut seen = HashMap::new();
        for index in 0..CELL_MESH_VERTICES {
            *seen.entry(vertex_xy(index).map(|v| v as i32)).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), 33 * 33);
        assert_eq!(seen[&[0, 0]], 1);
        assert_eq!(
            seen[&[2048, 2048]],
            4,
            "the cell centre is in every quadrant"
        );
        assert_eq!(seen[&[2048, 0]], 2);
        assert_eq!(vertex_xy(vertex_index(3, 16, 16)), [4096.0, 4096.0]);
    }

    #[test]
    fn edges_list_their_vertices_in_order() {
        let west = edge_vertices(CellEdge::West);
        assert_eq!(west.len(), 34);
        assert!(west.iter().all(|&index| vertex_xy(index)[0] == 0.0));
        let ys: Vec<f32> = west.iter().map(|&index| vertex_xy(index)[1]).collect();
        assert!(ys.windows(2).all(|pair| pair[0] <= pair[1]));
        let north = edge_vertices(CellEdge::North);
        assert!(north.iter().all(|&index| vertex_xy(index)[1] == 4096.0));
        let east = edge_vertices(CellEdge::East);
        assert!(east.iter().all(|&index| vertex_xy(index)[0] == 4096.0));
    }

    #[test]
    fn chunk_paths_floor_toward_negative_infinity() {
        assert_eq!(terrain_chunk_for_cell(0, 3), (0, 0));
        assert_eq!(terrain_chunk_for_cell(-1, 4), (-1, 1));
        assert_eq!(terrain_chunk_for_cell(-4, -5), (-1, -2));
        assert_eq!(
            terrain_chunk_path(0x3C, 5, -1),
            "terrain/0000003c/chunk_1_-1.rkyv"
        );
    }

    #[test]
    fn quadrant_triangles_match_the_runtime_split() {
        let triangles = quadrant_triangles(1);
        assert_eq!(triangles.len(), 512);
        let a = vertex_index(1, 0, 0) as u16;
        assert_eq!(triangles[0], [a, a + 1, a + 17]);
        assert_eq!(triangles[1], [a + 1, a + 18, a + 17]);
    }

    #[test]
    fn validate_accepts_consistent_meshes_at_both_strides() {
        let table = table(3);
        table.validate().unwrap();
        let ids = table.ids_by_form_id();
        for stride in TERRAIN_SLOT_STRIDES {
            let s = usize::from(stride);
            let mut mesh = flat_mesh(stride);
            mesh.quadrant_slots[2 * s] = 0x100;
            mesh.quadrant_slots[2 * s + 1] = 0x101;
            mesh.quadrant_slot_counts[2] = 2;
            mesh.weights[vertex_index(2, 4, 4) * s + 1] = 200;
            mesh.validate(&ids).unwrap();
        }
    }

    #[test]
    fn validate_refuses_broken_meshes() {
        let ids = table(3).ids_by_form_id();
        let mut mesh = flat_mesh(8);
        mesh.heights[5] = f32::NAN;
        assert_eq!(
            mesh.validate(&ids),
            Err(TerrainMeshError::NonFiniteHeight { vertex: 5 })
        );

        let mut mesh = flat_mesh(8);
        mesh.weights[vertex_index(1, 0, 0) * 8 + 3] = 1;
        assert_eq!(
            mesh.validate(&ids),
            Err(TerrainMeshError::UnusedSlotSet {
                quadrant: 1,
                slot: 3
            })
        );

        let mut mesh = flat_mesh(8);
        mesh.quadrant_slots[0] = 0x0BAD;
        assert_eq!(
            mesh.validate(&ids),
            Err(TerrainMeshError::UnknownTexture {
                quadrant: 0,
                slot: 0,
                ltex_form_id: 0x0BAD
            })
        );

        let mut mesh = flat_mesh(8);
        mesh.weights[7 * 8] = 1;
        assert_eq!(
            mesh.validate(&ids),
            Err(TerrainMeshError::ImpliedWeightSet { vertex: 7 })
        );

        let mut mesh = flat_mesh(8);
        mesh.quadrant_slot_counts[3] = 9;
        assert!(matches!(
            mesh.validate(&ids),
            Err(TerrainMeshError::SlotCount { .. })
        ));

        let mut mesh = flat_mesh(8);
        mesh.slot_stride = 12;
        assert_eq!(
            mesh.validate(&ids),
            Err(TerrainMeshError::SlotStride { found: 12 })
        );
    }

    /// A table with no `settings` key is the pre-field format: it parses with `settings` absent,
    /// and a table without settings serializes to exactly that key-free form, so readers that
    /// predate the field see no change.
    #[test]
    fn the_settings_field_is_additive_in_both_directions() {
        let without = serde_json::to_string(&table(2)).unwrap();
        assert!(!without.contains("settings"), "{without}");
        let parsed: LandTextureTable = serde_json::from_str(&without).unwrap();
        assert_eq!(parsed.settings, None);
        assert_eq!(parsed, table(2));

        let mut with = table(2);
        with.settings = Some(LandTextureSettings {
            request: LandTextureRequest::SkyrimIni,
            default: LandTextureDefault::SkyrimIniFallbackWhite,
            default_diffuse: Some("textures/landscape/Dirt02.dds".into()),
            default_normal: None,
            terrain_ini_sha256: Some("ab".repeat(32)),
            allow_downscale: true,
        });
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("\"skyrim_ini_fallback_white\""), "{json}");
        assert!(
            !json.contains("default_normal"),
            "an absent normal is omitted: {json}"
        );
        let back: LandTextureTable = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with);
        // The same arrays and entries under different settings are different tables.
        assert_ne!(back.settings, table(2).settings);
    }

    #[test]
    fn validate_refuses_bad_tables_and_typos_in_the_json() {
        let mut sparse = table(3);
        sparse.entries[1].id = 2;
        assert_eq!(
            sparse.validate(),
            Err(TerrainMeshError::TableNotDense { index: 1, id: 2 })
        );
        let mut repeated = table(3);
        repeated.entries[2].ltex_form_id = 0x100;
        assert_eq!(
            repeated.validate(),
            Err(TerrainMeshError::TableDuplicate {
                ltex_form_id: 0x100
            })
        );
        let mut odd = table(3);
        odd.arrays[0].size = 512;
        assert!(matches!(
            odd.validate(),
            Err(TerrainMeshError::ArrayShape { size: 512, .. })
        ));
        let mut bc5_diffuse = table(3);
        bc5_diffuse.arrays[0].format = LandTextureFormat::Bc5;
        assert_eq!(
            bc5_diffuse.validate(),
            Err(TerrainMeshError::ArrayClass { array: 0 })
        );
        let mut repeated_class = table(3);
        repeated_class.arrays.push(repeated_class.arrays[0].clone());
        assert_eq!(
            repeated_class.validate(),
            Err(TerrainMeshError::ArrayClass { array: 1 })
        );
        let json = serde_json::to_string(&table(2)).unwrap();
        assert!(json.contains("\"diffuse_srgb\""), "{json}");
        let typo = json.replace("diffuse_srgb", "diffuse_srbg");
        assert!(serde_json::from_str::<LandTextureTable>(&typo).is_err());
    }

    /// Absolute, drive, UNC, parent and encoded paths are refused before any reader opens them.
    #[test]
    fn validate_refuses_array_paths_that_leave_the_pack() {
        table(3).validate().unwrap();
        // Forms that would resolve outside the converted output root: `..` escapes on every
        // platform, and Windows `join` discards the root for a drive-absolute path.
        for escaped in [
            "",
            "../x.ktx2",
            "terrain/../../x.ktx2",
            "terrain\\..\\x.ktx2",
            "/terrain/x.ktx2",
            "C:/x.ktx2",
            "C:\\x.ktx2",
            "C:terrain/x.ktx2",
            "\\terrain\\x.ktx2",
            "\\\\host\\share\\x.ktx2",
            "\\\\?\\C:\\terrain\\x.ktx2",
            "//host/share/terrain/x.ktx2",
            "terrain/x.ktx2:stream",
            "terrain/%2e%2e/x.ktx2",
            "terrain/x.ktx2?query",
            "terrain/x.ktx2#label",
            "terrain/x.ktx2\0",
        ] {
            let mut bad = table(3);
            bad.arrays[0].path = escaped.into();
            assert_eq!(
                bad.validate(),
                Err(TerrainMeshError::ArrayPath {
                    array: 0,
                    path: escaped.into()
                }),
                "{escaped:?} must be refused"
            );
        }
        // Relative, but not the shape the converter writes: the engine would open a path the
        // pack does not have.
        for elsewhere in ["textures/x.ktx2", "terrain/x.dds", "terrain/x", "x.ktx2"] {
            let mut bad = table(3);
            bad.arrays[0].path = elsewhere.into();
            assert_eq!(
                bad.validate(),
                Err(TerrainMeshError::ArrayPathOutsideTerrain {
                    array: 0,
                    path: elsewhere.into()
                }),
                "{elsewhere:?} must be refused"
            );
        }
    }

    /// A path that only matches after normalising is refused as written, unused arrays included.
    #[test]
    fn validate_refuses_normalized_array_paths_before_readers_use_the_original() {
        for path in [
            "terrain\\x.ktx2",
            "TERRAIN/x.ktx2",
            "terrain/X.KTX2",
            "./terrain/x.ktx2",
            "terrain/./x.ktx2",
            "terrain//x.ktx2",
            "terrain/x.ktx2/.",
            "terrain/x.ktx2/",
        ] {
            let mut bad = table(3);
            // Validate unused normal arrays too, not only entry-referenced diffuse arrays.
            let mut normal = bad.arrays[0].clone();
            normal.path = path.into();
            normal.usage = LandTextureUsage::NormalLinear;
            normal.format = LandTextureFormat::Bc5;
            bad.arrays.push(normal);
            assert_eq!(
                bad.validate(),
                Err(TerrainMeshError::ArrayPath {
                    array: 1,
                    path: path.into(),
                }),
                "{path:?} must be refused without rewriting it"
            );
        }
    }

    /// An array path names exactly one nonempty file directly under `terrain/`.
    #[test]
    fn validate_requires_one_array_filename_under_terrain() {
        for path in [
            "terrain/.ktx2",
            "terrain/subdir/x.ktx2",
            "terrain/.. /x.ktx2",
            "terrain/.../x.ktx2",
        ] {
            let mut bad = table(3);
            bad.arrays[0].path = path.into();
            assert_eq!(
                bad.validate(),
                Err(TerrainMeshError::ArrayPathOutsideTerrain {
                    array: 0,
                    path: path.into(),
                }),
                "{path:?} is not one array filename under terrain"
            );
        }
    }

    #[test]
    fn validate_refuses_a_layer_in_an_array_of_the_other_usage() {
        let normal_array = LandTextureArray {
            path: "terrain/land-normal-bc3-1024.ktx2".into(),
            usage: LandTextureUsage::NormalLinear,
            format: LandTextureFormat::Bc3,
            size: 1024,
            layers: 3,
        };
        let mut ok = table(3);
        ok.arrays.push(normal_array);
        ok.entries[1].normal = Some(ArrayLayer { array: 1, layer: 1 });
        assert_eq!(ok.validate(), Ok(()));

        // A diffuse layer that names the normal array would sample the wrong class and colour space.
        let mut diffuse_in_normal = ok.clone();
        diffuse_in_normal.entries[1].diffuse = ArrayLayer { array: 1, layer: 1 };
        assert_eq!(
            diffuse_in_normal.validate(),
            Err(TerrainMeshError::TableArray { id: 1, array: 1 })
        );
        // And a normal layer that names a diffuse array.
        let mut normal_in_diffuse = ok.clone();
        normal_in_diffuse.entries[1].normal = Some(ArrayLayer { array: 0, layer: 1 });
        assert_eq!(
            normal_in_diffuse.validate(),
            Err(TerrainMeshError::TableArray { id: 1, array: 0 })
        );
    }
}
