//! Compiles `terrain_merged.wgsl` with naga for every specialisation the engine can ask for.
//!
//! Bevy builds shaders at pipeline creation, on a GPU. This test composes the same source with the
//! shader defs [`super::material::shader_defs`] produces, against stand-ins for the handful of Bevy
//! modules the shader imports (their real signatures, trivial bodies), and runs naga's validator
//! over the result: a typo, a binding the defs do not declare, a texture used with the wrong
//! sampling function or a stride the shader cannot express fails here. It does not replace one run
//! on a device, which also checks the pipeline layout; that is reported separately.

use super::{
    classes::{MAX_CLASSES_PER_USAGE, TerrainClassKey},
    material::shader_defs,
};
use naga_oil::compose::{
    ComposableModuleDescriptor, Composer, NagaModuleDescriptor, ShaderDefValue, ShaderLanguage,
    ShaderType,
};
use std::collections::HashMap;
use wgpu::naga::{self, Expression, Handle, Statement};

const SHADER: &str = include_str!("terrain_merged.wgsl");

const PBR_TYPES: &str = "
#define_import_path bevy_pbr::pbr_types
struct StandardMaterial { base_color: vec4<f32>, flags: u32, reflectance: vec3<f32> }
const STANDARD_MATERIAL_FLAGS_UNLIT_BIT: u32 = 32u;
struct PbrInput {
    material: StandardMaterial,
    world_normal: vec3<f32>,
    N: vec3<f32>,
}
";

const FORWARD_IO: &str = "
#define_import_path bevy_pbr::forward_io
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec4<f32>,
    @location(1) world_normal: vec3<f32>,
#ifdef VERTEX_UVS_A
    @location(2) uv: vec2<f32>,
#endif
#ifdef VERTEX_TANGENTS
    @location(3) world_tangent: vec4<f32>,
#endif
#ifdef VERTEX_UVS_B
    @location(4) uv_b: vec2<f32>,
#endif
#ifdef VERTEX_COLORS
    @location(5) color: vec4<f32>,
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    @location(6) @interpolate(flat) instance_index: u32,
#endif
#ifdef VISIBILITY_RANGE_DITHER
    @location(7) @interpolate(flat) visibility_range_dither: i32,
#endif
}
struct FragmentOutput {
    @location(0) color: vec4<f32>,
}
";

const MESH_FUNCTIONS: &str = "
#define_import_path bevy_pbr::mesh_functions
fn get_world_from_local(instance_index: u32) -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4<f32>(1.0, 0.0, 0.0, 0.0),
        vec4<f32>(0.0, 1.0, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, 1.0, 0.0),
        vec4<f32>(0.0, 0.0, 0.0, 1.0));
}
fn mesh_normal_local_to_world(vertex_normal: vec3<f32>, instance_index: u32) -> vec3<f32> {
    return vertex_normal;
}
fn mesh_position_local_to_world(world_from_local: mat4x4<f32>, vertex_position: vec4<f32>) -> vec4<f32> {
    return world_from_local * vertex_position;
}
fn get_visibility_range_dither_level(instance_index: u32, world_position: vec4<f32>) -> i32 {
    return 0;
}
";

const PBR_FRAGMENT: &str = "
#define_import_path bevy_pbr::pbr_fragment
#import bevy_pbr::{forward_io::VertexOutput, pbr_types::PbrInput}
fn pbr_input_from_standard_material(in: VertexOutput, is_front: bool) -> PbrInput {
    var pbr_input: PbrInput;
    pbr_input.world_normal = in.world_normal;
    pbr_input.N = in.world_normal;
    pbr_input.material.base_color = vec4<f32>(1.0);
    return pbr_input;
}
";

const PBR_FUNCTIONS: &str = "
#define_import_path bevy_pbr::pbr_functions
#import bevy_pbr::pbr_types::{PbrInput, StandardMaterial}
fn alpha_discard(material: StandardMaterial, output_color: vec4<f32>) -> vec4<f32> {
    return output_color;
}
fn apply_pbr_lighting(in: PbrInput) -> vec4<f32> {
    return in.material.base_color * vec4<f32>(in.N, 1.0);
}
fn main_pass_post_lighting_processing(input: PbrInput, output_color: vec4<f32>) -> vec4<f32> {
    return output_color;
}
";

const VIEW_TRANSFORMATIONS: &str = "
#define_import_path bevy_pbr::view_transformations
fn position_world_to_clip(world_pos: vec3<f32>) -> vec4<f32> {
    return vec4<f32>(world_pos, 1.0);
}
";

