//! Deterministic Skyrim SE plugin (`.esm`) fixtures.
//!
//! The writer emits a minimal worldspace with exterior cells, terrain, a
//! static, a texture set and landscape texture, plus one placement reference
//! per cell. A spec can also carry one interior cell joined to an exterior
//! cell by a reciprocal pair of load doors, which is what a caller needs to
//! exercise interiors, `XTEL` door links and cell-to-cell crossings without a
//! local game installation. A spec can instead carry one `LIGH` base record and
//! the single reference that places it, which is what a caller needs to exercise
//! point lights and the `XRDS` radius override a light reference carries without
//! a local game installation. Only the record types consumed by the converter's
//! ESM parser, exporter and cell cache are produced.
//!
//! The `DOOR` bases carry a `MODL` the way retail data does, but the converter's
//! exporter fills its `statics` table from `STAT`, `MSTT` and `FURN` only, so a
//! reference that places one of them exports with no model at all until that
//! changes; see [`Door::model_path`].

use crate::path::split_asset_name;
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

const TES4_FORM_ID: u32 = 0;
const WRLD_FORM_ID: u32 = 0x0000_0001;
const TXST_FORM_ID: u32 = 0x0000_0002;
const STAT_FORM_ID: u32 = 0x0000_0003;
const LTEX_FORM_ID: u32 = 0x0000_0004;
/// The `DOOR` base record the exterior door of an [`Interior`] places.
const EXTERIOR_DOOR_FORM_ID: u32 = 0x0000_0005;
/// The `DOOR` base record the interior door of an [`Interior`] places.
const INTERIOR_DOOR_FORM_ID: u32 = 0x0000_0006;
/// The `LIGH` base record a [`Light`]'s reference places.
const LIGHT_FORM_ID: u32 = 0x0000_0007;
const CELL_BASE_FORM_ID: u32 = 0x0000_0010;
const CELL_FORM_STRIDE: u32 = 0x10;
/// A door reference's offset inside its cell's block of [`CELL_FORM_STRIDE`]
/// FormIDs: past the cell itself, its `LAND` and the static reference.
const DOOR_REF_OFFSET: u32 = 3;
/// A light reference's offset in the same block, past the door reference.
const LIGHT_REF_OFFSET: u32 = 4;
/// The interior cell's FormID, past every id [`MAX_CELLS`] exterior cells can
/// hand out.
const INTERIOR_CELL_FORM_ID: u32 = 0x0001_0000;
const INTERIOR_DOOR_REF_FORM_ID: u32 = INTERIOR_CELL_FORM_ID + 1;
const LAND_SIDE: usize = 33;
const CELL_SIZE: f32 = 4096.0;
const RECORD_VERSION: u16 = 44;
const HEADER_RECORD_SIZE: usize = 24;
const GROUP_HEADER_SIZE: usize = 24;
const MAX_CELLS: usize = 0x0f00;
/// The highest FormID [`MAX_CELLS`] exterior cells can hand out is the last
/// cell's light reference; every exterior id has to stay below the interior
/// cell's own block, or an interior record would collide with an exterior one.
const _: () = assert!(
    CELL_BASE_FORM_ID + (MAX_CELLS as u32 - 1) * CELL_FORM_STRIDE + LIGHT_REF_OFFSET
        < INTERIOR_CELL_FORM_ID,
    "the exterior FormID block has grown into the interior block"
);
/// `XTEL`'s length: the destination reference's FormID, the arrival position
/// and rotation as six little-endian `f32`s, then a four-byte flag word.
const XTEL_SIZE: usize = 32;
/// A `LIGH` `DATA` subrecord's length, the layout every one of the 435 `LIGH`
/// records in `Skyrim.esm` carries: time (i32), radius (u32), colour (RGB plus
/// one unused byte), flags (u32), falloff exponent (f32), then FOV, near clip,
/// flicker period, flicker intensity amplitude, flicker movement amplitude,
/// value (u32) and weight (f32). The converter reads the first four fields and
/// the engine the `FNAM` fade; the fields after the falloff exponent are
/// written as zero.
const LIGHT_DATA_SIZE: usize = 48;
/// `CELL` `DATA` flag `0x01`: the cell is an interior, so it has no grid square
/// and belongs to no worldspace.
const INTERIOR_CELL_FLAG: u8 = 0x01;

/// `DOOR` `FNAM` flag `0x02`: an auto-load door, which crosses the moment an
/// actor walks into it rather than when the use key is pressed.
pub const AUTO_LOAD_FLAG: u8 = 0x02;

/// One exterior cell of the generated worldspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Grid X coordinate.
    pub grid_x: i32,
    /// Grid Y coordinate.
    pub grid_y: i32,
}

