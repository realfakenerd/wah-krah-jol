use crate::esm::{
    extractors::{SubrecordView, extract_cell_info, extract_land_data, serialize_subrecords},
    records::RawRecord,
};
use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    esm::records::record_type::vmad::parse_vmad,
};
use rusqlite::{Connection, Result, Transaction, params};
use std::{collections::HashMap, str::from_utf8};

const CELL_SIZE: f32 = 4096.0;

pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_info (version INTEGER NOT NULL);
         INSERT INTO schema_info(version) SELECT 4 WHERE NOT EXISTS (SELECT 1 FROM schema_info);
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
             id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL, parent_world INTEGER, flags INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS cells (
             id INTEGER PRIMARY KEY, worldspace_id INTEGER, grid_x INTEGER, grid_y INTEGER,
             interior_name TEXT, flags INTEGER NOT NULL, data BLOB
         );
         CREATE INDEX IF NOT EXISTS idx_cells_grid ON cells(worldspace_id, grid_x, grid_y);
         CREATE TABLE IF NOT EXISTS "references" (
             id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL, worldspace_id INTEGER,
             base_form_id INTEGER NOT NULL, is_exterior INTEGER NOT NULL,
             pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
             local_x REAL, local_y REAL, rot_x REAL NOT NULL, rot_y REAL NOT NULL,
             rot_z REAL NOT NULL, scale REAL NOT NULL DEFAULT 1.0,
             radius_override REAL, data BLOB
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
             fade REAL                      -- FNAM, if present
         );
         CREATE TABLE IF NOT EXISTS npcs (
             id INTEGER PRIMARY KEY, editor_id TEXT, full_name TEXT,
             race_id INTEGER, class_id INTEGER, flags INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_npcs_editor_id ON npcs(editor_id);
         CREATE TABLE IF NOT EXISTS lod (
             cell_id INTEGER NOT NULL, lod_level INTEGER NOT NULL, mesh_data BLOB NOT NULL,
             PRIMARY KEY (cell_id, lod_level)
         );
         CREATE TABLE IF NOT EXISTS waters (
             id INTEGER PRIMARY KEY, editor_id TEXT, opacity INTEGER, flags INTEGER NOT NULL,
             shallow_color INTEGER, deep_color INTEGER, reflection_color INTEGER,
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
    Ok(())
}

type CellMetadata = (Option<i32>, Option<i32>, Option<u32>);

pub fn export_to_db(conn: &Connection, master: &HashMap<u32, RawRecord>) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let mut cells: HashMap<u32, CellMetadata> = HashMap::new();

    for (&form_id, record) in master
        .iter()
        .filter(|(_, record)| &record.record_type == b"CELL")
    {
        let (grid_x, grid_y, interior_name) = extract_cell_info(&record.subrecords);
        let data = serialize_subrecords(&record.subrecords);
        insert_cell(
            &tx,
            CellRow {
                form_id,
                worldspace_id: record.worldspace_form_id,
                grid_x,
                grid_y,
                interior_name: interior_name.as_deref(),
                flags: record.flags,
                data: &data,
            },
        )?;
        cells.insert(form_id, (grid_x, grid_y, record.worldspace_form_id));
    }

    let mut ordered: Vec<_> = master.iter().collect();
    ordered.sort_unstable_by_key(|(form_id, _)| **form_id);
    for (&form_id, record) in ordered {
        let type_str = from_utf8(&record.record_type).unwrap_or("UNKN");
        let blob = serialize_subrecords(&record.subrecords);
        tx.execute(
            "INSERT OR REPLACE INTO records(form_id, record_type, cell_id, worldspace_id, load_order, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![form_id, type_str, record.cell_form_id, record.worldspace_form_id, record.load_order, blob],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO formid_map(form_id, plugin_name, internal_id, record_type) VALUES (?1, 'merged', ?1, ?2)",
            params![form_id, type_str],
        )?;

        let view = SubrecordView::new(&record.subrecords);
        if let Some(vmad_bytes) = view.find(b"VMAD")
            && let Ok((_, vmad)) = parse_vmad(vmad_bytes, &record.record_type)
        {
            for script in vmad.scripts {
                let properties = serde_json::to_string(&script.properties)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.execute(
                    "INSERT OR REPLACE INTO scripts(form_id, script_name, vmad, properties_json) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, script.name, vmad_bytes, properties],
                )?;
            }
        }

        match type_str {
            "WRLD" => {
                let view = SubrecordView::new(&record.subrecords);
                let editor_id = view
                    .get_string(b"EDID")
                    .unwrap_or_else(|| format!("WRLD_{form_id:08X}"));
                tx.execute(
                    "INSERT OR REPLACE INTO worldspaces(id, editor_id, parent_world, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, editor_id, view.get_form_id(b"WNAM"), record.flags],
                )?;
            }
            "REFR" | "ACHR" | "ACRE" | "PGRE" | "PMIS" => {
                let cell_id = record.cell_form_id.unwrap_or(0);
                insert_reference(
                    &tx,
                    form_id,
                    cell_id,
                    cells.get(&cell_id).copied(),
                    &record.subrecords,
                )?;
            }
            "LAND" => {
                let (heightmap, vtex, vclr, normals) = extract_land_data(&record.subrecords);
                let cell_id = record.cell_form_id.unwrap_or(form_id);
                tx.execute("INSERT OR REPLACE INTO land(cell_id, heightmap, vtex, vclr, normals) VALUES (?1, ?2, ?3, ?4, ?5)", params![cell_id, heightmap, vtex, vclr, normals])?;
            }
            "LIGH" => {
                let view = SubrecordView::new(&record.subrecords);
                // Every `LIGH` lights the space whether or not it has
                // geometry, so it gets a `lights` row regardless.
                insert_light(&tx, form_id, &view)?;
                // Most `LIGH` records carry no model: they light the space with
                // nothing to draw. Storing one row per invisible light would put
                // a meshless entry in `statics` for every candle in the game, so
                // those are skipped; a light with geometry is stored like any
                // other base object.
                if let Some(model_path) = view.get_string(b"MODL").filter(|path| !path.is_empty()) {
                    tx.execute(
                        "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags) VALUES (?1, ?2, ?3, ?4)",
                        params![form_id, view.get_string(b"EDID"), model_path, record.flags],
                    )?;
                }
            }
            "STAT" | "MSTT" | "FURN" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, view.get_string(b"EDID"), view.get_string(b"MODL"), record.flags],
                )?;
            }
            "NPC_" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO npcs(id, editor_id, full_name, race_id, class_id, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![form_id, view.get_string(b"EDID"), view.get_string(b"FULL"), view.get_form_id(b"RNAM"), view.get_form_id(b"CNAM"), record.flags],
                )?;
            }
            "WATR" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO waters(id,editor_id,opacity,flags,shallow_color,deep_color,reflection_color,flow_normal_path,data) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        view.find(b"ANAM").and_then(|bytes| bytes.first()).copied(),
                        record.flags,
                        packed_color(view.find(b"NAM0")),
                        packed_color(view.find(b"NAM1")),
                        packed_color(view.find(b"NAM2")),
                        water_flow_normal_path(&view),
                        blob,
                    ],
                )?;
            }
            "TXST" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO texture_sets(id,editor_id,diffuse_path,normal_path,glow_path,height_path,environment_path,mask_path,specular_path,detail_path) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        view.get_string(b"TX00"),
                        view.get_string(b"TX01"),
                        view.get_string(b"TX02"),
                        view.get_string(b"TX03"),
                        view.get_string(b"TX04"),
                        view.get_string(b"TX05"),
                        view.get_string(b"TX06"),
                        view.get_string(b"TX07"),
                    ],
                )?;
            }
            "LTEX" => {
                let view = SubrecordView::new(&record.subrecords);
                let material = view
                    .find(b"HNAM")
                    .filter(|bytes| bytes.len() >= 2)
                    .map(|bytes| {
                        u16::from_le_bytes(bytes[..2].try_into().expect("two-byte material type"))
                    });
                tx.execute(
                    "INSERT OR REPLACE INTO landscape_textures(id,editor_id,texture_set_id,material_type,friction,restitution) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![form_id, view.get_string(b"EDID"), view.get_form_id(b"TNAM"), material, Option::<f32>::None, Option::<f32>::None],
                )?;
            }
            _ => {}
        }
    }
    tx.commit()
}

