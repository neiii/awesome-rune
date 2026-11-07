//! Renders awesome-rune's plastic star badges and the bar chart sprite sheet, headless at 4x
//! with transparent backgrounds. Run through `render.sh`, which builds this as a rune-fx example
//! against a pinned Rune revision with the star shape patched in.
//!
//!   awesome-badges --out DIR
//!
//! Writes `stars/star-<tier>.png` (a 20 pt star over 12 pt of clear space, so `align="middle"`
//! centres it on a line of text) and `bars/bars.png` + `bars/bars.json` (every bar the chart can
//! draw, keyed by height in half points). Only the look lives here; CI assembles the charts.
#![allow(dead_code)]

mod ui;

use rune::render::{Gpu, Offscreen, write_png};
use rune_fx::*;
use std::ops::Range;
use std::path::PathBuf;
use ui::{TextAtlas, UiInst, UiRenderer};



const W: f32 = 640.0;
const H: f32 = 360.0;
const S: f32 = 4.0;
const FMT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

// ------------------------------------------------------------------ scene builder

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Main,
    Mask,
    Src,
}

enum Op {
    Ui(Range<usize>),
    Fx(usize),
    Backdrop,
}

struct B<'a> {
    atlas: &'a mut TextAtlas,
    ui: Vec<UiInst>,
    mask: Vec<UiInst>,
    src: Vec<UiInst>,
    src_rect: Option<Rect>,
    layers: Vec<FxLayer>,
    slots: Vec<FxSlot>,
    ops: Vec<Op>,
    target: Target,
    t: f32,
    reduced: bool,
}

const SRC_PAD: f32 = 4.0;

