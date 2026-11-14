//! The actor model catalog through the real ESM parser and database export.
//!
//! Every fixture is synthesized by `dummy-content`; the retail tests read a local
//! Skyrim install and are opt-in.

use converter::actor_models::{ActorModelCatalog, UnresolvedReason, resolve_actor_models};
use converter::esm::{
    EsmParser,
    load_order::{LoadOrder, StableId},
};
use dummy_content::{esm, inhouse_items as items, layout};
use rusqlite::Connection;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const MALE_BODY: &str = "actors\\character\\character assets\\malebody_1.nif";
const MALE_HANDS: &str = "actors\\character\\character assets\\malehands_1.nif";
const MALE_FEET: &str = "actors\\character\\character assets\\malefeet_1.nif";
const FEMALE_BODY: &str = "actors\\character\\character assets\\femalebody_1.nif";
const OTHER_BODY: &str = "actors\\character\\custom\\otherbody.nif";
const PATCH_BODY: &str = "actors\\patch\\nordbody.nif";
const EXACT_BODY: &str = "actors\\patch\\exactracebody.nif";
const DECOY_BODY: &str = "actors\\decoy\\decoybody.nif";

/// A plugin file: an empty `TES4` header followed by the fixture records.
fn plugin(records: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = items::record(b"TES4", 0, &[]);
    for record in records {
        bytes.extend(record);
    }
    bytes
}

/// Writes the fixture plugin under `name` and returns its path.
fn write_plugin(directory: &Path, name: &str, records: &[Vec<u8>]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, plugin(records)).unwrap();
    path
}

/// Writes a plugin whose `TES4` header declares `masters`, so its own records use the next index.
fn write_master_plugin(
    directory: &Path,
    name: &str,
    masters: &[&str],
    records: &[Vec<u8>],
) -> PathBuf {
    let mut fields: Vec<(&[u8; 4], Vec<u8>)> = Vec::new();
    for master in masters {
        fields.push((b"MAST", items::text(master)));
        fields.push((b"DATA", vec![0xa5; 8]));
    }
    let mut bytes = items::record(b"TES4", 0, &fields);
    for record in records {
        bytes.extend(record);
    }
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// Merges the plugin and resolves its catalog.
fn resolve(path: &Path) -> ActorModelCatalog {
    let paths = [path.to_path_buf()];
    resolve_plugins(&paths)
}

/// Merges the plugins and resolves their catalog, mapping each `NPC_` to its stable identity.
fn resolve_plugins(paths: &[PathBuf]) -> ActorModelCatalog {
    let records = EsmParser::merge_plugins(paths).unwrap();
    let order = LoadOrder::read(paths).unwrap();
    let plugin_ids: HashMap<u32, StableId> = records
        .iter()
        .filter(|(_, record)| &record.record_type == b"NPC_")
        .filter_map(|(id, _)| order.identity(*id).ok().map(|identity| (*id, identity)))
        .collect();
    resolve_actor_models(&records, &plugin_ids)
}

/// Converts the plugin into a fresh database and returns the open connection.
fn export(directory: &Path, path: &Path) -> Connection {
    let db = directory.join("skyrim_world.db");
    EsmParser::convert_plugins(std::slice::from_ref(&path.to_path_buf()), &db).unwrap();
    Connection::open(&db).unwrap()
}

/// A `NordRace` skin whose one armature fills the body slot.
fn nord_body_only(skin: u32) -> Vec<Vec<u8>> {
    vec![
        items::race(0x200, "NordRace", Some(skin), false),
        items::skin_armor(skin, &[0x400]),
        items::armature(
            0x400,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            Some(FEMALE_BODY),
        ),
    ]
}

#[test]
fn body_chain_follows_race_skin_armature() {
    let directory = tempfile::tempdir().unwrap();
    let mut records = vec![items::npc(0x100, 0x200, false)];
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let row = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(row.race_id, 0x200);
    assert_eq!(row.sex, "male");
    assert_eq!(row.skin_armo_id, Some(0x300));
    assert_eq!(row.resolution, "body_head");

    let parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x100)
        .collect();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].slot.as_str(), "body");
    assert_eq!(parts[0].model_path, MALE_BODY);
}

#[test]
fn exact_race_arma_wins_over_default_race() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::npc(0x100, 0x200, false),
        items::race(0x200, "NordRace", Some(0x300), false),
        // The default-race armature is listed first, so priority cannot be file order.
        items::skin_armor(0x300, &[0x400, 0x401]),
        items::armature(
            0x400,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(OTHER_BODY),
            None,
        ),
        items::armature(0x401, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x100)
        .collect();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].model_path, MALE_BODY);
}

#[test]
fn default_race_is_the_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let mut records = vec![items::npc(0x100, 0x200, false)];
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x100)
        .collect();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].model_path, MALE_BODY);
    assert!(
        catalog
            .models
            .iter()
            .any(|row| row.id == 0x100 && row.resolution == "body_head")
    );
}

fn templated_npc(
    id: u32,
    race_id: u32,
    female: bool,
    traits: bool,
    template: Option<u32>,
    skin: Option<u32>,
) -> Vec<u8> {
    let mut acbs = vec![0u8; 24];
    acbs[0] = u8::from(female);
    acbs[18..20].copy_from_slice(&u16::from(traits).to_le_bytes());
    let mut fields = vec![(b"ACBS", acbs)];
    if let Some(template) = template {
        fields.push((b"TPLT", template.to_le_bytes().to_vec()));
    }
    fields.push((b"RNAM", race_id.to_le_bytes().to_vec()));
    if let Some(skin) = skin {
        fields.push((b"WNAM", skin.to_le_bytes().to_vec()));
    }
    items::record(b"NPC_", id, &fields)
}

#[test]
fn traits_root_supplies_race_sex_skin_and_facegen_owner() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        templated_npc(0x100, 0x200, false, true, Some(0x101), None),
        templated_npc(0x101, 0x201, false, true, Some(0x102), None),
        templated_npc(0x102, 0x202, true, false, None, Some(0x301)),
        items::race(0x200, "PlaceholderRace", None, false),
        items::race(0x201, "IntermediateRace", None, false),
        items::race(0x202, "RootRace", Some(0x300), true),
        items::skin_armor(0x300, &[0x400]),
        items::skin_armor(0x301, &[0x401]),
        items::armature(0x400, 0x202, items::BIPED_BODY, Some(MALE_BODY), None),
        items::armature(0x401, 0x202, items::BIPED_BODY, Some(OTHER_BODY), None),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let catalog = resolve(&path);
    let row = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(
        (row.race_id, row.sex, row.skin_armo_id),
        (0x202, "female", Some(0x301))
    );
    assert_eq!(
        row.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/skyrim.esm/00000102.nif")
    );
    assert_eq!(
        catalog
            .parts
            .iter()
            .find(|part| part.npc_id == 0x100)
            .unwrap()
            .model_path,
        OTHER_BODY
    );
}