fn water_flow_normal_path(view: &SubrecordView<'_>) -> Option<String> {
    view.get_string(b"NAM5").and_then(|path| {
        canonical_asset_path(&path, AssetKind::Texture, "dds")
            .ok()
            .map(|canonical| canonical.trim_start_matches("textures/").to_owned())
    })
}

fn packed_color(bytes: Option<&[u8]>) -> Option<u32> {
    bytes
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| u32::from_le_bytes(bytes[..4].try_into().expect("four-byte color")))
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

fn insert_light(tx: &Transaction<'_>, form_id: u32, view: &SubrecordView<'_>) -> Result<()> {
    let Some(data) = view.find(b"DATA") else {
        eprintln!("warning: LIGH {form_id:08X} has no DATA; no lights row");
        return Ok(());
    };
    if data.len() < LIGHT_DATA_MIN {
        eprintln!(
            "warning: LIGH {form_id:08X} DATA is {} bytes, expected at least {LIGHT_DATA_MIN}; no lights row",
            data.len()
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
    tx.execute(
        "INSERT OR REPLACE INTO lights(id, editor_id, radius, color_r, color_g, color_b, flags, falloff, fade)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
        ],
    )?;
    Ok(())
}

pub fn insert_reference(
    tx: &Transaction<'_>,
    form_id: u32,
    cell_id: u32,
    cell: Option<CellMetadata>,
    subs: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    let view = SubrecordView::new(subs);
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
    let blob = serialize_subrecords(subs);
    // `XRDS` is the reference's own radius, in the same Creation units as a
    // light's `DATA` radius and usually different from it: 10,810 of the
    // 12,148 `LIGH` references in Skyrim.esm carry one. It rides on glow and beam
    // references as well, so any reference that has it keeps it; the engine
    // reads it for light references only, where it overrides the base
    // light's radius. It is a single little-endian `f32` and may be negative.
    let radius_override = view
        .find(b"XRDS")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| f32::from_le_bytes(bytes[..4].try_into().expect("four-byte XRDS radius")));

    tx.execute(
        "INSERT OR REPLACE INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, local_x, local_y, rot_x, rot_y, rot_z, scale, radius_override, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![form_id, cell_id, worldspace_id, base_form_id, is_exterior, pos[0], pos[1], pos[2], local_x, local_y, rot[0], rot[1], rot[2], scale, radius_override, blob],
    )?;
    if is_exterior {
        tx.execute("INSERT OR REPLACE INTO exterior_spatial(id, minX, maxX, minY, maxY, minZ, maxZ, cell_id, worldspace_id) VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, ?5, ?6)", params![form_id, pos[0], pos[1], pos[2], cell_id, worldspace_id])?;
    }
    Ok(())
}

struct CellRow<'a> {
    form_id: u32,
    worldspace_id: Option<u32>,
    grid_x: Option<i32>,
    grid_y: Option<i32>,
    interior_name: Option<&'a str>,
    flags: u32,
    data: &'a [u8],
}

fn insert_cell(tx: &Transaction<'_>, row: CellRow<'_>) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO cells(id, worldspace_id, grid_x, grid_y, interior_name, flags, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![row.form_id, row.worldspace_id, row.grid_x, row.grid_y, row.interior_name, row.flags, row.data],
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
            "npcs",
            "scripts",
            "waters",
            "texture_sets",
            "landscape_textures",
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
    #[ignore = "requires OPENSKYRIM_SKYRIM_DATA with locally installed game assets"]
    fn lights_of_the_real_plugin_decode_through_export() {
        use std::collections::HashSet;
        let data_dir = std::env::var_os("OPENSKYRIM_SKYRIM_DATA")
            .map(std::path::PathBuf::from)
            .expect("set OPENSKYRIM_SKYRIM_DATA to the Skyrim Data directory");
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
}
