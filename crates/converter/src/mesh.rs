use crate::material::{
    NifAlphaMode, NifMaterialDisposition, NifShapeMaterial, build_nif_material_contract,
    publish_gltf_materials,
};
use crate::texture::TextureSemantic;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use project_wormhole_esm::structs::strings::{SizedString8, SizedString32, StringN};
use project_wormhole_nif::{
    nif_block::NifBlock,
    nif_file::{NifFile, nif_to_model, nif_to_static_model},
    nif_header::{Endianess, NifFileVersion, NifHeader},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub struct MeshConverter;

#[derive(Debug, Clone, Default, Serialize)]
pub struct NifParseDiagnostics {
    pub block_count: usize,
    pub parsed_block_count: usize,
    pub geometry_block_count: usize,
    pub scene_node_count: usize,
    pub max_scene_depth: usize,
    pub block_types: BTreeMap<String, usize>,
    pub fallback_blocks: BTreeMap<String, usize>,
    pub fallback_offsets: BTreeMap<String, Vec<usize>>,
    pub material_shape_count: usize,
    pub validated_material_shape_count: usize,
    pub excluded_material_shape_count: usize,
    pub material_exclusions: BTreeMap<String, usize>,
}

impl MeshConverter {
    pub fn dependency_paths(nif_path: &Path) -> Vec<PathBuf> {
        find_skeleton(nif_path).into_iter().collect()
    }

    pub fn convert_nif_to_glb<P: AsRef<Path>>(nif_path: P, glb_output_path: P) -> Result<()> {
        let nif_path = nif_path.as_ref();
        let (nif, diagnostics, material_contract) = open_nif_resilient(nif_path)?;
        let skeleton = if nif.has_skeleton() {
            let skeleton_path = find_skeleton(nif_path).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "skinned NIF requires a skeleton, but none was found near {}",
                    nif_path.display()
                )
            })?;
            Some(open_nif_resilient(&skeleton_path)?.0)
        } else {
            None
        };
        let primary = catch_unwind(AssertUnwindSafe(|| nif_to_model(&nif, skeleton.as_ref())));
        let (mut model, used_static_fallback) = match primary {
            Ok(Ok(model)) => (model, false),
            primary => {
                let primary_error = match primary {
                    Ok(Err(error)) => format!("NIF model conversion failed: {error}"),
                    Err(_) => format!("NIF model conversion panicked for {}", nif_path.display()),
                    Ok(Ok(_)) => unreachable!(),
                };
                let static_model = catch_unwind(AssertUnwindSafe(|| nif_to_static_model(&nif)))
                    .map_err(|_| color_eyre::eyre::eyre!("static NIF fallback panicked"))?
                    .map_err(|error| color_eyre::eyre::eyre!("static NIF fallback failed: {error}"))
                    .wrap_err(primary_error)?;
                (static_model, true)
            }
        };
        model.scene_root_rotation = Some(shared::coordinates::CREATION_TO_RUNTIME_ROTATION);
        model
            .validate()
            .map_err(|error| color_eyre::eyre::eyre!("invalid converted NIF model: {error}"))?;
        normalize_cutout_vertex_alpha(&mut model, &nif, &material_contract)?;
        let name = nif_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if model.static_meshes.is_empty() && model.skeletal_meshes.is_empty() {
            ensure!(
                is_deferred_dynamic_mesh(nif_path)
                    || !diagnostics
                        .block_types
                        .keys()
                        .any(|block_type| is_declared_geometry_block(block_type)),
                "NIF declares mesh geometry, but no supported geometry was converted"
            );
            let output = glb_output_path.as_ref();
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            return write_glb_atomic(output, &empty_scene_glb(&name));
        }
        let output = glb_output_path.as_ref();
        let mut glb = catch_unwind(AssertUnwindSafe(|| model.to_glb(name.clone())))
            .map_err(|_| color_eyre::eyre::eyre!("NIF GLB export panicked"))?;
        if glb_bounds_from_bytes(&glb).is_err() && !used_static_fallback {
            let mut static_model = nif_to_static_model(&nif)
                .map_err(|error| color_eyre::eyre::eyre!("static NIF fallback failed: {error}"))?;
            static_model.scene_root_rotation =
                Some(shared::coordinates::CREATION_TO_RUNTIME_ROTATION);
            normalize_cutout_vertex_alpha(&mut static_model, &nif, &material_contract)?;
            ensure!(
                !static_model.static_meshes.is_empty(),
                "NIF contains no supported mesh geometry"
            );
            glb = catch_unwind(AssertUnwindSafe(|| static_model.to_glb(name)))
                .map_err(|_| color_eyre::eyre::eyre!("static NIF GLB export panicked"))?;
            model = static_model;
        }
        let shape_blocks = exported_shape_blocks(&nif, &model, &material_contract)?;
        let exported_material_contract = shape_blocks
            .iter()
            .map(|block| {
                material_contract
                    .iter()
                    .find(|shape| shape.shape_block == *block)
                    .cloned()
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "exported mesh references shape block {block} without a material contract"
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let glb = rewrite_materials_and_texture_uris(
            glb,
            &exported_material_contract,
            &shape_blocks,
            output,
        )?;
        ensure!(
            glb.len() >= 12 && &glb[..4] == b"glTF",
            "NIF exporter produced an invalid GLB header"
        );
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        write_glb_atomic(output, &glb)
    }

    pub fn inspect_nif(path: &Path) -> Result<NifParseDiagnostics> {
        open_nif_resilient(path).map(|(_, diagnostics, _)| diagnostics)
    }

    /// Extracts the validated per-shape NIF material contract without
    /// publishing glTF/PBR decisions that belong to the next pipeline stage.
    pub fn inspect_nif_materials(path: &Path) -> Result<Vec<NifShapeMaterial>> {
        open_nif_resilient(path).map(|(_, _, contract)| contract)
    }

    /// Reads the accessor bounds written to a GLB and applies the complete glTF
    /// node hierarchy. This avoids loading vertex buffers merely to build the
    /// runtime spatial index.
    pub fn glb_bounds(path: &Path) -> Result<shared::Bounds3> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        glb_bounds_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to extract bounds from {}", path.display()))
    }

    /// Returns every external image URI referenced by a GLB. Embedded images
    /// have no URI and are intentionally omitted.
    pub fn glb_texture_uris(path: &Path) -> Result<Vec<String>> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let document = glb_json_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to inspect textures in {}", path.display()))?;
        let mut uris = document
            .get("images")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|image| image.get("uri").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        uris.sort_by_key(|uri| uri.to_ascii_lowercase());
        uris.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        Ok(uris)
    }

    /// Returns material-aware external texture dependencies. Base-color
    /// textures are mandatory; auxiliary maps remain explicitly optional until
    /// the shader contract requires them.
    pub fn glb_texture_dependencies(path: &Path) -> Result<Vec<TextureDependency>> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let document = glb_json_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to inspect textures in {}", path.display()))?;
        Ok(texture_dependencies(&document))
    }

    /// Removes image URIs with no file on disk from every GLB under `root`.
    ///
    /// NIF sources occasionally reference textures Bethesda never shipped.
    /// Those references survive material publishing as dangling URIs, which
    /// fail strict runtime validation. Pruning drops the missing images along
    /// with every texture and core or OPEN_SKYRIM material slot that points at
    /// them, so the mesh renders with its remaining maps instead of failing to
    /// load. Files without dangling URIs are left untouched. Returns one
    /// report per rewritten file, ordered by path.
    pub fn prune_dangling_texture_uris(root: &Path) -> Result<Vec<PrunedGlb>> {
        let mut glbs: Vec<PathBuf> = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("glb"))
            })
            .map(|entry| entry.into_path())
            .collect();
        glbs.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
        let mut pruned = Vec::new();
        for glb_path in glbs {
            if let Some(report) = prune_dangling_uris_in_glb(root, &glb_path)? {
                pruned.push(report);
            }
        }
        Ok(pruned)
    }
}

