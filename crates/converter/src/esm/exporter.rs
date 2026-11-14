use crate::esm::{
    emittance,
    extractors::{SubrecordView, extract_cell_info, extract_land_data, serialize_subrecords},
    load_order::{LoadOrder, StableId},
    projection_omissions::{OmissionCategory, ProjectionOmissions},
    records::{RawRecord, achr::SubrecordXESP},
};
use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    esm::records::record_type::vmad::parse_vmad,
};
use rusqlite::{Connection, Result, Statement, ToSql, Transaction, params};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    str::from_utf8,
};

const CELL_SIZE: f32 = 4096.0;
const RECORDS_INSERT_SQL: &str = "INSERT OR REPLACE INTO records(form_id, record_type, cell_id, worldspace_id, load_order, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// Validate the fixed row shape once before any raw-bound record is executed.
fn check_record_insert(statement: &Statement<'_>) -> Result<()> {
    let expected = statement.parameter_count();
    if expected != 6 {
        // Match the old positional-slice cardinality diagnostic. This
        // constant SQL always has six; never step a drifted statement.
        let observed = 6.min(expected.saturating_add(1));
        return Err(rusqlite::Error::InvalidParameterCount(observed, expected));
    }
    Ok(())
}

/// Write one canonical row with unchanged positional values and clear successful bindings.
/// A failed bind or execute returns before clearing; dropping the cached statement resets it.
#[inline]
fn insert_canonical_record(
    statement: &mut Statement<'_>,
    form_id: u32,
    record_type: impl ToSql,
    cell_form_id: Option<u32>,
    worldspace_form_id: Option<u32>,
    load_order: u32,
    data: &[u8],
) -> Result<usize> {
    statement.raw_bind_parameter(1, form_id)?;
    statement.raw_bind_parameter(2, record_type)?;
    statement.raw_bind_parameter(3, cell_form_id)?;
    statement.raw_bind_parameter(4, worldspace_form_id)?;
    statement.raw_bind_parameter(5, load_order)?;
    statement.raw_bind_parameter(6, data)?;
    let changed = statement.raw_execute()?;
    statement.clear_bindings();
    Ok(changed)
}

#[cfg(test)]
#[path = "canonical_bind_tests.rs"]
mod canonical_bind_tests;

fn invalid_movement_field(form_id: u32, field: &str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("record {form_id:08X} has invalid {field}"),
    )))
}

fn decode_npc_default_speeds(view: &SubrecordView<'_>, form_id: u32) -> Result<[f32; 8]> {
    if view.get_string(b"EDID").as_deref() != Some("NPC_Default_MT") {
        return Err(invalid_movement_field(
            form_id,
            "MOVT EDID (expected NPC_Default_MT)",
        ));
    }
    let data = view
        .find(b"SPED")
        .ok_or_else(|| invalid_movement_field(form_id, "MOVT SPED"))?;
    if data.len() != 40 && data.len() != 44 {
        return Err(invalid_movement_field(
            form_id,
            "MOVT SPED length (expected 40 or 44 bytes)",
        ));
    }
    let mut speeds = [0.0; 8];
    for (index, speed) in speeds.iter_mut().enumerate() {
        *speed = f32::from_le_bytes(data[index * 4..index * 4 + 4].try_into().unwrap());
        if !speed.is_finite() || *speed <= 0.0 {
            return Err(invalid_movement_field(
                form_id,
                &format!("MOVT SPED[{index}]"),
            ));
        }
    }
    Ok(speeds)
}

fn decode_movement_setting(view: &SubrecordView<'_>, form_id: u32) -> Result<f32> {
    let data = view
        .find(b"DATA")
        .ok_or_else(|| invalid_movement_field(form_id, "GMST DATA"))?;
    if data.len() != 4 {
        return Err(invalid_movement_field(
            form_id,
            "GMST DATA length (expected 4 bytes)",
        ));
    }
    let value = f32::from_le_bytes(data.try_into().unwrap());
    if !value.is_finite() {
        return Err(invalid_movement_field(form_id, "GMST DATA value"));
    }
    Ok(value)
}

fn optional_race_movement_link(
    view: &SubrecordView<'_>,
    form_id: u32,
    tag: &[u8; 4],
) -> Result<Option<u32>> {
    let Some(data) = view.find(tag) else {
        return Ok(None);
    };
    if data.len() != 4 {
        return Err(invalid_movement_field(
            form_id,
            &format!(
                "RACE {} length (expected 4 bytes)",
                String::from_utf8_lossy(tag)
            ),
        ));
    }
    let value = u32::from_le_bytes(data.try_into().unwrap());
    Ok((value != 0).then_some(value))
}