/// A load door: the `DOOR` base record a reference places, where that reference
/// stands, and the arrival frame the door's own `XTEL` carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Door<'a> {
    /// `EDID` of the `DOOR` base record. The two doors of one [`Interior`] need
    /// two distinct editor ids, because they are two base records.
    pub editor_id: &'a str,
    /// `MODL` model path of the `DOOR` base record.
    ///
    /// The path is written to the base record, but the converter's exporter
    /// fills its `statics` table from `STAT`, `MSTT` and `FURN` records only,
    /// so a `REFR` that places this door exports as a reference without a model:
    /// `world-inspect` counts it under `references_without_model`, its entry in
    /// an `assets` listing never appears, and its mesh never reaches the GLB
    /// pipeline. That is a converter gap, not a fixture one - the fixture writes
    /// the `MODL` a retail plugin carries - and it stays until the exporter
    /// learns `DOOR`. `crates/converter/tests/fixture_interior_pipeline.rs`
    /// asserts the resulting count so the gap cannot go quiet.
    pub model_path: &'a str,
    /// `FNAM` flags of the `DOOR` base record; [`AUTO_LOAD_FLAG`] marks an
    /// auto-load door.
    pub flags: u8,
    /// `DATA` position of the reference, in Creation units.
    pub position: [f32; 3],
    /// `DATA` rotation of the reference, in radians.
    pub rotation: [f32; 3],
    /// Arrival position this door's own `XTEL` stores: where the player lands
    /// after using the door, expressed in the destination cell. Deliberately
    /// not the destination door's position - the game stores its own frame,
    /// and the two differ by tens to hundreds of units in retail data.
    pub arrival_position: [f32; 3],
    /// Arrival rotation this door's own `XTEL` stores, in radians.
    pub arrival_rotation: [f32; 3],
}

/// An interior cell joined to one exterior cell of the worldspace by a
/// reciprocal pair of load doors: each door's `XTEL` names the other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interior<'a> {
    /// `EDID` of the interior cell.
    pub editor_id: &'a str,
    /// `FULL` display name of the interior cell.
    pub full_name: &'a str,
    /// The exterior cell the outside door stands in; one of
    /// [`Plugin::cells`].
    pub exterior_cell: Cell,
    /// The door in the exterior cell, leading in.
    pub outside: Door<'a>,
    /// The door in the interior cell, leading back out.
    pub inside: Door<'a>,
}

/// A `LIGH` base record and the single reference that places it.
///
/// The reference carries the light's position and rotation and its own `XRDS`
/// radius override, [`Light::radius_override`], which wins over the base
/// record's radius wherever both are read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Light<'a> {
    /// `EDID` of the `LIGH` base record.
    pub editor_id: &'a str,
    /// `MODL` model path of the `LIGH` base record; `None` writes no `MODL`,
    /// the shape a light with no visible mesh has. The exporter writes a
    /// `statics` row for a `LIGH` with a model (the lamp's mesh) and none for
    /// one without; either way the light itself goes into the `lights` table.
    pub model_path: Option<&'a str>,
    /// The exterior cell the reference stands in; one of [`Plugin::cells`].
    pub cell: Cell,
    /// `DATA` time (i32). `-1` is what retail data writes for a light that is
    /// always on (`DefaultTorch01NS_FastSaturate`, `000CB3B0`); the converter
    /// stores no time.
    pub time: i32,
    /// `DATA` radius in Creation units. A `u32`, which the converter widens to
    /// a float: a radius of 256 reads as 256.0, where the same four bytes read
    /// as an `f32` are a denormal.
    pub radius: u32,
    /// `DATA` colour, RGB. The byte after it is unused and written as zero.
    pub color: [u8; 3],
    /// `DATA` flags (u32). Clear, so the light is on and positive: the engine
    /// renders no light that sets UESP's off-by-default bit (`0x20`) or its
    /// negative bit (`0x04`), and a fixture that set either would be dark.
    pub flags: u32,
    /// `DATA` falloff exponent (f32). The converter reads it; the engine loads
    /// it and does not apply it.
    pub falloff: f32,
    /// `FNAM` fade (f32), the brightness knob of the converted light.
    pub fade: f32,
    /// `DATA` position of the reference, in Creation units.
    pub position: [f32; 3],
    /// `DATA` rotation of the reference, in radians.
    pub rotation: [f32; 3],
    /// `XRDS` radius of the reference, in the same Creation units as
    /// [`Light::radius`] and deliberately not equal to it, the way retail data
    /// differs: 10,810 of the 12,148 `LIGH` references in `Skyrim.esm` carry
    /// an override.
    /// A single little-endian `f32`, which may be negative.
    pub radius_override: f32,
}

/// Description of a generated plugin.
///
/// The worldspace and its exterior cells, with the assets they reference. An
/// interior cell and its load doors are described by [`Interior`] and written
/// by [`plugin_with_interior`].
#[derive(Debug, Clone, Copy)]
pub struct Plugin<'a> {
    /// Author string stored in the `TES4` header.
    pub author: &'a str,
    /// Editor id of the generated worldspace.
    pub worldspace: &'a str,
    /// Exterior cells to generate; all get flat terrain.
    pub cells: &'a [Cell],
    /// Model path referenced by the generated static.
    pub model_path: &'a str,
    /// Diffuse texture path referenced by the generated texture set.
    pub diffuse: &'a str,
    /// Normal texture path referenced by the generated texture set.
    pub normal_texture: &'a str,
}

/// Generates a minimal Skyrim SE plugin.
pub fn plugin(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    write_plugin(spec, None, None)
}

/// Generates the same plugin as [`plugin`], with `interior` and its two load
/// doors.
///
/// The interior part is written after the exterior world, and the exterior
/// records above it are exactly the ones [`plugin`] writes: the only byte an
/// interior moves is the world group's own size field, which grows by the
/// reference of the door standing in the exterior cell.
pub fn plugin_with_interior(spec: &Plugin<'_>, interior: &Interior<'_>) -> Result<Vec<u8>> {
    write_plugin(spec, Some(interior), None)
}