#[test]
fn race_skin_fallback_and_traits_clear_ignore_template() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        templated_npc(0x100, 0x200, false, true, Some(0x101), None),
        templated_npc(0x101, 0x201, true, false, None, None),
        // Traits clear: stale TPLT is ignored; own sex/race/skin remain effective.
        templated_npc(0x102, 0x200, false, false, Some(0x999), None),
        items::race(0x200, "RaceA", Some(0x300), false),
        items::race(0x201, "RaceB", Some(0x301), false),
        items::skin_armor(0x300, &[0x400]),
        items::skin_armor(0x301, &[0x401]),
        items::armature(0x400, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
        items::armature(0x401, 0x201, items::BIPED_BODY, Some(OTHER_BODY), None),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let catalog = resolve(&path);
    let inherited = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(
        (inherited.race_id, inherited.sex, inherited.skin_armo_id),
        (0x201, "female", Some(0x301))
    );
    let own = catalog.models.iter().find(|row| row.id == 0x102).unwrap();
    assert_eq!(
        (own.race_id, own.sex, own.skin_armo_id),
        (0x200, "male", Some(0x300))
    );
}

#[test]
fn invalid_npc_skin_override_falls_back_with_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        templated_npc(0x100, 0x200, false, false, None, Some(0x999)),
        items::race(0x200, "RaceA", Some(0x300), false),
        items::skin_armor(0x300, &[0x400]),
        items::armature(0x400, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let catalog = resolve(&path);
    assert_eq!(catalog.models[0].skin_armo_id, Some(0x300));
    assert!(
        catalog
            .diagnostics
            .contains(&converter::actor_models::ActorModelDiagnostic {
                npc_id: 0x100,
                reason: UnresolvedReason::MissingNpcSkinOverride,
            })
    );
}

#[test]
fn zero_deleted_and_wrong_type_npc_skin_overrides_fall_back_precisely() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        templated_npc(0x100, 0x200, false, false, None, Some(0)),
        templated_npc(0x101, 0x200, false, false, None, Some(0x999)),
        templated_npc(0x102, 0x200, false, false, None, Some(0x400)),
        templated_npc(0x103, 0x200, false, false, None, Some(0x401)),
        items::npc(0x400, 0x200, false),
        items::race(0x200, "RaceA", Some(0x300), false),
        items::skin_armor(0x300, &[0x500]),
        items::armature(
            0x500,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            None,
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let mut raw = EsmParser::merge_plugins(std::slice::from_ref(&path)).unwrap();
    raw.insert(
        0x401,
        converter::esm::records::RawRecord {
            form_id: 0x401,
            record_type: *b"ARMO",
            flags: 0x20,
            subrecords: vec![],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        },
    );
    let catalog = resolve_actor_models(&raw, &HashMap::new());
    for (id, reason) in [
        (0x100, UnresolvedReason::ZeroNpcSkinOverride),
        (0x101, UnresolvedReason::MissingNpcSkinOverride),
        (0x102, UnresolvedReason::WrongTypeNpcSkinOverride),
        (0x103, UnresolvedReason::DeletedNpcSkinOverride),
    ] {
        let row = catalog.models.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.skin_armo_id, Some(0x300), "{id:#x}");
        assert!(
            catalog
                .diagnostics
                .contains(&converter::actor_models::ActorModelDiagnostic { npc_id: id, reason }),
            "{id:#x}: {:?}",
            catalog.diagnostics
        );
    }
}

#[test]
fn unresolved_traits_targets_keep_one_empty_row_with_specific_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        templated_npc(0x100, 0x200, false, true, None, None),
        templated_npc(0x109, 0x200, false, true, Some(0), None),
        templated_npc(0x101, 0x200, false, true, Some(0x999), None),
        templated_npc(0x102, 0x200, false, true, Some(0x106), None),
        templated_npc(0x103, 0x200, false, true, Some(0x200), None),
        templated_npc(0x104, 0x200, false, true, Some(0x105), None),
        templated_npc(0x105, 0x200, false, true, Some(0x104), None),
        templated_npc(0x107, 0x200, false, true, Some(0x108), None),
        items::record(b"LVLN", 0x108, &[]),
        items::race(0x200, "RaceA", Some(0x300), true),
        items::skin_armor(0x300, &[0x400]),
        items::armature(0x400, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let mut raw = EsmParser::merge_plugins(std::slice::from_ref(&path)).unwrap();
    raw.insert(
        0x106,
        converter::esm::records::RawRecord {
            form_id: 0x106,
            record_type: *b"NPC_",
            flags: 0x20,
            subrecords: vec![],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        },
    );
    let catalog = resolve_actor_models(&raw, &HashMap::new());
    let cases = [
        (0x100, UnresolvedReason::MissingTraitsTemplate),
        (0x109, UnresolvedReason::MissingTraitsTemplate),
        (0x101, UnresolvedReason::MissingTraitsTemplate),
        (0x102, UnresolvedReason::DeletedTraitsTemplate),
        (0x103, UnresolvedReason::WrongTypeTraitsTemplate),
        (0x104, UnresolvedReason::TraitsTemplateCycle),
        (0x105, UnresolvedReason::TraitsTemplateCycle),
        (0x107, UnresolvedReason::LeveledTraitsTemplate),
    ];
    for (id, reason) in cases {
        let row = catalog.models.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.resolution, "unresolved", "{id:#x}");
        assert_eq!(row.race_id, 0, "{id:#x}");
        assert!(row.head_model.is_none(), "{id:#x}");
        assert!(
            !catalog.parts.iter().any(|part| part.npc_id == id),
            "{id:#x}"
        );
        assert!(
            catalog
                .diagnostics
                .contains(&converter::actor_models::ActorModelDiagnostic { npc_id: id, reason }),
            "{id:#x}: {:?}",
            catalog.diagnostics
        );
    }
}

#[test]
fn additional_race_armature_matches_in_authored_order_before_default() {
    let directory = tempfile::tempdir().unwrap();
    let additional = items::record(
        b"ARMA",
        0x400,
        &[
            (b"EDID", items::text("AdditionalRace")),
            (b"BOD2", items::body_template(items::BIPED_BODY)),
            (b"RNAM", 0x201u32.to_le_bytes().to_vec()),
            (b"MODL", 0u32.to_le_bytes().to_vec()),
            (b"MODL", 0x999u32.to_le_bytes().to_vec()),
            (b"MODL", 0x200u32.to_le_bytes().to_vec()),
            (b"MOD2", items::text(OTHER_BODY)),
        ],
    );
    let records = vec![
        items::npc(0x100, 0x200, false),
        items::race(0x200, "RaceA", Some(0x300), false),
        items::skin_armor(0x300, &[0x400, 0x401, 0x402]),
        additional,
        items::armature(0x401, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
        items::armature(
            0x402,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(DECOY_BODY),
            None,
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let catalog = resolve(&path);
    assert_eq!(
        catalog
            .parts
            .iter()
            .find(|part| part.npc_id == 0x100)
            .unwrap()
            .model_path,
        OTHER_BODY
    );
}

#[test]
fn female_acbs_bit_selects_mod3() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::npc(0x100, 0x200, true),
        items::race(0x200, "NordRace", Some(0x300), false),
        items::skin_armor(0x300, &[0x400]),
        items::armature(
            0x400,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            Some(FEMALE_BODY),
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let row = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(row.sex, "female");
    let parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x100)
        .collect();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].model_path, FEMALE_BODY);
    assert_eq!(
        row.head_model.as_deref(),
        Some("meshes/actors/character/character assets/femalehead.nif")
    );
    assert_eq!(row.head_source, Some("default"));
}

#[test]
fn female_without_mod3_falls_back_to_mod2() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::npc(0x100, 0x200, true),
        items::race(0x200, "NordRace", Some(0x300), false),
        items::skin_armor(0x300, &[0x400]),
        items::armature(
            0x400,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            None,
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x100)
        .collect();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].model_path, MALE_BODY);
    assert!(
        !catalog
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.npc_id == 0x100),
        "{:?}",
        catalog.diagnostics
    );
}

