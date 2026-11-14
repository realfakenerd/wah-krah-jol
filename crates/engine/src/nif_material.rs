//! Native construction of converted NIF materials using Bevy's glTF extension hooks.
use crate::{
    nif_depth::{NifDepthExtension, NifDepthMaterial, NifDepthState, depth_material},
    skin_material::{
        FacegenMaps, SkinTintSurface, SkyrimSkinDepthMaterial, SkyrimSkinImages,
        SkyrimSkinMaterial, facegen_texture_indices, gltf_sampler, hair_material, is_skin_tint,
        load_linear_twin, nif_hair_tint, skin_material, specular_color_texture_index,
    },
};
use ::gltf::material::AlphaMode as GltfAlphaMode;
use bevy::{
    asset::{LoadContext, embedded_asset},
    gltf::{
        DefaultGltfImageSampler, GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
        gltf,
    },
    image::ImageSamplerDescriptor,
    light::NotShadowCaster,
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::{ShaderRef, load_shader_library},
};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, Mutex},
};

/// Register after `DefaultPlugins`; applies converter-tagged native material semantics.
pub struct NifMaterialPlugin;

impl Plugin for NifMaterialPlugin {
    fn build(&self, app: &mut App) {
        // `EffectPaletteMaterialPlugin` must run first when both plugins are present:
        // Bevy 0.19's MaterialPlugin::build calls init_asset unconditionally.
        // Keep this plugin's registrations conditional so it can also support headless
        // NIF loading without replacing stores initialized by the rendering plugin.
        init_asset_if_missing::<EffectPaletteImage>(app);
        init_asset_if_missing::<EffectPaletteMaterial>(app);
        init_asset_if_missing::<EffectPaletteDepthMaterial>(app);
        if !app.is_plugin_added::<MaterialPlugin<NifDepthMaterial>>() {
            app.add_plugins(MaterialPlugin::<NifDepthMaterial>::default());
        }
        crate::skin_material::build(app);
        app.register_type::<NifSourceMaterial>()
            .register_type::<NifNativeMaterialSource>()
            .register_type::<EffectPaletteDepthSource>()
            .register_type::<ExternalEmittance>()
            .register_type::<SkinTintSurface>();
    }

    fn finish(&self, app: &mut App) {
        // GltfPlugin creates this resource in its own `finish`. Capture its shared descriptor
        // only after that lifecycle hook has run so native linear twins inherit the configured
        // defaults instead of silently falling back to Bevy's linear sampler.
        let default_sampler = app
            .world()
            .get_resource::<DefaultGltfImageSampler>()
            .map(DefaultGltfImageSampler::get_internal);
        app.world_mut()
            .resource_mut::<GltfExtensionHandlers>()
            .0
            .write_blocking()
            .push(Box::new(NativeMaterialHandler {
                default_sampler,
                ..default()
            }));
    }
}

fn init_asset_if_missing<A: Asset>(app: &mut App) {
    if !app.world().contains_resource::<Assets<A>>() {
        app.init_asset::<A>();
    }
}

/// Registers the effect palette material and its shader. Add before [`NifMaterialPlugin`]
/// when both are used: Bevy 0.19's `MaterialPlugin` replaces an existing asset store.
/// Separate from [`NifMaterialPlugin`] so headless material-handler tests need no renderer.
pub struct EffectPaletteMaterialPlugin;

impl Plugin for EffectPaletteMaterialPlugin {
    fn build(&self, app: &mut App) {
        load_shader_library!(app, "shaders/effect_palette_functions.wgsl");
        embedded_asset!(app, "shaders/effect_palette.wgsl");
        embedded_asset!(app, "shaders/effect_palette_prepass.wgsl");
        init_asset_if_missing::<EffectPaletteImage>(app);
        if !app.is_plugin_added::<MaterialPlugin<EffectPaletteMaterial>>() {
            app.add_plugins(MaterialPlugin::<EffectPaletteMaterial>::default());
        }
        if !app.is_plugin_added::<MaterialPlugin<EffectPaletteDepthMaterial>>() {
            app.add_plugins(MaterialPlugin::<EffectPaletteDepthMaterial>::default());
        }
    }
}

/// A converted effect surface coloured through its greyscale palette.
pub type EffectPaletteMaterial = ExtendedMaterial<StandardMaterial, EffectPalette>;

/// An effect palette surface that also has an authored depth override: the depth extension
/// wraps the palette material and keeps its shaders, as [`NifDepthMaterial`] does for standard
/// surfaces.
pub type EffectPaletteDepthMaterial = ExtendedMaterial<EffectPaletteMaterial, NifDepthExtension>;

/// The palette material an [`EffectPaletteDepthMaterial`] wraps, kept for scene validation and
/// readiness checks, which only know the palette material.
#[derive(Component, Clone, Reflect)]
#[reflect(Component)]
pub(crate) struct EffectPaletteDepthSource(pub Handle<EffectPaletteMaterial>);

/// The palette half of an [`EffectPaletteMaterial`]; the standard half carries the source
/// texture, base colour, UV transform and blending as for every other converted material.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct EffectPalette {
    #[uniform(100)]
    settings: EffectPaletteSettings,
    #[texture(101)]
    #[sampler(102)]
    #[dependency]
    palette: Handle<Image>,
    // Keep the labeled dependency alive while this material is alive. The loader
    // scope records the edge; ExtendedMaterial does not visit its fields.
    #[dependency]
    image_holder: Option<Handle<EffectPaletteImage>>,
}

#[derive(ShaderType, Reflect, Debug, Clone, Copy, PartialEq)]
struct EffectPaletteSettings {
    /// x = 1 when the palette gives the colour, y = 1 when it gives the alpha,
    /// z = the shader's base colour scale. w unused.
    lookup: Vec4,
}

impl EffectPalette {
    /// `color` and `alpha` say which channels the palette supplies; `scale` is the shader's base
    /// colour scale.
    pub fn new(palette: Handle<Image>, color: bool, alpha: bool, scale: f32) -> Self {
        Self {
            settings: EffectPaletteSettings {
                lookup: Vec4::new(
                    f32::from(u8::from(color)),
                    f32::from(u8::from(alpha)),
                    scale,
                    0.0,
                ),
            },
            palette,
            image_holder: None,
        }
    }

    pub fn palette(&self) -> &Handle<Image> {
        &self.palette
    }
}

impl MaterialExtension for EffectPalette {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/effect_palette.wgsl".into()
    }

    fn prepass_fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/effect_palette_prepass.wgsl".into()
    }
}

/// An effect palette's image, held under a label of its own. `ExtendedMaterial` reports none of
/// its fields' handles to the asset system, so the palette material depends on this holder
/// instead: a scene then counts as loaded only once its palettes are.
#[derive(Asset, TypePath)]
struct EffectPaletteImage {
    #[dependency]
    image: Handle<Image>,
}

/// `BSEffectShaderProperty` flags 1 bits 4 and 5 (`Greyscale_To_PaletteColor` and
/// `Greyscale_To_PaletteAlpha`). The same bits mean other things on lighting shaders.
const EFFECT_PALETTE_COLOR: u64 = 1 << 4;
const EFFECT_PALETTE_ALPHA: u64 = 1 << 5;