/// Generates the same plugin as [`plugin`], with `light`'s `LIGH` base record
/// and the one reference that places it.
///
/// The base record is written after the exterior world, like the `DOOR` records
/// of [`plugin_with_interior`], and the reference joins the static placement in
/// the cell's own children group, so the exterior records the two share keep
/// their order and only the world group's size field moves.
pub fn plugin_with_lights(spec: &Plugin<'_>, light: &Light<'_>) -> Result<Vec<u8>> {
    write_plugin(spec, None, Some(light))
}

fn write_plugin(
    spec: &Plugin<'_>,
    interior: Option<&Interior<'_>>,
    light: Option<&Light<'_>>,
) -> Result<Vec<u8>> {
    validate(spec, interior, light)?;

    let mut bytes = header_record(spec)?;
    bytes.extend_from_slice(&texture_set_record(spec)?);
    bytes.extend_from_slice(&static_record(spec)?);
    bytes.extend_from_slice(&landscape_texture_record()?);
    bytes.extend_from_slice(&worldspace_record(spec)?);

    let mut world_children = Vec::new();
    for (index, cell) in spec.cells.iter().enumerate() {
        let cell_form_id = cell_form_id(index)?;
        world_children.extend_from_slice(&cell_record(cell_form_id, cell)?);
        let mut children = Vec::new();
        children.extend_from_slice(&land_record(cell_form_id + 1)?);
        children.extend_from_slice(&reference_record(cell_form_id + 2, cell)?);
        if let Some(interior) = interior
            && interior.exterior_cell == *cell
        {
            children.extend_from_slice(&door_reference_record(
                cell_form_id + DOOR_REF_OFFSET,
                EXTERIOR_DOOR_FORM_ID,
                &interior.outside,
                INTERIOR_DOOR_REF_FORM_ID,
            )?);
        }
        if let Some(light) = light
            && light.cell == *cell
        {
            children.extend_from_slice(&light_reference_record(
                cell_form_id + LIGHT_REF_OFFSET,
                light,
            )?);
        }
        world_children.extend_from_slice(&group(8, cell_form_id, &children)?);
    }
    bytes.extend_from_slice(&group(1, WRLD_FORM_ID, &world_children)?);

    if let Some(interior) = interior {
        let index = exterior_cell_index(spec, interior)?;
        let exterior_door_ref = cell_form_id(index)? + DOOR_REF_OFFSET;
        bytes.extend_from_slice(&door_record(EXTERIOR_DOOR_FORM_ID, &interior.outside)?);
        bytes.extend_from_slice(&door_record(INTERIOR_DOOR_FORM_ID, &interior.inside)?);
        bytes.extend_from_slice(&interior_group(interior, exterior_door_ref)?);
    }
    if let Some(light) = light {
        bytes.extend_from_slice(&light_record(LIGHT_FORM_ID, light)?);
    }
    Ok(bytes)
}

fn validate(
    spec: &Plugin<'_>,
    interior: Option<&Interior<'_>>,
    light: Option<&Light<'_>>,
) -> Result<()> {
    for (label, value) in [("author", spec.author), ("worldspace", spec.worldspace)] {
        ensure!(!value.is_empty(), "ESM {label} is empty");
        ensure!(
            value.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "ESM {label} is not printable ASCII: {value:?}"
        );
    }
    ensure!(!spec.cells.is_empty(), "ESM worldspace has no cells");
    ensure!(
        spec.cells.len() <= MAX_CELLS,
        "ESM cell count exceeds {MAX_CELLS}"
    );
    split_asset_name(spec.model_path, "ESM model")?;
    split_asset_name(spec.diffuse, "ESM diffuse")?;
    split_asset_name(spec.normal_texture, "ESM normal")?;

    if let Some(interior) = interior {
        // The outside door is a reference of an exterior cell, so that cell has
        // to be one of the generated ones.
        exterior_cell_index(spec, interior)?;
        for (label, value) in [
            ("interior editor id", interior.editor_id),
            ("interior display name", interior.full_name),
            ("exterior door editor id", interior.outside.editor_id),
            ("interior door editor id", interior.inside.editor_id),
        ] {
            ensure!(!value.is_empty(), "ESM {label} is empty");
            ensure!(
                value.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
                "ESM {label} is not printable ASCII: {value:?}"
            );
        }
        ensure!(
            interior.outside.editor_id != interior.inside.editor_id,
            "ESM door pair reuses the editor id {:?}; two DOOR records need two names",
            interior.outside.editor_id
        );
        for (side, door) in [
            ("exterior", &interior.outside),
            ("interior", &interior.inside),
        ] {
            split_asset_name(door.model_path, &format!("ESM {side} door model"))?;
            for (field, values) in [
                ("position", door.position),
                ("rotation", door.rotation),
                ("arrival position", door.arrival_position),
                ("arrival rotation", door.arrival_rotation),
            ] {
                ensure!(
                    values.iter().all(|value| value.is_finite()),
                    "ESM {side} door {field} is not finite: {values:?}"
                );
            }
        }
    }

    if let Some(light) = light {
        // The light's reference is a reference of an exterior cell, so that
        // cell has to be one of the generated ones.
        light_cell_index(spec, light)?;
        ensure!(!light.editor_id.is_empty(), "ESM light editor id is empty");
        ensure!(
            light
                .editor_id
                .bytes()
                .all(|byte| (0x20..0x7f).contains(&byte)),
            "ESM light editor id is not printable ASCII: {:?}",
            light.editor_id
        );
        if let Some(model_path) = light.model_path {
            split_asset_name(model_path, "ESM light model")?;
        }
        for (field, values) in [("position", light.position), ("rotation", light.rotation)] {
            ensure!(
                values.iter().all(|value| value.is_finite()),
                "ESM light {field} is not finite: {values:?}"
            );
        }
        for (field, value) in [
            ("falloff", light.falloff),
            ("fade", light.fade),
            ("radius override", light.radius_override),
        ] {
            ensure!(
                value.is_finite(),
                "ESM light {field} is not finite: {value:?}"
            );
        }
    }
    Ok(())
}