#[test]
fn missing_race_is_recorded_not_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::record(
            b"NPC_",
            0x100,
            &[
                (b"EDID", items::text("NoRaceLink")),
                (b"ACBS", vec![0u8; 24]),
            ],
        ),
        items::npc(0x101, 0x999, false),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let no_rnam = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(no_rnam.race_id, 0);
    assert_eq!(no_rnam.resolution, "unresolved");
    let no_record = catalog.models.iter().find(|row| row.id == 0x101).unwrap();
    assert_eq!(no_record.race_id, 0x999);
    assert_eq!(no_record.resolution, "unresolved");
    assert!(
        catalog
            .diagnostics
            .contains(&converter::actor_models::ActorModelDiagnostic {
                npc_id: 0x100,
                reason: UnresolvedReason::NoRnam,
            })
    );
    assert!(
        catalog
            .diagnostics
            .contains(&converter::actor_models::ActorModelDiagnostic {
                npc_id: 0x101,
                reason: UnresolvedReason::NoRaceRecord,
            })
    );

    let connection = export(directory.path(), &path);
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM actor_models", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "both unresolved bases are recorded");
    let resolution: String = connection
        .query_row(
            "SELECT resolution FROM actor_models WHERE id=0x100",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolution, "unresolved");
}

#[test]
fn facegen_path_wins_over_default_head() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::npc(0x100, 0x200, false),
        items::race(0x200, "NordRace", Some(0x300), true),
        items::npc(0x101, 0x201, false),
        items::race(0x201, "NordRace", Some(0x300), false),
        items::skin_armor(0x300, &[0x400]),
        items::armature(
            0x400,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            None,
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let facegen = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(
        facegen.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/skyrim.esm/00000100.nif")
    );
    assert_eq!(facegen.head_source, Some("facegen"));
    let default = catalog.models.iter().find(|row| row.id == 0x101).unwrap();
    assert_eq!(
        default.head_model.as_deref(),
        Some("meshes/actors/character/character assets/malehead.nif")
    );
    assert_eq!(default.head_source, Some("default"));
}

#[test]
fn facegen_path_uses_the_plugin_local_form_id() {
    let directory = tempfile::tempdir().unwrap();
    let skyrim = write_plugin(
        directory.path(),
        "Skyrim.esm",
        &[
            items::npc(0x100, 0x200, false),
            items::race(0x200, "NordRace", Some(0x300), true),
            items::skin_armor(0x300, &[0x400]),
            items::armature(
                0x400,
                items::DEFAULT_RACE_FORM_ID,
                items::BIPED_BODY,
                Some(MALE_BODY),
                None,
            ),
        ],
    );
    let patch = write_master_plugin(
        directory.path(),
        "Patch.esp",
        &["Skyrim.esm"],
        &[
            // Override keeps Skyrim.esm as defining owner for ordinary FaceGen.
            items::npc(0x0000_0100, 0x200, false),
            items::npc(0x0100_0D62, 0x200, false),
        ],
    );

    let catalog = resolve_plugins(&[skyrim, patch]);
    let row = catalog
        .models
        .iter()
        .find(|row| row.id == 0x0100_0D62)
        .expect("the NPC_ under its load-order FormID");
    assert_eq!(
        row.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/patch.esp/00000d62.nif")
    );
    assert_eq!(row.head_source, Some("facegen"));
    let ordinary_override = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(
        ordinary_override.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/skyrim.esm/00000100.nif")
    );
}

#[test]
fn alvor_resolves_body_hands_feet_and_facegen_head() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        items::npc(0x0001_3475, 79686, false),
        items::npc(0x0001_3476, 79686, true),
        items::race(79686, "NordRace", Some(3428), true),
        items::skin_armor(3428, &[0x500, 0x501, 0x502]),
        items::armature(
            0x500,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_BODY,
            Some(MALE_BODY),
            Some(FEMALE_BODY),
        ),
        items::armature(
            0x501,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_HANDS,
            Some(MALE_HANDS),
            None,
        ),
        items::armature(
            0x502,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_FEET,
            Some(MALE_FEET),
            None,
        ),
    ];
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let alvor = catalog
        .models
        .iter()
        .find(|row| row.id == 0x0001_3475)
        .expect("Alvor's row");
    assert_eq!(alvor.race_id, 79686);
    assert_eq!(alvor.sex, "male");
    assert_eq!(alvor.skin_armo_id, Some(3428));
    assert_eq!(alvor.resolution, "body_head");
    assert_eq!(
        alvor.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/skyrim.esm/00013475.nif")
    );
    assert_eq!(alvor.head_source, Some("facegen"));

    let mut parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x0001_3475)
        .collect();
    parts.sort_by_key(|part| part.slot);
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert_eq!(parts[0].slot.as_str(), "body");
    assert_eq!(parts[0].model_path, MALE_BODY);
    assert_eq!(parts[1].slot.as_str(), "hands");
    assert_eq!(parts[1].model_path, MALE_HANDS);
    assert_eq!(parts[2].slot.as_str(), "feet");
    assert_eq!(parts[2].model_path, MALE_FEET);

    let sigrid = catalog
        .models
        .iter()
        .find(|row| row.id == 0x0001_3476)
        .expect("Sigrid's row");
    println!("Sigrid (0x00013476) ACBS sex: {}", sigrid.sex);
}

type ModelRow = (
    i64,
    i64,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    String,
);
type PartRow = (i64, String, String);