/// What an effect material reads from its palette: the glTF texture index of the palette and
/// which channels it supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaletteLookup {
    texture: usize,
    color: bool,
    alpha: bool,
}

/// The palette lookup of a converted effect material, if its flags enable one and the converter
/// published the palette as the `greyscale` slot of `OPEN_SKYRIM_material`.
fn palette_lookup(material: &gltf::Material<'_>) -> Option<PaletteLookup> {
    let extension = material.extensions()?.get("OPEN_SKYRIM_material")?;
    if extension.get("shaderFamily")?.as_str()? != "effect" {
        return None;
    }
    let flags = extension.get("shaderFlags1")?.as_u64()?;
    let color = flags & EFFECT_PALETTE_COLOR != 0;
    let alpha = flags & EFFECT_PALETTE_ALPHA != 0;
    if !color && !alpha {
        return None;
    }
    let texture = extension
        .get("textureSlots")?
        .as_array()?
        .iter()
        .find(|slot| slot.get("semantic").and_then(|value| value.as_str()) == Some("greyscale"))?
        .get("texture")?
        .as_u64()?;
    Some(PaletteLookup {
        texture: usize::try_from(texture).ok()?,
        color,
        alpha,
    })
}

/// The effect shader's base colour scale. The converter publishes it as emission:
/// `emissiveFactor × emissiveStrength` is the base colour times the scale, so dividing by the
/// base colour's brightest channel recovers it. A black base colour publishes no emission, and
/// the scale then cannot be recovered; 1 stands in.
fn base_color_scale(material: &StandardMaterial) -> f32 {
    let base = material.base_color.to_linear();
    let peak = base.red.max(base.green).max(base.blue);
    let emissive = material.emissive;
    let scale = emissive.red.max(emissive.green).max(emissive.blue) / peak;
    if peak > 0.0 && scale.is_finite() {
        scale
    } else {
        1.0
    }
}

fn effect_palette_material(
    lookup: PaletteLookup,
    base: StandardMaterial,
    palette: Handle<Image>,
) -> EffectPaletteMaterial {
    let scale = base_color_scale(&base);
    EffectPaletteMaterial {
        base,
        extension: EffectPalette::new(palette, lookup.color, lookup.alpha, scale),
    }
}

/// Per load, Bevy clones the registered handler, so `textures` holds this glTF's images by
/// glTF texture index: Bevy reports every texture before any material. A slot Bevy never
/// reported holds a default handle, which spawn validation reports as not loaded.
#[derive(Clone, Default)]
struct NativeMaterialHandler {
    textures: Vec<Handle<Image>>,
    /// The sampler `bevy_gltf` loaded each texture with, by glTF texture index.
    samplers: HashMap<usize, ImageSamplerDescriptor>,
    /// `bevy_gltf`'s default sampler, which its loader starts every texture's sampler from.
    default_sampler: Option<Arc<Mutex<ImageSamplerDescriptor>>>,
    // Scene hooks use a child LoadContext, which cannot read the parent's material assets.
    // Bevy clones a handler per glTF load, so this cache also isolates concurrent loads.
    depth_states: HashMap<String, NifDepthState>,
    /// Materials given a `"{label}/skin"` [`SkyrimSkinMaterial`], or with a depth state a
    /// `"{label}/skin-depth"` [`SkyrimSkinDepthMaterial`].
    skin_labels: HashSet<String>,
}

impl NativeMaterialHandler {
    /// The linear twin of glTF texture `index`, loaded with that texture's sampler.
    fn linear_texture(&self, context: &mut LoadContext<'_>, index: usize) -> Option<Handle<Image>> {
        let sampler = self
            .samplers
            .get(&index)
            .cloned()
            .unwrap_or_else(ImageSamplerDescriptor::linear);
        load_linear_twin(context, self.textures.get(index)?, sampler)
    }

    fn palette(&self, material: &gltf::Material<'_>) -> Option<(PaletteLookup, Handle<Image>)> {
        let lookup = palette_lookup(material)?;
        Some((lookup, self.textures.get(lookup.texture)?.clone()))
    }

    /// Adds `"{label}/skin"`, a [`SkyrimSkinMaterial`] over `base`, and `"{label}/skin_images"`,
    /// the holder its images load under. The `_s` mask is the linear twin of the sRGB texture the
    /// converter published as the specular colour texture; a FaceGen head's tint and detail maps
    /// are the linear twins of its `facegen_tint` and `detail` slots.
    fn add_skin_material(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Material<'_>,
        label: &str,
        base: StandardMaterial,
        model_space_normal: Handle<Image>,
        specular_srgb: Option<Handle<Image>>,
    ) {
        let sampler = specular_color_texture_index(source)
            .and_then(|index| self.samplers.get(&index).cloned())
            .unwrap_or_else(ImageSamplerDescriptor::linear);
        let shader_flags_1 = native_depth_flags(source).map_or(0, |(flags_1, _)| flags_1);
        let holder = format!("{label}/skin_images");
        let mut specular = None;
        let mut facegen = None;
        let Ok(_) = context.labeled_asset_scope(holder.clone(), |scope| {
            specular = specular_srgb
                .as_ref()
                .and_then(|srgb| load_linear_twin(scope, srgb, sampler));
            facegen = facegen_texture_indices(source).and_then(|(tint, detail)| {
                Some(FacegenMaps {
                    tint: self.linear_texture(scope, tint)?,
                    detail: self.linear_texture(scope, detail)?,
                })
            });
            Ok::<_, Infallible>(SkyrimSkinImages::new(
                model_space_normal.clone(),
                specular.clone(),
                facegen.as_ref(),
            ))
        });
        let mut material = skin_material(
            base,
            model_space_normal,
            specular,
            facegen,
            // NIF's shader-tail skin colour can be white default data. Only the NPC's own
            // QNAM enables SKIN_TINT; actor_skin_tone derives that material per actor.
            None,
            shader_flags_1,
        );
        let Ok(_) = context.labeled_asset_scope(format!("{label}/skin"), |scope| {
            material.extension.images = Some(scope.get_label_handle::<SkyrimSkinImages>(holder));
            Ok::<_, Infallible>(material)
        });
        self.skin_labels.insert(label.to_owned());
    }
}

/// The stock hook records this dependency before our hook replaces its render binding.
/// Keep its strong handle in the scene so scene-only loads remain fully loaded.
#[derive(Component, Clone, Reflect)]
#[reflect(Component)]
pub(crate) struct NifSourceMaterial(pub Handle<StandardMaterial>);

/// The native base material behind an extended material that wraps a `StandardMaterial`
/// ([`NifDepthMaterial`], [`SkyrimSkinMaterial`]), retained for scene validation and collision
/// classification.
#[derive(Component, Clone, Reflect)]
#[reflect(Component)]
pub(crate) struct NifNativeMaterialSource(pub Handle<StandardMaterial>);

