//! Additive differential tests: existing remapping/selection remains the independent oracle.
use super::{
    RawRecord, RemapWarnings, binary::PluginMetadata, load_order::LoadOrder, remap_record_form_ids,
};
use shared::{
    form_key::FormKey,
    legacy_id_view::LegacyIdView,
    plugin_table::{PluginFacts, PluginFlags, PluginTable},
    source_form_id::{Resolution, SourceFormId, ZeroPolicy, resolve},
};

/// Keep all existing u32 APIs untouched; compare their actual output with the new pure view.
fn check(
    order: &LoadOrder,
    table: &PluginTable,
    supplier: usize,
    raw: u32,
    packed: Option<u64>,
    expected: u32,
) {
    let ids: Vec<_> = order
        .names
        .iter()
        .map(|name| table.id(name).unwrap())
        .collect();
    let resolution = resolve(
        table,
        SourceFormId {
            raw,
            containing_plugin: ids[supplier],
        },
        ZeroPolicy::Null,
    );
    assert_eq!(resolution.key().map(FormKey::to_packed), packed);
    let view = LegacyIdView::from_load_order(table, &ids).unwrap();
    let actual = resolution
        .key()
        .map_or(Ok(0), |key| view.to_legacy_u32(key));
    let mut record = RawRecord {
        form_id: raw,
        record_type: *b"STAT",
        flags: 0,
        subrecords: Vec::new(),
        cell_form_id: None,
        worldspace_form_id: None,
        load_order: 0,
    };
    remap_record_form_ids(
        &mut record,
        &order.names[supplier],
        &order.metadata[supplier].masters,
        &order.normal,
        &order.light,
        &RemapWarnings::default(),
    )
    .unwrap();
    assert_eq!(actual, Ok(expected));
    assert_eq!(record.form_id, expected);
    assert_eq!(actual, Ok(record.form_id));
    assert_eq!(view.from_legacy_u32(expected), Ok(resolution.key()));
    if let Some(key) = resolution.key() {
        // The existing reverse mapping independently checks the decoded identity.
        let decoded = view.from_legacy_u32(expected).unwrap().unwrap();
        let stable = order.identity(expected).unwrap();
        let named = table.named(decoded).unwrap();
        assert_eq!(
            named.parts(),
            Some((stable.plugin.as_str(), stable.local_id))
        );
        assert_eq!(
            view.to_legacy_u32(view.from_legacy_u32(expected).unwrap().unwrap()),
            Ok(expected)
        );
        assert_eq!(
            view.from_legacy_u32(view.to_legacy_u32(key).unwrap()),
            Ok(Some(key))
        );
    }
}

