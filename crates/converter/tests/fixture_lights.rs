//! The generated light and the reference that places it, through the real ESM
//! parser: one `LIGH` base record with the 48-byte `DATA` layout and an `FNAM`
//! fade, and one `REFR` carrying an `XRDS` radius override.

use converter::esm::{
    binary::{parse_group, parse_plugin_file, parse_record_header},
    records::RawRecord,
};
use dummy_content::{
    esm::{self, Plugin},
    layout,
};
use std::{fs, path::Path};

/// The bytes `dummy-content gen --with-lights` writes: one exterior cell, one
/// `LIGH` base record and the one reference that places it. The spec is
/// assembled from the same `layout` constants the command uses, so this file
/// cannot drift from the plugin the command publishes.
fn preset_plugin() -> Vec<u8> {
    let cells = [esm::PRESET_EXTERIOR_CELL];
    esm::plugin_with_lights(
        &Plugin {
            author: layout::GENERATED_AUTHOR,
            worldspace: layout::GENERATED_WORLDSPACE,
            cells: &cells,
            model_path: layout::GENERATED_MODEL_PATH,
            diffuse: layout::GENERATED_DIFFUSE_PATH,
            normal_texture: layout::GENERATED_NORMAL_PATH,
        },
        &esm::PRESET_LIGHT,
    )
    .unwrap()
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
