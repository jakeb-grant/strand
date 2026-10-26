"""exact-fma.py SRC DEST: fearless_simd 0.7.0 (vello_cpu's SIMD layer)
copied from SRC, the registry's unpacked crate, to DEST with one change
to its wasm backend: a multiply-add (`mul_add` and `mul_sub` on f32x4
and f64x2, which its wider vectors are made of) is fused, rounded once,
as fearless_simd's AVX2, AVX-512 and NEON backends fuse it natively,
instead of a multiply and an add each rounded (docs/live-examples.md,
"Pixel parity"). WebAssembly has no fused multiply-add but the relaxed
one, which a browser may fuse or not, and differently as its compiler
tiers up; so an f32x4 multiply-add is done in f64 lanes, exactly but for
the rare tie it checks for, where it takes compiler-builtins' correctly
rounded software `fmaf`, and an f64x2 one takes `fma` per lane: the same
bits as the processor's instruction. scripts/wasm/build.sh builds the
module with DEST patched in. Fails unless SRC is the file this was
written against."""

import hashlib
import shutil
import sys
from pathlib import Path

WASM_RS = "src/generated/wasm.rs"
SHA256 = "5249153b7d62186fde576a253544d2f0acd1ccf6e9409eb23850b24a08b83224"


F64_LANES = (
    "{{\n"
    "                // strand: fused, rounded once, as natively (scripts/wasm/exact-fma.py)\n"
    "                let (a, b, c): ([f64; 2], [f64; 2], [f64; 2]) = (a.into(), b.into(), c.into());\n"
    "                [a[0].mul_add(b[0], {c0}), a[1].mul_add(b[1], {c1})].simd_into(self)\n"
    "            }}"
)

# A fused f32x4 multiply-add in f64: the product of two f32 is exact in
# f64, so `a * b + c` there rounds once, to 53 bits, and rounding that
# to f32 is the correctly rounded result unless it lies exactly halfway
# between two f32 (its low 29 bits 0x10000000), where the first rounding
# may have made the tie: then each lane takes the software `fmaf`, as
# compiler-builtins' own `fmaf` does in that case.
HELPER = """
/// (strand, scripts/wasm/exact-fma.py) `a * b + c` on four f32 lanes,
/// rounded once, as AVX2's and NEON's fused multiply-add round it.
#[inline(always)]
fn strand_fma_f32x4(a: v128, b: v128, c: v128) -> v128 {
    let high = |v: v128| f64x2_promote_low_f32x4(i32x4_shuffle::<2, 3, 2, 3>(v, v));
    let lo = f64x2_add(
        f64x2_mul(f64x2_promote_low_f32x4(a), f64x2_promote_low_f32x4(b)),
        f64x2_promote_low_f32x4(c),
    );
    let hi = f64x2_add(f64x2_mul(high(a), high(b)), high(c));
    let low_bits = i64x2_splat(0x1fff_ffff);
    let tie = i64x2_splat(0x1000_0000);
    let ties = v128_or(
        i64x2_eq(v128_and(lo, low_bits), tie),
        i64x2_eq(v128_and(hi, low_bits), tie),
    );
    if v128_any_true(ties) {
        let lane = |v: v128, i: usize| -> f32 {
            match i {
                0 => f32x4_extract_lane::<0>(v),
                1 => f32x4_extract_lane::<1>(v),
                2 => f32x4_extract_lane::<2>(v),
                _ => f32x4_extract_lane::<3>(v),
            }
        };
        let r = |i: usize| lane(a, i).mul_add(lane(b, i), lane(c, i));
        f32x4(r(0), r(1), r(2), r(3))
    } else {
        i32x4_shuffle::<0, 1, 4, 5>(f32x4_demote_f64x2_zero(lo), f32x4_demote_f64x2_zero(hi))
    }
}
"""

CHANGES = [
    (
        "self.add_f32x4(self.mul_f32x4(a, b), c)",
        "strand_fma_f32x4(a.into(), b.into(), c.into()).simd_into(self)",
    ),
    (
        "self.sub_f32x4(self.mul_f32x4(a, b), c)",
        "strand_fma_f32x4(a.into(), b.into(), f32x4_neg(c.into())).simd_into(self)",
    ),
    ("self.add_f64x2(self.mul_f64x2(a, b), c)", F64_LANES.format(c0="c[0]", c1="c[1]")),
    ("self.sub_f64x2(self.mul_f64x2(a, b), c)", F64_LANES.format(c0="-c[0]", c1="-c[1]")),
]


def main() -> int:
    src, dest = Path(sys.argv[1]), Path(sys.argv[2])
    text = (src / WASM_RS).read_text()
    got = hashlib.sha256(text.encode()).hexdigest()
    if got != SHA256:
        print(f"exact-fma.py: {src / WASM_RS} is not fearless_simd 0.7.0's ({got})", file=sys.stderr)
        return 1
    for old, new in CHANGES:
        if text.count(old) != 1:
            print(f"exact-fma.py: `{old}` is not in {WASM_RS} once", file=sys.stderr)
            return 1
        text = text.replace(old, new)
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(src, dest)
    (dest / WASM_RS).write_text(text + HELPER)
    return 0


if __name__ == "__main__":
    sys.exit(main())
