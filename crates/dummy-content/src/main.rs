//! Command-line entry point for the deterministic fixture generator.
#![forbid(unsafe_code)]

use color_eyre::{
    Result,
    eyre::{bail, ensure, eyre},
};
use dummy_content::{
    esm,
    layout::{self, DEFAULT_SEED, Formats},
};
use std::{
    path::{Path, PathBuf},
    process::exit,
};

fn main() -> Result<()> {
    color_eyre::install()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            println!("{}", usage());
            Ok(())
        }
        Some("gen") => run_gen(&arguments[1..]),
        Some(command) => bail!("unknown command `{command}`\n\n{}", usage()),
    }
}

fn run_gen(arguments: &[String]) -> Result<()> {
    let options = parse_gen(arguments)?;
    let mut formats = options.formats;
    if options.with_interior || options.with_lights {
        // A preset writes the plugin itself, so the default one is not
        // generated: each preset replaces `Skyrim.esm`.
        formats.esm = false;
    }
    layout::prepare_directory(&options.output, options.force)?;
    let mut written = layout::generate(&options.output, options.seed, formats)?;
    if options.with_interior {
        written.push(write_interior_plugin(&options.output)?);
    } else if options.with_lights {
        written.push(write_lights_plugin(&options.output)?);
    }
    println!(
        "Generated {} fixture files in {}",
        written.len(),
        options.output.display()
    );
    Ok(())
}

/// The spec every preset plugin is written from: one exterior cell of the
/// generated worldspace, grid (0, 0), the square the crate's other fixtures
/// place their static in.
fn preset_spec() -> esm::Plugin<'static> {
    static CELLS: [esm::Cell; 1] = [esm::PRESET_EXTERIOR_CELL];
    esm::Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &CELLS,
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    }
}

/// Writes `Skyrim.esm` as the interior preset: one exterior cell, its
/// auto-load door into one interior cell, and the return door.
///
/// The preset replaces the plugin the default tree writes, so its bytes go
/// through [`layout::write_plugin`] - the same writer, and so the same symlink
/// refusal and atomic publication, as every other generated file.
fn write_interior_plugin(output: &Path) -> Result<PathBuf> {
    let bytes = esm::plugin_with_interior(&preset_spec(), &esm::PRESET_INTERIOR)?;
    layout::write_plugin(output, &bytes)
}

/// Writes `Skyrim.esm` as the light preset: one exterior cell and one `LIGH`
/// base record, placed by a single reference that carries the light's `XRDS`
/// radius override.
///
/// Like the interior preset it goes through [`layout::write_plugin`], and it
/// deliberately puts its reference in the same cell the crate's other fixtures
/// use, so the two presets describe one square of the same world.
fn write_lights_plugin(output: &Path) -> Result<PathBuf> {
    let bytes = esm::plugin_with_lights(&preset_spec(), &esm::PRESET_LIGHT)?;
    layout::write_plugin(output, &bytes)
}

#[derive(Debug)]
struct GenOptions {
    output: PathBuf,
    seed: u64,
    formats: Formats,
    force: bool,
    with_interior: bool,
    with_lights: bool,
}

fn parse_gen(arguments: &[String]) -> Result<GenOptions> {
    let mut output = None;
    let mut seed = DEFAULT_SEED;
    let mut formats = Formats::default();
    let mut force = false;
    let mut with_interior = false;
    let mut with_lights = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--seed" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| eyre!("--seed requires a value"))?;
                seed = value
                    .parse()
                    .map_err(|_| eyre!("invalid --seed value {value:?}"))?;
            }
            "--formats" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| eyre!("--formats requires a value"))?;
                formats = Formats::parse(value)?;
            }
            "--force" => force = true,
            "--with-interior" => with_interior = true,
            "--with-lights" => with_lights = true,
            "-h" | "--help" => {
                println!("{}", usage());
                exit(0);
            }
            argument if argument.starts_with("--") => {
                bail!("unknown option `{argument}`\n\n{}", usage())
            }
            argument => {
                ensure!(output.is_none(), "unexpected extra argument `{argument}`");
                output = Some(PathBuf::from(argument));
            }
        }
        index += 1;
    }
    let output =
        output.ok_or_else(|| eyre!("`gen` requires an output directory\n\n{}", usage()))?;
    for (name, enabled) in [
        ("--with-interior", with_interior),
        ("--with-lights", with_lights),
    ] {
        if enabled {
            ensure!(
                formats.esm,
                "{name} writes a plugin; add esm to --formats\n\n{}",
                usage()
            );
        }
    }
    ensure!(
        !(with_interior && with_lights),
        "--with-interior and --with-lights are two different presets, and each \
         replaces Skyrim.esm; pass one\n\n{}",
        usage()
    );
    Ok(GenOptions {
        output,
        seed,
        formats,
        force,
        with_interior,
        with_lights,
    })
}