/// `SLSF1_External_Emittance` (`vendor/project-wormhole-nif/src/nif_flags.rs`, bit 29).
const SLSF1_EXTERNAL_EMITTANCE: u64 = 1 << 29;

/// Marks a primitive whose NIF shader takes its emission colour from outside the model: from the
/// placing reference's `XEMI` light or region (`crate::emittance`). Reflected so the marker is
/// cloned with the scene into every world instance.
#[derive(Component, Clone, Copy, Default, Reflect)]
#[reflect(Component, Default)]
pub struct ExternalEmittance;

/// Whether the converter published `SLSF1_External_Emittance` in the material's
/// `extras.openSkyrim.shaderFlags1`.
fn has_external_emittance(material: &gltf::Material<'_>) -> bool {
    material
        .extras()
        .as_ref()
        .and_then(|extras| serde_json::from_str::<serde_json::Value>(extras.get()).ok())
        .and_then(|extras| extras["openSkyrim"]["shaderFlags1"].as_u64())
        .is_some_and(|flags| flags & SLSF1_EXTERNAL_EMITTANCE != 0)
}

fn native_tag(material: &gltf::Material<'_>, name: &str, value: &str) -> bool {
    material
        .extras()
        .as_ref()
        .and_then(|extras| serde_json::from_str::<serde_json::Value>(extras.get()).ok())
        .is_some_and(|extras| extras["openSkyrim"][name].as_str() == Some(value))
}

fn has_normal_alpha_mask(material: &gltf::Material<'_>) -> bool {
    native_tag(material, "specularMask", "normal_alpha")
}

/// `_msn` maps hold model-space normals with swizzled channels. The stock `StandardMaterial` can
/// only decode tangent-space maps, so binding one there shades garbage.
fn has_model_space_normals(material: &gltf::Material<'_>) -> bool {
    native_tag(material, "normalConvention", "model_space")
}

/// Default lighting shaders add their own emittance to diffuse lighting before multiplying
/// by the surface colour. glTF emission is additive instead. For the no-glow case, the stock
/// emissive texture binding can supply that diffuse modulation without changing the authored
/// tint or multiplier. Own_Emit does not make slot 2 a glow map.
///
/// Shader reference: Community Shaders e5ae2d0082469f92ac08dda89d230dc2d9328ba0,
/// package/Shaders/Lighting.hlsl:2632-2657,2815-2821 (non-TRUE_PBR path).
/// Glow-map, effect and specialised lighting shaders need their own evaluation; do not infer
/// this path from a missing/pruned emissive texture alone.
fn has_diffuse_modulated_emittance(material: &gltf::Material<'_>) -> bool {
    const OWN_EMIT: u32 = 1 << 22;
    const GLOW_MAP: u32 = 1 << 6;
    native_tag(material, "shaderFamily", "lighting")
        && native_tag(material, "lightingShaderType", "default")
        && native_depth_flags(material)
            .is_some_and(|(flags_1, flags_2)| flags_1 & OWN_EMIT != 0 && flags_2 & GLOW_MAP == 0)
        && material.emissive_texture().is_none()
}

fn native_uv_transform(material: &gltf::Material<'_>) -> Option<bevy::math::Affine2> {
    let extras: serde_json::Value = serde_json::from_str(material.extras().as_ref()?.get()).ok()?;
    let transform = extras.pointer("/openSkyrim/uvTransform")?;
    let offset: [f32; 2] = serde_json::from_value(transform.get("offset")?.clone()).ok()?;
    let scale: [f32; 2] = serde_json::from_value(transform.get("scale")?.clone()).ok()?;
    if !offset.into_iter().chain(scale).all(f32::is_finite) {
        return None;
    }
    Some(bevy::math::Affine2::from_scale_angle_translation(
        scale.into(),
        0.0,
        offset.into(),
    ))
}

fn needs_native_material(material: &gltf::Material<'_>) -> bool {
    let alpha_mode = match material.alpha_mode() {
        GltfAlphaMode::Opaque => AlphaMode::Opaque,
        GltfAlphaMode::Mask => AlphaMode::Mask(material.alpha_cutoff().unwrap_or(0.5)),
        GltfAlphaMode::Blend => AlphaMode::Blend,
    };
    has_normal_alpha_mask(material)
        || native_tag(material, "normalConvention", "directx")
        || has_model_space_normals(material)
        || has_diffuse_modulated_emittance(material)
        || nif_hair_tint(material).is_some()
        || native_uv_transform(material).is_some()
        || native_depth_state(material, alpha_mode).is_some()
}

fn native_depth_flags(material: &gltf::Material<'_>) -> Option<(u32, u32)> {
    let extras: serde_json::Value = serde_json::from_str(material.extras().as_ref()?.get()).ok()?;
    let tag = extras.get("openSkyrim")?;
    let flags_1 = u32::try_from(tag.get("shaderFlags1")?.as_u64()?).ok()?;
    let flags_2 = u32::try_from(tag.get("shaderFlags2")?.as_u64()?).ok()?;
    Some((flags_1, flags_2))
}

fn native_depth_state(
    material: &gltf::Material<'_>,
    alpha_mode: AlphaMode,
) -> Option<NifDepthState> {
    let (flags_1, flags_2) = native_depth_flags(material)?;
    NifDepthState::from_flags(flags_1, flags_2, alpha_mode)
}

fn native_material(
    source: &gltf::Material<'_>,
    loaded: &StandardMaterial,
) -> Option<StandardMaterial> {
    if !needs_native_material(source) {
        return None;
    }
    let mut material = loaded.clone();
    if has_diffuse_modulated_emittance(source) && material.emissive_texture.is_none() {
        let tint = material.base_color.to_linear();
        material.emissive.red *= tint.red;
        material.emissive.green *= tint.green;
        material.emissive.blue *= tint.blue;
        material.emissive_texture = material.base_color_texture.clone();
        material.emissive_channel = material.base_color_channel.clone();
    }
    if has_normal_alpha_mask(source) && material.specular_texture.is_some() {
        // Bevy 0.19 multiplies sampled mask alpha by 0.5 in pbr_fragment.wgsl.
        // Compensate only on this fresh native material, leaving valid glTF factors
        // and generic glTF assets unchanged. No mask (including pruning) means no doubling.
        material.reflectance *= 2.0;
    }
    if native_tag(source, "normalConvention", "directx") && material.normal_map_texture.is_some() {
        // Skyrim normal Y follows increasing texture V. Bevy's generated
        // bitangent uses the opposite orientation. Keep source RGB/alpha intact.
        material.flip_normal_map_y = true;
    }
    if has_model_space_normals(source) {
        // Bevy would decode the map as tangent-space. This is the base of the skin material
        // (`crate::skin_material`), which decodes it; on its own (authored depth states) the
        // surface shades with the interpolated vertex normal.
        material.normal_map_texture = None;
    }
    if let Some(transform) = native_uv_transform(source) {
        // NIF transforms every texture use, even with no diffuse texture from
        // which Bevy's stock glTF loader could recover the common transform.
        material.uv_transform = transform;
    }
    Some(material)
}

