#!/usr/bin/env bash
# Tests size-report.sh's attribution rule (`--attribute`): each
# demangled symbol name is charged to the crate it names; and its
# baseline crate table (`--crate-delta`).
# Run: scripts/test-size-report.sh (host or container; needs only awk).

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
fails=0

check() {
  local want=$1 name=$2 got
  got=$(printf '%s\n' "$name" | "$HERE/size-report.sh" --attribute | cut -f1)
  if [ "$got" = "$want" ]; then
    echo "ok   $want  $name"
  else
    echo "FAIL $name: want $want, got $got"
    fails=$((fails + 1))
  fi
}

check strand_render 'strand_render::scene::paint::h0123456789abcdef'
check strand_render 'strand_render[1a2b3c4d5e6f7a8b]::scene::paint'
check strand_theme '<strand_theme::image::Quantiser as core::ops::drop::Drop>::drop'
check zbus '<zbus::connection::Connection>::call_method::{{closure}}'
check strand_core '<alloc::vec::Vec<strand_core::Value> as core::clone::Clone>::clone'
check serde_json '<alloc::string::String as serde_json::value::Index>::index_into'
check strand_scene 'core::ptr::drop_in_place::<strand_scene::Node>'
check vello_cpu 'core::slice::sort::stable::driftsort_main::<vello_cpu::strip::Strip, _>'
check strand_scene '<alloc::collections::btree::map::BTreeMap<alloc::string::String, strand_scene::protocol::PropValue>>::insert'
check strand_scene 'core::slice::sort::stable::quicksort::quicksort::<(f32, alloc::vec::Vec<(strand_scene::protocol::Prop, u8)>), _>'
check alloc '<alloc::collections::btree::map::BTreeMap<alloc::string::String, std::path::PathBuf>>::insert'
check vello_common '<[vello_common::flatten_simd::Point32; 20: usize] as core::fmt::Debug>::fmt'
check core 'core::fmt::write'
check std 'std::thread::local::LocalKey<T>::with'
check strand_text '<&strand_text::Shaped as core::fmt::Debug>::fmt'
check strand_layout '<&mut strand_layout::Tree as core::iter::Iterator>::next'
check wgpu_core '<[wgpu_core::Id] as core::fmt::Debug>::fmt'
check '[c]' 'memcpy'
check '[c]' '__rust_probestack'
check '[c]' 'FT_Load_Glyph'

# Several names at once: one line out per line in, order kept.
out=$(printf 'a::b\nmemcpy\nb::c\n' | "$HERE/size-report.sh" --attribute | cut -f1 | tr '\n' ' ')
if [ "$out" = "a [c] b " ]; then
  echo "ok   one line per symbol"
else
  echo "FAIL one line per symbol: got '$out'"
  fails=$((fails + 1))
fi

# The baseline table (`--crate-delta`): the current rows with their Δ,
# then a baseline top crate the binary no longer has, as a 0-byte row
# with its whole negative Δ; one past the baseline's top N is not listed.
DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
printf '500\tstrand_render\t5\n300\timage\t3\n100\tzbus\t1\n50\tgone_far\t1\n' >"$DIR/base.tsv"
printf '520\tstrand_render\t5\n90\tzbus\t1\n10\tnew_crate\t1\n' >"$DIR/new.tsv"
got=$("$HERE/size-report.sh" --crate-delta "$DIR/base.tsv" "$DIR/new.tsv" 1000 3)
want='| 520 | +20 | 52.00 | 5 | strand_render |
| 90 | -10 | 9.00 | 1 | zbus |
| 10 | +10 | 1.00 | 1 | new_crate |
| 0 | -300 | 0.00 | 0 | image (gone) |'
if [ "$got" = "$want" ]; then
  echo "ok   a crate gone from the binary keeps its row and Δ"
else
  echo "FAIL a crate gone from the binary keeps its row and Δ: got"
  echo "$got"
  fails=$((fails + 1))
fi

[ "$fails" = 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }
