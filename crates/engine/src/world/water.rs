//! Queries over the same flat CELL/WRLD water surfaces that streaming renders.
//!
//! The converter has already resolved CELL height/type against WRLD defaults in
//! `TerrainSnapshot`. This module does not resolve records or maintain another registry.

use super::{
    cache::TerrainSnapshot,
    components::{CELL_SIZE, WorldPosition},
    database::CellKey,
};
use bevy::prelude::*;

/// A flat surface in Creation units (Z up), with its existing WATR catalog key.
/// A missing catalog row does not make the surface dry; rendering uses its usual fallback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatWater {
    pub height: f32,
    pub water_type_form_id: Option<u32>,
}

impl FlatWater {
    /// The single height-validity rule used by surface spawning and resource accounting.
    pub fn from_terrain(terrain: &TerrainSnapshot) -> Option<Self> {
        let height = terrain
            .water_height
            .filter(|height| height.is_finite() && height.abs() < 1.0e7)?;
        Some(Self {
            height,
            water_type_form_id: terrain.water_type_form_id,
        })
    }
}

/// Coverage of one committed exterior water plane, attached to its `WaterSurface` entity.
/// Despawning the cell removes query coverage with the rendered surface, including on a
/// worldspace change. Retiring cells remain queryable until actually despawned, like their meshes.
/// Coverage exists during texture preparation; it is not a GPU-readiness signal.
#[derive(Component, Debug, Clone)]
pub struct FlatWaterBody {
    worldspace_id: u32,
    grid: IVec2,
    cell_id: u32,
    surface: FlatWater,
    heights: Box<[f32]>,
}

impl FlatWaterBody {
    /// Only the existing complete exterior LAND height field defines a footprint today.
    /// Interior pools, water without LAND and authored nonrectangular water meshes need geometry
    /// that the current cache does not carry; do not invent coverage for them.
    pub fn from_terrain(key: CellKey, terrain: &TerrainSnapshot) -> Option<Self> {
        let CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } = key
        else {
            return None;
        };
        let surface = FlatWater::from_terrain(terrain)?;
        if terrain.width != 33
            || terrain.height != 33
            || terrain.heights.len() != 33 * 33
            || terrain.heights.iter().any(|height| !height.is_finite())
        {
            return None;
        }
        Some(Self {
            worldspace_id,
            grid: IVec2::new(grid_x, grid_y),
            cell_id: terrain.cell_id,
            surface,
            heights: terrain.heights.clone().into_boxed_slice(),
        })
    }

    fn sample(&self, worldspace_id: u32, point: WorldPosition) -> Option<WaterSample> {
        if self.worldspace_id != worldspace_id
            || self.grid != point.grid
            || !point.local.is_finite()
            || !(0.0..CELL_SIZE).contains(&point.local.x)
            || !(0.0..CELL_SIZE).contains(&point.local.y)
        {
            return None;
        }
        let sample = point.local.truncate() * (32.0 / CELL_SIZE);
        let x = (sample.x.floor() as usize).min(31);
        let y = (sample.y.floor() as usize).min(31);
        let u = sample.x - x as f32;
        let v = sample.y - y as f32;
        let a = self.heights[y * 33 + x];
        let b = self.heights[y * 33 + x + 1];
        let c = self.heights[(y + 1) * 33 + x];
        let d = self.heights[(y + 1) * 33 + x + 1];
        // Match build_terrain_quadrant_mesh's [a,b,c], [b,d,c] diagonal, not bilinear sampling.
        let ground = if u + v <= 1.0 {
            a * (1.0 - u - v) + b * u + c * v
        } else {
            b * (1.0 - v) + c * (1.0 - u) + d * (u + v - 1.0)
        };
        // The shoreline itself is dry. Only positive depth above the LAND mesh is coverage.
        (ground < self.surface.height).then_some(WaterSample {
            cell_id: self.cell_id,
            surface: self.surface,
        })
    }
}

/// Identity, height and catalog type of the selected existing flat-water body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterSample {
    pub cell_id: u32,
    pub surface: FlatWater,
}