/// Creates any missing world-database tables and stamps an empty database with the current schema.
pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_info (version INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS plugins (
             id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, priority INTEGER NOT NULL, checksum BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS records (
             form_id INTEGER PRIMARY KEY, record_type TEXT NOT NULL, cell_id INTEGER,
             worldspace_id INTEGER, load_order INTEGER NOT NULL, data BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_records_type ON records(record_type);
         CREATE INDEX IF NOT EXISTS idx_records_cell_id ON records(cell_id) WHERE cell_id IS NOT NULL;
         CREATE TABLE IF NOT EXISTS worldspaces (
             id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL, parent_world INTEGER, flags INTEGER NOT NULL,
             lod_origin_x INTEGER, lod_origin_y INTEGER, full_name TEXT
         );
         CREATE TABLE IF NOT EXISTS cells (
             id INTEGER PRIMARY KEY, worldspace_id INTEGER, grid_x INTEGER, grid_y INTEGER,
             interior_name TEXT, flags INTEGER NOT NULL, data BLOB, full_name TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_cells_grid ON cells(worldspace_id, grid_x, grid_y);
         CREATE TABLE IF NOT EXISTS "references" (
             id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL, worldspace_id INTEGER,
             base_form_id INTEGER NOT NULL, is_exterior INTEGER NOT NULL,
             pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
             local_x REAL, local_y REAL, rot_x REAL NOT NULL, rot_y REAL NOT NULL,
             rot_z REAL NOT NULL, scale REAL NOT NULL DEFAULT 1.0,
             radius_override REAL, header_flags INTEGER NOT NULL DEFAULT 0,
             enable_parent_id INTEGER, enable_parent_flags INTEGER, emittance_id INTEGER, data BLOB
         );
         CREATE INDEX IF NOT EXISTS idx_references_cell ON "references"(cell_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS exterior_spatial USING rtree(
             id, minX, maxX, minY, maxY, minZ, maxZ, +cell_id, +worldspace_id
         );
         CREATE TABLE IF NOT EXISTS land (
             cell_id INTEGER PRIMARY KEY, heightmap BLOB NOT NULL, vtex BLOB, vclr BLOB, normals BLOB
         );
         CREATE TABLE IF NOT EXISTS statics (
             id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT, flags INTEGER NOT NULL,
             full_name TEXT,
             bounds_min_x REAL NOT NULL DEFAULT -64, bounds_min_y REAL NOT NULL DEFAULT -64,
             bounds_min_z REAL NOT NULL DEFAULT -64, bounds_max_x REAL NOT NULL DEFAULT 64,
             bounds_max_y REAL NOT NULL DEFAULT 64, bounds_max_z REAL NOT NULL DEFAULT 64,
             bounds_valid INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_statics_editor_id ON statics(editor_id);
         CREATE TABLE IF NOT EXISTS lights (
             id INTEGER PRIMARY KEY,        -- LIGH FormID
             editor_id TEXT,
             radius REAL NOT NULL,          -- Creation units
             color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
             flags INTEGER NOT NULL,        -- DATA flags (dynamic, can carry, negative, flicker, off by default, ...)
             falloff REAL NOT NULL,
             fade REAL,                     -- FNAM, if present
             time INTEGER, field_of_view REAL, near_clip REAL,
             flicker_period REAL, flicker_intensity_amplitude REAL, flicker_movement_amplitude REAL,
             value INTEGER, weight REAL,
             sound_id INTEGER,              -- SNAM -> SNDR
             lens_id INTEGER                -- LNAM -> LENS
         );
         CREATE TABLE IF NOT EXISTS emittance (
             id INTEGER PRIMARY KEY,        -- XEMI target: a LIGH or REGN FormID
             source_type TEXT NOT NULL,     -- 'LIGH' or 'REGN'
             weather_id INTEGER,            -- REGN: the WTHR its colours come from
             sunrise_r INTEGER NOT NULL, sunrise_g INTEGER NOT NULL, sunrise_b INTEGER NOT NULL,
             day_r INTEGER NOT NULL, day_g INTEGER NOT NULL, day_b INTEGER NOT NULL,
             sunset_r INTEGER NOT NULL, sunset_g INTEGER NOT NULL, sunset_b INTEGER NOT NULL,
             night_r INTEGER NOT NULL, night_g INTEGER NOT NULL, night_b INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS npcs (
             id INTEGER PRIMARY KEY, editor_id TEXT, full_name TEXT,
             race_id INTEGER, class_id INTEGER, flags INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_npcs_editor_id ON npcs(editor_id);
         CREATE TABLE IF NOT EXISTS actor_models (
             id INTEGER PRIMARY KEY,          -- NPC_ FormID
             race_id INTEGER NOT NULL,
             sex TEXT NOT NULL,               -- 'male' | 'female'
             skin_armo_id INTEGER,            -- effective NPC WNAM override, else effective RACE/WNAM
             head_model TEXT,                 -- FaceGen or default head NIF path
             head_source TEXT,                -- 'facegen' | 'default'
             resolution TEXT NOT NULL,        -- 'body', 'body_head', 'unresolved'
             skin_tone_r REAL,                -- NPC_/QNAM, 0..1, encoded scale
             skin_tone_g REAL,
             skin_tone_b REAL,
             weight REAL                      -- NPC_/NAM7, 0..100
         );
         CREATE TABLE IF NOT EXISTS actor_model_parts (
             npc_id INTEGER NOT NULL,
             slot TEXT NOT NULL,              -- 'body' | 'hands' | 'feet'
             model_path TEXT NOT NULL,
             armature_race_id INTEGER,        -- ARMA/RNAM: the NPC's race or DefaultRace (25)
             skin_texture_set_id INTEGER,     -- ARMA/NAM0 (male) or NAM1 (female), a TXST
             PRIMARY KEY (npc_id, slot)
         );
         CREATE TABLE IF NOT EXISTS movement_types (
             id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL,
             left_walk REAL NOT NULL, left_run REAL NOT NULL,
             right_walk REAL NOT NULL, right_run REAL NOT NULL,
             forward_walk REAL NOT NULL, forward_run REAL NOT NULL,
             back_walk REAL NOT NULL, back_run REAL NOT NULL,
             load_order INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS race_movement_links (
             id INTEGER PRIMARY KEY, walk_movt_id INTEGER, run_movt_id INTEGER,
             load_order INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS movement_game_settings (
             id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL,
             value REAL NOT NULL, load_order INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS lod (
             cell_id INTEGER NOT NULL, lod_level INTEGER NOT NULL, mesh_data BLOB NOT NULL,
             PRIMARY KEY (cell_id, lod_level)
         );
         CREATE TABLE IF NOT EXISTS lod_chunks (
             worldspace_id INTEGER NOT NULL, tier INTEGER NOT NULL,
             anchor_x INTEGER NOT NULL, anchor_y INTEGER NOT NULL,
             payload_path TEXT NOT NULL, content_hash TEXT NOT NULL,
             bounds_min_x REAL NOT NULL, bounds_min_y REAL NOT NULL, bounds_min_z REAL NOT NULL,
             bounds_max_x REAL NOT NULL, bounds_max_y REAL NOT NULL, bounds_max_z REAL NOT NULL,
             source_cells TEXT NOT NULL DEFAULT '',
             PRIMARY KEY (worldspace_id, tier, anchor_x, anchor_y)
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS lod_chunks_spatial USING rtree(
             id, minX, maxX, minY, maxY, +worldspace_id, +tier, +anchor_x, +anchor_y
         );
         CREATE TABLE IF NOT EXISTS lod_build (
             id INTEGER PRIMARY KEY CHECK (id = 1), build_identity TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS waters (
             id INTEGER PRIMARY KEY, editor_id TEXT, opacity INTEGER, flags INTEGER NOT NULL,
             shallow_color INTEGER, deep_color INTEGER, reflection_color INTEGER,
             fresnel REAL, reflectivity REAL,
             flow_normal_path TEXT, data BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS texture_sets (
             id INTEGER PRIMARY KEY, editor_id TEXT, diffuse_path TEXT, normal_path TEXT,
             glow_path TEXT, height_path TEXT, environment_path TEXT, mask_path TEXT,
             specular_path TEXT, detail_path TEXT
         );
         CREATE TABLE IF NOT EXISTS landscape_textures (
             id INTEGER PRIMARY KEY, editor_id TEXT, texture_set_id INTEGER,
             material_type INTEGER, friction REAL, restitution REAL
         );
         CREATE TABLE IF NOT EXISTS grass_types (
             id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT,
             density INTEGER, min_slope INTEGER, max_slope INTEGER,
             units_from_water INTEGER, water_comparison INTEGER,
             position_range REAL, height_range REAL, color_range REAL,
             wave_period REAL, flags INTEGER, load_order INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS landscape_texture_grasses (
             ltex_id INTEGER NOT NULL, gras_id INTEGER NOT NULL,
             PRIMARY KEY (ltex_id, gras_id)
         );
         CREATE TABLE IF NOT EXISTS scripts (
             form_id INTEGER NOT NULL, script_name TEXT NOT NULL,
             vmad BLOB NOT NULL, properties_json TEXT NOT NULL,
             PRIMARY KEY (form_id, script_name)
         );
         CREATE TABLE IF NOT EXISTS formid_map (
             form_id INTEGER PRIMARY KEY, plugin_name TEXT NOT NULL, internal_id INTEGER NOT NULL, record_type TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS conversion_cache (
             plugin_path TEXT PRIMARY KEY, file_hash BLOB NOT NULL, last_converted INTEGER NOT NULL
         );"#
    )?;
    // Optional interior room graph (db-schema.md section 15); an older reused database gains it
    // here, and an older-shaped `room_portals` (no `pair_index`) is migrated in place.
    crate::esm::rooms::migrate_room_portals(conn)?;
    conn.execute_batch(crate::esm::rooms::ROOM_TABLES_SQL)?;
    // Typed race animation identity (db-schema.md, actor models). Archive-only snapshots keep
    // the same schema and defer its rows like the other runtime views.
    conn.execute_batch(crate::animation::authoring::RACE_ANIMATION_SOURCES_SQL)?;
    // Optional projection omission counters (db-schema.md section 18, #297).
    conn.execute_batch(crate::esm::projection_omissions::PROJECTION_OMISSION_TABLES_SQL)?;
    // `CREATE TABLE IF NOT EXISTS` leaves a reused older table as it was; give it the optional
    // display-name columns (#269), so a subset export can still insert into it.
    for table in ["worldspaces", "cells", "statics"] {
        add_missing_columns(conn, table, &[("full_name", "TEXT")])?;
    }

    // Optional environment, weather and image-space tables (db-schema.md, #269 gaps 1 and 2).
    conn.execute_batch(&crate::esm::environment::tables_sql())?;

    // Optional reference link and detail tables (db-schema.md, #269 gap 3).
    conn.execute_batch(crate::esm::reference_links::REFERENCE_LINK_TABLES_SQL)?;
    // `CREATE TABLE IF NOT EXISTS` leaves a reused older table as it was; give it the columns
    // later exports added, so a subset export can still insert into it.
    add_missing_columns(conn, "lights", LIGHT_DETAIL_COLUMNS)?;
    add_missing_columns(conn, "lights", &[("fade_status", "INTEGER")])?;

    // Optional load-door table (db-schema.md section 17), added the same way.
    super::doors::ensure_table(conn)?;
    // `CREATE TABLE IF NOT EXISTS` leaves a reused older table as it was; give it the columns
    // later exports added, so a complete export can still insert into it.
    add_missing_columns(
        conn,
        "references",
        &[("emittance_id", "INTEGER"), ("radius_status", "INTEGER")],
    )?;
    add_missing_columns(conn, "actor_models", ACTOR_SKIN_TONE_COLUMNS)?;
    add_missing_columns(conn, "actor_models", ACTOR_WEIGHT_COLUMNS)?;
    add_missing_columns(conn, "actor_model_parts", ACTOR_PART_SKIN_COLUMNS)?;
    conn.prepare_cached(
        "INSERT INTO schema_info(version) SELECT ?1 WHERE NOT EXISTS (SELECT 1 FROM schema_info)",
    )?
    .execute([shared::WORLD_DATABASE_SCHEMA_VERSION])?;
    Ok(())
}

/// Adds each `(name, declaration)` column `table` lacks.
fn add_missing_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    for (name, declaration) in columns {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
            [table, name],
            |row| row.get(0),
        )?;
        if !present {
            conn.execute(
                &format!("ALTER TABLE \"{table}\" ADD COLUMN {name} {declaration}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// The `actor_models` columns added after the first catalog export: `NPC_/QNAM`.
const ACTOR_SKIN_TONE_COLUMNS: &[(&str, &str)] = &[
    ("skin_tone_r", "REAL"),
    ("skin_tone_g", "REAL"),
    ("skin_tone_b", "REAL"),
];

/// The `actor_models` column added after the skin tone columns: `NPC_/NAM7`.
const ACTOR_WEIGHT_COLUMNS: &[(&str, &str)] = &[("weight", "REAL")];

/// The `actor_model_parts` columns added after the first catalog export: the armature's race
/// and its skin texture set.
const ACTOR_PART_SKIN_COLUMNS: &[(&str, &str)] = &[
    ("armature_race_id", "INTEGER"),
    ("skin_texture_set_id", "INTEGER"),
];

/// The `lights` columns #269 added after the first light export.
const LIGHT_DETAIL_COLUMNS: &[(&str, &str)] = &[
    ("time", "INTEGER"),
    ("field_of_view", "REAL"),
    ("near_clip", "REAL"),
    ("flicker_period", "REAL"),
    ("flicker_intensity_amplitude", "REAL"),
    ("flicker_movement_amplitude", "REAL"),
    ("value", "INTEGER"),
    ("weight", "REAL"),
    ("sound_id", "INTEGER"),
    ("lens_id", "INTEGER"),
];

type CellMetadata = (Option<i32>, Option<i32>, Option<u32>);

/// Light inputs the in-house decoder rejected as malformed. A rejected field is in neither the
/// canonical subrecords nor the runtime projection, so without this record the status columns
/// would read "absent" (0) where the legacy reader, which sees the raw bytes, writes "malformed" (2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RejectedLightInputs {
    /// `LIGH` `FNAM` fade was rejected.
    pub fnam: bool,
    /// Reference `XRDS` radius offset was rejected.
    pub xrds: bool,
}

/// Rejected light inputs by form ID, for the in-house reader.
pub(crate) type RejectedLightFields = HashMap<u32, RejectedLightInputs>;

/// Select runtime projections and script storage independently of snapshot ownership.
struct ExportOptions<'a> {
    /// Emit runtime views as well as the canonical record archive.
    runtime_views: bool,
    /// Replace in-house script snapshots using their normalized scalar layout.
    scalar_scripts: bool,
    /// Malformed light fields the decoder rejected; `None` for the legacy reader.
    rejected_light_fields: Option<&'a RejectedLightFields>,
}

/// Export selected records without changing existing provenance. Grass records
/// outside this subset are preserved; movement projections are rebuilt from it.
/// Synthetic records without a source load order have no formid_map entry.
/// Export a legacy subset without replacing complete-load-order metadata.
pub fn export_to_db(conn: &Connection, master: &HashMap<u32, RawRecord>) -> Result<()> {
    export_records(
        conn,
        master,
        None,
        None,
        None,
        None,
        ExportOptions {
            runtime_views: true,
            scalar_scripts: false,
            rejected_light_fields: None,
        },
    )
}

/// Replace the complete canonical/runtime snapshot for an effective load order.
/// Plugin metadata remains caller-managed; subset exports retain unrelated rows.
/// Record stable owning-plugin/local-ID pairs separately from the winning
/// override priority in records.load_order.
pub fn export_to_db_with_load_order(
    conn: &Connection,
    master: &HashMap<u32, RawRecord>,
    order: &LoadOrder,
) -> Result<()> {
    export_records(
        conn,
        master,
        Some(order),
        None,
        None,
        None,
        ExportOptions {
            runtime_views: true,
            scalar_scripts: false,
            rejected_light_fields: None,
        },
    )
}

/// Replace plugin metadata and the complete legacy snapshot in one transaction.
/// `unresolved_fields` are the localized fields the legacy text pass left without text; like the
/// in-house reader's, they stay in `records.data` and are left out of the runtime projections.
pub(super) fn export_plugins_to_db(
    conn: &Connection,
    master: &HashMap<u32, RawRecord>,
    order: &LoadOrder,
    plugin_paths: &[PathBuf],
    unresolved_fields: &HashMap<u32, BTreeSet<usize>>,
) -> Result<()> {
    export_records(
        conn,
        master,
        Some(order),
        Some(plugin_paths),
        Some(unresolved_fields),
        None,
        ExportOptions {
            runtime_views: true,
            scalar_scripts: false,
            rejected_light_fields: None,
        },
    )
}

/// Export canonical decoded fields with Skyrim's texture slots and byte-wide XESP flags.
/// `rejected_light_fields` records light fields the decoder rejected as malformed.
pub(crate) fn export_inhouse_to_db(
    conn: &Connection,
    master: &HashMap<u32, RawRecord>,
    order: &LoadOrder,
    plugin_paths: &[PathBuf],
    unresolved_fields: &HashMap<u32, BTreeSet<usize>>,
    terrain_offsets: &HashMap<u32, u64>,
    rejected_light_fields: Option<&RejectedLightFields>,
) -> Result<()> {
    export_records(
        conn,
        master,
        Some(order),
        Some(plugin_paths),
        Some(unresolved_fields),
        Some(terrain_offsets),
        ExportOptions {
            runtime_views: true,
            scalar_scripts: true,
            rejected_light_fields,
        },
    )
}

/// Fresh diagnostic bundles only: archive canonical records and identities without runtime views.
/// The shared loop writes identical records/formid_map SQL before its explicit deferral branch.
pub(crate) fn export_inhouse_record_archive_to_db(
    conn: &Connection,
    master: &HashMap<u32, RawRecord>,
    order: &LoadOrder,
    plugin_paths: &[PathBuf],
) -> Result<()> {
    export_records(
        conn,
        master,
        Some(order),
        Some(plugin_paths),
        None,
        None,
        ExportOptions {
            runtime_views: false,
            scalar_scripts: true,
            rejected_light_fields: None,
        },
    )
}

/// Recreate reader-owned outputs while preserving caller tables and asset-cache proofs.
/// DDL participates in the export transaction, so an export error restores the old snapshot.
fn replace_complete_snapshot(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        r#"DROP TABLE IF EXISTS record_diagnostics;
        DROP TABLE IF EXISTS inhouse_plugin_masters;
        DROP TABLE IF EXISTS record_overrides;
        DROP TABLE IF EXISTS inhouse_schema_addons;
        DROP TABLE IF EXISTS inhouse_unresolved_terrain_textures;
        DROP TABLE IF EXISTS inhouse_terrain_layers;
        DROP TABLE IF EXISTS inhouse_terrain_source_order;
        DROP TABLE IF EXISTS inhouse_localized_fields;
        DROP TABLE IF EXISTS inhouse_source_records;
        DROP TABLE IF EXISTS script_property_values;
        DROP TABLE IF EXISTS script_properties;
        DROP TABLE IF EXISTS scripts;
        DROP TABLE IF EXISTS landscape_texture_grasses;
        DROP TABLE IF EXISTS grass_types;
        DROP TABLE IF EXISTS landscape_textures;
        DROP TABLE IF EXISTS texture_sets;
        DROP TABLE IF EXISTS waters;
        DROP TABLE IF EXISTS movement_game_settings;
        DROP TABLE IF EXISTS race_movement_links;
        DROP TABLE IF EXISTS race_animation_sources;
        DROP TABLE IF EXISTS movement_types;
        DROP TABLE IF EXISTS npcs;
        DROP TABLE IF EXISTS actor_model_parts;
        DROP TABLE IF EXISTS actor_models;
        DROP TABLE IF EXISTS lights;
        DROP TABLE IF EXISTS statics;
        DROP TABLE IF EXISTS land;
        DROP TABLE IF EXISTS exterior_spatial;
        DROP TABLE IF EXISTS "references";
        DROP TABLE IF EXISTS cells;
        DROP TABLE IF EXISTS worldspaces;
        DROP TABLE IF EXISTS records;
        DROP TABLE IF EXISTS formid_map;
        DROP TABLE IF EXISTS lod_chunks_spatial;
        DROP TABLE IF EXISTS lod_chunks;
        DROP TABLE IF EXISTS lod_build;
        DROP TABLE IF EXISTS lod;"#,
    )?;
    for table in crate::esm::environment::ENVIRONMENT_TABLES
        .iter()
        .chain(crate::esm::reference_links::REFERENCE_LINK_TABLES.iter())
    {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }
    create_tables(tx)?;
    // Every complete snapshot owns these derived rows, including archive-only exports
    // that deliberately defer all runtime projections.
    crate::esm::rooms::clear(tx)?;
    crate::esm::projection_omissions::clear(tx)?;
    super::doors::clear(tx)?;
    tx.execute("DELETE FROM schema_info", [])?;
    tx.execute(
        "INSERT INTO schema_info(version) VALUES (?1)",
        [shared::WORLD_DATABASE_SCHEMA_VERSION],
    )?;
    Ok(())
}

/// Keep unresolved text occurrences in the archive while nullable projections see no text.
/// A genuinely resolved empty string is not filtered out.
fn runtime_projection(
    record: &RawRecord,
    unresolved_fields: Option<&HashMap<u32, BTreeSet<usize>>>,
) -> Option<RawRecord> {
    let unresolved = unresolved_fields?.get(&record.form_id)?;
    Some(RawRecord {
        form_id: record.form_id,
        record_type: record.record_type,
        flags: record.flags,
        subrecords: record
            .subrecords
            .iter()
            .enumerate()
            .filter(|(index, _)| !unresolved.contains(index))
            .map(|(_, field)| field.clone())
            .collect(),
        cell_form_id: record.cell_form_id,
        worldspace_form_id: record.worldspace_form_id,
        load_order: record.load_order,
    })
}

/// Validate movement projections before either frontend replaces a usable predecessor.
pub(crate) fn validate_inhouse_record(record: &RawRecord) -> Result<()> {
    let view = SubrecordView::new(&record.subrecords);
    match &record.record_type {
        b"MOVT" if record.form_id == 0x0003_580D => {
            decode_npc_default_speeds(&view, record.form_id)?;
        }
        b"GMST"
            if view.get_string(b"EDID").is_some_and(|name| {
                ["fMoveCharWalkBase", "fJumpHeightMin"]
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(&name))
            }) =>
        {
            decode_movement_setting(&view, record.form_id)?;
        }
        b"RACE" => {
            optional_race_movement_link(&view, record.form_id, b"WKMV")?;
            optional_race_movement_link(&view, record.form_id, b"RNMV")?;
        }
        _ => {}
    }
    Ok(())
}

/// Writes every merged record; with a load order, also records each record's
/// owning plugin and plugin-local ID in `formid_map`.
fn export_records(
    conn: &Connection,
    master: &HashMap<u32, RawRecord>,
    order: Option<&LoadOrder>,
    plugin_paths: Option<&[PathBuf]>,
    unresolved_fields: Option<&HashMap<u32, BTreeSet<usize>>>,
    terrain_offsets: Option<&HashMap<u32, u64>>,
    options: ExportOptions<'_>,
) -> Result<()> {
    let ExportOptions {
        runtime_views,
        scalar_scripts,
        rejected_light_fields,
    } = options;
    conn.set_prepared_statement_cache_capacity(64);
    let tx = conn.unchecked_transaction()?;
    // Only a full load-order export writes these; a subset export keeps the stored ones.
    let mut omissions = ProjectionOmissions::default();
    if order.is_some() {
        replace_complete_snapshot(&tx)?;
        if let Some(paths) = plugin_paths {
            tx.execute("DELETE FROM plugins", [])?;
            super::insert_plugins(&tx, paths).map_err(|error| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    error.to_string(),
                )))
            })?;
        }
    }
    if scalar_scripts {
        // In-house exports own the complete script snapshot; legacy partial APIs retain theirs.
        super::inhouse_script_properties::replace_snapshot_tables(&tx)?;
    }
    // Partial public exports retain unrelated rows and follow the existing database's format.
    let scalar_script_storage = scalar_scripts || tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='script_properties')",
        [], |row| row.get::<_, bool>(0),
    )?;
    if runtime_views {
        tx.prepare_cached("DELETE FROM movement_types")?
            .execute([])?;
        tx.prepare_cached("DELETE FROM movement_game_settings")?
            .execute([])?;
        tx.prepare_cached("DELETE FROM race_movement_links")?
            .execute([])?;
    }
    let mut cells: HashMap<u32, CellMetadata> = HashMap::new();
    let unordered_offsets = HashMap::new();
    let terrain_winners = if runtime_views {
        crate::esm::cell_cache::land_winners_by_cell(
            master,
            terrain_offsets.unwrap_or(&unordered_offsets),
        )
    } else {
        HashMap::new()
    };

    if runtime_views {
        for (&form_id, record) in master
            .iter()
            .filter(|(_, record)| &record.record_type == b"CELL")
        {
            let projection = runtime_projection(record, unresolved_fields);
            let projected = projection.as_ref().unwrap_or(record);
            let (grid_x, grid_y, interior_name) = extract_cell_info(&projected.subrecords);
            let full_name = SubrecordView::new(&projected.subrecords).get_string(b"FULL");

            crate::esm::environment::insert_cell(
                &tx,
                &crate::esm::environment::Links { master },
                form_id,
                &SubrecordView::new(&projected.subrecords),
            )?;
            let data = serialize_subrecords(&record.subrecords);
            insert_cell(
                &tx,
                CellRow {
                    form_id,
                    worldspace_id: record.worldspace_form_id,
                    grid_x,
                    grid_y,
                    interior_name: interior_name.as_deref(),
                    full_name: full_name.as_deref(),
                    flags: record.flags,
                    data: &data,
                },
            )?;
            cells.insert(form_id, (grid_x, grid_y, record.worldspace_form_id));
        }
    }
    let interior_cells: std::collections::HashSet<u32> = cells
        .iter()
        .filter(|(_, (grid_x, grid_y, worldspace))| {
            grid_x.is_none() && grid_y.is_none() && worldspace.is_none()
        })
        .map(|(&form_id, _)| form_id)
        .collect();

    let mut ordered: Vec<_> = master.iter().collect();
    ordered.sort_unstable_by_key(|(form_id, _)| **form_id);
    // Keep the hot statement checked out. Repeated cache checkout/return recreates
    // its owned SQL key and cache entry for every record. Prepare lazily for an
    // empty legacy subset, and clear each row's bindings as before.
    let mut records_insert = if ordered.is_empty() {
        None
    } else {
        let statement = tx.prepare_cached(RECORDS_INSERT_SQL)?;
        check_record_insert(&statement)?;
        Some(statement)
    };
    for (&form_id, record) in ordered {
        let type_str = from_utf8(&record.record_type).unwrap_or("UNKN");
        let blob = serialize_subrecords(&record.subrecords);
        let insert = records_insert.as_mut().expect("nonempty record loop");
        insert_canonical_record(
            insert,
            form_id,
            type_str,
            record.cell_form_id,
            record.worldspace_form_id,
            record.load_order,
            &blob,
        )?;
        if let Some(order) = order {
            let identity = order.identity(form_id).map_err(|error| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("record {form_id:08X}: {error}"),
                )))
            })?;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO formid_map(form_id, plugin_name, internal_id, record_type) VALUES (?1, ?2, ?3, ?4)")?.execute(
                params![form_id, identity.plugin, identity.local_id, type_str],
            )?;
        }

        if !runtime_views {
            // Canonical records/formid_map are complete; only materialized runtime views defer.
            continue;
        }
        let canonical_record = record;
        let projection = runtime_projection(record, unresolved_fields);
        let record = projection.as_ref().unwrap_or(record);
        let view = SubrecordView::new(&record.subrecords);
        let vmad_bytes = view.find(b"VMAD");
        if let Some(vmad_bytes) = vmad_bytes {
            let parsed = parse_vmad(vmad_bytes, &record.record_type);
            if let Ok((_, vmad)) = parsed {
                for script in vmad.scripts {
                    if scalar_script_storage {
                        super::inhouse_script_properties::write_script(
                            &tx, form_id, &script, vmad_bytes,
                        )?;
                        continue;
                    }
                    let properties =
                        serde_json::to_string(&script.properties).map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                        })?;
                    tx.prepare_cached(
                    "INSERT OR REPLACE INTO scripts(form_id, script_name, vmad, properties_json) VALUES (?1, ?2, ?3, ?4)")?.execute(
                    params![form_id, script.name, vmad_bytes, properties],
                )?;
                }
            }
        }

        match type_str {
            "LGTM" | "WTHR" | "CLMT" | "REGN" | "IMGS" => {
                crate::esm::environment::insert_record(
                    &tx,
                    &crate::esm::environment::Links { master },
                    record,
                    &view,
                )?;
            }
            "GRAS" => {
                insert_grass(&tx, form_id, &view, record.load_order)?;
            }
            "WRLD" => {
                let view = SubrecordView::new(&record.subrecords);
                let editor_id = view
                    .get_string(b"EDID")
                    .unwrap_or_else(|| format!("WRLD_{form_id:08X}"));
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO worldspaces(id, editor_id, parent_world, flags, full_name) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(
                    params![form_id, editor_id, view.get_form_id(b"WNAM"), record.flags, view.get_string(b"FULL")],
                )?;
                crate::esm::environment::insert_worldspace(
                    &tx,
                    &crate::esm::environment::Links { master },
                    form_id,
                    &view,
                )?;
            }
            _ if super::is_placed_record_type(&record.record_type) => {
                let cell_id = record.cell_form_id.unwrap_or(0);
                insert_reference_with_canonical_data(
                    &tx,
                    form_id,
                    cell_id,
                    cells.get(&cell_id).copied(),
                    record.flags,
                    &record.subrecords,
                    &canonical_record.subrecords,
                    rejected_light_fields
                        .and_then(|rejected| rejected.get(&form_id))
                        .is_some_and(|rejected| rejected.xrds),
                )?;
                crate::esm::reference_links::insert(
                    &tx,
                    &crate::esm::reference_links::Context { master, order },
                    record,
                    &SubrecordView::new(&record.subrecords),
                )?;
            }
            "LAND" => {
                let cell_id = record.cell_form_id.unwrap_or(form_id);
                if terrain_winners.get(&cell_id) != Some(&form_id) {
                    continue;
                }
                let (heightmap, vtex, vclr, normals) = extract_land_data(&record.subrecords);
                tx.prepare_cached("INSERT OR REPLACE INTO land(cell_id, heightmap, vtex, vclr, normals) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute( params![cell_id, heightmap, vtex, vclr, normals])?;
            }
            "LIGH" => {
                let view = SubrecordView::new(&record.subrecords);
                // Every `LIGH` lights the space whether or not it has
                // geometry, so it gets a `lights` row regardless.
                insert_light(
                    &tx,
                    master,
                    form_id,
                    &view,
                    rejected_light_fields
                        .and_then(|rejected| rejected.get(&form_id))
                        .is_some_and(|rejected| rejected.fnam),
                    &mut omissions,
                )?;
                // Most `LIGH` records carry no model: they light the space with
                // nothing to draw. Storing one row per invisible light would put
                // a meshless entry in `statics` for every candle in the game, so
                // those are skipped; a light with geometry is stored like any
                // other base object.
                if let Some(model_path) = view.get_string(b"MODL").filter(|path| !path.is_empty()) {
                    tx.prepare_cached(
                        "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags, full_name) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(
                        params![form_id, view.get_string(b"EDID"), model_path, record.flags, view.get_string(b"FULL")],
                    )?;
                }
            }
            // Every base record type with a world model. The runtime spawns
            // anything that resolves a model path here, so trees, flora,
            // containers, doors, activators, and placed inventory all render;
            // Records without MODL (triggers, markers) store NULL and are
            // skipped at spawn time. `LIGH` is handled above: the light goes
            // in `lights` and only a light with geometry lands in `statics`.
            // `TACT` (talking activators), `ADDN` (addon nodes), `APPA`
            // (apparatus) and `ARTO` (art objects) are placeable `REFR` bases
            // with a model too (`REFR.NAME` targets in xEdit's TES5 definitions).
            "STAT" | "MSTT" | "FURN" | "TREE" | "FLOR" | "CONT" | "DOOR" | "ACTI" | "TACT"
            | "WEAP" | "MISC" | "BOOK" | "AMMO" | "ALCH" | "INGR" | "SLGM" | "KEYM" | "SCRL"
            | "ARMO" | "APPA" | "ADDN" | "ARTO" | "PROJ" | "HAZD" => {
                let view = SubrecordView::new(&record.subrecords);
                // ARMO has no MODL path; its world models live in the
                // gendered world-model slots (male first, female fallback).
                // Both frontends use MOD4 for the female world model.
                let model = if type_str == "ARMO" {
                    view.get_string(b"MOD2")
                        .filter(|path| !path.is_empty())
                        .or_else(|| view.get_string(b"MOD4"))
                } else {
                    view.get_string(b"MODL")
                }
                .filter(|path| !path.is_empty());
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags, full_name) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(
                    params![form_id, view.get_string(b"EDID"), model, record.flags, view.get_string(b"FULL")],
                )?;
            }
            "NPC_" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO npcs(id, editor_id, full_name, race_id, class_id, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?.execute(
                    params![form_id, view.get_string(b"EDID"), view.get_string(b"FULL"), view.get_form_id(b"RNAM"), view.get_form_id(b"CNAM"), record.flags],
                )?;
            }
            "RACE" => {
                let walk = optional_race_movement_link(&view, form_id, b"WKMV")?;
                let run = optional_race_movement_link(&view, form_id, b"RNMV")?;
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO race_movement_links(id, walk_movt_id, run_movt_id, load_order) VALUES (?1, ?2, ?3, ?4)")?.execute(
                    params![form_id, walk, run, record.load_order],
                )?;
            }
            "MOVT" if form_id == 0x0003_580D => {
                let speeds = decode_npc_default_speeds(&view, form_id)?;
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO movement_types(id, editor_id, left_walk, left_run, right_walk, right_run, forward_walk, forward_run, back_walk, back_run, load_order) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)")?.execute(
                    params![form_id, "NPC_Default_MT", speeds[0], speeds[1], speeds[2], speeds[3], speeds[4], speeds[5], speeds[6], speeds[7], record.load_order],
                )?;
            }
            "GMST" => {
                let editor_id = view.get_string(b"EDID");
                let movement_name = editor_id.as_deref().and_then(|name| {
                    ["fMoveCharWalkBase", "fJumpHeightMin"]
                        .into_iter()
                        .find(|known| known.eq_ignore_ascii_case(name))
                });
                if let Some(editor_id) = movement_name {
                    let value = decode_movement_setting(&view, form_id)?;
                    tx.prepare_cached(
                        "INSERT OR REPLACE INTO movement_game_settings(id, editor_id, value, load_order) VALUES (?1, ?2, ?3, ?4)")?.execute(
                        params![form_id, editor_id, value, record.load_order],
                    )?;
                }
            }
            "WATR" => {
                let view = SubrecordView::new(&record.subrecords);
                let dnam = view.find(b"DNAM").and_then(parse_water_dnam);
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO waters(id,editor_id,opacity,flags,shallow_color,deep_color,reflection_color,fresnel,reflectivity,flow_normal_path,data) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?.execute(
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        view.find(b"ANAM").and_then(|bytes| bytes.first()).copied(),
                        record.flags,
                        dnam.as_ref().map(|dnam| dnam.shallow_color),
                        dnam.as_ref().map(|dnam| dnam.deep_color),
                        dnam.as_ref().map(|dnam| dnam.reflection_color),
                        dnam.as_ref().map(|dnam| dnam.fresnel),
                        dnam.as_ref().map(|dnam| dnam.reflectivity),
                        water_flow_normal_path(&view),
                        blob,
                    ],
                )?;
            }
            "TXST" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO texture_sets(id,editor_id,diffuse_path,normal_path,glow_path,height_path,environment_path,mask_path,specular_path,detail_path) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?.execute(
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        texture_set_slot_path(&view, form_id, b"TX00"),
                        texture_set_slot_path(&view, form_id, b"TX01"),
                        texture_set_slot_path(&view, form_id, b"TX03"),
                        texture_set_slot_path(&view, form_id, b"TX04"),
                        texture_set_slot_path(&view, form_id, b"TX05"),
                        texture_set_slot_path(&view, form_id, b"TX02"),
                        texture_set_slot_path(&view, form_id, b"TX07"),
                        texture_set_slot_path(&view, form_id, b"TX06"),
                    ],
                )?;
            }
            "LTEX" => {
                let view = SubrecordView::new(&record.subrecords);
                let havok = view.find(b"HNAM");
                let friction = havok
                    .and_then(|bytes| bytes.first())
                    .copied()
                    .map(f32::from);
                let restitution = havok.and_then(|bytes| bytes.get(1)).copied().map(f32::from);
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO landscape_textures(id,editor_id,texture_set_id,material_type,friction,restitution) VALUES (?1,?2,?3,?4,?5,?6)")?.execute(
                    params![form_id, view.get_string(b"EDID"), view.get_form_id(b"TNAM"), view.get_form_id(b"MNAM"), friction, restitution],
                )?;
                // An included LTEX always replaces its own list, including an
                // empty list, even when other records are outside this export.
                tx.prepare_cached("DELETE FROM landscape_texture_grasses WHERE ltex_id=?1")?
                    .execute([form_id])?;
                for (_, data) in record.subrecords.iter().filter(|(tag, _)| tag == b"GNAM") {
                    if let Ok(bytes) = <[u8; 4]>::try_from(data.as_slice()) {
                        let grass_id = u32::from_le_bytes(bytes);
                        if grass_id != 0 {
                            tx.prepare_cached(
                                "INSERT OR IGNORE INTO landscape_texture_grasses(ltex_id, gras_id) VALUES (?1, ?2)")?.execute(
                                params![form_id, grass_id],
                            )?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    drop(records_insert);
    // Derived snapshots like the actor model catalog and the grass tables: only a full load-order
    // export owns them, so the movement annotator's subset export leaves them alone, and
    // diagnostic archives defer them. The emittance pass holds every reference and the records
    // their XEMI names, and may run on a database older than `emittance_id`.
    if runtime_views && order.is_some() {
        insert_actor_models(&tx, master)?;
        let authoring = master
            .iter()
            .filter(|(_, row)| &row.record_type == b"RACE" && !row.is_deleted())
            .filter_map(|(&id, row)| {
                crate::animation::authoring::read_race_authoring_fields(
                    row.subrecords
                        .iter()
                        .map(|(tag, data)| (tag.as_slice(), data.as_slice())),
                )
                .ok()
                .map(|authoring| (id, authoring))
            })
            .collect();
        crate::animation::authoring::replace_race_projection(&tx, authoring)?;
        crate::esm::rooms::export_room_graph(&tx, master, &interior_cells)?;
        insert_emittance_sources(&tx, master, &mut omissions)?;
        omissions.write(&tx)?;
        omissions.print_summary();
        // Read back from the canonical `records` and `references` rows just written, so the
        // legacy and in-house readers share one projection.
        super::doors::project_door_links(&tx)?;
    }
    tx.commit()
}

/// Publishes the actor model catalog for a complete load-order export in the same transaction.
///
/// The resolver needs the whole record map, so it runs once after the record loop and only when
/// `formid_map` carries complete ownership. Each `NPC_`'s own row and parts are replaced, which
/// keeps the freshly created tables consistent with `master`.
fn insert_actor_models(tx: &Transaction<'_>, master: &HashMap<u32, RawRecord>) -> Result<()> {
    let plugin_ids: HashMap<u32, StableId> = tx
        .prepare(
            "SELECT form_id, plugin_name, internal_id FROM formid_map WHERE record_type='NPC_'",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                StableId {
                    plugin: row.get(1)?,
                    local_id: row.get(2)?,
                },
            ))
        })?
        .collect::<Result<_>>()?;
    let catalog = crate::actor_models::resolve_actor_models(master, &plugin_ids);

    let mut delete_model = tx.prepare_cached("DELETE FROM actor_models WHERE id=?1")?;
    let mut delete_parts = tx.prepare_cached("DELETE FROM actor_model_parts WHERE npc_id=?1")?;
    let mut insert_model = tx.prepare_cached(
        "INSERT OR REPLACE INTO actor_models(id, race_id, sex, skin_armo_id, head_model, head_source, resolution, skin_tone_r, skin_tone_g, skin_tone_b, weight) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut insert_part = tx.prepare_cached(
        "INSERT OR REPLACE INTO actor_model_parts(npc_id, slot, model_path, armature_race_id, skin_texture_set_id) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for row in &catalog.models {
        delete_model.execute([row.id])?;
        delete_parts.execute([row.id])?;
        insert_model.execute(params![
            row.id,
            row.race_id,
            row.sex,
            row.skin_armo_id,
            row.head_model,
            row.head_source,
            row.resolution,
            row.skin_tone.map(|tone| tone[0]),
            row.skin_tone.map(|tone| tone[1]),
            row.skin_tone.map(|tone| tone[2]),
            row.weight
        ])?;
    }
    for part in &catalog.parts {
        insert_part.execute(params![
            part.npc_id,
            part.slot.as_str(),
            part.model_path,
            part.armature_race_id,
            part.skin_texture_set_id
        ])?;
    }
    report_actor_model_diagnostics(&catalog.diagnostics);
    Ok(())
}