/// The index of the exterior cell an interior's door stands in.
fn exterior_cell_index(spec: &Plugin<'_>, interior: &Interior<'_>) -> Result<usize> {
    spec.cells
        .iter()
        .position(|cell| *cell == interior.exterior_cell)
        .ok_or_else(|| {
            eyre!(
                "ESM interior {} is not attached to a generated exterior cell",
                interior.editor_id
            )
        })
}

/// The index of the exterior cell a light's reference stands in.
fn light_cell_index(spec: &Plugin<'_>, light: &Light<'_>) -> Result<usize> {
    spec.cells
        .iter()
        .position(|cell| *cell == light.cell)
        .ok_or_else(|| {
            eyre!(
                "ESM light {} is not attached to a generated exterior cell",
                light.editor_id
            )
        })
}

fn header_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    let mut hedr = Vec::with_capacity(12);
    hedr.extend_from_slice(&1.7f32.to_le_bytes());
    hedr.extend_from_slice(&0u32.to_le_bytes());
    hedr.extend_from_slice(&0u32.to_le_bytes());
    record(
        *b"TES4",
        TES4_FORM_ID,
        &[(*b"HEDR", hedr), (*b"CNAM", cstring(spec.author))],
    )
}

fn texture_set_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"TXST",
        TXST_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedTextures")),
            (*b"TX00", cstring(spec.diffuse)),
            (*b"TX01", cstring(spec.normal_texture)),
        ],
    )
}

fn static_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"STAT",
        STAT_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedStatic")),
            (*b"MODL", cstring(spec.model_path)),
        ],
    )
}

/// A `DOOR` base record: its editor id, the model a reference of it draws, and
/// its one-byte `FNAM` flags.
fn door_record(form_id: u32, door: &Door<'_>) -> Result<Vec<u8>> {
    record(
        *b"DOOR",
        form_id,
        &[
            (*b"EDID", cstring(door.editor_id)),
            (*b"MODL", cstring(door.model_path)),
            (*b"FNAM", vec![door.flags]),
        ],
    )
}

/// A `LIGH` base record: its editor id, an optional `MODL`, the [`LIGHT_DATA_SIZE`]
/// `DATA` layout and the four-byte `FNAM` fade.
fn light_record(form_id: u32, light: &Light<'_>) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(LIGHT_DATA_SIZE);
    data.extend_from_slice(&light.time.to_le_bytes());
    data.extend_from_slice(&light.radius.to_le_bytes());
    data.extend_from_slice(&light.color);
    // The colour's fourth byte is unused; the game writes it as zero.
    data.push(0);
    data.extend_from_slice(&light.flags.to_le_bytes());
    data.extend_from_slice(&light.falloff.to_le_bytes());
    // FOV, near clip, flicker period, flicker intensity amplitude, flicker
    // movement amplitude, value and weight: the fields behind the ones the
    // converter reads, so the fixture leaves them clear rather than inventing
    // values no consumer can check.
    data.extend(std::iter::repeat_n(0u8, LIGHT_DATA_SIZE - data.len()));
    let mut subrecords = vec![(*b"EDID", cstring(light.editor_id))];
    if let Some(model_path) = light.model_path {
        subrecords.push((*b"MODL", cstring(model_path)));
    }
    subrecords.push((*b"DATA", data));
    subrecords.push((*b"FNAM", light.fade.to_le_bytes().to_vec()));
    record(*b"LIGH", form_id, &subrecords)
}

fn landscape_texture_record() -> Result<Vec<u8>> {
    record(
        *b"LTEX",
        LTEX_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedLandscape")),
            (*b"TNAM", TXST_FORM_ID.to_le_bytes().to_vec()),
            (*b"HNAM", 0u16.to_le_bytes().to_vec()),
        ],
    )
}

fn worldspace_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"WRLD",
        WRLD_FORM_ID,
        &[(*b"EDID", cstring(spec.worldspace))],
    )
}

fn cell_record(form_id: u32, cell: &Cell) -> Result<Vec<u8>> {
    let mut xclc = Vec::with_capacity(8);
    xclc.extend_from_slice(&cell.grid_x.to_le_bytes());
    xclc.extend_from_slice(&cell.grid_y.to_le_bytes());
    record(
        *b"CELL",
        form_id,
        &[(*b"EDID", cstring("GeneratedCell")), (*b"XCLC", xclc)],
    )
}

/// The interior `CELL`: an editor id, a `FULL` display name and a `DATA` flag
/// byte whose [`INTERIOR_CELL_FLAG`] bit marks it as an interior. It carries no
/// `XCLC`, because an interior has no grid square, and it sits in no worldspace
/// group. The converter stores it with a NULL grid and worldspace and reads its
/// editor id as the cell's name.
fn interior_cell_record(interior: &Interior<'_>) -> Result<Vec<u8>> {
    record(
        *b"CELL",
        INTERIOR_CELL_FORM_ID,
        &[
            (*b"EDID", cstring(interior.editor_id)),
            (*b"FULL", cstring(interior.full_name)),
            (*b"DATA", vec![INTERIOR_CELL_FLAG]),
        ],
    )
}