impl GltfExtensionHandler for NativeMaterialHandler {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new(self.clone())
    }

    fn on_texture(&mut self, texture: &gltf::Texture<'_>, handle: Handle<Image>) {
        let index = texture.index();
        if self.textures.len() <= index {
            self.textures.resize(index + 1, Handle::default());
        }
        self.textures[index] = handle;
        let default_sampler = self
            .default_sampler
            .as_ref()
            .and_then(|sampler| sampler.lock().ok().map(|sampler| sampler.clone()))
            .unwrap_or_else(ImageSamplerDescriptor::linear);
        self.samplers
            .insert(index, gltf_sampler(texture, &default_sampler));
    }

    fn on_material(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Material<'_>,
        _handle: Handle<GltfMaterial>,
        _loaded: &GltfMaterial,
        label: &str,
    ) {
        let palette = self.palette(source);
        if !needs_native_material(source) && palette.is_none() {
            return;
        }
        // Reuse Bevy's fully constructed standard material instead of duplicating
        // its field conversion. This handler registers after the stock PBR handler.
        let standard = context
            .get_labeled(format!("{label}/std"))
            .and_then(|asset| asset.get::<StandardMaterial>())
            .expect("NifMaterialPlugin requires the stock PBR glTF handler before it");
        let material = native_material(source, standard).unwrap_or_else(|| standard.clone());
        // `native_material` drops a model-space map from the base; the skin material takes it
        // from the stock material instead.
        let model_space_normal = standard.normal_map_texture.clone();
        let specular_srgb = standard.specular_tint_texture.clone();
        match palette {
            Some((lookup, image)) => {
                let holder = format!("{label}/nif_palette_image");
                let image_holder = context.add_labeled_asset(
                    holder.clone(),
                    EffectPaletteImage {
                        image: image.clone(),
                    },
                );
                let depth = native_depth_state(source, material.alpha_mode);
                let mut material = effect_palette_material(lookup, material, image);
                material.extension.image_holder = Some(image_holder);
                if let Some(state) = depth {
                    self.depth_states.insert(label.to_owned(), state);
                    let depth_material = depth_material(material.clone(), state);
                    let Ok(_) = context.labeled_asset_scope(
                        format!("{label}/nif_palette_depth"),
                        |scope| {
                            scope.get_label_handle::<EffectPaletteImage>(holder.clone());
                            Ok::<_, Infallible>(depth_material)
                        },
                    );
                }
                let Ok(_) = context.labeled_asset_scope(format!("{label}/nif_palette"), |scope| {
                    scope.get_label_handle::<EffectPaletteImage>(holder);
                    Ok::<_, Infallible>(material)
                });
            }
            None => {
                let hair_tint = nif_hair_tint(source).filter(|_| !has_model_space_normals(source));
                if let Some(state) = native_depth_state(source, material.alpha_mode) {
                    self.depth_states.insert(label.to_owned(), state);
                    if let Some(tint) = hair_tint {
                        // Retail brows and hairlines are authored decals.
                        self.skin_labels.insert(label.to_owned());
                        context.add_labeled_asset(
                            format!("{label}/skin-depth"),
                            depth_material(hair_material(material.clone(), tint), state),
                        );
                    } else {
                        // A model-space surface among these keeps the vertex-normal shading of
                        // `native_material`: of the retail pack's model-space materials only two
                        // load-screen heads carry a depth state.
                        context.add_labeled_asset(
                            format!("{label}/nif-depth"),
                            depth_material(material.clone(), state),
                        );
                    }
                } else if let Some(tint) = hair_tint {
                    self.skin_labels.insert(label.to_owned());
                    context.add_labeled_asset(
                        format!("{label}/skin"),
                        hair_material(material.clone(), tint),
                    );
                } else if has_model_space_normals(source)
                    && let Some(model_space_normal) = model_space_normal
                {
                    self.add_skin_material(
                        context,
                        source,
                        label,
                        material.clone(),
                        model_space_normal,
                        specular_srgb,
                    );
                }
                context.add_labeled_asset(format!("{label}/nif"), material);
            }
        }
    }

    fn on_spawn_mesh_and_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _primitive: &gltf::Primitive<'_>,
        _mesh: &gltf::Mesh<'_>,
        material: &gltf::Material<'_>,
        entity: &mut EntityWorldMut,
        label: &str,
    ) {
        if has_external_emittance(material) {
            entity.insert(ExternalEmittance);
        }
        let palette = self.palette(material).is_some();
        if palette || needs_native_material(material) {
            let source = entity
                .get::<MeshMaterial3d<StandardMaterial>>()
                .expect("NifMaterialPlugin requires the stock PBR glTF handler before it")
                .0
                .clone();
            entity.insert(NifSourceMaterial(source));
            if palette {
                // A mesh draws once per material component, so the standard one has to go.
                entity.remove::<MeshMaterial3d<StandardMaterial>>();
                let palette = context
                    .get_label_handle::<EffectPaletteMaterial>(format!("{label}/nif_palette"));
                if let Some(state) = self.depth_states.get(label).copied() {
                    entity.insert((
                        EffectPaletteDepthSource(palette),
                        MeshMaterial3d(context.get_label_handle::<EffectPaletteDepthMaterial>(
                            format!("{label}/nif_palette_depth"),
                        )),
                    ));
                    if state.excludes_shadow() {
                        entity.insert(NotShadowCaster);
                    }
                } else {
                    entity.insert(MeshMaterial3d(palette));
                }
            } else if let Some(state) = self.depth_states.get(label).copied() {
                let base = context.get_label_handle::<StandardMaterial>(format!("{label}/nif"));
                entity.remove::<MeshMaterial3d<StandardMaterial>>();
                entity.insert(NifNativeMaterialSource(base));
                if self.skin_labels.contains(label) {
                    entity.insert(MeshMaterial3d(
                        context.get_label_handle::<SkyrimSkinDepthMaterial>(format!(
                            "{label}/skin-depth"
                        )),
                    ));
                } else {
                    entity.insert(MeshMaterial3d(
                        context.get_label_handle::<NifDepthMaterial>(format!("{label}/nif-depth")),
                    ));
                }
                if state.excludes_shadow() {
                    entity.insert(NotShadowCaster);
                }
            } else if self.skin_labels.contains(label) {
                let base = context.get_label_handle::<StandardMaterial>(format!("{label}/nif"));
                entity.remove::<MeshMaterial3d<StandardMaterial>>();
                entity.insert((
                    NifNativeMaterialSource(base),
                    MeshMaterial3d(
                        context.get_label_handle::<SkyrimSkinMaterial>(format!("{label}/skin")),
                    ),
                ));
                if is_skin_tint(material) {
                    entity.insert(SkinTintSurface);
                }
            } else {
                entity.insert(MeshMaterial3d(
                    context.get_label_handle::<StandardMaterial>(format!("{label}/nif")),
                ));
            }
        }
    }
}

#[cfg(test)]
#[path = "nif_material/palette_shader_check.rs"]
mod palette_shader_check;

#[cfg(test)]
#[path = "nif_material/skin_load_tests.rs"]
mod skin_load_tests;

