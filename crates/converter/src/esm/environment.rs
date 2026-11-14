//! Cell and worldspace environment, weather, climate, image space and lighting templates
//! (#269 gaps 1 and 2). The tables are projected from the canonical winning records, so the
//! legacy and in-house readers publish the same rows (db-schema.md, "Cell and Worldspace
//! Environment" and "Weather, Climate, Image Space and Lighting Templates"). Layouts follow
//! xEdit 4.1.5f `Core/wbDefinitionsTES5.pas` and the in-house schema.

use super::{extractors::SubrecordView, records::RawRecord};
use rusqlite::{Result, Transaction, types::Value};
use std::collections::HashMap;

/// Tables this module owns. A complete export drops and recreates them.
pub(crate) const ENVIRONMENT_TABLES: [&str; 12] = [
    "cell_environment",
    "cell_regions",
    "cell_lighting",
    "worldspace_environment",
    "lighting_templates",
    "weathers",
    "weather_colors",
    "weather_directional_ambient",
    "climates",
    "climate_weathers",
    "region_weathers",
    "image_spaces",
];

/// The smallest `XCLL` xEdit accepts: every field up to and including the directional ambient
/// colours (optional from field 11). The full form is 92 bytes. `XCLL` is untyped bytes in the
/// in-house schema, so both readers keep any length and this rule applies to both.
const XCLL_MIN: usize = 72;

/// `LGTM` `DATA` lengths the in-house schema accepts (`short_ambient`, `specular_prefix` and the
/// `full_ambient` forms).
const LGTM_DATA_SIZES: &[usize] = &[64, 68, 72, 76, 80, 88, 92];

/// `DALC` lengths: six colours, then optionally the specular colour and the scale.
const DALC_SIZES: &[usize] = &[24, 28, 32];

/// `WTHR` `NAM0` lengths: 13 to 17 colour types of 16 bytes.
const NAM0_SIZES: &[usize] = &[208, 224, 240, 256, 272];

/// The subrecord when its length is one the in-house schema accepts. The in-house reader leaves a
/// field of another length out of the runtime projection; the legacy reader keeps it, so every
/// projection here applies the same sizes to make both readers agree.
fn sized<'a>(view: &SubrecordView<'a>, tag: &[u8; 4], sizes: &[usize]) -> &'a [u8] {
    view.find(tag)
        .filter(|bytes| sizes.contains(&bytes.len()))
        .unwrap_or_default()
}

/// Directional ambient (`DALC`) colours in file order, then the specular colour.
const DALC_COLOURS: [&str; 7] = [
    "x_pos", "x_neg", "y_pos", "y_neg", "z_pos", "z_neg", "specular",
];

/// `WTHR` `NAM0` colour types in file order (in-house schema `weather_colors`).
const WEATHER_COLOUR_TYPES: [&str; 17] = [
    "sky_upper",
    "fog_near",
    "unknown",
    "ambient",
    "sunlight",
    "sun",
    "stars",
    "sky_lower",
    "horizon",
    "effect_lighting",
    "cloud_lod_diffuse",
    "cloud_lod_ambient",
    "fog_far",
    "sky_statics",
    "water_multiplier",
    "sun_glare",
    "moon_glare",
];

const TIMES_OF_DAY: [&str; 4] = ["sunrise", "day", "sunset", "night"];

fn rgb_columns(prefix: &str) -> Vec<(String, &'static str)> {
    ["r", "g", "b"]
        .iter()
        .map(|channel| (format!("{prefix}_{channel}"), "INTEGER"))
        .collect()
}

/// The 22 columns of a 32-byte directional ambient block.
fn dalc_columns(prefix: &str) -> Vec<(String, &'static str)> {
    let mut columns: Vec<_> = DALC_COLOURS
        .iter()
        .flat_map(|colour| rgb_columns(&format!("{prefix}{colour}")))
        .collect();
    columns.push((format!("{prefix}scale"), "REAL"));
    columns
}

