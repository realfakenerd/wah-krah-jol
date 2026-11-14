//! Authored lighting, weather, climate and image space from the world database (#269 gaps 1, 2).
//!
//! The converter publishes the environment tables documented in db-schema.md ("Cell and
//! Worldspace Environment", "Weather, Climate, Image Space and Lighting Templates"). They are
//! optional: a database converted before them, or with a table of another shape, loads an empty
//! map for that table and the engine keeps its built-in lighting. Nothing renders from this
//! catalog yet; it is the reader later lighting and sky work consumes (#264).
//! Interior runtime consumption now lives in [`interior`]; exterior weather selection remains open.

pub(crate) mod interior;

use bevy::prelude::*;
use rusqlite::{Connection, OpenFlags, Row};
use std::{collections::HashMap, path::Path};

/// The four times of day Skyrim keys weather colours, image spaces and directional ambient by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeOfDay {
    Sunrise,
    Day,
    Sunset,
    Night,
}

impl TimeOfDay {
    pub const ALL: [Self; 4] = [Self::Sunrise, Self::Day, Self::Sunset, Self::Night];

    fn parse(name: &str) -> Option<Self> {
        match name {
            "sunrise" => Some(Self::Sunrise),
            "day" => Some(Self::Day),
            "sunset" => Some(Self::Sunset),
            "night" => Some(Self::Night),
            _ => None,
        }
    }
}

/// A directional ambient block (`DALC`): colours along X+, X-, Y+, Y-, Z+, and Z-.
/// Specular colour and scale are optional. Colours are 8-bit sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct DirectionalAmbient {
    pub colors: [[u8; 3]; 6],
    pub specular: Option<[u8; 3]>,
    pub scale: Option<f32>,
}

/// The lighting fields an `XCLL` and an `LGTM` `DATA` share. A field is `None` when the block is
/// absent or too short to hold it. Colours are 8-bit sRGB; rotations are degrees, as authored.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct Lighting {
    pub ambient: Option<[u8; 3]>,
    pub directional: Option<[u8; 3]>,
    pub fog_near_color: Option<[u8; 3]>,
    pub fog_near: Option<f32>,
    pub fog_far: Option<f32>,
    pub directional_rotation_xy: Option<i32>,
    pub directional_rotation_z: Option<i32>,
    pub directional_fade: Option<f32>,
    pub fog_clip_distance: Option<f32>,
    pub fog_power: Option<f32>,
    pub directional_ambient: Option<DirectionalAmbient>,
    pub fog_far_color: Option<[u8; 3]>,
    pub fog_max: Option<f32>,
    pub light_fade_begin: Option<f32>,
    pub light_fade_end: Option<f32>,
}

/// A cell's `XCLL` lighting and `LTMP` template.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CellLighting {
    /// Authored XCLL length, including rejected blocks under 72 bytes. None means absent.
    pub xcll_size: Option<u32>,
    pub lighting_template: Option<u32>,
    pub lighting: Lighting,
    /// Which fields come from the lighting template instead (db-schema.md lists the bits).
    pub inherit_flags: Option<u32>,
}

/// A lighting template (`LGTM`): its `DATA` lighting and its separate `DALC` subrecord.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LightingTemplate {
    pub lighting: Lighting,
    pub directional_ambient: Option<DirectionalAmbient>,
}

/// A worldspace's climate, lighting template, music and flags.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WorldspaceEnvironment {
    pub data_flags: Option<u8>,
    pub parent_flags: Option<u8>,
    pub climate: Option<u32>,
    pub water: Option<u32>,
    pub lighting_template: Option<u32>,
    pub music: Option<u32>,
}

/// A weather's fog distances (`FNAM`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WeatherFog {
    pub day_near: Option<f32>,
    pub day_far: Option<f32>,
    pub night_near: Option<f32>,
    pub night_far: Option<f32>,
    pub day_power: Option<f32>,
    pub night_power: Option<f32>,
    pub day_max: Option<f32>,
    pub night_max: Option<f32>,
}

/// A weather (`WTHR`): fog, image spaces, `NAM0` colours and directional ambient by time of day.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Weather {
    pub editor_id: Option<String>,
    pub fog: WeatherFog,
    /// `IMSP`, indexed like [`TimeOfDay::ALL`].
    pub image_spaces: [Option<u32>; 4],
    /// `NAM0` colours by colour type (`sky_upper`, `fog_near`, `ambient`, `sunlight`, ...).
    pub colors: HashMap<(String, TimeOfDay), [u8; 3]>,
    pub directional_ambient: HashMap<TimeOfDay, DirectionalAmbient>,
}

