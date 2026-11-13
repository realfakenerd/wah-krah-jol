//! Skyrim INI settings the engine honours.
//!
//! Skyrim reads its view distances from `Skyrim.ini` and `SkyrimPrefs.ini`, and
//! mod managers, LOD generators and players tune the same keys. `--ini <path>`
//! reads files in that format so a load order's tuned values carry over: files
//! apply in the order given, later files override earlier ones, and explicit
//! command-line options override every file. Keys without an engine consumer
//! are ignored, as Skyrim ignores keys it does not know.
//!
//! Honoured keys:
//!
//! | Section | Key | Engine setting |
//! |---|---|---|
//! | `[Sky]` | `fSunXExtreme`, `fSunYExtreme`, `fSunZExtreme`, `fSunDirXExtreme`, `fSunAlphaTransTime` | explicit sun-path overrides over GMST/fallback values |
//! | `[General]` | `uGridsToLoad` | full-detail grid: `stream_radius = (uGridsToLoad - 1) / 2` |
//! | `[Display]` | `fShadowDistance` | upper bound for sun shadow cascades, in Creation units |
//! | `[TerrainManager]` | `fBlockLevel0Distance` | level-4 terrain LOD distance, before the multiplier |
//! | `[TerrainManager]` | `fBlockLevel1Distance` | level-8 terrain LOD distance, before the multiplier |
//! | `[TerrainManager]` | `fBlockMaximumDistance` | level-16 terrain LOD distance, before the multiplier |
//! | `[TerrainManager]` | `fSplitDistanceMult` | terrain LOD multiplier on the three block distances |

use crate::config::{EngineConfig, MAX_STREAM_RADIUS};
use std::{collections::HashMap, path::Path};

/// The first cascade reaches 10 metres (700 Creation units); a smaller maximum would make the
/// cascade builder's bounds invalid.
const MIN_SUN_SHADOW_DISTANCE: f32 = 700.0;

/// Values from one or more INI files, keyed by lower-cased section and key.
#[derive(Debug, Default)]
pub struct SkyrimIni {
    values: HashMap<(String, String), String>,
}

impl SkyrimIni {
    /// Reads `path` over the values already read, as Skyrim layers its INI files.
    pub fn merge_file(&mut self, path: &Path) -> std::io::Result<()> {
        self.merge_str(&std::fs::read_to_string(path)?);
        Ok(())
    }