fn land_record(form_id: u32) -> Result<Vec<u8>> {
    let mut vhgt = Vec::with_capacity(4 + LAND_SIDE * LAND_SIDE + 3);
    vhgt.extend_from_slice(&0.0f32.to_le_bytes());
    vhgt.extend(std::iter::repeat_n(0u8, LAND_SIDE * LAND_SIDE));
    vhgt.extend_from_slice(&[0u8; 3]);
    let mut btxt = Vec::with_capacity(8);
    btxt.extend_from_slice(&LTEX_FORM_ID.to_le_bytes());
    btxt.extend_from_slice(&[0u8, 0u8]);
    btxt.extend_from_slice(&0u16.to_le_bytes());
    record(*b"LAND", form_id, &[(*b"VHGT", vhgt), (*b"BTXT", btxt)])
}

fn reference_record(form_id: u32, cell: &Cell) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(24);
    let center_x = cell.grid_x as f32 * CELL_SIZE + CELL_SIZE * 0.5;
    let center_y = cell.grid_y as f32 * CELL_SIZE + CELL_SIZE * 0.5;
    for value in [center_x, center_y, 0.0, 0.0, 0.0, 0.0] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    record(
        *b"REFR",
        form_id,
        &[
            (*b"NAME", STAT_FORM_ID.to_le_bytes().to_vec()),
            (*b"DATA", data),
        ],
    )
}

/// A `REFR` that places `base_form_id`'s `DOOR` record, with `door`'s
/// transform and the `XTEL` the door crosses on: the destination reference's
/// FormID, the arrival position and rotation, and a four-byte flag word -
/// [`XTEL_SIZE`] bytes, no flag bits set.
///
/// `destination_ref_id` is written as the fixture's own local id, and the
/// converter's load-order remap leaves it that way: remapping rewrites only the
/// subrecords `is_form_id_subrecord` recognises as 4-byte FormIDs, and `XTEL`
/// is not one of them, so the id reaches a database unchanged. That is correct
/// only while this plugin owns load-order index 0, the single-plugin case
/// `dummy-content gen` writes - a second plugin needs the remap extended to
/// `XTEL`. Nothing consumes `XTEL` yet either, so the link is only as good as
/// the fixture's own reader (`crates/converter/tests/fixture_doors.rs`).
fn door_reference_record(
    form_id: u32,
    base_form_id: u32,
    door: &Door<'_>,
    destination_ref_id: u32,
) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(24);
    for value in door.position.iter().chain(door.rotation.iter()) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    let mut xtel = Vec::with_capacity(XTEL_SIZE);
    xtel.extend_from_slice(&destination_ref_id.to_le_bytes());
    xtel.extend_from_slice(&floats(&door.arrival_position));
    xtel.extend_from_slice(&floats(&door.arrival_rotation));
    xtel.extend_from_slice(&0u32.to_le_bytes());
    record(
        *b"REFR",
        form_id,
        &[
            (*b"NAME", base_form_id.to_le_bytes().to_vec()),
            (*b"DATA", data),
            (*b"XTEL", xtel),
        ],
    )
}

/// A `REFR` that places [`LIGHT_FORM_ID`]: `light`'s transform, and the
/// four-byte `XRDS` radius override the reference carries.
///
/// `XRDS` is a single little-endian `f32`, so the load-order remap leaves it
/// alone: it rewrites only the subrecords it recognises as 4-byte FormIDs, and
/// `XRDS` is not one of them. The value therefore reaches a database exactly as
/// written, including when this plugin is not the first.
fn light_reference_record(form_id: u32, light: &Light<'_>) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(24);
    for value in light.position.iter().chain(light.rotation.iter()) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    record(
        *b"REFR",
        form_id,
        &[
            (*b"NAME", LIGHT_FORM_ID.to_le_bytes().to_vec()),
            (*b"DATA", data),
            (*b"XRDS", light.radius_override.to_le_bytes().to_vec()),
        ],
    )
}

/// The interior cell group: `GRUP` type 2 (interior block) around type 3
/// (sub-block), the nesting a plugin puts an interior cell in, with the cell's
/// own type 6 (cell children) group holding the return door as a persistent
/// reference. Both block labels are 0: they only sort interior cells into
/// blocks for a reader that groups them, and the converter's group walk takes
/// an owned cell from the type 6 label below, never from a block label.
fn interior_group(interior: &Interior<'_>, exterior_door_ref_form_id: u32) -> Result<Vec<u8>> {
    let mut cell_children = Vec::new();
    cell_children.extend_from_slice(&door_reference_record(
        INTERIOR_DOOR_REF_FORM_ID,
        INTERIOR_DOOR_FORM_ID,
        &interior.inside,
        exterior_door_ref_form_id,
    )?);
    let mut sub_block = interior_cell_record(interior)?;
    sub_block.extend_from_slice(&group(
        6,
        INTERIOR_CELL_FORM_ID,
        &group(8, INTERIOR_CELL_FORM_ID, &cell_children)?,
    )?);
    group(2, 0, &group(3, 0, &sub_block)?)
}