#[cfg(test)]
#[path = "nif_material/skin_shader_check.rs"]
mod skin_shader_check;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nif_depth::{CAST_SHADOWS, DECAL, DEPTH_TEST, DEPTH_WRITE, DYNAMIC_DECAL};
    use bevy::{pbr::Material, shader::ShaderRef};

    fn assert_asset_server_accepts_typed_asset<A: Asset>(app: &App, path: &'static str) {
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load::<A>(path);
        assert_eq!(asset_server.get_path(handle.id()), Some(path.into()));
    }

    #[test]
    fn nif_plugin_registers_palette_asset_types_without_a_renderer() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .add_plugins(NifMaterialPlugin);
        // Finish without a GltfPlugin: the native handler keeps its linear-sampler fallback.
        app.finish();

        assert!(
            app.world()
                .contains_resource::<Assets<EffectPaletteImage>>()
        );
        assert!(
            app.world()
                .contains_resource::<Assets<EffectPaletteMaterial>>()
        );
        assert!(
            app.world()
                .contains_resource::<Assets<EffectPaletteDepthMaterial>>()
        );
        assert_asset_server_accepts_typed_asset::<EffectPaletteImage>(
            &app,
            "palette-registration-image-probe.no-loader",
        );
        assert_asset_server_accepts_typed_asset::<EffectPaletteMaterial>(
            &app,
            "palette-registration-material-probe.no-loader",
        );
        assert_asset_server_accepts_typed_asset::<EffectPaletteDepthMaterial>(
            &app,
            "palette-registration-depth-probe.no-loader",
        );

        let image = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteImage>>()
            .add(EffectPaletteImage {
                image: Handle::default(),
            });
        let palette = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteMaterial>>()
            .add(effect_palette_material(
                PaletteLookup {
                    texture: 0,
                    color: true,
                    alpha: true,
                },
                StandardMaterial::default(),
                Handle::default(),
            ));
        let palette_asset = app
            .world()
            .resource::<Assets<EffectPaletteMaterial>>()
            .get(&palette)
            .unwrap()
            .clone();
        let wrapped = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteDepthMaterial>>()
            .add(depth_material(
                palette_asset,
                NifDepthState::from_flags(DEPTH_TEST | DECAL, 0, AlphaMode::Opaque).unwrap(),
            ));

        assert!(
            app.world()
                .resource::<Assets<EffectPaletteImage>>()
                .get(&image)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteMaterial>>()
                .get(&palette)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteDepthMaterial>>()
                .get(&wrapped)
                .is_some()
        );
    }

    #[test]
    fn nif_plugin_keeps_preinitialized_palette_assets_and_handles() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .init_asset::<EffectPaletteImage>()
            .init_asset::<EffectPaletteMaterial>()
            .init_asset::<EffectPaletteDepthMaterial>();

        let image = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteImage>>()
            .add(EffectPaletteImage {
                image: Handle::default(),
            });
        let palette = effect_palette_material(
            PaletteLookup {
                texture: 0,
                color: true,
                alpha: false,
            },
            StandardMaterial::default(),
            Handle::default(),
        );
        let material = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteMaterial>>()
            .add(palette.clone());
        let wrapped = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteDepthMaterial>>()
            .add(depth_material(
                palette,
                NifDepthState::from_flags(DEPTH_TEST | DECAL, 0, AlphaMode::Opaque).unwrap(),
            ));

        app.add_plugins(NifMaterialPlugin);

        assert!(
            app.world()
                .resource::<Assets<EffectPaletteImage>>()
                .get(&image)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteMaterial>>()
                .get(&material)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteDepthMaterial>>()
                .get(&wrapped)
                .is_some()
        );
    }

    #[test]
    fn rendering_plugin_order_keeps_palette_asset_stores_and_handles() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Shader>()
            .init_resource::<GltfExtensionHandlers>()
            .add_plugins(EffectPaletteMaterialPlugin);

        let palette = effect_palette_material(
            PaletteLookup {
                texture: 0,
                color: true,
                alpha: true,
            },
            StandardMaterial::default(),
            Handle::default(),
        );
        let image = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteImage>>()
            .add(EffectPaletteImage {
                image: Handle::default(),
            });
        let material = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteMaterial>>()
            .add(palette.clone());
        let wrapped = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteDepthMaterial>>()
            .add(depth_material(
                palette,
                NifDepthState::from_flags(DEPTH_TEST | DECAL, 0, AlphaMode::Opaque).unwrap(),
            ));

        app.add_plugins(NifMaterialPlugin);

        assert!(
            app.world()
                .resource::<Assets<EffectPaletteImage>>()
                .get(&image)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteMaterial>>()
                .get(&material)
                .is_some()
        );
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteDepthMaterial>>()
                .get(&wrapped)
                .is_some()
        );
        assert!(app.is_plugin_added::<MaterialPlugin<EffectPaletteMaterial>>());
        assert!(app.is_plugin_added::<MaterialPlugin<EffectPaletteDepthMaterial>>());
    }

    #[test]
    fn native_depth_reads_authored_shadow_flag_for_decals_and_depth_overrides() {
        for flags in [
            0,
            DEPTH_TEST,
            DEPTH_TEST | DECAL,
            DEPTH_TEST | DYNAMIC_DECAL,
        ] {
            for casts in [false, true] {
                let document = gltf::Gltf::from_slice(
                    &serde_json::to_vec(&serde_json::json!({
                        "asset":{"version":"2.0"},"materials":[{
                            "extras":{"openSkyrim":{
                                "shaderFlags1":flags | if casts { CAST_SHADOWS } else { 0 },
                                "shaderFlags2":0
                            }}
                        }]
                    }))
                    .unwrap(),
                )
                .unwrap();
                let source = document.materials().next().unwrap();
                let state = native_depth_state(&source, AlphaMode::Opaque).unwrap();
                assert_eq!(state.cast_shadows, casts);
                assert_eq!(state.excludes_shadow(), !casts);
            }
        }
    }

    #[test]
    fn ordinary_blend_without_depth_write_keeps_loaded_material() {
        for decal in [0, DECAL, DYNAMIC_DECAL] {
            let document = gltf::Gltf::from_slice(
                &serde_json::to_vec(&serde_json::json!({
                    "asset":{"version":"2.0"},"materials":[{
                        "alphaMode":"BLEND",
                        "extras":{"openSkyrim":{
                            "shaderFlags1":DEPTH_TEST | decal,"shaderFlags2":0
                        }}
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
            let source = document.materials().next().unwrap();
            let loaded = StandardMaterial {
                alpha_mode: AlphaMode::Blend,
                ..default()
            };
            assert_eq!(needs_native_material(&source), decal != 0);
            assert_eq!(native_material(&source, &loaded).is_some(), decal != 0);
            assert_eq!(
                native_depth_state(&source, loaded.alpha_mode).is_some(),
                decal != 0
            );
        }
    }

    #[test]
    fn native_depth_requires_both_valid_flags_and_keeps_generic_gltf_unchanged() {
        for tag in [
            serde_json::json!({}),
            serde_json::json!({"shaderFlags1":DEPTH_TEST}),
            serde_json::json!({"shaderFlags1":-1,"shaderFlags2":0}),
            serde_json::json!({"shaderFlags1":4294967296_u64,"shaderFlags2":0}),
            serde_json::json!({"shaderFlags1":"0","shaderFlags2":0}),
            serde_json::json!({"shaderFlags1":DEPTH_TEST,"shaderFlags2":DEPTH_WRITE}),
        ] {
            let document = gltf::Gltf::from_slice(
                &serde_json::to_vec(&serde_json::json!({
                    "asset":{"version":"2.0"},"materials":[{"extras":{"openSkyrim":tag}}]
                }))
                .unwrap(),
            )
            .unwrap();
            let source = document.materials().next().unwrap();
            assert!(native_material(&source, &StandardMaterial::default()).is_none());
            assert!(native_depth_state(&source, AlphaMode::Opaque).is_none());
        }
    }

    #[test]
    fn depth_scene_clone_retains_one_render_binding_and_base_dependency() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .init_asset::<StandardMaterial>()
            .add_plugins(NifMaterialPlugin)
            .register_type::<MeshMaterial3d<NifDepthMaterial>>()
            .register_type::<NotShadowCaster>();
        let source = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let state = NifDepthState::from_flags(DEPTH_TEST | DECAL, 0, AlphaMode::Mask(0.5)).unwrap();
        let material = app
            .world_mut()
            .resource_mut::<Assets<NifDepthMaterial>>()
            .add(depth_material(StandardMaterial::default(), state));
        let mut world = World::new();
        world.spawn((
            NifSourceMaterial(source.clone()),
            NifNativeMaterialSource(source.clone()),
            MeshMaterial3d(material.clone()),
            NotShadowCaster,
        ));
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned.world.query::<(
            &NifNativeMaterialSource,
            &MeshMaterial3d<NifDepthMaterial>,
            Option<&MeshMaterial3d<StandardMaterial>>,
            Option<&NotShadowCaster>,
        )>();
        let (base, rendered, standard, no_shadow) = query.single(&cloned.world).unwrap();
        assert_eq!(base.0, source);
        assert_eq!(rendered.0, material);
        assert!(standard.is_none());
        assert!(no_shadow.is_some());
    }

    #[test]
    fn v15_native_uv_transform_works_without_a_diffuse_texture() {
        let document = gltf::Gltf::from_slice(
            br#"{"asset":{"version":"2.0"},"materials":[{
            "extras":{"openSkyrim":{"uvTransform":{"offset":[0.25,-0.5],"scale":[2,-3]}}}
        }]}"#,
        )
        .unwrap();
        let source = document.materials().next().unwrap();
        let loaded = StandardMaterial::default();
        let native = native_material(&source, &loaded).unwrap();
        assert_eq!(
            native.uv_transform.transform_point2(Vec2::new(0.5, 0.5)),
            Vec2::new(1.25, -2.0)
        );
        assert_eq!(loaded.uv_transform, bevy::math::Affine2::IDENTITY);
    }

    #[test]
    fn v13_native_normal_convention_is_scoped_and_preserves_mask() {
        for convention in [None, Some("directx"), Some("model_space")] {
            let document = gltf::Gltf::from_slice(
                serde_json::to_string(&serde_json::json!({
                    "asset":{"version":"2.0"}, "materials":[{
                        "extras":{"openSkyrim":{"normalConvention":convention}}
                    }]
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            let source = document.materials().next().unwrap();
            let loaded = StandardMaterial {
                normal_map_texture: Some(Handle::default()),
                specular_texture: Some(Handle::default()),
                reflectance: 0.4,
                ..default()
            };
            let native = native_material(&source, &loaded);
            match convention {
                Some("directx") => {
                    let native =
                        native.expect("tagged tangent normals require native construction");
                    assert!(native.flip_normal_map_y);
                    assert_eq!(native.reflectance, loaded.reflectance);
                    assert_eq!(native.specular_texture, loaded.specular_texture);
                    assert_eq!(native.normal_map_texture, loaded.normal_map_texture);
                }
                Some("model_space") => {
                    let native = native.expect("model-space normals require native construction");
                    assert!(native.normal_map_texture.is_none());
                    assert!(!native.flip_normal_map_y);
                    assert_eq!(native.reflectance, loaded.reflectance);
                    assert_eq!(native.specular_texture, loaded.specular_texture);
                }
                _ => assert!(native.is_none()),
            }
            assert!(loaded.normal_map_texture.is_some());
            assert!(!loaded.flip_normal_map_y);
        }
    }

    #[test]
    fn model_space_normals_drop_only_the_normal_map() {
        let document = gltf::Gltf::from_slice(
            serde_json::to_string(&serde_json::json!({
                "asset":{"version":"2.0"}, "materials":[{
                    "alphaMode":"MASK", "alphaCutoff":0.25,
                    "extras":{"openSkyrim":{
                        "normalConvention":"model_space",
                        "uvTransform":{"offset":[0.5,0.0],"scale":[2,1]}
                    }}
                }]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let source = document.materials().next().unwrap();
        assert!(needs_native_material(&source));
        let loaded = StandardMaterial {
            base_color: Color::srgb(0.8, 0.6, 0.5),
            base_color_texture: Some(Handle::default()),
            normal_map_texture: Some(Handle::default()),
            specular_texture: Some(Handle::default()),
            alpha_mode: AlphaMode::Mask(0.25),
            perceptual_roughness: 0.7,
            reflectance: 0.3,
            ..default()
        };
        let native = native_material(&source, &loaded).unwrap();
        assert!(native.normal_map_texture.is_none());
        assert_eq!(native.base_color, loaded.base_color);
        assert_eq!(native.base_color_texture, loaded.base_color_texture);
        assert_eq!(native.specular_texture, loaded.specular_texture);
        assert_eq!(native.alpha_mode, loaded.alpha_mode);
        assert_eq!(native.perceptual_roughness, loaded.perceptual_roughness);
        assert_eq!(native.reflectance, loaded.reflectance);
        assert_eq!(
            native.uv_transform.transform_point2(Vec2::ZERO),
            Vec2::new(0.5, 0.0)
        );
    }

    #[test]
    fn v12_scene_cloning_retains_source_and_native_material_handles() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .add_plugins(NifMaterialPlugin)
            .register_type::<MeshMaterial3d<StandardMaterial>>();
        let mut assets = Assets::<StandardMaterial>::default();
        let source = assets.add(StandardMaterial::default());
        let native = assets.add(StandardMaterial::default());
        let mut world = World::new();
        world.spawn((
            NifSourceMaterial(source.clone()),
            MeshMaterial3d(native.clone()),
        ));
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned
            .world
            .query::<(&NifSourceMaterial, &MeshMaterial3d<StandardMaterial>)>();
        let (retained, rendered) = query.single(&cloned.world).unwrap();
        assert_eq!(retained.0, source);
        assert_eq!(rendered.0, native);
    }

    /// An effect palette decal, like `dlc01/effects/icechunk01.nif`: the palette lookup and the
    /// authored depth override both apply, and the wrapper keeps the palette and its blending.
    #[test]
    fn palette_decal_keeps_its_palette_under_the_depth_override() {
        let document = first_material(serde_json::json!({
            "alphaMode": "MASK",
            "extensions": {"OPEN_SKYRIM_material": {
                "shaderFamily": "effect", "shaderFlags1": DEPTH_TEST | DECAL | 0x30,
                "textureSlots": [{"semantic": "greyscale", "slot": 1, "texture": 0}]
            }},
            "extras": {"openSkyrim": {"shaderFlags1": DEPTH_TEST | DECAL | 0x30, "shaderFlags2": 0}}
        }));
        let source = document.materials().next().unwrap();
        let lookup = palette_lookup(&source).unwrap();
        let state = native_depth_state(&source, AlphaMode::Mask(0.5)).unwrap();
        assert!(state.decal && state.alpha_mask && !state.depth_write);
        let mut images = Assets::<Image>::default();
        let palette = images.add(Image::default());
        let base = StandardMaterial {
            alpha_mode: AlphaMode::Mask(0.5),
            ..default()
        };
        let wrapped = depth_material(
            effect_palette_material(lookup, base, palette.clone()),
            state,
        );
        assert_eq!(wrapped.extension.state, state);
        assert_eq!(wrapped.base.extension.palette(), &palette);
        assert_eq!(wrapped.base.base.alpha_mode, AlphaMode::Mask(0.5));
        assert_eq!(bevy::pbr::Material::alpha_mode(&wrapped), AlphaMode::Blend);
    }

    #[test]
    fn depth_wrapped_palette_scene_clone_retains_the_palette_source() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .init_asset::<StandardMaterial>()
            .init_asset::<EffectPaletteMaterial>()
            .init_asset::<EffectPaletteDepthMaterial>()
            .add_plugins(NifMaterialPlugin)
            .register_type::<MeshMaterial3d<EffectPaletteDepthMaterial>>()
            .register_type::<NotShadowCaster>();
        let palette = EffectPaletteMaterial {
            base: StandardMaterial::default(),
            extension: EffectPalette::new(Handle::default(), true, true, 1.0),
        };
        let state = NifDepthState::from_flags(DEPTH_TEST | DECAL, 0, AlphaMode::Opaque).unwrap();
        let wrapped = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteDepthMaterial>>()
            .add(depth_material(palette.clone(), state));
        let palette = app
            .world_mut()
            .resource_mut::<Assets<EffectPaletteMaterial>>()
            .add(palette);
        let mut world = World::new();
        world.spawn((
            EffectPaletteDepthSource(palette.clone()),
            MeshMaterial3d(wrapped.clone()),
            NotShadowCaster,
        ));
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned.world.query::<(
            &EffectPaletteDepthSource,
            &MeshMaterial3d<EffectPaletteDepthMaterial>,
            Option<&MeshMaterial3d<EffectPaletteMaterial>>,
            Option<&NotShadowCaster>,
        )>();
        let (source, rendered, unwrapped, no_shadow) = query.single(&cloned.world).unwrap();
        assert_eq!(source.0, palette);
        assert_eq!(rendered.0, wrapped);
        // One material component per mesh: the palette itself must not also draw.
        assert!(unwrapped.is_none());
        assert!(no_shadow.is_some());
    }

    fn first_material(material: serde_json::Value) -> gltf::Gltf {
        gltf::Gltf::from_slice(
            serde_json::to_string(&serde_json::json!({
                "asset": {"version": "2.0"}, "materials": [material]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn own_emittance_uses_diffuse_colour_without_mutating_the_loaded_material() {
        let document = first_material(serde_json::json!({"extras":{"openSkyrim":{
            "shaderFamily":"lighting", "lightingShaderType":"default",
            "shaderFlags1": 1 << 22, "shaderFlags2": 0
        }}}));
        let source = document.materials().next().unwrap();
        for multiple in [0.0, 0.25, 1.0, 4.0] {
            for textured in [false, true] {
                let loaded = StandardMaterial {
                    base_color: Color::linear_rgb(0.2, 0.4, 0.8),
                    base_color_texture: textured.then(Handle::default),
                    base_color_channel: bevy::mesh::UvChannel::Uv1,
                    emissive: LinearRgba::new(0.5 * multiple, 0.4 * multiple, 0.2 * multiple, 1.0),
                    alpha_mode: AlphaMode::Mask(0.5),
                    ..default()
                };
                for _ in 0..2 {
                    let native = native_material(&source, &loaded).unwrap();
                    let expected = [0.1 * multiple, 0.16 * multiple, 0.16 * multiple];
                    for (actual, expected) in
                        native.emissive.to_f32_array()[..3].iter().zip(expected)
                    {
                        assert!((actual - expected).abs() < 1e-6);
                    }
                    assert_eq!(native.emissive.alpha, loaded.emissive.alpha);
                    assert_eq!(native.emissive_texture, loaded.base_color_texture);
                    assert_eq!(native.emissive_channel, loaded.base_color_channel);
                    assert_eq!(native.alpha_mode, loaded.alpha_mode);
                    assert_eq!(native.base_color_texture, loaded.base_color_texture);
                }
                assert!(loaded.emissive_texture.is_none());
                assert_eq!(loaded.emissive.red, 0.5 * multiple);
            }
        }
        let loaded = StandardMaterial {
            emissive: LinearRgba::rgb(2.0, 1.0, 0.5),
            emissive_texture: Some(Handle::default()),
            ..default()
        };
        let native = native_material(&source, &loaded).unwrap();
        assert_eq!(native.emissive, loaded.emissive);
        assert_eq!(native.emissive_texture, loaded.emissive_texture);
    }

    #[test]
    fn diffuse_emittance_requires_default_lighting_own_emit_and_no_glow_flag() {
        for (family, shader, flags_1, flags_2, expected) in [
            ("lighting", "default", 1 << 22, 0, true),
            ("lighting", "default", 0, 0, false),
            // A pruned glow texture must not silently switch to diffuse emission.
            ("lighting", "default", 1 << 22, 1 << 6, false),
            ("lighting", "glow", 1 << 22, 0, false),
            ("lighting", "skin_tint", 1 << 22, 0, false),
            ("effect", "default", 1 << 22, 0, false),
            ("", "", 1 << 22, 0, false),
        ] {
            let document = first_material(serde_json::json!({"extras":{"openSkyrim":{
                "shaderFamily":family, "lightingShaderType":shader,
                "shaderFlags1":flags_1, "shaderFlags2":flags_2
            }}}));
            assert_eq!(
                has_diffuse_modulated_emittance(&document.materials().next().unwrap()),
                expected
            );
        }
        assert!(!has_diffuse_modulated_emittance(
            &first_material(serde_json::json!({}))
                .materials()
                .next()
                .unwrap()
        ));
    }

    /// The forge's `Flames` (both palette channels) and `JetBoost` (palette colour only) from
    /// `furniture\blacksmithforgemarker.nif`, as the converter publishes them.
    #[test]
    fn palette_lookup_reads_effect_flags_and_the_greyscale_slot() {
        let effect = |flags: u64, family: &str, slots: serde_json::Value| {
            serde_json::json!({"extensions": {"OPEN_SKYRIM_material": {
                "shaderFamily": family, "shaderFlags1": flags, "textureSlots": slots
            }}})
        };
        let greyscale = serde_json::json!([{"semantic": "greyscale", "slot": 1, "texture": 2}]);
        for (material, expected) in [
            (
                effect(0xC000_0078, "effect", greyscale.clone()),
                Some(PaletteLookup {
                    texture: 2,
                    color: true,
                    alpha: true,
                }),
            ),
            (
                effect(0x8000_0058, "effect", greyscale.clone()),
                Some(PaletteLookup {
                    texture: 2,
                    color: true,
                    alpha: false,
                }),
            ),
            (
                effect(0xC000_0028, "effect", greyscale.clone()),
                Some(PaletteLookup {
                    texture: 2,
                    color: false,
                    alpha: true,
                }),
            ),
            // The forge glow: an effect with no palette flag.
            (effect(0xC000_0008, "effect", greyscale.clone()), None),
            (effect(0, "effect", greyscale.clone()), None),
            // Bits 4 and 5 mean other things on a lighting shader.
            (effect(0xC000_0078, "lighting", greyscale.clone()), None),
            (
                effect(
                    0xC000_0078,
                    "effect",
                    serde_json::json!([{"semantic": "detail", "slot": 1, "texture": 2}]),
                ),
                None,
            ),
            (serde_json::json!({}), None),
        ] {
            let document = first_material(material.clone());
            let source = document.materials().next().unwrap();
            assert_eq!(palette_lookup(&source), expected, "{material}");
        }
    }

    #[test]
    fn authored_depth_wrapper_keeps_palette_shadow_fragment_shader() {
        let shader = <EffectPaletteDepthMaterial as Material>::prepass_fragment_shader();
        assert!(matches!(
            shader,
            ShaderRef::Path(path)
                if path == bevy::asset::AssetPath::from("embedded://engine/shaders/effect_palette_prepass.wgsl")
        ));
    }

    #[test]
    fn handler_hands_out_the_palette_by_gltf_texture_index() {
        let document = gltf::Gltf::from_slice(
            serde_json::to_string(&serde_json::json!({
                "asset": {"version": "2.0"},
                "images": [{"uri": "a.ktx2"}, {"uri": "b.ktx2"}, {"uri": "c.ktx2"}],
                "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
                "materials": [{"extensions": {"OPEN_SKYRIM_material": {
                    "shaderFamily": "effect", "shaderFlags1": 0x10,
                    "textureSlots": [{"semantic": "greyscale", "slot": 1, "texture": 2}]
                }}}]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let mut images = Assets::<Image>::default();
        let handles: Vec<_> = (0..3).map(|_| images.add(Image::default())).collect();
        // Reported out of order, the palette still resolves by its own index.
        let mut handler = NativeMaterialHandler::default();
        for index in [2, 0, 1] {
            let texture = document.textures().nth(index).unwrap();
            GltfExtensionHandler::on_texture(&mut handler, &texture, handles[index].clone());
        }
        let source = document.materials().next().unwrap();
        let (lookup, palette) = handler.palette(&source).unwrap();
        assert!(lookup.color && !lookup.alpha);
        assert_eq!(palette, handles[2]);
        // A slot below a reported texture that Bevy never reported stays unloadable.
        let mut sparse = NativeMaterialHandler::default();
        GltfExtensionHandler::on_texture(
            &mut sparse,
            &document.textures().nth(2).unwrap(),
            handles[2].clone(),
        );
        assert_eq!(sparse.textures[0], Handle::default());
        let empty = NativeMaterialHandler::default();
        assert!(empty.palette(&source).is_none());
    }

    #[test]
    fn base_color_scale_is_recovered_from_the_published_emission() {
        for (base, scale) in [
            (LinearRgba::rgb(1.0, 1.0, 1.0), 1.6),
            (LinearRgba::rgb(0.706, 0.706, 0.706), 1.5),
            (LinearRgba::rgb(1.0, 0.56, 0.24), 0.856),
        ] {
            let material = StandardMaterial {
                base_color: base.into(),
                emissive: base * scale,
                ..default()
            };
            assert!((base_color_scale(&material) - scale).abs() < 1e-5);
        }
        let black = StandardMaterial {
            base_color: Color::BLACK,
            emissive: LinearRgba::BLACK,
            ..default()
        };
        assert_eq!(base_color_scale(&black), 1.0);
    }

    /// The flags are those of `architecture\farmhouse\inn01.nif`: its window, then its walls.
    #[test]
    fn external_emittance_needs_bit_29_of_shader_flags_1() {
        for (extras, expected) in [
            (
                serde_json::json!({"openSkyrim": {"shaderFlags1": 0xA240_0301u32}}),
                true,
            ),
            (
                serde_json::json!({"openSkyrim": {"shaderFlags1": 0x8240_0301u32}}),
                false,
            ),
            (
                serde_json::json!({"openSkyrim": {"shaderFlags2": 0x2000_0000u32}}),
                false,
            ),
            (serde_json::json!({}), false),
        ] {
            let document = gltf::Gltf::from_slice(
                serde_json::to_string(&serde_json::json!({
                    "asset": {"version": "2.0"}, "materials": [{"extras": extras}]
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            let material = document.materials().next().unwrap();
            assert_eq!(has_external_emittance(&material), expected, "{extras}");
        }
    }

    #[test]
    fn scene_cloning_retains_the_external_emittance_marker() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_resource::<GltfExtensionHandlers>()
            .init_asset::<StandardMaterial>()
            .add_plugins(NifMaterialPlugin);
        let mut world = World::new();
        world.spawn(ExternalEmittance);
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned.world.query::<&ExternalEmittance>();
        assert!(query.single(&cloned.world).is_ok());
    }

    #[test]
    fn v10_native_compensation_requires_tag_and_loaded_mask_and_does_not_compound() {
        for tagged in [false, true] {
            let document = gltf::Gltf::from_slice(serde_json::to_string(&serde_json::json!({
                "asset":{"version":"2.0"},"materials":[{
                    "extras":if tagged { serde_json::json!({"openSkyrim":{"specularMask":"normal_alpha"}}) }
                             else { serde_json::json!({}) }
                }]
            })).unwrap().as_bytes()).unwrap();
            let source = document.materials().next().unwrap();
            for mask in [false, true] {
                let loaded = StandardMaterial {
                    reflectance: 0.4,
                    specular_texture: mask.then(Handle::default),
                    ..default()
                };
                for _ in 0..2 {
                    let material = native_material(&source, &loaded);
                    if tagged {
                        assert!(
                            (material.unwrap().reflectance - if mask { 0.8 } else { 0.4 }).abs()
                                < 1e-6
                        );
                    } else {
                        assert!(material.is_none());
                    }
                    assert_eq!(loaded.reflectance, 0.4);
                }
            }
        }
    }
}
