# Badge renderer

Renders the plastic star badges (`assets/stars/`) and the bar chart sprite sheet (`assets/bars/`)
with Rune's own effects, headless. Run it only when the look changes; the README workflow
assembles each project's chart from the committed sprites on a plain CPU.

```sh
tools/badges/render.sh
```

It clones Rune at the revision pinned in `render.sh`, applies `rune-fx-star.patch` (a five-point
star in rune-fx's shape SDF, selected by a negative radius) and builds `main.rs` as a rune-fx
example. Needs Rust and a GPU: Metal on macOS, Vulkan or DX12 elsewhere.

`main.rs`, `ui.rs`, `ui.wgsl` and `resolve.wgsl` are adapted from rune-fx's gallery example and,
with the patch, are MIT-licensed by Anomaly; see [`LICENSE`](LICENSE).