#[test]
/// Nonidentity MAST, light overrides, shared locals, boundaries and recovery match old packing.
fn form_key_differential_nonidentity_masters() {
    let names: Vec<String> = ["A.esm", "B.esm", "Light.esp", "Patch.esp"]
        .map(str::to_owned)
        .to_vec();
    let metadata = vec![
        PluginMetadata::default(),
        PluginMetadata::default(),
        PluginMetadata {
            flags: 0x200,
            masters: vec!["A.esm".into()],
        },
        PluginMetadata {
            flags: 0,
            masters: vec!["B.esm".into(), "Light.esp".into(), "A.esm".into()],
        },
    ];
    let table = PluginTable::fresh(
        names
            .iter()
            .zip(&metadata)
            .map(|(name, facts)| {
                (
                    name.clone(),
                    PluginFacts {
                        masters: facts.masters.clone(),
                        flags: PluginFlags {
                            master: facts.flags & 1 != 0,
                            light: facts.flags & 0x200 != 0,
                            medium: false,
                        },
                        header_version: Some(1.71),
                        ..Default::default()
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    let order = LoadOrder::from_metadata(names, metadata).unwrap();
    for (raw, packed, runtime) in [
        (0x0000_0800, Some(0x0200_0800), 0x0100_0800),
        (0x0200_0800, Some(0x0100_0800), 0x0000_0800),
        (0x0100_0fff, Some(0x0300_0fff), 0xfe00_0fff),
        (0x0300_0800, Some(0x0400_0800), 0x0200_0800),
        (0x02ff_ffff, Some(0x01ff_ffff), 0x00ff_ffff),
        (0x0500_0123, Some(0x0400_0123), 0x0200_0123),
        (0, None, 0),
    ] {
        check(&order, &table, 3, raw, packed, runtime);
    }
    check(
        &order,
        &table,
        2,
        0x00ff_ffff,
        Some(0x01ff_ffff),
        0x00ff_ffff,
    );
    check(
        &order,
        &table,
        2,
        0x0100_0800,
        Some(0x0300_0800),
        0xfe00_0800,
    );
    assert!(matches!(
        resolve(
            &table,
            SourceFormId {
                containing_plugin: table.id("Patch.esp").unwrap(),
                raw: 0x0100_1000
            },
            ZeroPolicy::Null
        ),
        Resolution::InvalidWidth { .. }
    ));
    // Malformed wide light locals are deliberately outside differential equivalence:
    // today's permissive STAT path truncates; this contract rejects instead of aliasing.
    let mut malformed = RawRecord {
        form_id: 0x0100_1000,
        record_type: *b"STAT",
        flags: 0,
        subrecords: Vec::new(),
        cell_form_id: None,
        worldspace_form_id: None,
        load_order: 0,
    };
    let warnings = RemapWarnings::default();
    remap_record_form_ids(
        &mut malformed,
        "Light.esp",
        &order.metadata[2].masters,
        &order.normal,
        &order.light,
        &warnings,
    )
    .unwrap();
    assert_eq!(malformed.form_id, 0xfe00_0000);
    malformed.form_id = 0x0100_1000;
    malformed.record_type = *b"GRAS";
    assert!(
        remap_record_form_ids(
            &mut malformed,
            "Light.esp",
            &order.metadata[2].masters,
            &order.normal,
            &order.light,
            &warnings
        )
        .is_err()
    );
    malformed.form_id = 0x0100_0800;
    malformed.record_type = *b"REFR";
    malformed.subrecords = vec![
        (
            b"XESP".to_vec(),
            [0x0500_0800u32.to_le_bytes(), [1, 2, 3, 4]].concat(),
        ),
        (b"XEMI".to_vec(), 0x0000_0801u32.to_le_bytes().to_vec()),
    ];
    remap_record_form_ids(
        &mut malformed,
        "Light.esp",
        &order.metadata[2].masters,
        &order.normal,
        &order.light,
        &warnings,
    )
    .unwrap();
    assert_eq!(
        malformed.subrecords,
        vec![
            (
                b"XESP".to_vec(),
                [0u32.to_le_bytes(), [1, 2, 3, 4]].concat()
            ),
            (b"XEMI".to_vec(), 0x0000_0801u32.to_le_bytes().to_vec())
        ]
    );
    assert_eq!(warnings.skipped_optional.borrow()["Light.esp"].0, 1);
}

/// Generate a tiny real native-format TES4 header, with no proprietary fixture data.
fn native_header(light: bool) -> Vec<u8> {
    let hedr = [
        1.71f32.to_le_bytes(),
        0u32.to_le_bytes(),
        0x800u32.to_le_bytes(),
    ]
    .concat();
    let payload = [b"HEDR".as_slice(), &12u16.to_le_bytes(), &hedr].concat();
    [
        b"TES4".as_slice(),
        &(payload.len() as u32).to_le_bytes(),
        &(if light { 0x200u32 } else { 1u32 }).to_le_bytes(),
        &[0; 12],
        &payload,
    ]
    .concat()
}

#[test]
/// Actual player/CCC selection fixes official slots and preserves nonalphabetical CC order.
fn form_key_differential_official_and_ccc_order() {
    let root = tempfile::tempdir().unwrap();
    let names = [
        "Skyrim.esm",
        "Update.esm",
        "Dawnguard.esm",
        "HearthFires.esm",
        "Dragonborn.esm",
        "ccZ-Full.esm",
        "ccA-Full.esm",
        "ccZ-Light.esl",
        "ccA-Light.esl",
    ];
    for name in names {
        std::fs::write(
            root.path().join(name),
            native_header(name.ends_with(".esl")),
        )
        .unwrap();
    }
    let ccc = root.path().join("Skyrim.ccc");
    std::fs::write(
        &ccc,
        "ccZ-Full.esm\nccZ-Light.esl\nccA-Full.esm\nccA-Light.esl\n",
    )
    .unwrap();
    let list = root.path().join("plugins.txt");
    std::fs::write(&list, "# official and CCC inputs are implicit\n").unwrap();
    let paths =
        super::plugins_txt::read_plugins_txt_with_options(&list, root.path(), Some(&ccc), None)
            .unwrap();
    let expected = [
        "skyrim.esm",
        "update.esm",
        "dawnguard.esm",
        "hearthfires.esm",
        "dragonborn.esm",
        "ccz-full.esm",
        "ccz-light.esl",
        "cca-full.esm",
        "cca-light.esl",
    ];
    assert_eq!(
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>(),
        expected
    );
    let order = LoadOrder::read(&paths).unwrap();
    let table = PluginTable::fresh(
        names
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    PluginFacts {
                        header_version: Some(1.71),
                        ..Default::default()
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    // Sorted table ids differ from official/load slots: Skyrim=8, Update=9, Dawnguard=5.
    for (supplier, packed, runtime) in [
        (0, 0x0800_0800, 0x0000_0800),
        (1, 0x0900_0800, 0x0100_0800),
        (2, 0x0500_0800, 0x0200_0800),
        (3, 0x0700_0800, 0x0300_0800),
        (4, 0x0600_0800, 0x0400_0800),
        (5, 0x0300_0800, 0x0500_0800),
        (6, 0x0400_0800, 0xfe00_0800),
        (7, 0x0100_0800, 0x0600_0800),
        (8, 0x0200_0800, 0xfe00_1800),
    ] {
        check(&order, &table, supplier, 0x800, Some(packed), runtime);
    }
}
