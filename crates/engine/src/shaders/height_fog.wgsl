// SPDX-License-Identifier: GPL-3.0-only
//
// Exponential height fog, ported from Community Shaders for Skyrim:
//   features/Exponential Height Fog/Shaders/ExponentialHeightFog/ExponentialHeightFog.hlsli
//     (GetExponentialHeightFogInternal, analytical path)
//   features/Exponential Height Fog/Shaders/ExponentialHeightFog/VolumetricFogCommon.hlsli
//     (HenyeyGreenstein)
//   package/Shaders/ISSAOComposite.hlsl (screen-space composite over geometry and sky)
// Copyright (C) the Community Shaders contributors,
// https://github.com/community-shaders/skyrim-community-shaders, GPL-3.0.
// Modified for Mudcrab: translated from HLSL to WGSL, Y-up world space, reverse-Z depth,
// weather Fog Far colour as the base inscattering colour, no volumetric or IBL terms.

#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput
#import bevy_render::view::View

@group(0) @binding(0) var screen_texture: texture_2d<f32>;
@group(0) @binding(1) var texture_sampler: sampler;
@group(0) @binding(2) var<uniform> fog: HeightFog;
@group(0) @binding(3) var<uniform> view: View;
#ifdef MULTISAMPLED
@group(0) @binding(4) var depth_texture: texture_depth_multisampled_2d;
#else
@group(0) @binding(4) var depth_texture: texture_depth_2d;
#endif

struct HeightFog {
    // Weather Fog Far colour, already in the scene's post-exposure units.
    inscattering_color: vec3<f32>,
    directional_inscattering: f32,
    // Unit vector from the scene towards the sun.
    sun_direction: vec3<f32>,
    directional_anisotropy: f32,
    // Sun colour times illuminance; the shader applies the view's exposure.
    sun_color: vec3<f32>,
    start_distance: f32,
    density: f32,
    height_falloff: f32,
    height: f32,
    sky_distance: f32,
    density2: f32,
    height_falloff2: f32,
    height2: f32,
    _padding: f32,
}

const PI: f32 = 3.141592653589793;

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = 1.0 + g2 - 2.0 * g * cos_theta;
    return (1.0 - g2) / (4.0 * PI * pow(max(denom, 1e-5), 1.5));
}

// exp2-based density integrated along the ray, with the Taylor fallback near horizontal rays
// where (1 - exp2(-x)) / x loses precision.
fn line_integral(falloff: f32) -> f32 {
    if abs(falloff) > 0.01 {
        return (1.0 - exp2(-falloff)) / falloff;
    }
    return 0.69314718056 - 0.24022650695 * falloff;
}

// Camera-relative position in, fog colour and opacity out.
fn exponential_height_fog(view_to_pos: vec3<f32>, camera_height: f32) -> vec4<f32> {
    let length_to_pos = length(view_to_pos);
    let inv_length = 1.0 / max(length_to_pos, 1e-4);

    var ray_origin_terms = fog.density * exp2(-fog.height_falloff * max(camera_height - fog.height, 0.0));
    var ray_origin_terms2 = fog.density2 * exp2(-fog.height_falloff2 * max(camera_height - fog.height2, 0.0));
    var ray_length = length_to_pos;
    var ray_direction_y = view_to_pos.y;

    if fog.start_distance > 0.0 {
        let exclude_distance = min(fog.start_distance, length_to_pos);
        let exclude_time = exclude_distance * inv_length;
        let camera_to_exclusion_y = exclude_time * view_to_pos.y;
        let exclusion_y = camera_height + camera_to_exclusion_y;
        ray_length = (1.0 - exclude_time) * length_to_pos;
        ray_direction_y = view_to_pos.y - camera_to_exclusion_y;
        ray_origin_terms = fog.density * exp2(-fog.height_falloff * max(exclusion_y - fog.height, 0.0));
        ray_origin_terms2 = fog.density2 * exp2(-fog.height_falloff2 * max(exclusion_y - fog.height2, 0.0));
    }

    let integral = ray_origin_terms * line_integral(fog.height_falloff * ray_direction_y)
        + ray_origin_terms2 * line_integral(fog.height_falloff2 * ray_direction_y);
    let transmittance = saturate(exp2(-integral * ray_length));

    var color = fog.inscattering_color;
    if fog.directional_inscattering > 0.0 {
        let cos_theta = dot(fog.sun_direction, view_to_pos * inv_length);
        let phase = henyey_greenstein(cos_theta, fog.directional_anisotropy);
        // The reference's light colour is the radiance a lit white surface reflects; Bevy's
        // illuminance reaches that only after exposure and the Lambert 1/pi.
        color += fog.sun_color * (view.exposure / PI) * phase * fog.directional_inscattering;
    }
    return vec4<f32>(color, 1.0 - transmittance);
}

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let scene = textureSample(screen_texture, texture_sampler, in.uv);
    // The last argument is the mip level for a plain depth texture and the sample index for a
    // multisampled one; zero is the right choice for both.
    let depth = textureLoad(depth_texture, vec2<i32>(in.position.xy), 0);

    let ndc = vec2<f32>(in.uv.x * 2.0 - 1.0, 1.0 - in.uv.y * 2.0);
    var view_to_pos: vec3<f32>;
    if depth > 0.0 {
        let world = view.world_from_clip * vec4<f32>(ndc, depth, 1.0);
        view_to_pos = world.xyz / world.w - view.world_position;
    } else {
        // Reverse-Z clears the sky to zero (infinitely far), so the sky is fogged out to a fixed
        // distance along its view ray instead, as the far plane does for the reference.
        let far = view.world_from_clip * vec4<f32>(ndc, 1.0e-7, 1.0);
        view_to_pos = normalize(far.xyz / far.w - view.world_position) * fog.sky_distance;
    }

    let height_fog = exponential_height_fog(view_to_pos, view.world_position.y);
    return vec4<f32>(mix(scene.rgb, height_fog.rgb, height_fog.a), scene.a);
}
