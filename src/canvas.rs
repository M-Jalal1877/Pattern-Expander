//! Canvas: drawing, zoom/pan, rulers, hit testing, selection, handles and tool input.

use crate::document::{Document, History, ObjId, Object, ObjectKind, ShapeObj, pattern_pieces};
use crate::pattern::PatternFrame;
use crate::shapes::{
    Bounds, Color, ShapeGeom, Style, Transform, bounds_overlap, bounds_union, custom_from_world,
    dist_to_segment, point_in_evenodd, ring_points, rotate_vec,
};
use egui::epaint::{TextShape, Vertex, WHITE_UV};
use egui::{
    Align2, Color32, CursorIcon, Event, FontId, Key, Mesh, MouseWheelUnit, Painter, PointerButton,
    Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use geo::{LineString, MultiPolygon, Polygon, TriangulateEarcut};
use std::collections::HashMap;

/// egui points per millimetre at 100 % zoom. egui points are 1/96 inch
/// (they already include the OS display scaling), so pixels = mm × zoom × DPI / 25.4.
pub const PT_PER_MM: f64 = 96.0 / 25.4;
const RULER: f32 = 20.0;
const HANDLE: f32 = 4.0;
const HIT_PX: f64 = 5.0;
const PATTERN_HANDLE_LEN: f64 = 70.0;
const ACCENT: Color32 = Color32::from_rgb(0x1e, 0x88, 0xe5);
const PATTERN_ACCENT: Color32 = Color32::from_rgb(0xd8, 0x1b, 0x60);
/// Above this exact-preview time (ms) a simplified preview is shown while dragging.
const SLOW_PREVIEW_MS: f64 = 14.0;
/// Previews expected to take longer than this (ms) are computed off the UI thread.
const ASYNC_MS: f64 = 30.0;
/// …as are previews with more estimated tiles than this.
const ASYNC_TILES: f64 = 20_000.0;
/// Tiles smaller than this on screen (px) are not drawn one by one: the shape is
/// tinted with the pattern's average colour instead, and nothing is computed.
pub const LOD_TILE_PX: f64 = 1.5;

/// Milliseconds since start (monotonic). Returns 0 on WebAssembly.
pub fn now_ms() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
    }
    #[cfg(target_arch = "wasm32")]
    {
        0.0
    }
}

pub fn to_color32(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Rect,
    Ellipse,
    Polygon,
    Star,
    Pen,
    /// Pan the view by dragging.
    Hand,
}

impl Tool {
    pub const ALL: [Tool; 7] =
        [Tool::Select, Tool::Hand, Tool::Rect, Tool::Ellipse, Tool::Polygon, Tool::Star, Tool::Pen];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Rect => "Rectangle",
            Tool::Ellipse => "Ellipse",
            Tool::Polygon => "Polygon",
            Tool::Star => "Star",
            Tool::Pen => "Pen",
            Tool::Hand => "Hand (pan)",
        }
    }

    pub fn key(self) -> Key {
        match self {
            Tool::Select => Key::V,
            Tool::Rect => Key::R,
            Tool::Ellipse => Key::E,
            Tool::Polygon => Key::O,
            Tool::Star => Key::S,
            Tool::Pen => Key::N,
            Tool::Hand => Key::H,
        }
    }
}

/// World (mm) <-> screen (points) mapping: `screen = o + world * k`.
#[derive(Clone, Copy)]
struct Xf {
    o: [f64; 2],
    k: f64,
}

impl Xf {
    fn s(&self, p: [f64; 2]) -> Pos2 {
        pos2((self.o[0] + p[0] * self.k) as f32, (self.o[1] + p[1] * self.k) as f32)
    }
    fn w(&self, p: Pos2) -> [f64; 2] {
        [(f64::from(p.x) - self.o[0]) / self.k, (f64::from(p.y) - self.o[1]) / self.k]
    }
}

// ---------------------------------------------------------------------------
// Render cache
// ---------------------------------------------------------------------------

/// Triangulated geometry in world mm.
#[derive(Default)]
struct TriMesh {
    pos: Vec<[f64; 2]>,
    col: Vec<Color32>,
    idx: Vec<u32>,
}

fn is_convex(pts: &[[f64; 2]]) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0.0_f64;
    for i in 0..n {
        let (a, b, c) = (pts[i], pts[(i + 1) % n], pts[(i + 2) % n]);
        let cross = (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]);
        if cross.abs() > 1e-12 {
            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }
    }
    true
}

impl TriMesh {
    fn add_polygon(&mut self, poly: &Polygon<f64>, color: Color32) {
        let base = self.pos.len() as u32;
        let ext = ring_points(poly.exterior());
        if poly.interiors().is_empty() && is_convex(&ext) {
            for p in &ext {
                self.pos.push(*p);
                self.col.push(color);
            }
            for i in 1..ext.len() as u32 - 1 {
                self.idx.extend([base, base + i, base + i + 1]);
            }
        } else {
            let raw = poly.earcut_triangles_raw();
            for v in &raw.vertices {
                self.pos.push(*v);
                self.col.push(color);
            }
            self.idx.extend(raw.triangle_indices.iter().map(|&i| base + i as u32));
        }
    }
}

struct ShapeEntry {
    rev: u64,
    tol: u64,
    geom: MultiPolygon<f64>,
    bounds: Option<Bounds>,
    fill: TriMesh,
    /// The outlined stroke (real joins / miter limit), built on first use.
    stroke: Option<TriMesh>,
    frame: u64,
    /// What the entry was built from, to recognise a pure move.
    src_geom: ShapeGeom,
    src_xf: Transform,
    src_style: Style,
}

impl ShapeEntry {
    /// Does `s` differ from what this entry was built from only by position?
    fn moved_only(&self, s: &ShapeObj) -> bool {
        let geom_same = match (&self.src_geom, &s.geom) {
            (
                ShapeGeom::Custom { polys: a, base_w: aw, base_h: ah },
                ShapeGeom::Custom { polys: b, base_w: bw, base_h: bh },
            ) => std::sync::Arc::ptr_eq(a, b) && aw == bw && ah == bh,
            (a, b) => a == b,
        };
        geom_same
            && self.src_xf.w == s.xf.w
            && self.src_xf.h == s.xf.h
            && self.src_xf.rotation_deg == s.xf.rotation_deg
            && self.src_style == s.style
    }

    /// Shift everything cached by (dx, dy) — no re-triangulation.
    fn translate(&mut self, dx: f64, dy: f64) {
        use geo::Translate;
        self.geom.translate_mut(dx, dy);
        self.bounds = self.bounds.map(|b| [b[0] + dx, b[1] + dy, b[2] + dx, b[3] + dy]);
        for m in std::iter::once(&mut self.fill).chain(self.stroke.as_mut()) {
            for p in &mut m.pos {
                p[0] += dx;
                p[1] += dy;
            }
        }
    }
}

/// Turn cosine below which a corner counts as a near-reversal (≈ 135°+ turn).
/// egui joins path segments with unlimited miters, so such corners would shoot
/// spikes across the canvas; lines are split there instead.
const SPIKE_COS: f32 = -0.7;

/// Draw a closed screen-space outline without miter spikes.
///
/// Points closer than a fraction of a pixel are merged (curve-converted text
/// has many), then the ring is split into open polylines at every sharp corner,
/// so egui only ever mitres gentle corners. Gentle rings stay one closed line.
fn add_outline(painter: &Painter, ring: &[Pos2], stroke: Stroke) {
    match outline_parts(ring) {
        OutlineParts::Closed(pts) => {
            painter.add(Shape::closed_line(pts, stroke));
        }
        OutlineParts::Open(lines) => {
            for l in lines {
                painter.add(Shape::line(l, stroke));
            }
        }
    }
}

#[derive(Debug)]
enum OutlineParts {
    /// No sharp corner: one closed line.
    Closed(Vec<Pos2>),
    /// Split at sharp corners into open lines (empty if degenerate).
    Open(Vec<Vec<Pos2>>),
}

fn outline_parts(ring: &[Pos2]) -> OutlineParts {
    let mut pts: Vec<Pos2> = Vec::with_capacity(ring.len());
    for &p in ring {
        if pts.last().is_none_or(|l: &Pos2| l.distance_sq(p) > 0.09) {
            pts.push(p);
        }
    }
    while pts.len() > 2 && pts[0].distance_sq(pts[pts.len() - 1]) <= 0.09 {
        pts.pop();
    }
    let n = pts.len();
    if n < 2 {
        return OutlineParts::Open(vec![]);
    }
    let sharp = |i: usize| {
        let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
        let (d1, d2) = ((b - a).normalized(), (c - b).normalized());
        d1.dot(d2) < SPIKE_COS
    };
    let Some(start) = (0..n).find(|&i| sharp(i)) else {
        return OutlineParts::Closed(pts);
    };
    // Walk once around the ring from the first sharp corner, cutting at each one.
    let mut lines = Vec::new();
    let mut line = vec![pts[start]];
    for k in 1..=n {
        let i = (start + k) % n;
        line.push(pts[i]);
        if k == n || sharp(i) {
            if line.len() >= 2 {
                lines.push(std::mem::take(&mut line));
            }
            line = vec![pts[i]];
        }
    }
    OutlineParts::Open(lines)
}

pub struct PatternStats {
    pub pieces: usize,
    pub ms: f64,
    pub capped: bool,
    pub exact: bool,
    /// Tiles are too small to see at this zoom: drawn as a tint, not computed.
    /// Holds the estimated tile count.
    pub too_fine: Option<f64>,
    /// A background computation is running.
    pub computing: bool,
}

