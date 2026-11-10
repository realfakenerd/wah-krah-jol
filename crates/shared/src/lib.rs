//! Stable data contracts shared by the offline converter and the runtime.

pub mod asset_lock;
pub mod collision;
pub mod coordinates;
pub mod extensions;
pub mod lod;
pub mod plugin_selection;
pub mod records;
pub mod streaming_costs;
pub mod terrain_mesh;
pub mod world_assets;

use rkyv::{Archive, Deserialize, Serialize};

/// Current combined database contract: corrected TXST roles and one-byte XESP flags.
pub const WORLD_DATABASE_SCHEMA_VERSION: u32 = 9;
/// Latest producer required when a package advertises the combined LOD contract.
/// 27: collision coverage (sphere shapes as zero-length capsules, multi-sphere and cylinder
/// shapes) changes GLB collision extras.
/// 28: KTX2 texture containers store mip level data smallest-first. Uncompressed
/// levels use exact `lcm(texel block bytes, 4)` alignment; supercompressed levels
/// are contiguous (KTX 2.0 sections 3.7 and 3.9.7). Producer-27
/// texture bytes keep the largest-first, 16-byte-padded layout and must be regenerated.
/// GLB bytes are unchanged, so producer-27 meshes stay reusable.
pub const LOD_CONVERTER_SCHEMA_VERSION: u32 = 28;
pub const WORLD_DATABASE_LOD_SCHEMA_VERSION: u32 = 6;

/// The oldest world database schema the runtime (the engine and `world-inspect`) still reads, and
/// so the oldest the launcher calls ready. Schemas 4, 5 and 8 only added tables and columns
/// (`lights`, `references.radius_override`, the movement tables, water fresnel columns, grass tables,
/// `references.emittance_id` and `emittance`), and every runtime query probes for them, so a schema
/// 3 database still loads.
pub const MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION: u32 = 3;

/// The oldest converter manifest schema the runtime still loads, and so the oldest the launcher
/// calls ready. Converter schema 15 wrote world database schema 3, which the runtime reads (see
/// [`MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION`]). The newest is the converter's own
/// `CONVERTER_SCHEMA_VERSION`, which lives in the converter crate.
pub const MIN_RUNTIME_CONVERTER_SCHEMA_VERSION: u32 = 15;

/// Whether the runtime reads a world database of this schema: from
/// [`MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION`] through [`WORLD_DATABASE_SCHEMA_VERSION`].
pub fn supports_runtime_world_database_schema(version: u32) -> bool {
    (MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION..=WORLD_DATABASE_SCHEMA_VERSION).contains(&version)
}

pub const CELL_CACHE_VERSION: u32 = 3;
pub const LAND_SIDE: u16 = 33;

/// Default LAND texture repeats per full cell per axis. Skyrim advances texture UVs
/// by `fLandTextureTilingMult / 4` per sample interval; the vanilla default is 3,
/// so 32 intervals give `32 * (3 / 4) = 24` repeats per cell.
/// Apply this to diffuse/normal sampling, never to the LAND blend-weight grid.
/// See [landscape texture scale](../../../docs/specs/engine/landscape-texture-scale.md)
/// for sources and verification.
pub const LAND_TEXTURE_REPEATS_PER_CELL: f32 = 32.0 * (3.0 / 4.0);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_world_database_schemas_run_from_the_oldest_to_the_current() {
        let oldest = MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION;
        assert!(!supports_runtime_world_database_schema(oldest - 1));
        assert!(supports_runtime_world_database_schema(oldest));
        assert!(supports_runtime_world_database_schema(
            WORLD_DATABASE_SCHEMA_VERSION
        ));
        assert!(!supports_runtime_world_database_schema(
            WORLD_DATABASE_SCHEMA_VERSION + 1
        ));
    }
}
