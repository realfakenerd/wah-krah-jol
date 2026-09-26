//! The generated light and the reference that places it, through the real ESM
//! parser and then through the database export: one `LIGH` base record with the
//! 48-byte `DATA` layout and an `FNAM` fade, and one `REFR` carrying an `XRDS`
//! radius override.

use converter::esm::{
    binary::{parse_group, parse_plugin_file, parse_record_header},
    exporter::{create_tables, export_to_db, validate_database},
    records::RawRecord,
};
use dummy_content::{
    esm::{self, Plugin},
    layout,
};
use rusqlite::Connection;
use std::{collections::HashMap, fs, path::Path};

/// The spec `dummy-content gen --with-lights` assembles: one exterior cell, its
/// static and the light, built from the same `layout` constants the command
/// uses so this file cannot drift from the plugin the command publishes.
fn spec() -> Plugin<'static> {
    Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    }
}

/// The bytes `dummy-content gen --with-lights` writes: one exterior cell, one
/// `LIGH` base record and the one reference that places it.
fn preset_plugin() -> Vec<u8> {
    esm::plugin_with_lights(&spec(), &esm::PRESET_LIGHT).unwrap()
}

/// The same preset with the base record's model taken away. Most `LIGH`
/// records in the game are like this: they light the space with nothing to
/// draw, so they have no `MODL` to hang a mesh on.
fn preset_plugin_without_a_light_model() -> Vec<u8> {
    let invisible = esm::Light {
        model_path: None,
        ..esm::PRESET_LIGHT
    };
    esm::plugin_with_lights(&spec(), &invisible).unwrap()
}

fn write_plugin(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("Skyrim.esm");
    fs::write(&path, preset_plugin()).unwrap();
    path
}

fn subrecord<'a>(record: &'a RawRecord, tag: &[u8; 4]) -> Option<&'a [u8]> {
    record
        .subrecords
        .iter()
        .find(|(candidate, _)| candidate.as_slice() == tag)
        .map(|(_, data)| data.as_slice())
}

fn subrecord_or_panic<'a>(record: &'a RawRecord, tag: &[u8; 4]) -> &'a [u8] {
    subrecord(record, tag).unwrap_or_else(|| {
        panic!(
            "{} record {:08X} has no {}",
            String::from_utf8_lossy(&record.record_type),
            record.form_id,
            String::from_utf8_lossy(tag)
        )
    })
}

/// An `EDID` as the parser hands it back: the bytes as they were written, NUL
/// terminator included.
fn editor_id(record: &RawRecord) -> String {
    String::from_utf8_lossy(subrecord(record, b"EDID").unwrap_or_default()).into_owned()
}