/// A mesh split into spatial chunks so off-screen parts are not drawn.
#[derive(Default)]
struct ChunkedMesh {
    chunks: Vec<(Bounds, TriMesh)>,
}

impl ChunkedMesh {
    /// Up to 16 × 16 chunks, assigned by piece centre.
    fn from_pieces(pieces: &[crate::expand::Piece]) -> Self {
        let all = pieces.iter().filter_map(|p| geo::BoundingRect::bounding_rect(&p.poly)).fold(None, |b: Option<Bounds>, r| {
            bounds_union(b, Some([r.min().x, r.min().y, r.max().x, r.max().y]))
        });
        let Some(all) = all else { return Self::default() };
        const N: usize = 16;
        let (w, h) = ((all[2] - all[0]).max(1e-9), (all[3] - all[1]).max(1e-9));
        let mut chunks: Vec<(Option<Bounds>, TriMesh)> = (0..N * N).map(|_| (None, TriMesh::default())).collect();
        for p in pieces {
            let Some(r) = geo::BoundingRect::bounding_rect(&p.poly) else { continue };
            let b = [r.min().x, r.min().y, r.max().x, r.max().y];
            let i = ((((b[0] + b[2]) / 2.0 - all[0]) / w * N as f64) as usize).min(N - 1);
            let j = ((((b[1] + b[3]) / 2.0 - all[1]) / h * N as f64) as usize).min(N - 1);
            let c = &mut chunks[j * N + i];
            c.0 = bounds_union(c.0, Some(b));
            c.1.add_polygon(&p.poly, to_color32(p.color));
        }
        Self { chunks: chunks.into_iter().filter_map(|(b, m)| b.map(|b| (b, m))).collect() }
    }
}

/// Result of a (possibly background) pattern computation.
struct PatternDone {
    mesh: ChunkedMesh,
    pieces: usize,
    capped: bool,
    ms: f64,
    exact: bool,
}

fn compute_pattern(s: &ShapeObj, settings: &crate::document::DocSettings, fast: bool) -> PatternDone {
    let t0 = now_ms();
    let res = pattern_pieces(s, settings, fast);
    PatternDone {
        mesh: ChunkedMesh::from_pieces(&res.pieces),
        pieces: res.pieces.len(),
        capped: res.capped,
        ms: now_ms() - t0,
        exact: !fast,
    }
}

/// A preview being computed on a worker thread.
struct PatternJob {
    rev: u64,
    key: u64,
    rx: std::sync::mpsc::Receiver<PatternDone>,
}

struct PatternEntry {
    /// Revision / settings the mesh belongs to (u64::MAX = nothing yet).
    rev: u64,
    key: u64,
    exact: bool,
    mesh: ChunkedMesh,
    pieces: usize,
    capped: bool,
    /// Time of the last exact computation.
    exact_ms: f64,
    frame: u64,
    job: Option<PatternJob>,
}

#[derive(Default)]
struct RenderCache {
    shapes: HashMap<ObjId, ShapeEntry>,
    patterns: HashMap<ObjId, PatternEntry>,
    frame: u64,
}

fn settings_key(doc: &Document) -> u64 {
    let s = &doc.settings;
    (s.expand_mode as u64)
        ^ s.tolerance().to_bits().rotate_left(7)
        ^ s.min_piece_area.to_bits().rotate_left(29)
}

impl RenderCache {
    fn shape(&mut self, id: ObjId, s: &ShapeObj, tol: f64) -> &ShapeEntry {
        let tolb = tol.to_bits();
        let frame = self.frame;
        let fresh = self.shapes.get(&id).is_some_and(|e| e.rev == s.rev && e.tol == tolb);
        // A pure move (drag, arrow nudge, moving a big expanded group) only
        // shifts the cached triangles instead of re-triangulating.
        let moved = !fresh
            && self.shapes.get(&id).is_some_and(|e| {
                e.tol == tolb && e.moved_only(s) && (s.xf.cx - e.src_xf.cx).is_finite() && (s.xf.cy - e.src_xf.cy).is_finite()
            });
        if moved && let Some(e) = self.shapes.get_mut(&id) {
            let (dx, dy) = (s.xf.cx - e.src_xf.cx, s.xf.cy - e.src_xf.cy);
            e.translate(dx, dy);
            e.src_xf = s.xf.clone();
            e.rev = s.rev;
        } else if !fresh {
            let geom = s.world_geometry(tol);
            // Triangulated even without a fill: it also serves as the
            // placeholder tint of a pattern that is too fine / still computing.
            let mut fill = TriMesh::default();
            let c = s.style.fill.map_or(Color32::TRANSPARENT, to_color32);
            for p in &geom.0 {
                fill.add_polygon(p, c);
            }
            let bounds = crate::shapes::bounds_of(&geom);
            self.shapes.insert(
                id,
                ShapeEntry {
                    rev: s.rev,
                    tol: tolb,
                    geom,
                    bounds,
                    fill,
                    stroke: None,
                    frame,
                    src_geom: s.geom.clone(),
                    src_xf: s.xf.clone(),
                    src_style: s.style.clone(),
                },
            );
        }
        let e = self.shapes.get_mut(&id).unwrap_or_else(|| unreachable!());
        e.frame = frame;
        e
    }

    /// The shape's stroke as filled geometry (cached; same outlining as Flatten).
    fn stroke_mesh(&mut self, id: ObjId, s: &ShapeObj, tol: f64) -> Option<&TriMesh> {
        let color = s.style.stroke?;
        self.shape(id, s, tol);
        let e = self.shapes.get_mut(&id)?;
        if e.stroke.is_none() {
            let area = crate::flatten::shape_stroke_area(s, tol);
            let mut mesh = TriMesh::default();
            let c = to_color32(color);
            for p in &area.0 {
                mesh.add_polygon(p, c);
            }
            e.stroke = Some(mesh);
        }
        e.stroke.as_ref()
    }

    /// The pattern preview of a shape, if one is ready for its current state.
    ///
    /// Cheap previews are computed right here. Expensive ones (many tiles, or a
    /// previous run that was slow) run on a worker thread; meanwhile `None` is
    /// returned and the caller draws a placeholder tint, so the UI never waits.
    fn pattern(
        &mut self,
        id: ObjId,
        s: &ShapeObj,
        doc: &Document,
        interacting: bool,
        est_tiles: f64,
        ctx: &egui::Context,
    ) -> Option<&PatternEntry> {
        let key = settings_key(doc);
        let frame = self.frame;
        let e = self.patterns.entry(id).or_insert_with(|| PatternEntry {
            rev: u64::MAX,
            key: 0,
            exact: false,
            mesh: ChunkedMesh::default(),
            pieces: 0,
            capped: false,
            exact_ms: 0.0,
            frame,
            job: None,
        });
        e.frame = frame;

        // Collect a finished background job (discarded if the shape changed meanwhile).
        if let Some(job) = &e.job {
            match job.rx.try_recv() {
                Ok(done) => {
                    if job.rev == s.rev && job.key == key {
                        install(e, s.rev, key, done);
                    }
                    e.job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => e.job = None,
            }
        }

        let current = e.rev == s.rev && e.key == key;
        let needed = !current || (!e.exact && !interacting);
        if needed && e.job.is_none() {
            let fast = interacting && e.exact_ms > SLOW_PREVIEW_MS;
            let heavy = e.exact_ms > ASYNC_MS || est_tiles > ASYNC_TILES;
            if heavy && cfg!(not(target_arch = "wasm32")) {
                let (tx, rx) = std::sync::mpsc::channel();
                let (shape, settings, ctx) = (s.clone(), doc.settings.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let done = compute_pattern(&shape, &settings, fast);
                    if tx.send(done).is_ok() {
                        ctx.request_repaint();
                    }
                });
                e.job = Some(PatternJob { rev: s.rev, key, rx });
            } else {
                let done = compute_pattern(s, &doc.settings, fast);
                install(e, s.rev, key, done);
            }
        }
        if e.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        (e.rev == s.rev && e.key == key).then_some(&*e)
    }

    fn gc(&mut self) {
        if self.frame.is_multiple_of(120) {
            let f = self.frame;
            self.shapes.retain(|_, e| f - e.frame < 120);
            self.patterns.retain(|_, e| f - e.frame < 120);
        }
    }
}

fn install(e: &mut PatternEntry, rev: u64, key: u64, done: PatternDone) {
    e.rev = rev;
    e.key = key;
    e.exact = done.exact;
    e.mesh = done.mesh;
    e.pieces = done.pieces;
    e.capped = done.capped;
    if done.exact {
        e.exact_ms = done.ms;
    }
}

/// Average colour of a pattern, with alpha = how much of each cell its tile
/// covers — what a very fine pattern looks like from a distance.
fn pattern_tint(p: &crate::pattern::Pattern) -> Color32 {
    use crate::pattern::ColorMode;
    let cols: Vec<Color> = match p.color_mode {
        ColorMode::Single => vec![p.color_a],
        ColorMode::Alternating => vec![p.color_a, p.color_b],
        ColorMode::Random if !p.palette.is_empty() => p.palette.clone(),
        ColorMode::Random => vec![p.color_a],
    };
    let n = cols.len() as f64;
    let avg = |f: fn(&Color) -> u8| (cols.iter().map(|c| f64::from(f(c))).sum::<f64>() / n).round() as u8;
    let cell = p.cell_size.max(1e-9);
    let cover = (((cell - p.gap).max(0.0) / cell) * p.tile_scale_pct / 100.0).powi(2).clamp(0.05, 1.0);
    Color32::from_rgba_unmultiplied(avg(|c| c.r), avg(|c| c.g), avg(|c| c.b), (cover * 0.85 * 255.0) as u8)
}

