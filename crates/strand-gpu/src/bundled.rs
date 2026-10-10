//! design.md's bundled GPU effects (`Bundled`), as WGSL run by the same
//! pass pipeline as a `.wgsl` file (docs/architecture.md, "`strand-gpu`",
//! the ABI): each is a [`ShaderCode`] with four `vec4` uniforms `u0`–`u3`
//! at `@group(1)` bindings 0–3, filled from `ShaderPass::uniforms` in
//! order (16 floats; missing ones are zero). Render's builders lay the
//! knobs out (decisions.md, m4-gpu-effects); slot 0's first float is the
//! effect's reach where it has one (`Bundled::reach`).
//!
//! | Effect | Input | `u0` | `u1`–`u3` |
//! |---|---|---|---|
//! | `bloom` | content | radius px, strength | |
//! | `chromatic` | content | offset px | |
//! | `crt` | content | | |
//! | `wobble` | content | amplitude px, wavelength px, period s | |
//! | `tilt` | content | turn about x, about y (radians) | |
//! | `glass` | backdrop | refraction px, corner radius px, dispersion, frost px | |
//! | `backdrop_blur` | backdrop | σ px | |
//! | `aurora` | none | first curtain's colour (straight) | second, third colour; `u3.x` speed |
//!
//! `backdrop_blur` runs as two steps (across, then down). `particles`
//! is not a fragment pass: it draws sprites instanced ([`particles`]).

use std::sync::{Arc, OnceLock};

use strand_scene::{Bundled, ShaderCode, UniformType};

/// The uniforms every bundled module declares, and helpers.
const COMMON: &str = "
@group(1) @binding(0) var<uniform> u0: vec4<f32>;
@group(1) @binding(1) var<uniform> u1: vec4<f32>;
@group(1) @binding(2) var<uniform> u2: vec4<f32>;
@group(1) @binding(3) var<uniform> u3: vec4<f32>;
const TAU: f32 = 6.2831853;
// The input at buffer pixel `p`; transparent outside it.
fn at(p: vec2<f32>) -> vec4<f32> {
    let uv = p / strand.size;
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0);
    }
    return textureSampleLevel(strand_input, strand_sampler, uv, 0.0);
}
// The input at `p`, its edge repeated (a backdrop goes on past its box).
fn edge(p: vec2<f32>) -> vec4<f32> {
    let uv = clamp(p / strand.size, vec2<f32>(0.0), vec2<f32>(1.0));
    return textureSampleLevel(strand_input, strand_sampler, uv, 0.0);
}
";

/// Bright parts bleed light: a Gaussian (σ = radius / 3) of the parts
/// whose straight luma passes a soft knee (0.2 to 0.7), times the
/// strength, screened over the input.
/// 64 taps on a golden-angle spiral cover the disc.
const BLOOM: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let p = v.pos.xy;
    let base = at(p);
    let r = u0.x;
    if (r < 0.5) {
        return base;
    }
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var i = 0u; i < 64u; i = i + 1u) {
        let f = (f32(i) + 0.5) / 64.0;
        let a = f32(i) * 2.3999632;
        let s = at(p + vec2<f32>(cos(a), sin(a)) * sqrt(f) * r);
        let w = exp(-4.5 * f);
        let l = dot(s.rgb, vec3<f32>(0.2126, 0.7152, 0.0722)) / max(s.a, 0.0001);
        acc = acc + s * (w * smoothstep(0.2, 0.7, l));
        wsum = wsum + w;
    }
    let g = clamp(acc / wsum * max(u0.y, 0.0), vec4<f32>(0.0), vec4<f32>(1.0));
    return base + g - base * g;
}
";

/// Red from a copy `offset` to the left, blue from one to the right.
const CHROMATIC: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let p = v.pos.xy;
    let o = vec2<f32>(u0.x, 0.0);
    let c = at(p);
    let r = at(p - o);
    let b = at(p + o);
    return vec4<f32>(r.r, c.g, b.b, max(c.a, max(r.a, b.a)));
}
";