fn floats(values: &[f32; 3]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn cell_form_id(index: usize) -> Result<u32> {
    let offset = u32::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(CELL_FORM_STRIDE))
        .ok_or_else(|| eyre!("ESM cell index overflow"))?;
    CELL_BASE_FORM_ID
        .checked_add(offset)
        .ok_or_else(|| eyre!("ESM cell form id overflow"))
}

fn record(tag: [u8; 4], form_id: u32, subrecords: &[([u8; 4], Vec<u8>)]) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for (sub_tag, data) in subrecords {
        let length = u16::try_from(data.len())
            .map_err(|_| eyre!("ESM subrecord {:?} exceeds 65535 bytes", sub_tag))?;
        payload.extend_from_slice(sub_tag);
        payload.extend_from_slice(&length.to_le_bytes());
        payload.extend_from_slice(data);
    }
    let mut bytes = Vec::with_capacity(HEADER_RECORD_SIZE + payload.len());
    bytes.extend_from_slice(&tag);
    bytes.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| eyre!("ESM record payload overflow"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&form_id.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&RECORD_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn group(group_type: i32, label: u32, content: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        content.len() < u32::MAX as usize,
        "ESM group payload overflow"
    );
    let data_size = u32::try_from(content.len())
        .ok()
        .and_then(|length| length.checked_add(GROUP_HEADER_SIZE as u32))
        .ok_or_else(|| eyre!("ESM group payload overflow"))?;
    let mut bytes = Vec::with_capacity(GROUP_HEADER_SIZE + content.len());
    bytes.extend_from_slice(b"GRUP");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    bytes.extend_from_slice(&label.to_le_bytes());
    bytes.extend_from_slice(&group_type.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(content);
    Ok(bytes)
}

fn cstring(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.len() + 1);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes
}

/// The exterior cell the `--with-interior` preset hangs its door on: grid
/// (0, 0) of the generated worldspace, the square the crate's other fixtures
/// place their static in.
pub const PRESET_EXTERIOR_CELL: Cell = Cell {
    grid_x: 0,
    grid_y: 0,
};

/// The interior cell and reciprocal load door pair the `--with-interior`
/// preset writes, and the fixture `crates/converter/tests/fixture_doors.rs`
/// converts.
///
/// The outside door is an auto-load door ([`AUTO_LOAD_FLAG`], editor id
/// `AutoLoadDoor01`) and the inside door is an ordinary one, so a fixture can
/// exercise both ways a load door opens. Neither door stands on the arrival
/// point that leads to it, as the game's own `XTEL` data does not. Both doors
/// draw the fixture's generated mesh ([`crate::layout::GENERATED_MODEL_PATH`],
/// the only model the crate's default data tree writes); to exercise a marker
/// model's own path a caller has to describe its own [`Interior`].
///
/// Both doors do carry a `MODL`, but the mesh never reaches the converted
/// world: the exporter fills `statics` from `STAT`, `MSTT` and `FURN` only, so
/// the two door references export without a model until that changes - see
/// [`Door::model_path`].
pub const PRESET_INTERIOR: Interior<'static> = Interior {
    editor_id: "GeneratedInterior",
    full_name: "Generated Interior",
    exterior_cell: PRESET_EXTERIOR_CELL,
    outside: Door {
        editor_id: "AutoLoadDoor01",
        model_path: crate::layout::GENERATED_MODEL_PATH,
        flags: AUTO_LOAD_FLAG,
        position: [2048.0, 1024.0, 0.0],
        rotation: [0.0, 0.0, 0.0],
        arrival_position: [128.0, 256.0, 0.0],
        arrival_rotation: [0.0, 0.0, 0.0],
    },
    inside: Door {
        editor_id: "GeneratedDoor01",
        model_path: crate::layout::GENERATED_MODEL_PATH,
        flags: 0,
        position: [128.0, 512.0, 0.0],
        rotation: [0.0, 0.0, 0.0],
        arrival_position: [2048.0, 512.0, 0.0],
        arrival_rotation: [0.0, 0.0, 0.0],
    },
};

/// The `LIGH` base record and the reference that places it, which the
/// `--with-lights` preset writes and `crates/converter/tests/fixture_lights.rs`
/// converts.
///
/// The values are plausible rather than retail: a radius of 512 Creation units,
/// the widest common `LIGH` radius, and a warm colour, with flags clear so the
/// light is on and positive (see [`Light::flags`]). The reference's `XRDS`
/// radius of 1024 is deliberately twice the base record's, so a reader that
/// picks up the override cannot be confused with one that picked up the base.
/// The base record carries the fixture's generated mesh as its `MODL`, so the
/// exporter writes a `statics` row for it as well as its `lights` row.
pub const PRESET_LIGHT: Light<'static> = Light {
    editor_id: "GeneratedLight01",
    model_path: Some(crate::layout::GENERATED_MODEL_PATH),
    cell: PRESET_EXTERIOR_CELL,
    time: -1,
    radius: 512,
    color: [216, 128, 39],
    flags: 0,
    falloff: 1.0,
    fade: 1.0,
    position: [1024.0, 2048.0, 128.0],
    rotation: [0.0, 0.0, 0.0],
    radius_override: 1024.0,
};

#[cfg(test)]
mod tests {
    use super::*;

    const CELLS: [Cell; 4] = [
        Cell {
            grid_x: 0,
            grid_y: 0,
        },
        Cell {
            grid_x: 1,
            grid_y: 0,
        },
        Cell {
            grid_x: 0,
            grid_y: 1,
        },
        Cell {
            grid_x: 1,
            grid_y: 1,
        },
    ];