/// Accumulates filled triangles into one mesh; flushed before strokes so z-order is kept.
struct Batch<'a> {
    painter: &'a Painter,
    xf: Xf,
    mesh: Mesh,
}

impl Batch<'_> {
    fn add(&mut self, tm: &TriMesh, off: [f64; 2]) {
        if tm.idx.is_empty() {
            return;
        }
        if self.mesh.vertices.len() + tm.pos.len() > 2_000_000 {
            self.flush();
        }
        let base = self.mesh.vertices.len() as u32;
        self.mesh.vertices.extend(tm.pos.iter().zip(&tm.col).map(|(p, c)| Vertex {
            pos: self.xf.s([p[0] + off[0], p[1] + off[1]]),
            uv: WHITE_UV,
            color: *c,
        }));
        self.mesh.indices.extend(tm.idx.iter().map(|i| base + i));
    }

    /// Like `add`, but every vertex gets `color`.
    fn add_tinted(&mut self, tm: &TriMesh, off: [f64; 2], color: Color32) {
        if tm.idx.is_empty() {
            return;
        }
        if self.mesh.vertices.len() + tm.pos.len() > 2_000_000 {
            self.flush();
        }
        let base = self.mesh.vertices.len() as u32;
        self.mesh.vertices.extend(tm.pos.iter().map(|p| Vertex {
            pos: self.xf.s([p[0] + off[0], p[1] + off[1]]),
            uv: WHITE_UV,
            color,
        }));
        self.mesh.indices.extend(tm.idx.iter().map(|i| base + i));
    }

    /// Add only the chunks that overlap the visible area.
    fn add_chunked(&mut self, cm: &ChunkedMesh, off: [f64; 2], visible: &Bounds) {
        for (b, m) in &cm.chunks {
            let b = [b[0] + off[0], b[1] + off[1], b[2] + off[0], b[3] + off[1]];
            if bounds_overlap(&b, visible) {
                self.add(m, off);
            }
        }
    }

    fn flush(&mut self) {
        if !self.mesh.is_empty() {
            self.painter.add(Shape::mesh(std::mem::take(&mut self.mesh)));
        }
    }
}

// ---------------------------------------------------------------------------
// Canvas state
// ---------------------------------------------------------------------------

enum ResizeKind {
    /// One shape, resized along its own (rotated) axes.
    Single { id: ObjId, xf0: Transform },
    /// Several objects or a group, scaled inside their bounding box.
    Box { b0: Bounds },
}

enum Drag {
    Pan,
    Move {
        start: [f64; 2],
        shapes: Vec<(ObjId, ShapeObj)>,
        groups: Vec<ObjId>,
        delta: [f64; 2],
    },
    Resize {
        handle: [i8; 2],
        kind: ResizeKind,
        originals: Vec<Object>,
        changed: bool,
    },
    Create {
        tool: Tool,
        start: [f64; 2],
        cur: [f64; 2],
    },
    Marquee {
        start: [f64; 2],
        cur: [f64; 2],
        additive: bool,
    },
    PatternMove {
        id: ObjId,
        start: [f64; 2],
        frame0: PatternFrame,
        changed: bool,
    },
    PatternScale {
        id: ObjId,
        frame0: PatternFrame,
        d0: f64,
        changed: bool,
    },
    PatternRotate {
        id: ObjId,
        frame0: PatternFrame,
        a0: f64,
        changed: bool,
    },
}

/// What the canvas may read and change.
pub struct CanvasIo<'a> {
    pub doc: &'a mut Document,
    pub history: &'a mut History,
    pub selection: &'a mut Vec<ObjId>,
    pub tool: Tool,
    pub status: &'a mut String,
}

pub struct Canvas {
    pub zoom: f64,
    /// Screen offset (points) of the world origin from the canvas' top-left corner.
    pub pan: Vec2,
    pub fit_requested: bool,
    pub cursor_mm: Option<[f64; 2]>,
    pub pattern_edit: bool,
    pub pen_points: Vec<[f64; 2]>,
    rect: Rect,
    cache: RenderCache,
    /// The plain mouse wheel scrolls (pans) the view instead of zooming;
    /// Ctrl + wheel / pinch then zooms.
    pub wheel_scrolls: bool,
    /// Patterned shapes drawn as a tint because their tiles are below
    /// `LOD_TILE_PX` at the current zoom (value: estimated tile count).
    too_fine: HashMap<ObjId, f64>,
    drag: Option<Drag>,
    last_shift: bool,
    last_alt: bool,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: vec2(60.0, 60.0),
            fit_requested: true,
            cursor_mm: None,
            pattern_edit: false,
            pen_points: Vec::new(),
            rect: Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
            cache: RenderCache::default(),
            wheel_scrolls: false,
            too_fine: HashMap::new(),
            drag: None,
            last_shift: false,
            last_alt: false,
        }
    }
}