impl Weather {
    pub fn color(&self, color_type: &str, time: TimeOfDay) -> Option<[u8; 3]> {
        self.colors.get(&(color_type.to_owned(), time)).copied()
    }
}

/// An image space (`IMGS`): HDR, cinematic and tint values.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ImageSpace {
    pub eye_adaptation_speed: Option<f32>,
    pub bloom_blur_radius: Option<f32>,
    pub bloom_threshold: Option<f32>,
    pub bloom_scale: Option<f32>,
    pub receive_bloom_threshold: Option<f32>,
    pub white: Option<f32>,
    pub sunlight_scale: Option<f32>,
    pub sky_scale: Option<f32>,
    pub eye_adaptation_strength: Option<f32>,
    pub saturation: Option<f32>,
    pub brightness: Option<f32>,
    pub contrast: Option<f32>,
    pub tint_amount: Option<f32>,
    pub tint: Option<[f32; 3]>,
}

/// One entry of a climate's weather list (`WLST`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateWeather {
    pub weather: Option<u32>,
    pub chance: i32,
    pub chance_global: Option<u32>,
}

#[derive(Resource, Debug, Default)]
pub struct EnvironmentCatalog {
    cells: HashMap<u32, CellLighting>,
    cell_flags: HashMap<u32, Option<u32>>,
    worldspaces: HashMap<u32, WorldspaceEnvironment>,
    templates: HashMap<u32, LightingTemplate>,
    weathers: HashMap<u32, Weather>,
    image_spaces: HashMap<u32, ImageSpace>,
    climate_weathers: HashMap<u32, Vec<ClimateWeather>>,
}

/// A directional ambient block's 22 columns after `prefix`.
fn dalc_columns(prefix: &str) -> String {
    let mut columns: Vec<String> = [
        "x_pos", "x_neg", "y_pos", "y_neg", "z_pos", "z_neg", "specular",
    ]
    .iter()
    .flat_map(|colour| ["r", "g", "b"].map(|channel| format!("{prefix}{colour}_{channel}")))
    .collect();
    columns.push(format!("{prefix}scale"));
    columns.join(",")
}

/// The 44 columns `cell_lighting` and `lighting_templates` share, in [`lighting`]'s order.
fn lighting_columns() -> String {
    format!(
        "ambient_r,ambient_g,ambient_b,directional_r,directional_g,directional_b,\
         fog_near_r,fog_near_g,fog_near_b,fog_near,fog_far,\
         directional_rotation_xy,directional_rotation_z,directional_fade,fog_clip_distance,fog_power,\
         {},fog_far_r,fog_far_g,fog_far_b,fog_max,light_fade_begin,light_fade_end",
        dalc_columns("dalc_")
    )
}

const LIGHTING_WIDTH: usize = 44;
const DALC_WIDTH: usize = 22;

fn rgb(row: &Row<'_>, first: usize) -> rusqlite::Result<Option<[u8; 3]>> {
    Ok(
        match (
            row.get::<_, Option<u8>>(first)?,
            row.get::<_, Option<u8>>(first + 1)?,
            row.get::<_, Option<u8>>(first + 2)?,
        ) {
            (Some(r), Some(g), Some(b)) => Some([r, g, b]),
            _ => None,
        },
    )
}

/// A directional ambient block, or `None` unless all six directional colours are present.
fn dalc(row: &Row<'_>, first: usize) -> rusqlite::Result<Option<DirectionalAmbient>> {
    let mut colors = [[0; 3]; 6];
    for (index, colour) in colors.iter_mut().enumerate() {
        let Some(value) = rgb(row, first + index * 3)? else {
            return Ok(None);
        };
        *colour = value;
    }
    Ok(Some(DirectionalAmbient {
        colors,
        specular: rgb(row, first + 18)?,
        scale: row.get(first + 21)?,
    }))
}

fn lighting(row: &Row<'_>, first: usize) -> rusqlite::Result<Lighting> {
    let at = |offset: usize| first + offset;
    Ok(Lighting {
        ambient: rgb(row, at(0))?,
        directional: rgb(row, at(3))?,
        fog_near_color: rgb(row, at(6))?,
        fog_near: row.get(at(9))?,
        fog_far: row.get(at(10))?,
        directional_rotation_xy: row.get(at(11))?,
        directional_rotation_z: row.get(at(12))?,
        directional_fade: row.get(at(13))?,
        fog_clip_distance: row.get(at(14))?,
        fog_power: row.get(at(15))?,
        directional_ambient: dalc(row, at(16))?,
        fog_far_color: rgb(row, at(16 + DALC_WIDTH))?,
        fog_max: row.get(at(19 + DALC_WIDTH))?,
        light_fade_begin: row.get(at(20 + DALC_WIDTH))?,
        light_fade_end: row.get(at(21 + DALC_WIDTH))?,
    })
}