/// Every published `actor_models` and `actor_model_parts` row, in key order.
fn actor_rows(connection: &Connection) -> (Vec<ModelRow>, Vec<PartRow>) {
    let models: Vec<ModelRow> = connection
        .prepare(
            "SELECT id, race_id, sex, skin_armo_id, head_model, head_source, resolution FROM actor_models ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let parts: Vec<PartRow> = connection
        .prepare("SELECT npc_id, slot, model_path FROM actor_model_parts ORDER BY npc_id, slot")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    (models, parts)
}

/// Patch.esp's own `01` index is its slot `02`, behind Unrelated.esp, whose own `01` records
/// are same-numbered decoys for the patch's skin and armature. Both frontends must follow the
/// patch's body chain and publish identical rows.
#[test]
fn shifted_patch_body_chain_matches_across_frontends() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("Data");
    fs::create_dir(&data).unwrap();
    let generated = esm::plugin(&esm::Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    })
    .unwrap();
    // Header only: the generated body's player NPC is outside this patch chain and would add a row.
    let skyrim = data.join("Skyrim.esm");
    fs::write(&skyrim, items::dependent_plugin(&generated, &[], &[])).unwrap();
    let unrelated = data.join("Unrelated.esp");
    fs::write(
        &unrelated,
        items::dependent_plugin(
            &generated,
            &["Skyrim.esm"],
            &[
                items::race(0x0100_0201, "UnrelatedDecoyRace", Some(0x0100_0301), false),
                items::skin_armor(0x0100_0300, &[0x0100_0400]),
                items::skin_armor(0x0100_0301, &[0x0100_0410, 0x0100_0400]),
                items::npc(0x0100_0D63, 0x0100_0201, false),
                items::armature(
                    0x0100_0400,
                    items::DEFAULT_RACE_FORM_ID,
                    items::BIPED_BODY,
                    Some(DECOY_BODY),
                    None,
                ),
                items::armature(
                    0x0100_0410,
                    items::DEFAULT_RACE_FORM_ID,
                    items::BIPED_BODY,
                    Some(DECOY_BODY),
                    None,
                ),
            ],
        ),
    )
    .unwrap();
    let patch = data.join("Patch.esp");
    fs::write(
        &patch,
        items::dependent_plugin(
            &generated,
            &["Skyrim.esm"],
            &[
                templated_npc(
                    0x0100_0D62,
                    0x0100_0201,
                    false,
                    true,
                    Some(0x0100_0D63),
                    Some(0x0100_0300),
                ),
                templated_npc(
                    0x0100_0D63,
                    0x0100_0201,
                    false,
                    true,
                    Some(0x0100_0D64),
                    None,
                ),
                templated_npc(
                    0x0100_0D64,
                    0x0100_0200,
                    true,
                    false,
                    None,
                    Some(0x0100_0301),
                ),
                // No NPC WNAM: this row exercises shifted race WNAM and armor links.
                items::npc(0x0100_0D65, 0x0100_0200, false),
                items::race(0x0100_0200, "PatchNordRace", Some(0x0100_0300), true),
                items::race(0x0100_0201, "PatchOtherRace", None, false),
                // Decoy race is skipped; exact/additional race matches precede DefaultRace even
                // though the exact match follows it in authored order.
                items::skin_armor(
                    0x0100_0300,
                    &[
                        0x0100_0420,
                        0x0100_0440,
                        0x0100_0400,
                        0x0100_0430,
                        0x0100_0410,
                    ],
                ),
                items::skin_armor(0x0100_0301, &[0x0100_0410, 0x0100_0400]),
                items::armature(
                    0x0100_0420,
                    0x0100_0201,
                    items::BIPED_BODY,
                    Some(OTHER_BODY),
                    None,
                ),
                items::armature(
                    0x0100_0400,
                    items::DEFAULT_RACE_FORM_ID,
                    items::BIPED_HANDS,
                    Some(MALE_HANDS),
                    None,
                ),
                items::armature(
                    0x0100_0440,
                    items::DEFAULT_RACE_FORM_ID,
                    items::BIPED_BODY,
                    Some(DECOY_BODY),
                    None,
                ),
                items::armature(
                    0x0100_0430,
                    0x0100_0200,
                    items::BIPED_BODY,
                    Some(EXACT_BODY),
                    None,
                ),
                items::record(
                    b"ARMA",
                    0x0100_0410,
                    &[
                        (b"EDID", items::text("PatchAdditionalRace")),
                        (b"BOD2", items::body_template(items::BIPED_BODY)),
                        (b"RNAM", 0x0100_0201u32.to_le_bytes().to_vec()),
                        (b"MOD2", items::text(PATCH_BODY)),
                        (b"MODL", 0x0100_0200u32.to_le_bytes().to_vec()),
                    ],
                ),
            ],
        ),
    )
    .unwrap();
    let paths = [skyrim.clone(), unrelated, patch];

    let legacy_path = root.path().join("legacy.db");
    EsmParser::convert_plugins(&paths, &legacy_path).unwrap();
    let legacy = Connection::open(&legacy_path).unwrap();

    let order = data.join("inhouse.plugins.txt");
    fs::write(&order, "*Skyrim.esm\n*Unrelated.esp\n*Patch.esp\n").unwrap();
    let output = root.path().join("inhouse");
    converter::esm::inhouse::export_record_bundle_typed(&data, &order, &output, &[]).unwrap();
    let inhouse = Connection::open(output.join("skyrim_world.db")).unwrap();

    let (models, parts) = actor_rows(&legacy);
    let facegen = "meshes/actors/character/facegendata/facegeom/patch.esp/00000d64.nif";
    let expected_models: Vec<ModelRow> = vec![
        (
            0x0100_0D63,
            0x0100_0201,
            "male".to_owned(),
            Some(0x0100_0301),
            None,
            None,
            "body".to_owned(),
        ),
        (
            0x0200_0D62,
            0x0200_0200,
            "female".to_owned(),
            Some(0x0200_0301),
            Some(facegen.to_owned()),
            Some("facegen".to_owned()),
            "body_head".to_owned(),
        ),
        (
            0x0200_0D63,
            0x0200_0200,
            "female".to_owned(),
            Some(0x0200_0301),
            Some(facegen.to_owned()),
            Some("facegen".to_owned()),
            "body_head".to_owned(),
        ),
        (
            0x0200_0D64,
            0x0200_0200,
            "female".to_owned(),
            Some(0x0200_0301),
            Some(facegen.to_owned()),
            Some("facegen".to_owned()),
            "body_head".to_owned(),
        ),
        (
            0x0200_0D65,
            0x0200_0200,
            "male".to_owned(),
            Some(0x0200_0300),
            Some("meshes/actors/character/facegendata/facegeom/patch.esp/00000d65.nif".to_owned()),
            Some("facegen".to_owned()),
            "body_head".to_owned(),
        ),
    ];
    assert_eq!(models, expected_models);
    let expected_parts: Vec<PartRow> = vec![
        (0x0100_0D63, "body".to_owned(), DECOY_BODY.to_owned()),
        (0x0200_0D62, "body".to_owned(), PATCH_BODY.to_owned()),
        (0x0200_0D62, "hands".to_owned(), MALE_HANDS.to_owned()),
        (0x0200_0D63, "body".to_owned(), PATCH_BODY.to_owned()),
        (0x0200_0D63, "hands".to_owned(), MALE_HANDS.to_owned()),
        (0x0200_0D64, "body".to_owned(), PATCH_BODY.to_owned()),
        (0x0200_0D64, "hands".to_owned(), MALE_HANDS.to_owned()),
        (0x0200_0D65, "body".to_owned(), EXACT_BODY.to_owned()),
        (0x0200_0D65, "hands".to_owned(), MALE_HANDS.to_owned()),
    ];
    assert_eq!(parts, expected_parts);
    assert_eq!(actor_rows(&inhouse), (models, parts));
}