/// A curved screen (barrel distortion, the corners cut round), a hint of
/// colour fringing, scanlines every 3 logical pixels and a vignette.
const CRT: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let size = strand.size;
    let c = v.pos.xy / size * 2.0 - 1.0;
    let k = 0.06;
    let d = c * (1.0 + k * dot(c, c)) / (1.0 + k);
    let q = (d * 0.5 + 0.5) * size;
    // How far inside the curved screen, in pixels.
    let inside = min(min(q.x, size.x - q.x), min(q.y, size.y - q.y));
    let cover = clamp(inside + 0.5, 0.0, 1.0);
    let f = vec2<f32>(0.6 * strand.scale, 0.0);
    let g = at(q);
    let col = vec4<f32>(at(q - f).r, g.g, at(q + f).b, g.a);
    let pitch = 3.0 * max(strand.scale, 1.0);
    let scan = 0.8 + 0.2 * cos(TAU * v.pos.y / pitch);
    let vig = 1.0 - 0.2 * dot(c, c) * dot(c, c);
    let a = max(col.a, max(at(q - f).a, at(q + f).a));
    return vec4<f32>(col.rgb * scan * vig, a) * cover;
}
";

/// Rows slide sideways along a travelling sine.
const WOBBLE: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let p = v.pos.xy;
    let len = max(u0.y, 1.0);
    let period = max(u0.z, 0.01);
    let phase = TAU * (p.y / len - strand.time / period);
    return at(p + vec2<f32>(u0.x * sin(phase), 0.0));
}
";

/// The input as a card turned about its x and y axes, seen in
/// perspective from 2.5 × its longer side, scaled so the turned card
/// just fits the box.
const TILT: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let size = strand.size;
    let h = size * 0.5;
    let c = v.pos.xy - h;
    let cx = cos(u0.x);
    let sx = sin(u0.x);
    let cy = cos(u0.y);
    let sy = sin(u0.y);
    // R = Ry * Rx: its columns are the card's axes and normal.
    let ex = vec3<f32>(cy, 0.0, -sy);
    let ey = vec3<f32>(sy * sx, cx, cy * sx);
    let n = vec3<f32>(sy * cx, -sx, cy * cx);
    let dist = max(size.x, size.y) * 2.5;
    // How far the turned corners reach, as a share of the box.
    var reach = vec2<f32>(0.0);
    for (var i = 0u; i < 4u; i = i + 1u) {
        let k = vec2<f32>(f32(i & 1u), f32(i >> 1u)) * 2.0 - 1.0;
        let w = ex * (k.x * h.x) + ey * (k.y * h.y);
        let p = w.xy * dist / max(dist - w.z, 1.0);
        reach = max(reach, abs(p) / h);
    }
    let f = max(max(reach.x, reach.y), 0.0001);
    let eye = vec3<f32>(0.0, 0.0, dist);
    let dir = vec3<f32>(c * f, 0.0) - eye;
    let denom = dot(n, dir);
    if (abs(denom) < 0.000001) {
        return vec4<f32>(0.0);
    }
    let hit = eye + dir * (-dot(n, eye) / denom);
    let q = vec2<f32>(dot(hit, ex), dot(hit, ey)) + h;
    let inside = min(min(q.x, size.x - q.x), min(q.y, size.y - q.y));
    let cover = clamp(inside + 0.5, 0.0, 1.0);
    return textureSampleLevel(strand_input, strand_sampler, q / size, 0.0) * cover;
}
";

/// Liquid glass over the backdrop: a convex rim that bends what is
/// behind towards the middle, the bend split by colour (dispersion), a
/// light frost, a fresnel rim, and a highlight under the pointer with
/// the rim facing it lit.
const GLASS: &str = "
fn sd_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}
fn frosted(p: vec2<f32>) -> vec4<f32> {
    let r = u0.w;
    var acc = edge(p);
    for (var i = 0u; i < 8u; i = i + 1u) {
        let a = f32(i) * TAU / 8.0;
        acc = acc + edge(p + vec2<f32>(cos(a), sin(a)) * r);
    }
    return acc / 9.0;
}
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let size = strand.size;
    let h = size * 0.5;
    let p = v.pos.xy;
    let c = p - h;
    let r = clamp(u0.y, 0.0, min(h.x, h.y));
    let d = sd_box(c, h, r);
    let gx = sd_box(c + vec2<f32>(1.0, 0.0), h, r) - sd_box(c - vec2<f32>(1.0, 0.0), h, r);
    let gy = sd_box(c + vec2<f32>(0.0, 1.0), h, r) - sd_box(c - vec2<f32>(0.0, 1.0), h, r);
    let n = normalize(vec2<f32>(gx, gy) + vec2<f32>(0.000001));
    let bevel = max(u0.x * 2.0, 1.0);
    let e = 1.0 - clamp(-d / bevel, 0.0, 1.0);
    let disp = -n * (e * e * u0.x);
    let k = u0.z;
    let fr = frosted(p + disp * (1.0 + k));
    let fg = frosted(p + disp);
    let fb = frosted(p + disp * (1.0 - k));
    let a = max(fg.a, max(fr.a, fb.a));
    var col = vec3<f32>(fr.r, fg.g, fb.b);
    col = col + (vec3<f32>(a) - col) * 0.06;
    var light = 0.35 * e * e * e;
    if (strand.pointer.x >= 0.0) {
        let l = strand.pointer - p;
        let dist = length(l);
        let sigma = 0.35 * min(size.x, size.y);
        light = light + 0.25 * exp(-dist * dist / (2.0 * sigma * sigma));
        let facing = max(dot(n, l / max(dist, 0.0001)), 0.0);
        light = light + 0.4 * e * e * pow(facing, 4.0);
    }
    col = col + (vec3<f32>(a) - col) * clamp(light, 0.0, 1.0);
    return vec4<f32>(col, a) * clamp(0.5 - d, 0.0, 1.0);
}
";