impl EnvironmentCatalog {
    /// Reads every environment table. A missing table, or one without every column read here,
    /// gives an empty map with one log line, never an error: the environment is optional.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self::from_connection(&connection))
    }

    pub fn from_connection(connection: &Connection) -> Self {
        let cells = read(
            connection,
            &format!(
                "SELECT cell_id, lighting_template_id, {}, inherit_flags, xcll_size FROM cell_lighting",
                lighting_columns()
            ),
            "cell_lighting",
            |row| {
                Ok((
                    row.get(0)?,
                    CellLighting {
                        xcll_size: row.get(3 + LIGHTING_WIDTH)?,
                        lighting_template: row.get(1)?,
                        lighting: lighting(row, 2)?,
                        inherit_flags: row.get(2 + LIGHTING_WIDTH)?,
                    },
                ))
            },
        );
        let cell_flags = read(
            connection,
            "SELECT cell_id, data_flags FROM cell_environment",
            "cell_environment",
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        let worldspaces = read(
            connection,
            "SELECT worldspace_id, data_flags, parent_flags, climate_id, water_id, \
             lighting_template_id, music_id FROM worldspace_environment",
            "worldspace_environment",
            |row| {
                Ok((
                    row.get(0)?,
                    WorldspaceEnvironment {
                        data_flags: row.get(1)?,
                        parent_flags: row.get(2)?,
                        climate: row.get(3)?,
                        water: row.get(4)?,
                        lighting_template: row.get(5)?,
                        music: row.get(6)?,
                    },
                ))
            },
        );
        let templates = read(
            connection,
            &format!(
                "SELECT id, {}, {} FROM lighting_templates",
                lighting_columns(),
                dalc_columns("subrecord_dalc_")
            ),
            "lighting_templates",
            |row| {
                Ok((
                    row.get(0)?,
                    LightingTemplate {
                        lighting: lighting(row, 1)?,
                        directional_ambient: dalc(row, 1 + LIGHTING_WIDTH)?,
                    },
                ))
            },
        );
        let image_spaces = read(
            connection,
            "SELECT id, eye_adaptation_speed, bloom_blur_radius, bloom_threshold, bloom_scale, \
             receive_bloom_threshold, white, sunlight_scale, sky_scale, eye_adaptation_strength, \
             saturation, brightness, contrast, tint_amount, tint_r, tint_g, tint_b FROM image_spaces",
            "image_spaces",
            |row| {
                let tint = match (row.get(14)?, row.get(15)?, row.get(16)?) {
                    (Some(r), Some(g), Some(b)) => Some([r, g, b]),
                    _ => None,
                };
                Ok((
                    row.get(0)?,
                    ImageSpace {
                        eye_adaptation_speed: row.get(1)?,
                        bloom_blur_radius: row.get(2)?,
                        bloom_threshold: row.get(3)?,
                        bloom_scale: row.get(4)?,
                        receive_bloom_threshold: row.get(5)?,
                        white: row.get(6)?,
                        sunlight_scale: row.get(7)?,
                        sky_scale: row.get(8)?,
                        eye_adaptation_strength: row.get(9)?,
                        saturation: row.get(10)?,
                        brightness: row.get(11)?,
                        contrast: row.get(12)?,
                        tint_amount: row.get(13)?,
                        tint,
                    },
                ))
            },
        );
        let weathers = read_weathers(connection);
        let mut climate_weathers: HashMap<u32, Vec<ClimateWeather>> = HashMap::new();
        for (climate, entry) in read_rows(
            connection,
            "SELECT climate_id, weather_id, chance, chance_global_id FROM climate_weathers \
             ORDER BY climate_id, position",
            "climate_weathers",
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    ClimateWeather {
                        weather: row.get(1)?,
                        chance: row.get(2)?,
                        chance_global: row.get(3)?,
                    },
                ))
            },
        ) {
            climate_weathers.entry(climate).or_default().push(entry);
        }
        info!(
            "environment: {} cells with authored lighting, {} worldspaces, {} lighting templates, \
             {} weathers, {} image spaces, {} climates",
            cells.len(),
            worldspaces.len(),
            templates.len(),
            weathers.len(),
            image_spaces.len(),
            climate_weathers.len()
        );
        Self {
            cells,
            cell_flags,
            worldspaces,
            templates,
            weathers,
            image_spaces,
            climate_weathers,
        }
    }

    /// CELL DATA flags, including independent Show Sky and Use Sky Lighting bits.
    pub fn cell_flags(&self, cell_id: u32) -> Option<u32> {
        self.cell_flags.get(&cell_id).copied().flatten()
    }

    /// The cell's authored lighting, or `None` when it has none or the database predates it.
    pub fn cell_lighting(&self, cell_id: u32) -> Option<&CellLighting> {
        self.cells.get(&cell_id)
    }

    pub fn worldspace(&self, worldspace_id: u32) -> Option<&WorldspaceEnvironment> {
        self.worldspaces.get(&worldspace_id)
    }

    pub fn lighting_template(&self, id: u32) -> Option<&LightingTemplate> {
        self.templates.get(&id)
    }

    pub fn weather(&self, id: u32) -> Option<&Weather> {
        self.weathers.get(&id)
    }

    pub fn image_space(&self, id: u32) -> Option<&ImageSpace> {
        self.image_spaces.get(&id)
    }

    /// A climate's weather list in authored order; empty when unknown.
    pub fn climate_weathers(&self, climate_id: u32) -> &[ClimateWeather] {
        self.climate_weathers
            .get(&climate_id)
            .map_or(&[], Vec::as_slice)
    }
}

