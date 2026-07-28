// koma — single pipeline for both solid quads (backgrounds, cursor, dividers)
// and glyph quads (alpha-mask sampled from the atlas).

struct Globals {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct Inst {
    @location(0) rect: vec4<f32>,  // x, y, w, h in physical pixels
    @location(1) uv: vec4<f32>,    // u0, v0, u1, v1
    @location(2) color: vec4<f32>,
    @location(3) mode: u32,        // 0 = solid fill, 1 = glyph alpha mask
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) mode: u32,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    // 4-vertex triangle strip: 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1)
    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let px = inst.rect.xy + corner * inst.rect.zw;
    let ndc = vec2<f32>(
        px.x / globals.screen.x * 2.0 - 1.0,
        1.0 - px.y / globals.screen.y * 2.0,
    );

    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = mix(inst.uv.xy, inst.uv.zw, corner);
    out.color = inst.color;
    out.mode = inst.mode;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // Output is premultiplied; the pipeline uses PREMULTIPLIED_ALPHA_BLENDING.
    if in.mode == 0u {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    let a = textureSample(atlas_tex, atlas_smp, in.uv).r;
    return vec4<f32>(in.color.rgb * in.color.a * a, in.color.a * a);
}