impl<'a> B<'a> {
    fn new(atlas: &'a mut TextAtlas, t: f32, reduced: bool) -> B<'a> {
        B { atlas, ui: vec![], mask: vec![], src: vec![], src_rect: None, layers: vec![], slots: vec![], ops: vec![], target: Target::Main, t, reduced }
    }
    fn push(&mut self, mut u: UiInst) {
        match self.target {
            Target::Main => {
                u.vp = [W * S, H * S, 0.0, 0.0];
                let i = self.ui.len();
                self.ui.push(u);
                if let Some(Op::Ui(r)) = self.ops.last_mut() {
                    if r.end == i {
                        r.end = i + 1;
                        return;
                    }
                }
                self.ops.push(Op::Ui(i..i + 1));
            }
            Target::Mask => {
                u.vp = [W * S, H * S, 0.0, 0.0];
                self.mask.push(u);
            }
            Target::Src => {
                let r = self.src_rect.unwrap();
                u.rect[0] -= (r.x - SRC_PAD) * S;
                u.rect[1] -= (r.y - SRC_PAD) * S;
                u.vp = [(r.w + 2.0 * SRC_PAD) * S, (r.h + 2.0 * SRC_PAD) * S, 0.0, 0.0];
                self.src.push(u);
            }
        }
    }
    fn rect(&mut self, r: Rect, radius: f32, fill: [f32; 4], fill2: [f32; 4], border: [f32; 4], bw: f32) {
        self.push(UiInst {
            rect: [r.x * S, r.y * S, r.w * S, r.h * S],
            p: [radius * S, bw * S, 0.0, 0.0],
            fill,
            fill2,
            border,
            uv: [0.0; 4],
            vp: [0.0; 4],
        });
    }
    fn shadow(&mut self, r: Rect, radius: f32, sigma: f32, c: [f32; 4]) {
        self.push(UiInst {
            rect: [r.x * S, r.y * S, r.w * S, r.h * S],
            p: [radius * S, 0.0, 2.0, sigma * S],
            fill: c,
            fill2: c,
            border: [0.0; 4],
            uv: [0.0; 4],
            vp: [0.0; 4],
        });
    }
    /// Text with its baseline at y. Returns width in pt.
    fn text(&mut self, x: f32, y: f32, s: &str, size: f32, weight: u8, c: [f32; 4]) -> f32 {
        let r = self.atlas.get(s, size * S, weight);
        self.push(UiInst {
            rect: [x * S - 2.0, y * S - r.ascent, r.w, r.h],
            p: [0.0, 0.0, 1.0, 0.0],
            fill: c,
            fill2: c,
            border: [0.0; 4],
            uv: r.uv,
            vp: [0.0; 4],
        });
        r.w / S
    }
    fn text_w(&mut self, s: &str, size: f32, weight: u8) -> f32 {
        self.atlas.get(s, size * S, weight).w / S - 4.0 / S
    }
    fn text_c(&mut self, cx: f32, cy: f32, s: &str, size: f32, weight: u8, c: [f32; 4]) {
        let w = self.text_w(s, size, weight);
        self.text(cx - w * 0.5, cy + size * 0.36, s, size, weight, c);
    }
    fn fx(&mut self, l: FxLayer) {
        self.fx_with(l, FxSlot::default());
    }
    fn fx_with(&mut self, l: FxLayer, slot: FxSlot) {
        let l = l.at(if l.time == 0.0 { self.t } else { l.time }).reduced(self.reduced);
        self.ops.push(Op::Fx(self.layers.len()));
        self.layers.push(l);
        self.slots.push(slot);
    }
    fn backdrop(&mut self) {
        self.ops.push(Op::Backdrop);
    }
}


// ------------------------------------------------------------------ GPU

struct Ctx {
    device: wgpu::Device,
    queue: wgpu::Queue,
    ui: UiRenderer,
    fx: FxRenderer,
    atlas: TextAtlas,
    scene: wgpu::Texture,
    scene_view: wgpu::TextureView,
    backdrop: wgpu::Texture,
    backdrop_view: wgpu::TextureView,
    out: Offscreen,
    pyramid: MaskPyramid,
    src: Option<(wgpu::Texture, wgpu::TextureView, [u32; 2])>,
    resolve: wgpu::RenderPipeline,
    resolve_bg: wgpu::BindGroup,
    timestamps: bool,
    w: u32,
    h: u32,
}

fn tex(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> (wgpu::Texture, wgpu::TextureView) {
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let v = t.create_view(&Default::default());
    (t, v)
}

fn pass<'e>(enc: &'e mut wgpu::CommandEncoder, view: &'e wgpu::TextureView, clear: Option<wgpu::Color>, ts: Option<wgpu::RenderPassTimestampWrites<'e>>) -> wgpu::RenderPass<'e> {
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: clear.map_or(wgpu::LoadOp::Load, wgpu::LoadOp::Clear), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: ts,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

impl Ctx {
    fn new(w: u32, h: u32) -> Ctx {
        let Gpu { adapter, device, queue, .. } = Gpu::headless();
        let timestamps = device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        eprintln!("adapter: {:?} timestamps={timestamps}", adapter.get_info().name);
        let ui = UiRenderer::new(&device, FMT);
        let fx = FxRenderer::new(&device, FMT, FxOptions { dither: false });
        let u = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
        let (scene, scene_view) = tex(&device, w, h, FMT, u);
        let (backdrop, backdrop_view) = tex(&device, w, h, FMT, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let out = Offscreen::with_format(&device, w, h, wgpu::TextureFormat::Rgba8Unorm);
        let pyramid = MaskPyramid::new(&device, w, h);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("resolve"),
            source: wgpu::ShaderSource::Wgsl(include_str!("resolve.wgsl").into()),
        });
        let resolve = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("resolve"),
            layout: None,
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_full"), compilation_options: Default::default(), buffers: &[] },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_resolve"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let resolve_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &resolve.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&scene_view) }],
        });
        Ctx {
            device,
            queue,
            ui,
            fx,
            atlas: TextAtlas::new(),
            scene,
            scene_view,
            backdrop,
            backdrop_view,
            out,
            pyramid,
            src: None,
            resolve,
            resolve_bg,
            timestamps,
            w,
            h,
        }
    }

    fn render(&mut self, name: &str, t: f32, reduced: bool) -> Vec<u8> {
        let mut b = B::new(&mut self.atlas, t, reduced);
        scene(name, &mut b);
        let B { ui, mask, src, src_rect, layers, slots, ops, .. } = b;
        let mut all = ui;
        let mask_r = all.len()..all.len() + mask.len();
        all.extend_from_slice(&mask);
        let src_r = all.len()..all.len() + src.len();
        all.extend_from_slice(&src);
        self.ui.prepare(&self.device, &self.queue, &mut self.atlas, &all);
        if let Some(r) = src_rect {
            let sz = [((r.w + 2.0 * SRC_PAD) * S) as u32, ((r.h + 2.0 * SRC_PAD) * S) as u32];
            if self.src.as_ref().is_none_or(|s| s.2 != sz) {
                let (t, v) = tex(&self.device, sz[0], sz[1], FMT, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING);
                self.src = Some((t, v, sz));
            }
        }
        let src_view = self.src.as_ref().map(|s| &s.1);
        let mut sources: Vec<&wgpu::TextureView> = vec![&self.pyramid.view];
        if let Some(v) = src_view {
            sources.push(v);
        }
        let frame = FxFrame { viewport: [self.w, self.h], scale: S, layers: &layers, backdrop: Some(&self.backdrop_view), sources: &sources };
        self.fx.prepare_with_slots(&self.device, &self.queue, &frame, &slots);

        let mut enc = self.device.create_command_encoder(&Default::default());
        if !mask.is_empty() {
            {
                let mut p = pass(&mut enc, &self.pyramid.levels[0], Some(wgpu::Color::TRANSPARENT), None);
                self.ui.draw(&mut p, mask_r.clone());
            }
            self.fx.build_mips(&self.device, &mut enc, &self.pyramid);
        }
        if let Some(s) = &self.src {
            if !src.is_empty() {
                let mut p = pass(&mut enc, &s.1, Some(wgpu::Color::TRANSPARENT), None);
                self.ui.draw(&mut p, src_r.clone());
            }
        }
        {
            let _ = pass(&mut enc, &self.scene_view, Some(wgpu::Color::TRANSPARENT), None);
        }
        for op in &ops {
            match op {
                Op::Ui(r) => {
                    let mut p = pass(&mut enc, &self.scene_view, None, None);
                    self.ui.draw(&mut p, r.clone());
                }
                Op::Fx(i) => {
                    self.fx.blur_backdrop(&self.device, &self.queue, &mut enc, &self.backdrop_view, *i);
                    let mut p = pass(&mut enc, &self.scene_view, None, None);
                    self.fx.draw_layer(&mut p, *i);
                }
                Op::Backdrop => {
                    enc.copy_texture_to_texture(self.scene.as_image_copy(), self.backdrop.as_image_copy(), wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 });
                }
            }
        }
        {
            let mut p = pass(&mut enc, &self.out.view, Some(wgpu::Color::BLACK), None);
            p.set_pipeline(&self.resolve);
            p.set_bind_group(0, &self.resolve_bg, &[]);
            p.draw(0..3, 0..1);
        }
        self.out.copy(&mut enc);
        self.queue.submit([enc.finish()]);
        self.out.read(&self.device)
    }
}