/// One axis of a Gaussian of σ = `u0.x` along `u0.yz`, out to 3σ in at
/// most 65 taps (bilinear in between).
const BACKDROP_BLUR: &str = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let p = v.pos.xy;
    let sigma = max(u0.x, 0.01);
    let dir = u0.yz;
    let r = ceil(3.0 * sigma);
    let stride = max(1.0, r / 32.0);
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    var i = -r;
    loop {
        if (i > r) {
            break;
        }
        let w = exp(-(i * i) / (2.0 * sigma * sigma));
        acc = acc + edge(p + dir * i) * w;
        wsum = wsum + w;
        i = i + stride;
    }
    return acc / wsum;
}
";

/// The CPU fallback's three curtains (`effects::builtin::aurora` in
/// strand-render, the same formula), animated by `strand.time`.
const AURORA: &str = "
fn curtain(j: u32) -> vec4<f32> {
    if (j == 0u) {
        return u0;
    }
    if (j == 1u) {
        return u1;
    }
    return u2;
}
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let t = max(strand.time, 0.0) * clamp(u3.x, 0.0, 100.0);
    let size = max(strand.size, vec2<f32>(1.0));
    let nx = floor(v.pos.x) / size.x;
    let ny = floor(v.pos.y) / size.y;
    var dst = vec4<f32>(0.0);
    for (var j = 0u; j < 3u; j = j + 1u) {
        let jf = f32(j);
        let centre = 0.3 + 0.2 * jf
            + 0.12 * sin(TAU * (nx * (1.0 + 0.5 * jf) + t * 0.05 * (jf + 1.0)))
            + 0.05 * sin(TAU * (nx * 3.1 - t * 0.08));
        let light = 0.55 + 0.45 * sin(TAU * (nx * 2.0 + t * 0.1 * (jf + 1.0)) + jf);
        let d = (ny - centre) / 0.13;
        var fall = exp(-d);
        if (d < 0.0) {
            fall = exp(-d * d * 3.0);
        }
        let a = 0.5 * light * fall;
        if (a > 0.004) {
            let c = curtain(j);
            let aa = clamp(c.a * a, 0.0, 1.0);
            dst = vec4<f32>(c.rgb * aa, aa) + dst * (1.0 - aa);
        }
    }
    return dst;
}
";

/// The instanced sprite pipeline's module (after the prelude): each
/// instance is a particle's centre and alpha; `u0` is the dot's radius,
/// its glow and the sprite's half size (device pixels), `u1` its colour
/// (straight). The coverage is the CPU's `Sprite::dot`, sampled at the
/// same integer offsets from the rounded centre.
pub(crate) const PARTICLES: &str = "
@group(1) @binding(0) var<uniform> u0: vec4<f32>;
@group(1) @binding(1) var<uniform> u1: vec4<f32>;
struct Dot {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) centre: vec2<f32>,
    @location(1) @interpolate(flat) alpha: f32,
}
@vertex
fn strand_particle_vertex(@builtin(vertex_index) i: u32, @location(0) inst: vec3<f32>) -> Dot {
    let corner = vec2<f32>(f32(i & 1u), f32((i >> 1u) & 1u)) * 2.0 - 1.0;
    let centre = round(inst.xy) + vec2<f32>(0.5);
    let p = centre + corner * (u0.z + 0.5);
    var out: Dot;
    out.pos = vec4<f32>(p.x / strand.size.x * 2.0 - 1.0, 1.0 - p.y / strand.size.y * 2.0, 0.0, 1.0);
    out.centre = centre;
    out.alpha = inst.z;
    return out;
}
@fragment
fn strand_particle_fragment(v: Dot) -> @location(0) vec4<f32> {
    let off = v.pos.xy - v.centre;
    if (abs(off.x) > u0.z + 0.01 || abs(off.y) > u0.z + 0.01) {
        discard;
    }
    let d = length(off);
    let radius = u0.x;
    let glow = u0.y;
    let disc = clamp(radius + 0.5 - d, 0.0, 1.0);
    var halo = 0.0;
    if (glow > 0.0 && d > radius) {
        let sigma = max(glow / 2.5, 0.01);
        halo = 0.6 * exp(-(d - radius) * (d - radius) / (2.0 * sigma * sigma));
        if (halo < 1.0 / 255.0) {
            halo = 0.0;
        }
    }
    let a = clamp(u1.a * max(disc, halo) * v.alpha, 0.0, 1.0);
    return vec4<f32>(u1.rgb * a, a);
}
";