fn form_id(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn floats(bytes: &[u8]) -> [f32; 3] {
    let mut values = [0.0f32; 3];
    for (index, value) in values.iter_mut().enumerate() {
        *value = f32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
    }
    values
}

/// What the real parser reads out of the generated plugin: a `LIGH` whose
/// `DATA` is the 48-byte layout `Skyrim.esm` uses, whose `FNAM` carries the
/// fade, and a reference in the exterior cell that points at it and overrides
/// its radius through a four-byte `XRDS`.
#[test]
fn generated_light_plugin_holds_a_light_and_its_placed_reference() {
    let directory = tempfile::tempdir().unwrap();
    let records = parse_plugin_file(&write_plugin(directory.path())).unwrap();

    let lights: Vec<&RawRecord> = records
        .iter()
        .filter(|record| &record.record_type == b"LIGH")
        .collect();
    assert_eq!(lights.len(), 1, "one LIGH base record");
    let light = lights[0];
    assert_eq!(editor_id(light), "GeneratedLight01\0");
    let expected_modl = [layout::GENERATED_MODEL_PATH.as_bytes(), b"\0"].concat();
    assert_eq!(
        subrecord(light, b"MODL"),
        Some(expected_modl.as_slice()),
        "the base record's MODL arrives as written, NUL included"
    );

    // The fields UESP documents, at the offsets the converter's `insert_light`
    // reads them from: time (i32), radius (u32), colour (RGB plus one unused
    // byte), flags (u32), falloff exponent (f32). The 28 bytes behind the
    // falloff are the FOV, near clip, flicker and value fields the fixture
    // leaves clear.
    let data = subrecord_or_panic(light, b"DATA");
    assert_eq!(data.len(), 48, "the size every LIGH DATA has in Skyrim.esm");
    assert_eq!(
        i32::from_le_bytes(data[0..4].try_into().unwrap()),
        -1,
        "time"
    );
    assert_eq!(
        u32::from_le_bytes(data[4..8].try_into().unwrap()),
        512,
        "radius, a u32 the converter widens to a float"
    );
    assert_eq!(
        &data[8..12],
        &[216, 128, 39, 0],
        "colour RGB and the unused byte"
    );
    assert_eq!(
        u32::from_le_bytes(data[12..16].try_into().unwrap()),
        0,
        "flags: clear, so the light is on and positive"
    );
    assert_eq!(
        f32::from_le_bytes(data[16..20].try_into().unwrap()),
        1.0,
        "falloff exponent"
    );
    assert_eq!(&data[20..], &[0u8; 28], "the fields nothing reads");
    assert_eq!(
        f32::from_le_bytes(subrecord_or_panic(light, b"FNAM")[..4].try_into().unwrap()),
        1.0,
        "the FNAM fade"
    );

    let exterior = records
        .iter()
        .find(|record| &record.record_type == b"CELL")
        .expect("the exterior CELL record");
    let worldspace = records
        .iter()
        .find(|record| &record.record_type == b"WRLD")
        .expect("the WRLD record");
    let references: Vec<&RawRecord> = records
        .iter()
        .filter(|record| &record.record_type == b"REFR")
        .collect();
    assert_eq!(references.len(), 2, "the static placement and the light");
    let placed = references
        .iter()
        .find(|record| subrecord(record, b"XRDS").is_some())
        .expect("the light's reference");
    assert_eq!(
        placed.cell_form_id,
        Some(exterior.form_id),
        "the reference belongs to the exterior cell"
    );
    assert_eq!(
        placed.worldspace_form_id,
        Some(worldspace.form_id),
        "and to the generated worldspace"
    );
    assert_eq!(
        form_id(subrecord_or_panic(placed, b"NAME")),
        light.form_id,
        "the reference names the base record it places"
    );
    assert_eq!(
        floats(subrecord_or_panic(placed, b"DATA")),
        [1024.0, 2048.0, 128.0],
        "the light's own position in the cell"
    );
    assert_eq!(
        floats(&subrecord_or_panic(placed, b"DATA")[12..24]),
        [0.0, 0.0, 0.0],
        "rotation"
    );

    // `XRDS` is a single little-endian `f32`, and the load-order remap rewrites
    // only the subrecords it recognises as 4-byte FormIDs, which `XRDS` is not.
    // The value therefore arrives here exactly as written,
    // and it differs from the base record's radius so a reader cannot mistake
    // one for the other.
    let xrds = subrecord_or_panic(placed, b"XRDS");
    assert_eq!(xrds.len(), 4, "the radius override is one float");
    let radius_override = f32::from_le_bytes(xrds.try_into().unwrap());
    assert_eq!(radius_override, 1024.0);
    assert_ne!(
        radius_override,
        u32::from_le_bytes(data[4..8].try_into().unwrap()) as f32,
        "the override is not the base radius"
    );
}

/// `parse_plugin_file` without file IO, so a prefix of the fixture can be swept.
fn parse_prefix(bytes: &[u8]) {
    let Ok((rest, header)) = parse_record_header(bytes) else {
        return;
    };
    if &header.type_tag != b"TES4" || header.data_size as usize > rest.len() {
        return;
    }
    let mut records = Vec::new();
    let _ = parse_group(&rest[header.data_size as usize..], None, None, &mut records);
}

#[test]
fn generated_light_plugin_never_panics_under_truncation_or_mutation() {
    let bytes = preset_plugin();
    for length in 0..bytes.len() {
        assert!(
            std::panic::catch_unwind(|| parse_prefix(&bytes[..length])).is_ok(),
            "ESM parser panicked at length {length}"
        );
    }
    let mut rng = dummy_content::rng::Rng::new(39);
    for _ in 0..256 {
        let mut mutated = bytes.clone();
        let index = rng.next_u64() as usize % mutated.len();
        mutated[index] ^= 0xff;
        assert!(
            std::panic::catch_unwind(|| parse_prefix(&mutated)).is_ok(),
            "ESM parser panicked on mutation at {index}"
        );
    }
}

/// The parsed plugin through the converter's database export: the same
/// `create_tables` + `export_to_db` pair the pipeline runs on `Skyrim.esm`.
fn export_to_database(records: Vec<RawRecord>) -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    create_tables(&connection).unwrap();
    let master: HashMap<u32, RawRecord> = records
        .into_iter()
        .map(|record| (record.form_id, record))
        .collect();
    export_to_db(&connection, &master).unwrap();
    connection
}