/// The columns `XCLL` and `LGTM` `DATA` share, from the ambient colour to the light fade end.
fn lighting_columns() -> Vec<(String, &'static str)> {
    let mut columns = Vec::new();
    for colour in ["ambient", "directional", "fog_near"] {
        columns.extend(rgb_columns(colour));
    }
    for (name, kind) in [
        ("fog_near", "REAL"),
        ("fog_far", "REAL"),
        ("directional_rotation_xy", "INTEGER"),
        ("directional_rotation_z", "INTEGER"),
        ("directional_fade", "REAL"),
        ("fog_clip_distance", "REAL"),
        ("fog_power", "REAL"),
    ] {
        columns.push((name.to_owned(), kind));
    }
    columns.extend(dalc_columns("dalc_"));
    columns.extend(rgb_columns("fog_far"));
    for name in ["fog_max", "light_fade_begin", "light_fade_end"] {
        columns.push((name.to_owned(), "REAL"));
    }
    columns
}

fn table_sql(table: &str, head: &str, columns: &[(String, &str)], tail: &str) -> String {
    let body = columns
        .iter()
        .map(|(name, kind)| format!("{name} {kind}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("CREATE TABLE IF NOT EXISTS {table} ({head}, {body}{tail});\n")
}

fn named(names: &[&str], kind: &'static str) -> Vec<(String, &'static str)> {
    names
        .iter()
        .map(|name| ((*name).to_owned(), kind))
        .collect()
}

/// DDL for every table this module owns; `create_tables` runs it.
pub(crate) fn tables_sql() -> String {
    let mut sql = String::new();
    sql += &table_sql(
        "cell_environment",
        "cell_id INTEGER PRIMARY KEY",
        &[
            named(
                &[
                    "data_flags",
                    "image_space_id",
                    "acoustic_space_id",
                    "music_id",
                    "sky_region_id",
                    "location_id",
                    "encounter_zone_id",
                    "owner_id",
                    "owner_rank",
                    "water_id",
                ],
                "INTEGER",
            ),
            named(&["water_height"], "REAL"),
        ]
        .concat(),
        "",
    );
    sql += "CREATE TABLE IF NOT EXISTS cell_regions (cell_id INTEGER NOT NULL, region_id INTEGER NOT NULL, PRIMARY KEY (cell_id, region_id)) WITHOUT ROWID;\n";
    let mut cell_lighting = named(&["lighting_template_id", "xcll_size"], "INTEGER");
    cell_lighting.extend(lighting_columns());
    cell_lighting.extend(named(&["inherit_flags"], "INTEGER"));
    sql += &table_sql(
        "cell_lighting",
        "cell_id INTEGER PRIMARY KEY",
        &cell_lighting,
        "",
    );
    sql += &table_sql(
        "worldspace_environment",
        "worldspace_id INTEGER PRIMARY KEY",
        &[
            named(
                &[
                    "data_flags",
                    "parent_flags",
                    "climate_id",
                    "water_id",
                    "lod_water_id",
                ],
                "INTEGER",
            ),
            named(
                &[
                    "lod_water_height",
                    "default_land_height",
                    "default_water_height",
                ],
                "REAL",
            ),
            named(
                &[
                    "lighting_template_id",
                    "music_id",
                    "location_id",
                    "encounter_zone_id",
                ],
                "INTEGER",
            ),
            named(
                &[
                    "bounds_min_x",
                    "bounds_min_y",
                    "bounds_max_x",
                    "bounds_max_y",
                    "lod_multiplier",
                ],
                "REAL",
            ),
            named(&["center_cell_x", "center_cell_y"], "INTEGER"),
        ]
        .concat(),
        "",
    );
    let mut templates = named(&["editor_id"], "TEXT");
    templates.extend(named(&["data_size"], "INTEGER"));
    templates.extend(lighting_columns());
    templates.extend(dalc_columns("subrecord_dalc_"));
    sql += &table_sql(
        "lighting_templates",
        "id INTEGER PRIMARY KEY",
        &templates,
        "",
    );
    let fog = named(
        &[
            "fog_day_near",
            "fog_day_far",
            "fog_night_near",
            "fog_night_far",
            "fog_day_power",
            "fog_night_power",
            "fog_day_max",
            "fog_night_max",
        ],
        "REAL",
    );
    let mut weathers = named(&["editor_id"], "TEXT");
    weathers.extend(fog);
    weathers.extend(named(&WEATHER_DATA.map(|(name, _)| name), "INTEGER"));
    weathers.extend(named(
        &[
            "disabled_cloud_layers",
            "precipitation_id",
            "visual_effect_id",
            "sky_static_id",
            "sun_glare_lens_id",
            "image_space_sunrise_id",
            "image_space_day_id",
            "image_space_sunset_id",
            "image_space_night_id",
            "volumetric_lighting_sunrise_id",
            "volumetric_lighting_day_id",
            "volumetric_lighting_sunset_id",
            "volumetric_lighting_night_id",
        ],
        "INTEGER",
    ));
    sql += &table_sql("weathers", "id INTEGER PRIMARY KEY", &weathers, "");
    sql += "CREATE TABLE IF NOT EXISTS weather_colors (weather_id INTEGER NOT NULL, color_type TEXT NOT NULL, time_of_day TEXT NOT NULL, r INTEGER NOT NULL, g INTEGER NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (weather_id, color_type, time_of_day)) WITHOUT ROWID;\n";
    sql += &table_sql(
        "weather_directional_ambient",
        "weather_id INTEGER NOT NULL, time_of_day TEXT NOT NULL",
        &dalc_columns(""),
        ", PRIMARY KEY (weather_id, time_of_day)",
    );
    sql += &table_sql(
        "climates",
        "id INTEGER PRIMARY KEY",
        &[
            named(&["editor_id", "sun_texture", "sun_glare_texture"], "TEXT"),
            named(
                &[
                    "sunrise_begin",
                    "sunrise_end",
                    "sunset_begin",
                    "sunset_end",
                    "volatility",
                    "moon_phase",
                ],
                "INTEGER",
            ),
        ]
        .concat(),
        "",
    );
    for (table, owner) in [
        ("climate_weathers", "climate_id"),
        ("region_weathers", "region_id"),
    ] {
        sql += &format!(
            "CREATE TABLE IF NOT EXISTS {table} ({owner} INTEGER NOT NULL, position INTEGER NOT NULL, weather_id INTEGER, chance INTEGER NOT NULL, chance_global_id INTEGER, PRIMARY KEY ({owner}, position));\n"
        );
    }
    sql += &table_sql(
        "image_spaces",
        "id INTEGER PRIMARY KEY",
        &[
            named(&["editor_id"], "TEXT"),
            named(
                &[
                    "eye_adaptation_speed",
                    "bloom_blur_radius",
                    "bloom_threshold",
                    "bloom_scale",
                    "receive_bloom_threshold",
                    "white",
                    "sunlight_scale",
                    "sky_scale",
                    "eye_adaptation_strength",
                    "saturation",
                    "brightness",
                    "contrast",
                    "tint_amount",
                    "tint_r",
                    "tint_g",
                    "tint_b",
                    "dof_strength",
                    "dof_distance",
                    "dof_range",
                ],
                "REAL",
            ),
            named(&["dof_sky_blur_radius"], "INTEGER"),
        ]
        .concat(),
        "",
    );
    sql
}

/// `WTHR` `DATA` bytes (19 in Skyrim SE), as raw unsigned values with their offsets.
const WEATHER_DATA: [(&str, usize); 17] = [
    ("wind_speed", 0),
    ("transition_delta", 3),
    ("sun_glare", 4),
    ("sun_damage", 5),
    ("precipitation_begin", 6),
    ("precipitation_end", 7),
    ("lightning_begin", 8),
    ("lightning_end", 9),
    ("lightning_frequency", 10),
    ("classification_flags", 11),
    ("lightning_r", 12),
    ("lightning_g", 13),
    ("lightning_b", 14),
    ("visual_effect_begin", 15),
    ("visual_effect_end", 16),
    ("wind_direction", 17),
    ("wind_direction_range", 18),
];

fn f32_at(bytes: &[u8], offset: usize) -> Option<f32> {
    let value = f32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?);
    value.is_finite().then_some(value)
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// One row, built column by column so the SQL and the values cannot drift apart.
#[derive(Default)]
struct Row {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl Row {
    fn put(&mut self, column: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.columns.push(column.into());
        self.values.push(value.into());
        self
    }

    fn real(&mut self, column: impl Into<String>, value: Option<f32>) -> &mut Self {
        self.put(column, value.map(f64::from))
    }

    /// An RGBA byte colour at `offset`: red, green and blue; the fourth byte is unused.
    fn rgb(&mut self, prefix: &str, bytes: &[u8], offset: usize) -> &mut Self {
        let rgb = bytes.get(offset..offset + 3);
        for (index, channel) in ["r", "g", "b"].iter().enumerate() {
            self.put(
                format!("{prefix}_{channel}"),
                rgb.map(|rgb| i64::from(rgb[index])),
            );
        }
        self
    }

    /// A 32-byte directional ambient block: six colours, specular, scale.
    fn dalc(&mut self, prefix: &str, bytes: &[u8]) -> &mut Self {
        for (index, colour) in DALC_COLOURS.iter().enumerate() {
            self.rgb(&format!("{prefix}{colour}"), bytes, index * 4);
        }
        self.real(format!("{prefix}scale"), f32_at(bytes, 28))
    }

    /// The fields `XCLL` and `LGTM` `DATA` share; NULL past the end of `bytes`.
    fn lighting(&mut self, bytes: &[u8]) -> &mut Self {
        self.rgb("ambient", bytes, 0)
            .rgb("directional", bytes, 4)
            .rgb("fog_near", bytes, 8)
            .real("fog_near", f32_at(bytes, 12))
            .real("fog_far", f32_at(bytes, 16))
            .put("directional_rotation_xy", i32_at(bytes, 20))
            .put("directional_rotation_z", i32_at(bytes, 24))
            .real("directional_fade", f32_at(bytes, 28))
            .real("fog_clip_distance", f32_at(bytes, 32))
            .real("fog_power", f32_at(bytes, 36))
            .dalc("dalc_", bytes.get(40..).unwrap_or_default())
            .rgb("fog_far", bytes, 72)
            .real("fog_max", f32_at(bytes, 76))
            .real("light_fade_begin", f32_at(bytes, 80))
            .real("light_fade_end", f32_at(bytes, 84))
    }

    fn insert(&self, tx: &Transaction<'_>, table: &str) -> Result<()> {
        let placeholders = (1..=self.values.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        tx.prepare_cached(&format!(
            "INSERT OR REPLACE INTO {table}({}) VALUES ({placeholders})",
            self.columns.join(", ")
        ))?
        .execute(rusqlite::params_from_iter(&self.values))?;
        Ok(())
    }
}

/// Link columns keep a FormID only when the winning record it names exists and has one of the
/// types xEdit allows there. The in-house reader already clears links that do not resolve; the
/// legacy reader keeps them, so the check here makes both publish the same rows.
pub(crate) struct Links<'a> {
    pub(crate) master: &'a HashMap<u32, RawRecord>,
}

impl Links<'_> {
    fn id(&self, id: Option<u32>, targets: &[&[u8; 4]]) -> Option<u32> {
        id.filter(|id| {
            self.master.get(id).is_some_and(|record| {
                !record.is_deleted() && targets.contains(&&record.record_type)
            })
        })
    }

    fn get(&self, view: &SubrecordView<'_>, tag: &[u8; 4], targets: &[&[u8; 4]]) -> Option<u32> {
        self.id(view.get_form_id(tag), targets)
    }
}

/// Writes the environment rows of one winning `CELL`.
pub(crate) fn insert_cell(
    tx: &Transaction<'_>,
    links: &Links<'_>,
    cell_id: u32,
    view: &SubrecordView<'_>,
) -> Result<()> {
    let link = |tag: &[u8; 4], targets: &[&[u8; 4]]| links.get(view, tag, targets);
    // DATA is one byte in old plugins and a u16 in Skyrim SE.
    let data_flags = Some(sized(view, b"DATA", &[1, 2])).and_then(|bytes| match bytes {
        [low] => Some(u32::from(*low)),
        [low, high, ..] => Some(u32::from(u16::from_le_bytes([*low, *high]))),
        [] => None,
    });
    Row::default()
        .put("cell_id", cell_id)
        .put("data_flags", data_flags)
        .put("image_space_id", link(b"XCIM", &[b"IMGS"]))
        .put("acoustic_space_id", link(b"XCAS", &[b"ASPC"]))
        .put("music_id", link(b"XCMO", &[b"MUSC"]))
        .put("sky_region_id", link(b"XCCM", &[b"REGN"]))
        .put("location_id", link(b"XLCN", &[b"LCTN"]))
        .put("encounter_zone_id", link(b"XEZN", &[b"ECZN"]))
        .put("owner_id", link(b"XOWN", &[b"FACT", b"ACHR", b"NPC_"]))
        .put("owner_rank", i32_at(sized(view, b"XRNK", &[4]), 0))
        .put("water_id", link(b"XCWT", &[b"WATR"]))
        .real("water_height", f32_at(sized(view, b"XCLW", &[4]), 0))
        .insert(tx, "cell_environment")?;
    tx.prepare_cached("DELETE FROM cell_regions WHERE cell_id=?1")?
        .execute([cell_id])?;
    for region in view.find(b"XCLR").unwrap_or_default().as_chunks::<4>().0 {
        let region = u32::from_le_bytes(*region);
        if let Some(region) = links.id(Some(region), &[b"REGN"]) {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO cell_regions(cell_id, region_id) VALUES (?1, ?2)",
            )?
            .execute([cell_id, region])?;
        }
    }

    let authored_xcll = view.find(b"XCLL");
    let xcll = authored_xcll.filter(|bytes| bytes.len() >= XCLL_MIN);
    let template = link(b"LTMP", &[b"LGTM"]);
    if authored_xcll.is_none() && template.is_none() {
        tx.prepare_cached("DELETE FROM cell_lighting WHERE cell_id=?1")?
            .execute([cell_id])?;
        return Ok(());
    }
    let bytes = xcll.unwrap_or_default();
    Row::default()
        .put("cell_id", cell_id)
        .put("lighting_template_id", template)
        // Keep rejected block presence distinct from an LTMP-only cell. The runtime must
        // not turn malformed local lighting into implicit inheritance of every group.
        .put("xcll_size", authored_xcll.map(|bytes| bytes.len() as i64))
        .lighting(bytes)
        .put("inherit_flags", u32_at(bytes, 88))
        .insert(tx, "cell_lighting")
}

/// Writes the environment row of one winning `WRLD`.
pub(crate) fn insert_worldspace(
    tx: &Transaction<'_>,
    links: &Links<'_>,
    worldspace_id: u32,
    view: &SubrecordView<'_>,
) -> Result<()> {
    let link = |tag: &[u8; 4], targets: &[&[u8; 4]]| links.get(view, tag, targets);
    let center = Some(sized(view, b"WCTR", &[4])).filter(|bytes| !bytes.is_empty());
    Row::default()
        .put("worldspace_id", worldspace_id)
        .put("data_flags", sized(view, b"DATA", &[1]).first().copied())
        .put("parent_flags", sized(view, b"PNAM", &[2]).first().copied())
        .put("climate_id", link(b"CNAM", &[b"CLMT"]))
        .put("water_id", link(b"NAM2", &[b"WATR"]))
        .put("lod_water_id", link(b"NAM3", &[b"WATR"]))
        .real("lod_water_height", f32_at(sized(view, b"NAM4", &[4]), 0))
        .real("default_land_height", f32_at(sized(view, b"DNAM", &[8]), 0))
        .real(
            "default_water_height",
            f32_at(sized(view, b"DNAM", &[8]), 4),
        )
        .put("lighting_template_id", link(b"LTMP", &[b"LGTM"]))
        .put("music_id", link(b"ZNAM", &[b"MUSC"]))
        .put("location_id", link(b"XLCN", &[b"LCTN"]))
        .put("encounter_zone_id", link(b"XEZN", &[b"ECZN"]))
        .real("bounds_min_x", f32_at(sized(view, b"NAM0", &[8]), 0))
        .real("bounds_min_y", f32_at(sized(view, b"NAM0", &[8]), 4))
        .real("bounds_max_x", f32_at(sized(view, b"NAM9", &[8]), 0))
        .real("bounds_max_y", f32_at(sized(view, b"NAM9", &[8]), 4))
        .real("lod_multiplier", f32_at(sized(view, b"NAMA", &[4]), 0))
        .put(
            "center_cell_x",
            center.map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]])),
        )
        .put(
            "center_cell_y",
            center.map(|bytes| i16::from_le_bytes([bytes[2], bytes[3]])),
        )
        .insert(tx, "worldspace_environment")
}