// ------------------------------------------------------------------ badges

/// Star tiers, lowest first, each moulded in its own colour: gunmetal under 10 stars, rose gold
/// from 10, silver from 100, gold from 1k.
const TIERS: [(&str, u32); 4] = [("gunmetal", 0x5d6979), ("rose", 0xc0765d), ("silver", 0x98a3b1), ("gold", 0xd29a1c)];
/// The star's box, and the clear space under it that centres it on text.
const STAR: f32 = 20.0;
const STAR_PAD: f32 = 12.0;

/// Bars are 4 pt wide and 6..32 pt tall in half-point steps; a quiet day is a 4 pt stub.
const BAR_W: f32 = 4.0;
const BAR_MIN: f32 = 6.0;
const BAR_MAX: f32 = 32.0;
const STUB: f32 = 4.0;
const RUNE_BLUE: u32 = 0x54b3fd;

fn plastic(color: u32, bevel: f32) -> PlasticParams {
    PlasticParams { color: rgb(color), roughness: 0.12, dome: 0.3, bevel, ..PlasticParams::glossy() }
}

/// A five-point star filling a 20 pt box; the patched rune-fx reads a negative radius as a star
/// whose points are rounded by its magnitude.
fn star(x: f32, y: f32) -> Shape {
    Shape { rect: Rect::new(x, y, STAR, STAR), radius: -0.5 }
}

enum Job {
    Star { x: f32, y: f32, tier: usize },
    Bar { x: f32, bottom: f32, h: f32 },
}

