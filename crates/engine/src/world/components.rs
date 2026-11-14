use bevy::prelude::*;

pub const CELL_SIZE: f32 = 4096.0;

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormId(pub u32);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRef(pub u32);

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct WorldPosition {
    pub grid: IVec2,
    pub local: Vec3,
}

impl WorldPosition {
    pub fn from_creation_units(position: Vec3) -> Self {
        let grid = IVec2::new(
            (position.x / CELL_SIZE).floor() as i32,
            (position.y / CELL_SIZE).floor() as i32,
        );
        Self {
            grid,
            local: Vec3::new(
                position.x - grid.x as f32 * CELL_SIZE,
                position.y - grid.y as f32 * CELL_SIZE,
                position.z,
            ),
        }
    }

    pub fn relative_to(self, origin: IVec2) -> Vec3 {
        self.local
            + Vec3::new(
                (self.grid.x - origin.x) as f32 * CELL_SIZE,
                (self.grid.y - origin.y) as f32 * CELL_SIZE,
                0.0,
            )
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct WorldTransform(pub Mat4);

#[derive(Component, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeshHandle(pub String);

#[derive(Component, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MaterialHandle(pub String);

#[derive(Component, Debug, Clone, Copy)]
pub struct InstanceBounds {
    pub min: Vec3,
    pub max: Vec3,
}

/// Aggregate model-space bounds produced by the converter after applying the
/// complete glTF node hierarchy. Runtime validation compares these bounds with
/// the meshes Bevy actually spawned before an instance is accepted.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct ExpectedModelBounds {
    pub min: Vec3,
    pub max: Vec3,
}

impl ExpectedModelBounds {
    pub fn new(min: Vec3, max: Vec3) -> Option<Self> {
        (min.is_finite()
            && max.is_finite()
            && min.cmple(max).all()
            && (max - min).max_element() > f32::EPSILON)
            .then_some(Self { min, max })
    }
}

impl InstanceBounds {
    pub fn transformed(min: Vec3, max: Vec3, transform: Mat4) -> Self {
        let mut output_min = Vec3::splat(f32::INFINITY);
        let mut output_max = Vec3::splat(f32::NEG_INFINITY);
        for x in [min.x, max.x] {
            for y in [min.y, max.y] {
                for z in [min.z, max.z] {
                    let point = transform.transform_point3(Vec3::new(x, y, z));
                    output_min = output_min.min(point);
                    output_max = output_max.max(point);
                }
            }
        }
        Self {
            min: output_min,
            max: output_max,
        }
    }
}

#[derive(Component, Clone, bevy::render::extract_component::ExtractComponent)]
pub struct StreamingCamera;

#[derive(Component)]
pub struct StreamedCellRoot;

#[derive(Component, Debug, Clone, Copy)]
pub struct ExteriorCellGrid(pub IVec2);

#[derive(Component)]
pub struct TerrainPatch;

#[derive(Component)]
pub struct WaterSurface;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_render_coordinates_near_the_origin() {
        let position = WorldPosition::from_creation_units(Vec3::new(20_500.0, -8_100.0, 12.0));
        let relative = position.relative_to(IVec2::new(5, -2));
        assert!(relative.x.abs() <= CELL_SIZE);
        assert!(relative.y.abs() <= CELL_SIZE);
        assert_eq!(relative.z, 12.0);
    }

    #[test]
    fn transforms_all_aabb_corners_for_rotation_and_scale() {
        let transform = Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 3.0, 4.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(10.0, 20.0, 30.0),
        );
        let bounds = InstanceBounds::transformed(Vec3::splat(-1.0), Vec3::splat(1.0), transform);
        assert!((bounds.min - Vec3::new(6.0, 17.0, 28.0)).length() < 0.001);
        assert!((bounds.max - Vec3::new(14.0, 23.0, 32.0)).length() < 0.001);
    }

    #[test]
    fn rejects_invalid_expected_model_bounds() {
        assert!(ExpectedModelBounds::new(Vec3::ZERO, Vec3::ONE).is_some());
        assert!(ExpectedModelBounds::new(Vec3::ONE, Vec3::ZERO).is_none());
        assert!(ExpectedModelBounds::new(Vec3::ZERO, Vec3::ZERO).is_none());
        assert!(ExpectedModelBounds::new(Vec3::ZERO, Vec3::splat(f32::NAN)).is_none());
    }
}