/// Summarizes the catalog's diagnostics and names every unrecognized biped slot.
fn report_actor_model_diagnostics(diagnostics: &[crate::actor_models::ActorModelDiagnostic]) {
    use crate::actor_models::UnresolvedReason;
    if diagnostics.is_empty() {
        return;
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for diagnostic in diagnostics {
        *counts.entry(diagnostic.reason.as_str()).or_default() += 1;
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(reason, count)| format!("{reason}={count}"))
        .collect();
    eprintln!(
        "actor models: {} diagnostic(s): {}",
        diagnostics.len(),
        summary.join(", ")
    );
    for diagnostic in diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.reason == UnresolvedReason::UnrecognizedSlot)
    {
        eprintln!(
            "actor model: NPC {:08X} has an armature with no stored biped slot",
            diagnostic.npc_id
        );
    }
}

/// Rebuilds `emittance`: one row per `XEMI` target in `references` that resolves to a colour.
///
/// A light lends its `DATA` colour at every hour; a region lends its likeliest weather's Effect
/// Lighting colour at each time of day (see [`crate::esm::emittance`]). A target that is missing
/// or does not resolve gets no row, so the runtime keeps the model's own emission for its
/// references. Rows from an earlier export into the same database are dropped first, so a target
/// that no longer resolves loses its old colour.
fn insert_emittance_sources(
    tx: &Transaction<'_>,
    master: &HashMap<u32, RawRecord>,
    omissions: &mut ProjectionOmissions,
) -> Result<()> {
    tx.execute("DELETE FROM emittance", [])?;
    let targets: Vec<u32> = tx
        .prepare("SELECT DISTINCT emittance_id FROM \"references\" WHERE emittance_id IS NOT NULL")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_>>()?;
    for target in targets {
        let Some(record) = master.get(&target).filter(|record| !record.is_deleted()) else {
            omissions.record(
                OmissionCategory::EmittanceTargetAbsent,
                target,
                "not in the load order; no emittance row",
            );
            continue;
        };
        let view = SubrecordView::new(&record.subrecords);
        let (source_type, weather, colours) = match &record.record_type {
            b"LIGH" => {
                let Some(data) = view
                    .find(b"DATA")
                    .filter(|data| data.len() >= LIGHT_DATA_MIN)
                else {
                    omissions.record(
                        OmissionCategory::EmittanceLightUnusableData,
                        target,
                        "LIGH has no usable DATA; no emittance row",
                    );
                    continue;
                };
                ("LIGH", None, [[data[8], data[9], data[10]]; 4])
            }
            b"REGN" => {
                let Some(weather) = view.find(b"RDWT").and_then(emittance::region_weather) else {
                    omissions.record(
                        OmissionCategory::EmittanceRegionNoWeather,
                        target,
                        "REGN names no weather; no emittance row",
                    );
                    continue;
                };
                let Some(colours) = master
                    .get(&weather)
                    .filter(|record| record.record_type == *b"WTHR" && !record.is_deleted())
                    .and_then(|record| SubrecordView::new(&record.subrecords).find(b"NAM0"))
                    .and_then(emittance::weather_effect_lighting)
                else {
                    omissions.record(
                        OmissionCategory::EmittanceWeatherNoLighting,
                        target,
                        format!(
                            "REGN weather {weather:08X} has no Effect Lighting colours; no emittance row"
                        ),
                    );
                    continue;
                };
                ("REGN", Some(weather), colours)
            }
            other => {
                omissions.record(
                    OmissionCategory::EmittanceTargetWrongType,
                    target,
                    format!(
                        "is a {}, not a LIGH or REGN; no emittance row",
                        String::from_utf8_lossy(other)
                    ),
                );
                continue;
            }
        };
        let [sunrise, day, sunset, night] = colours;
        tx.execute(
            "INSERT OR REPLACE INTO emittance(id, source_type, weather_id, sunrise_r, sunrise_g, sunrise_b, day_r, day_g, day_b, sunset_r, sunset_g, sunset_b, night_r, night_g, night_b)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                target, source_type, weather,
                sunrise[0], sunrise[1], sunrise[2],
                day[0], day[1], day[2],
                sunset[0], sunset[1], sunset[2],
                night[0], night[1], night[2],
            ],
        )?;
    }
    Ok(())
}