thread_local! {
    static JOBS: std::cell::RefCell<Vec<Job>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn scene(_name: &str, b: &mut B) {
    JOBS.with(|jobs| {
        for job in jobs.borrow().iter() {
            match *job {
                Job::Star { x, y, tier } => b.fx(Fx::fill(PlasticParams { dome: 0.35, bevel: 3.0, ..plastic(TIERS[tier].1, 3.0) }).on(star(x, y))),
                Job::Bar { x, bottom, h } => b.fx(Fx::fill(plastic(RUNE_BLUE, 3.2)).on(Shape::rounded(x, bottom - h, BAR_W, h, BAR_W * 0.5))),
            }
        }
    });
}

/// RGBA pixels of the whole-point rect (x, y, w, h).
fn crop(px: &[u8], x: f32, y: f32, w: f32, h: f32) -> (u32, u32, Vec<u8>) {
    let full = (W * S) as u32;
    let (x0, y0, cw, ch) = ((x * S) as u32, (y * S) as u32, (w * S) as u32, (h * S) as u32);
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in y0..y0 + ch {
        let s = ((row * full + x0) * 4) as usize;
        out.extend_from_slice(&px[s..s + (cw * 4) as usize]);
    }
    (cw, ch, out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1)).map_or_else(|| PathBuf::from("assets"), PathBuf::from);
    std::fs::create_dir_all(out.join("stars")).unwrap();
    std::fs::create_dir_all(out.join("bars")).unwrap();
    let mut ctx = Ctx::new((W * S) as u32, (H * S) as u32);

    // Stars along the top row, bar sprites in rows below, each in its own whole-point cell.
    let heights: Vec<f32> = std::iter::once(STUB).chain((0..).map(|i| BAR_MIN + 0.5 * i as f32).take_while(|&h| h <= BAR_MAX)).collect();
    let per_row = 64;
    let mut jobs: Vec<Job> = (0..TIERS.len()).map(|tier| Job::Star { x: 8.0 + tier as f32 * 32.0, y: 8.0, tier }).collect();
    let cell = |i: usize| (8.0 + (i % per_row) as f32 * 8.0, 48.0 + (i / per_row) as f32 * 40.0 + BAR_MAX);
    for (i, &h) in heights.iter().enumerate() {
        let (x, bottom) = cell(i);
        jobs.push(Job::Bar { x, bottom, h });
    }
    JOBS.with(|j| *j.borrow_mut() = jobs);
    let px = ctx.render("badges", 0.0, false);

    for (tier, (name, _)) in TIERS.iter().enumerate() {
        let (w, h, c) = crop(&px, 8.0 + tier as f32 * 32.0, 8.0, STAR, STAR + STAR_PAD);
        write_png(&out.join(format!("stars/star-{name}.png")), w, h, &c).unwrap();
    }

    // Pack the bars side by side, bottoms aligned, and index them by height in half points.
    let (bw, bh) = ((BAR_W * S) as u32, (BAR_MAX * S) as u32);
    let mut sheet = vec![0u8; (bw * heights.len() as u32 * bh * 4) as usize];
    let sheet_w = bw * heights.len() as u32;
    let mut index = vec![];
    for (i, &h) in heights.iter().enumerate() {
        let (x, bottom) = cell(i);
        let (w, ph, c) = crop(&px, x, bottom - h, BAR_W, h);
        let (sx, sy) = (i as u32 * bw, bh - ph);
        for row in 0..ph {
            let d = (((sy + row) * sheet_w + sx) * 4) as usize;
            sheet[d..d + (w * 4) as usize].copy_from_slice(&c[(row * w * 4) as usize..((row + 1) * w * 4) as usize]);
        }
        let key = if h == STUB { "stub".to_string() } else { ((h * 2.0).round() as u32).to_string() };
        index.push(format!("    \"{key}\": [{sx}, {sy}, {w}, {ph}]"));
    }
    write_png(&out.join("bars/bars.png"), sheet_w, bh, &sheet).unwrap();
    std::fs::write(
        out.join("bars/bars.json"),
        format!("{{\n  \"scale\": {S},\n  \"bar_width_pt\": {BAR_W},\n  \"sprites\": {{\n{}\n  }}\n}}\n", index.join(",\n")),
    )
    .unwrap();
    eprintln!("wrote {} stars and {} bars to {}", TIERS.len(), heights.len(), out.display());
}