fn read_weathers(connection: &Connection) -> HashMap<u32, Weather> {
    let mut weathers = read(
        connection,
        "SELECT id, editor_id, fog_day_near, fog_day_far, fog_night_near, fog_night_far, \
         fog_day_power, fog_night_power, fog_day_max, fog_night_max, image_space_sunrise_id, \
         image_space_day_id, image_space_sunset_id, image_space_night_id FROM weathers",
        "weathers",
        |row| {
            Ok((
                row.get(0)?,
                Weather {
                    editor_id: row.get(1)?,
                    fog: WeatherFog {
                        day_near: row.get(2)?,
                        day_far: row.get(3)?,
                        night_near: row.get(4)?,
                        night_far: row.get(5)?,
                        day_power: row.get(6)?,
                        night_power: row.get(7)?,
                        day_max: row.get(8)?,
                        night_max: row.get(9)?,
                    },
                    image_spaces: [row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?],
                    ..Default::default()
                },
            ))
        },
    );
    for (weather, color_type, time, color) in read_rows(
        connection,
        "SELECT weather_id, color_type, time_of_day, r, g, b FROM weather_colors",
        "weather_colors",
        |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                [row.get(3)?, row.get(4)?, row.get(5)?],
            ))
        },
    ) {
        if let (Some(entry), Some(time)) = (weathers.get_mut(&weather), TimeOfDay::parse(&time)) {
            entry.colors.insert((color_type, time), color);
        }
    }
    for (weather, time, block) in read_rows(
        connection,
        &format!(
            "SELECT weather_id, time_of_day, {} FROM weather_directional_ambient",
            dalc_columns("")
        ),
        "weather_directional_ambient",
        |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                dalc(row, 2)?,
            ))
        },
    ) {
        if let (Some(entry), Some(time), Some(block)) =
            (weathers.get_mut(&weather), TimeOfDay::parse(&time), block)
        {
            entry.directional_ambient.insert(time, block);
        }
    }
    weathers
}

fn read<T>(
    connection: &Connection,
    sql: &str,
    table: &str,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<(u32, T)>,
) -> HashMap<u32, T> {
    read_rows(connection, sql, table, map).into_iter().collect()
}