/// The `LIGH` base record of a parsed plugin.
fn light_form_id(records: &[RawRecord]) -> u32 {
    records
        .iter()
        .find(|record| &record.record_type == b"LIGH")
        .expect("the LIGH base record")
        .form_id
}

/// The exported pair the runtime places a point light from: a `lights` row for
/// the base record and the placing reference's own radius in
/// `references.radius_override`.
///
/// The expected values are the preset's: radius 512 (a `DATA` u32 the exporter
/// widens to a float), the warm colour, flags clear so the light is on and
/// positive, falloff 1.0 and the `FNAM` fade 1.0. The reference's override is
/// 1024 against the base record's 512, so a reader cannot mistake one for the
/// other, and the static placement beside it has no `XRDS` at all.
#[test]
fn generated_light_plugin_exports_a_lights_row_and_a_reference_radius_override() {
    let directory = tempfile::tempdir().unwrap();
    let records = parse_plugin_file(&write_plugin(directory.path())).unwrap();
    let base_form_id = light_form_id(&records);
    let (lit_ref_id, static_ref_id) = {
        let mut lit = None;
        let mut placement = None;
        for record in records.iter().filter(|r| &r.record_type == b"REFR") {
            if subrecord(record, b"XRDS").is_some() {
                lit = Some(record.form_id);
            } else {
                placement = Some(record.form_id);
            }
        }
        (
            lit.expect("the light's reference"),
            placement.expect("the static placement"),
        )
    };

    let connection = export_to_database(records);

    type LightRow = (String, f64, i64, i64, i64, i64, f64, f64);
    let light: LightRow = connection
        .query_row(
            "SELECT editor_id,radius,color_r,color_g,color_b,flags,falloff,fade FROM lights WHERE id=?1",
            [base_form_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        light,
        (
            "GeneratedLight01".to_owned(),
            512.0,
            216,
            128,
            39,
            0,
            1.0,
            1.0
        )
    );

    let radius_override = |id: u32| -> Option<f64> {
        connection
            .query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(radius_override(lit_ref_id), Some(1024.0));
    assert_eq!(
        radius_override(static_ref_id),
        None,
        "a reference without XRDS has no override"
    );

    // This preset's light does carry the generated mesh, so it is a static too:
    // the `LIGH` arm stores the model a reference of it draws.
    let model_path: Option<String> = connection
        .query_row(
            "SELECT model_path FROM statics WHERE id=?1",
            [base_form_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(model_path.as_deref(), Some(layout::GENERATED_MODEL_PATH));

    let version: u32 = connection
        .query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        version, 4,
        "the lights table and the radius_override column are schema 4"
    );
    // The exporter's stamp and the contract the engine checks are one version.
    validate_database(&connection).unwrap();
}

/// A `LIGH` without a `MODL` still lights the space, so it gets a `lights` row
/// like any other, and no `statics` row: there is no mesh to draw, and one
/// meshless static per candle would be most of the game's lights.
#[test]
fn a_light_without_a_model_exports_a_lights_row_and_no_statics_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Skyrim.esm");
    fs::write(&path, preset_plugin_without_a_light_model()).unwrap();
    let records = parse_plugin_file(&path).unwrap();
    let base_form_id = light_form_id(&records);

    let connection = export_to_database(records);

    let radius: f64 = connection
        .query_row(
            "SELECT radius FROM lights WHERE id=?1",
            [base_form_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(radius, 512.0, "an invisible light still lights a space");
    let statics: i64 = connection
        .query_row(
            "SELECT count(*) FROM statics WHERE id=?1",
            [base_form_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(statics, 0, "a model-less LIGH is not a static");
    let all_statics: i64 = connection
        .query_row("SELECT count(*) FROM statics", [], |row| row.get(0))
        .unwrap();
    assert_eq!(all_statics, 1, "only the fixture's own STAT is in statics");
}