#[test]
fn shifted_light_patch_template_skin_and_additional_race_links_match_frontends() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("Data");
    fs::create_dir(&data).unwrap();
    let generated = esm::plugin(&esm::Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    })
    .unwrap();
    let skyrim = data.join("Skyrim.esm");
    fs::write(&skyrim, items::dependent_plugin(&generated, &[], &[])).unwrap();
    let unrelated = data.join("Unrelated.esp");
    fs::write(
        &unrelated,
        items::dependent_plugin(
            &generated,
            &["Skyrim.esm"],
            &[
                items::npc(0x0100_0D63, 0x0100_0201, false),
                items::race(0x0100_0200, "DecoyRace", None, false),
                items::race(0x0100_0201, "DecoyRootRace", Some(0x0100_0301), false),
                items::skin_armor(0x0100_0301, &[0x0100_0410]),
                items::record(
                    b"ARMA",
                    0x0100_0410,
                    &[
                        (b"EDID", items::text("DecoyAdditionalRace")),
                        (b"BOD2", items::body_template(items::BIPED_BODY)),
                        (b"RNAM", 0x0100_0201u32.to_le_bytes().to_vec()),
                        (b"MOD2", items::text(DECOY_BODY)),
                        (b"MODL", 0x0100_0200u32.to_le_bytes().to_vec()),
                    ],
                ),
            ],
        ),
    )
    .unwrap();
    let patch = data.join("Patch.esl");
    let mut patch_bytes = items::dependent_plugin(
        &generated,
        &["Skyrim.esm"],
        &[
            templated_npc(
                0x0100_0D62,
                0x0100_0201,
                false,
                true,
                Some(0x0100_0D63),
                Some(0x0100_0300),
            ),
            templated_npc(
                0x0100_0D63,
                0x0100_0200,
                true,
                false,
                None,
                Some(0x0100_0301),
            ),
            // No NPC WNAM: this row exercises shifted light-plugin race WNAM and armor links.
            items::npc(0x0100_0D64, 0x0100_0200, false),
            items::race(0x0100_0200, "PatchRootRace", Some(0x0100_0300), true),
            items::race(0x0100_0201, "PatchPlaceholderRace", None, false),
            items::skin_armor(
                0x0100_0300,
                &[0x0100_0440, 0x0100_0400, 0x0100_0430, 0x0100_0410],
            ),
            items::skin_armor(0x0100_0301, &[0x0100_0410, 0x0100_0400]),
            items::record(
                b"ARMA",
                0x0100_0400,
                &[
                    (b"EDID", items::text("PatchDefaultRaceHands")),
                    (b"BOD2", items::body_template(items::BIPED_HANDS)),
                    (b"RNAM", items::DEFAULT_RACE_FORM_ID.to_le_bytes().to_vec()),
                    (b"MOD2", items::text(MALE_HANDS)),
                ],
            ),
            items::record(
                b"ARMA",
                0x0100_0440,
                &[
                    (b"EDID", items::text("PatchDefaultRaceBody")),
                    (b"BOD2", items::body_template(items::BIPED_BODY)),
                    (b"RNAM", items::DEFAULT_RACE_FORM_ID.to_le_bytes().to_vec()),
                    (b"MOD2", items::text(DECOY_BODY)),
                ],
            ),
            items::record(
                b"ARMA",
                0x0100_0430,
                &[
                    (b"EDID", items::text("PatchExactRaceBody")),
                    (b"BOD2", items::body_template(items::BIPED_BODY)),
                    (b"RNAM", 0x0100_0200u32.to_le_bytes().to_vec()),
                    (b"MOD2", items::text(EXACT_BODY)),
                ],
            ),
            items::record(
                b"ARMA",
                0x0100_0410,
                &[
                    (b"EDID", items::text("PatchAdditionalRace")),
                    (b"BOD2", items::body_template(items::BIPED_BODY)),
                    (b"RNAM", 0x0100_0201u32.to_le_bytes().to_vec()),
                    (b"MOD2", items::text(PATCH_BODY)),
                    (b"MODL", 0x0100_0200u32.to_le_bytes().to_vec()),
                ],
            ),
        ],
    );
    patch_bytes[8..12].copy_from_slice(&0x200u32.to_le_bytes());
    fs::write(&patch, patch_bytes).unwrap();
    let paths = [skyrim.clone(), unrelated, patch];

    let legacy_path = root.path().join("legacy.db");
    EsmParser::convert_plugins(&paths, &legacy_path).unwrap();
    let legacy = Connection::open(&legacy_path).unwrap();
    let order = data.join("inhouse.plugins.txt");
    fs::write(&order, "*Skyrim.esm\n*Unrelated.esp\n*Patch.esl\n").unwrap();
    let output = root.path().join("inhouse");
    converter::esm::inhouse::export_record_bundle_typed(&data, &order, &output, &[]).unwrap();
    let inhouse = Connection::open(output.join("skyrim_world.db")).unwrap();

    let (legacy_models, legacy_parts) = actor_rows(&legacy);
    assert_eq!(
        actor_rows(&inhouse),
        (legacy_models.clone(), legacy_parts.clone())
    );
    let child = legacy_models
        .iter()
        .find(|row| row.0 == 0xFE00_0D62)
        .unwrap();
    assert_eq!(child.1, 0xFE00_0200);
    assert_eq!(child.3, Some(0xFE00_0301));
    assert_eq!(
        child.4.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/patch.esl/00000d63.nif")
    );
    assert!(
        legacy_parts
            .iter()
            .any(|part| { part.0 == 0xFE00_0D62 && part.2 == PATCH_BODY })
    );
    let race_skin_child = legacy_models
        .iter()
        .find(|row| row.0 == 0xFE00_0D64)
        .unwrap();
    assert_eq!(race_skin_child.1, 0xFE00_0200);
    assert_eq!(race_skin_child.3, Some(0xFE00_0300));
    assert!(
        legacy_parts
            .iter()
            .any(|part| { part.0 == 0xFE00_0D64 && part.1 == "body" && part.2 == EXACT_BODY })
    );
    assert!(
        legacy_parts
            .iter()
            .any(|part| { part.0 == 0xFE00_0D64 && part.1 == "hands" && part.2 == MALE_HANDS })
    );
}

/// An `NPC_` like [`items::npc`] that also carries a `QNAM` skin tone.
fn npc_with_skin_tone(id: u32, race_id: u32, tone: [f32; 3]) -> Vec<u8> {
    items::record(
        b"NPC_",
        id,
        &[
            (b"EDID", items::text("TonedNpc")),
            (b"ACBS", vec![0u8; 24]),
            (b"RNAM", race_id.to_le_bytes().to_vec()),
            (
                b"QNAM",
                tone.iter()
                    .flat_map(|channel| channel.to_le_bytes())
                    .collect(),
            ),
        ],
    )
}

/// The `actor_models` skin tone columns of `id`.
fn stored_skin_tone(connection: &Connection, id: u32) -> [Option<f64>; 3] {
    connection
        .query_row(
            "SELECT skin_tone_r, skin_tone_g, skin_tone_b FROM actor_models WHERE id=?1",
            [id],
            |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?]),
        )
        .unwrap()
}

#[test]
fn qnam_skin_tone_is_published_and_absent_qnam_stays_null() {
    let directory = tempfile::tempdir().unwrap();
    let tone = [198.0 / 255.0, 176.0 / 255.0, 168.0 / 255.0];
    let mut records = vec![
        npc_with_skin_tone(0x100, 0x200, tone),
        items::npc(0x101, 0x200, false),
        npc_with_skin_tone(0x102, 0x200, [0.0; 3]),
    ];
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let toned = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(toned.skin_tone, Some(tone));
    let plain = catalog.models.iter().find(|row| row.id == 0x101).unwrap();
    assert_eq!(plain.skin_tone, None);
    assert_eq!(
        catalog
            .models
            .iter()
            .find(|row| row.id == 0x102)
            .unwrap()
            .skin_tone,
        Some([0.0; 3])
    );

    let connection = export(directory.path(), &path);
    assert_eq!(
        stored_skin_tone(&connection, 0x100),
        tone.map(|channel| Some(f64::from(channel)))
    );
    assert_eq!(stored_skin_tone(&connection, 0x101), [None; 3]);
    assert_eq!(stored_skin_tone(&connection, 0x102), [Some(0.0); 3]);
}