/// Projects xEdit's TES5 GRAS/DATA layout without inventing missing fields.
/// The complete payload is 32 bytes; unknown padding stays in records.data.
/// See the primary definitions linked in the database schema specification.
fn insert_grass(
    tx: &Transaction<'_>,
    form_id: u32,
    view: &SubrecordView<'_>,
    load_order: u32,
) -> Result<()> {
    let data = view.find(b"DATA").unwrap_or_default();
    let float = |offset| {
        data.get(offset..offset + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(f32::from_le_bytes)
            .filter(|value| value.is_finite())
    };
    let model = view
        .get_string(b"MODL")
        .and_then(|path| canonical_asset_path(&path, AssetKind::Mesh, "glb").ok());
    tx.prepare_cached(
        "INSERT OR REPLACE INTO grass_types(id, editor_id, model_path, density, min_slope, max_slope, units_from_water, water_comparison, position_range, height_range, color_range, wave_period, flags, load_order) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)")?.execute(
        params![
            form_id,
            view.get_string(b"EDID"),
            model,
            data.first().copied(),
            data.get(1).copied(),
            data.get(2).copied(),
            data.get(4..6).and_then(|bytes| <[u8; 2]>::try_from(bytes).ok()).map(u16::from_le_bytes),
            data.get(8..12).and_then(|bytes| <[u8; 4]>::try_from(bytes).ok()).map(u32::from_le_bytes),
            float(12),
            float(16),
            float(20),
            float(24),
            data.get(28).copied(),
            load_order,
        ],
    )?;
    Ok(())
}

/// One TXST slot's path as authored, or nothing when no asset lookup could accept it. The later
/// texture passes treat a stored path they cannot canonicalize as fatal, so one bad slot would
/// otherwise stop the whole conversion. The archived record keeps the original bytes.
fn texture_set_slot_path(view: &SubrecordView<'_>, form_id: u32, slot: &[u8; 4]) -> Option<String> {
    let path = view.get_string(slot)?;
    if path.is_empty() {
        return Some(path);
    }
    match canonical_asset_path(&path, AssetKind::Texture, "dds") {
        Ok(_) => Some(path),
        Err(error) => {
            eprintln!(
                "warning: TXST {form_id:08X} {}: dropped invalid texture path {path:?}: {error}",
                String::from_utf8_lossy(slot)
            );
            None
        }
    }
}

fn water_flow_normal_path(view: &SubrecordView<'_>) -> Option<String> {
    view.get_string(b"NAM5").and_then(|path| {
        canonical_asset_path(&path, AssetKind::Texture, "dds")
            .ok()
            .map(|canonical| canonical.trim_start_matches("textures/").to_owned())
    })
}

/// The WATR colours and reflectivity factors decoded from `DNAM`. Field order and offsets are
/// xEdit's `Water.psc` layout (`docs/research/water.md` section 1.2), confirmed against
/// `Skyrim.esm`/`Update.esm`. `DNAM` is 228 bytes (30 records) or 232 bytes (8 SSE records, an
/// extra trailing Flowmap Scale float); none of the fields read here move between the two.
struct WaterDnam {
    reflectivity: f32,
    fresnel: f32,
    shallow_color: u32,
    deep_color: u32,
    reflection_color: u32,
}

/// Packs a WATR colour subrecord's leading RGB bytes (plus the trailing pad byte, ignored by
/// callers) into a `u32`, the same little-endian layout the `waters` table has always stored
/// colours in.
fn dnam_color(dnam: &[u8], offset: usize) -> Option<u32> {
    dnam.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four-byte color")))
}

fn dnam_f32(dnam: &[u8], offset: usize) -> Option<f32> {
    dnam.get(offset..offset + 4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four-byte float")))
}

/// Decodes Reflectivity Amount, Fresnel Amount, and the three colours from a WATR `DNAM`
/// subrecord. Returns `None` if `dnam` is too short to hold every field (real records never are:
/// the shortest is 228 bytes, and the last field read here ends at byte 52).
fn parse_water_dnam(dnam: &[u8]) -> Option<WaterDnam> {
    Some(WaterDnam {
        reflectivity: dnam_f32(dnam, 20)?,
        fresnel: dnam_f32(dnam, 24)?,
        shallow_color: dnam_color(dnam, 40)?,
        deep_color: dnam_color(dnam, 44)?,
        reflection_color: dnam_color(dnam, 48)?,
    })
}

/// `LIGH` `DATA`, per UESP "Skyrim Mod:Mod File Format/LIGH": time (i32),
/// radius (u32, Creation units), colour (RGB + one unused byte), flags (u32),
/// falloff exponent (f32), then FOV, near clip, flicker period, flicker
/// intensity amplitude, flicker movement amplitude, value (u32) and weight
/// (f32). Every one of the 435 `LIGH` records in Skyrim.esm carries exactly
/// 48 bytes, which is the sum of those fields.
///
/// The engine reads only the first four - radius, colour, flags, falloff - so
/// a `DATA` holding at least those 20 bytes is accepted and its tail ignored;
/// one shorter than that is a record this code cannot read and is dropped with
/// a warning rather than guessed at. A shorter `DATA` is accepted because the
/// leading fields are the same ones Oblivion-era light data has.
const LIGHT_DATA_MIN: usize = 20;
/// The full `LIGH` `DATA` in Skyrim SE.
const LIGHT_DATA_FULL: usize = 48;

fn insert_light(
    tx: &Transaction<'_>,
    master: &HashMap<u32, RawRecord>,
    form_id: u32,
    view: &SubrecordView<'_>,
    fnam_rejected: bool,
    omissions: &mut ProjectionOmissions,
) -> Result<()> {
    let Some(data) = view.find(b"DATA") else {
        omissions.record(
            OmissionCategory::LightMissingData,
            form_id,
            "LIGH has no DATA; no lights row",
        );
        return Ok(());
    };
    if data.len() < LIGHT_DATA_MIN {
        omissions.record(
            OmissionCategory::LightShortData,
            form_id,
            format!(
                "LIGH DATA is {} bytes, expected at least {LIGHT_DATA_MIN}; no lights row",
                data.len()
            ),
        );
        return Ok(());
    }
    // The radius is a u32 even though the column is REAL: a radius of 256
    // reads as 256.0, where the same four bytes read as f32 are a denormal.
    let radius = u32::from_le_bytes(data[4..8].try_into().expect("four-byte light radius")) as f32;
    let flags = u32::from_le_bytes(data[12..16].try_into().expect("four-byte light flags"));
    let falloff = f32::from_le_bytes(data[16..20].try_into().expect("four-byte light falloff"));
    let fade = view
        .find(b"FNAM")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| f32::from_le_bytes(bytes[..4].try_into().expect("four-byte light fade")));
    // 0 absent, 1 valid finite nonnegative, 2 malformed/unsupported negative.
    // Do not merge valid zero, missing FNAM and malformed FNAM into one state.
    let fade_status = match view.find(b"FNAM") {
        // The in-house decoder rejected a malformed FNAM; the legacy reader would see it as 2.
        None if fnam_rejected => 2,
        None => 0,
        Some(bytes) if bytes.len() == 4 && fade.is_some_and(|v| v.is_finite() && v >= 0.0) => 1,
        Some(_) => 2,
    };
    let fade = (fade_status == 1).then_some(fade).flatten();
    // The rest of the 48-byte DATA (xEdit 4.1.5f `wbDefinitionsTES5.pas`, LIGH): time, FOV, near
    // clip, the flicker period and amplitudes, value and weight. The in-house schema accepts only
    // the full 48 bytes, so a shorter DATA leaves them NULL for both readers.
    let full = (data.len() == LIGHT_DATA_FULL).then_some(data);
    let real = |offset: usize| {
        full.map(|data| f32::from_le_bytes(data[offset..offset + 4].try_into().expect("f32")))
            .filter(|value| value.is_finite())
    };
    let word = |offset: usize| {
        full.map(|data| u32::from_le_bytes(data[offset..offset + 4].try_into().expect("u32")))
    };
    // Links keep a FormID only when the winning record exists with the allowed type: the
    // in-house reader clears unresolved links and the legacy reader keeps them.
    let link = |tag: &[u8; 4], target: &[u8; 4]| {
        view.find(tag)
            .filter(|bytes| bytes.len() == 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four-byte link")))
            .filter(|id| {
                master
                    .get(id)
                    .is_some_and(|record| !record.is_deleted() && record.record_type == *target)
            })
    };
    tx.prepare_cached(
        "INSERT OR REPLACE INTO lights(id, editor_id, radius, color_r, color_g, color_b, flags, falloff, fade, time, field_of_view, near_clip, flicker_period, flicker_intensity_amplitude, flicker_movement_amplitude, value, weight, sound_id, lens_id, fade_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)")?.execute(
        params![
            form_id,
            view.get_string(b"EDID"),
            radius,
            i64::from(data[8]),
            i64::from(data[9]),
            i64::from(data[10]),
            flags,
            falloff,
            fade,
            word(0).map(|time| time as i32),
            real(20),
            real(24),
            real(28),
            real(32),
            real(36),
            word(40),
            real(44),
            link(b"SNAM", b"SNDR"),
            link(b"LNAM", b"LENS"),
            fade_status,
        ],
    )?;
    Ok(())
}

/// Write a reference and its exterior spatial projection.
pub fn insert_reference(
    tx: &Transaction<'_>,
    form_id: u32,
    cell_id: u32,
    cell: Option<CellMetadata>,
    header_flags: u32,
    subs: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    insert_reference_with_canonical_data(
        tx,
        form_id,
        cell_id,
        cell,
        header_flags,
        subs,
        subs,
        false,
    )
}

/// Extract runtime reference columns from projected fields and preserve the canonical data mirror.
#[allow(clippy::too_many_arguments)]
fn insert_reference_with_canonical_data(
    tx: &Transaction<'_>,
    form_id: u32,
    cell_id: u32,
    cell: Option<CellMetadata>,
    header_flags: u32,
    projected_subrecords: &[(Vec<u8>, Vec<u8>)],
    canonical_subrecords: &[(Vec<u8>, Vec<u8>)],
    xrds_rejected: bool,
) -> Result<()> {
    let view = SubrecordView::new(projected_subrecords);
    let transform = view.get_f32_slice(b"DATA").unwrap_or_default();
    let pos = [
        *transform.first().unwrap_or(&0.0),
        *transform.get(1).unwrap_or(&0.0),
        *transform.get(2).unwrap_or(&0.0),
    ];
    let rot = [
        *transform.get(3).unwrap_or(&0.0),
        *transform.get(4).unwrap_or(&0.0),
        *transform.get(5).unwrap_or(&0.0),
    ];
    let scale = view
        .find(b"XSCL")
        .filter(|data| data.len() >= 4)
        .map(|data| f32::from_le_bytes(data[..4].try_into().unwrap()))
        .unwrap_or(1.0);
    let base_form_id = view.get_form_id(b"NAME").unwrap_or(0);
    let (grid_x, grid_y, worldspace_id) = cell.unwrap_or((None, None, None));
    let is_exterior = worldspace_id.is_some() || (grid_x.is_some() && grid_y.is_some());
    // Exterior persistent references are owned by the worldspace's persistent
    // cell even when their position lies many cells away. Derive local
    // coordinates from the actual position and keep the R-Tree global so
    // streaming is spatial rather than tied to the owning CELL record.
    let local_x = is_exterior.then(|| pos[0] - (pos[0] / CELL_SIZE).floor() * CELL_SIZE);
    let local_y = is_exterior.then(|| pos[1] - (pos[1] / CELL_SIZE).floor() * CELL_SIZE);
    let blob = serialize_subrecords(canonical_subrecords);
    // XRDS is a signed f32 radius offset. xEdit cce1c424, wbDefinitionsCommon.pas
    // wbREFRRadiusToStr adds it to LIGH DATA radius when displaying/editing effective range.
    // Keep the raw offset; the runtime evaluates and bounds the sum separately.
    let radius_override = view
        .find(b"XRDS")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| f32::from_le_bytes(bytes[..4].try_into().expect("four-byte XRDS radius")));
    let radius_status = match view.find(b"XRDS") {
        // The in-house decoder rejected a malformed XRDS; the legacy reader would see it as 2.
        None if xrds_rejected => 2,
        None => 0,
        Some(bytes) if bytes.len() == 4 && radius_override.is_some_and(f32::is_finite) => 1,
        Some(_) => 2,
    };
    let radius_override = (radius_status == 1).then_some(radius_override).flatten();
    let (enable_parent_id, enable_parent_flags) = view
        .find(b"XESP")
        // xEdit 4.1.5f, wbDefinitionsTES5.pas:5156 (wbXESP): FormID, one flags byte, three unused bytes.
        // Pinned layout source: see SubrecordXESP.
        .and_then(SubrecordXESP::parse)
        .map(|parent| (parent.parent_ref, parent.flags))
        .unzip();
    // `XEMI` names the light or region whose colour the model's `External_Emittance` surfaces
    // take; `insert_emittance_sources` resolves it. A cleared link (0) stores NULL.
    let emittance_id = view
        .find(b"XEMI")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| u32::from_le_bytes(bytes[..4].try_into().expect("four-byte XEMI source")))
        .filter(|&id| id != 0);

    tx.prepare_cached(
        "INSERT OR REPLACE INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, local_x, local_y, rot_x, rot_y, rot_z, scale, radius_override, header_flags, enable_parent_id, enable_parent_flags, emittance_id, data, radius_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)")?.execute(
        params![form_id, cell_id, worldspace_id, base_form_id, is_exterior, pos[0], pos[1], pos[2], local_x, local_y, rot[0], rot[1], rot[2], scale, radius_override, header_flags, enable_parent_id, enable_parent_flags, emittance_id, blob, radius_status],
    )?;
    // A subset update can move an existing reference into an interior.
    tx.prepare_cached("DELETE FROM exterior_spatial WHERE id=?1")?
        .execute([form_id])?;
    if is_exterior {
        tx.prepare_cached("INSERT OR REPLACE INTO exterior_spatial(id, minX, maxX, minY, maxY, minZ, maxZ, cell_id, worldspace_id) VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, ?5, ?6)")?.execute( params![form_id, pos[0], pos[1], pos[2], cell_id, worldspace_id])?;
    }
    Ok(())
}

struct CellRow<'a> {
    form_id: u32,
    worldspace_id: Option<u32>,
    grid_x: Option<i32>,
    grid_y: Option<i32>,
    interior_name: Option<&'a str>,
    full_name: Option<&'a str>,
    flags: u32,
    data: &'a [u8],
}

fn insert_cell(tx: &Transaction<'_>, row: CellRow<'_>) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR REPLACE INTO cells(id, worldspace_id, grid_x, grid_y, interior_name, flags, data, full_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?.execute(
        params![row.form_id, row.worldspace_id, row.grid_x, row.grid_y, row.interior_name, row.flags, row.data, row.full_name],
    )?;
    Ok(())
}