/// Floats a particle instance takes (x, y, alpha).
pub(crate) const PARTICLE_FLOATS: usize = 3;

/// Floats before the instances in a `particles` pass's uniforms: `u0`
/// and `u1`.
pub(crate) const PARTICLE_HEADER: usize = 8;

fn module(b: Bundled) -> Option<&'static str> {
    Some(match b {
        Bundled::Bloom => BLOOM,
        Bundled::Chromatic => CHROMATIC,
        Bundled::Crt => CRT,
        Bundled::Wobble => WOBBLE,
        Bundled::Tilt => TILT,
        Bundled::Glass => GLASS,
        Bundled::BackdropBlur => BACKDROP_BLUR,
        Bundled::Aurora => AURORA,
        Bundled::Particles => return None,
    })
}

/// The checked-code form of bundled effect `b` (`None` for `particles`).
pub(crate) fn code(b: Bundled) -> Option<Arc<ShaderCode>> {
    static CODES: OnceLock<Vec<(Bundled, Arc<ShaderCode>)>> = OnceLock::new();
    let codes = CODES.get_or_init(|| {
        Bundled::ALL
            .iter()
            .filter_map(|b| {
                let body = module(*b)?;
                let mut wgsl = String::with_capacity(COMMON.len() + body.len());
                wgsl.push_str(COMMON);
                wgsl.push_str(body);
                Some((
                    *b,
                    Arc::new(ShaderCode {
                        path: format!("bundled:{}", b.name()),
                        wgsl,
                        uniforms: ShaderCode::packed(
                            (0..4)
                                .map(|i| (format!("u{i}"), UniformType::Vec4, i))
                                .collect(),
                        ),
                    }),
                ))
            })
            .collect()
    });
    codes.iter().find(|(k, _)| *k == b).map(|(_, c)| c.clone())
}

/// The steps bundled pass `b` runs, each a code and its 16 uniforms, the
/// first reading the pass's input and each later one the step before.
pub(crate) fn steps(b: Bundled, uniforms: &[f32]) -> Vec<(Arc<ShaderCode>, Vec<f32>)> {
    let Some(code) = code(b) else {
        return Vec::new();
    };
    let mut u: Vec<f32> = uniforms.iter().copied().take(16).collect();
    u.resize(16, 0.0);
    match b {
        Bundled::BackdropBlur => {
            let mut across = u.clone();
            across[1..3].copy_from_slice(&[1.0, 0.0]);
            let mut down = u;
            down[1..3].copy_from_slice(&[0.0, 1.0]);
            vec![(code.clone(), across), (code, down)]
        }
        _ => vec![(code, u)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_pass_but_particles_has_code_and_steps() {
        for b in Bundled::ALL {
            match b {
                Bundled::Particles => assert!(code(*b).is_none()),
                _ => {
                    let c = code(*b).expect("code");
                    assert_eq!(c.uniform_floats(), 16);
                    assert!(c.wgsl.contains("fn main"));
                }
            }
        }
        let s = steps(Bundled::BackdropBlur, &[8.0]);
        assert_eq!(s.len(), 2);
        assert_eq!(&s[0].1[..3], &[8.0, 1.0, 0.0]);
        assert_eq!(&s[1].1[..3], &[8.0, 0.0, 1.0]);
        assert_eq!(steps(Bundled::Bloom, &[4.0, 1.0]).len(), 1);
    }
}