/// Writes the rows of one winning `LGTM`, `WTHR`, `CLMT`, `REGN` or `IMGS`; other records are
/// ignored.
pub(crate) fn insert_record(
    tx: &Transaction<'_>,
    links: &Links<'_>,
    record: &RawRecord,
    view: &SubrecordView<'_>,
) -> Result<()> {
    let id = record.form_id;
    match &record.record_type {
        b"LGTM" => {
            let data =
                Some(sized(view, b"DATA", LGTM_DATA_SIZES)).filter(|bytes| !bytes.is_empty());
            Row::default()
                .put("id", id)
                .put("editor_id", view.get_string(b"EDID"))
                .put("data_size", data.map(|bytes| bytes.len() as i64))
                .lighting(data.unwrap_or_default())
                .dalc("subrecord_dalc_", sized(view, b"DALC", DALC_SIZES))
                .insert(tx, "lighting_templates")
        }
        b"WTHR" => insert_weather(tx, links, id, view),
        b"CLMT" => {
            let timing = sized(view, b"TNAM", &[6]);
            let mut row = Row::default();
            row.put("id", id)
                .put("editor_id", view.get_string(b"EDID"))
                .put("sun_texture", view.get_string(b"FNAM"))
                .put("sun_glare_texture", view.get_string(b"GNAM"));
            for (index, name) in [
                "sunrise_begin",
                "sunrise_end",
                "sunset_begin",
                "sunset_end",
                "volatility",
                "moon_phase",
            ]
            .iter()
            .enumerate()
            {
                row.put(*name, timing.get(index).map(|&value| i64::from(value)));
            }
            row.insert(tx, "climates")?;
            insert_weather_list(
                tx,
                links,
                "climate_weathers",
                "climate_id",
                id,
                [view.find(b"WLST").unwrap_or_default()],
                |entry| i32_at(entry, 4).map(i64::from),
            )
        }
        b"REGN" => {
            let lists = record
                .subrecords
                .iter()
                .filter(|(tag, _)| tag.as_slice() == b"RDWT")
                .map(|(_, data)| data.as_slice());
            insert_weather_list(
                tx,
                links,
                "region_weathers",
                "region_id",
                id,
                lists,
                |entry| u32_at(entry, 4).map(i64::from),
            )
        }
        b"IMGS" => {
            let hdr = sized(view, b"HNAM", &[36]);
            let cinematic = sized(view, b"CNAM", &[12]);
            let tint = sized(view, b"TNAM", &[16]);
            let dof = sized(view, b"DNAM", &[12, 16]);
            let mut row = Row::default();
            row.put("id", id).put("editor_id", view.get_string(b"EDID"));
            for (index, name) in [
                "eye_adaptation_speed",
                "bloom_blur_radius",
                "bloom_threshold",
                "bloom_scale",
                "receive_bloom_threshold",
                "white",
                "sunlight_scale",
                "sky_scale",
                "eye_adaptation_strength",
            ]
            .iter()
            .enumerate()
            {
                row.real(*name, f32_at(hdr, index * 4));
            }
            row.real("saturation", f32_at(cinematic, 0))
                .real("brightness", f32_at(cinematic, 4))
                .real("contrast", f32_at(cinematic, 8))
                .real("tint_amount", f32_at(tint, 0))
                .real("tint_r", f32_at(tint, 4))
                .real("tint_g", f32_at(tint, 8))
                .real("tint_b", f32_at(tint, 12))
                .real("dof_strength", f32_at(dof, 0))
                .real("dof_distance", f32_at(dof, 4))
                .real("dof_range", f32_at(dof, 8))
                .put(
                    "dof_sky_blur_radius",
                    dof.get(14..16)
                        .map(|bytes| i64::from(u16::from_le_bytes([bytes[0], bytes[1]]))),
                )
                .insert(tx, "image_spaces")
        }
        _ => Ok(()),
    }
}

