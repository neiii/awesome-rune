#!/bin/sh
# Renders assets/stars/*.png and assets/bars/bars.{png,json} with Rune's own effects.
#
# Run this only when the look changes; the README workflow assembles charts from the committed
# sprites on a plain CPU. Needs Rust and a GPU (Metal on macOS, Vulkan or DX12 elsewhere).
#
#   tools/badges/render.sh
set -eu

# The Rune revision the badges are drawn with. rune-fx-star.patch adds a five-point star to its
# shape SDF (a negative radius); bump both together.
RUNE_REV=f3d04f3f2069d177d04c963b0ba9f6b2065e46eb

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
rune="$here/vendor/rune"

if [ ! -d "$rune/.git" ]; then
  git clone --filter=blob:none https://github.com/anomalyco/rune.git "$rune"
fi
git -C "$rune" fetch --quiet origin "$RUNE_REV"
git -C "$rune" checkout --quiet --force "$RUNE_REV"
git -C "$rune" apply "$here/rune-fx-star.patch"

example="$rune/crates/rune-fx/examples/awesome-badges"
mkdir -p "$example"
cp "$here/main.rs" "$here/ui.rs" "$here/ui.wgsl" "$here/resolve.wgsl" "$example/"

(cd "$rune" && cargo run --release --quiet -p rune-fx --example awesome-badges -- --out "$root/assets")
