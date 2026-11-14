// Resolves the opaque pass's bed depth into a single-sample red channel the water shader reads.
//
// The main opaque pass stores its depth at the end of the pass, but the transparent pass that draws
// water attaches the same depth texture, so water cannot sample it directly. `water_depth.rs` runs
// this pass between the two and copies the depth out. Reverse-Z depth means the nearest bed among a
// pixel's samples is the largest value, which is what the water's path reconstruction wants.

#ifdef MULTISAMPLED
@group(0) @binding(0) var bed_depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(0) var bed_depth: texture_depth_2d;
#endif

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) f32 {
    let coord = vec2<i32>(position.xy);
    var ndc = 0.0;
    for (var sample = 0u; sample < #{BED_DEPTH_SAMPLES}u; sample += 1u) {
#ifdef MULTISAMPLED
        ndc = max(ndc, textureLoad(bed_depth, coord, sample));
#else
        ndc = max(ndc, textureLoad(bed_depth, coord, 0));
#endif
    }
    return ndc;
}