/// One GLB rewritten by [`MeshConverter::prune_dangling_texture_uris`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrunedGlb {
    /// GLB path relative to the pruned root, with `/` separators.
    pub glb: String,
    /// Removed image URIs exactly as they appeared in the document.
    pub removed_uris: Vec<String>,
}

fn prune_dangling_uris_in_glb(root: &Path, glb_path: &Path) -> Result<Option<PrunedGlb>> {
    let bytes =
        fs::read(glb_path).wrap_err_with(|| format!("failed to read {}", glb_path.display()))?;
    let mut document = glb_json_from_bytes(&bytes)
        .wrap_err_with(|| format!("failed to inspect textures in {}", glb_path.display()))?;
    let missing: Vec<(usize, String)> = document
        .get("images")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, image)| {
            image
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .map(|uri| (index, uri.to_owned()))
        })
        .filter(|(_, uri)| !texture_uri_resolves(root, glb_path, uri))
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let removed: HashSet<usize> = missing.iter().map(|(index, _)| *index).collect();
    prune_document_images(&mut document, &removed);
    let pruned = rebuild_glb_with_document(&bytes, &document)
        .wrap_err_with(|| format!("failed to rebuild {}", glb_path.display()))?;
    write_glb_atomic(glb_path, &pruned)?;
    let relative = glb_path
        .strip_prefix(root)
        .unwrap_or(glb_path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Some(PrunedGlb {
        glb: relative,
        removed_uris: missing.into_iter().map(|(_, uri)| uri).collect(),
    }))
}

fn texture_uri_resolves(root: &Path, glb_path: &Path, uri: &str) -> bool {
    // Embedded and remote content needs no file; only prune what claims to be
    // a local path but has nothing on disk.
    if uri.is_empty() || uri.starts_with("data:") || uri.contains("://") {
        return uri.starts_with("data:") || uri.contains("://");
    }
    if crate::asset_path::resolve_asset_uri(root, glb_path, uri)
        .is_ok_and(|candidate| candidate.is_file())
    {
        return true;
    }
    // Canonical URIs without `../` segments resolve against the tree root.
    crate::asset_path::resolve_asset_uri(root, &root.join("meshes"), uri)
        .is_ok_and(|candidate| candidate.is_file())
}

fn prune_document_images(document: &mut serde_json::Value, removed: &HashSet<usize>) {
    let Some(images) = document
        .get_mut("images")
        .and_then(|images| images.as_array_mut())
    else {
        return;
    };
    let mut image_remap = vec![None; images.len()];
    let mut kept = Vec::with_capacity(images.len());
    for (index, image) in images.drain(..).enumerate() {
        if removed.contains(&index) {
            continue;
        }
        image_remap[index] = Some(kept.len());
        kept.push(image);
    }
    *images = kept;
    let mut texture_remap = Vec::new();
    if let Some(textures) = document
        .get_mut("textures")
        .and_then(|textures| textures.as_array_mut())
    {
        let mut kept = Vec::with_capacity(textures.len());
        for mut texture in textures.drain(..) {
            let remapped = texture
                .get("source")
                .and_then(serde_json::Value::as_u64)
                .and_then(|source| usize::try_from(source).ok())
                .and_then(|source| image_remap.get(source).copied().flatten());
            match remapped {
                Some(source) => {
                    texture["source"] = serde_json::Value::from(source as u64);
                    texture_remap.push(Some(kept.len()));
                    kept.push(texture);
                }
                None => {
                    texture_remap.push(None);
                }
            }
        }
        *textures = kept;
    }
    if texture_remap.iter().all(|entry| entry.is_some()) {
        return;
    }
    let Some(materials) = document
        .get_mut("materials")
        .and_then(|materials| materials.as_array_mut())
    else {
        return;
    };
    for material in materials.iter_mut() {
        let Some(object) = material.as_object_mut() else {
            continue;
        };
        if let Some(pbr) = object
            .get_mut("pbrMetallicRoughness")
            .and_then(|pbr| pbr.as_object_mut())
        {
            for slot in ["baseColorTexture", "metallicRoughnessTexture"] {
                remap_texture_info(pbr, slot, "index", &texture_remap);
            }
        }
        for slot in ["normalTexture", "occlusionTexture", "emissiveTexture"] {
            remap_texture_info(object, slot, "index", &texture_remap);
        }
        if let Some(slots) = object
            .get_mut("extensions")
            .and_then(|extensions| extensions.get_mut("OPEN_SKYRIM_material"))
            .and_then(|extension| extension.get_mut("textureSlots"))
            .and_then(|slots| slots.as_array_mut())
        {
            let mut kept = Vec::with_capacity(slots.len());
            for mut slot in slots.drain(..) {
                let remapped = slot
                    .get("texture")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| texture_remap.get(index).copied().flatten());
                if let Some(texture) = remapped {
                    slot["texture"] = serde_json::Value::from(texture as u64);
                    kept.push(slot);
                }
            }
            *slots = kept;
        }
    }
}

fn remap_texture_info(
    object: &mut serde_json::Map<String, serde_json::Value>,
    slot: &str,
    index_key: &str,
    texture_remap: &[Option<usize>],
) {
    let remapped = object
        .get(slot)
        .and_then(|info| info.get(index_key))
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| texture_remap.get(index).copied().flatten());
    match remapped {
        Some(index) => {
            object[slot][index_key] = serde_json::Value::from(index as u64);
        }
        None => {
            object.remove(slot);
        }
    }
}