/// QNAM is not a confirmed Traits inheritance field. Neither the child's own tone nor the root's
/// tone is published for this source NPC.
#[test]
fn traits_enabled_source_does_not_inherit_qnam_from_child_or_root() {
    let directory = tempfile::tempdir().unwrap();
    // QNAM inheritance for Traits-enabled NPCs is unproved.
    let root = items::record(
        b"NPC_",
        0x101,
        &[
            (b"ACBS", vec![0u8; 24]),
            (b"RNAM", 0x200u32.to_le_bytes().to_vec()),
            (
                b"QNAM",
                [0.25_f32, 0.5, 0.75]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            ),
        ],
    );
    let child = items::record(
        b"NPC_",
        0x100,
        &[
            (b"ACBS", {
                let mut v = vec![0u8; 24];
                v[18] = 1;
                v
            }),
            (b"TPLT", 0x101u32.to_le_bytes().to_vec()),
            (b"RNAM", 0x200u32.to_le_bytes().to_vec()),
            (
                b"QNAM",
                [0.1_f32, 0.2, 0.3]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            ),
        ],
    );
    let records = vec![
        child,
        root,
        items::race(0x200, "NordRace", Some(0x300), false),
        items::skin_armor(0x300, &[0x400]),
        items::armature(0x400, 0x200, items::BIPED_BODY, Some(MALE_BODY), None),
    ];
    let path = write_plugin(directory.path(), "TraitsQnam.esp", &records);
    let catalog = resolve(&path);
    assert_eq!(
        catalog
            .models
            .iter()
            .find(|row| row.id == 0x100)
            .unwrap()
            .skin_tone,
        None
    );
    assert_eq!(
        catalog
            .models
            .iter()
            .find(|row| row.id == 0x101)
            .unwrap()
            .skin_tone,
        Some([0.25, 0.5, 0.75])
    );
}

/// The `actor_models.weight` of `id`.
fn stored_weight(connection: &Connection, id: u32) -> Option<f64> {
    connection
        .query_row("SELECT weight FROM actor_models WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn nam7_body_weight_is_published_and_an_absent_nam7_stays_null() {
    let directory = tempfile::tempdir().unwrap();
    let mut records = vec![
        items::npc_with_weight(0x100, 0x200, false, 20.0),
        items::npc(0x101, 0x200, false),
        items::npc_with_weight(0x102, 0x200, false, 100.0),
        items::npc_with_weight(0x103, 0x200, false, 0.0),
    ];
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let light = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(light.weight, Some(20.0));
    let plain = catalog.models.iter().find(|row| row.id == 0x101).unwrap();
    assert_eq!(plain.weight, None);
    let heavy = catalog.models.iter().find(|row| row.id == 0x102).unwrap();
    assert_eq!(heavy.weight, Some(100.0));
    let thin = catalog.models.iter().find(|row| row.id == 0x103).unwrap();
    assert_eq!(thin.weight, Some(0.0));

    let connection = export(directory.path(), &path);
    assert_eq!(stored_weight(&connection, 0x100), Some(20.0));
    assert_eq!(stored_weight(&connection, 0x101), None);
    assert_eq!(stored_weight(&connection, 0x102), Some(100.0));
    assert_eq!(stored_weight(&connection, 0x103), Some(0.0));
}

#[test]
fn an_out_of_range_or_non_finite_nam7_stays_null() {
    let directory = tempfile::tempdir().unwrap();
    let weights = [100.5_f32, -0.5, f32::NAN, f32::INFINITY];
    let mut records: Vec<Vec<u8>> = weights
        .iter()
        .enumerate()
        .map(|(index, weight)| items::npc_with_weight(0x100 + index as u32, 0x200, false, *weight))
        .collect();
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let connection = export(directory.path(), &path);
    for (index, weight) in weights.iter().enumerate() {
        let id = 0x100 + index as u32;
        let row = catalog.models.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.weight, None, "{weight}");
        assert_eq!(stored_weight(&connection, id), None, "{weight}");
    }
}

#[test]
fn nam7_with_non_four_byte_payloads_is_rejected_as_malformed() {
    let directory = tempfile::tempdir().unwrap();
    let trailing = [25.0_f32.to_le_bytes().as_slice(), &[0xAA]].concat();
    let payloads = [vec![0x00, 0x00, 0xC8], trailing];
    let mut records: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            items::npc_with_weight_payload(0x100 + index as u32, 0x200, false, payload)
        })
        .collect();
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let connection = export(directory.path(), &path);
    for (index, payload) in payloads.iter().enumerate() {
        let id = 0x100 + index as u32;
        let row = catalog.models.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.weight, None, "payload {payload:?}");
        assert_eq!(stored_weight(&connection, id), None, "payload {payload:?}");
    }
}

#[test]
fn a_templated_npc_keeps_its_own_nam7_and_never_inherits_one() {
    let directory = tempfile::tempdir().unwrap();
    let traits_npc = |id: u32, race: u32, weight: Option<f32>| {
        let mut acbs = vec![0u8; 24];
        acbs[18..20].copy_from_slice(&1u16.to_le_bytes());
        let mut fields = vec![
            (b"ACBS", acbs),
            (b"TPLT", 0x900u32.to_le_bytes().to_vec()),
            (b"RNAM", race.to_le_bytes().to_vec()),
        ];
        if let Some(weight) = weight {
            fields.push((b"NAM7", weight.to_le_bytes().to_vec()));
        }
        items::record(b"NPC_", id, &fields)
    };
    let mut records = vec![
        // Traits inherit race from template; NAM7 remains own-record only.
        items::npc_with_weight(0x900, 0x201, false, 80.0),
        traits_npc(0x100, 0x200, Some(20.0)),
        traits_npc(0x101, 0x200, None),
        items::race(0x200, "OwnRace", Some(0x300), false),
        items::race(0x201, "TemplateRace", Some(0x301), false),
    ];
    records.extend(nord_body_only(0x300));
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);

    let catalog = resolve(&path);
    let no_nam7 = catalog.models.iter().find(|row| row.id == 0x101).unwrap();
    assert_eq!(
        no_nam7.race_id, 0x201,
        "traits must resolve through template"
    );
    assert_eq!(no_nam7.weight, None);
    let own = catalog.models.iter().find(|row| row.id == 0x100).unwrap();
    assert_eq!(own.race_id, 0x201, "traits must resolve through template");
    assert_eq!(own.weight, Some(20.0));

    let connection = export(directory.path(), &path);
    assert_eq!(stored_weight(&connection, 0x100), Some(20.0));
    assert_eq!(stored_weight(&connection, 0x101), None);
}

/// A database whose `actor_models` predates the skin tone and weight columns gains them when the
/// tables are created again, so a subset export into it can still insert its rows.
#[test]
fn an_older_actor_models_table_gains_the_skin_tone_and_weight_columns() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE actor_models (
                 id INTEGER PRIMARY KEY, race_id INTEGER NOT NULL, sex TEXT NOT NULL,
                 skin_armo_id INTEGER, head_model TEXT, head_source TEXT,
                 resolution TEXT NOT NULL
             );
             INSERT INTO actor_models(id, race_id, sex, resolution)
                 VALUES (0x100, 0x200, 'male', 'body');",
        )
        .unwrap();
    converter::esm::exporter::create_tables(&connection).unwrap();
    converter::esm::exporter::create_tables(&connection).unwrap();
    assert_eq!(stored_skin_tone(&connection, 0x100), [None; 3]);
    assert_eq!(stored_weight(&connection, 0x100), None);
}

const FEMALE_HANDS: &str = "actors\\character\\character assets\\femalehands_1.nif";