pub fn validate_database(conn: &Connection) -> Result<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let version: u32 = conn.query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
        row.get(0)
    })?;
    if version != shared::WORLD_DATABASE_SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn combined_optional_schema_upgrade_preserves_rows_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        for table in ["worldspaces", "cells", "statics"] {
            conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN full_name;"))
                .unwrap();
        }
        conn.execute_batch("ALTER TABLE door_links DROP COLUMN destination_name;")
            .unwrap();
        // Use the pre-extension table verbatim: SQLite cannot DROP its final commented column.
        conn.execute_batch(
            "DROP TABLE lights;
             CREATE TABLE lights (
                 id INTEGER PRIMARY KEY, editor_id TEXT, radius REAL NOT NULL,
                 color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
                 flags INTEGER NOT NULL, falloff REAL NOT NULL, fade REAL
             );",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO statics(id, editor_id, flags) VALUES (1, 'retained', 0);")
            .unwrap();
        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();
        conn.prepare("SELECT destination_name FROM door_links")
            .unwrap();
        let row: (String, Option<String>) = conn
            .query_row(
                "SELECT editor_id, full_name FROM statics WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("retained".to_owned(), None));
        for table in ["worldspaces", "cells", "statics"] {
            conn.prepare(&format!("SELECT full_name FROM {table}"))
                .unwrap();
        }
        for (column, _) in LIGHT_DETAIL_COLUMNS {
            conn.prepare(&format!("SELECT {column} FROM lights"))
                .unwrap();
        }
    }

    #[test]
    fn complete_snapshot_replaces_both_optional_families_transactionally() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO cell_environment(cell_id) VALUES (1);
            INSERT INTO reference_details(reference_id) VALUES (2);
            INSERT INTO door_links(ref_id,destination_ref_id,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,destination_cell_id) VALUES (2,3,1,2,3,0,0,0,4);
            CREATE TABLE caller_owned(value INTEGER);
            INSERT INTO caller_owned VALUES (3);",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        replace_complete_snapshot(&tx).unwrap();
        for table in crate::esm::environment::ENVIRONMENT_TABLES
            .iter()
            .chain(crate::esm::reference_links::REFERENCE_LINK_TABLES.iter())
            .chain(["door_links"].iter())
        {
            let count: i64 = tx
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        tx.rollback().unwrap();
        for table in [
            "cell_environment",
            "reference_details",
            "door_links",
            "caller_owned",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
        let tx = conn.transaction().unwrap();
        replace_complete_snapshot(&tx).unwrap();
        tx.commit().unwrap();
        let value: i64 = conn
            .query_row("SELECT value FROM caller_owned", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, 3);
    }

    /// The schema has the interior index, the exterior spatial table and every projection table.
    #[test]
    fn creates_hybrid_spatial_schema() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let interior_index: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_references_cell'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let exterior_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'exterior_spatial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((interior_index, exterior_table), (1, 1));
        for table in [
            "worldspaces",
            "cells",
            "references",
            "land",
            "statics",
            "lights",
            "emittance",
            "npcs",
            "movement_types",
            "race_movement_links",
            "movement_game_settings",
            "scripts",
            "waters",
            "texture_sets",
            "landscape_textures",
            "grass_types",
            "landscape_texture_grasses",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "missing semantic table {table}");
        }
        validate_database(&conn).unwrap();
    }

    #[test]
    fn exports_selected_movement_values_and_rejects_bad_sped() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let speeds: Vec<u8> = [
            80.09f32,
            370.0,
            79.75,
            370.0,
            80.1,
            370.0,
            71.93,
            205.25,
            std::f32::consts::PI,
            std::f32::consts::PI,
            std::f32::consts::PI,
        ]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
        let movement = RawRecord {
            form_id: 0x0003_580D,
            record_type: *b"MOVT",
            flags: 0,
            subrecords: vec![
                (b"EDID".to_vec(), b"NPC_Default_MT\0".to_vec()),
                (b"SPED".to_vec(), speeds),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 2,
        };
        let jump = RawRecord {
            form_id: 0x000A_BEF6,
            record_type: *b"GMST",
            flags: 0,
            subrecords: vec![
                (b"EDID".to_vec(), b"fJumpHeightMin\0".to_vec()),
                (b"DATA".to_vec(), 76.0f32.to_le_bytes().to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        let race = RawRecord {
            form_id: 0x0001_3746,
            record_type: *b"RACE",
            flags: 0,
            subrecords: vec![(b"EDID".to_vec(), b"NordRace\0".to_vec())],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        let master = HashMap::from([
            (movement.form_id, movement.clone()),
            (jump.form_id, jump),
            (race.form_id, race.clone()),
        ]);
        export_to_db(&conn, &master).unwrap();
        let (walk_link, run_link, race_source): (Option<u32>, Option<u32>, u32) = conn
            .query_row(
                "SELECT walk_movt_id, run_movt_id, load_order FROM race_movement_links WHERE id=79686",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((walk_link, run_link, race_source), (None, None, 1));
        let (forward_walk, forward_run, back_run, source): (f32, f32, f32, u32) = conn.query_row(
            "SELECT forward_walk, forward_run, back_run, load_order FROM movement_types WHERE id=219149",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(
            (forward_walk, forward_run, back_run, source),
            (80.1, 370.0, 205.25, 2)
        );
        let height: f32 = conn
            .query_row(
                "SELECT value FROM movement_game_settings WHERE editor_id='fJumpHeightMin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(height, 76.0);

        let mut legacy = movement.clone();
        legacy.subrecords[1].1.truncate(40);
        export_to_db(&conn, &HashMap::from([(legacy.form_id, legacy)])).unwrap();

        let mut broken = movement;
        broken.subrecords[1].1[0..4].copy_from_slice(&f32::NAN.to_le_bytes());
        let error = export_to_db(&conn, &HashMap::from([(broken.form_id, broken)])).unwrap_err();
        assert!(error.to_string().contains("MOVT SPED[0]"));
        let mut broken_race = race;
        broken_race
            .subrecords
            .push((b"WKMV".to_vec(), vec![1, 2, 3]));
        let error =
            export_to_db(&conn, &HashMap::from([(broken_race.form_id, broken_race)])).unwrap_err();
        assert!(error.to_string().contains("RACE WKMV length"));
    }

    #[test]
    fn reads_water_flow_normals_from_nam5_not_binary_dnam() {
        let subrecords = vec![
            (b"DNAM".to_vec(), vec![0, 1, 2, 3, 4, 5]),
            (
                b"NAM5".to_vec(),
                b"Data\\Textures\\Water\\RiverFlow.dds\0".to_vec(),
            ),
        ];
        let view = SubrecordView::new(&subrecords);

        assert_eq!(
            water_flow_normal_path(&view).as_deref(),
            Some("water/riverflow.dds")
        );
    }

    /// A synthetic DNAM with DefaultWater's final (Update.esm-applied) values at the documented
    /// offsets (`docs/research/water.md` section 1.2/1.3): reflectivity 0.8, fresnel 0.10,
    /// shallow (38,39,24), deep (5,14,18), reflection (119,140,157). Everything else is zeroed.
    fn default_water_dnam(len: usize) -> Vec<u8> {
        let mut dnam = vec![0u8; len];
        dnam[20..24].copy_from_slice(&0.8f32.to_le_bytes());
        dnam[24..28].copy_from_slice(&0.10f32.to_le_bytes());
        dnam[40..44].copy_from_slice(&[38, 39, 24, 0]);
        dnam[44..48].copy_from_slice(&[5, 14, 18, 0]);
        dnam[48..52].copy_from_slice(&[119, 140, 157, 0]);
        dnam
    }

    #[test]
    fn parses_water_dnam_colors_and_factors_at_both_dnam_lengths() {
        for len in [228usize, 232] {
            let dnam = default_water_dnam(len);
            let parsed =
                parse_water_dnam(&dnam).unwrap_or_else(|| panic!("{len}-byte DNAM must decode"));
            assert_eq!(parsed.shallow_color, u32::from_le_bytes([38, 39, 24, 0]));
            assert_eq!(parsed.deep_color, u32::from_le_bytes([5, 14, 18, 0]));
            assert_eq!(
                parsed.reflection_color,
                u32::from_le_bytes([119, 140, 157, 0])
            );
            assert!((parsed.fresnel - 0.10).abs() < 1.0e-6);
            assert!((parsed.reflectivity - 0.8).abs() < 1.0e-6);
        }
    }

    #[test]
    fn water_export_reads_colors_from_dnam_not_nam0_nam1() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        // NAM0 (linear velocity) and NAM1 (angular velocity) are three f32s each; the old code
        // packed their first four bytes as if they were the shallow/deep colour. Give them a
        // value that would decode to a colour nothing like DNAM's, so the test fails on the old
        // code and passes once WATR reads colours from DNAM instead.
        let nam0: Vec<u8> = [9.0f32, 9.0, 9.0]
            .into_iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let nam1 = nam0.clone();
        let record = RawRecord {
            form_id: 0x0000_0018,
            record_type: *b"WATR",
            flags: 0,
            subrecords: vec![
                (b"EDID".to_vec(), b"DefaultWater\0".to_vec()),
                (b"NAM0".to_vec(), nam0),
                (b"NAM1".to_vec(), nam1),
                (b"DNAM".to_vec(), default_water_dnam(232)),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        let mut master = HashMap::new();
        master.insert(record.form_id, record);
        export_to_db(&conn, &master).unwrap();

        let (shallow, deep, reflection, fresnel, reflectivity): (u32, u32, u32, f32, f32) = conn
            .query_row(
                "SELECT shallow_color, deep_color, reflection_color, fresnel, reflectivity FROM waters WHERE id=?1",
                [0x0000_0018u32],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(shallow, u32::from_le_bytes([38, 39, 24, 0]));
        assert_eq!(deep, u32::from_le_bytes([5, 14, 18, 0]));
        assert_eq!(reflection, u32::from_le_bytes([119, 140, 157, 0]));
        assert!((fresnel - 0.10).abs() < 1.0e-6);
        assert!((reflectivity - 0.8).abs() < 1.0e-6);
    }

    #[test]
    fn reads_ltex_material_from_mnam_and_havok_from_hnam() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        // Real-world shape: MNAM is the material-type MATT FormID, HNAM is two
        // Havok bytes (friction, restitution). Values mirror observed rows like
        // LDirtSnowPath01 (HNAM 1e 1e).
        let record = RawRecord {
            form_id: 0x0001_B342,
            record_type: *b"LTEX",
            flags: 0,
            subrecords: vec![
                (b"EDID".to_vec(), b"LDirtSnowPath01\0".to_vec()),
                (b"TNAM".to_vec(), 0x0001_2F38u32.to_le_bytes().to_vec()),
                (b"MNAM".to_vec(), 0x0001_2F38u32.to_le_bytes().to_vec()),
                (b"HNAM".to_vec(), vec![0x1e, 0x1e]),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        let mut master = HashMap::new();
        master.insert(record.form_id, record);
        export_to_db(&conn, &master).unwrap();

        let (texture_set_id, material, friction, restitution): (
            Option<u32>,
            Option<u32>,
            Option<f32>,
            Option<f32>,
        ) = conn
            .query_row(
                "SELECT texture_set_id, material_type, friction, restitution FROM landscape_textures WHERE id=?1",
                [0x0001_B342u32],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(texture_set_id, Some(0x0001_2F38));
        assert_eq!(material, Some(0x0001_2F38));
        assert_eq!(friction, Some(30.0));
        assert_eq!(restitution, Some(30.0));
    }

    /// Exports placeable world models, including ARMO male and female world slots.
    #[test]
    fn exports_world_models_for_every_placeable_base_type() {
        /// Builds one synthetic base object with an optional model-slot payload.
        fn base(
            form_id: u32,
            record_type: &[u8; 4],
            model_tag: &[u8; 4],
            model: Option<&str>,
        ) -> RawRecord {
            let mut subrecords = vec![(b"EDID".to_vec(), b"TestBase\0".to_vec())];
            if let Some(model) = model {
                let mut path = model.as_bytes().to_vec();
                path.push(0);
                subrecords.push((model_tag.to_vec(), path));
            }
            RawRecord {
                form_id,
                record_type: *record_type,
                flags: 0,
                subrecords,
                cell_form_id: None,
                worldspace_form_id: None,
                load_order: 0,
            }
        }
        let master: HashMap<u32, RawRecord> = [
            base(1, b"TREE", b"MODL", Some("Landscape\\Trees\\Pine01.nif")),
            base(
                2,
                b"FLOR",
                b"MODL",
                Some("Landscape\\Plants\\Thistle01.nif"),
            ),
            base(3, b"CONT", b"MODL", Some("Furniture\\Chest01.nif")),
            base(4, b"DOOR", b"MODL", Some("Architecture\\Door01.nif")),
            base(5, b"ACTI", b"MODL", None),
            // A light with geometry lands in `statics` like any other modelled
            // base object; a modelless light lives only in the `lights` table
            // (PR40) and gets no `statics` row at all.
            base(6, b"LIGH", b"MODL", Some("Clutter\\Lantern01.nif")),
            base(7, b"WEAP", b"MODL", Some("Weapons\\Sword01.nif")),
            base(8, b"ARMO", b"MOD2", Some("Armor\\Helmet01.nif")),
            base(9, b"ARMO", b"MOD4", Some("Armor\\HelmetFemale01.nif")),
            base(10, b"NPC_", b"MODL", None),
            // ARMO carries a binary MODL chunk that is not a path; with no
            // MOD2/MOD4 present the row must store NULL, not garbage.
            base(11, b"ARMO", b"MODL", Some("G.\u{1}binary")),
        ]
        .into_iter()
        .map(|record| (record.form_id, record))
        .collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();
        let models: Vec<(u32, Option<String>)> = conn
            .prepare("SELECT id, model_path FROM statics ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            models,
            vec![
                (1, Some("Landscape\\Trees\\Pine01.nif".to_owned())),
                (2, Some("Landscape\\Plants\\Thistle01.nif".to_owned())),
                (3, Some("Furniture\\Chest01.nif".to_owned())),
                (4, Some("Architecture\\Door01.nif".to_owned())),
                (5, None),
                (6, Some("Clutter\\Lantern01.nif".to_owned())),
                (7, Some("Weapons\\Sword01.nif".to_owned())),
                (8, Some("Armor\\Helmet01.nif".to_owned())),
                (9, Some("Armor\\HelmetFemale01.nif".to_owned())),
                (11, None),
            ]
        );
    }

    #[test]
    fn indexes_persistent_exterior_references_by_global_position() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        let position = [147_182.05f32, 34_033.137f32, 80.0f32];
        let mut data = Vec::new();
        for value in position.into_iter().chain([0.0, 0.0, 0.0]) {
            data.extend_from_slice(&value.to_le_bytes());
        }
        insert_reference(
            &tx,
            0xE7F,
            1,
            Some((None, None, Some(0x3C))),
            0,
            &[(b"DATA".to_vec(), data)],
        )
        .unwrap();
        let (x, y, local_x, local_y): (f32, f32, f32, f32) = tx
            .query_row(
                "SELECT x.minX,x.minY,r.local_x,r.local_y FROM exterior_spatial x JOIN \"references\" r ON r.id=x.id WHERE x.id=?1",
                [0xE7Fu32],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert!((x - position[0]).abs() < 0.1);
        assert!((y - position[1]).abs() < 0.1);
        assert!((0.0..CELL_SIZE).contains(&local_x));
        assert!((0.0..CELL_SIZE).contains(&local_y));
    }

    fn cstr(value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    fn floats(values: [f32; 3]) -> Vec<u8> {
        values.into_iter().flat_map(f32::to_le_bytes).collect()
    }

    fn record(
        form_id: u32,
        record_type: &[u8; 4],
        cell: Option<u32>,
        worldspace: Option<u32>,
        subrecords: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> RawRecord {
        RawRecord {
            form_id,
            record_type: *record_type,
            flags: 0,
            subrecords,
            cell_form_id: cell,
            worldspace_form_id: worldspace,
            load_order: 0,
        }
    }

    /// A `REFR` inside `cell` with a `DATA` transform. The owning cell, not the
    /// record, supplies the worldspace the exporter stores, so it is not a
    /// parameter here.
    fn reference(
        form_id: u32,
        cell: u32,
        base_form_id: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) -> RawRecord {
        record(
            form_id,
            b"REFR",
            Some(cell),
            None,
            vec![
                (b"NAME".to_vec(), base_form_id.to_le_bytes().to_vec()),
                (
                    b"DATA".to_vec(),
                    [floats(position), floats(rotation)].concat(),
                ),
            ],
        )
    }

    /// A `LIGH` `DATA` built the way the game writes it: time, radius, colour
    /// (RGB + one unused byte), flags and falloff first, then the FOV, near
    /// clip, flicker period, two flicker amplitudes, value and weight the
    /// engine does not read - 48 bytes in all.
    fn light_data_bytes(radius: u32, color: [u8; 3], flags: u32, falloff: f32) -> Vec<u8> {
        let mut bytes = (-1i32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&radius.to_le_bytes());
        bytes.extend_from_slice(&[color[0], color[1], color[2], 0]);
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&falloff.to_le_bytes());
        for value in [90.0f32, 6.585, 0.333, 0.5, 16.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        assert_eq!(bytes.len(), 48);
        bytes
    }

    /// A `LIGH` record: editor id, `DATA`, and `FNAM` when the light has one.
    fn light(
        form_id: u32,
        editor_id: &str,
        data: Vec<u8>,
        fade: Option<f32>,
        model: Option<&str>,
    ) -> RawRecord {
        let mut subrecords = vec![
            (b"EDID".to_vec(), cstr(editor_id)),
            (b"DATA".to_vec(), data),
        ];
        if let Some(fade) = fade {
            subrecords.push((b"FNAM".to_vec(), fade.to_le_bytes().to_vec()));
        }
        if let Some(model) = model {
            subrecords.push((b"MODL".to_vec(), cstr(model)));
        }
        record(form_id, b"LIGH", None, None, subrecords)
    }

    #[test]
    fn writes_a_lights_row_from_the_light_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        master.insert(
            0xCB3B0,
            light(
                0xCB3B0,
                "DefaultTorch01NS_FastSaturate",
                light_data_bytes(256, [0xE9, 0x9E, 0x4B], 0x2009, 1.0),
                Some(1.25),
                None,
            ),
        );

        export_to_db(&conn, &master).unwrap();

        type LightRow = (u32, String, f64, i64, i64, i64, i64, f64, f64);
        let row: LightRow = conn
            .query_row(
                "SELECT id,editor_id,radius,color_r,color_g,color_b,flags,falloff,fade FROM lights WHERE id=?1",
                [0xCB3B0u32],
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
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                0xCB3B0,
                "DefaultTorch01NS_FastSaturate".to_owned(),
                256.0,
                233,
                158,
                75,
                0x2009,
                1.0,
                1.25,
            )
        );
    }

    #[test]
    fn gives_a_lights_row_to_a_light_with_no_model_and_none_to_a_light_without_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        // Invisible lights are the common case: they light the space with
        // nothing to draw, so they belong in `lights` and not in `statics`.
        master.insert(
            0x800,
            light(
                0x800,
                "FXLightInvisible",
                light_data_bytes(128, [255, 255, 255], 0x10, 1.0),
                None,
                None,
            ),
        );
        // A light that does have geometry is both.
        master.insert(
            0x600,
            light(
                0x600,
                "LightWithModel",
                light_data_bytes(512, [16, 32, 64], 0x1, 2.0),
                None,
                Some("Clutter\\InvisibleLightMarker.nif"),
            ),
        );
        // `radius`, `color_*`, `flags` and `falloff` are NOT NULL, so a record
        // whose DATA cannot supply them gets no row rather than invented ones.
        master.insert(
            0x900,
            record(
                0x900,
                b"LIGH",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("NoDataAtAll"))],
            ),
        );

        export_to_db(&conn, &master).unwrap();

        let rows: Vec<(u32, f64)> = {
            let mut statement = conn
                .prepare("SELECT id,radius FROM lights ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(rows, vec![(0x600, 512.0), (0x800, 128.0)]);
        let statics: Vec<u32> = {
            let mut statement = conn.prepare("SELECT id FROM statics ORDER BY id").unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<u32>>>()
                .unwrap()
        };
        assert_eq!(statics, vec![0x600], "only the modelled light is a static");
    }

    #[test]
    fn drops_a_truncated_light_data_without_panicking() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let full = light_data_bytes(256, [1, 2, 3], 0x8, 1.0);
        for (form_id, length) in [(0x1000u32, 0usize), (0x2000, 4), (0x3000, 12), (0x4000, 19)] {
            let mut truncated = full.clone();
            truncated.truncate(length);
            master.insert(form_id, light(form_id, "Truncated", truncated, None, None));
        }
        // A light whose DATA is shorter than the four fields the row needs is
        // dropped; the rest of the export still runs.
        master.insert(
            0x5000,
            light(
                0x5000,
                "Complete",
                full,
                Some(1.0),
                Some("Clutter\\Candle.nif"),
            ),
        );

        export_to_db(&conn, &master).unwrap();

        let ids: Vec<u32> = {
            let mut statement = conn.prepare("SELECT id FROM lights").unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<u32>>>()
                .unwrap()
        };
        assert_eq!(ids, vec![0x5000], "only the complete DATA produced a row");
        let statics: i64 = conn
            .query_row("SELECT count(*) FROM statics WHERE id=0x5000", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(statics, 1);
    }

    #[test]
    fn v174_exported_enable_parent_ignores_xesp_padding() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let mut expected = Vec::new();
        let parent = 0x1234_5678u32;
        let mut id = 0x1000;
        for record_type in [
            *b"REFR", *b"ACHR", *b"ACRE", *b"PGRE", *b"PMIS", *b"PHZD", *b"PARW", *b"PBAR",
            *b"PBEA", *b"PCON", *b"PFLA",
        ] {
            for flags in [0u8, 1, 2, 3, 0x80, 0xFF] {
                for padding in [[0; 3], [0x0D, 0xBF, 0x38]] {
                    let mut record = reference(id, 0x100, 0x900, [0.0; 3], [0.0; 3]);
                    record.record_type = record_type;
                    let mut xesp = parent.to_le_bytes().to_vec();
                    xesp.push(flags);
                    xesp.extend_from_slice(&padding);
                    record.subrecords.push((b"XESP".to_vec(), xesp));
                    master.insert(id, record);
                    expected.push((id, Some(parent), Some(u32::from(flags))));
                    id += 1;
                }
            }
        }
        // Missing and incomplete enable-parent subrecords have no projection.
        master.insert(id, reference(id, 0x100, 0x900, [0.0; 3], [0.0; 3]));
        expected.push((id, None, None));
        id += 1;
        for length in 0..8 {
            let mut record = reference(id, 0x100, 0x900, [0.0; 3], [0.0; 3]);
            record
                .subrecords
                .push((b"XESP".to_vec(), vec![0xFF; length]));
            master.insert(id, record);
            expected.push((id, None, None));
            id += 1;
        }

        export_to_db(&conn, &master).unwrap();

        for (id, parent, flags) in expected {
            let actual: (Option<u32>, Option<u32>, Vec<u8>) = conn
                .query_row(
                    "SELECT enable_parent_id, enable_parent_flags, data FROM \"references\" WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!((actual.0, actual.1), (parent, flags), "reference {id:08X}");
            let raw =
                rkyv::from_bytes::<crate::esm::types::ArchivedRecordData, rkyv::rancor::Error>(
                    &actual.2,
                )
                .unwrap();
            let stored = raw.subrecords.iter().find(|sub| &sub.tag == b"XESP");
            let original = master[&id]
                .subrecords
                .iter()
                .find(|(tag, _)| tag == b"XESP");
            assert_eq!(
                stored.map(|sub| &sub.data),
                original.map(|(_, data)| data),
                "raw XESP for reference {id:08X}"
            );
        }
    }

    #[test]
    fn stores_a_reference_radius_override_from_xrds() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let mut overridden = reference(0x1000, 0x100, 0x900, [0.0; 3], [0.0; 3]);
        overridden.subrecords.push((
            b"XRDS".to_vec(),
            544.3856f32.to_le_bytes().to_vec(), // an ice candle in Skyrim.esm
        ));
        master.insert(0x1000, overridden);
        master.insert(0x2000, reference(0x2000, 0x100, 0x900, [0.0; 3], [0.0; 3]));
        // `XRDS` shorter than the float it holds is not a radius.
        let mut truncated = reference(0x3000, 0x100, 0x900, [0.0; 3], [0.0; 3]);
        truncated
            .subrecords
            .push((b"XRDS".to_vec(), vec![0xAE, 0x18]));
        master.insert(0x3000, truncated);

        export_to_db(&conn, &master).unwrap();

        let radius = |id: u32| -> Option<f64> {
            conn.query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(radius(0x1000), Some(f64::from(544.3856f32)));
        assert_eq!(radius(0x2000), None, "a reference without XRDS stays NULL");
        assert_eq!(radius(0x3000), None, "a two-byte XRDS holds no radius");
    }

    /// The in-house decoder drops a field too short for its float before export, so the exporter
    /// never sees the bytes. The rejection it reports must write the same "malformed" status (2)
    /// the legacy reader writes for the same bytes, not "absent" (0).
    #[test]
    fn rejected_light_inputs_write_the_same_malformed_status_as_the_legacy_reader() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let data = || light_data_bytes(256, [255, 200, 120], 0, 1.0);
        // Legacy shape: the short fields are still in the record.
        let mut legacy_light = light(0xA00, "LegacyShortFade", data(), None, None);
        legacy_light
            .subrecords
            .push((b"FNAM".to_vec(), vec![0xAE, 0x18]));
        master.insert(0xA00, legacy_light);
        let mut legacy_ref = reference(0xB00, 0x100, 0xA00, [0.0; 3], [0.0; 3]);
        legacy_ref
            .subrecords
            .push((b"XRDS".to_vec(), vec![0xAE, 0x18]));
        master.insert(0xB00, legacy_ref);
        // In-house shape: the same fields were rejected, so the record has no such subrecord.
        master.insert(0xA01, light(0xA01, "RejectedFade", data(), None, None));
        master.insert(0xB01, reference(0xB01, 0x100, 0xA01, [0.0; 3], [0.0; 3]));
        // Neither shape: genuinely absent.
        master.insert(0xA02, light(0xA02, "NoFade", data(), None, None));
        master.insert(0xB02, reference(0xB02, 0x100, 0xA02, [0.0; 3], [0.0; 3]));
        // A valid field that is present wins over a rejection flag for a duplicate occurrence.
        master.insert(0xA03, light(0xA03, "ValidFade", data(), Some(1.5), None));
        let mut valid_ref = reference(0xB03, 0x100, 0xA03, [0.0; 3], [0.0; 3]);
        valid_ref
            .subrecords
            .push((b"XRDS".to_vec(), 64.0f32.to_le_bytes().to_vec()));
        master.insert(0xB03, valid_ref);

        let mut rejected = RejectedLightFields::new();
        rejected.insert(
            0xA01,
            RejectedLightInputs {
                fnam: true,
                xrds: false,
            },
        );
        rejected.insert(
            0xB01,
            RejectedLightInputs {
                fnam: false,
                xrds: true,
            },
        );
        rejected.insert(
            0xA03,
            RejectedLightInputs {
                fnam: true,
                xrds: false,
            },
        );
        rejected.insert(
            0xB03,
            RejectedLightInputs {
                fnam: false,
                xrds: true,
            },
        );
        export_records(
            &conn,
            &master,
            None,
            None,
            None,
            None,
            ExportOptions {
                runtime_views: true,
                scalar_scripts: false,
                rejected_light_fields: Some(&rejected),
            },
        )
        .unwrap();

        let fade = |id: u32| -> (Option<f64>, i64) {
            conn.query_row(
                "SELECT fade, fade_status FROM lights WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        let radius = |id: u32| -> (Option<f64>, i64) {
            conn.query_row(
                "SELECT radius_override, radius_status FROM \"references\" WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(fade(0xA00), (None, 2), "legacy short FNAM");
        assert_eq!(fade(0xA01), (None, 2), "rejected FNAM");
        assert_eq!(fade(0xA02), (None, 0), "absent FNAM");
        assert_eq!(fade(0xA03), (Some(1.5), 1), "valid FNAM beats the flag");
        assert_eq!(radius(0xB00), (None, 2), "legacy short XRDS");
        assert_eq!(radius(0xB01), (None, 2), "rejected XRDS");
        assert_eq!(radius(0xB02), (None, 0), "absent XRDS");
        assert_eq!(radius(0xB03), (Some(64.0), 1), "valid XRDS beats the flag");
    }

    fn omission_ids(conn: &Connection) -> Vec<(String, u64, Vec<u32>)> {
        ProjectionOmissions::read(conn)
            .unwrap()
            .categories
            .into_iter()
            .map(|(category, outcome)| {
                let ids = outcome
                    .samples
                    .iter()
                    .map(|sample| sample.form_id)
                    .collect();
                (category, outcome.count, ids)
            })
            .collect()
    }

    /// Every light and emittance branch that writes no row is counted under its own category,
    /// next to healthy rows that are still written (#297).
    #[test]
    fn records_light_and_emittance_omissions_beside_healthy_rows() {
        use crate::esm::projection_omissions::Severity;
        let order = LoadOrder::from_metadata(
            vec!["Lights.esp".into()],
            vec![crate::esm::binary::PluginMetadata {
                masters: Vec::new(),
                flags: 0,
            }],
        )
        .unwrap();
        let mut short = light_data_bytes(256, [1, 2, 3], 0, 1.0);
        short.truncate(12);
        let healthy = light(
            0x700,
            "Healthy",
            light_data_bytes(256, [255, 200, 120], 0, 1.0),
            None,
            None,
        );
        let broken = [
            record(
                0x801,
                b"LIGH",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("NoData"))],
            ),
            light(0x802, "Short", short, None, None),
        ];
        let mut records = vec![
            healthy.clone(),
            emitting_reference(0x1000, 0x700),
            emitting_reference(0x1001, 0xDEAD),
            emitting_reference(0x1002, 0x900),
            emitting_reference(0x1003, 0x600),
            emitting_reference(0x1004, 0x610),
            emitting_reference(0x1005, 0x802),
            record(0x900, b"STAT", None, None, Vec::new()),
            record(0x600, b"REGN", None, None, Vec::new()),
            region(0x610, &[(0x611, 100)]),
            record(0x611, b"WTHR", None, None, Vec::new()),
        ];
        records.extend(broken.iter().cloned());
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .map(|record| (record.form_id, record))
            .collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();

        export_to_db_with_load_order(&conn, &master, &order).unwrap();

        let lights: Vec<u32> = conn
            .prepare("SELECT id FROM lights ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(lights, vec![0x700], "the healthy light keeps its row");
        assert_eq!(
            emittance_row(&conn, 0x700),
            Some(("LIGH".into(), None, [[255, 200, 120]; 4]))
        );
        let expected = vec![
            ("emittance_light_unusable_data".to_owned(), 1, vec![0x802]),
            ("emittance_region_no_weather".to_owned(), 1, vec![0x600]),
            ("emittance_target_absent".to_owned(), 1, vec![0xDEAD]),
            ("emittance_target_wrong_type".to_owned(), 1, vec![0x900]),
            ("emittance_weather_no_lighting".to_owned(), 1, vec![0x610]),
            ("light_missing_data".to_owned(), 1, vec![0x801]),
            ("light_short_data".to_owned(), 1, vec![0x802]),
        ];
        assert_eq!(omission_ids(&conn), expected);
        let stored = ProjectionOmissions::read(&conn).unwrap();
        for (category, outcome) in &stored.categories {
            let data_loss = category.starts_with("light_");
            assert_eq!(
                outcome.severity == Severity::DataLoss,
                data_loss,
                "{category}"
            );
        }
        assert!(
            stored.categories["light_short_data"].samples[0]
                .reason
                .contains("12 bytes"),
            "{stored:?}"
        );

        // Exporting the same load order again replaces the counts instead of adding to them.
        export_to_db_with_load_order(&conn, &master, &order).unwrap();
        assert_eq!(omission_ids(&conn), expected);

        // A subset export does not own the snapshot, so it keeps the stored omissions.
        export_to_db(&conn, &HashMap::new()).unwrap();
        assert_eq!(omission_ids(&conn), expected);

        // Once the data is repaired, a full export leaves nothing behind.
        let repaired: HashMap<u32, RawRecord> = [healthy, emitting_reference(0x1000, 0x700)]
            .into_iter()
            .map(|record| (record.form_id, record))
            .collect();
        export_to_db_with_load_order(&conn, &repaired, &order).unwrap();
        assert!(omission_ids(&conn).is_empty());
    }

    /// A reference lit by `source` through `XEMI`.
    fn emitting_reference(form_id: u32, source: u32) -> RawRecord {
        let mut reference = reference(form_id, 0x100, 0x900, [0.0; 3], [0.0; 3]);
        reference
            .subrecords
            .push((b"XEMI".to_vec(), source.to_le_bytes().to_vec()));
        reference
    }

    /// A `WTHR` whose 14 `NAM0` colour types are black except Effect Lighting.
    fn weather(form_id: u32, effect_lighting: [[u8; 3]; 4]) -> RawRecord {
        let mut nam0 = vec![0u8; 14 * 16];
        for (time, rgb) in effect_lighting.iter().enumerate() {
            nam0[9 * 16 + time * 4..9 * 16 + time * 4 + 3].copy_from_slice(rgb);
        }
        record(form_id, b"WTHR", None, None, vec![(b"NAM0".to_vec(), nam0)])
    }

    /// A `REGN` listing `(weather, chance)` pairs, without globals.
    fn region(form_id: u32, weathers: &[(u32, u32)]) -> RawRecord {
        let rdwt = weathers
            .iter()
            .flat_map(|(weather, chance)| {
                [weather.to_le_bytes(), chance.to_le_bytes(), [0; 4]].concat()
            })
            .collect();
        record(form_id, b"REGN", None, None, vec![(b"RDWT".to_vec(), rdwt)])
    }

    type EmittanceRow = (String, Option<u32>, [[u8; 3]; 4]);

    fn emittance_row(conn: &Connection, id: u32) -> Option<EmittanceRow> {
        conn.query_row(
            "SELECT source_type, weather_id, sunrise_r, sunrise_g, sunrise_b, day_r, day_g, day_b,
                    sunset_r, sunset_g, sunset_b, night_r, night_g, night_b
             FROM emittance WHERE id=?1",
            [id],
            |row| {
                let colour = |time: usize| -> Result<[u8; 3]> {
                    Ok([
                        row.get(2 + time * 3)?,
                        row.get(3 + time * 3)?,
                        row.get(4 + time * 3)?,
                    ])
                };
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    [colour(0)?, colour(1)?, colour(2)?, colour(3)?],
                ))
            },
        )
        .optional()
        .unwrap()
    }

    /// Writes the `emittance` rows the way the load-order export does after its records.
    fn export_with_emittance(conn: &Connection, master: &HashMap<u32, RawRecord>) {
        export_to_db(conn, master).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        insert_emittance_sources(&tx, master, &mut ProjectionOmissions::default()).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn resolves_reference_emittance_from_regions_and_lights() {
        const INVERT_WINDOW: [[u8; 3]; 4] =
            [[124, 70, 16], [0, 0, 0], [243, 118, 54], [242, 176, 55]];
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        for record in [
            emitting_reference(0x1000, 0x8277B),
            emitting_reference(0x1001, 0x8277B),
            emitting_reference(0x2000, 0x700),
            emitting_reference(0x3000, 0x600),
            emitting_reference(0x4000, 0x900),
            emitting_reference(0x5000, 0xDEAD),
            emitting_reference(0x6000, 0),
            reference(0x7000, 0x100, 0x900, [0.0; 3], [0.0; 3]),
            // The likeliest weather wins.
            region(0x8277B, &[(0x500, 10), (0x8277A, 90)]),
            weather(0x500, [[255; 3]; 4]),
            weather(0x8277A, INVERT_WINDOW),
            light(
                0x700,
                "WindowFill",
                light_data_bytes(256, [255, 200, 120], 0, 1.0),
                None,
                None,
            ),
            // A region whose only weather is missing resolves to nothing.
            region(0x600, &[(0x601, 100)]),
            // `XEMI` names a light or a region; a static has no colour to lend.
            record(0x900, b"STAT", None, None, Vec::new()),
        ] {
            master.insert(record.form_id, record);
        }

        export_with_emittance(&conn, &master);

        let emittance_id = |id: u32| -> Option<u32> {
            conn.query_row(
                "SELECT emittance_id FROM \"references\" WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(emittance_id(0x1000), Some(0x8277B));
        assert_eq!(emittance_id(0x6000), None, "a cleared XEMI stores NULL");
        assert_eq!(emittance_id(0x7000), None, "no XEMI stores NULL");

        assert_eq!(
            emittance_row(&conn, 0x8277B),
            Some(("REGN".into(), Some(0x8277A), INVERT_WINDOW))
        );
        assert_eq!(
            emittance_row(&conn, 0x700),
            Some(("LIGH".into(), None, [[255, 200, 120]; 4]))
        );
        for unresolved in [0x600, 0x900, 0xDEAD] {
            assert_eq!(emittance_row(&conn, unresolved), None, "{unresolved:08X}");
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM emittance", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }

    /// A second export into the same database drops the colour of a target that no longer
    /// resolves, instead of keeping the first export's row.
    #[test]
    fn re_exporting_drops_emittance_that_no_longer_resolves() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        for record in [
            emitting_reference(0x1000, 0x8277B),
            region(0x8277B, &[(0x8277A, 100)]),
            weather(0x8277A, [[0; 3]; 4]),
        ] {
            master.insert(record.form_id, record);
        }
        export_with_emittance(&conn, &master);
        assert!(emittance_row(&conn, 0x8277B).is_some());

        master.remove(&0x8277A);
        export_with_emittance(&conn, &master);
        assert_eq!(emittance_row(&conn, 0x8277B), None);
    }

    /// A `references` table from before the emittance export has no `emittance_id`; `create_tables`
    /// adds it, so a complete export into a reused database still inserts its references.
    #[test]
    fn create_tables_adds_emittance_id_to_an_older_references_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE "references" (
                id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL, worldspace_id INTEGER,
                base_form_id INTEGER NOT NULL, is_exterior INTEGER NOT NULL,
                pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
                local_x REAL, local_y REAL, rot_x REAL NOT NULL, rot_y REAL NOT NULL,
                rot_z REAL NOT NULL, scale REAL NOT NULL DEFAULT 1.0,
                radius_override REAL, header_flags INTEGER NOT NULL DEFAULT 0,
                enable_parent_id INTEGER, enable_parent_flags INTEGER, data BLOB
            );"#,
        )
        .unwrap();
        create_tables(&conn).unwrap();
        // A second call finds the column and leaves the table alone.
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        for record in [
            emitting_reference(0x1000, 0x8277B),
            region(0x8277B, &[(0x8277A, 100)]),
            weather(0x8277A, [[0; 3]; 4]),
        ] {
            master.insert(record.form_id, record);
        }
        export_with_emittance(&conn, &master);
        let emittance_id: Option<u32> = conn
            .query_row(
                "SELECT emittance_id FROM \"references\" WHERE id=0x1000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(emittance_id, Some(0x8277B));
        assert!(emittance_row(&conn, 0x8277B).is_some());
    }

    /// The Sleeping Giant Inn's reference in Riverwood names `FXLightRegionInvertWindowWhiterun`,
    /// whose weather `FXWthrInvertWindowsWhiterun` gives its windows no colour by day (#242).
    ///
    /// Reads the game install, so it is opt-in:
    /// `cargo test -p converter --lib -- --ignored`.
    #[test]
    #[ignore = "requires MUDCRAB_SKYRIM_DATA with locally installed game assets"]
    fn the_sleeping_giant_inn_windows_are_dark_by_day() {
        const INN: u32 = 0x2D763;
        const REGION: u32 = 0x8277B;
        const WEATHER: u32 = 0x8277A;
        let data_dir = std::env::var_os("MUDCRAB_SKYRIM_DATA")
            .or_else(|| std::env::var_os("OPENSKYRIM_SKYRIM_DATA"))
            .map(std::path::PathBuf::from)
            .expect("set MUDCRAB_SKYRIM_DATA to the Skyrim Data directory");
        let records = crate::esm::binary::parse_plugin_file(&data_dir.join("Skyrim.esm")).unwrap();
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .filter(|record| [INN, REGION, WEATHER].contains(&record.form_id))
            .map(|record| (record.form_id, record))
            .collect();
        assert_eq!(master.len(), 3, "the inn, its region and its weather");

        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_with_emittance(&conn, &master);

        let source: Option<u32> = conn
            .query_row(
                "SELECT emittance_id FROM \"references\" WHERE id=?1",
                [INN],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, Some(REGION));
        assert_eq!(
            emittance_row(&conn, REGION),
            Some((
                "REGN".into(),
                Some(WEATHER),
                [[124, 70, 16], [0, 0, 0], [243, 118, 54], [242, 176, 55]]
            ))
        );
    }

    /// The real `LIGH` records and their references, read through the same
    /// `export_to_db` path the converter uses. Two things were measured here
    /// before the table was written, and both are asserted so a change in the
    /// game data or in this code shows up as a failure rather than as wrong
    /// light in the engine:
    ///
    /// - `DATA` is 48 bytes in every `LIGH` record of Skyrim.esm, which is the
    ///   size of the layout UESP documents.
    /// - `XRDS`, the reference's own radius, rides on almost every light
    ///   reference (10,810 of 12,148 in Skyrim.esm), which is why the
    ///   `references` table has a `radius_override` column at all.
    ///
    /// The expected torch values are the bytes `DefaultTorch01NS_FastSaturate`
    /// (000CB3B0) holds: `FF FF FF FF` time -1, `00 01 00 00` radius 256,
    /// `E9 9E 4B 00` colour (233, 158, 75), `09 20 00 00` flags 0x2009,
    /// `00 00 80 3F` falloff 1.0, and `FNAM` 1.0.
    ///
    /// Reads the game install, so it is opt-in:
    /// `cargo test -p converter --lib -- --ignored`.
    #[test]
    #[ignore = "requires MUDCRAB_SKYRIM_DATA with locally installed game assets"]
    fn lights_of_the_real_plugin_decode_through_export() {
        use std::collections::HashSet;
        let data_dir = std::env::var_os("MUDCRAB_SKYRIM_DATA")
            .or_else(|| std::env::var_os("OPENSKYRIM_SKYRIM_DATA"))
            .map(std::path::PathBuf::from)
            .expect("set MUDCRAB_SKYRIM_DATA to the Skyrim Data directory");
        let plugin = data_dir.join("Skyrim.esm");
        assert!(plugin.is_file(), "no Skyrim.esm at {}", plugin.display());
        let records = crate::esm::binary::parse_plugin_file(&plugin).unwrap();

        let mut light_count = 0usize;
        for record in records.iter().filter(|r| r.record_type == *b"LIGH") {
            light_count += 1;
            let data_length = record
                .subrecords
                .iter()
                .find(|(tag, _)| tag.as_slice() == b"DATA")
                .map(|(_, data)| data.len());
            assert_eq!(
                data_length,
                Some(48),
                "LIGH {:08X} DATA is {data_length:?} bytes, expected 48",
                record.form_id
            );
        }
        eprintln!("LIGH records with a 48-byte DATA: {light_count}");
        assert!(light_count > 400, "only {light_count} LIGH records found");

        let light_ids: HashSet<u32> = records
            .iter()
            .filter(|record| record.record_type == *b"LIGH")
            .map(|record| record.form_id)
            .collect();
        let (mut all_refs, mut all_overrides) = (0usize, 0usize);
        for record in records.iter().filter(|r| r.record_type == *b"REFR") {
            let base = record
                .subrecords
                .iter()
                .find(|(tag, _)| tag.as_slice() == b"NAME")
                .filter(|(_, data)| data.len() >= 4)
                .map(|(_, data)| u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
            if !base.is_some_and(|base| light_ids.contains(&base)) {
                continue;
            }
            let has_override = record
                .subrecords
                .iter()
                .any(|(tag, data)| tag.as_slice() == b"XRDS" && data.len() >= 4);
            all_refs += 1;
            all_overrides += usize::from(has_override);
        }
        eprintln!("LIGH references with an XRDS radius override: {all_overrides}/{all_refs}");
        assert!(
            all_overrides * 10 > all_refs * 8,
            "an override is common in the plugin itself"
        );

        const TORCH: u32 = 0xCB3B0;
        // An ice candle's reference, its `IceCandleLight01` base, and the cell it stands in.
        const CANDLE_REF: u32 = 0x56D56;
        const CANDLE_LIGHT: u32 = 0x194F4;
        let wanted: HashSet<u32> = [TORCH, CANDLE_REF, CANDLE_LIGHT, 0x152C3]
            .into_iter()
            .collect();
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .filter(|record| wanted.contains(&record.form_id))
            .map(|record| (record.form_id, record))
            .collect();
        for form_id in &wanted {
            assert!(
                master.contains_key(form_id),
                "{form_id:08X} is not in Skyrim.esm"
            );
        }

        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();

        type TorchRow = (f64, i64, i64, i64, i64, f64, f64);
        let torch: TorchRow = conn
            .query_row(
                "SELECT radius,color_r,color_g,color_b,flags,falloff,fade FROM lights WHERE id=?1",
                [TORCH],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            torch,
            (256.0, 233, 158, 75, 0x2009, 1.0, 1.0),
            "DefaultTorch01NS_FastSaturate"
        );
        // A torch is warm: reading the colour at the wrong offset would not be.
        assert!(
            torch.1 > torch.2 && torch.2 > torch.3,
            "torch colour is not warm: ({}, {}, {})",
            torch.1,
            torch.2,
            torch.3
        );

        // The reference's override is its own, not its base light's: 544.4
        // units against the 256 its `IceCandleLight01` base asks for.
        let override_radius: Option<f64> = conn
            .query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [CANDLE_REF],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(override_radius, Some(f64::from(544.3856f32)));
        let base_radius: f64 = conn
            .query_row(
                "SELECT radius FROM lights WHERE id=?1",
                [CANDLE_LIGHT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(base_radius, 256.0);
    }

    // ---- Interior room graph (db-schema.md section 15) ----

    use crate::esm::rooms::{self, PORTAL_MARKER_BASE, ROOM_MARKER_BASE};

    fn xprm(half: [f32; 3], shape: u32) -> Vec<u8> {
        let mut bytes = floats(half);
        bytes.extend(floats([0.5, 0.5, 0.5]));
        bytes.extend(0.2f32.to_le_bytes());
        bytes.extend(shape.to_le_bytes());
        bytes
    }

    fn word(value: u32) -> Vec<u8> {
        value.to_le_bytes().to_vec()
    }

    /// A marker reference: `NAME`, `DATA` and any extra subrecords.
    fn marker(
        form_id: u32,
        cell: u32,
        base: u32,
        position: [f32; 3],
        rotation: [f32; 3],
        extra: Vec<(&[u8; 4], Vec<u8>)>,
    ) -> RawRecord {
        let mut record = reference(form_id, cell, base, position, rotation);
        record
            .subrecords
            .extend(extra.into_iter().map(|(tag, data)| (tag.to_vec(), data)));
        record
    }

    fn room(
        form_id: u32,
        cell: u32,
        position: [f32; 3],
        rotation: [f32; 3],
        half: [f32; 3],
        mut extra: Vec<(&[u8; 4], Vec<u8>)>,
    ) -> RawRecord {
        extra.insert(0, (b"XPRM", xprm(half, 1)));
        marker(form_id, cell, ROOM_MARKER_BASE, position, rotation, extra)
    }

    fn portal(form_id: u32, cell: u32, sides: Option<(u32, u32)>) -> RawRecord {
        let mut extra = vec![(b"XPRM", xprm([40.0, 1.0, 60.0], 3))];
        if let Some((a, b)) = sides {
            extra.push((b"XPOD", [word(a), word(b)].concat()));
        }
        marker(form_id, cell, PORTAL_MARKER_BASE, [0.0; 3], [0.0; 3], extra)
    }

    fn interior_cell(form_id: u32) -> RawRecord {
        record(form_id, b"CELL", None, None, Vec::new())
    }

    /// Full runtime exports own the room graph; diagnostic archives defer it and subsets retain it.
    #[test]
    fn archive_exports_clear_room_graph_and_runtime_exports_restore_it() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = directory.path().join("Rooms.esp");
        std::fs::write(&plugin, b"synthetic room provenance").unwrap();
        let paths = [plugin];
        let order = LoadOrder::from_metadata(
            vec!["Rooms.esp".into()],
            vec![crate::esm::binary::PluginMetadata {
                masters: Vec::new(),
                flags: 0,
            }],
        )
        .unwrap();
        let records = vec![
            interior_cell(0x100),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [100.0; 3], vec![]),
            portal(0x3001, 0x100, Some((0x1001, 0))),
            reference(0x2001, 0x100, 0x500, [20.0, 0.0, 0.0], [0.0; 3]),
        ];
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .map(|record| (record.form_id, record))
            .collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_inhouse_to_db(
            &conn,
            &master,
            &order,
            &paths,
            &HashMap::new(),
            &HashMap::new(),
            None,
        )
        .unwrap();
        let expected = rooms::RoomGraphReport::read(&conn).unwrap();
        assert_eq!((expected.rooms, expected.portals), (1, 1));
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);

        export_to_db(&conn, &HashMap::new()).unwrap();
        assert_eq!(rooms::RoomGraphReport::read(&conn).unwrap(), expected);
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);

        export_inhouse_record_archive_to_db(&conn, &master, &order, &paths).unwrap();
        for table in [
            "room_bounds",
            "room_portals",
            "room_links",
            "reference_rooms",
            "room_graph_stats",
            "projection_omission_stats",
            "projection_omission_samples",
            "cells",
            "references",
        ] {
            let count: u64 = conn
                .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "deferred runtime table {table}");
        }
        let canonical_rows: u64 = conn
            .query_row("SELECT count(*) FROM records", [], |row| row.get(0))
            .unwrap();
        assert_eq!(canonical_rows, 4);
        export_inhouse_to_db(
            &conn,
            &master,
            &order,
            &paths,
            &HashMap::new(),
            &HashMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(rooms::RoomGraphReport::read(&conn).unwrap(), expected);
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);
    }

    // ---- Load door links (db-schema.md section 17) ----

    fn load_door(form_id: u32, cell: u32, destination: u32, arrival: [f32; 3]) -> RawRecord {
        let mut xtel = word(destination);
        xtel.extend(floats(arrival));
        xtel.extend(floats([0.0, 0.0, 1.5]));
        xtel.extend(word(0));
        let mut door = reference(form_id, cell, 0x500, [0.0; 3], [0.0; 3]);
        door.subrecords.push((b"XTEL".to_vec(), xtel));
        door
    }

    fn door_links(conn: &Connection) -> Vec<(u32, u32, u32, Option<String>)> {
        conn.prepare(
            "SELECT ref_id,destination_ref_id,destination_cell_id,destination_name
             FROM door_links ORDER BY ref_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// Full runtime exports own `door_links`; diagnostic archives clear it and subsets retain it.
    #[test]
    fn archive_exports_clear_door_links_and_runtime_exports_restore_them() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = directory.path().join("Doors.esp");
        std::fs::write(&plugin, b"synthetic door provenance").unwrap();
        let paths = [plugin];
        let order = LoadOrder::from_metadata(
            vec!["Doors.esp".into()],
            vec![crate::esm::binary::PluginMetadata {
                masters: Vec::new(),
                flags: 0,
            }],
        )
        .unwrap();
        let named_cell = |form_id: u32, name: &[u8]| {
            record(
                form_id,
                b"CELL",
                None,
                None,
                vec![(b"FULL".to_vec(), name.to_vec())],
            )
        };
        let records = vec![
            record(0x500, b"DOOR", None, None, Vec::new()),
            named_cell(0x100, b"Hall\0"),
            named_cell(0x101, b"Cellar\0"),
            load_door(0x2001, 0x100, 0x2002, [1.0, 2.0, 3.0]),
            load_door(0x2002, 0x101, 0x2001, [4.0, 5.0, 6.0]),
        ];
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .map(|record| (record.form_id, record))
            .collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        assert!(door_links(&conn).is_empty(), "create_tables adds the table");
        let export = |conn: &Connection| {
            export_inhouse_to_db(
                conn,
                &master,
                &order,
                &paths,
                &HashMap::new(),
                &HashMap::new(),
                None,
            )
            .unwrap()
        };
        let expected = vec![
            (0x2001, 0x2002, 0x101, Some("Cellar".to_owned())),
            (0x2002, 0x2001, 0x100, Some("Hall".to_owned())),
        ];
        export(&conn);
        assert_eq!(door_links(&conn), expected);

        export_to_db(&conn, &HashMap::new()).unwrap();
        assert_eq!(door_links(&conn), expected, "a subset export leaves links");

        export_inhouse_record_archive_to_db(&conn, &master, &order, &paths).unwrap();
        assert!(door_links(&conn).is_empty(), "deferred runtime table");
        export(&conn);
        assert_eq!(door_links(&conn), expected);
    }

    /// Exports `records` and returns the connection plus the room graph report.
    fn export_rooms(records: Vec<RawRecord>) -> (Connection, rooms::RoomGraphReport) {
        let master: HashMap<u32, RawRecord> = records.into_iter().map(|r| (r.form_id, r)).collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();
        let interior: std::collections::HashSet<u32> = master
            .values()
            .filter(|r| &r.record_type == b"CELL" && r.worldspace_form_id.is_none())
            .map(|r| r.form_id)
            .collect();
        let tx = conn.unchecked_transaction().unwrap();
        let report = rooms::export_room_graph(&tx, &master, &interior).unwrap();
        tx.commit().unwrap();
        (conn, report)
    }

    fn rooms_of(conn: &Connection, reference: u32) -> Vec<(u32, i64)> {
        let mut statement = conn
            .prepare(
                "SELECT room_ref, contained FROM reference_rooms WHERE reference_id=?1 ORDER BY room_ref",
            )
            .unwrap();
        statement
            .query_map([reference], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn assigns_objects_to_the_room_boxes_that_contain_their_pivot() {
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [100.0; 3], vec![]),
            room(
                0x1002,
                0x100,
                [150.0, 0.0, 0.0],
                [0.0; 3],
                [100.0; 3],
                vec![],
            ),
            reference(0x2001, 0x100, 0x500, [-50.0, 0.0, 0.0], [0.0; 3]),
            reference(0x2002, 0x100, 0x500, [1000.0, 0.0, 0.0], [0.0; 3]),
            reference(0x2003, 0x100, 0x500, [75.0, 0.0, 0.0], [0.0; 3]),
        ]);
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);
        assert_eq!(rooms_of(&conn, 0x2002), vec![]);
        assert_eq!(rooms_of(&conn, 0x2003), vec![(0x1001, 1), (0x1002, 1)]);
        // Markers are not members of anything.
        assert_eq!(rooms_of(&conn, 0x1001), vec![]);
        assert_eq!(report.rooms, 2);
        assert_eq!(report.objects_in_no_room, 1);
        assert_eq!(report.objects_in_several_rooms, 1);
        let read = rooms::RoomGraphReport::read(&conn).unwrap();
        assert_eq!(read, report);
    }

    #[test]
    fn a_rotated_room_is_tested_in_its_own_frame() {
        // Long along local X (half 100), narrow along local Y (half 10). Turned a quarter
        // turn about Z, the long side lies along world Y.
        let (conn, _) = export_rooms(vec![
            interior_cell(0x100),
            room(
                0x1001,
                0x100,
                [0.0; 3],
                [0.0, 0.0, std::f32::consts::FRAC_PI_2],
                [100.0, 10.0, 50.0],
                vec![],
            ),
            reference(0x2001, 0x100, 0x500, [0.0, 80.0, 0.0], [0.0; 3]),
            reference(0x2002, 0x100, 0x500, [80.0, 0.0, 0.0], [0.0; 3]),
        ]);
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);
        assert_eq!(rooms_of(&conn, 0x2002), vec![]);
    }

    #[test]
    fn writes_room_bounds_with_lighting_template_image_space_flags_and_links() {
        let xrmr = vec![2, 0xC0, 1, 0];
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            room(
                0x1001,
                0x100,
                [10.0, 20.0, 30.0],
                [0.1, 0.2, 0.3],
                [100.0, 200.0, 300.0],
                vec![
                    (b"XRMR", xrmr),
                    (b"LNAM", word(0x0001_2345)),
                    (b"INAM", word(0x0006_7890)),
                    (b"XLRM", word(0x1002)),
                    (b"XLRM", word(0x1003)),
                    (b"XLRM", word(0)),
                ],
            ),
            room(0x1002, 0x100, [0.0; 3], [0.0; 3], [1.0; 3], vec![]),
        ]);
        let (cell, pos_x, pos_z, rot_y, rot_z): (u32, f64, f64, f64, f64) = conn
            .query_row(
                "SELECT cell_id,pos_x,pos_z,rot_y,rot_z FROM room_bounds WHERE ref_id=0x1001",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(cell, 0x100);
        assert_eq!((pos_x, pos_z), (10.0, 30.0));
        assert_eq!((rot_y, rot_z), (f64::from(0.2f32), f64::from(0.3f32)));
        let half: (f64, f64, f64) = conn
            .query_row(
                "SELECT half_x,half_y,half_z FROM room_bounds WHERE ref_id=0x1001",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(half, (100.0, 200.0, 300.0));
        let lighting: (Option<u32>, Option<u32>, i64) = conn
            .query_row(
                "SELECT lighting_template_id,image_space_id,flags FROM room_bounds WHERE ref_id=0x1001",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        // flags: XRMR flags byte | (u16 << 8)
        assert_eq!(
            lighting,
            (Some(0x0001_2345), Some(0x0006_7890), 0xC0 | (1 << 8))
        );
        let second: (Option<u32>, Option<u32>) = conn
            .query_row(
                "SELECT lighting_template_id,image_space_id FROM room_bounds WHERE ref_id=0x1002",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(second, (None, None));
        // Links are stored as written, including one to a room that does not exist; a zero
        // target (no room) is not a link.
        let links: Vec<(u32, u32)> = conn
            .prepare("SELECT room_ref, linked_ref FROM room_links ORDER BY linked_ref")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(links, vec![(0x1001, 0x1002), (0x1001, 0x1003)]);
        assert_eq!(report.links, 2);
    }

    #[test]
    fn portals_name_the_rooms_on_both_sides_or_null() {
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            interior_cell(0x101),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            room(0x1002, 0x100, [50.0, 0.0, 0.0], [0.0; 3], [10.0; 3], vec![]),
            room(0x1003, 0x101, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            portal(0x3001, 0x100, Some((0x1001, 0x1002))),
            portal(0x3002, 0x100, Some((0x1001, 0))),
            // 0x9999 is no room; 0x1003 is a room of another cell.
            portal(0x3003, 0x100, Some((0x9999, 0x1003))),
            portal(0x3004, 0x100, None),
        ]);
        let sides = |id: u32| -> (Option<u32>, Option<u32>) {
            conn.query_row(
                "SELECT room_a, room_b FROM room_portals WHERE ref_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(sides(0x3001), (Some(0x1001), Some(0x1002)));
        assert_eq!(sides(0x3002), (Some(0x1001), None));
        assert_eq!(sides(0x3003), (None, None));
        assert_eq!(sides(0x3004), (None, None));
        let size: (f64, f64) = conn
            .query_row(
                "SELECT half_x, half_z FROM room_portals WHERE ref_id=0x3001",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(size, (40.0, 60.0));
        assert_eq!(report.portals, 4);
        assert_eq!(report.portals_with_missing_side, 3);
    }

    /// A portal marker whose `XPOD` is the given raw bytes.
    fn portal_with_xpod(form_id: u32, cell: u32, xpod: Vec<u8>) -> RawRecord {
        marker(
            form_id,
            cell,
            PORTAL_MARKER_BASE,
            [0.0; 3],
            [0.0; 3],
            vec![(b"XPRM", xprm([40.0, 1.0, 60.0], 3)), (b"XPOD", xpod)],
        )
    }

    fn portal_rows(conn: &Connection, id: u32) -> Vec<(i64, Option<u32>, Option<u32>)> {
        conn.prepare(
            "SELECT pair_index, room_a, room_b FROM room_portals WHERE ref_id=?1 ORDER BY pair_index",
        )
        .unwrap()
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    #[test]
    fn a_portal_with_several_xpod_pairs_gets_one_row_per_pair() {
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            room(0x1002, 0x100, [50.0, 0.0, 0.0], [0.0; 3], [10.0; 3], vec![]),
            room(0x1003, 0x100, [90.0, 0.0, 0.0], [0.0; 3], [10.0; 3], vec![]),
            // Two pairs.
            portal_with_xpod(
                0x3001,
                0x100,
                [word(0x1001), word(0x1002), word(0x1002), word(0x1003)].concat(),
            ),
            // One pair and four trailing bytes, which are ignored and counted.
            portal_with_xpod(
                0x3002,
                0x100,
                [word(0x1001), word(0x1002), word(0x1003)].concat(),
            ),
            // The old shape: exactly one pair.
            portal_with_xpod(0x3003, 0x100, [word(0x1002), word(0x1003)].concat()),
            // Shorter than one pair: one row, NULL sides, counted as trailing bytes.
            portal_with_xpod(0x3004, 0x100, word(0x1001)),
            // Empty XPOD, and none at all.
            portal_with_xpod(0x3005, 0x100, Vec::new()),
            portal(0x3006, 0x100, None),
        ]);
        assert_eq!(
            portal_rows(&conn, 0x3001),
            vec![
                (0, Some(0x1001), Some(0x1002)),
                (1, Some(0x1002), Some(0x1003))
            ]
        );
        assert_eq!(
            portal_rows(&conn, 0x3002),
            vec![(0, Some(0x1001), Some(0x1002))]
        );
        assert_eq!(
            portal_rows(&conn, 0x3003),
            vec![(0, Some(0x1002), Some(0x1003))]
        );
        for id in [0x3004, 0x3005, 0x3006] {
            assert_eq!(portal_rows(&conn, id), vec![(0, None, None)], "{id:X}");
        }
        // Geometry repeats on every row of a portal.
        let sizes: Vec<(f64, f64)> = conn
            .prepare("SELECT half_x, half_z FROM room_portals WHERE ref_id=0x3001")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(sizes, vec![(40.0, 60.0); 2]);
        // `portals` counts markers; the missing-side counter counts rows (3004-3006).
        assert_eq!(report.portals, 6);
        assert_eq!(report.portals_with_missing_side, 3);
        assert_eq!(report.portals_with_extra_pairs, 1);
        assert_eq!(report.portals_with_trailing_xpod_bytes, 2);
        let read = rooms::RoomGraphReport::read(&conn).unwrap();
        assert_eq!(read, report);
    }

    /// A database whose `room_portals` has the shape before `pair_index`, holding two portals.
    fn old_shape_database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE room_portals (
                 ref_id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL,
                 pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
                 rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,
                 half_x REAL NOT NULL, half_z REAL NOT NULL,
                 room_a INTEGER, room_b INTEGER
             );
             CREATE INDEX idx_room_portals_cell ON room_portals(cell_id);
             INSERT INTO room_portals VALUES (1, 2, 0, 0, 0, 0, 0, 0, 3, 4, 7, 8);
             INSERT INTO room_portals VALUES (5, 2, 1, 2, 3, 0, 0, 0, 5, 6, NULL, NULL);",
        )
        .unwrap();
        conn
    }

    /// `room_portals` as (ref_id, pair_index, cell_id, half_x, room_a, room_b).
    type PortalTableRow = (i64, i64, i64, f64, Option<i64>, Option<i64>);

    fn portal_table(conn: &Connection) -> Vec<PortalTableRow> {
        conn.prepare(
            "SELECT ref_id, pair_index, cell_id, half_x, room_a, room_b FROM room_portals ORDER BY ref_id, pair_index",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn old_shape_rows() -> Vec<PortalTableRow> {
        vec![(1, 0, 2, 3.0, Some(7), Some(8)), (5, 0, 2, 5.0, None, None)]
    }

    #[test]
    fn an_older_room_portals_table_is_migrated_in_place_keeping_its_rows() {
        let conn = old_shape_database();
        create_tables(&conn).unwrap();
        assert_eq!(portal_table(&conn), old_shape_rows());
        // The key is now (ref_id, pair_index), and the cell index is back.
        conn.execute(
            "INSERT INTO room_portals(ref_id, pair_index, cell_id, pos_x, pos_y, pos_z, rot_x, rot_y, rot_z, half_x, half_z)
             VALUES (1, 1, 2, 0, 0, 0, 0, 0, 0, 3, 4)",
            [],
        )
        .unwrap();
        let index: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_room_portals_cell'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
        let leftovers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='room_portals_new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0);
        // A second pass is a no-op.
        create_tables(&conn).unwrap();
        assert_eq!(portal_table(&conn).len(), 3);
    }

    #[test]
    fn a_subset_export_on_a_migrated_database_leaves_the_portals_alone() {
        let conn = old_shape_database();
        create_tables(&conn).unwrap();
        let records = vec![
            interior_cell(0x100),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
        ];
        let master: HashMap<u32, RawRecord> = records.into_iter().map(|r| (r.form_id, r)).collect();
        export_to_db(&conn, &master).unwrap();
        assert_eq!(portal_table(&conn), old_shape_rows());
    }

    /// A RELEASE that fails (another connection holds a read lock, so the commit is busy)
    /// leaves no transaction open and the old table untouched; the next pass migrates it.
    #[test]
    fn a_busy_commit_rolls_the_migration_back_and_closes_the_savepoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE room_portals (
                     ref_id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL,
                     pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
                     rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,
                     half_x REAL NOT NULL, half_z REAL NOT NULL,
                     room_a INTEGER, room_b INTEGER
                 );
                 INSERT INTO room_portals VALUES (1, 2, 0, 0, 0, 0, 0, 0, 3, 4, 7, 8);",
            )
            .unwrap();
        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        let reader = Connection::open(&path).unwrap();
        // An open read transaction keeps its shared lock until COMMIT.
        reader.execute_batch("BEGIN").unwrap();
        let held: i64 = reader
            .query_row("SELECT count(*) FROM room_portals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(held, 1);

        assert!(rooms::migrate_room_portals(&writer).is_err());
        assert!(writer.is_autocommit(), "the savepoint was left open");

        reader.execute_batch("COMMIT").unwrap();
        use rusqlite::OptionalExtension;
        let pair_index: Option<i64> = writer
            .query_row(
                "SELECT 1 FROM pragma_table_info('room_portals') WHERE name='pair_index'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(pair_index, None);
        rooms::migrate_room_portals(&writer).unwrap();
        assert_eq!(
            portal_table(&writer),
            vec![(1, 0, 2, 3.0, Some(7), Some(8))]
        );
    }

    #[test]
    fn a_failed_full_export_leaves_the_migrated_portals_intact() {
        use crate::esm::load_order::LoadOrder;
        let conn = old_shape_database();
        create_tables(&conn).unwrap();
        // Fail when the export writes its counters, which is after it cleared the room tables
        // inside the transaction.
        conn.execute_batch(
            "CREATE TRIGGER fail_stats_insert BEFORE INSERT ON room_graph_stats
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
        )
        .unwrap();
        let master: HashMap<u32, RawRecord> = HashMap::new();
        let error = export_to_db_with_load_order(&conn, &master, &LoadOrder::read(&[]).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("forced failure"), "{error}");
        assert_eq!(portal_table(&conn), old_shape_rows());
    }

    #[test]
    fn skips_non_box_rooms_and_exterior_markers() {
        let mut sphere = room(0x1001, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]);
        sphere.subrecords.retain(|(tag, _)| tag != b"XPRM");
        sphere
            .subrecords
            .push((b"XPRM".to_vec(), xprm([10.0; 3], 2)));
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            // An exterior cell: it has a worldspace.
            record(0x200, b"CELL", None, Some(0x3C), Vec::new()),
            sphere,
            room(0x1002, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            room(0x1003, 0x200, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            portal(0x3001, 0x200, Some((0x1003, 0x1003))),
            reference(0x2001, 0x200, 0x500, [0.0; 3], [0.0; 3]),
        ]);
        let ids: Vec<u32> = conn
            .prepare("SELECT ref_id FROM room_bounds")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec![0x1002]);
        assert_eq!(report.rooms, 1);
        assert_eq!(report.non_box_rooms_skipped, 1);
        assert_eq!(report.portals, 0);
        assert_eq!(rooms_of(&conn, 0x2001), vec![]);
    }

    #[test]
    fn a_repeated_export_replaces_the_room_tables() {
        let records = vec![
            interior_cell(0x100),
            room(0x1001, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            reference(0x2001, 0x100, 0x500, [0.0; 3], [0.0; 3]),
        ];
        let master: HashMap<u32, RawRecord> = records.into_iter().map(|r| (r.form_id, r)).collect();
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();
        let interior = std::collections::HashSet::from([0x100u32]);
        for _ in 0..2 {
            let tx = conn.unchecked_transaction().unwrap();
            rooms::export_room_graph(&tx, &master, &interior).unwrap();
            tx.commit().unwrap();
        }
        let count: i64 = conn
            .query_row("SELECT count(*) FROM reference_rooms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // With no rooms left in the records the tables empty out.
        let tx = conn.unchecked_transaction().unwrap();
        rooms::export_room_graph(&tx, &HashMap::new(), &interior).unwrap();
        tx.commit().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM room_bounds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn skips_markers_with_non_finite_sizes() {
        let (conn, report) = export_rooms(vec![
            interior_cell(0x100),
            room(
                0x1001,
                0x100,
                [0.0; 3],
                [0.0; 3],
                [f32::NAN, 1.0, 1.0],
                vec![],
            ),
            room(
                0x1002,
                0x100,
                [0.0; 3],
                [0.0; 3],
                [f32::INFINITY, 1.0, 1.0],
                vec![],
            ),
            room(0x1003, 0x100, [0.0; 3], [0.0; 3], [10.0; 3], vec![]),
            marker(
                0x3001,
                0x100,
                PORTAL_MARKER_BASE,
                [0.0; 3],
                [0.0; 3],
                vec![(b"XPRM", xprm([f32::NAN, 1.0, 60.0], 3))],
            ),
            portal(0x3002, 0x100, Some((0x1003, 0))),
        ]);
        assert_eq!(report.rooms, 1);
        assert_eq!(report.portals, 1);
        assert_eq!(report.invalid_markers_skipped, 3);
        let read = rooms::RoomGraphReport::read(&conn).unwrap();
        assert_eq!(read.invalid_markers_skipped, 3);
    }

    /// A room turned 0.6 rad about Z with an asymmetric box: probe points on its long axis are
    /// inside only under the right inverse. The forward matrix, or the opposite turn, puts the
    /// same points outside.
    #[test]
    fn a_room_turned_by_an_arbitrary_yaw_uses_the_inverse_rotation() {
        let (sin, cos) = 0.6f32.sin_cos();
        // Local +X in world space is (cos, -sin) for a Creation yaw of +0.6 (clockwise).
        let along = |distance: f32| [distance * cos, -distance * sin, 0.0];
        let mirrored = |distance: f32| [distance * cos, distance * sin, 0.0];
        let beside = {
            let on_axis = along(100.0);
            // 30 from the axis across the narrow side (half 20).
            [on_axis[0] + 30.0 * sin, on_axis[1] + 30.0 * cos, 0.0]
        };
        let (conn, _) = export_rooms(vec![
            interior_cell(0x100),
            room(
                0x1001,
                0x100,
                [0.0; 3],
                [0.0, 0.0, 0.6],
                [200.0, 20.0, 50.0],
                vec![],
            ),
            reference(0x2001, 0x100, 0x500, along(150.0), [0.0; 3]),
            reference(0x2002, 0x100, 0x500, along(190.0), [0.0; 3]),
            reference(0x2003, 0x100, 0x500, along(210.0), [0.0; 3]),
            reference(0x2004, 0x100, 0x500, mirrored(150.0), [0.0; 3]),
            reference(0x2005, 0x100, 0x500, beside, [0.0; 3]),
        ]);
        assert_eq!(rooms_of(&conn, 0x2001), vec![(0x1001, 1)]);
        assert_eq!(rooms_of(&conn, 0x2002), vec![(0x1001, 1)]);
        assert_eq!(rooms_of(&conn, 0x2003), vec![]);
        assert_eq!(rooms_of(&conn, 0x2004), vec![]);
        assert_eq!(rooms_of(&conn, 0x2005), vec![]);
    }
}