fn rebuild_glb_with_document(original: &[u8], document: &serde_json::Value) -> Result<Vec<u8>> {
    let json_length = u32::from_le_bytes(
        original
            .get(12..16)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB header"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let binary = original
        .get(json_end..)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    let mut json = serde_json::to_vec(document)?;
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut glb = Vec::with_capacity(20 + json.len() + binary.len());
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&((20 + json.len() + binary.len()) as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(binary);
    Ok(glb)
}

fn is_declared_geometry_block(block_type: &str) -> bool {
    matches!(
        block_type,
        "BSTriShape"
            | "BSDynamicTriShape"
            | "BSSubIndexTriShape"
            | "BSMeshLODTriShape"
            | "BSLODTriShape"
            | "NiTriShape"
            | "NiTriStrips"
    )
}

fn is_deferred_dynamic_mesh(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    // Creation Club layouts nest the same dynamic categories under an extra
    // `creationclub/<mod>/` infix, so match the category segment anywhere
    // below the meshes root instead of only directly below it.
    ["/actors/", "/magic/", "/effects/"]
        .iter()
        .any(|category| normalized.contains(category))
}

fn empty_scene_glb(name: &str) -> Vec<u8> {
    let mut json = serde_json::to_vec(&serde_json::json!({
        "asset": { "version": "2.0", "generator": "OpenSkyrim converter" },
        "scene": 0,
        "scenes": [{ "name": name, "nodes": [] }]
    }))
    .expect("static empty-scene glTF JSON is serializable");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total_length = 20 + json.len();
    let mut glb = Vec::with_capacity(total_length);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_length as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb
}

fn glb_json_from_bytes(bytes: &[u8]) -> Result<serde_json::Value> {
    ensure!(
        bytes.len() >= 20 && &bytes[..4] == b"glTF",
        "invalid GLB container"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == 2,
        "unsupported GLB version"
    );
    let declared_length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    ensure!(declared_length == bytes.len(), "GLB length is inconsistent");
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    ensure!(&bytes[16..20] == b"JSON", "GLB JSON chunk is missing");
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let json = bytes
        .get(20..json_end)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    serde_json::from_slice(json).wrap_err("invalid glTF JSON")
}

fn exported_shape_blocks(
    nif: &NifFile,
    model: &project_wormhole_nif::model::all::Model,
    contract: &[NifShapeMaterial],
) -> Result<Vec<u32>> {
    let mut blocks = Vec::with_capacity(model.static_meshes.len() + model.skeletal_meshes.len());
    for mesh_index in 0..model.static_meshes.len() {
        let mut matches = model
            .static_nodes
            .iter()
            .filter(|node| node.mesh == Some(mesh_index));
        let node = matches.next().ok_or_else(|| {
            color_eyre::eyre::eyre!("static mesh {mesh_index} has no source shape block")
        })?;
        ensure!(
            matches.next().is_none(),
            "static mesh {mesh_index} is associated with multiple source shape blocks"
        );
        blocks.push(node.block_index);
    }
    if !model.skeletal_meshes.is_empty() {
        let mut skeletal_blocks = Vec::new();
        let predicates: [fn(&NifBlock) -> bool; 3] = [
            |block: &NifBlock| matches!(block, NifBlock::BSTriShape(_)),
            |block: &NifBlock| matches!(block, NifBlock::BSDynamicTriShape(_)),
            |block: &NifBlock| matches!(block, NifBlock::BSSubIndexTriShape(_)),
        ];
        for predicate in predicates {
            skeletal_blocks.extend(
                nif.blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| predicate(block))
                    .map(|(index, _)| u32::try_from(index))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        ensure!(
            skeletal_blocks.len() == model.skeletal_meshes.len(),
            "skeletal mesh/material association is incomplete: {} meshes, {} source shapes",
            model.skeletal_meshes.len(),
            skeletal_blocks.len()
        );
        blocks.extend(skeletal_blocks);
    }
    ensure!(
        blocks.len() <= contract.len(),
        "mesh/material contract is incomplete: {} exported meshes, {} source shapes",
        blocks.len(),
        contract.len()
    );
    Ok(blocks)
}

/// Forces vertex-color alpha to opaque on alpha-tested (Cutout) shapes.
///
/// Bethesda stores edge fade in vertex alpha on foliage cards. The runtime
/// multiplies vertex alpha into the alpha test, so minified distant texels
/// fall below the authored cutoff and whole forests discard to sky. The
/// cutout decision must come from the texture alpha alone.
fn normalize_cutout_vertex_alpha(
    model: &mut project_wormhole_nif::model::all::Model,
    nif: &NifFile,
    contract: &[NifShapeMaterial],
) -> Result<()> {
    let blocks = exported_shape_blocks(nif, model, contract)?;
    let static_count = model.static_meshes.len();
    for (mesh_index, block) in blocks.iter().enumerate() {
        let shape = contract
            .iter()
            .find(|shape| shape.shape_block == *block)
            .ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "exported mesh references shape block {block} without a material contract"
                )
            })?;
        let cutout = matches!(
            &shape.disposition,
            NifMaterialDisposition::Validated { material }
                if material.alpha_mode == NifAlphaMode::Cutout
        );
        if !cutout {
            continue;
        }
        if mesh_index < static_count {
            for color in &mut model.static_meshes[mesh_index].colors {
                color.0.w = 1.0;
            }
        } else if let Some(inner) = model
            .skeletal_meshes
            .get_mut(mesh_index - static_count)
            .and_then(|mesh| mesh.mesh.as_mut())
        {
            for color in &mut inner.colors {
                color.0.w = 1.0;
            }
        }
    }
    Ok(())
}

