//! Authored Havok project/character/behavior metadata used for rig and clip discovery.

use super::packfile::{Packfile, Reader};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Resolves Havok's project-relative paths, including shared `..` references, without
/// allowing an archive key to escape `meshes/`. Character and behavior references
/// are relative to the project, not to the directory of the referring HKX file.
pub fn resolve_reference(project: &str, reference: &str) -> Result<String> {
    ensure!(
        project.starts_with("meshes/")
            && project.split('/').all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(['\\', ':', '\0'])),
        "noncanonical project path {project:?}"
    );
    let reference = reference.replace('\\', "/").to_ascii_lowercase();
    ensure!(
        !reference.starts_with('/') && !reference.contains(':') && !reference.contains('\0'),
        "invalid authoring reference {reference:?}"
    );
    let mut parts: Vec<_> = if reference.starts_with("meshes/") {
        Vec::new()
    } else {
        project
            .rsplit_once('/')
            .ok_or_else(|| eyre!("project has no parent"))?
            .0
            .split('/')
            .map(str::to_owned)
            .collect()
    };
    for part in reference.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                ensure!(
                    parts.len() > 1,
                    "authoring reference escapes meshes: {reference}"
                );
                parts.pop();
            }
            _ => parts.push(part.to_owned()),
        }
    }
    ensure!(
        parts.first().is_some_and(|part| part == "meshes") && parts.len() > 1,
        "authoring reference is outside meshes: {reference}"
    );
    Ok(parts.join("/"))
}