fn usage() -> &'static str {
    "dummy-content: deterministic Skyrim-format fixtures

USAGE:
    dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]
                      [--with-interior | --with-lights]

COMMANDS:
    gen    Generate a synthetic Data directory

OPTIONS:
    --seed <n>        Seed for generated texture content
    --formats <list>  Comma-separated subset of: dds, pex, nif, bsa, ba2, esm
    --force           Overwrite generated files in a non-empty directory
    --with-interior   Write Skyrim.esm with one exterior cell, one interior
                      cell and a reciprocal pair of load doors (needs esm)
    --with-lights     Write Skyrim.esm with one exterior cell, one LIGH base
                      record and the one reference that places it, carrying
                      its own XRDS radius (needs esm)"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_the_interior_preset_flag() {
        let options = parse_gen(&arguments(&["out", "--with-interior"])).unwrap();
        assert!(options.with_interior);
        assert_eq!(options.output, PathBuf::from("out"));
        assert!(
            options.formats.esm,
            "the default formats include the plugin"
        );
        assert!(!parse_gen(&arguments(&["out"])).unwrap().with_interior);
        assert!(
            parse_gen(&arguments(&["out", "--with-interior", "--formats", "dds"])).is_err(),
            "the preset needs the esm format"
        );
        assert!(
            parse_gen(&arguments(&[
                "out",
                "--formats",
                "dds,esm",
                "--with-interior"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn parses_the_lights_preset_flag() {
        let options = parse_gen(&arguments(&["out", "--with-lights"])).unwrap();
        assert!(options.with_lights);
        assert!(!options.with_interior);
        assert!(
            options.formats.esm,
            "the default formats include the plugin"
        );
        assert!(!parse_gen(&arguments(&["out"])).unwrap().with_lights);
        assert!(
            parse_gen(&arguments(&["out", "--with-lights", "--formats", "dds"])).is_err(),
            "the preset needs the esm format"
        );
        assert!(parse_gen(&arguments(&["out", "--formats", "esm", "--with-lights"])).is_ok());
        assert!(
            parse_gen(&arguments(&["out", "--with-interior", "--with-lights"])).is_err(),
            "the two presets each replace Skyrim.esm and cannot be combined"
        );
    }

    #[test]
    fn writes_the_lights_preset_plugin() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Data");
        run_gen(&arguments(&[
            output.to_str().unwrap(),
            "--formats",
            "esm",
            "--with-lights",
        ]))
        .unwrap();

        let plugin = std::fs::read(output.join("Skyrim.esm")).unwrap();
        assert_eq!(&plugin[..4], b"TES4");
        assert!(plugin.windows(4).any(|window| window == b"LIGH"));
        assert!(plugin.windows(4).any(|window| window == b"XRDS"));
        let names: Vec<String> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Skyrim.esm"], "the preset left another file behind");
    }

    #[test]
    fn writes_the_interior_preset_plugin() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Data");
        run_gen(&arguments(&[
            output.to_str().unwrap(),
            "--formats",
            "esm",
            "--with-interior",
        ]))
        .unwrap();

        let plugin = std::fs::read(output.join("Skyrim.esm")).unwrap();
        assert_eq!(&plugin[..4], b"TES4");
        assert!(plugin.windows(4).any(|window| window == b"DOOR"));
        assert!(plugin.windows(4).any(|window| window == b"XTEL"));
        let names: Vec<String> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Skyrim.esm"], "the preset left another file behind");
    }

    /// The preset's plugin goes through the same writer as every other
    /// generated file: the writer refuses a stale temporary left by an
    /// interrupted run, where a direct `fs::write` would have ignored it and
    /// published the plugin anyway.
    #[test]
    fn the_interior_preset_refuses_a_stale_plugin_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Data");
        layout::prepare_directory(&output, false).unwrap();
        let stale = output.join(format!("Skyrim.esm.{}.partial", std::process::id()));
        std::fs::write(&stale, b"stale").unwrap();

        let error = run_gen(&arguments(&[
            output.to_str().unwrap(),
            "--formats",
            "esm",
            "--force",
            "--with-interior",
        ]))
        .unwrap_err();
        assert!(
            error.to_string().contains("stale fixture temporary"),
            "unexpected error: {error:#}"
        );
        assert!(
            !output.join("Skyrim.esm").exists(),
            "the preset wrote the plugin despite the stale temporary"
        );
    }
}