/// Composes the shader with `defs` plus the defs Bevy's own mesh pipeline adds for a mesh with
/// positions, normals, UVs and colours.
fn compose_module(
    source: &str,
    defs: Vec<bevy::shader::ShaderDefVal>,
) -> Result<naga::Module, String> {
    let mut composer = Composer::default();
    let mut shader_defs: HashMap<String, ShaderDefValue> = HashMap::new();
    for name in [
        "VERTEX_UVS_A",
        "VERTEX_COLORS",
        "VERTEX_OUTPUT_INSTANCE_INDEX",
        "VISIBILITY_RANGE_DITHER",
    ] {
        shader_defs.insert(name.to_owned(), ShaderDefValue::Bool(true));
    }
    shader_defs.insert("MATERIAL_BIND_GROUP".to_owned(), ShaderDefValue::UInt(3));
    for def in defs {
        match def {
            bevy::shader::ShaderDefVal::Bool(name, value) => {
                shader_defs.insert(name, ShaderDefValue::Bool(value));
            }
            other => panic!("unexpected shader def {other:?}"),
        }
    }
    for (path, source) in [
        ("pbr_types.wgsl", PBR_TYPES),
        ("forward_io.wgsl", FORWARD_IO),
        ("mesh_functions.wgsl", MESH_FUNCTIONS),
        ("pbr_fragment.wgsl", PBR_FRAGMENT),
        ("pbr_functions.wgsl", PBR_FUNCTIONS),
        ("view_transformations.wgsl", VIEW_TRANSFORMATIONS),
    ] {
        composer
            .add_composable_module(ComposableModuleDescriptor {
                source,
                file_path: path,
                language: ShaderLanguage::Wgsl,
                shader_defs: shader_defs.clone(),
                ..Default::default()
            })
            .map_err(|error| format!("{path}: {error:?}"))?;
    }
    composer
        .make_naga_module(NagaModuleDescriptor {
            source,
            file_path: "terrain_merged.wgsl",
            shader_type: ShaderType::Wgsl,
            shader_defs,
            additional_imports: &[],
        })
        .map_err(|error| error.emit_to_string(&composer))
}

/// Validate a production shader against the stand-ins without retaining its IR.
fn compose_source(source: &str, defs: Vec<bevy::shader::ShaderDefVal>) -> Result<(), String> {
    compose_module(source, defs).map(|_| ())
}

/// Compile the merged path with its production specialisation flags.
fn compose(defs: Vec<bevy::shader::ShaderDefVal>) -> Result<(), String> {
    compose_source(SHADER, defs)
}

/// Build one production terrain class key for compilation coverage.
fn key(diffuse_classes: u8, normal_classes: u8) -> TerrainClassKey {
    TerrainClassKey {
        diffuse_classes,
        normal_classes,
    }
}

/// Both vertex strides compile with the usual vanilla texture classes.
#[test]
fn the_shader_compiles_for_both_strides_with_a_vanilla_class_set() {
    for wide in [false, true] {
        compose(shader_defs(key(4, 2), wide)).unwrap_or_else(|error| {
            panic!("stride {} failed:\n{error}", if wide { 16 } else { 8 })
        });
    }
}

/// Every supported texture-class count compiles, including no normal maps.
#[test]
fn the_shader_compiles_for_the_smallest_and_the_largest_class_sets() {
    let most = MAX_CLASSES_PER_USAGE as u8;
    for wide in [false, true] {
        for (diffuse, normal) in (1..=most).flat_map(|d| (0..=most).map(move |n| (d, n))) {
            compose(shader_defs(key(diffuse, normal), wide)).unwrap_or_else(|error| {
                panic!("{diffuse} diffuse and {normal} normal classes, wide {wide}:\n{error}")
            });
        }
    }
}

/// The quadrant path compiles with each optional weight carrier. Normal presence is runtime
/// uniform data, so compilation alone does not exercise present/absent normal maps.
#[test]
fn quadrant_shader_compiles_with_every_weight_carrier() {
    for tangents in [false, true] {
        for uv_b in [false, true] {
            let defs = [("VERTEX_TANGENTS", tangents), ("VERTEX_UVS_B", uv_b)]
                .into_iter()
                .filter(|(_, enabled)| *enabled)
                .map(|(name, _)| bevy::shader::ShaderDefVal::Bool(name.to_owned(), true))
                .collect();
            compose_source(include_str!("../shaders/terrain.wgsl"), defs).unwrap();
        }
    }
}