#[derive(Debug, Clone, Serialize)]
pub struct CharacterAssets {
    pub source: String,
    pub rig: String,
    pub behavior: String,
    pub animations: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RaceAuthoring {
    pub male_skeleton: Option<String>,
    pub female_skeleton: Option<String>,
    pub male_project: Option<String>,
    pub female_project: Option<String>,
}

/// RACE has two ANAM skeleton slots before its NAM1 morph section and two
/// NAM3/MODL behavior-project slots. MNAM/FNAM select sex within each section.
/// A later morph or head model must never become an animation project.
pub fn read_race_authoring(fields: &[shared::records::ArchivedSubRecord]) -> Result<RaceAuthoring> {
    read_race_authoring_fields(
        fields
            .iter()
            .map(|field| (field.tag.as_slice(), field.data.as_slice())),
    )
}

pub(crate) fn read_race_authoring_fields<'a>(
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<RaceAuthoring> {
    enum Section {
        Skeleton,
        Project,
        Other,
    }
    let mut section = Section::Skeleton;
    let mut female = None;
    let mut result = RaceAuthoring::default();
    for (tag, data) in fields {
        match tag {
            b"MNAM" => female = Some(false),
            b"FNAM" => female = Some(true),
            b"NAM3" => {
                section = Section::Project;
                female = None;
            }
            tag if tag.len() == 4 && tag[..3] == *b"NAM" && tag[3].is_ascii_digit() => {
                section = Section::Other;
                female = None;
            }
            tag if matches!(
                (&section, tag),
                (Section::Skeleton, b"ANAM") | (Section::Project, b"MODL")
            ) =>
            {
                let female =
                    female.ok_or_else(|| eyre!("race authoring model has no sex selector"))?;
                let bytes = data.strip_suffix(&[0]).unwrap_or(data);
                let value = std::str::from_utf8(bytes)?;
                if value.is_empty() {
                    continue;
                }
                let value = resolve_reference("meshes/race.hkx", value)?;
                let destination = match (&section, female) {
                    (Section::Skeleton, false) => &mut result.male_skeleton,
                    (Section::Skeleton, true) => &mut result.female_skeleton,
                    (Section::Project, false) => &mut result.male_project,
                    (Section::Project, true) => &mut result.female_project,
                    _ => unreachable!(),
                };
                ensure!(destination.is_none(), "duplicate race authoring model slot");
                *destination = Some(value);
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Additive typed projection for runtime identity checks. Metadata rebuilds refresh
/// this from their new winners; a retained catalog cannot animate a race whose
/// project or skeleton was replaced while its body filename stayed the same.
pub(crate) fn project_races(connection: &rusqlite::Connection) -> Result<()> {
    let mut statement =
        connection.prepare("SELECT form_id,data FROM records WHERE record_type='RACE'")?;
    let mut rows = Vec::new();
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (id, bytes) = row?;
        let record =
            rkyv::from_bytes::<shared::records::ArchivedRecordData, rkyv::rancor::Error>(&bytes)
                .map_err(|error| eyre!("RACE {id:08X}: {error}"))?;
        if let Ok(authoring) = read_race_authoring(&record.subrecords) {
            rows.push((id, authoring));
        }
    }
    drop(statement);
    let transaction = connection.unchecked_transaction()?;
    replace_race_projection(&transaction, rows)?;
    transaction.commit()?;
    Ok(())
}

/// One definition for every creation path, so full and archive-only schemas stay identical.
pub(crate) const RACE_ANIMATION_SOURCES_SQL: &str =
    "CREATE TABLE IF NOT EXISTS race_animation_sources (
    race_id INTEGER NOT NULL, sex TEXT NOT NULL, skeleton_nif TEXT, project_hkx TEXT,
    PRIMARY KEY (race_id, sex));";

pub(crate) fn replace_race_projection(
    connection: &rusqlite::Connection,
    rows: Vec<(u32, RaceAuthoring)>,
) -> rusqlite::Result<()> {
    connection.execute_batch(RACE_ANIMATION_SOURCES_SQL)?;
    connection.execute_batch("DELETE FROM race_animation_sources;")?;
    for (id, row) in rows {
        for (sex, skeleton, project) in [
            ("male", row.male_skeleton, row.male_project),
            ("female", row.female_skeleton, row.female_project),
        ] {
            connection.execute("INSERT INTO race_animation_sources(race_id,sex,skeleton_nif,project_hkx) VALUES (?1,?2,?3,?4)",rusqlite::params![id,sex,skeleton,project])?;
        }
    }
    Ok(())
}

/// Discovers the authored rig and complete animation list without inferring a
/// species, skeleton, or clip from a body basename.
pub fn discover_project(
    project: &str,
    mut read: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<Vec<CharacterAssets>> {
    let metadata = read_project(&read(project)?)?;
    metadata
        .characters
        .into_iter()
        .map(|character| {
            let source = resolve_reference(project, &character)?;
            let metadata = read_character(&read(&source)?)?;
            Ok(CharacterAssets {
                source,
                rig: resolve_reference(project, &metadata.rig)?,
                behavior: resolve_reference(project, &metadata.behavior)?,
                animations: metadata
                    .animations
                    .iter()
                    .map(|animation| resolve_reference(project, animation))
                    .collect::<Result<_>>()?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub characters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Character {
    pub name: String,
    pub rig: String,
    pub behavior: String,
    pub animations: Vec<String>,
    pub property_names: Vec<String>,
    pub property_values: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipGenerator {
    pub name: String,
    pub animation: String,
    pub crop_start: f32,
    pub crop_end: f32,
    pub start_time: f32,
    pub playback_speed: f32,
    pub enforced_duration: f32,
    pub mode: u8,
    pub flags: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub name: String,
    pub id: i32,
    pub generator: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlendChild {
    pub generator: u32,
    pub weight: f32,
}

#[derive(Debug, Clone, Serialize)]
pub enum Generator {
    ReferencePose,
    Clip(ClipGenerator),
    Reference(String),
    StateMachine { start: i32, states: Vec<State> },
    Child(u32),
    Selector { selected: i32, children: Vec<u32> },
    Blender { children: Vec<BlendChild> },
    Alternatives { children: Vec<u32> },
    Unsupported(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Behavior {
    pub root: u32,
    pub nodes: BTreeMap<u32, Generator>,
    pub events: Vec<String>,
}

fn object(pack: &Packfile, class: &str) -> Result<u32> {
    let matches: Vec<_> = pack
        .objects
        .iter()
        .filter(|object| object.class == class)
        .collect();
    ensure!(
        matches.len() == 1,
        "expected one {class}, found {}",
        matches.len()
    );
    Ok(matches[0].offset)
}

fn required_string(reader: Reader<'_>, offset: u32) -> Result<String> {
    reader
        .string(offset)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| eyre!("missing authoring string at field {offset}"))
}

fn required_ptr(reader: Reader<'_>, offset: u32) -> Result<u32> {
    reader
        .ptr(offset)?
        .ok_or_else(|| eyre!("missing authoring pointer at field {offset}"))
}

fn strings(pack: &Packfile, reader: Reader<'_>, offset: u32) -> Result<Vec<String>> {
    let (at, count) = reader.array(offset)?;
    reader.array_bytes(offset, 8)?;
    (0..count)
        .map(|index| required_string(pack.reader(at + index * 8), 0))
        .collect()
}

fn pointers(pack: &Packfile, reader: Reader<'_>, offset: u32) -> Result<Vec<u32>> {
    let (at, count) = reader.array(offset)?;
    reader.array_bytes(offset, 8)?;
    (0..count)
        .map(|index| required_ptr(pack.reader(at + index * 8), 0))
        .collect()
}

fn words(reader: Reader<'_>, offset: u32) -> Result<Vec<u32>> {
    Ok(reader
        .array_bytes(offset, 4)?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect())
}

pub fn read_project(bytes: &[u8]) -> Result<Project> {
    let pack = Packfile::parse(bytes)?;
    let reader = pack.reader(object(&pack, "hkbProjectStringData")?);
    Ok(Project {
        characters: strings(&pack, reader, 48)?,
    })
}

pub fn read_character(bytes: &[u8]) -> Result<Character> {
    let pack = Packfile::parse(bytes)?;
    let reader = pack.reader(object(&pack, "hkbCharacterStringData")?);
    let data = pack.reader(object(&pack, "hkbCharacterData")?);
    let property_values = match data.ptr(128)? {
        Some(at) => words(pack.reader(at), 16)?,
        None => Vec::new(),
    };
    Ok(Character {
        name: reader.string(160)?.unwrap_or_default(),
        rig: required_string(reader, 168)?,
        behavior: required_string(reader, 184)?,
        animations: strings(&pack, reader, 48)?,
        property_names: strings(&pack, reader, 80)?,
        property_values,
    })
}

pub fn read_behavior(bytes: &[u8]) -> Result<Behavior> {
    let pack = Packfile::parse(bytes)?;
    let root = required_ptr(pack.reader(object(&pack, "hkbBehaviorGraph")?), 128)?;
    let events = strings(
        &pack,
        pack.reader(object(&pack, "hkbBehaviorGraphStringData")?),
        16,
    )?;
    let mut nodes = BTreeMap::new();
    for object in &pack.objects {
        let reader = pack.reader(object.offset);
        let node = match object.class.as_str() {
            "hkbReferencePoseGenerator" => Generator::ReferencePose,
            "hkbClipGenerator" => {
                let flags = reader.i32(112)? as u32;
                Generator::Clip(ClipGenerator {
                    name: reader.string(56)?.unwrap_or_default(),
                    animation: required_string(reader, 72)?,
                    crop_start: reader.f32(88)?,
                    crop_end: reader.f32(92)?,
                    start_time: reader.f32(96)?,
                    playback_speed: reader.f32(100)?,
                    enforced_duration: reader.f32(104)?,
                    mode: (flags >> 16) as u8,
                    flags: (flags >> 24) as u8,
                })
            }
            "hkbBehaviorReferenceGenerator" => Generator::Reference(required_string(reader, 72)?),
            "hkbStateMachine" => {
                let states = pointers(&pack, reader, 144)?
                    .into_iter()
                    .map(|at| {
                        ensure!(
                            pack.object_at(at)?.class == "hkbStateMachineStateInfo",
                            "state pointer has wrong class"
                        );
                        let state = pack.reader(at);
                        Ok(State {
                            name: state.string(96)?.unwrap_or_default(),
                            id: state.i32(104)?,
                            generator: required_ptr(state, 88)?,
                            enabled: state.i32(112)? & 255 != 0,
                        })
                    })
                    .collect::<Result<_>>()?;
                Generator::StateMachine {
                    start: reader.i32(104)?,
                    states,
                }
            }
            "hkbModifierGenerator"
            | "BSiStateTaggingGenerator"
            | "BSCyclicBlendTransitionGenerator"
            | "BSSynchronizedClipGenerator" => Generator::Child(required_ptr(reader, 80)?),
            "hkbManualSelectorGenerator" => Generator::Selector {
                selected: i32::from(reader.i32(88)? as i16),
                children: pointers(&pack, reader, 72)?,
            },
            "hkbBlenderGenerator" => {
                let children = pointers(&pack, reader, 96)?
                    .into_iter()
                    .map(|at| {
                        ensure!(
                            pack.object_at(at)?.class == "hkbBlenderGeneratorChild",
                            "blend child pointer has wrong class"
                        );
                        let child = pack.reader(at);
                        Ok(BlendChild {
                            generator: required_ptr(child, 48)?,
                            weight: child.f32(64)?,
                        })
                    })
                    .collect::<Result<_>>()?;
                Generator::Blender { children }
            }
            "BSBoneSwitchGenerator" => {
                let mut children = vec![required_ptr(reader, 80)?];
                for at in pointers(&pack, reader, 88)? {
                    ensure!(
                        pack.object_at(at)?.class == "BSBoneSwitchGeneratorBoneData",
                        "bone switch child has wrong class"
                    );
                    children.push(required_ptr(pack.reader(at), 48)?);
                }
                Generator::Alternatives { children }
            }
            class if class.ends_with("Generator") => Generator::Unsupported(class.to_owned()),
            _ => continue,
        };
        nodes.insert(object.offset, node);
    }
    Ok(Behavior {
        root,
        nodes,
        events,
    })
}

#[derive(Debug, Default)]
pub struct IdleSources {
    pub clips: BTreeSet<String>,
    pub issues: Vec<String>,
}

/// Conservatively includes declared idle-named clips and every clip below authored
/// idle states/generators/referenced behaviors. Filename spelling is not the only
/// discovery mechanism. This inventories pose clips; it does not execute transitions,
/// variable expressions, modifiers, or blend masks.
pub fn idle_sources(
    project: &str,
    character: &CharacterAssets,
    mut read: impl FnMut(&str) -> Result<Vec<u8>>,
) -> IdleSources {
    let mut result = IdleSources::default();
    let mut loaded = BTreeMap::new();
    let mut queue = VecDeque::from([character.behavior.clone()]);
    while let Some(path) = queue.pop_front() {
        if loaded.contains_key(&path) {
            continue;
        }
        match read(&path).and_then(|bytes| read_behavior(&bytes)) {
            Ok(behavior) => {
                for generator in behavior.nodes.values() {
                    if let Generator::Reference(reference) = generator {
                        match resolve_reference(project, reference) {
                            Ok(path) if !loaded.contains_key(&path) => queue.push_back(path),
                            Ok(_) => {}
                            Err(error) => result.issues.push(format!("{path}: {error:#}")),
                        }
                    }
                }
                loaded.insert(path, Some(behavior));
            }
            Err(error) => {
                result.issues.push(format!("{path}: {error:#}"));
                loaded.insert(path, None);
            }
        }
        if loaded.len() > 1024 {
            result
                .issues
                .push("behavior inventory exceeds 1024 files".into());
            break;
        }
    }
    collect_idle_nodes(project, character, &loaded, result)
}

fn collect_idle_nodes(
    project: &str,
    character: &CharacterAssets,
    loaded: &BTreeMap<String, Option<Behavior>>,
    mut result: IdleSources,
) -> IdleSources {
    let idle = |name: &str| name.to_ascii_lowercase().contains("idle");
    let declared: BTreeSet<_> = character.animations.iter().cloned().collect();
    result
        .clips
        .extend(declared.iter().filter(|path| idle(path)).cloned());
    let mut pending = Vec::new();
    for (path, behavior) in loaded {
        let Some(behavior) = behavior else { continue };
        let behavior_idle = idle(path);
        for (&node, generator) in &behavior.nodes {
            if behavior_idle {
                pending.push((path.clone(), node));
            }
            match generator {
                Generator::Clip(clip) if idle(&clip.name) => pending.push((path.clone(), node)),
                Generator::StateMachine { states, .. } => {
                    for state in states {
                        if idle(&state.name) {
                            pending.push((path.clone(), state.generator));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut visited = BTreeSet::new();
    while let Some((path, node)) = pending.pop() {
        if !visited.insert((path.clone(), node)) {
            continue;
        }
        let Some(Some(behavior)) = loaded.get(&path) else {
            continue;
        };
        let mut children = Vec::new();
        match behavior.nodes.get(&node) {
            Some(Generator::ReferencePose) => {}
            Some(Generator::Clip(clip)) => match resolve_reference(project, &clip.animation) {
                Ok(source) if declared.contains(&source) => {
                    result.clips.insert(source);
                }
                Ok(source) => result
                    .issues
                    .push(format!("{path}: idle source is undeclared: {source}")),
                Err(error) => result.issues.push(format!("{path}: {error:#}")),
            },
            Some(Generator::Reference(reference)) => {
                if let Ok(target) = resolve_reference(project, reference)
                    && let Some(Some(behavior)) = loaded.get(&target)
                {
                    pending.push((target, behavior.root));
                }
            }
            Some(Generator::StateMachine { states, .. }) => {
                children.extend(states.iter().map(|state| state.generator))
            }
            Some(Generator::Child(child)) => children.push(*child),
            Some(Generator::Selector {
                children: found, ..
            })
            | Some(Generator::Alternatives { children: found }) => children.extend(found),
            Some(Generator::Blender { children: found }) => {
                children.extend(found.iter().map(|child| child.generator))
            }
            Some(Generator::Unsupported(class)) => result
                .issues
                .push(format!("{path}: idle discovery does not traverse {class}")),
            None => result
                .issues
                .push(format!("{path}: missing idle generator {node}")),
        }
        pending.extend(children.into_iter().map(|child| (path.clone(), child)));
    }
    result.issues.sort();
    result.issues.dedup();
    result
}

#[cfg(test)]
mod tests {
    use crate::{animation::packfile::Packfile, archive::ArchiveExtractor};
    use serde_json::json;
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
    };

    #[test]
    fn authored_idle_state_discovers_a_clip_without_idle_in_its_filename() {
        use super::{Behavior, CharacterAssets, ClipGenerator, Generator, IdleSources, State};
        let project = "meshes/actors/test/project.hkx";
        let behavior = "meshes/actors/test/behaviors/base.hkx";
        let character = CharacterAssets {
            source: "meshes/actors/test/character.hkx".into(),
            rig: "meshes/actors/test/rig.hkx".into(),
            behavior: behavior.into(),
            animations: vec![
                "meshes/actors/test/animations/clip_a.hkx".into(),
                "meshes/actors/test/animations/clip_b.hkx".into(),
            ],
        };
        let clip = |name: &str, animation: &str| {
            Generator::Clip(ClipGenerator {
                name: name.into(),
                animation: animation.into(),
                crop_start: 0.0,
                crop_end: 0.0,
                start_time: 0.0,
                playback_speed: 1.0,
                enforced_duration: 0.0,
                mode: 1,
                flags: 0,
            })
        };
        let graph = Behavior {
            root: 1,
            events: Vec::new(),
            nodes: std::collections::BTreeMap::from([
                (
                    1,
                    Generator::StateMachine {
                        start: 0,
                        states: vec![
                            State {
                                name: "Calm Idle".into(),
                                id: 0,
                                generator: 2,
                                enabled: true,
                            },
                            State {
                                name: "Combat".into(),
                                id: 1,
                                generator: 4,
                                enabled: true,
                            },
                        ],
                    },
                ),
                (2, Generator::Child(3)),
                (3, clip("Breathe", "Animations/clip_a.hkx")),
                (4, clip("Run", "Animations/clip_b.hkx")),
            ]),
        };
        let loaded = std::collections::BTreeMap::from([(behavior.into(), Some(graph))]);
        let result =
            super::collect_idle_nodes(project, &character, &loaded, IdleSources::default());
        assert_eq!(
            result.clips,
            std::collections::BTreeSet::from([character.animations[0].clone()])
        );
        assert!(result.issues.is_empty());
    }

    #[test]
    fn authoring_references_use_the_project_parent_and_keep_shared_paths_inside_meshes() {
        let project = "meshes/actors/canine/wolfproject.hkx";
        assert_eq!(
            super::resolve_reference(project, r"Characters Wolf\\Wolf.hkx").unwrap(),
            "meshes/actors/canine/characters wolf/wolf.hkx"
        );
        assert_eq!(
            super::resolve_reference(project, r"..\SharedKillMoves\Wolf.hkx").unwrap(),
            "meshes/actors/sharedkillmoves/wolf.hkx"
        );
        assert_eq!(
            super::resolve_reference(project, "meshes/actors/cow/cow.hkx").unwrap(),
            "meshes/actors/cow/cow.hkx"
        );
        for invalid in [
            "../../../escape.hkx",
            "/tmp/rig.hkx",
            r"C:\rig.hkx",
            "meshes/../escape.hkx",
            "bad\0.hkx",
        ] {
            assert!(
                super::resolve_reference(project, invalid).is_err(),
                "{invalid:?}"
            );
        }
        assert!(super::resolve_reference("meshes/../../unsafe/project.hkx", "rig.hkx").is_err());
    }

    #[test]
    fn authoring_arrays_reject_unbacked_counts_before_allocating() {
        use std::collections::HashMap;
        let mut pack = Packfile {
            data: vec![0; 32],
            pointers: HashMap::from([(0, 16)]),
            objects: Vec::new(),
            contents_offset: 0,
        };
        pack.data[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(super::strings(&pack, pack.reader(0), 0).is_err());
        assert!(super::pointers(&pack, pack.reader(0), 0).is_err());
        assert!(super::words(pack.reader(0), 0).is_err());
    }

    #[test]
    fn race_authoring_keeps_sex_slots_and_ignores_morph_and_head_models() {
        use shared::records::ArchivedSubRecord;
        let fields = [
            (*b"MNAM", ""),
            (*b"ANAM", r"Actors\Rig\male.nif"),
            (*b"FNAM", ""),
            (*b"ANAM", r"Actors\Rig\female.nif"),
            (*b"NAM1", ""),
            (*b"MNAM", ""),
            (*b"MODL", "wrong-morph.egt"),
            (*b"NAM3", ""),
            (*b"MNAM", ""),
            (*b"MODL", r"Actors\Rig\male.hkx"),
            (*b"FNAM", ""),
            (*b"MODL", r"Actors\Rig\female.hkx"),
            (*b"NAM0", ""),
            (*b"MNAM", ""),
            (*b"MODL", "wrong-head.nif"),
        ]
        .into_iter()
        .map(|(tag, value)| ArchivedSubRecord {
            tag,
            data: format!("{value}\0").into_bytes(),
        })
        .collect::<Vec<_>>();
        let race = super::read_race_authoring(&fields).unwrap();
        assert_eq!(
            race.male_skeleton.as_deref(),
            Some("meshes/actors/rig/male.nif")
        );
        assert_eq!(
            race.female_skeleton.as_deref(),
            Some("meshes/actors/rig/female.nif")
        );
        assert_eq!(
            race.male_project.as_deref(),
            Some("meshes/actors/rig/male.hkx")
        );
        assert_eq!(
            race.female_project.as_deref(),
            Some("meshes/actors/rig/female.hkx")
        );
        assert!(
            super::read_race_authoring(&[ArchivedSubRecord {
                tag: *b"ANAM",
                data: b"rig.nif\0".to_vec()
            }])
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires extracted authoring corpus, source Data, and new task-owned output"]
    fn retail_project_clip_contract() {
        use crate::animation::{decode_clip_with_skeleton, decode_skeleton};
        let data = PathBuf::from(std::env::var_os("MUDCRAB_SKYRIM_DATA").unwrap());
        let input = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_AUTHORING_INPUT").unwrap());
        let output = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_AUTHORING_OUTPUT").unwrap());
        fs::create_dir(&output).expect("new task-owned clip output");
        let mut characters = Vec::new();
        let mut discovery_errors = Vec::new();
        for entry in walkdir::WalkDir::new(&input) {
            let entry = entry.unwrap();
            if !entry.file_type().is_file()
                || entry.path().extension().is_none_or(|ext| ext != "hkx")
            {
                continue;
            }
            let project = entry
                .path()
                .strip_prefix(&input)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            // Human gait/composition remains its existing contract; first person is not an actor body.
            if !project.starts_with("meshes/actors/")
                || project.starts_with("meshes/actors/character/")
            {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            if super::read_project(&bytes).is_err() {
                continue;
            }
            match super::discover_project(&project, |key| Ok(fs::read(input.join(key))?)) {
                Ok(found) => {
                    for character in found {
                        characters.push((project.clone(), character));
                    }
                }
                Err(error) => {
                    discovery_errors.push(json!({"project":project,"error":format!("{error:#}")}))
                }
            }
        }
        let selected: BTreeSet<PathBuf> = characters
            .iter()
            .flat_map(|(_, character)| &character.animations)
            .map(PathBuf::from)
            .collect();
        let mut archives: Vec<_> = fs::read_dir(&data)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("bsa"))
            })
            .collect();
        archives.sort();
        let mut sources = std::collections::BTreeMap::new();
        for archive in archives {
            let paths: BTreeSet<_> = ArchiveExtractor::list_paths(&archive)
                .unwrap()
                .into_iter()
                .filter(|path| selected.contains(path))
                .collect();
            for file in ArchiveExtractor::extract_paths(&archive, &output, &paths).unwrap() {
                sources.insert(file.path, archive.clone());
            }
        }
        let mut reports = Vec::new();
        for (project, character) in &characters {
            let skeleton = fs::read(input.join(&character.rig));
            for source in &character.animations {
                let decoded = (|| -> color_eyre::Result<serde_json::Value> {
                    let skeleton = skeleton.as_ref().map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
                    let bones = decode_skeleton(skeleton)?;
                    let bytes = fs::read(output.join(source))?;
                    let clip = decode_clip_with_skeleton(&bytes, skeleton)?;
                    Ok(json!({"frames":clip.frame_count,"duration":clip.duration_seconds,"tracks":clip.tracks.len(),"bones":bones.bone_names.len(),"sha256":crate::cache::hash_bytes(&bytes)}))
                })().unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
                reports.push(json!({"project":project,"character":character.source,"rig":character.rig,"source":source,"archive":sources.get(Path::new(source)),"decoded":decoded}));
            }
        }
        let failures = reports
            .iter()
            .filter(|report| report["decoded"].get("error").is_some())
            .count();
        fs::write(
            output.join("clips.json"),
            serde_json::to_vec_pretty(
                &json!({"discovery_errors":discovery_errors,"clips":reports}),
            )
            .unwrap(),
        )
        .unwrap();
        println!(
            "{} projects/characters; {} clip contracts; {failures} decode failures; {} discovery failures; {}",
            characters.len(),
            reports.len(),
            discovery_errors.len(),
            output.display()
        );
        // This is an audit, not a success-rate gate: failures remain in the retained receipt.
    }

    #[test]
    #[ignore = "requires source Data and task-owned MUDCRAB_IDLE_AUTHORING_OUTPUT"]
    fn retail_idle_authoring_inventory() {
        let data = PathBuf::from(std::env::var_os("MUDCRAB_SKYRIM_DATA").unwrap());
        let output = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_AUTHORING_OUTPUT").unwrap());
        fs::create_dir(&output).expect("new task-owned authoring output");
        let input = std::env::var_os("MUDCRAB_IDLE_AUTHORING_INPUT").map(PathBuf::from);
        let source = input.as_deref().unwrap_or(&output);
        let mut extracted = BTreeSet::new();
        if input.is_some() {
            for entry in walkdir::WalkDir::new(source) {
                let entry = entry.unwrap();
                if entry.file_type().is_file()
                    && entry.path().extension().is_some_and(|ext| ext == "hkx")
                {
                    extracted.insert(entry.path().strip_prefix(source).unwrap().to_owned());
                }
            }
        } else {
            let mut archives: Vec<_> = fs::read_dir(&data)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("bsa"))
                })
                .collect();
            archives.sort();
            for archive in archives {
                let selected: BTreeSet<_> = ArchiveExtractor::list_paths(&archive)
                    .unwrap()
                    .into_iter()
                    .filter(|path| {
                        let key = path.to_string_lossy();
                        key.starts_with("meshes/")
                            && key.ends_with(".hkx")
                            && !key.contains("/animations/")
                    })
                    .collect();
                for file in ArchiveExtractor::extract_paths(&archive, &output, &selected).unwrap() {
                    extracted.insert(file.path);
                }
            }
        }
        let mut reports = Vec::new();
        for file in extracted {
            let bytes = fs::read(source.join(&file)).unwrap();
            let parsed = (|| -> color_eyre::Result<serde_json::Value> {
                let pack = Packfile::parse(&bytes)?;
                let classes: BTreeSet<_> = pack
                    .objects
                    .iter()
                    .map(|object| object.class.as_str())
                    .collect();
                let value = if classes.contains("hkbProjectStringData") {
                    serde_json::to_value(super::read_project(&bytes)?)?
                } else if classes.contains("hkbCharacterStringData") {
                    serde_json::to_value(super::read_character(&bytes)?)?
                } else if classes.contains("hkbBehaviorGraph") {
                    serde_json::to_value(super::read_behavior(&bytes)?)?
                } else {
                    serde_json::Value::Null
                };
                Ok(json!({"classes": classes, "value": value}))
            })()
            .unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
            reports.push(
                json!({"path":file,"sha256":crate::cache::hash_bytes(&bytes),"parsed":parsed}),
            );
        }
        fs::write(
            output.join("authoring.json"),
            serde_json::to_vec_pretty(&reports).unwrap(),
        )
        .unwrap();
        println!(
            "{} authoring files inspected; {}",
            reports.len(),
            output.display()
        );
    }
}