fn insert_weather(
    tx: &Transaction<'_>,
    links: &Links<'_>,
    id: u32,
    view: &SubrecordView<'_>,
) -> Result<()> {
    let fog = sized(view, b"FNAM", &[32]);
    let data = sized(view, b"DATA", &[19]);
    let mut row = Row::default();
    row.put("id", id).put("editor_id", view.get_string(b"EDID"));
    for (index, name) in [
        "fog_day_near",
        "fog_day_far",
        "fog_night_near",
        "fog_night_far",
        "fog_day_power",
        "fog_night_power",
        "fog_day_max",
        "fog_night_max",
    ]
    .iter()
    .enumerate()
    {
        row.real(*name, f32_at(fog, index * 4));
    }
    for (name, offset) in WEATHER_DATA {
        row.put(name, data.get(offset).map(|&value| i64::from(value)));
    }
    row.put(
        "disabled_cloud_layers",
        u32_at(sized(view, b"NAM1", &[4]), 0),
    )
    .put("precipitation_id", links.get(view, b"MNAM", &[b"SPGD"]))
    .put("visual_effect_id", links.get(view, b"NNAM", &[b"RFCT"]))
    .put("sky_static_id", links.get(view, b"TNAM", &[b"STAT"]))
    .put("sun_glare_lens_id", links.get(view, b"GNAM", &[b"LENS"]));
    for (slots, prefix, target) in [
        (sized(view, b"IMSP", &[16]), "image_space", b"IMGS"),
        (sized(view, b"HNAM", &[16]), "volumetric_lighting", b"VOLI"),
    ] {
        for (index, time) in TIMES_OF_DAY.iter().enumerate() {
            row.put(
                format!("{prefix}_{time}_id"),
                links.id(u32_at(slots, index * 4), &[target]),
            );
        }
    }
    row.insert(tx, "weathers")?;

    tx.prepare_cached("DELETE FROM weather_colors WHERE weather_id=?1")?
        .execute([id])?;
    for (colour_type, colours) in WEATHER_COLOUR_TYPES
        .iter()
        .zip(sized(view, b"NAM0", NAM0_SIZES).as_chunks::<16>().0)
    {
        for (time, rgb) in TIMES_OF_DAY.iter().zip(colours.as_chunks::<4>().0) {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO weather_colors(weather_id, color_type, time_of_day, r, g, b) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(rusqlite::params![id, colour_type, time, rgb[0], rgb[1], rgb[2]])?;
        }
    }

    // Four `DALC` subrecords, one per time of day, in file order.
    tx.prepare_cached("DELETE FROM weather_directional_ambient WHERE weather_id=?1")?
        .execute([id])?;
    for (time, dalc) in TIMES_OF_DAY.iter().zip(view.find_all(b"DALC")) {
        if !DALC_SIZES.contains(&dalc.len()) {
            continue;
        }
        Row::default()
            .put("weather_id", id)
            .put("time_of_day", (*time).to_owned())
            .dalc("", dalc)
            .insert(tx, "weather_directional_ambient")?;
    }
    Ok(())
}

/// Rows of `(weather, chance, global)` 12-byte entries from a `CLMT` `WLST` or `REGN` `RDWT`,
/// numbered across all of the record's lists in file order.
fn insert_weather_list<'a>(
    tx: &Transaction<'_>,
    links: &Links<'_>,
    table: &str,
    owner: &str,
    id: u32,
    lists: impl IntoIterator<Item = &'a [u8]>,
    chance: impl Fn(&[u8]) -> Option<i64>,
) -> Result<()> {
    tx.prepare_cached(&format!("DELETE FROM {table} WHERE {owner}=?1"))?
        .execute([id])?;
    let entries = lists.into_iter().flat_map(|list| list.as_chunks::<12>().0);
    for (position, entry) in entries.enumerate() {
        Row::default()
            .put(owner, id)
            .put("position", position as i64)
            .put("weather_id", links.id(u32_at(entry, 0), &[b"WTHR"]))
            .put("chance", chance(entry))
            .put("chance_global_id", links.id(u32_at(entry, 8), &[b"GLOB"]))
            .insert(tx, table)?;
    }
    Ok(())
}