    /// Reads INI text over the values already read. Sections and keys are
    /// case-insensitive; `;` and `#` start a comment line; a key outside any
    /// section and a line without `=` are skipped.
    pub fn merge_str(&mut self, text: &str) {
        let mut section = None;
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = Some(name.trim().to_ascii_lowercase());
                continue;
            }
            let (Some(section), Some((key, value))) = (&section, line.split_once('=')) else {
                continue;
            };
            self.values.insert(
                (section.clone(), key.trim().to_ascii_lowercase()),
                value.trim().to_owned(),
            );
        }
    }

    fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.values
            .get(&(section.to_ascii_lowercase(), key.to_ascii_lowercase()))
            .map(String::as_str)
    }

    /// Applies every honoured key to `config`. A malformed value is reported and
    /// leaves the setting as it was.
    pub fn apply(&self, config: &mut EngineConfig) {
        if let Some(raw) = self.get("General", "uGridsToLoad") {
            match raw.parse::<i32>() {
                // Skyrim centres the grid on the player's cell, so only odd sizes are a grid.
                Ok(grids)
                    if grids >= 1 && grids % 2 == 1 && (grids - 1) / 2 <= MAX_STREAM_RADIUS =>
                {
                    config.stream_radius = (grids - 1) / 2;
                    config.unload_radius = config.stream_radius + 1;
                    config.settings_overrides.push("ini:stream-radius".into());
                }
                _ => eprintln!(
                    "warning: ignoring uGridsToLoad={raw:?}; expected an odd whole number from 1 to {}",
                    MAX_STREAM_RADIUS * 2 + 1
                ),
            }
        }
        if let Some(raw) = self.get("Display", "fShadowDistance") {
            match raw.parse::<f32>() {
                Ok(value) if value.is_finite() && value >= MIN_SUN_SHADOW_DISTANCE => {
                    config.sun_shadow_distance_limit = Some(value);
                }
                _ => eprintln!(
                    "warning: ignoring fShadowDistance={raw:?}; expected at least {MIN_SUN_SHADOW_DISTANCE} Creation units"
                ),
            }
        }
        for (index, key) in crate::sun::SUN_KEYS.into_iter().enumerate() {
            let Some(raw) = self.get("Sky", key) else {
                continue;
            };
            match raw.parse::<f32>() {
                Ok(value) if crate::sun::SunSettings::valid(index, value) => {
                    config.sun_settings_overrides[index] = Some(value)
                }
                _ => eprintln!(
                    "warning: ignoring [Sky] {key}={raw:?}; unusable sun-path value, retaining GMST/fallback"
                ),
            }
        }
        let lod = &mut config.terrain_lod;
        for (key, setting) in [
            ("fBlockLevel0Distance", &mut lod.block_level0_distance),
            ("fBlockLevel1Distance", &mut lod.block_level1_distance),
            ("fBlockMaximumDistance", &mut lod.block_maximum_distance),
            ("fSplitDistanceMult", &mut lod.split_distance_mult),
        ] {
            let Some(raw) = self.get("TerrainManager", key) else {
                continue;
            };
            match raw.parse::<f32>() {
                Ok(value) if value.is_finite() && value > 0.0 => {
                    *setting = value;
                    config.settings_overrides.push(format!("ini:{key}"));
                }
                _ => eprintln!("warning: ignoring {key}={raw:?}; expected a positive number"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::lod::LodTier;

    fn apply(text: &str) -> EngineConfig {
        let mut ini = SkyrimIni::default();
        ini.merge_str(text);
        let mut config = EngineConfig::default();
        ini.apply(&mut config);
        config
    }

    /// Explicit values layer by key; bad values cannot poison authored settings.
    #[test]
    fn sun_ini_overrides_validate_and_layer_without_aborting() {
        let mut ini = SkyrimIni::default();
        ini.merge_str("[sKy]\nfSunXExtreme=-300\nfSunYExtreme=-70\nfSunZExtreme=-120\nfSunDirXExtreme=0\nfSunAlphaTransTime=1.5\n");
        ini.merge_str("[Sky]\nfSunYExtreme=80\n");
        let mut config = EngineConfig::default();
        ini.apply(&mut config);
        assert_eq!(
            config.sun_settings_overrides,
            [Some(-300.0), Some(80.0), Some(-120.0), Some(0.0), Some(1.5)]
        );
        for (key, value) in [
            ("fSunXExtreme", "0"),
            ("fSunYExtreme", "NaN"),
            ("fSunZExtreme", "inf"),
            ("fSunDirXExtreme", "bad"),
            ("fSunAlphaTransTime", "24"),
        ] {
            let config = apply(&format!("[Sky]\n{key}={value}\n"));
            assert_eq!(config.sun_settings_overrides, [None; 5]);
        }
    }

    #[test]
    fn ini_v88_preserves_the_cli_stream_radius_bound() {
        let widest = apply(&format!(
            "[General]\nuGridsToLoad={}\n",
            MAX_STREAM_RADIUS * 2 + 1
        ));
        assert_eq!(widest.stream_radius, MAX_STREAM_RADIUS);
        let overflowing = apply("[General]\nuGridsToLoad=2147483647\n");
        assert_eq!(
            overflowing.stream_radius,
            EngineConfig::default().stream_radius
        );
    }

    #[test]
    fn reads_skyrim_prefs_keys_case_insensitively_with_a_bom() {
        let config = apply(
            "\u{feff}[General]\nuGridsToLoad=7\n; comment\n[DISPLAY]\nfShadowDistance=8000.0000\n[TERRAINMANAGER]\n\
             fblocklevel0distance=40000.0000\nfBlockLevel1Distance = 80000.0000\n\
             fBlockMaximumDistance=300000.0000\nfSplitDistanceMult=2.0000\n",
        );
        assert_eq!((config.stream_radius, config.unload_radius), (3, 4));
        assert_eq!(config.sun_shadow_distance_limit, Some(8_000.0));
        let lod = config.terrain_lod;
        assert_eq!(
            (
                lod.block_level0_distance,
                lod.block_level1_distance,
                lod.block_maximum_distance,
                lod.split_distance_mult
            ),
            (40_000.0, 80_000.0, 300_000.0, 2.0)
        );
        // 40000 * 2 / 4096 = 19.5 cells; 80000 * 2 / 4096 = 39.1; 300000 * 2 / 4096 = 146.5.
        assert_eq!(
            LodTier::ALL.map(|tier| lod.reach_cells(tier)),
            [19, 39, 146]
        );
    }

    #[test]
    fn later_files_override_earlier_ones_key_by_key() {
        let mut ini = SkyrimIni::default();
        ini.merge_str("[TerrainManager]\nfBlockLevel0Distance=20000\nfSplitDistanceMult=2\n");
        ini.merge_str("[TerrainManager]\nfBlockLevel0Distance=30000\n");
        let mut config = EngineConfig::default();
        ini.apply(&mut config);
        assert_eq!(config.terrain_lod.block_level0_distance, 30_000.0);
        assert_eq!(config.terrain_lod.split_distance_mult, 2.0);
    }

    #[test]
    fn malformed_values_and_keys_outside_their_section_change_nothing() {
        let defaults = EngineConfig::default();
        for text in [
            "[General]\nuGridsToLoad=4\n",
            "[General]\nuGridsToLoad=0\n",
            "[General]\nuGridsToLoad=five\n",
            "[TerrainManager]\nfSplitDistanceMult=0\n",
            "[TerrainManager]\nfBlockLevel0Distance=-1\n",
            "[TerrainManager]\nfBlockMaximumDistance=inf\n",
            "uGridsToLoad=9\nfSplitDistanceMult=3\n",
            "[Display]\nfSplitDistanceMult=3\n",
        ] {
            let config = apply(text);
            assert_eq!(config.stream_radius, defaults.stream_radius, "{text}");
            assert_eq!(config.terrain_lod, defaults.terrain_lod, "{text}");
        }
    }

    #[test]
    fn malformed_or_unusable_shadow_distances_leave_the_limit_unset() {
        for value in ["0", "699", "NaN", "inf", "far"] {
            let config = apply(&format!("[Display]\nfShadowDistance={value}\n"));
            assert_eq!(config.sun_shadow_distance_limit, None, "{value}");
        }
    }
}
