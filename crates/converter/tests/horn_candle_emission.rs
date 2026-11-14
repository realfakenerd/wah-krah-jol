//! Optional retail trace for #469. Outputs belong in task-owned shared storage.
use converter::{
    archive::ArchiveExtractor,
    material::{LightingShaderType, NifMaterialDisposition, NifShaderFamily, NifTextureSemantic},
    mesh::MeshConverter,
};
use std::{collections::BTreeSet, fs, path::PathBuf};

#[test]
#[ignore = "requires MUDCRAB_RETAIL_DATA and a new MUDCRAB_HORN_TRACE output directory"]
fn retail_horn_candle_emission_trace() {
    let data =
        PathBuf::from(std::env::var_os("MUDCRAB_RETAIL_DATA").expect("retail Data directory"));
    let output =
        PathBuf::from(std::env::var_os("MUDCRAB_HORN_TRACE").expect("new trace directory"));
    assert!(
        !output.exists(),
        "preserve earlier evidence; use a new output"
    );
    fs::create_dir_all(&output).unwrap();
    let names = [
        "candlehornchandelier01",
        "candlehornchandelier02",
        "candlehornfloor01",
        "candlehorntable01",
        "candlehornwall01",
    ];
    let paths: BTreeSet<_> = names
        .iter()
        .map(|name| PathBuf::from(format!("meshes/clutter/candles/{name}.nif")))
        .collect();
    for archive in ["Skyrim - Meshes0.bsa", "Skyrim - Meshes1.bsa"] {
        ArchiveExtractor::extract_paths(&data.join(archive), &output.join("source"), &paths)
            .unwrap();
    }
    let mut trace = Vec::new();
    let mut shapes = 0;
    for path in paths {
        let source = output.join("source").join(&path);
        let contract = MeshConverter::inspect_nif_materials(&source).unwrap();
        for shape in &contract {
            let NifMaterialDisposition::Validated { material } = &shape.disposition else {
                continue;
            };
            shapes += 1;
            assert_eq!(material.shader_family, NifShaderFamily::Lighting);
            assert_eq!(
                material.lighting_shader_type,
                Some(LightingShaderType::Default)
            );
            assert_ne!(material.shader_flags_1 & (1 << 22), 0, "Own_Emit");
            assert_eq!(material.shader_flags_2 & (1 << 6), 0, "Glow_Map");
            assert_eq!(material.emissive_multiple, 1.0);
            for (actual, expected) in material.emissive_color.into_iter().zip([
                127.0 / 255.0,
                104.0 / 255.0,
                44.0 / 255.0,
            ]) {
                assert!((actual - expected).abs() < 1e-6);
            }
            assert!(
                material
                    .textures
                    .iter()
                    .any(|slot| slot.semantic == NifTextureSemantic::Diffuse)
            );
            assert!(
                !material
                    .textures
                    .iter()
                    .any(|slot| slot.semantic == NifTextureSemantic::Glow)
            );
        }
        let glb = output.join("converted").join(path.with_extension("glb"));
        fs::create_dir_all(glb.parent().unwrap()).unwrap();
        MeshConverter::convert_nif_to_glb(&source, &glb).unwrap();
        trace.push(serde_json::json!({"source":source,"converted":glb,"materials":contract}));
    }
    assert_eq!(shapes, 9);
    fs::write(
        output.join("nif-materials.json"),
        serde_json::to_vec_pretty(&trace).unwrap(),
    )
    .unwrap();
}

/// Comparison candidates from the expanded #469 report, not identified placed references.
#[test]
#[ignore = "requires MUDCRAB_RETAIL_DATA and a new MUDCRAB_STATION_TRACE output directory"]
fn retail_station_emission_comparison() {
    let data =
        PathBuf::from(std::env::var_os("MUDCRAB_RETAIL_DATA").expect("retail Data directory"));
    let output =
        PathBuf::from(std::env::var_os("MUDCRAB_STATION_TRACE").expect("new trace directory"));
    assert!(
        !output.exists(),
        "preserve earlier evidence; use a new output"
    );
    fs::create_dir_all(&output).unwrap();
    let paths: BTreeSet<_> = [
        "meshes/furniture/alchemyworkbench.nif",
        "meshes/furniture/enchantingworkbench.nif",
        "meshes/furniture/workbenches/alchemyworkbench01.nif",
        "meshes/furniture/workbenches/enchantingworkbench01.nif",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    for archive in ["Skyrim - Meshes0.bsa", "Skyrim - Meshes1.bsa"] {
        ArchiveExtractor::extract_paths(&data.join(archive), &output.join("source"), &paths)
            .unwrap();
    }
    let mut trace = Vec::new();
    let mut luminous_liquids = 0;
    let mut glow_runes = 0;
    let mut effects = 0;
    for path in paths {
        let source = output.join("source").join(&path);
        let contract = MeshConverter::inspect_nif_materials(&source).unwrap();
        let glb = output.join("converted").join(path.with_extension("glb"));
        fs::create_dir_all(glb.parent().unwrap()).unwrap();
        MeshConverter::convert_nif_to_glb(&source, &glb).unwrap();
        let bytes = fs::read(&glb).unwrap();
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let document: serde_json::Value =
            serde_json::from_slice(&bytes[20..20 + json_len]).unwrap();
        for shape in &contract {
            let NifMaterialDisposition::Validated { material } = &shape.disposition else {
                continue;
            };
            let published = document["materials"]
                .as_array()
                .unwrap()
                .iter()
                .find(|published| {
                    published["extras"]["openSkyrim"]["shapeBlock"].as_u64()
                        == Some(u64::from(shape.shape_block))
                })
                .unwrap();
            let strength =
                published["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"]
                    .as_f64()
                    .unwrap_or(1.0);
            for (channel, tint) in material.emissive_color.iter().enumerate() {
                let energy =
                    published["emissiveFactor"][channel].as_f64().unwrap_or(0.0) * strength;
                assert!(
                    (energy - f64::from(tint.max(0.0) * material.emissive_multiple.max(0.0))).abs()
                        < 1e-5
                );
            }
            if shape
                .shape_name
                .as_deref()
                .is_some_and(|name| name.starts_with("Liquid"))
            {
                assert_eq!(
                    material.lighting_shader_type,
                    Some(LightingShaderType::EnvironmentMap)
                );
                assert!(material.emissive_multiple > 0.0);
                luminous_liquids += 1;
            }
            if material.lighting_shader_type == Some(LightingShaderType::Glow) {
                assert!(
                    published.get("emissiveTexture").is_some(),
                    "preserve authored rune glow map"
                );
                glow_runes += 1;
            }
            if material.shader_family == NifShaderFamily::Effect {
                effects += 1;
            }
        }
        trace.push(serde_json::json!({"source":source,"converted":glb,"materials":contract}));
    }
    assert_eq!(luminous_liquids, 4);
    assert_eq!(glow_runes, 2);
    assert!(effects >= 6);
    fs::write(
        output.join("nif-materials.json"),
        serde_json::to_vec_pretty(&trace).unwrap(),
    )
    .unwrap();
}