    fn spec() -> Plugin<'static> {
        Plugin {
            author: crate::layout::GENERATED_AUTHOR,
            worldspace: crate::layout::GENERATED_WORLDSPACE,
            cells: &CELLS,
            model_path: crate::layout::GENERATED_MODEL_PATH,
            diffuse: crate::layout::GENERATED_DIFFUSE_PATH,
            normal_texture: crate::layout::GENERATED_NORMAL_PATH,
        }
    }

    /// Every group, record and subrecord tag in `bytes`, in file order.
    ///
    /// Walks the header sizes rather than matching four-byte windows, so a tag
    /// spelled inside a payload - an `EDID`, a `MODL` path, the bytes of an
    /// `f32` - can never be counted as a record.
    fn tags(bytes: &[u8]) -> Vec<[u8; 4]> {
        /// Subrecords of one record's payload: tag, `u16` length, contents.
        fn subrecords(payload: &[u8], found: &mut Vec<[u8; 4]>) {
            let mut offset = 0;
            while offset + 6 <= payload.len() {
                let tag: [u8; 4] = payload[offset..offset + 4].try_into().unwrap();
                let length = u16::from_le_bytes(payload[offset + 4..offset + 6].try_into().unwrap())
                    as usize;
                found.push(tag);
                offset += 6 + length;
            }
        }

        fn walk(bytes: &[u8], found: &mut Vec<[u8; 4]>) {
            let mut offset = 0;
            while offset + HEADER_RECORD_SIZE <= bytes.len() {
                let tag: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
                let size =
                    u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                found.push(tag);
                if tag == *b"GRUP" {
                    // A group's size counts its own 24-byte header, so the group
                    // ends at `offset + size` and its children start past the
                    // header; a record's size counts only its payload.
                    let children = bytes.get(offset + GROUP_HEADER_SIZE..offset + size);
                    let Some(children) = children else { return };
                    walk(children, found);
                    offset += size;
                } else {
                    let payload =
                        bytes.get(offset + HEADER_RECORD_SIZE..offset + HEADER_RECORD_SIZE + size);
                    if let Some(payload) = payload {
                        subrecords(payload, found);
                    }
                    offset += HEADER_RECORD_SIZE + size;
                }
            }
        }

        let mut found = Vec::new();
        walk(bytes, &mut found);
        found
    }

    /// How many tags in `bytes` are `tag`.
    fn count(bytes: &[u8], tag: &[u8; 4]) -> usize {
        tags(bytes).iter().filter(|found| *found == tag).count()
    }

    #[test]
    fn writes_tes4_header_and_groups() {
        let bytes = plugin(&spec()).unwrap();
        assert_eq!(&bytes[..4], b"TES4");
        let tags = tags(&bytes);
        for tag in [b"GRUP", b"WRLD", b"LAND"] {
            assert!(tags.contains(tag), "{tag:?} is missing from {tags:?}");
        }
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(plugin(&spec()).unwrap(), plugin(&spec()).unwrap());
        assert_eq!(
            plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap(),
            plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap()
        );
        assert_eq!(
            plugin_with_lights(&spec(), &PRESET_LIGHT).unwrap(),
            plugin_with_lights(&spec(), &PRESET_LIGHT).unwrap()
        );
    }

    /// FNV-1a of [`plugin`]'s output, recorded from the writer that the
    /// interiors commit left byte-identical.
    ///
    /// The guard used to re-run a copy of the pre-interior writer; the copy is
    /// gone, and this constant stands in for it. A deliberate change to the
    /// exterior bytes refreshes it in the same commit, which is what keeps the
    /// change visible.
    const EXTERIOR_ONLY_HASH: u64 = 0xECE9_D84B_E35F_6B24;

    /// FNV-1a over every byte of `bytes`.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    #[test]
    fn exterior_only_output_is_unchanged() {
        assert_eq!(
            fnv1a(&plugin(&spec()).unwrap()),
            EXTERIOR_ONLY_HASH,
            "the exterior-only bytes moved; if that is intended, refresh the constant"
        );
    }

    #[test]
    fn an_interior_is_appended_after_the_exterior_world() {
        let exterior_only = plugin(&spec()).unwrap();
        let with_interior = plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap();
        assert_eq!(count(&exterior_only, b"DOOR"), 0);
        assert_eq!(count(&exterior_only, b"XTEL"), 0);
        // Every record above the world group is byte-identical, and the interior
        // part starts at or after the point the exterior-only plugin ends.
        let world = with_interior
            .windows(4)
            .position(|window| window == b"WRLD")
            .expect("the WRLD record");
        assert_eq!(&with_interior[..world], &exterior_only[..world]);
        let doors = with_interior
            .windows(4)
            .position(|window| window == b"DOOR")
            .expect("the DOOR records");
        assert!(doors >= exterior_only.len());
        assert_eq!(count(&with_interior, b"DOOR"), 2, "one record per door");
        assert_eq!(count(&with_interior, b"XTEL"), 2, "one link per door");
        assert_eq!(count(&with_interior, b"FNAM"), 2, "one FNAM per door");
        assert_eq!(count(&with_interior, b"FULL"), 1, "the interior's name");
    }

    #[test]
    fn rejects_invalid_specs() {
        let mut invalid = spec();
        invalid.cells = &[];
        assert!(plugin(&invalid).is_err());
        let mut invalid = spec();
        invalid.model_path = "../escape.nif";
        assert!(plugin(&invalid).is_err());
        let mut invalid = spec();
        invalid.worldspace = "";
        assert!(plugin(&invalid).is_err());
    }

    #[test]
    fn a_light_is_appended_after_the_exterior_world() {
        let exterior_only = plugin(&spec()).unwrap();
        let with_light = plugin_with_lights(&spec(), &PRESET_LIGHT).unwrap();
        assert_eq!(count(&exterior_only, b"LIGH"), 0);
        assert_eq!(count(&exterior_only, b"XRDS"), 0);
        // Every record above the world group is byte-identical, and the base
        // record starts after the point the exterior-only plugin ends: the
        // reference it places sits inside a cell group the exterior world owns,
        // so the world group's own size field is the only byte between them that
        // moves.
        let world = with_light
            .windows(4)
            .position(|window| window == b"WRLD")
            .expect("the WRLD record");
        assert_eq!(&with_light[..world], &exterior_only[..world]);
        let light = with_light
            .windows(4)
            .position(|window| window == b"LIGH")
            .expect("the LIGH record");
        assert!(light >= exterior_only.len());
        assert_eq!(count(&with_light, b"LIGH"), 1, "one base record");
        assert_eq!(count(&with_light, b"XRDS"), 1, "one radius override");
        assert_eq!(count(&with_light, b"FNAM"), 1, "one fade");
        assert_eq!(
            count(&with_light, b"REFR"),
            5,
            "one static per cell and the light"
        );
        assert_eq!(count(&with_light, b"DOOR"), 0, "no doors in this preset");
    }

    /// Every record's FormID in `bytes`, in file order, walking the header
    /// sizes the way [`tags`] does.
    fn form_ids(bytes: &[u8]) -> Vec<u32> {
        fn walk(bytes: &[u8], found: &mut Vec<u32>) {
            let mut offset = 0;
            while offset + HEADER_RECORD_SIZE <= bytes.len() {
                let tag: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
                let size =
                    u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                if tag == *b"GRUP" {
                    // A group header's own word at this offset is the group
                    // type, not a FormID.
                    let Some(children) = bytes.get(offset + GROUP_HEADER_SIZE..offset + size)
                    else {
                        return;
                    };
                    walk(children, found);
                    offset += size;
                } else {
                    found.push(u32::from_le_bytes(
                        bytes[offset + 12..offset + 16].try_into().unwrap(),
                    ));
                    offset += HEADER_RECORD_SIZE + size;
                }
            }
        }

        let mut found = Vec::new();
        walk(bytes, &mut found);
        found
    }

    /// An interior and a light are two extras the writer can put on one spec:
    /// their references take different offsets in the cell's FormID block
    /// ([`DOOR_REF_OFFSET`] and [`LIGHT_REF_OFFSET`]), so no two records of the
    /// combined plugin may share an id. The CLI exposes one preset at a time,
    /// which is why this is the only caller of the combination.
    #[test]
    fn an_interior_and_a_light_in_one_cell_get_distinct_form_ids() {
        let bytes = write_plugin(&spec(), Some(&PRESET_INTERIOR), Some(&PRESET_LIGHT)).unwrap();
        assert_eq!(count(&bytes, b"DOOR"), 2);
        assert_eq!(count(&bytes, b"LIGH"), 1);
        assert_eq!(count(&bytes, b"XTEL"), 2);
        assert_eq!(count(&bytes, b"XRDS"), 1);
        assert_eq!(
            count(&bytes, b"REFR"),
            7,
            "one static per cell, two doors and the light"
        );

        let mut ids = form_ids(&bytes);
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "two records share a FormID: {ids:?}");
    }

    #[test]
    fn rejects_invalid_lights() {
        for mutate in [
            (|light: &mut Light<'_>| light.editor_id = "") as fn(&mut Light<'_>),
            |light| light.editor_id = "bad\nlight",
            |light| light.model_path = Some("../escape.nif"),
            |light| {
                light.cell = Cell {
                    grid_x: 7,
                    grid_y: 7,
                }
            },
            |light| light.position = [f32::NAN, 0.0, 0.0],
            |light| light.rotation = [0.0, f32::INFINITY, 0.0],
            |light| light.falloff = f32::NAN,
            |light| light.fade = f32::NEG_INFINITY,
            |light| light.radius_override = f32::NAN,
        ] {
            let mut invalid = PRESET_LIGHT;
            mutate(&mut invalid);
            assert!(
                plugin_with_lights(&spec(), &invalid).is_err(),
                "invalid light {invalid:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_invalid_interiors() {
        for mutate in [
            (|interior: &mut Interior<'_>| interior.editor_id = "") as fn(&mut Interior<'_>),
            |interior| interior.full_name = "",
            |interior| interior.inside.editor_id = interior.outside.editor_id,
            |interior| interior.outside.model_path = "../escape.nif",
            |interior| interior.inside.position = [f32::NAN, 0.0, 0.0],
            |interior| interior.outside.arrival_rotation = [0.0, f32::INFINITY, 0.0],
            |interior| {
                interior.exterior_cell = Cell {
                    grid_x: 7,
                    grid_y: 7,
                }
            },
        ] {
            let mut invalid = PRESET_INTERIOR;
            mutate(&mut invalid);
            assert!(
                plugin_with_interior(&spec(), &invalid).is_err(),
                "invalid interior {invalid:?} was accepted"
            );
        }
    }
}