/// Evaluate the small pure material-parameter function in the actual WGSL, without a GPU.
/// Absence means the old shader's unmodified material reflectance, so the baseline fails on
/// a numerical assertion rather than a parser or compilation error.
fn shader_reflectance(source: &str, reflectance: [f32; 3], mask_sum: f32, total: f32) -> Vec<f32> {
    use wgpu::naga::{Expression, Literal, MathFunction, Statement};

    /// Interpret just the scalar/vector arithmetic used by the parameter function.
    fn evaluate(
        function: &wgpu::naga::Function,
        expression: wgpu::naga::Handle<Expression>,
        arguments: &[Vec<f32>],
    ) -> Vec<f32> {
        let eval = |handle| evaluate(function, handle, arguments);
        match function.expressions[expression] {
            Expression::Literal(Literal::F32(value)) => vec![value],
            Expression::FunctionArgument(index) => arguments[index as usize].clone(),
            Expression::Binary { op, left, right } => {
                let left = eval(left);
                let right = eval(right);
                (0..left.len().max(right.len()))
                    .map(|i| match op {
                        wgpu::naga::BinaryOperator::Multiply => {
                            left[i % left.len()] * right[i % right.len()]
                        }
                        wgpu::naga::BinaryOperator::Divide => {
                            left[i % left.len()] / right[i % right.len()]
                        }
                        _ => panic!("unsupported parameter operator {op:?}"),
                    })
                    .collect()
            }
            Expression::Math {
                fun: MathFunction::Sqrt,
                arg,
                ..
            } => eval(arg).into_iter().map(f32::sqrt).collect(),
            Expression::Math {
                fun: MathFunction::Clamp,
                arg,
                arg1: Some(low),
                arg2: Some(high),
                ..
            } => {
                let low = eval(low)[0];
                let high = eval(high)[0];
                eval(arg)
                    .into_iter()
                    .map(|value| value.clamp(low, high))
                    .collect()
            }
            ref other => panic!("unsupported parameter expression {other:?}"),
        }
    }

    let Some(start) = source.find("fn terrain_reflectance(") else {
        return reflectance.to_vec();
    };
    let end = start + source[start..].find("\n}").unwrap() + 2;
    let module = wgpu::naga::front::wgsl::parse_str(&source[start..end]).unwrap();
    let (_, function) = module.functions.iter().next().unwrap();
    let result = function
        .body
        .iter()
        .find_map(|statement| match statement {
            Statement::Return { value } => *value,
            _ => None,
        })
        .unwrap();
    evaluate(
        function,
        result,
        &[reflectance.to_vec(), vec![mask_sum], vec![total]],
    )
}

/// Normal alpha scales dielectric F0 linearly; applying it to reflectance directly would square
/// the mask. Asymmetric RGB and non-unit totals distinguish both mistakes and overpaint handling.
#[test]
fn terrain_specular_mask_scales_f0_instead_of_squaring_it() {
    for source in [SHADER, include_str!("../shaders/terrain.wgsl")] {
        for (mask_sum, total, expected_f0) in [
            (0.0, 1.0, [0.0, 0.0, 0.0]),
            (0.25, 1.0, [0.01, 0.0025, 0.0225]),
            (0.75, 1.5, [0.02, 0.005, 0.045]),
            (1.0, 1.0, [0.04, 0.01, 0.09]),
            (1.2, 1.0, [0.04, 0.01, 0.09]),
        ] {
            let actual = shader_reflectance(source, [0.5, 0.25, 0.75], mask_sum, total);
            for (reflectance, expected) in actual.into_iter().zip(expected_f0) {
                assert!(
                    (0.16 * reflectance * reflectance - expected).abs() < 1.0e-6,
                    "mask_sum={mask_sum}, total={total}, expected F0={expected}, reflectance={reflectance}"
                );
            }
        }
    }
}