fn open_nif_resilient(
    path: &Path,
) -> Result<(NifFile, NifParseDiagnostics, Vec<NifShapeMaterial>)> {
    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let (mut data, header) = parse_skyrim_header(&bytes, path)?;
    let block_count = usize::try_from(header.block_count)
        .wrap_err_with(|| format!("NIF block count is out of range in {}", path.display()))?;
    ensure!(
        header.block_type_index.len() == block_count
            && header.block_size_index.len() == block_count,
        "NIF header block tables are inconsistent in {}",
        path.display()
    );
    let mut diagnostics = NifParseDiagnostics {
        block_count,
        ..Default::default()
    };
    let mut blocks = Vec::with_capacity(block_count);
    for index in 0..block_count {
        let size = usize::try_from(header.block_size_index[index])
            .wrap_err("NIF block size is out of range")?;
        ensure!(
            data.len() >= size,
            "NIF block {index} is truncated in {}",
            path.display()
        );
        let (raw, remaining) = data.split_at(size);
        data = remaining;
        let block_type = header
            .get_block_type(index)
            .map_err(|_| {
                color_eyre::eyre::eyre!(
                    "NIF block {index} has an invalid type index in {}",
                    path.display()
                )
            })?
            .to_owned();
        *diagnostics
            .block_types
            .entry(block_type.clone())
            .or_default() += 1;
        let parsed = catch_unwind(AssertUnwindSafe(|| {
            NifBlock::parse(raw, block_type.clone())
        }));
        let block = match parsed {
            Ok(Ok((_, NifBlock::Unhandled))) => {
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(0);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
            Ok(Ok((_, block))) => {
                diagnostics.parsed_block_count += 1;
                if matches!(
                    &block,
                    NifBlock::BSTriShape(_)
                        | NifBlock::BSDynamicTriShape(_)
                        | NifBlock::BSSubIndexTriShape(_)
                        | NifBlock::BSLODTriShape(_)
                        | NifBlock::NiTriShape(_)
                ) {
                    diagnostics.geometry_block_count += 1;
                }
                block
            }
            Ok(Err(error)) => {
                let offset = match error {
                    nom_derive::nom::Err::Error(error) | nom_derive::nom::Err::Failure(error) => {
                        raw.len().saturating_sub(error.input.len())
                    }
                    nom_derive::nom::Err::Incomplete(_) => raw.len(),
                };
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(offset);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
            Err(_) => {
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(usize::MAX);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
        };
        blocks.push(block);
    }
    diagnostics.scene_node_count = blocks
        .iter()
        .filter(|block| {
            matches!(
                block,
                NifBlock::NiNode(_)
                    | NifBlock::BSFadeNode(_)
                    | NifBlock::BSTriShape(_)
                    | NifBlock::BSDynamicTriShape(_)
                    | NifBlock::BSSubIndexTriShape(_)
                    | NifBlock::BSLODTriShape(_)
                    | NifBlock::NiTriShape(_)
            )
        })
        .count();
    diagnostics.max_scene_depth = nif_scene_depth(&blocks);
    let nif = NifFile { header, blocks };
    let material_contract = build_nif_material_contract(&nif, path)?;
    diagnostics.material_shape_count = material_contract.len();
    for shape in &material_contract {
        match &shape.disposition {
            NifMaterialDisposition::Validated { .. } => {
                diagnostics.validated_material_shape_count += 1;
            }
            NifMaterialDisposition::Excluded { reason } => {
                diagnostics.excluded_material_shape_count += 1;
                *diagnostics
                    .material_exclusions
                    .entry(reason.clone())
                    .or_default() += 1;
            }
        }
    }
    Ok((nif, diagnostics, material_contract))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextureDependency {
    pub uri: String,
    pub semantic: TextureSemantic,
    pub required: bool,
}

fn write_glb_atomic(output: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let extension = output
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("glb");
    let temporary = output.with_extension(format!("{extension}.{}.partial", std::process::id()));
    let backup = output.with_extension(format!("{extension}.{}.backup", std::process::id()));
    ensure!(
        !temporary.exists() && !backup.exists(),
        "stale temporary GLB exists for {}",
        output.display()
    );
    let mut file = fs::File::create(&temporary)
        .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if output.exists() {
        fs::rename(output, &backup)
            .wrap_err_with(|| format!("failed to preserve {}", output.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, output) {
        if backup.exists() {
            let _ = fs::rename(&backup, output);
        }
        return Err(error).wrap_err_with(|| format!("failed to publish {}", output.display()));
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn nif_scene_depth(blocks: &[NifBlock]) -> usize {
    fn depth(index: usize, blocks: &[NifBlock], visiting: &mut Vec<usize>) -> usize {
        if visiting.contains(&index) {
            return 0;
        }
        let children = match blocks.get(index) {
            Some(NifBlock::NiNode(node) | NifBlock::BSFadeNode(node)) => &node.children,
            _ => return usize::from(index < blocks.len()),
        };
        visiting.push(index);
        let child_depth = children
            .iter()
            .filter_map(|child| usize::try_from(*child).ok())
            .map(|child| depth(child, blocks, visiting))
            .max()
            .unwrap_or(0);
        visiting.pop();
        1 + child_depth
    }

    (0..blocks.len())
        .map(|index| depth(index, blocks, &mut Vec::new()))
        .max()
        .unwrap_or(0)
}

fn parse_skyrim_header<'a>(bytes: &'a [u8], path: &Path) -> Result<(&'a [u8], NifHeader)> {
    let mut cursor = NifCursor::new(bytes, path);
    let file_desc = cursor.line()?;
    ensure!(
        file_desc.starts_with("Gamebryo File Format"),
        "unsupported NIF signature in {}",
        path.display()
    );
    let nif_version = cursor.u32()?;
    let endian_type = cursor.u8()?;
    ensure!(
        endian_type == 1,
        "big-endian NIF is not supported in {}",
        path.display()
    );
    let user_version = cursor.u32()?;
    let block_count = cursor.u32()?;
    ensure!(
        block_count <= 1_000_000,
        "NIF block count exceeds the safety limit in {}",
        path.display()
    );
    let bethesda_version = cursor.u32()?;
    let author = cursor.sized_string8_optional()?;
    let process_script = cursor.sized_string8_optional()?;
    let export_script = cursor.sized_string8_optional()?;
    let block_type_count = usize::from(cursor.u16()?);
    let mut block_types = Vec::with_capacity(block_type_count);
    for _ in 0..block_type_count {
        block_types.push(SizedString32(cursor.sized_string32()?));
    }
    let block_count_usize = usize::try_from(block_count).wrap_err("NIF block count overflow")?;
    let mut block_type_index = Vec::with_capacity(block_count_usize);
    for _ in 0..block_count_usize {
        block_type_index.push(cursor.u16()?);
    }
    let mut block_size_index = Vec::with_capacity(block_count_usize);
    for _ in 0..block_count_usize {
        block_size_index.push(cursor.u32()?);
    }
    let string_count = cursor.u32()?;
    ensure!(
        string_count <= 1_000_000,
        "NIF string count exceeds the safety limit in {}",
        path.display()
    );
    let string_max_size = cursor.u32()?;
    let mut strings = Vec::with_capacity(usize::try_from(string_count)?);
    for _ in 0..string_count {
        strings.push(SizedString32(cursor.sized_string32()?));
    }
    let group_count = cursor.u32()?;
    ensure!(
        group_count <= 1_000_000,
        "NIF group count exceeds the safety limit in {}",
        path.display()
    );
    let mut groups = Vec::with_capacity(usize::try_from(group_count)?);
    for _ in 0..group_count {
        groups.push(cursor.u32()?);
    }
    let remaining = &bytes[cursor.position..];
    Ok((
        remaining,
        NifHeader {
            file_desc: StringN { value: file_desc },
            nif_version: NifFileVersion(nif_version),
            endian_type: Endianess::Little,
            user_version,
            block_count,
            bethesda_version,
            author,
            process_script,
            export_script,
            max_filepath: None,
            block_types,
            block_type_index,
            block_size_index,
            string_count,
            string_max_size,
            strings,
            groups,
        },
    ))
}

struct NifCursor<'a> {
    bytes: &'a [u8],
    position: usize,
    path: &'a Path,
}

impl<'a> NifCursor<'a> {
    fn new(bytes: &'a [u8], path: &'a Path) -> Self {
        Self {
            bytes,
            position: 0,
            path,
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| color_eyre::eyre::eyre!("NIF offset overflow"))?;
        ensure!(
            end <= self.bytes.len(),
            "truncated NIF header at byte {} in {}",
            self.position,
            self.path.display()
        );
        let result = &self.bytes[self.position..end];
        self.position = end;
        Ok(result)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn line(&mut self) -> Result<String> {
        let Some(length) = self.bytes[self.position..]
            .iter()
            .position(|byte| *byte == b'\n')
        else {
            color_eyre::eyre::bail!(
                "NIF header line is not terminated in {}",
                self.path.display()
            );
        };
        let value = String::from_utf8_lossy(self.take(length)?).into_owned();
        self.take(1)?;
        Ok(value)
    }

    fn sized_string8_optional(&mut self) -> Result<Option<SizedString8>> {
        let length = usize::from(self.u8()?);
        let value = String::from_utf8_lossy(self.take(length)?)
            .trim_end_matches('\0')
            .to_owned();
        Ok((!value.is_empty()).then_some(SizedString8(value)))
    }

    fn sized_string32(&mut self) -> Result<String> {
        let length = usize::try_from(self.u32()?).wrap_err("NIF string length overflow")?;
        ensure!(
            length <= 16 * 1024 * 1024,
            "NIF string exceeds the safety limit in {}",
            self.path.display()
        );
        Ok(String::from_utf8_lossy(self.take(length)?)
            .trim_end_matches('\0')
            .to_owned())
    }
}

fn glb_bounds_from_bytes(glb: &[u8]) -> Result<shared::Bounds3> {
    ensure!(
        glb.len() >= 20 && &glb[..4] == b"glTF",
        "invalid GLB container"
    );
    let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    ensure!(&glb[16..20] == b"JSON", "GLB JSON chunk is missing");
    let document: serde_json::Value = serde_json::from_slice(
        glb.get(20..20 + json_length)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?,
    )?;
    let nodes = document
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let roots = scene_roots(&document, nodes);
    let mut bounds = BoundsAccumulator::default();
    for root in roots {
        visit_node(&document, nodes, root, Mat4::IDENTITY, 0, &mut bounds)?;
    }
    bounds.finish()
}

fn scene_roots(document: &serde_json::Value, nodes: &[serde_json::Value]) -> Vec<usize> {
    if let Some(scenes) = document.get("scenes").and_then(serde_json::Value::as_array) {
        let scene_index = document
            .get("scene")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        if let Some(scene_nodes) = scenes
            .get(scene_index)
            .and_then(|scene| scene.get("nodes"))
            .and_then(serde_json::Value::as_array)
        {
            return scene_nodes
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|index| index as usize)
                .collect();
        }
    }
    let mut children = vec![false; nodes.len()];
    for node in nodes {
        if let Some(indices) = node.get("children").and_then(serde_json::Value::as_array) {
            for index in indices.iter().filter_map(serde_json::Value::as_u64) {
                if let Some(child) = children.get_mut(index as usize) {
                    *child = true;
                }
            }
        }
    }
    children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| (!child).then_some(index))
        .collect()
}

fn visit_node(
    document: &serde_json::Value,
    nodes: &[serde_json::Value],
    index: usize,
    parent: Mat4,
    depth: usize,
    bounds: &mut BoundsAccumulator,
) -> Result<()> {
    ensure!(depth <= nodes.len(), "cyclic glTF node hierarchy");
    let node = nodes
        .get(index)
        .ok_or_else(|| color_eyre::eyre::eyre!("glTF node {index} is out of range"))?;
    let transform = parent.mul(Mat4::from_node(node));
    if let Some(mesh_index) = node.get("mesh").and_then(serde_json::Value::as_u64) {
        accumulate_mesh(document, mesh_index as usize, transform, bounds)?;
    }
    if let Some(children) = node.get("children").and_then(serde_json::Value::as_array) {
        for child in children.iter().filter_map(serde_json::Value::as_u64) {
            visit_node(
                document,
                nodes,
                child as usize,
                transform,
                depth + 1,
                bounds,
            )?;
        }
    }
    Ok(())
}

fn accumulate_mesh(
    document: &serde_json::Value,
    mesh_index: usize,
    transform: Mat4,
    output: &mut BoundsAccumulator,
) -> Result<()> {
    let meshes = document
        .get("meshes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB has no meshes"))?;
    let accessors = document
        .get("accessors")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB has no accessors"))?;
    let primitives = meshes
        .get(mesh_index)
        .and_then(|mesh| mesh.get("primitives"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("mesh {mesh_index} has no primitives"))?;
    for primitive in primitives {
        let Some(accessor_index) = primitive
            .pointer("/attributes/POSITION")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let accessor = accessors
            .get(accessor_index as usize)
            .ok_or_else(|| color_eyre::eyre::eyre!("POSITION accessor is out of range"))?;
        let min = json_vec3(accessor.get("min"))?;
        let max = json_vec3(accessor.get("max"))?;
        for x in [min[0], max[0]] {
            for y in [min[1], max[1]] {
                for z in [min[2], max[2]] {
                    output.include(transform.transform([x, y, z]));
                }
            }
        }
    }
    Ok(())
}

fn json_vec3(value: Option<&serde_json::Value>) -> Result<[f64; 3]> {
    let values = value
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("POSITION accessor has no min/max"))?;
    ensure!(values.len() >= 3, "POSITION accessor min/max is not a vec3");
    Ok([
        values[0]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
        values[1]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
        values[2]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
    ])
}

#[derive(Clone, Copy)]
struct Mat4([f64; 16]);

impl Mat4 {
    const IDENTITY: Self = Self([
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]);

    fn from_node(node: &serde_json::Value) -> Self {
        if let Some(matrix) = node.get("matrix").and_then(serde_json::Value::as_array)
            && matrix.len() == 16
        {
            let mut output = [0.0; 16];
            for (target, source) in output.iter_mut().zip(matrix) {
                *target = source.as_f64().unwrap_or(0.0);
            }
            return Self(output);
        }
        let t = array_or(node.get("translation"), [0.0, 0.0, 0.0]);
        let r = array_or(node.get("rotation"), [0.0, 0.0, 0.0, 1.0]);
        let s = array_or(node.get("scale"), [1.0, 1.0, 1.0]);
        let [x, y, z, w] = r;
        let mut matrix = [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + z * w),
            2.0 * (x * z - y * w),
            0.0,
            2.0 * (x * y - z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + x * w),
            0.0,
            2.0 * (x * z + y * w),
            2.0 * (y * z - x * w),
            1.0 - 2.0 * (x * x + y * y),
            0.0,
            t[0],
            t[1],
            t[2],
            1.0,
        ];
        for row in 0..4 {
            matrix[row] *= s[0];
            matrix[4 + row] *= s[1];
            matrix[8 + row] *= s[2];
        }
        Self(matrix)
    }

    fn mul(self, rhs: Self) -> Self {
        let mut output = [0.0; 16];
        for column in 0..4 {
            for row in 0..4 {
                output[column * 4 + row] = (0..4)
                    .map(|axis| self.0[axis * 4 + row] * rhs.0[column * 4 + axis])
                    .sum();
            }
        }
        Self(output)
    }

    fn transform(self, point: [f64; 3]) -> [f64; 3] {
        [
            self.0[0] * point[0] + self.0[4] * point[1] + self.0[8] * point[2] + self.0[12],
            self.0[1] * point[0] + self.0[5] * point[1] + self.0[9] * point[2] + self.0[13],
            self.0[2] * point[0] + self.0[6] * point[1] + self.0[10] * point[2] + self.0[14],
        ]
    }
}

fn array_or<const N: usize>(value: Option<&serde_json::Value>, fallback: [f64; N]) -> [f64; N] {
    let Some(values) = value.and_then(serde_json::Value::as_array) else {
        return fallback;
    };
    std::array::from_fn(|index| {
        values
            .get(index)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(fallback[index])
    })
}

#[derive(Default)]
struct BoundsAccumulator {
    min: [f64; 3],
    max: [f64; 3],
    populated: bool,
}

impl BoundsAccumulator {
    fn include(&mut self, point: [f64; 3]) {
        if !self.populated {
            self.min = point;
            self.max = point;
            self.populated = true;
        } else {
            for (axis, value) in point.into_iter().enumerate() {
                self.min[axis] = self.min[axis].min(value);
                self.max[axis] = self.max[axis].max(value);
            }
        }
    }

    fn finish(self) -> Result<shared::Bounds3> {
        ensure!(self.populated, "GLB contains no bounded POSITION accessor");
        let bounds = shared::Bounds3 {
            min: self.min.map(|value| value as f32),
            max: self.max.map(|value| value as f32),
        };
        ensure!(
            bounds.is_finite_and_ordered(),
            "GLB contains invalid bounds"
        );
        Ok(bounds)
    }
}

fn rewrite_materials_and_texture_uris(
    glb: Vec<u8>,
    material_contract: &[NifShapeMaterial],
    shape_blocks: &[u32],
    glb_output_path: &Path,
) -> Result<Vec<u8>> {
    ensure!(
        glb.len() >= 20 && &glb[..4] == b"glTF",
        "invalid GLB container"
    );
    let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    ensure!(&glb[16..20] == b"JSON", "GLB JSON chunk is missing");
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let json_bytes = glb
        .get(20..json_end)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    let mut document: serde_json::Value =
        serde_json::from_slice(json_bytes).wrap_err("NIF exporter produced invalid glTF JSON")?;
    publish_gltf_materials(
        &mut document,
        material_contract,
        shape_blocks,
        glb_output_path,
    )?;
    let mut json = serde_json::to_vec(&document)?;
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let suffix = glb
        .get(json_end..)
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid GLB suffix"))?;
    let total_length = 20usize
        .checked_add(json.len())
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB size overflow"))?;
    let mut output = Vec::with_capacity(total_length);
    output.extend_from_slice(&glb[..8]);
    let total_length = u32::try_from(total_length).wrap_err("GLB exceeds 4 GiB")?;
    let json_length = u32::try_from(json.len()).wrap_err("GLB JSON exceeds 4 GiB")?;
    output.extend_from_slice(&total_length.to_le_bytes());
    output.extend_from_slice(&json_length.to_le_bytes());
    output.extend_from_slice(b"JSON");
    output.extend_from_slice(&json);
    output.extend_from_slice(suffix);
    Ok(output)
}

fn texture_dependencies(document: &serde_json::Value) -> Vec<TextureDependency> {
    let mut dependencies = BTreeMap::<(String, TextureSemantic), TextureDependency>::new();
    if let Some(materials) = document
        .get("materials")
        .and_then(serde_json::Value::as_array)
    {
        for material in materials {
            for (pointer, semantic, required) in [
                (
                    "/pbrMetallicRoughness/baseColorTexture/index",
                    TextureSemantic::BaseColor,
                    true,
                ),
                ("/normalTexture/index", TextureSemantic::Normal, false),
                ("/emissiveTexture/index", TextureSemantic::Emissive, false),
                (
                    "/pbrMetallicRoughness/metallicRoughnessTexture/index",
                    TextureSemantic::MetallicRoughness,
                    false,
                ),
                ("/occlusionTexture/index", TextureSemantic::Occlusion, false),
                (
                    "/extensions/KHR_materials_pbrSpecularGlossiness/diffuseTexture/index",
                    TextureSemantic::BaseColor,
                    true,
                ),
                (
                    "/extensions/KHR_materials_pbrSpecularGlossiness/specularGlossinessTexture/index",
                    TextureSemantic::SpecularGlossiness,
                    false,
                ),
                (
                    "/extensions/KHR_materials_specular/specularColorTexture/index",
                    TextureSemantic::SpecularGlossiness,
                    false,
                ),
            ] {
                if let Some(index) = material
                    .pointer(pointer)
                    .and_then(serde_json::Value::as_u64)
                    && let Some(uri) = texture_uri(document, index as usize)
                {
                    dependencies.insert(
                        (uri.to_ascii_lowercase(), semantic),
                        TextureDependency {
                            uri: uri.to_owned(),
                            semantic,
                            required,
                        },
                    );
                }
            }
            if let Some(slots) = material
                .pointer("/extensions/OPEN_SKYRIM_material/textureSlots")
                .and_then(serde_json::Value::as_array)
            {
                for slot in slots {
                    let Some(index) = slot.get("texture").and_then(serde_json::Value::as_u64)
                    else {
                        continue;
                    };
                    let Some(uri) = texture_uri(document, index as usize) else {
                        continue;
                    };
                    let semantic = match slot.get("semantic").and_then(serde_json::Value::as_str) {
                        Some("height") => TextureSemantic::Height,
                        Some("detail") => TextureSemantic::Detail,
                        Some("environment_cube") => TextureSemantic::EnvironmentCube,
                        Some("environment_mask") => TextureSemantic::EnvironmentMask,
                        Some("inner_layer") => TextureSemantic::InnerLayer,
                        Some("greyscale") => TextureSemantic::Greyscale,
                        _ => TextureSemantic::Unclassified,
                    };
                    let required = slot
                        .get("required")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    dependencies.insert(
                        (uri.to_ascii_lowercase(), semantic),
                        TextureDependency {
                            uri: uri.to_owned(),
                            semantic,
                            required,
                        },
                    );
                }
            }
        }
    }
    if let Some(images) = document.get("images").and_then(serde_json::Value::as_array) {
        for uri in images
            .iter()
            .filter_map(|image| image.get("uri").and_then(serde_json::Value::as_str))
        {
            if !dependencies
                .keys()
                .any(|(known, _)| known.eq_ignore_ascii_case(uri))
            {
                dependencies.insert(
                    (uri.to_ascii_lowercase(), TextureSemantic::Unclassified),
                    TextureDependency {
                        uri: uri.to_owned(),
                        semantic: TextureSemantic::Unclassified,
                        required: false,
                    },
                );
            }
        }
    }
    dependencies.into_values().collect()
}

fn texture_uri(document: &serde_json::Value, texture_index: usize) -> Option<&str> {
    let image_index = document
        .get("textures")?
        .get(texture_index)?
        .get("source")?
        .as_u64()? as usize;
    document
        .get("images")?
        .get(image_index)?
        .get("uri")?
        .as_str()
}

fn find_skeleton(nif_path: &Path) -> Option<PathBuf> {
    let parent = nif_path.parent()?;
    let mut candidates = vec![
        parent.join("skeleton.nif"),
        parent.join("skeleton_female.nif"),
        parent.join("character assets").join("skeleton.nif"),
    ];
    if let Some(grandparent) = parent.parent() {
        candidates.extend([
            grandparent.join("skeleton.nif"),
            grandparent.join("character assets").join("skeleton.nif"),
        ]);
    }
    if let Some((actors_root, actor_name)) = actor_root(nif_path) {
        let actor_dir = actors_root.join(actor_name);
        candidates.extend([
            actor_dir.join("character assets").join("skeleton.nif"),
            actor_dir
                .join("character assets female")
                .join("skeleton_female.nif"),
        ]);
        if let Some(found) = WalkDir::new(&actor_dir)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.into_path())
            .find(|path| {
                path.is_file()
                    && path.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .to_ascii_lowercase()
                            .starts_with("skeleton")
                            && path.extension().is_some_and(|ext| {
                                ext.to_string_lossy().eq_ignore_ascii_case("nif")
                            })
                    })
            })
        {
            candidates.push(found);
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn actor_root(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let components: Vec<_> = path.components().collect();
    let index = components.iter().position(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("actors")
    })?;
    let actor = components.get(index + 1)?.as_os_str();
    let mut root = PathBuf::new();
    for component in &components[..=index] {
        root.push(component.as_os_str());
    }
    Some((root, PathBuf::from(actor)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_nif_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("bad.nif");
        let output = dir.path().join("bad.glb");
        fs::write(&input, b"not a nif").unwrap();
        assert!(MeshConverter::convert_nif_to_glb(&input, &output).is_err());
        assert!(!output.exists());
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_NIF_FIXTURE with a locally installed Skyrim NIF"]
    fn converts_installed_non_renderable_nif_to_empty_scene() {
        let path = std::env::var_os("OPENSKYRIM_NIF_FIXTURE")
            .map(PathBuf::from)
            .expect("set OPENSKYRIM_NIF_FIXTURE to a Skyrim NIF");
        let diagnostics = MeshConverter::inspect_nif(&path).unwrap();
        assert_eq!(diagnostics.geometry_block_count, 0);
        assert!(
            !diagnostics
                .block_types
                .keys()
                .any(|block_type| is_declared_geometry_block(block_type))
        );
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("non-renderable.glb");
        MeshConverter::convert_nif_to_glb(&path, &output).unwrap();
        let document = glb_json_from_bytes(&fs::read(output).unwrap()).unwrap();
        assert_eq!(document["scenes"][0]["nodes"], serde_json::json!([]));
        assert!(document.get("meshes").is_none());
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_STATIC_NIF_FIXTURE with a locally installed Skyrim NIF"]
    fn static_fallback_converts_installed_nif_fixture() {
        let path = std::env::var_os("OPENSKYRIM_STATIC_NIF_FIXTURE")
            .map(PathBuf::from)
            .expect("set OPENSKYRIM_STATIC_NIF_FIXTURE to a Skyrim NIF");
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("static-fallback.glb");
        MeshConverter::convert_nif_to_glb(&path, &output).unwrap();
        MeshConverter::glb_bounds(&output).unwrap();
    }

    #[test]
    fn derives_actor_root_case_insensitively() {
        let (root, actor) = actor_root(Path::new(
            "vfs/Meshes/Actors/Dragon/character assets/dragon.nif",
        ))
        .unwrap();
        assert_eq!(root, PathBuf::from("vfs/Meshes/Actors"));
        assert_eq!(actor, PathBuf::from("Dragon"));
    }

    #[test]
    fn defers_only_dynamic_runtime_geometry() {
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/Meshes/Actors/Character/FaceGenData/FaceGeom/Skyrim.esm/00045CB1.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/actors/character/character assets/hair/elf/female/hair03.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/magic/lightningbolt01.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/effects/fxemptycontroller.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/creationclub/vsvsse003/actors/character/facegendata/facegeom/mod.esp/0000091a.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/creationclub/bgssse067/actors/ccbgssse067_deadrichorse.nif"
        )));
        assert!(!is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/architecture/whiterun/wrwall.nif"
        )));
        assert!(!is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/creationclub/bgssse001/clutter/tent.nif"
        )));
    }

    #[test]
    fn rewrites_glb_material_chunk_without_corrupting_the_container() {
        let mut json = br#"{"asset":{"version":"2.0"},"meshes":[{"primitives":[{}]}]}"#.to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let contract = [NifShapeMaterial {
            shape_block: 7,
            shape_name: Some("excluded".to_owned()),
            shader_property_block: None,
            alpha_property_block: None,
            disposition: NifMaterialDisposition::Excluded {
                reason: "fixture".to_owned(),
            },
        }];
        let rewritten =
            rewrite_materials_and_texture_uris(glb, &contract, &[7], Path::new("meshes/a.glb"))
                .unwrap();
        assert_eq!(
            u32::from_le_bytes(rewritten[8..12].try_into().unwrap()) as usize,
            rewritten.len()
        );
        let length = u32::from_le_bytes(rewritten[12..16].try_into().unwrap()) as usize;
        let document: serde_json::Value =
            serde_json::from_slice(&rewritten[20..20 + length]).unwrap();
        assert_eq!(document["materials"][0]["alphaMode"], "MASK");
        assert_eq!(document["meshes"][0]["primitives"][0]["material"], 0);
        assert_eq!(
            document["meshes"][0]["primitives"][0]["extras"]["openSkyrim"]["shapeBlock"],
            7
        );
    }

    #[test]
    fn extracts_bounds_with_node_transform() {
        let mut json = br#"{
            "asset":{"version":"2.0"},
            "scene":0,
            "scenes":[{"nodes":[0]}],
            "nodes":[{"mesh":0,"translation":[10,20,30],"scale":[2,3,4]}],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],
            "accessors":[{"min":[-1,-2,-3],"max":[1,2,3]}]
        }"#
        .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let bounds = glb_bounds_from_bytes(&glb).unwrap();
        assert_eq!(bounds.min, [8.0, 14.0, 18.0]);
        assert_eq!(bounds.max, [12.0, 26.0, 42.0]);
    }

    #[test]
    fn extracts_bounds_through_rotated_non_uniform_hierarchy() {
        let half_sqrt = std::f64::consts::FRAC_1_SQRT_2;
        let mut json = format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "scene":0,
                "scenes":[{{"nodes":[0]}}],
                "nodes":[
                    {{"children":[1],"translation":[10,0,0],"rotation":[0,0,{half_sqrt},{half_sqrt}],"scale":[2,1,1]}},
                    {{"mesh":0,"translation":[1,2,0],"rotation":[{half_sqrt},0,0,{half_sqrt}],"scale":[1,3,2]}}
                ],
                "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
                "accessors":[{{"min":[-1,-1,-1],"max":[1,1,1]}}]
            }}"#
        )
        .into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);

        let bounds = glb_bounds_from_bytes(&glb).unwrap();
        for (actual, expected) in bounds.min.into_iter().zip([6.0, 0.0, -3.0]) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
        for (actual, expected) in bounds.max.into_iter().zip([10.0, 4.0, 3.0]) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
    }

    #[test]
    fn lists_external_glb_textures_deterministically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mesh.glb");
        let mut json = br#"{
            "asset":{"version":"2.0"},
            "images":[
                {"uri":"../textures/B.ktx2"},
                {"bufferView":0,"mimeType":"image/png"},
                {"uri":"../textures/a.ktx2"},
                {"uri":"../textures/A.ktx2"}
            ]
        }"#
        .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        fs::write(&path, glb).unwrap();

        assert_eq!(
            MeshConverter::glb_texture_uris(&path).unwrap(),
            vec!["../textures/a.ktx2", "../textures/B.ktx2"]
        );
    }

    #[test]
    fn classifies_required_and_optional_texture_dependencies() {
        let document = serde_json::json!({
            "images": [
                {"uri": "../textures/diffuse.ktx2"},
                {"uri": "../textures/normal.ktx2"}
            ],
            "textures": [{"source": 0}, {"source": 1}],
            "materials": [{
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}},
                "normalTexture": {"index": 1}
            }]
        });
        let dependencies = texture_dependencies(&document);
        assert_eq!(dependencies.len(), 2);
        assert!(dependencies.iter().any(|dependency| {
            dependency.semantic == TextureSemantic::BaseColor && dependency.required
        }));
        assert!(dependencies.iter().any(|dependency| {
            dependency.semantic == TextureSemantic::Normal && !dependency.required
        }));
    }

    fn glb_bytes(document: &serde_json::Value, binary: &[u8]) -> Vec<u8> {
        let mut json = serde_json::to_vec(document).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut padded = binary.to_vec();
        while !padded.len().is_multiple_of(4) {
            padded.push(0);
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((20 + json.len() + 8 + padded.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(padded.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&padded);
        glb
    }

    fn write_prune_fixture(root: &Path, document: &serde_json::Value) -> PathBuf {
        fs::create_dir_all(root.join("meshes/a")).unwrap();
        fs::create_dir_all(root.join("textures")).unwrap();
        fs::write(root.join("textures/keep.ktx2"), b"ktx2").unwrap();
        let glb = root.join("meshes/a/model.glb");
        fs::write(&glb, glb_bytes(document, b"\x01\x02\x03\x04\x05")).unwrap();
        glb
    }

    #[test]
    fn prunes_missing_images_and_remaps_dependents() {
        let document = serde_json::json!({
            "asset": {"version": "2.0"},
            "images": [
                {"uri": "../../textures/keep.ktx2"},
                {"uri": "../../textures/gone.ktx2"},
                {"bufferView": 0, "mimeType": "image/png"}
            ],
            "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
            "materials": [{
                "pbrMetallicRoughness": {
                    "baseColorTexture": {"index": 0},
                    "metallicRoughnessTexture": {"index": 1}
                },
                "normalTexture": {"index": 1},
                "emissiveTexture": {"index": 2},
                "extensions": {"OPEN_SKYRIM_material": {"textureSlots": [
                    {"slot": "glow", "texture": 1},
                    {"slot": "detail", "texture": 2}
                ]}}
            }]
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let glb = write_prune_fixture(root, &document);
        let before = fs::read(&glb).unwrap();

        let report = MeshConverter::prune_dangling_texture_uris(root).unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].glb, "meshes/a/model.glb");
        assert_eq!(
            report[0].removed_uris,
            vec!["../../textures/gone.ktx2".to_owned()]
        );

        let bytes = fs::read(&glb).unwrap();
        let pruned = glb_json_from_bytes(&bytes).unwrap();
        assert_eq!(pruned["images"].as_array().unwrap().len(), 2);
        assert_eq!(pruned["images"][0]["uri"], "../../textures/keep.ktx2");
        assert!(pruned["images"][1].get("uri").is_none());
        assert_eq!(pruned["textures"].as_array().unwrap().len(), 2);
        assert_eq!(pruned["textures"][0]["source"], 0);
        assert_eq!(pruned["textures"][1]["source"], 1);
        let material = &pruned["materials"][0];
        assert_eq!(
            material["pbrMetallicRoughness"]["baseColorTexture"]["index"],
            0
        );
        assert!(
            material["pbrMetallicRoughness"]
                .get("metallicRoughnessTexture")
                .is_none()
        );
        assert!(material.get("normalTexture").is_none());
        assert_eq!(material["emissiveTexture"]["index"], 1);
        assert_eq!(
            material["extensions"]["OPEN_SKYRIM_material"]["textureSlots"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            material["extensions"]["OPEN_SKYRIM_material"]["textureSlots"][0]["texture"],
            1
        );
        // The binary chunk survives byte-identical.
        let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let before_json_length = u32::from_le_bytes(before[12..16].try_into().unwrap()) as usize;
        assert_eq!(
            &bytes[20 + json_length..],
            &before[20 + before_json_length..]
        );

        // Pruning is idempotent: a second pass rewrites nothing.
        assert!(
            MeshConverter::prune_dangling_texture_uris(root)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&glb).unwrap(), bytes);
    }

    #[test]
    fn leaves_clean_trees_untouched() {
        let document = serde_json::json!({
            "asset": {"version": "2.0"},
            "images": [{"uri": "../../textures/keep.ktx2"}],
            "textures": [{"source": 0}],
            "materials": [{
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}
            }]
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let glb = write_prune_fixture(root, &document);
        let before = fs::read(&glb).unwrap();
        assert!(
            MeshConverter::prune_dangling_texture_uris(root)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&glb).unwrap(), before);
    }

    #[test]
    fn keeps_embedded_and_remote_uris() {
        let document = serde_json::json!({
            "asset": {"version": "2.0"},
            "images": [
                {"uri": "data:image/png;base64,iVBORw0KGgo="},
                {"uri": "https://example.com/textures/remote.ktx2"}
            ],
            "textures": [{"source": 0}, {"source": 1}]
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let glb = write_prune_fixture(root, &document);
        let before = fs::read(&glb).unwrap();
        assert!(
            MeshConverter::prune_dangling_texture_uris(root)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&glb).unwrap(), before);
    }

    #[test]
    fn cutout_shapes_get_opaque_vertex_alpha() {
        use crate::material::{LightingShaderType, NifShaderFamily, ValidatedNifMaterial};
        use project_wormhole_nif::model::all::{Model, StaticMesh, StaticSceneNode};
        use project_wormhole_shared::glam::{Mat3, Vec3, Vec4};
        use project_wormhole_shared::prelude::BSVec4;

        fn node(block_index: u32, mesh: usize) -> StaticSceneNode {
            StaticSceneNode {
                block_index,
                name: None,
                translation: Vec3::ZERO,
                rotation: Mat3::IDENTITY,
                scale: 1.0,
                children: Vec::new(),
                mesh: Some(mesh),
            }
        }

        fn shape(block: u32, alpha_mode: NifAlphaMode) -> NifShapeMaterial {
            NifShapeMaterial {
                shape_block: block,
                shape_name: None,
                shader_property_block: None,
                alpha_property_block: None,
                disposition: NifMaterialDisposition::Validated {
                    material: ValidatedNifMaterial {
                        shader_family: NifShaderFamily::Lighting,
                        lighting_shader_type: Some(LightingShaderType::Default),
                        shader_block: 0,
                        texture_set_block: None,
                        alpha_property_block: None,
                        shader_flags_1: 0,
                        shader_flags_2: 0,
                        base_color: [1.0, 1.0, 1.0, 1.0],
                        alpha: 1.0,
                        alpha_mode,
                        alpha_threshold: Some(112),
                        glossiness: 0.0,
                        specular_color: [0.0, 0.0, 0.0],
                        specular_strength: 0.0,
                        emissive_color: [0.0, 0.0, 0.0],
                        emissive_multiple: 1.0,
                        double_sided: true,
                        textures: Vec::new(),
                    },
                },
            }
        }

        let mut model = Model {
            name: None,
            static_meshes: vec![
                StaticMesh {
                    colors: vec![
                        BSVec4(Vec4::new(1.0, 0.5, 0.25, 0.0)),
                        BSVec4(Vec4::new(0.0, 1.0, 0.5, 0.44)),
                    ],
                    ..StaticMesh::default()
                },
                StaticMesh {
                    colors: vec![BSVec4(Vec4::new(1.0, 1.0, 1.0, 0.0))],
                    ..StaticMesh::default()
                },
            ],
            static_nodes: vec![node(7, 0), node(9, 1)],
            skeletal_meshes: Vec::new(),
            materials: Vec::new(),
            material_indices: Vec::new(),
            scene_root_rotation: None,
        };
        let nif = NifFile {
            header: NifHeader {
                file_desc: StringN {
                    value: String::new(),
                },
                nif_version: NifFileVersion(0),
                endian_type: Endianess::Little,
                user_version: 0,
                block_count: 0,
                bethesda_version: 0,
                author: None,
                process_script: None,
                export_script: None,
                max_filepath: None,
                block_types: Vec::new(),
                block_type_index: Vec::new(),
                block_size_index: Vec::new(),
                string_count: 0,
                string_max_size: 0,
                strings: Vec::new(),
                groups: Vec::new(),
            },
            blocks: Vec::new(),
        };
        let contract = vec![
            shape(7, NifAlphaMode::Cutout),
            shape(9, NifAlphaMode::Opaque),
        ];

        normalize_cutout_vertex_alpha(&mut model, &nif, &contract).unwrap();

        let cutout = &model.static_meshes[0].colors;
        assert_eq!(cutout.len(), 2);
        for color in cutout {
            assert_eq!(color.0.w, 1.0);
        }
        assert_eq!(cutout[0].0.x, 1.0);
        assert_eq!(cutout[0].0.y, 0.5);
        assert_eq!(cutout[0].0.z, 0.25);
        let opaque = &model.static_meshes[1].colors;
        assert_eq!(opaque.len(), 1);
        assert_eq!(opaque[0].0.w, 0.0);
    }
}