/// The single selected top-level shape with a pattern fill, if any.
pub fn pattern_target(doc: &Document, selection: &[ObjId]) -> Option<ObjId> {
    match selection {
        [id] if doc.get(*id).is_some_and(Object::has_pattern) => Some(*id),
        _ => None,
    }
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn pattern_frame_of(doc: &Document, id: ObjId) -> Option<PatternFrame> {
    let s = doc.get(id)?.as_shape()?;
    Some(s.pattern.as_ref()?.world_frame(&s.xf))
}

fn set_pattern_frame(doc: &mut Document, id: ObjId, f: PatternFrame) {
    if let Some(s) = doc.shape_mut(id) {
        let xf = s.xf.clone();
        if let Some(fill) = s.pattern.as_mut() {
            fill.set_world_frame(&xf, f);
            s.touch();
        }
    }
}

impl Canvas {
    fn xf(&self) -> Xf {
        let k = self.zoom * PT_PER_MM;
        Xf {
            o: [
                f64::from(self.rect.min.x + self.pan.x),
                f64::from(self.rect.min.y + self.pan.y),
            ],
            k,
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Fit the artboard into the visible canvas.
    pub fn fit(&mut self, w: f64, h: f64) {
        let avail = self.rect.size() - vec2(RULER + 40.0, RULER + 40.0);
        let z = (f64::from(avail.x) / (w * PT_PER_MM)).min(f64::from(avail.y) / (h * PT_PER_MM));
        self.zoom = z.clamp(0.02, 200.0);
        self.center_on([w / 2.0, h / 2.0]);
    }

    fn center_on(&mut self, c: [f64; 2]) {
        let k = self.zoom * PT_PER_MM;
        let mid = self.rect.size() / 2.0 + vec2(RULER / 2.0, RULER / 2.0);
        self.pan = vec2(mid.x - (c[0] * k) as f32, mid.y - (c[1] * k) as f32);
    }

    /// Zoom to an absolute factor keeping the view centre fixed.
    pub fn set_zoom(&mut self, z: f64) {
        let centre = self.xf().w(self.rect.center());
        self.zoom = z.clamp(0.02, 200.0);
        self.center_on(centre);
    }

    pub fn pattern_stats(&self, id: ObjId) -> Option<PatternStats> {
        if let Some(&est) = self.too_fine.get(&id) {
            return Some(PatternStats {
                pieces: 0,
                ms: 0.0,
                capped: false,
                exact: false,
                too_fine: Some(est),
                computing: false,
            });
        }
        self.cache.patterns.get(&id).map(|e| PatternStats {
            pieces: e.pieces,
            ms: e.exact_ms,
            capped: e.capped,
            exact: e.exact,
            too_fine: None,
            computing: e.job.is_some(),
        })
    }

    /// Bounds of an object using the render cache (cheap for big groups).
    pub fn object_bounds(&mut self, o: &Object, tol: f64) -> Option<Bounds> {
        match &o.kind {
            ObjectKind::Shape(s) => self.cache.shape(o.id, s, tol).bounds,
            ObjectKind::Group(c) => c.iter().fold(None, |b, ch| bounds_union(b, self.object_bounds(ch, tol))),
        }
    }

    fn selection_bounds(&mut self, doc: &Document, sel: &[ObjId]) -> Option<Bounds> {
        let tol = doc.settings.tolerance();
        if sel.len() <= 16 {
            return sel
                .iter()
                .filter_map(|&id| doc.get(id))
                .fold(None, |b, o| bounds_union(b, self.object_bounds(o, tol)));
        }
        // Large selections (Select All on thousands of pieces): one pass over the
        // top level instead of a search per selected id.
        let want: std::collections::HashSet<ObjId> = sel.iter().copied().collect();
        doc.objects
            .iter()
            .filter(|o| want.contains(&o.id))
            .fold(None, |b, o| bounds_union(b, self.object_bounds(o, tol)))
    }

    // ---- hit testing -------------------------------------------------------

    fn hit_shape(&mut self, id: ObjId, s: &ShapeObj, p: [f64; 2], tol_mm: f64, flat_tol: f64) -> bool {
        let e = self.cache.shape(id, s, flat_tol);
        let half_stroke = if s.style.stroke.is_some() { s.style.stroke_width / 2.0 } else { 0.0 };
        let pad = tol_mm + half_stroke;
        let Some(b) = e.bounds else { return false };
        if p[0] < b[0] - pad || p[0] > b[2] + pad || p[1] < b[1] - pad || p[1] > b[3] + pad {
            return false;
        }
        if (s.style.fill.is_some() || s.pattern.is_some()) && point_in_evenodd(p, &e.geom) {
            return true;
        }
        e.geom.0.iter().any(|poly| {
            std::iter::once(poly.exterior()).chain(poly.interiors()).any(|r| {
                r.0.windows(2)
                    .any(|w| dist_to_segment(p, [w[0].x, w[0].y], [w[1].x, w[1].y]) <= pad)
            })
        })
    }

    fn hit_object(&mut self, o: &Object, p: [f64; 2], tol_mm: f64, flat_tol: f64) -> bool {
        match &o.kind {
            ObjectKind::Shape(s) => self.hit_shape(o.id, s, p, tol_mm, flat_tol),
            ObjectKind::Group(c) => c.iter().rev().any(|ch| self.hit_object(ch, p, tol_mm, flat_tol)),
        }
    }

    /// Topmost top-level object under `p`.
    fn hit_top(&mut self, doc: &Document, p: [f64; 2]) -> Option<ObjId> {
        let tol_mm = HIT_PX / self.xf().k;
        let flat = doc.settings.tolerance();
        doc.objects
            .iter()
            .rev()
            .find(|o| self.hit_object(o, p, tol_mm, flat))
            .map(|o| o.id)
    }

    /// Selection handles in world coordinates: (handle id, position).
    fn handles(&mut self, doc: &Document, sel: &[ObjId]) -> Vec<([i8; 2], [f64; 2])> {
        let mut out = Vec::new();
        if sel.is_empty() {
            return out;
        }
        let single = match sel {
            [id] => doc.get(*id).and_then(Object::as_shape).map(|s| s.xf.sanitized()),
            _ => None,
        };
        // Computed once, not once per handle.
        let sel_bounds = if single.is_none() { self.selection_bounds(doc, sel) } else { None };
        for hy in -1i8..=1 {
            for hx in -1i8..=1 {
                if hx == 0 && hy == 0 {
                    continue;
                }
                let p = if let Some(xf) = &single {
                    xf.local_to_world([f64::from(hx) * xf.w / 2.0, f64::from(hy) * xf.h / 2.0])
                } else if let Some(b) = sel_bounds {
                    let pick = |h: i8, lo: f64, hi: f64| match h {
                        -1 => lo,
                        0 => (lo + hi) / 2.0,
                        _ => hi,
                    };
                    [pick(hx, b[0], b[2]), pick(hy, b[1], b[3])]
                } else {
                    continue;
                };
                out.push(([hx, hy], p));
            }
        }
        out
    }

    fn handle_at(&mut self, doc: &Document, sel: &[ObjId], p: [f64; 2]) -> Option<[i8; 2]> {
        let xf = self.xf();
        let sp = xf.s(p);
        self.handles(doc, sel)
            .into_iter()
            .find(|(_, hp)| (xf.s(*hp) - sp).length() <= HANDLE + 3.0)
            .map(|(h, _)| h)
    }

    fn pattern_handles(&self, f: &PatternFrame) -> ([f64; 2], [f64; 2]) {
        let l = PATTERN_HANDLE_LEN / self.xf().k;
        let a = rotate_vec([l, 0.0], f.rotation_deg);
        let b = rotate_vec([0.0, -l], f.rotation_deg);
        (
            [f.origin[0] + a[0], f.origin[1] + a[1]],
            [f.origin[0] + b[0], f.origin[1] + b[1]],
        )
    }

    // ---- input -------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut Ui, io: &mut CanvasIo) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        self.rect = resp.rect;
        if self.fit_requested && self.rect.width() > 100.0 {
            self.fit(io.doc.artboard.width, io.doc.artboard.height);
            self.fit_requested = false;
        }
        self.cache.frame += 1;

        let target = pattern_target(io.doc, io.selection);
        if self.pattern_edit && target.is_none() {
            self.pattern_edit = false;
        }
        if io.tool != Tool::Pen {
            self.pen_points.clear();
        }

        if let Some(h) = resp.hover_pos() {
            self.handle_wheel(ui, h, io, target);
        }
        self.handle_pointer(ui, &resp, io, target);
        self.cursor_mm = ui
            .input(|i| i.pointer.latest_pos())
            .filter(|p| self.rect.contains(*p))
            .map(|p| self.xf().w(p));
        self.update_cursor_icon(ui, &resp, io, target);

        let painter = painter.with_clip_rect(self.rect);
        self.paint(&painter, io, target);
        self.cache.gc();
        if self.drag.is_some() {
            ui.ctx().request_repaint();
        }
    }

    fn handle_wheel(&mut self, ui: &Ui, hover: Pos2, io: &mut CanvasIo, target: Option<ObjId>) {
        let events: Vec<_> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    Event::MouseWheel {
                        unit, delta, modifiers, ..
                    } => Some((*unit, *delta, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        // Touchpad pinch (and Ctrl + touchpad scroll) arrives as a zoom factor.
        let pinch = ui.input(|i| {
            i.events.iter().fold(1.0f32, |z, e| match e {
                Event::Zoom(f) => z * *f,
                _ => z,
            })
        });
        if (pinch - 1.0).abs() > 1e-6 {
            self.zoom_at(hover, f64::from(pinch));
        }
        for (unit, delta, m) in events {
            let raw = f64::from(if delta.y.abs() >= delta.x.abs() { delta.y } else { delta.x });
            let notches = match unit {
                MouseWheelUnit::Point => raw / 40.0,
                MouseWheelUnit::Line => raw,
                MouseWheelUnit::Page => raw * 3.0,
            };
            if notches == 0.0 && delta.x == 0.0 {
                continue;
            }
            // Screen points to scroll for this event (a wheel notch ≈ 60 pt).
            let to_pts = |v: f32| match unit {
                MouseWheelUnit::Point => v,
                MouseWheelUnit::Line => v * 60.0,
                MouseWheelUnit::Page => v * self.rect.height(),
            };
            // In Pattern Edit, Ctrl / Alt + wheel scale / rotate the pattern.
            let pattern_wheel = target.is_some() && self.pattern_edit && (m.command || m.alt);
            if !pattern_wheel {
                // Windows sends a touchpad pinch as Ctrl + wheel, so Ctrl + wheel
                // always zooms; everything else below pans unless it is a plain wheel.
                if m.command {
                    self.zoom_at(hover, 1.15_f64.powf(notches));
                    continue;
                }
                if m.shift {
                    // Shift + wheel: pan sideways.
                    let d = if delta.x != 0.0 { delta.x } else { delta.y };
                    self.pan.x += to_pts(d);
                    continue;
                }
                let sideways = delta.x != 0.0 && delta.y == 0.0; // tilt wheel / touchpad
                let pan_gesture = unit == MouseWheelUnit::Point; // trackpad pan gesture (macOS)
                if m.alt || sideways || pan_gesture || self.wheel_scrolls {
                    self.pan += vec2(to_pts(delta.x), to_pts(delta.y));
                    continue;
                }
            }
            match target {
                Some(id) if pattern_wheel => {
                    let Some(mut f) = pattern_frame_of(io.doc, id) else { continue };
                    io.history.push_coalesced(io.doc, 0x5745_454c ^ id);
                    if m.command {
                        let mut pct = f.scale * 100.0 * 1.05_f64.powf(notches);
                        if m.shift {
                            pct = ((pct / 5.0).round() * 5.0).max(5.0);
                            if (pct - f.scale * 100.0).abs() < 1e-9 {
                                pct += 5.0 * notches.signum();
                            }
                        }
                        f.scale = pct / 100.0;
                    } else if m.shift {
                        let snapped = (f.rotation_deg / 15.0).round() * 15.0;
                        f.rotation_deg = snapped + 15.0 * notches.signum();
                    } else {
                        f.rotation_deg += notches;
                    }
                    set_pattern_frame(io.doc, id, f);
                }
                // Plain mouse wheel: zoom at the cursor.
                _ => self.zoom_at(hover, 1.15_f64.powf(notches)),
            }
        }
    }

    /// Zoom by `factor`, keeping the world point under `hover` in place.
    fn zoom_at(&mut self, hover: Pos2, factor: f64) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let before = self.xf().w(hover);
        self.zoom = (self.zoom * factor).clamp(0.02, 200.0);
        let k = self.zoom * PT_PER_MM;
        self.pan = vec2(
            hover.x - self.rect.min.x - (before[0] * k) as f32,
            hover.y - self.rect.min.y - (before[1] * k) as f32,
        );
    }

    /// Pan the view by a screen offset (points), e.g. from the arrow keys.
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan += vec2(dx, dy);
    }

    fn handle_pointer(&mut self, ui: &Ui, resp: &Response, io: &mut CanvasIo, target: Option<ObjId>) {
        let (space, shift, alt, press, latest) = ui.input(|i| {
            (
                i.key_down(Key::Space),
                i.modifiers.shift,
                i.modifiers.alt,
                i.pointer.press_origin(),
                i.pointer.latest_pos(),
            )
        });
        self.last_shift = shift;
        self.last_alt = alt;
        // Pan: Hand tool, middle button, or Space + left drag.
        let hand = io.tool == Tool::Hand;
        if resp.drag_started_by(PointerButton::Middle)
            || (resp.drag_started_by(PointerButton::Primary) && (space || hand))
        {
            self.drag = Some(Drag::Pan);
        }
        if matches!(self.drag, Some(Drag::Pan)) {
            self.pan += resp.drag_delta();
            if resp.drag_stopped() {
                self.drag = None;
            }
            return;
        }
        if resp.drag_started_by(PointerButton::Primary)
            && let Some(p0) = press
        {
            let w = self.xf().w(p0);
            self.begin_drag(w, shift, io, target);
        }
        if self.drag.is_some() {
            if let Some(p) = latest {
                let w = self.xf().w(p);
                self.update_drag(w, shift, io);
            }
            if resp.drag_stopped() {
                self.end_drag(io);
            }
        }
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let w = self.xf().w(p);
            self.click(w, shift, io);
        }
        if resp.double_clicked() && io.tool == Tool::Pen {
            self.finish_pen(io);
        }
    }

    fn begin_drag(&mut self, p: [f64; 2], shift: bool, io: &mut CanvasIo, target: Option<ObjId>) {
        match io.tool {
            Tool::Select => {
                if self.pattern_edit {
                    let Some(id) = target else { return };
                    let Some(frame0) = pattern_frame_of(io.doc, id) else { return };
                    let (sh, rh) = self.pattern_handles(&frame0);
                    let near = |q: [f64; 2]| dist(q, p) * self.xf().k <= f64::from(HANDLE) + 5.0;
                    let drag = if near(sh) {
                        Some(Drag::PatternScale {
                            id,
                            frame0,
                            d0: dist(p, frame0.origin).max(1e-9),
                            changed: false,
                        })
                    } else if near(rh) {
                        Some(Drag::PatternRotate {
                            id,
                            frame0,
                            a0: (p[1] - frame0.origin[1]).atan2(p[0] - frame0.origin[0]),
                            changed: false,
                        })
                    } else {
                        let tol = io.doc.settings.tolerance();
                        let inside = io
                            .doc
                            .get(id)
                            .and_then(|o| self.object_bounds(o, tol))
                            .is_some_and(|b| p[0] >= b[0] && p[0] <= b[2] && p[1] >= b[1] && p[1] <= b[3]);
                        inside.then_some(Drag::PatternMove {
                            id,
                            start: p,
                            frame0,
                            changed: false,
                        })
                    };
                    if drag.is_some() {
                        io.history.push(io.doc);
                        self.drag = drag;
                    }
                    return;
                }
                let sel = io.selection.clone();
                if let Some(handle) = self.handle_at(io.doc, &sel, p) {
                    // Selections are top-level objects: one linear pass for any size.
                    let originals: Vec<Object> =
                        io.doc.top_indices(&sel).into_iter().map(|i| io.doc.objects[i].clone()).collect();
                    let kind = match (sel.as_slice(), originals.first().and_then(Object::as_shape)) {
                        ([id], Some(s)) => ResizeKind::Single {
                            id: *id,
                            xf0: s.xf.sanitized(),
                        },
                        _ => match self.selection_bounds(io.doc, &sel) {
                            Some(b0) => ResizeKind::Box { b0 },
                            None => return,
                        },
                    };
                    io.history.push(io.doc);
                    self.drag = Some(Drag::Resize {
                        handle,
                        kind,
                        originals,
                        changed: false,
                    });
                    return;
                }
                match self.hit_top(io.doc, p) {
                    Some(id) => {
                        if !io.selection.contains(&id) {
                            if shift {
                                io.selection.push(id);
                            } else {
                                *io.selection = vec![id];
                            }
                        }
                        let mut shapes = Vec::new();
                        let mut groups = Vec::new();
                        for i in io.doc.top_indices(io.selection) {
                            let o = &io.doc.objects[i];
                            match &o.kind {
                                ObjectKind::Shape(s) => shapes.push((o.id, s.clone())),
                                ObjectKind::Group(_) => groups.push(o.id),
                            }
                        }
                        io.history.push(io.doc);
                        self.drag = Some(Drag::Move {
                            start: p,
                            shapes,
                            groups,
                            delta: [0.0, 0.0],
                        });
                    }
                    None => {
                        self.drag = Some(Drag::Marquee {
                            start: p,
                            cur: p,
                            additive: shift,
                        });
                    }
                }
            }
            // Hand drags are handled as pans before this point.
            Tool::Pen | Tool::Hand => {}
            tool => {
                self.drag = Some(Drag::Create { tool, start: p, cur: p });
            }
        }
    }

    fn update_drag(&mut self, p: [f64; 2], shift: bool, io: &mut CanvasIo) {
        let Some(drag) = self.drag.as_mut() else { return };
        match drag {
            Drag::Pan => {}
            Drag::Move {
                start,
                shapes,
                delta,
                ..
            } => {
                let mut d = [p[0] - start[0], p[1] - start[1]];
                if shift {
                    if d[0].abs() > d[1].abs() {
                        d[1] = 0.0;
                    } else {
                        d[0] = 0.0;
                    }
                }
                if d == *delta {
                    return;
                }
                *delta = d;
                // Moved shapes are top-level (they come from the selection): one
                // index pass per frame instead of a document search per shape.
                let index: HashMap<ObjId, usize> = if shapes.len() > 16 {
                    io.doc.objects.iter().enumerate().map(|(i, o)| (o.id, i)).collect()
                } else {
                    HashMap::new()
                };
                for (id, orig) in shapes.iter() {
                    let s = match index.get(id) {
                        Some(&i) => io.doc.objects[i].as_shape_mut(),
                        None => io.doc.shape_mut(*id),
                    };
                    if let Some(s) = s {
                        *s = orig.clone();
                        s.xf.cx += d[0];
                        s.xf.cy += d[1];
                        s.touch();
                    }
                }
            }
            Drag::Resize {
                handle,
                kind,
                originals,
                changed,
            } => {
                let (hx, hy) = (f64::from(handle[0]), f64::from(handle[1]));
                match kind {
                    ResizeKind::Single { id, xf0 } => {
                        let local = xf0.world_to_local(p);
                        let (fx, fy) = (-hx * xf0.w / 2.0, -hy * xf0.h / 2.0);
                        let (mut w, mut h) = (xf0.w, xf0.h);
                        let (mut sx, mut sy) = (1.0, 1.0);
                        if hx != 0.0 {
                            w = (local[0] - fx).abs();
                            sx = if local[0] >= fx { 1.0 } else { -1.0 };
                        }
                        if hy != 0.0 {
                            h = (local[1] - fy).abs();
                            sy = if local[1] >= fy { 1.0 } else { -1.0 };
                        }
                        if shift && hx != 0.0 && hy != 0.0 {
                            let u = (w / xf0.w).max(h / xf0.h);
                            w = xf0.w * u;
                            h = xf0.h * u;
                        }
                        let (w, h) = (w.max(0.01), h.max(0.01));
                        let cx = if hx != 0.0 { fx + sx * w / 2.0 } else { 0.0 };
                        let cy = if hy != 0.0 { fy + sy * h / 2.0 } else { 0.0 };
                        let c = xf0.local_to_world([cx, cy]);
                        if let Some(s) = io.doc.shape_mut(*id) {
                            s.xf = Transform {
                                cx: c[0],
                                cy: c[1],
                                w,
                                h,
                                rotation_deg: xf0.rotation_deg,
                            };
                            s.touch();
                            *changed = true;
                        }
                    }
                    ResizeKind::Box { b0 } => {
                        let w0 = (b0[2] - b0[0]).max(1e-6);
                        let h0 = (b0[3] - b0[1]).max(1e-6);
                        let ax = if hx < 0.0 { b0[2] } else { b0[0] };
                        let ay = if hy < 0.0 { b0[3] } else { b0[1] };
                        let mut sx = if hx != 0.0 { ((p[0] - ax) * hx).max(0.01) / w0 } else { 1.0 };
                        let mut sy = if hy != 0.0 { ((p[1] - ay) * hy).max(0.01) / h0 } else { 1.0 };
                        if shift && hx != 0.0 && hy != 0.0 {
                            let u = sx.max(sy);
                            sx = u;
                            sy = u;
                        }
                        let anchor = [
                            if hx == 0.0 { (b0[0] + b0[2]) / 2.0 } else { ax },
                            if hy == 0.0 { (b0[1] + b0[3]) / 2.0 } else { ay },
                        ];
                        let index: HashMap<ObjId, usize> =
                            io.doc.objects.iter().enumerate().map(|(i, o)| (o.id, i)).collect();
                        for orig in originals.iter() {
                            if let Some(&i) = index.get(&orig.id) {
                                let mut o = orig.clone();
                                o.scale_about(anchor, sx, sy);
                                io.doc.objects[i] = o;
                                *changed = true;
                            }
                        }
                    }
                }
            }
            Drag::Create { cur, .. } | Drag::Marquee { cur, .. } => {
                *cur = p;
            }
            Drag::PatternMove {
                id,
                start,
                frame0,
                changed,
            } => {
                let mut f = *frame0;
                f.origin = [f.origin[0] + p[0] - start[0], f.origin[1] + p[1] - start[1]];
                *changed |= f.origin != frame0.origin;
                set_pattern_frame(io.doc, *id, f);
            }
            Drag::PatternScale {
                id,
                frame0,
                d0,
                changed,
            } => {
                let mut f = *frame0;
                let mut pct = frame0.scale * 100.0 * dist(p, frame0.origin) / *d0;
                if shift {
                    pct = (pct / 5.0).round() * 5.0;
                }
                f.scale = pct.max(1.0) / 100.0;
                *changed |= f.scale != frame0.scale;
                set_pattern_frame(io.doc, *id, f);
            }
            Drag::PatternRotate {
                id,
                frame0,
                a0,
                changed,
            } => {
                let mut f = *frame0;
                let a = (p[1] - frame0.origin[1]).atan2(p[0] - frame0.origin[0]);
                let mut rot = frame0.rotation_deg + (a - *a0).to_degrees();
                if shift {
                    rot = (rot / 15.0).round() * 15.0;
                }
                f.rotation_deg = rot;
                *changed |= f.rotation_deg != frame0.rotation_deg;
                set_pattern_frame(io.doc, *id, f);
            }
        }
    }

    /// Box of a Create drag (Shift = square, Alt = from centre).
    fn create_box(start: [f64; 2], cur: [f64; 2], shift: bool, alt: bool) -> Bounds {
        let mut d = [cur[0] - start[0], cur[1] - start[1]];
        if shift {
            let m = d[0].abs().max(d[1].abs());
            d = [m.copysign(d[0]), m.copysign(d[1])];
        }
        if alt {
            [start[0] - d[0].abs(), start[1] - d[1].abs(), start[0] + d[0].abs(), start[1] + d[1].abs()]
        } else {
            [
                start[0].min(start[0] + d[0]),
                start[1].min(start[1] + d[1]),
                start[0].max(start[0] + d[0]),
                start[1].max(start[1] + d[1]),
            ]
        }
    }

    fn end_drag(&mut self, io: &mut CanvasIo) {
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Pan => {}
            Drag::Move { groups, delta, .. } => {
                if delta == [0.0, 0.0] {
                    io.history.discard_last();
                } else {
                    for g in groups {
                        if let Some(o) = io.doc.get_mut(g) {
                            o.translate(delta[0], delta[1]);
                        }
                    }
                }
            }
            Drag::Resize { changed, .. }
            | Drag::PatternMove { changed, .. }
            | Drag::PatternScale { changed, .. }
            | Drag::PatternRotate { changed, .. } => {
                if !changed {
                    io.history.discard_last();
                }
            }
            Drag::Create { tool, start, cur } => {
                let b = Self::create_box(start, cur, self.last_shift, self.last_alt);
                if (b[2] - b[0]) * self.xf().k >= 3.0 && (b[3] - b[1]) * self.xf().k >= 3.0 {
                    create_shape(tool, b, io);
                }
            }
            Drag::Marquee { start, cur, additive } => {
                let m = [start[0].min(cur[0]), start[1].min(cur[1]), start[0].max(cur[0]), start[1].max(cur[1])];
                let tol = io.doc.settings.tolerance();
                let hits: Vec<ObjId> = io
                    .doc
                    .objects
                    .iter()
                    .filter(|o| self.object_bounds(o, tol).is_some_and(|b| bounds_overlap(&b, &m)))
                    .map(|o| o.id)
                    .collect();
                if !additive {
                    io.selection.clear();
                }
                // A set keeps marquee-selecting thousands of pieces linear.
                let mut have: std::collections::HashSet<ObjId> = io.selection.iter().copied().collect();
                for id in hits {
                    if have.insert(id) {
                        io.selection.push(id);
                    }
                }
            }
        }
    }

    fn click(&mut self, p: [f64; 2], shift: bool, io: &mut CanvasIo) {
        match io.tool {
            Tool::Select => {
                if self.pattern_edit {
                    return;
                }
                match self.hit_top(io.doc, p) {
                    Some(id) if shift => {
                        if let Some(i) = io.selection.iter().position(|&s| s == id) {
                            io.selection.remove(i);
                        } else {
                            io.selection.push(id);
                        }
                    }
                    Some(id) => *io.selection = vec![id],
                    None if !shift => io.selection.clear(),
                    None => {}
                }
            }
            Tool::Pen => {
                let close = self.pen_points.len() >= 3
                    && dist(self.pen_points[0], p) * self.xf().k <= 8.0;
                if close {
                    self.finish_pen(io);
                } else {
                    self.pen_points.push(p);
                }
            }
            tool => {
                // A plain click creates a 20 × 20 mm shape centred on the click.
                create_shape(tool, [p[0] - 10.0, p[1] - 10.0, p[0] + 10.0, p[1] + 10.0], io);
            }
        }
    }

    /// Close the pen path (double-click / Enter).
    pub fn finish_pen(&mut self, io: &mut CanvasIo) {
        let min_gap = 2.0 / self.xf().k;
        let mut pts: Vec<[f64; 2]> = Vec::new();
        for p in std::mem::take(&mut self.pen_points) {
            if pts.last().is_none_or(|q| dist(*q, p) > min_gap) {
                pts.push(p);
            }
        }
        if pts.len() >= 3 && dist(pts[0], pts[pts.len() - 1]) <= min_gap {
            pts.pop();
        }
        if pts.len() < 3 {
            *io.status = "Pen: a path needs at least 3 points.".into();
            return;
        }
        let mp = MultiPolygon::new(vec![Polygon::new(LineString::from(pts), vec![])]);
        if let Some((geom, xf)) = custom_from_world(&mp) {
            io.history.push(io.doc);
            let n = io.doc.next_id;
            let id = io.doc.add_shape(format!("Path {n}"), ShapeObj::new(geom, xf, Style::default()));
            *io.selection = vec![id];
        }
    }

    fn update_cursor_icon(&mut self, ui: &Ui, resp: &Response, io: &mut CanvasIo, target: Option<ObjId>) {
        if !resp.hovered() && self.drag.is_none() {
            return;
        }
        let space = ui.input(|i| i.key_down(Key::Space));
        let icon = match (&self.drag, io.tool) {
            (Some(Drag::Pan), _) => CursorIcon::Grabbing,
            (Some(Drag::Move { .. } | Drag::PatternMove { .. }), _) => CursorIcon::Grabbing,
            (Some(Drag::PatternRotate { .. }), _) => CursorIcon::Alias,
            _ if space => CursorIcon::Grab,
            (_, Tool::Hand) => CursorIcon::Grab,
            (_, Tool::Select) => {
                let Some(p) = self.cursor_mm else { return };
                if self.pattern_edit {
                    match target.and_then(|id| pattern_frame_of(io.doc, id)) {
                        Some(f) => {
                            let (sh, rh) = self.pattern_handles(&f);
                            let k = self.xf().k;
                            if dist(sh, p) * k <= 9.0 {
                                CursorIcon::ResizeHorizontal
                            } else if dist(rh, p) * k <= 9.0 {
                                CursorIcon::Alias
                            } else {
                                CursorIcon::Move
                            }
                        }
                        None => CursorIcon::Default,
                    }
                } else {
                    let sel = io.selection.clone();
                    match self.handle_at(io.doc, &sel, p) {
                        Some([0, _]) => CursorIcon::ResizeVertical,
                        Some([_, 0]) => CursorIcon::ResizeHorizontal,
                        Some([a, b]) if a == b => CursorIcon::ResizeNwSe,
                        Some(_) => CursorIcon::ResizeNeSw,
                        None => CursorIcon::Default,
                    }
                }
            }
            _ => CursorIcon::Crosshair,
        };
        ui.ctx().set_cursor_icon(icon);
    }

    // ---- painting ----------------------------------------------------------

    fn move_offset(&self, id: ObjId) -> [f64; 2] {
        match &self.drag {
            Some(Drag::Move { groups, delta, .. }) if groups.contains(&id) => *delta,
            _ => [0.0, 0.0],
        }
    }

    fn paint(&mut self, painter: &Painter, io: &mut CanvasIo, target: Option<ObjId>) {
        let rect = self.rect;
        let xf = self.xf();
        let doc: &Document = io.doc;

        // Background, artboard, grid.
        painter.rect_filled(rect, 0.0, Color32::from_gray(0xc8));
        let ab = Rect::from_min_max(xf.s([0.0, 0.0]), xf.s([doc.artboard.width, doc.artboard.height]));
        painter.rect_filled(ab.translate(vec2(3.0, 3.0)), 0.0, Color32::from_black_alpha(50));
        painter.rect_filled(ab, 0.0, Color32::WHITE);
        self.paint_grid(painter, xf);
        paint_watermark(painter, ab, rect);
        painter.rect_stroke(ab, 0.0, Stroke::new(1.0, Color32::from_gray(0x90)), StrokeKind::Outside);

        // Objects.
        let vis0 = xf.w(rect.min);
        let vis1 = xf.w(rect.max);
        let visible = [vis0[0], vis0[1], vis1[0], vis1[1]];
        // Dragging anything (canvas, or a number field such as Scale %) counts
        // as interacting: slow patterns show the quick preview meanwhile.
        let interacting = self.drag.is_some() || painter.ctx().input(|i| i.pointer.any_down());
        let mut batch = Batch {
            painter,
            xf,
            mesh: Mesh::default(),
        };
        for o in &doc.objects {
            let off = self.move_offset(o.id);
            self.paint_object(&mut batch, o, off, doc, &visible, interacting);
        }
        batch.flush();

        self.paint_selection(painter, io, target);
        self.paint_overlays(painter, io.doc);
        self.paint_rulers(painter);
    }

    fn paint_object(
        &mut self,
        batch: &mut Batch,
        o: &Object,
        off: [f64; 2],
        doc: &Document,
        visible: &Bounds,
        interacting: bool,
    ) {
        match &o.kind {
            ObjectKind::Group(children) => {
                for c in children.iter() {
                    self.paint_object(batch, c, off, doc, visible, interacting);
                }
            }
            ObjectKind::Shape(s) => {
                let tol = doc.settings.tolerance();
                let e = self.cache.shape(o.id, s, tol);
                let Some(b) = e.bounds else { return };
                // Miter corners can reach miter_limit × width / 2 beyond the edge.
                let pad = s.style.stroke_width.max(0.0) * s.style.miter_limit.clamp(1.0, 100.0);
                let ob = [b[0] + off[0] - pad, b[1] + off[1] - pad, b[2] + off[0] + pad, b[3] + off[1] + pad];
                if !bounds_overlap(&ob, visible) {
                    return;
                }
                let shape_area = (b[2] - b[0]) * (b[3] - b[1]);
                let base = s.pattern.as_ref().is_none_or(|f| f.keep_background);
                if base {
                    if s.style.fill.is_some() {
                        batch.add(&e.fill, off);
                    }
                    if let Some(c) = s.style.stroke.filter(|_| s.style.stroke_width > 0.0) {
                        let px = (s.style.stroke_width * batch.xf.k) as f32;
                        if px >= 2.5 {
                            // Wide enough to see joins: draw the real outlined stroke,
                            // exactly as Flatten / Export will produce it.
                            if let Some(m) = self.cache.stroke_mesh(o.id, s, tol) {
                                batch.add(m, off);
                            }
                        } else {
                            // Hairline: an anti-aliased line, split at sharp corners.
                            batch.flush();
                            let stroke = Stroke::new(px.max(0.6), to_color32(c));
                            let e = self.cache.shape(o.id, s, tol);
                            for poly in &e.geom.0 {
                                for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                                    let pts: Vec<Pos2> = ring_points(ring)
                                        .into_iter()
                                        .map(|p| batch.xf.s([p[0] + off[0], p[1] + off[1]]))
                                        .collect();
                                    add_outline(batch.painter, &pts, stroke);
                                }
                            }
                        }
                    }
                }
                if let Some(fill) = &s.pattern {
                    let frame = fill.world_frame(&s.xf);
                    let cell = crate::pattern::world_cell_size(&fill.pattern, &frame).max(1e-9);
                    let est_tiles = shape_area / (cell * cell);
                    let tint = pattern_tint(&fill.pattern);
                    if cell * batch.xf.k < LOD_TILE_PX {
                        // Finer than the screen can show: tint, compute nothing.
                        self.too_fine.insert(o.id, est_tiles);
                        let e = self.cache.shape(o.id, s, tol);
                        batch.add_tinted(&e.fill, off, tint);
                    } else {
                        self.too_fine.remove(&o.id);
                        let ctx = batch.painter.ctx().clone();
                        match self.cache.pattern(o.id, s, doc, interacting, est_tiles, &ctx) {
                            Some(pe) => batch.add_chunked(&pe.mesh, off, visible),
                            None => {
                                // Still computing in the background: placeholder tint.
                                let e = self.cache.shape(o.id, s, tol);
                                batch.add_tinted(&e.fill, off, tint);
                            }
                        }
                    }
                }
            }
        }
    }

    fn paint_grid(&self, painter: &Painter, xf: Xf) {
        let rect = self.rect;
        let step = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0]
            .into_iter()
            .find(|s| s * xf.k >= 24.0)
            .unwrap_or(1000.0);
        let a = xf.w(rect.min);
        let b = xf.w(rect.max);
        let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(70, 110, 160, 28));
        let major = Stroke::new(1.0, Color32::from_rgba_unmultiplied(70, 110, 160, 50));
        let mut x = (a[0] / step).floor() * step;
        while x <= b[0] {
            let sx = xf.s([x, 0.0]).x;
            let st = if (x / (step * 5.0)).fract().abs() < 1e-9 { major } else { stroke };
            painter.vline(sx, rect.y_range(), st);
            x += step;
        }
        let mut y = (a[1] / step).floor() * step;
        while y <= b[1] {
            let sy = xf.s([0.0, y]).y;
            let st = if (y / (step * 5.0)).fract().abs() < 1e-9 { major } else { stroke };
            painter.hline(rect.x_range(), sy, st);
            y += step;
        }
    }

    fn outline_screen(&mut self, o: &Object, off: [f64; 2], tol: f64) -> Vec<Vec<Pos2>> {
        let xf = self.xf();
        match &o.kind {
            ObjectKind::Shape(s) => {
                let e = self.cache.shape(o.id, s, tol);
                e.geom
                    .0
                    .iter()
                    .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
                    .map(|r| ring_points(r).into_iter().map(|p| xf.s([p[0] + off[0], p[1] + off[1]])).collect())
                    .collect()
            }
            ObjectKind::Group(_) => Vec::new(),
        }
    }

    fn paint_selection(&mut self, painter: &Painter, io: &mut CanvasIo, target: Option<ObjId>) {
        let xf = self.xf();
        let doc: &Document = io.doc;
        let tol = doc.settings.tolerance();
        let sel = io.selection.clone();
        if sel.is_empty() {
            return;
        }
        // Outline each selected shape, box each selected group. (Selections are
        // top-level objects; one pass keeps Select All on many pieces linear.)
        for i in doc.top_indices(&sel) {
            let o = &doc.objects[i];
            let id = o.id;
            let off = self.move_offset(id);
            if o.is_group() {
                if let Some(b) = self.object_bounds(o, tol) {
                    let r = Rect::from_min_max(xf.s([b[0] + off[0], b[1] + off[1]]), xf.s([b[2] + off[0], b[3] + off[1]]));
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Middle);
                }
            } else {
                for ring in self.outline_screen(o, off, tol) {
                    add_outline(painter, &ring, Stroke::new(1.0, ACCENT));
                }
            }
        }
        if self.pattern_edit {
            if let Some(f) = target.and_then(|id| pattern_frame_of(doc, id)) {
                self.paint_pattern_handles(painter, &f);
            }
            return;
        }
        // Bounding box + handles.
        let handles = self.handles(doc, &sel);
        let off = if sel.len() == 1 { self.move_offset(sel[0]) } else { [0.0, 0.0] };
        let pos = |h: [i8; 2]| handles.iter().find(|(k, _)| *k == h).map(|(_, p)| xf.s([p[0] + off[0], p[1] + off[1]]));
        if let (Some(a), Some(b), Some(c), Some(d)) = (pos([-1, -1]), pos([1, -1]), pos([1, 1]), pos([-1, 1])) {
            painter.add(Shape::closed_line(vec![a, b, c, d], Stroke::new(1.0, ACCENT)));
        }
        if self.drag.is_none() || matches!(self.drag, Some(Drag::Resize { .. })) {
            for (_, p) in &handles {
                let r = Rect::from_center_size(xf.s([p[0] + off[0], p[1] + off[1]]), vec2(HANDLE * 2.0, HANDLE * 2.0));
                painter.rect_filled(r, 0.0, Color32::WHITE);
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Middle);
            }
        }
    }

    fn paint_pattern_handles(&self, painter: &Painter, f: &PatternFrame) {
        let xf = self.xf();
        let o = xf.s(f.origin);
        let (sh, rh) = self.pattern_handles(f);
        let (sp, rp) = (xf.s(sh), xf.s(rh));
        let st = Stroke::new(1.5, PATTERN_ACCENT);
        let halo = Stroke::new(3.5, Color32::from_white_alpha(200));
        for (a, b) in [(o, sp), (o, rp)] {
            painter.line_segment([a, b], halo);
            painter.line_segment([a, b], st);
        }
        // Origin crosshair.
        for d in [vec2(12.0, 0.0), vec2(0.0, 12.0)] {
            painter.line_segment([o - d, o + d], halo);
            painter.line_segment([o - d, o + d], st);
        }
        painter.circle_stroke(o, 5.0, st);
        // Scale handle (square) and rotation handle (circle).
        let r = Rect::from_center_size(sp, vec2(10.0, 10.0));
        painter.rect_filled(r, 0.0, Color32::WHITE);
        painter.rect_stroke(r, 0.0, st, StrokeKind::Middle);
        painter.circle_filled(rp, 5.5, Color32::WHITE);
        painter.circle_stroke(rp, 5.5, st);
        let galley = painter.layout_no_wrap(
            format!("{:.1} %  {:.1}°", f.scale * 100.0, f.rotation_deg),
            FontId::proportional(11.0),
            PATTERN_ACCENT,
        );
        let pos = o + vec2(12.0, 12.0);
        painter.rect_filled(
            Rect::from_min_size(pos, galley.size()).expand(3.0),
            3.0,
            Color32::from_white_alpha(225),
        );
        painter.galley(pos, galley, PATTERN_ACCENT);
    }

    fn paint_overlays(&self, painter: &Painter, _doc: &Document) {
        let xf = self.xf();
        match &self.drag {
            Some(Drag::Create { start, cur, .. }) => {
                let b = Self::create_box(*start, *cur, self.last_shift, self.last_alt);
                let r = Rect::from_min_max(xf.s([b[0], b[1]]), xf.s([b[2], b[3]]));
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Middle);
                painter.text(
                    r.right_bottom() + vec2(6.0, 4.0),
                    Align2::LEFT_TOP,
                    format!("{:.2} × {:.2} mm", b[2] - b[0], b[3] - b[1]),
                    FontId::proportional(11.0),
                    ACCENT,
                );
            }
            Some(Drag::Marquee { start, cur, .. }) => {
                let r = Rect::from_two_pos(xf.s(*start), xf.s(*cur));
                painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(30, 136, 229, 25));
                let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
                painter.extend(Shape::dashed_line(&pts, Stroke::new(1.0, ACCENT), 4.0, 3.0));
            }
            _ => {}
        }
        if !self.pen_points.is_empty() {
            let mut pts: Vec<Pos2> = self.pen_points.iter().map(|p| xf.s(*p)).collect();
            if let Some(c) = self.cursor_mm {
                pts.push(xf.s(c));
            }
            painter.add(Shape::line(pts.clone(), Stroke::new(1.5, ACCENT)));
            for (i, p) in self.pen_points.iter().enumerate() {
                let r = Rect::from_center_size(xf.s(*p), vec2(7.0, 7.0));
                painter.rect_filled(r, 0.0, if i == 0 { ACCENT } else { Color32::WHITE });
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Middle);
            }
        }
    }

    fn paint_rulers(&self, painter: &Painter) {
        let xf = self.xf();
        let rect = self.rect;
        let bg = Color32::from_gray(0xf2);
        let ink = Color32::from_gray(0x55);
        let top = Rect::from_min_size(rect.min, vec2(rect.width(), RULER));
        let left = Rect::from_min_size(rect.min, vec2(RULER, rect.height()));
        painter.rect_filled(top, 0.0, bg);
        painter.rect_filled(left, 0.0, bg);
        painter.hline(rect.x_range(), top.bottom(), Stroke::new(1.0, Color32::from_gray(0xa0)));
        painter.vline(left.right(), rect.y_range(), Stroke::new(1.0, Color32::from_gray(0xa0)));

        let steps = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0];
        let major = steps.into_iter().find(|s| s * xf.k >= 60.0).unwrap_or(5000.0);
        let minor = steps
            .into_iter()
            .find(|s| s * xf.k >= 6.0 && ((major / s).round() - major / s).abs() < 1e-9)
            .unwrap_or(major);
        let font = FontId::proportional(9.5);
        let label = |v: f64| {
            if major >= 1.0 { format!("{}", v.round() as i64) } else { format!("{v:.1}") }
        };
        let n_per_major = (major / minor).round().max(1.0) as i64;

        // Horizontal ruler.
        let a = xf.w(pos2(rect.left() + RULER, 0.0))[0];
        let b = xf.w(pos2(rect.right(), 0.0))[0];
        let mut i = (a / minor).floor() as i64;
        let last = (b / minor).ceil() as i64;
        while i <= last {
            let x = i as f64 * minor;
            let sx = xf.s([x, 0.0]).x;
            if sx >= rect.left() + RULER {
                let len = if i % n_per_major == 0 {
                    RULER * 0.6
                } else if n_per_major % 2 == 0 && i % (n_per_major / 2) == 0 {
                    RULER * 0.35
                } else {
                    RULER * 0.2
                };
                painter.vline(sx, (top.bottom() - len)..=top.bottom(), Stroke::new(1.0, ink));
                if i % n_per_major == 0 {
                    painter.text(pos2(sx + 2.0, top.top() + 1.0), Align2::LEFT_TOP, label(x), font.clone(), ink);
                }
            }
            i += 1;
        }
        // Vertical ruler (labels rotated).
        let a = xf.w(pos2(0.0, rect.top() + RULER))[1];
        let b = xf.w(pos2(0.0, rect.bottom()))[1];
        let mut i = (a / minor).floor() as i64;
        let last = (b / minor).ceil() as i64;
        while i <= last {
            let y = i as f64 * minor;
            let sy = xf.s([0.0, y]).y;
            if sy >= rect.top() + RULER {
                let len = if i % n_per_major == 0 {
                    RULER * 0.6
                } else if n_per_major % 2 == 0 && i % (n_per_major / 2) == 0 {
                    RULER * 0.35
                } else {
                    RULER * 0.2
                };
                painter.hline((left.right() - len)..=left.right(), sy, Stroke::new(1.0, ink));
                if i % n_per_major == 0 {
                    let galley = painter.layout_no_wrap(label(y), font.clone(), ink);
                    let pos = pos2(left.left() + 1.0, sy - 2.0);
                    painter.add(TextShape::new(pos, galley, ink).with_angle(-std::f32::consts::FRAC_PI_2));
                }
            }
            i += 1;
        }
        // Cursor markers.
        if let Some(c) = self.cursor_mm {
            let s = xf.s(c);
            let red = Stroke::new(1.0, Color32::from_rgb(0xe5, 0x39, 0x35));
            if s.x > left.right() {
                painter.vline(s.x, top.y_range(), red);
            }
            if s.y > top.bottom() {
                painter.hline(left.x_range(), s.y, red);
            }
        }
        let corner = Rect::from_min_size(rect.min, vec2(RULER, RULER));
        painter.rect_filled(corner, 0.0, Color32::from_gray(0xe2));
        painter.text(corner.center(), Align2::CENTER_CENTER, "mm", FontId::proportional(8.5), ink);
    }
}