/// Visit structured IR in execution order, retaining control-flow depth for unconditional wiring.
fn statements(block: &naga::Block) -> Vec<(&Statement, usize)> {
    /// Descend into each control-flow construct without interpreting it.
    fn visit<'a>(block: &'a naga::Block, depth: usize, out: &mut Vec<(&'a Statement, usize)>) {
        for statement in block.iter() {
            out.push((statement, depth));
            match statement {
                Statement::If { accept, reject, .. } => {
                    visit(accept, depth + 1, out);
                    visit(reject, depth + 1, out);
                }
                Statement::Loop {
                    body, continuing, ..
                } => {
                    visit(body, depth + 1, out);
                    visit(continuing, depth + 1, out);
                }
                Statement::Block(body) => visit(body, depth + 1, out),
                Statement::Switch { cases, .. } => {
                    for case in cases {
                        visit(&case.body, depth + 1, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut result = Vec::new();
    visit(block, 0, &mut result);
    result
}

/// Identify an access path without depending on WGSL spelling, whitespace or temporary names.
fn access_path(
    function: &naga::Function,
    expression: Handle<Expression>,
) -> (Handle<Expression>, Vec<u32>) {
    match function.expressions[expression] {
        Expression::Load { pointer } => access_path(function, pointer),
        Expression::AccessIndex { base, index } => {
            let (root, mut path) = access_path(function, base);
            path.push(index);
            (root, path)
        }
        _ => (expression, Vec::new()),
    }
}

/// Naga broadcasts a scalar divisor before vector division; compare the original scalar
/// access rather than that generated vector expression with the mask helper's scalar input.
fn scalar_access_path(
    function: &naga::Function,
    expression: Handle<Expression>,
) -> (Handle<Expression>, Vec<u32>) {
    match function.expressions[expression] {
        Expression::Splat { value, .. } => scalar_access_path(function, value),
        _ => access_path(function, expression),
    }
}

/// Resolve one production call result, rather than accepting a disconnected helper invocation.
fn call<'a>(
    module: &naga::Module,
    function: &'a naga::Function,
    result: Handle<Expression>,
    name: &str,
) -> &'a [Handle<Expression>] {
    statements(&function.body)
        .into_iter()
        .find_map(|(statement, _)| match statement {
            Statement::Call {
                function: callee,
                arguments,
                result: Some(value),
            } if *value == result && module.functions[*callee].name.as_deref() == Some(name) => {
                Some(arguments.as_slice())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected connected {name} call"))
}

/// Evaluate only the sampled alpha/early-return slice. Normal reconstruction, texture filtering
/// and Bevy lighting are deliberately outside this tiny evaluator; unsupported IR fails closed.
fn sampled_alpha(
    module: &naga::Module,
    function: &naga::Function,
    block: &naga::Block,
    present: f32,
    samples: [[f32; 4]; 2],
) -> f32 {
    /// Follow only the alpha lane, with distinct normal and diffuse texture fixtures.
    fn lane(
        module: &naga::Module,
        function: &naga::Function,
        expression: Handle<Expression>,
        component: usize,
        present: f32,
        samples: [[f32; 4]; 2],
    ) -> f32 {
        let eval =
            |expression, component| lane(module, function, expression, component, present, samples);
        match &function.expressions[expression] {
            Expression::Literal(naga::Literal::F32(value)) => *value,
            Expression::FunctionArgument(2) => present,
            Expression::ImageSample { image, .. } => {
                let texture = match function.expressions[*image] {
                    // The near fragment's caller binding is checked by mask_slots.
                    Expression::FunctionArgument(0) => 0,
                    Expression::GlobalVariable(image) => {
                        let name = module.global_variables[image].name.as_deref().unwrap();
                        if name.starts_with("normal_") {
                            0
                        } else if name.starts_with("diffuse_") || name.starts_with("layer_") {
                            1
                        } else {
                            panic!("unexpected sampled image {name}")
                        }
                    }
                    ref other => panic!("unsupported image binding {other:?}"),
                };
                samples[texture][component]
            }
            Expression::AccessIndex { base, index } => eval(*base, *index as usize),
            Expression::Swizzle {
                vector, pattern, ..
            } => eval(*vector, pattern[component] as usize),
            Expression::Splat { value, .. } => eval(*value, 0),
            // The vec4 constructor's final scalar is alpha, even when xyz is constructed from
            // a vec2 and a scalar. We never need to execute the normal reconstruction slice.
            Expression::Compose { components, .. } if component == 3 => {
                eval(*components.last().unwrap(), 0)
            }
            Expression::Binary {
                op: naga::BinaryOperator::Less,
                left,
                right,
            } => u8::from(eval(*left, 0) < eval(*right, 0)) as f32,
            other => panic!("unsupported alpha slice {other:?}"),
        }
    }
    for statement in block.iter() {
        match statement {
            Statement::Return { value: Some(value) } => {
                return lane(module, function, *value, 3, present, samples);
            }
            Statement::If {
                condition,
                accept,
                reject,
            } => {
                let branch = if lane(module, function, *condition, 0, present, samples) != 0.0 {
                    accept
                } else {
                    reject
                };
                if !branch.is_empty() {
                    return sampled_alpha(module, function, branch, present, samples);
                }
            }
            Statement::Emit(_) => {}
            other => panic!("unsupported alpha control flow {other:?}"),
        }
    }
    panic!("alpha slice did not return")
}

/// Check the fragment's actual stores/calls and recover its contributing normal slots. This is
/// an IR wiring check plus sampled-parameter test, not execution of a complete fragment shader.
fn mask_slots(
    module: &naga::Module,
    merged: bool,
    slots: usize,
) -> (Vec<usize>, Handle<Expression>) {
    let fragment = &module
        .entry_points
        .iter()
        .find(|entry| entry.stage == naga::ShaderStage::Fragment)
        .unwrap()
        .function;
    let body = statements(&fragment.body);
    let (apply_index, arguments, result) = body
        .iter()
        .enumerate()
        .find_map(|(index, (statement, depth))| match statement {
            Statement::Call {
                function,
                arguments,
                result: Some(result),
            } if module.functions[*function].name.as_deref() == Some("terrain_reflectance") => {
                assert_eq!(*depth, 0, "all-absent normals must still mask reflectance");
                Some((index, arguments, *result))
            }
            _ => None,
        })
        .expect("fragment must apply the mask");
    let (reflectance_root, reflectance_path) = access_path(fragment, arguments[0]);
    // Member indices are from this file's controlled PBR_TYPES stand-in, not Bevy internals.
    assert_eq!(reflectance_path, [0, 2]);
    let store_index = body
        .iter()
        .position(|(statement, depth)| {
            matches!(statement,
                Statement::Store { pointer, value } if *depth == 0 && *value == result
                    && access_path(fragment, *pointer) == (reflectance_root, vec![0, 2])
            )
        })
        .expect("masked reflectance must be stored on the lit material");
    let lighting_index = body
        .iter()
        .position(|(statement, _)| match statement {
            Statement::Call {
                function,
                arguments,
                ..
            } if module.functions[*function]
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with("apply_pbr_lighting")) =>
            {
                assert_eq!(
                    access_path(fragment, arguments[0]),
                    (reflectance_root, vec![])
                );
                true
            }
            _ => false,
        })
        .unwrap();
    assert!(apply_index < store_index && store_index < lighting_index);
    // The mask must use the same denominator as colour, including missing-normal layers.
    assert!(
        fragment
            .expressions
            .iter()
            .any(|(_, expression)| matches!(expression,
                Expression::Binary { op: naga::BinaryOperator::Divide, right, .. }
            if scalar_access_path(fragment, *right) == scalar_access_path(fragment, arguments[2])
            ))
    );
    let (mask_root, path) = access_path(fragment, arguments[1]);
    assert!(path.is_empty());
    let Expression::LocalVariable(mask) = fragment.expressions[mask_root] else {
        panic!("expected mask accumulator")
    };
    assert!(matches!(
        fragment.expressions[fragment.local_variables[mask].init.unwrap()],
        Expression::Literal(naga::Literal::F32(0.0))
    ));
    let mask_values: Vec<_> = body
        .iter()
        .filter_map(|(statement, _)| match statement {
            Statement::Store { pointer, value } if *pointer == mask_root => Some(*value),
            _ => None,
        })
        .collect();
    if merged {
        let (count_handle, count) = module
            .constants
            .iter()
            .find(|(_, constant)| constant.name.as_deref() == Some("SLOT_COUNT"))
            .unwrap();
        assert!(
            matches!(module.global_expressions[count.init], Expression::Literal(naga::Literal::U32(value)) if value as usize == slots)
        );
        assert_eq!(fragment.expressions.iter().filter(|(_, expression)| matches!(expression,
            Expression::Binary { op: naga::BinaryOperator::Less, right, .. }
                if matches!(fragment.expressions[*right], Expression::Constant(constant) if constant == count_handle)
        )).count(), 2, "both paint and sampling loops must include the last slot");
        if mask_values.is_empty() {
            // No normal classes: only the zero initializer, still connected to reflectance.
            assert!(
                !module
                    .global_variables
                    .iter()
                    .any(|(_, global)| global.name.as_deref() == Some("normal_0"))
            );
        } else {
            assert_eq!(mask_values.len(), 1);
            let Expression::Binary {
                op: naga::BinaryOperator::Add,
                left,
                right,
            } = fragment.expressions[mask_values[0]]
            else {
                panic!("mask must accumulate")
            };
            assert_eq!(access_path(fragment, left).0, mask_root);
            let Expression::Binary {
                op: naga::BinaryOperator::Multiply,
                left,
                right: weight,
            } = fragment.expressions[right]
            else {
                panic!("mask must use paint weight")
            };
            let (sample, channel) = access_path(fragment, left);
            assert_eq!(channel, [3], "specular intensity must use alpha");
            let normal = call(module, fragment, sample, "sample_normal");
            // Both class and array layer must come from entry.y (normal), never entry.x (diffuse).
            for argument in [normal[0], normal[2]] {
                let Expression::Binary { left, .. } = fragment.expressions[argument] else {
                    panic!("expected packed normal lookup")
                };
                assert_eq!(access_path(fragment, left).1, [1]);
            }
            let (weight_root, _) = access_path(fragment, weight);
            assert!(
                matches!(fragment.expressions[weight_root], Expression::LocalVariable(local) if fragment.local_variables[local].name.as_deref() == Some("weight"))
            );
            assert!(body.iter().any(|(statement, _)| match statement {
                Statement::If { condition, accept, reject } => {
                    let Expression::Binary { op: naga::BinaryOperator::NotEqual, left, right } = fragment.expressions[*condition] else { return false };
                    access_path(fragment, left).1 == [1]
                        && matches!(fragment.expressions[right], Expression::Constant(constant)
                            if module.constants[constant].name.as_deref() == Some("NO_NORMAL_WORD"))
                        && statements(accept).iter().any(|(statement, _)| matches!(statement, Statement::Store { pointer, .. } if *pointer == mask_root))
                        && !statements(reject).iter().any(|(statement, _)| matches!(statement, Statement::Store { pointer, .. } if *pointer == mask_root))
                }
                _ => false,
            }), "missing-normal sentinel must guard mask accumulation");
        }
        ((0..slots).collect(), arguments[2])
    } else {
        assert_eq!(mask_values.len(), 1);
        let (surface, channel) = access_path(fragment, mask_values[0]);
        assert_eq!(channel, [3]);
        let terms: Vec<_> = body
            .iter()
            .filter_map(|(statement, _)| match statement {
                Statement::Store { pointer, value } if *pointer == surface => Some(*value),
                _ => None,
            })
            .collect();
        let mut recovered = Vec::new();
        for term in terms {
            let product = match fragment.expressions[term] {
                Expression::Binary {
                    op: naga::BinaryOperator::Add,
                    left,
                    right,
                } => {
                    assert_eq!(access_path(fragment, left).0, surface);
                    right
                }
                _ => term,
            };
            let Expression::Binary {
                op: naga::BinaryOperator::Multiply,
                left,
                right,
            } = fragment.expressions[product]
            else {
                panic!("surface must use paint weight")
            };
            let (_, weight_index) = access_path(fragment, right);
            assert_eq!(weight_index.len(), 1);
            let slot = weight_index[0] as usize;
            let normal = call(module, fragment, left, "layer_surface");
            let Expression::GlobalVariable(image) = fragment.expressions[normal[0]] else {
                panic!("expected bound normal")
            };
            assert_eq!(
                module.global_variables[image].name.as_deref(),
                Some(format!("normal_{slot}").as_str())
            );
            let (presence_root, presence_path) = access_path(fragment, normal[2]);
            let Expression::GlobalVariable(settings) = fragment.expressions[presence_root] else {
                panic!("expected normal-presence uniform")
            };
            let naga::TypeInner::Struct { ref members, .. } =
                module.types[module.global_variables[settings].ty].inner
            else {
                panic!("expected terrain settings")
            };
            assert_eq!(presence_path.len(), 2);
            assert_eq!(
                members[presence_path[0] as usize].name.as_deref(),
                Some(format!("normal_layers_{}", slot / 4).as_str())
            );
            assert_eq!(presence_path[1], (slot % 4) as u32);
            recovered.push(slot);
        }
        assert_eq!(
            recovered,
            [0, 1, 2, 3, 4, 5],
            "every near layer contributes exactly once"
        );
        (recovered, arguments[2])
    }
}

/// Evaluate the production base-weight/denominator arithmetic with fixed paint inputs. The
/// merged overlay reduction and near weight-grid interpolation are fixture boundaries, not
/// interpreted shader execution. Unsupported changes require explicitly reviewing this slice.
fn paint_scalar(function: &naga::Function, expression: Handle<Expression>, weights: &[f32]) -> f32 {
    let eval = |expression| paint_scalar(function, expression, weights);
    match function.expressions[expression] {
        Expression::Literal(naga::Literal::F32(value)) => value,
        Expression::Load { pointer } => eval(pointer),
        Expression::AccessIndex { .. } => {
            let (root, path) = access_path(function, expression);
            assert_eq!(path.len(), 1);
            assert!(
                matches!(function.expressions[root], Expression::LocalVariable(local)
                if function.local_variables[local].name.as_deref() == Some("weights"))
            );
            weights[path[0] as usize]
        }
        Expression::LocalVariable(local)
            if function.local_variables[local].name.as_deref() == Some("overlay_sum") =>
        {
            weights[1..].iter().sum()
        }
        Expression::Binary {
            op: naga::BinaryOperator::Add,
            left,
            right,
        } => eval(left) + eval(right),
        Expression::Binary {
            op: naga::BinaryOperator::Subtract,
            left,
            right,
        } => eval(left) - eval(right),
        Expression::Math {
            fun: naga::MathFunction::Max,
            arg,
            arg1: Some(right),
            ..
        } => eval(arg).max(eval(right)),
        ref other => panic!("unsupported paint slice {other:?}"),
    }
}

/// Normal-alpha sampling and fragment wiring for near, merged 8/16 and zero-normal-class paths.
/// Fixtures use unequal weights, an absent middle normal and the last slot (including slot 15).
/// Diffuse alpha is independent; filtering, real uniforms, grid interpolation and device lighting
/// still require device coverage. No full-fragment or native Skyrim parity is claimed here.
#[test]
fn terrain_normal_alpha_sampling_blending_and_fragment_wiring() {
    for (source, merged, slots, normals) in [
        (include_str!("../shaders/terrain.wgsl"), false, 6, 1),
        (SHADER, true, 8, 1),
        (SHADER, true, 16, 1),
        (SHADER, true, 8, 0),
        (SHADER, true, 16, 0),
    ] {
        let defs = if merged {
            shader_defs(key(1, normals), slots == 16)
        } else {
            vec![]
        };
        let module = compose_module(source, defs).unwrap();
        let (contributing, denominator) = mask_slots(&module, merged, slots);
        let fragment = &module
            .entry_points
            .iter()
            .find(|entry| entry.stage == naga::ShaderStage::Fragment)
            .unwrap()
            .function;
        let helper = module
            .functions
            .iter()
            .find(|(_, function)| {
                function.name.as_deref()
                    == Some(if merged {
                        "sample_normal"
                    } else {
                        "layer_surface"
                    })
            })
            .unwrap()
            .1;
        let sample_block = if merged && normals != 0 {
            let Statement::Switch { cases, .. } = helper
                .body
                .iter()
                .find(|statement| matches!(statement, Statement::Switch { .. }))
                .unwrap()
            else {
                unreachable!()
            };
            let fallback = &cases
                .iter()
                .find(|case| case.value == naga::SwitchValue::Default)
                .unwrap()
                .body;
            assert_eq!(
                sampled_alpha(&module, helper, fallback, 1.0, [[0.5, 0.5, 1.0, 1.0]; 2]),
                0.0
            );
            &cases
                .iter()
                .find(|case| case.value == naga::SwitchValue::U32(0))
                .unwrap()
                .body
        } else {
            &helper.body
        };
        if merged && normals == 0 {
            assert_eq!(
                sampled_alpha(
                    &module,
                    helper,
                    sample_block,
                    1.0,
                    [[0.5, 0.5, 1.0, 1.0]; 2]
                ),
                0.0
            );
        }
        for diffuse_alpha in [0.0, 0.37, 1.0] {
            for (overlays, alphas, all_absent, expected) in [
                ([0.15, 0.65], [0.0, 0.0], false, 0.0),
                ([0.15, 0.65], [0.25, 0.25], false, 0.2125),
                ([0.15, 0.65], [1.0, 1.0], false, 0.85),
                ([0.0, 0.65], [1.0, 1.0], false, 1.0),
                ([0.15, 0.65], [0.25, 1.0], false, 0.7),
                ([0.15, 0.65], [1.0, 1.0], true, 0.0),
                ([0.4, 1.1], [0.25, 0.25], false, 1.1 * 0.25 / 1.5),
            ] {
                let mut weights = vec![0.0_f32; slots];
                weights[1] = overlays[0];
                weights[slots - 1] = overlays[1];
                weights[0] = (1.0 - overlays[0] - overlays[1]).max(0.0);
                if !merged {
                    let base = statements(&fragment.body).into_iter().find_map(|(statement, _)| match statement {
                        Statement::Store { pointer, value } if access_path(fragment, *pointer).1 == [0]
                            && matches!(fragment.expressions[access_path(fragment, *pointer).0], Expression::LocalVariable(local)
                                if fragment.local_variables[local].name.as_deref() == Some("weights")) => Some(*value),
                        _ => None,
                    }).unwrap();
                    let base_weight = paint_scalar(fragment, base, &weights);
                    assert!((base_weight - weights[0]).abs() < 1.0e-6);
                    weights[0] = base_weight;
                }
                let total = paint_scalar(fragment, denominator, &weights);
                assert!((total - weights.iter().sum::<f32>()).abs() < 1.0e-6);
                let mut mask_sum = 0.0;
                for slot in &contributing {
                    let present = !all_absent && *slot != 1 && normals != 0;
                    let alpha = alphas[usize::from(*slot != 0)];
                    let sampled = if merged && !present {
                        0.0
                    } else {
                        sampled_alpha(
                            &module,
                            helper,
                            sample_block,
                            u8::from(present) as f32,
                            [[0.5, 0.5, 1.0, alpha], [0.2, 0.4, 0.6, diffuse_alpha]],
                        )
                    };
                    mask_sum += sampled * weights[*slot];
                }
                let actual = shader_reflectance(source, [0.5; 3], mask_sum, total)[0];
                let expected = if normals == 0 { 0.0 } else { expected };
                assert!(
                    (0.16 * actual * actual - 0.04 * expected).abs() < 1.0e-6,
                    "merged={merged}, slots={slots}, normals={normals}, diffuse alpha={diffuse_alpha}, alphas={alphas:?}, absent={all_absent}"
                );
            }
        }
    }
}

/// Prove the IR guard rejects a computed mask whose result never reaches the lit material.
#[test]
#[should_panic(expected = "masked reflectance must be stored on the lit material")]
fn terrain_fragment_wiring_rejects_a_disconnected_reflectance_store() {
    let mut module = compose_module(SHADER, shader_defs(key(1, 1), false)).unwrap();
    let fragment = &mut module
        .entry_points
        .iter_mut()
        .find(|entry| entry.stage == naga::ShaderStage::Fragment)
        .unwrap()
        .function;
    let (result, original) = fragment
        .body
        .iter()
        .find_map(|statement| match statement {
            Statement::Call {
                function,
                arguments,
                result: Some(result),
            } if module.functions[*function].name.as_deref() == Some("terrain_reflectance") => {
                Some((*result, arguments[0]))
            }
            _ => None,
        })
        .unwrap();
    for statement in fragment.body.iter_mut() {
        if let Statement::Store { value, .. } = statement
            && *value == result
        {
            *value = original;
        }
    }
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap();
    mask_slots(&module, true, 8);
}

/// Prove that a type-compatible diffuse sample cannot silently replace the normal-alpha source.
#[test]
#[should_panic(expected = "expected connected sample_normal call")]
fn terrain_fragment_wiring_rejects_diffuse_alpha_as_the_mask() {
    let mut module = compose_module(SHADER, shader_defs(key(1, 1), false)).unwrap();
    let diffuse = module
        .functions
        .iter()
        .find(|(_, function)| function.name.as_deref() == Some("sample_diffuse"))
        .unwrap()
        .0;
    let fragment = &mut module
        .entry_points
        .iter_mut()
        .find(|entry| entry.stage == naga::ShaderStage::Fragment)
        .unwrap()
        .function;
    // Calls inside the sampling loop are nested: mutate their callee without changing types,
    // bindings, paint weights, or the downstream reflectance helper.
    /// Replace the normal sampler call throughout this fragment's structured body.
    fn substitute(
        block: &mut naga::Block,
        functions: &naga::Arena<naga::Function>,
        diffuse: Handle<naga::Function>,
    ) {
        for statement in block.iter_mut() {
            match statement {
                Statement::Call { function, .. }
                    if functions[*function].name.as_deref() == Some("sample_normal") =>
                {
                    *function = diffuse
                }
                Statement::Loop {
                    body, continuing, ..
                } => {
                    substitute(body, functions, diffuse);
                    substitute(continuing, functions, diffuse);
                }
                Statement::Block(body) => substitute(body, functions, diffuse),
                Statement::Switch { cases, .. } => {
                    for case in cases {
                        substitute(&mut case.body, functions, diffuse);
                    }
                }
                Statement::If { accept, reject, .. } => {
                    substitute(accept, functions, diffuse);
                    substitute(reject, functions, diffuse);
                }
                _ => {}
            }
        }
    }
    substitute(&mut fragment.body, &module.functions, diffuse);
    for (_, expression) in fragment.expressions.iter_mut() {
        if let Expression::CallResult(function) = expression
            && module.functions[*function].name.as_deref() == Some("sample_normal")
        {
            *function = diffuse;
        }
    }
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap();
    mask_slots(&module, true, 8);
}

/// Bevy 0.19.0's ordinary punctual Fresnel and multiscatter factor, transcribed from
/// https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_pbr/src/render/pbr_lighting.wgsl
/// (`fresnel`, `F_Schlick_vec`, `specular_multiscatter`, MIT/Apache-2.0, Bevy contributors).
/// D, V and intensity are held at one; F_ab is a finite test input, not an evaluated BRDF LUT.
fn bevy_019_fresnel_compensated(f0: f32, l_dot_h: f32) -> f32 {
    let f90 = (3.0 * f0 * (50.0 * 0.33)).clamp(0.0, 1.0);
    let fresnel = f0 + (f90 - f0) * (1.0 - l_dot_h).powi(5);
    fresnel * (1.0 + f0 * (1.0 / (0.6 + 0.1) - 1.0))
}

/// Pin the reference to the resolved dependency; re-review these expressions on Bevy upgrades.
/// Partial masks scale F0, not the complete lobe. This excludes clearcoat/metallic and LTC lights.
#[test]
fn terrain_zero_mask_extinguishes_bevy_019_punctual_fresnel_at_all_angles() {
    let lock = include_str!("../../../../Cargo.lock");
    let pbr = lock
        .split("[[package]]")
        .find(|package| package.lines().any(|line| line == "name = \"bevy_pbr\""))
        .unwrap();
    assert!(
        pbr.lines().any(|line| line == "version = \"0.19.0\""),
        "re-review the pinned Bevy Fresnel reference"
    );
    for source in [SHADER, include_str!("../shaders/terrain.wgsl")] {
        for (mask, normal, grazing) in [
            (0.0, 0.0, 0.0),
            (0.25, 0.010042857, 0.49712143),
            (1.0, 0.040685713, 1.0171429),
        ] {
            let reflectance = shader_reflectance(source, [0.5; 3], mask, 1.0)[0];
            let f0 = 0.16 * reflectance * reflectance;
            for l_dot_h in [0.0, 0.01, 0.5, 0.99, 1.0] {
                let actual = bevy_019_fresnel_compensated(f0, l_dot_h);
                assert!(actual.is_finite());
                if mask == 0.0 {
                    assert_eq!(actual, 0.0);
                }
            }
            assert!((bevy_019_fresnel_compensated(f0, 1.0) - normal).abs() < 1.0e-6);
            assert!((bevy_019_fresnel_compensated(f0, 0.0) - grazing).abs() < 1.0e-6);
        }
    }
}

/// The quadrant path's meshes carry tangents, so Bevy never flips their normal on a back face; the
/// merged mesh has none, so the shader must report every fragment as a front face to get the same
/// lighting. The GPU probe renders a turned-over cell to check the result; this pins the source.
#[test]
fn back_faces_are_not_flipped_to_match_the_quadrant_path() {
    assert!(SHADER.contains("pbr_input_from_standard_material(standard_in, true)"));
    assert!(!SHADER.contains("front_facing"));
}