/// A `NordRace` whose skin holds an exact-race body and a `DefaultRace` hands armature, each with
/// a male and a female skin texture set, and one NPC of each sex.
fn nord_with_skin_texture_sets() -> Vec<Vec<u8>> {
    vec![
        items::npc(0x100, 0x200, false),
        items::npc(0x101, 0x200, true),
        items::race(0x200, "NordRace", Some(0x300), false),
        items::skin_armor(0x300, &[0x400, 0x401]),
        items::armature_with_skins(
            0x400,
            0x200,
            items::BIPED_BODY,
            (Some(MALE_BODY), Some(FEMALE_BODY)),
            (Some(0x500), Some(0x501)),
        ),
        items::armature_with_skins(
            0x401,
            items::DEFAULT_RACE_FORM_ID,
            items::BIPED_HANDS,
            (Some(MALE_HANDS), Some(FEMALE_HANDS)),
            (Some(0x502), None),
        ),
    ]
}

#[test]
fn parts_carry_the_armature_race_and_the_sex_skin_texture_set() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_plugin(
        directory.path(),
        "Skyrim.esm",
        &nord_with_skin_texture_sets(),
    );

    let catalog = resolve(&path);
    let parts: Vec<_> = catalog
        .parts
        .iter()
        .map(|part| {
            (
                part.npc_id,
                part.slot.as_str(),
                part.armature_race_id,
                part.skin_texture_set_id,
            )
        })
        .collect();
    assert_eq!(
        parts,
        [
            (0x100, "body", 0x200, Some(0x500)),
            (0x100, "hands", items::DEFAULT_RACE_FORM_ID, Some(0x502)),
            (0x101, "body", 0x200, Some(0x501)),
            // NAM1 is absent: the female hands keep the NIF's own textures.
            (0x101, "hands", items::DEFAULT_RACE_FORM_ID, None),
        ]
    );

    let connection = export(directory.path(), &path);
    let stored: Vec<(u32, String, u32, Option<u32>)> = connection
        .prepare(
            "SELECT npc_id, slot, armature_race_id, skin_texture_set_id FROM actor_model_parts \
             ORDER BY npc_id, slot",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let expected: Vec<(u32, String, u32, Option<u32>)> = parts
        .iter()
        .map(|(npc, slot, race, set)| (*npc, (*slot).to_owned(), *race, *set))
        .collect();
    assert_eq!(stored, expected);
}

#[test]
fn the_default_body_takes_the_default_race_armature_skin_texture_sets() {
    let directory = tempfile::tempdir().unwrap();
    let mut records = nord_with_skin_texture_sets();
    records.push(
        dummy_content::esm::texture_set(
            0x502,
            [
                Some("actors/character/male/malehands_1.dds"),
                Some("actors/character/male/malehands_1_msn.dds"),
                None,
                Some("actors/character/male/malehands_1_sk.dds"),
                None,
                None,
                None,
                Some("actors/character/male/malehands_1_s.dds"),
            ],
        )
        .unwrap(),
    );
    let path = write_plugin(directory.path(), "Skyrim.esm", &records);
    let database = directory.path().join("skyrim_world.db");
    drop(export(directory.path(), &path));

    let male = converter::compose::default_skin_texture_sets(&database, "male").unwrap();
    assert_eq!(
        male.len(),
        1,
        "only the hands armature is DefaultRace's: {male:?}"
    );
    assert_eq!(male[0].kind, converter::compose::BodyPartKind::Hands);
    assert_eq!(male[0].model_path, MALE_HANDS);
    let slots: Vec<Option<&str>> = male[0]
        .textures
        .slots
        .iter()
        .map(Option::as_deref)
        .collect();
    // TX03 is NIF slot 2 and TX07 slot 7.
    assert_eq!(
        slots,
        [
            Some("actors/character/male/malehands_1.dds"),
            Some("actors/character/male/malehands_1_msn.dds"),
            Some("actors/character/male/malehands_1_sk.dds"),
            None,
            None,
            None,
            None,
            Some("actors/character/male/malehands_1_s.dds"),
        ]
    );
    assert!(
        converter::compose::skin_texture_set_for(
            &male,
            converter::compose::BodyPartKind::Hands,
            Path::new("/data/meshes/actors/character/character assets/MaleHands_1.nif"),
        )
        .is_some()
    );
    assert!(
        converter::compose::skin_texture_set_for(
            &male,
            converter::compose::BodyPartKind::Hands,
            Path::new("/data/meshes/actors/character/character assets/malehandsargonian_1.nif"),
        )
        .is_none()
    );
    assert!(
        converter::compose::default_skin_texture_sets(&database, "female")
            .unwrap()
            .is_empty()
    );
}

/// A database whose `actor_model_parts` predates the skin columns gains them when the tables are
/// created again.
#[test]
fn an_older_actor_model_parts_table_gains_the_skin_columns() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE actor_model_parts (
                 npc_id INTEGER NOT NULL, slot TEXT NOT NULL, model_path TEXT NOT NULL,
                 PRIMARY KEY (npc_id, slot)
             );
             INSERT INTO actor_model_parts VALUES (0x100, 'hands', 'hands.nif');",
        )
        .unwrap();
    converter::esm::exporter::create_tables(&connection).unwrap();
    converter::esm::exporter::create_tables(&connection).unwrap();
    let stored: (Option<u32>, Option<u32>) = connection
        .query_row(
            "SELECT armature_race_id, skin_texture_set_id FROM actor_model_parts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, (None, None));
}