/// Add a new shape of the given tool's kind inside `b`.
/// Watermark text drawn on the artboard (canvas only; never exported).
pub const WATERMARK: &str = "ACME ENGINEERING";

/// Draw the watermark in light grey along the artboard's rising diagonal,
/// sized to ~80 % of the diagonal so it scales with zoom and paper size.
fn paint_watermark(painter: &Painter, ab: Rect, view: Rect) {
    let clip = ab.intersect(view);
    if !clip.is_positive() || ab.width() < 40.0 {
        return;
    }
    let diag = ab.width().hypot(ab.height());
    // Measure at a reference size, then pick the font size that fits.
    const REF: f32 = 100.0;
    let ref_w = painter
        .layout_no_wrap(WATERMARK.into(), FontId::proportional(REF), Color32::WHITE)
        .size()
        .x
        .max(1.0);
    // Quantise to 1/8-octave steps and cap it, so zooming does not fill the
    // font atlas with one glyph set per pixel size.
    let wanted = (REF * diag * 0.8 / ref_w).clamp(6.0, 480.0);
    let size = 2f32.powf((wanted.log2() * 8.0).round() / 8.0);
    let color = Color32::from_gray(0xd6);
    let galley = painter.layout_no_wrap(WATERMARK.into(), FontId::proportional(size), color);
    let angle = -(ab.height() / ab.width()).atan();
    // TextShape rotates around its top-left corner: offset so the text is centred.
    let half = galley.size() * 0.5;
    let rot = egui::emath::Rot2::from_angle(angle);
    let pos = ab.center() - rot * half;
    painter
        .with_clip_rect(clip)
        .add(TextShape::new(pos, galley, color).with_angle(angle));
}

