//! Stable data contracts shared by the offline converter and the runtime.

pub mod coordinates;

use rkyv::{Archive, Deserialize, Serialize};

pub const WORLD_DATABASE_SCHEMA_VERSION: u32 = 4;
pub const CELL_CACHE_VERSION: u32 = 3;
pub const LAND_SIDE: u16 = 33;

#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct TerrainLayer {
    pub texture_form_id: u32,
    pub quadrant: u8,
    pub layer: u16,
    pub is_base: bool,
    pub weights: Vec<TerrainWeight>,
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct TerrainWeight {
    pub vertex: u16,
    pub opacity: f32,
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct CachedLand {
    pub cell_id: u32,
    pub width: u16,
    pub height: u16,
    /// Row-major, absolute Creation Engine height units.
    pub heights: Vec<f32>,
    /// Packed signed XYZ normals, three bytes per vertex.
    pub normals: Vec<i8>,
    /// Packed RGB colors, three bytes per vertex.
    pub vertex_colors: Vec<u8>,
    pub layers: Vec<TerrainLayer>,
    pub water_height: Option<f32>,
    pub water_type_form_id: Option<u32>,
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize, PartialEq)]
#[rkyv(bytecheck())]
pub struct CellCache {
    pub version: u32,
    pub cells: Vec<CachedLand>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds3 {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds3 {
    pub const UNIT: Self = Self {
        min: [-0.5; 3],
        max: [0.5; 3],
    };

    pub fn is_finite_and_ordered(self) -> bool {
        self.min
            .iter()
            .chain(self.max.iter())
            .all(|value| value.is_finite())
            && (0..3).all(|axis| self.min[axis] <= self.max[axis])
    }
}