/// Retail bodies: the full resolved/unresolved breakdown, Alvor's row and the
/// `statics` invariant. The 2,392 count is printed, not asserted.
#[test]
#[ignore = "requires MUDCRAB_SKYRIM_DATA with locally installed game assets"]
fn referenced_npc_bases_resolve_bodies() {
    let Some(directory) = std::env::var_os("MUDCRAB_SKYRIM_DATA").map(PathBuf::from) else {
        return;
    };
    let plugin = directory.join("Skyrim.esm");
    let temp = tempfile::tempdir().unwrap();
    let connection = export(temp.path(), &plugin);

    let overlap: i64 = connection
        .query_row(
            "SELECT count(*) FROM statics WHERE id IN (SELECT id FROM npcs)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlap, 0, "no NPC_ row is written into statics");

    let referenced: HashSet<u32> = connection
        .prepare(
            "SELECT DISTINCT base_form_id FROM \"references\" WHERE base_form_id IN (SELECT id FROM npcs)",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!referenced.is_empty());

    let catalog = resolve(&plugin);
    let alvor = catalog
        .models
        .iter()
        .find(|row| row.id == 0x0001_3475)
        .expect("Alvor's row");
    assert_eq!(alvor.race_id, 79686);
    assert_eq!(alvor.sex, "male");
    assert_eq!(alvor.skin_armo_id, Some(3428));
    assert_eq!(alvor.head_source, Some("facegen"));
    assert_eq!(
        alvor.head_model.as_deref(),
        Some("meshes/actors/character/facegendata/facegeom/skyrim.esm/00013475.nif")
    );

    let mut parts: Vec<_> = catalog
        .parts
        .iter()
        .filter(|part| part.npc_id == 0x0001_3475)
        .collect();
    parts.sort_by_key(|part| part.slot);
    assert_eq!(parts.len(), 3, "Alvor has body, hands and feet: {parts:?}");
    for (part, expected) in
        parts
            .iter()
            .zip(["malebody_1.nif", "malehands_1.nif", "malefeet_1.nif"])
    {
        assert!(
            part.model_path.to_lowercase().ends_with(expected),
            "{}: {}",
            part.slot.as_str(),
            part.model_path
        );
    }

    let sigrid = catalog
        .models
        .iter()
        .find(|row| row.id == 0x0001_3476)
        .expect("Sigrid's row");
    println!("Sigrid (0x00013476) ACBS sex: {}", sigrid.sex);
    println!("Alvor: {alvor:?}");
    // Retail `QNAM` bytes are whole steps of 1/255: Alvor 198,176,168 and Sigrid 221,221,221.
    for (row, encoded) in [(alvor, [198.0_f32, 176.0, 168.0]), (sigrid, [221.0; 3])] {
        let tone = row.skin_tone.expect("retail NPC_ carries QNAM");
        for (channel, expected) in tone.iter().zip(encoded) {
            assert!(
                (channel * 255.0 - expected).abs() < 1e-3,
                "{:08X}: {tone:?}",
                row.id
            );
        }
        assert_eq!(
            stored_skin_tone(&connection, row.id),
            tone.map(|channel| Some(f64::from(channel)))
        );
    }
    for part in &parts {
        println!("Alvor part {}: {}", part.slot.as_str(), part.model_path);
    }

    let referenced_models: Vec<_> = catalog
        .models
        .iter()
        .filter(|row| referenced.contains(&row.id))
        .collect();
    let resolved = referenced_models
        .iter()
        .filter(|row| row.resolution != "unresolved")
        .count();
    let mut reasons: HashMap<&str, usize> = HashMap::new();
    for diagnostic in &catalog.diagnostics {
        if referenced.contains(&diagnostic.npc_id)
            && catalog
                .models
                .iter()
                .any(|row| row.id == diagnostic.npc_id && row.resolution == "unresolved")
        {
            *reasons.entry(diagnostic.reason.as_str()).or_default() += 1;
        }
    }
    println!(
        "referenced NPC bases: {} total, {resolved} resolved, {} unresolved",
        referenced_models.len(),
        referenced_models.len() - resolved
    );
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    for (reason, count) in reasons {
        println!("  unresolved {reason}: {count}");
    }
}

/// Retail FaceGen heads: the count is printed, not asserted.
#[test]
#[ignore = "requires MUDCRAB_SKYRIM_DATA with locally installed game assets"]
fn referenced_npc_bases_resolve_facegen_heads() {
    let Some(directory) = std::env::var_os("MUDCRAB_SKYRIM_DATA").map(PathBuf::from) else {
        return;
    };
    let plugin = directory.join("Skyrim.esm");
    let temp = tempfile::tempdir().unwrap();
    let connection = export(temp.path(), &plugin);
    let referenced: HashSet<u32> = connection
        .prepare(
            "SELECT DISTINCT base_form_id FROM \"references\" WHERE base_form_id IN (SELECT id FROM npcs)",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    let catalog = resolve(&plugin);
    let facegen = catalog
        .models
        .iter()
        .filter(|row| referenced.contains(&row.id) && row.head_source == Some("facegen"))
        .count();
    println!(
        "referenced NPC bases with a FaceGen head: {facegen} of {}",
        referenced.len()
    );
    assert!(facegen > 0);
}

fn creature_head_slot_race(project: &str, facegen: bool) -> Vec<u8> {
    let mut data = vec![0; 36];
    if facegen {
        data[32..36].copy_from_slice(&2_u32.to_le_bytes());
    }
    items::record(
        b"RACE",
        0x200,
        &[
            (b"EDID", items::text("SyntheticCreature")),
            (b"WNAM", 0x300_u32.to_le_bytes().to_vec()),
            (b"DATA", data),
            (b"MNAM", Vec::new()),
            (b"ANAM", items::text("Actors/Custom/skeleton.nif")),
            (b"FNAM", Vec::new()),
            (b"ANAM", items::text("Actors/Custom/skeleton.nif")),
            (b"NAM3", Vec::new()),
            (b"MNAM", Vec::new()),
            (b"MODL", items::text(project)),
            (b"FNAM", Vec::new()),
            (b"MODL", items::text(project)),
        ],
    )
}

fn head_slot_armature(id: u32, path: &str) -> Vec<u8> {
    let mut bod = vec![0; 12];
    bod[..4].copy_from_slice(&1_u32.to_le_bytes());
    items::record(
        b"ARMA",
        id,
        &[
            (b"BODT", bod),
            (b"RNAM", 0x200_u32.to_le_bytes().to_vec()),
            (b"MOD2", items::text(path)),
        ],
    )
}

#[test]
fn creature_whole_skin_in_retail_head_slot_becomes_its_body() {
    for female in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let records = vec![
            items::npc(0x100, 0x200, female),
            creature_head_slot_race("Actors/Custom/CreatureProject.hkx", false),
            items::skin_armor(0x300, &[0x400]),
            head_slot_armature(0x400, "Actors/Custom/whole_creature.nif"),
        ];
        let path = write_plugin(directory.path(), "Skyrim.esm", &records);
        let catalog = resolve(&path);
        let part = catalog
            .parts
            .iter()
            .find(|row| row.npc_id == 0x100 && row.slot.as_str() == "body")
            .unwrap();
        assert_eq!(part.model_path, "Actors/Custom/whole_creature.nif");
        assert_eq!(
            catalog
                .models
                .iter()
                .find(|row| row.id == 0x100)
                .unwrap()
                .resolution,
            "body"
        );
        let db = export(directory.path(), &path);
        assert_eq!(
            db.query_row(
                "SELECT model_path FROM actor_model_parts WHERE npc_id=256 AND slot='body'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            part.model_path
        );
    }
}

#[test]
fn creature_head_slot_never_displaces_a_body_or_guesses_between_models() {
    let directory = tempfile::tempdir().unwrap();
    let base = || {
        vec![
            items::npc(0x100, 0x200, false),
            creature_head_slot_race("Actors/Custom/CreatureProject.hkx", false),
            items::skin_armor(0x300, &[0x400, 0x401]),
            head_slot_armature(0x400, "Actors/Custom/head.nif"),
        ]
    };
    let mut records = base();
    records.push(items::armature(
        0x401,
        0x200,
        items::BIPED_BODY,
        Some(OTHER_BODY),
        None,
    ));
    let path = write_plugin(directory.path(), "body.esm", &records);
    assert_eq!(resolve(&path).parts[0].model_path, OTHER_BODY);
    let mut records = base();
    records.push(head_slot_armature(0x401, "Actors/Custom/another.nif"));
    let path = write_plugin(directory.path(), "ambiguous.esm", &records);
    let catalog = resolve(&path);
    assert!(catalog.parts.is_empty());
    assert!(
        catalog
            .diagnostics
            .iter()
            .any(|row| row.reason == UnresolvedReason::AmbiguousCreatureBody)
    );
}

#[test]
fn facegen_and_shared_human_projects_do_not_treat_a_head_as_a_body() {
    let directory = tempfile::tempdir().unwrap();
    for (project, facegen) in [
        ("Actors/Custom/CreatureProject.hkx", true),
        ("Actors/Character/DefaultMale.hkx", false),
    ] {
        let path = write_plugin(
            directory.path(),
            "human.esm",
            &[
                items::npc(0x100, 0x200, false),
                creature_head_slot_race(project, facegen),
                items::skin_armor(0x300, &[0x400]),
                head_slot_armature(0x400, "Actors/Custom/head.nif"),
            ],
        );
        assert!(resolve(&path).parts.is_empty());
    }
}