fn create_shape(tool: Tool, b: Bounds, io: &mut CanvasIo) {
    let (geom, base) = match tool {
        Tool::Rect => (ShapeGeom::Rect, "Rectangle"),
        Tool::Ellipse => (ShapeGeom::Ellipse, "Ellipse"),
        Tool::Polygon => (ShapeGeom::Polygon { sides: 6 }, "Polygon"),
        Tool::Star => (
            ShapeGeom::Star {
                points: 5,
                inner_ratio: 0.5,
            },
            "Star",
        ),
        Tool::Select | Tool::Pen | Tool::Hand => return,
    };
    let xf = Transform::new((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, b[2] - b[0], b[3] - b[1]);
    io.history.push(io.doc);
    let n = io.doc.next_id;
    let id = io.doc.add_shape(format!("{base} {n}"), ShapeObj::new(geom, xf, Style::default()));
    *io.selection = vec![id];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zooming (wheel, Ctrl+wheel, pinch) keeps the point under the cursor fixed;
    /// panning moves the view by exactly the given screen offset.
    #[test]
    fn zoom_at_cursor_and_pan() {
        let mut c = Canvas { rect: Rect::from_min_size(pos2(100.0, 50.0), vec2(800.0, 600.0)), ..Canvas::default() };
        let cursor = pos2(420.0, 333.0);
        let before = c.xf().w(cursor);
        c.zoom_at(cursor, 1.15f64.powi(3));
        let after = c.xf().w(cursor);
        assert!((before[0] - after[0]).abs() < 1e-3 && (before[1] - after[1]).abs() < 1e-3);
        c.zoom_at(cursor, 0.0); // ignored
        c.zoom_at(cursor, f64::NAN); // ignored
        assert!(c.zoom.is_finite() && c.zoom > 0.0);

        let p = c.xf().w(pos2(500.0, 300.0));
        c.pan_by(40.0, -20.0);
        let s = c.xf().s(p);
        assert!((s.x - 540.0).abs() < 1e-3 && (s.y - 280.0).abs() < 1e-3);
    }

    /// No polyline handed to egui may contain a near-reversal corner (those
    /// become miter spikes across the canvas), and no point may be lost.
    #[test]
    fn outlines_are_split_at_spikes() {
        let cos_at = |l: &[Pos2], i: usize| (l[i] - l[i - 1]).normalized().dot((l[i + 1] - l[i]).normalized());

        // A gentle square stays one closed line.
        let square = [pos2(0.0, 0.0), pos2(10.0, 0.0), pos2(10.0, 10.0), pos2(0.0, 10.0)];
        assert!(matches!(outline_parts(&square), OutlineParts::Closed(p) if p.len() == 4));

        // Text-like ring: a hairpin (going out and straight back) plus
        // sub-pixel jitter, as curve-converted glyphs have at small zoom.
        let ring = [
            pos2(0.0, 0.0),
            pos2(20.0, 0.0),
            pos2(20.1, 0.05), // jitter < 0.3 px: merged
            pos2(40.0, 0.2),
            pos2(20.0, 0.4), // hairpin: turns back ~180°
            pos2(20.0, 10.0),
            pos2(0.0, 10.0),
        ];
        let check = |ring: &[Pos2], expect_lines: usize, expect_pts: usize| {
            let OutlineParts::Open(lines) = outline_parts(ring) else { panic!("expected a split") };
            // One hairpin: a single open line that starts and ends there (no join).
            assert_eq!(lines.len(), expect_lines);
            for l in &lines {
                for i in 1..l.len() - 1 {
                    assert!(cos_at(l, i) >= SPIKE_COS, "sharp corner left inside a polyline: {l:?}");
                }
            }
            // Every kept point is still drawn (each starts one segment).
            let drawn: usize = lines.iter().map(|l| l.len() - 1).sum();
            assert_eq!(drawn, expect_pts);
        };
        check(&ring, 1, 6); // jitter merged: 6 points
        // A second hairpin on the left edge: split into two lines.
        let mut two = ring.to_vec();
        two.extend([pos2(0.0, 6.0), pos2(-15.0, 5.9), pos2(0.0, 5.8)]);
        check(&two, 2, 9);
        // Degenerate input draws nothing.
        assert!(matches!(outline_parts(&[pos2(1.0, 1.0), pos2(1.1, 1.0)]), OutlineParts::Open(l) if l.is_empty()));
    }
}