fn read_rows<T>(
    connection: &Connection,
    sql: &str,
    table: &str,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Vec<T> {
    let rows = connection.prepare(sql).and_then(|mut statement| {
        statement
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>()
    });
    rows.unwrap_or_else(|error| {
        info!("environment: no usable `{table}` table ({error}); authored fields unavailable");
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_without_the_tables_gives_an_empty_catalog() {
        let connection = Connection::open_in_memory().unwrap();
        let catalog = EnvironmentCatalog::from_connection(&connection);
        assert!(catalog.cell_lighting(1).is_none());
        assert!(catalog.worldspace(1).is_none());
        assert!(catalog.weather(1).is_none());
        assert!(catalog.climate_weathers(1).is_empty());
    }

    #[test]
    fn a_table_of_another_shape_gives_an_empty_catalog() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cell_lighting(cell_id INTEGER PRIMARY KEY, ambient_r INTEGER);
                 INSERT INTO cell_lighting VALUES(1, 2);
                 CREATE TABLE worldspace_environment(worldspace_id INTEGER PRIMARY KEY);
                 INSERT INTO worldspace_environment VALUES(60);
                 CREATE TABLE weathers(id INTEGER PRIMARY KEY);
                 INSERT INTO weathers VALUES(7);",
            )
            .unwrap();
        let catalog = EnvironmentCatalog::from_connection(&connection);
        assert!(catalog.cell_lighting(1).is_none());
        assert!(catalog.worldspace(60).is_none());
        assert!(catalog.weather(7).is_none());
    }

    #[test]
    fn optional_dalc_fields_survive_weather_template_and_cell_reads() {
        let connection = Connection::open_in_memory().unwrap();
        converter::esm::exporter::create_tables(&connection).unwrap();

        let cases = [
            ("24-byte", 18, true),
            ("28-byte", 21, true),
            ("32-byte", 22, true),
            ("incomplete-colors", 17, false),
        ];
        for (index, (name, value_count, _)) in cases.into_iter().enumerate() {
            let cell_id = 100 + index as u32;
            let template_id = 200 + index as u32;
            let weather_id = 300 + index as u32;
            connection
                .execute("INSERT INTO cell_lighting(cell_id) VALUES (?1)", [cell_id])
                .unwrap();
            connection
                .execute(
                    "INSERT INTO lighting_templates(id) VALUES (?1)",
                    [template_id],
                )
                .unwrap();
            connection
                .execute("INSERT INTO weathers(id) VALUES (?1)", [weather_id])
                .unwrap();
            connection
                .execute(
                    "INSERT INTO weather_directional_ambient(weather_id, time_of_day) VALUES (?1, 'day')",
                    [weather_id],
                )
                .unwrap();

            let seed = 40 + index as u8 * 40;
            for (table, key, prefix) in [
                ("cell_lighting", cell_id, "dalc_"),
                ("lighting_templates", template_id, "subrecord_dalc_"),
                ("weather_directional_ambient", weather_id, ""),
            ] {
                let columns: Vec<_> = dalc_columns(prefix).split(',').map(str::to_owned).collect();
                let updates = columns
                    .iter()
                    .enumerate()
                    .map(|(column_index, column)| {
                        if column_index < value_count {
                            let value = if column_index == 21 {
                                format!("{}", 2.5 + index as f32)
                            } else {
                                (seed + column_index as u8).to_string()
                            };
                            format!("{column}={value}")
                        } else {
                            format!("{column}=NULL")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let key_column = match table {
                    "cell_lighting" => "cell_id",
                    "lighting_templates" => "id",
                    _ => "weather_id",
                };
                connection
                    .execute(
                        &format!("UPDATE {table} SET {updates} WHERE {key_column}=?1"),
                        [key],
                    )
                    .unwrap_or_else(|error| panic!("{name} {table}: {error}"));
            }
        }

        let catalog = EnvironmentCatalog::from_connection(&connection);
        for (index, (_, _, complete)) in cases.into_iter().enumerate() {
            let seed = 40 + index as u8 * 40;
            let expected = complete.then(|| DirectionalAmbient {
                colors: std::array::from_fn(|color| {
                    std::array::from_fn(|channel| seed + (color * 3 + channel) as u8)
                }),
                specular: (cases[index].1 >= 21)
                    .then(|| std::array::from_fn(|channel| seed + (18 + channel) as u8)),
                scale: (cases[index].1 == 22).then_some(2.5 + index as f32),
            });
            assert_eq!(
                catalog
                    .cell_lighting(100 + index as u32)
                    .unwrap()
                    .lighting
                    .directional_ambient,
                expected,
                "cell lighting case {}",
                cases[index].0
            );
            assert_eq!(
                catalog
                    .lighting_template(200 + index as u32)
                    .unwrap()
                    .directional_ambient,
                expected,
                "lighting template case {}",
                cases[index].0
            );
            assert_eq!(
                catalog
                    .weather(300 + index as u32)
                    .unwrap()
                    .directional_ambient
                    .get(&TimeOfDay::Day)
                    .copied(),
                expected,
                "weather case {}",
                cases[index].0
            );
        }
    }

    /// The tables exactly as the converter creates them, read back through the catalog.
    #[test]
    fn reads_the_converter_tables() {
        let connection = Connection::open_in_memory().unwrap();
        converter::esm::exporter::create_tables(&connection).unwrap();
        let dalc = |prefix: &str| {
            dalc_columns(prefix)
                .split(',')
                .enumerate()
                .map(|(index, column)| format!("{column}={}", index + 1))
                .collect::<Vec<_>>()
                .join(",")
        };
        connection
            .execute_batch(&format!(
                "INSERT INTO cell_lighting(cell_id, lighting_template_id, ambient_r, ambient_g, ambient_b,
                     fog_near, inherit_flags, dalc_x_pos_r, dalc_x_pos_g, dalc_x_pos_b)
                 VALUES(0x1000, 0x2000, 10, 20, 30, 64.5, 1024, 1, 2, 3);
                 INSERT INTO worldspace_environment(worldspace_id, data_flags, climate_id, music_id)
                 VALUES(60, 32, 0x3000, 0x4000);
                 INSERT INTO lighting_templates(id, editor_id, fog_far) VALUES(0x2000, 'Template', 9000);
                 UPDATE lighting_templates SET {} WHERE id=0x2000;
                 INSERT INTO weathers(id, editor_id, fog_day_near, fog_day_far, image_space_day_id)
                 VALUES(0x81A, 'SkyrimClear', 1024, 200000, 0x5000);
                 INSERT INTO weather_colors VALUES(0x81A, 'sunlight', 'day', 255, 240, 200);
                 INSERT INTO weather_directional_ambient(weather_id, time_of_day) VALUES(0x81A, 'day');
                 UPDATE weather_directional_ambient SET {} WHERE weather_id=0x81A;
                 INSERT INTO image_spaces(id, saturation, tint_amount, tint_r, tint_g, tint_b)
                 VALUES(0x5000, 1.5, 0.25, 0.5, 0.75, 1.0);
                 INSERT INTO climate_weathers VALUES(0x3000, 1, 0x81B, 20, NULL);
                 INSERT INTO climate_weathers VALUES(0x3000, 0, 0x81A, 80, 0x6000);",
                dalc("subrecord_dalc_"),
                dalc("")
            ))
            .unwrap();
        let catalog = EnvironmentCatalog::from_connection(&connection);

        let cell = catalog.cell_lighting(0x1000).unwrap();
        assert_eq!(cell.lighting_template, Some(0x2000));
        assert_eq!(cell.lighting.ambient, Some([10, 20, 30]));
        assert_eq!(cell.lighting.fog_near, Some(64.5));
        assert_eq!(cell.inherit_flags, Some(1024));
        assert_eq!(cell.lighting.directional, None);
        assert_eq!(
            cell.lighting.directional_ambient, None,
            "one direction alone is not a full block"
        );

        let world = catalog.worldspace(60).unwrap();
        assert_eq!(world.data_flags, Some(32));
        assert_eq!(world.climate, Some(0x3000));
        assert_eq!(world.music, Some(0x4000));

        let template = catalog.lighting_template(0x2000).unwrap();
        assert_eq!(template.lighting.fog_far, Some(9000.0));
        let block = template.directional_ambient.unwrap();
        assert_eq!(block.colors[0], [1, 2, 3]);
        assert_eq!(block.specular, Some([19, 20, 21]));
        assert_eq!(block.scale, Some(22.0));

        let weather = catalog.weather(0x81A).unwrap();
        assert_eq!(weather.editor_id.as_deref(), Some("SkyrimClear"));
        assert_eq!(weather.fog.day_far, Some(200_000.0));
        assert_eq!(weather.image_spaces[1], Some(0x5000));
        assert_eq!(
            weather.color("sunlight", TimeOfDay::Day),
            Some([255, 240, 200])
        );
        assert_eq!(weather.color("sunlight", TimeOfDay::Night), None);
        assert_eq!(
            weather.directional_ambient[&TimeOfDay::Day].colors[5],
            [16, 17, 18]
        );

        let image_space = catalog.image_space(0x5000).unwrap();
        assert_eq!(image_space.saturation, Some(1.5));
        assert_eq!(image_space.tint, Some([0.5, 0.75, 1.0]));

        let climate = catalog.climate_weathers(0x3000);
        assert_eq!(climate.len(), 2);
        assert_eq!(climate[0].weather, Some(0x81A), "authored order");
        assert_eq!(climate[0].chance_global, Some(0x6000));
        assert_eq!(climate[1].chance, 20);
    }
}