/// Query horizontal coverage in one worldspace, independent of the point's altitude.
/// Pass canonical Creation coordinates (`WorldPosition::from_creation_units` or grid + local).
/// Cells own [min, max) on both axes, including negative coordinates. Rebased render coordinates
/// must first be converted back to Creation coordinates with their render origin.
///
/// Overlap policy: the highest covering surface wins; equal heights choose the highest CELL
/// FormID, independent of iteration order. This is an explicit flat-water policy, not a claim
/// about native Skyrim overlap rules. A caller decides whether its point is below the surface.
/// `None` means no coverage among the supplied committed bodies, not proof an unloaded cell is dry.
pub fn water_at_point<'a>(
    bodies: impl IntoIterator<Item = &'a FlatWaterBody>,
    worldspace_id: u32,
    point: WorldPosition,
) -> Option<WaterSample> {
    bodies
        .into_iter()
        .filter_map(|body| body.sample(worldspace_id, point))
        .max_by(|a, b| {
            a.surface
                .height
                .partial_cmp(&b.surface.height)
                .expect("validated finite water heights")
                .then(a.cell_id.cmp(&b.cell_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terrain(cell_id: u32, height: Option<f32>) -> TerrainSnapshot {
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![0.0; 33 * 33],
            normals: vec![],
            vertex_colors: vec![],
            layers: vec![],
            water_height: height,
            water_type_form_id: Some(cell_id + 100),
        }
    }

    fn body(terrain: &TerrainSnapshot, grid_x: i32, grid_y: i32) -> FlatWaterBody {
        FlatWaterBody::from_terrain(
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x,
                grid_y,
            },
            terrain,
        )
        .unwrap()
    }

    fn point(x: f32, y: f32, z: f32) -> WorldPosition {
        WorldPosition::from_creation_units(Vec3::new(x, y, z))
    }

    #[test]
    fn overlap_selects_highest_surface_then_cell_id_in_either_order() {
        let low = body(&terrain(1, Some(10.0)), 0, 0);
        let high = body(&terrain(2, Some(20.0)), 0, 0);
        let tie = body(&terrain(3, Some(20.0)), 0, 0);
        for bodies in [[&low, &high, &tie], [&tie, &high, &low]] {
            for z in [-100.0, 15.0, 100.0] {
                let hit = water_at_point(bodies, 60, point(64.0, 64.0, z)).unwrap();
                assert_eq!(hit.cell_id, 3);
                assert_eq!(hit.surface.height, 20.0);
                assert_eq!(hit.surface.water_type_form_id, Some(103));
            }
        }
        let mut dry_high = high.clone();
        dry_high.heights.fill(30.0);
        assert_eq!(
            water_at_point([&low, &dry_high], 60, point(64.0, 64.0, 0.0))
                .unwrap()
                .cell_id,
            1
        );
        let mut zero_land = terrain(4, Some(0.0));
        zero_land.heights.fill(-1.0);
        let zero = body(&zero_land, 0, 0);
        let mut negative_zero_land = terrain(5, Some(-0.0));
        negative_zero_land.heights.fill(-1.0);
        let negative_zero = body(&negative_zero_land, 0, 0);
        assert_eq!(
            water_at_point([&negative_zero, &zero], 60, point(64.0, 64.0, 0.0))
                .unwrap()
                .cell_id,
            5
        );
    }

    #[test]
    fn shoreline_and_dry_ground_follow_land_triangles() {
        let mut land = terrain(1, Some(10.0));
        // A ramp reaches the surface halfway across the cell.
        for y in 0..33 {
            for x in 0..33 {
                land.heights[y * 33 + x] = x as f32 * 20.0 / 32.0;
            }
        }
        let ramp = body(&land, 0, 0);
        assert!(water_at_point([&ramp], 60, point(2047.0, 64.0, 100.0)).is_some());
        assert!(water_at_point([&ramp], 60, point(2048.0, 64.0, -100.0)).is_none());
        assert!(water_at_point([&ramp], 60, point(3000.0, 64.0, -100.0)).is_none());

        // Saddle in the first LAND quad: a=d=20, b=c=0. Bilinear interpolation would
        // incorrectly put its centre at height 10; the actual b-c edge is at height 0.
        land.heights.fill(20.0);
        land.heights[1] = 0.0;
        land.heights[33] = 0.0;
        let saddle = body(&land, 0, 0);
        assert!(water_at_point([&saddle], 60, point(64.0, 64.0, 0.0)).is_some());
        assert!(water_at_point([&saddle], 60, point(80.0, 80.0, 0.0)).is_some());
        assert!(water_at_point([&saddle], 60, point(112.0, 112.0, 0.0)).is_none());
        assert!(water_at_point([&saddle], 60, point(16.0, 16.0, 0.0)).is_none());
    }

    #[test]
    fn negative_coordinates_and_boundaries_have_one_owner_and_worldspace() {
        let negative = body(&terrain(1, Some(10.0)), -1, -1);
        let east = body(&terrain(2, Some(20.0)), 0, -1);
        let north = body(&terrain(3, Some(30.0)), -1, 0);
        let bodies = [&negative, &east, &north];
        for (x, y, expected) in [
            (-4096.0, -4096.0, Some(1)),
            (-0.5, -0.5, Some(1)),
            (0.0, -0.5, Some(2)),
            (-0.5, 0.0, Some(3)),
            (0.0, 0.0, None),
            (-4096.5, -0.5, None),
            (-0.5, -4096.5, None),
        ] {
            assert_eq!(
                water_at_point(bodies, 60, point(x, y, 0.0)).map(|hit| hit.cell_id),
                expected,
                "({x}, {y})"
            );
        }
        assert!(water_at_point(bodies, 61, point(-1.0, -1.0, 0.0)).is_none());
        // No dependence on a nearby render origin or loss of the sub-cell position at large grids.
        let distant = body(&terrain(4, Some(10.0)), -1_000_000, 1_000_000);
        assert!(
            water_at_point(
                [&distant],
                60,
                WorldPosition {
                    grid: IVec2::new(-1_000_000, 1_000_000),
                    local: Vec3::new(0.5, 0.5, 0.0),
                }
            )
            .is_some()
        );
    }

    #[test]
    fn absent_invalid_and_unsupported_data_do_not_invent_coverage() {
        for height in [
            None,
            Some(f32::MAX),
            Some(-f32::MAX),
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(1.0e7),
        ] {
            assert!(FlatWater::from_terrain(&terrain(1, height)).is_none());
        }
        let mut land = terrain(1, Some(-11592.0));
        assert_eq!(FlatWater::from_terrain(&land).unwrap().height, -11592.0);
        land.heights.fill(-12000.0);
        let sea = body(&land, 0, 0);
        assert_eq!(
            water_at_point([&sea], 60, point(1.0, 1.0, 0.0))
                .unwrap()
                .surface
                .height,
            -11592.0
        );
        land.water_height = Some(10.0);
        land.water_type_form_id = None;
        let no_type = body(&land, 0, 0);
        assert_eq!(
            water_at_point([&no_type], 60, point(1.0, 1.0, 0.0))
                .unwrap()
                .surface
                .water_type_form_id,
            None
        );
        assert!(FlatWaterBody::from_terrain(CellKey::Interior(1), &land).is_none());
        land.heights[0] = f32::NAN;
        assert!(
            FlatWaterBody::from_terrain(
                CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 0,
                    grid_y: 0
                },
                &land
            )
            .is_none()
        );
        land.heights.clear();
        assert!(
            FlatWaterBody::from_terrain(
                CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 0,
                    grid_y: 0
                },
                &land
            )
            .is_none()
        );
        assert!(water_at_point(std::iter::empty(), 60, point(1.0, 1.0, 0.0)).is_none());
        for local in [
            Vec3::splat(f32::NAN),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(CELL_SIZE, 0.0, 0.0),
        ] {
            assert!(
                water_at_point(
                    [&no_type],
                    60,
                    WorldPosition {
                        grid: IVec2::ZERO,
                        local
                    }
                )
                .is_none()
            );
        }
    }
}
