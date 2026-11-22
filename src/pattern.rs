//! Pattern definition, pattern placement (frame) and tile generation.
//!
//! Pure geometry, no egui. All lengths are millimetres.
//!
//! A pattern lives in its own "pattern space": cell (0, 0) is centred on the
//! origin and cells are laid out on the lattice of the chosen [`Layout`].
//! A [`PatternFrame`] maps pattern space to the world:
//! `world = origin + R(rotation) * (scale * p)`.

use crate::shapes::{
    Bounds, Color, Rings, Transform, circle_points, clamp_finite, finite_or, normalize_deg,
    rotate_vec, segments_for_radius, star_unit,
};
use geo::{LineString, Polygon};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Hard cap on generated tiles per shape.
pub const MAX_TILES: usize = 200_000;
pub const MIN_CELL: f64 = 0.05;
pub const MAX_CELL: f64 = 10_000.0;
const SQRT3: f64 = 1.732_050_807_568_877_2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    Square,
    Honeycomb,
    Brick,
    Triangle,
    Diamond,
    Circle,
}

impl Layout {
    pub const ALL: [Layout; 6] = [
        Layout::Square,
        Layout::Honeycomb,
        Layout::Brick,
        Layout::Triangle,
        Layout::Diamond,
        Layout::Circle,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Layout::Square => "Square grid",
            Layout::Honeycomb => "Honeycomb",
            Layout::Brick => "Brick / offset grid",
            Layout::Triangle => "Triangle grid",
            Layout::Diamond => "Diamond grid",
            Layout::Circle => "Circle grid",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HexOrientation {
    FlatTop,
    PointyTop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircleSpacing {
    Square,
    Hex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileShape {
    /// The natural tile of the layout (square, hexagon, brick, triangle, diamond, circle).
    Auto,
    Square,
    Hexagon,
    Triangle,
    Circle,
    Star,
    /// The polygons stored in [`Pattern::custom_tile`].
    Custom,
}

impl TileShape {
    pub const ALL: [TileShape; 7] = [
        TileShape::Auto,
        TileShape::Square,
        TileShape::Hexagon,
        TileShape::Triangle,
        TileShape::Circle,
        TileShape::Star,
        TileShape::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TileShape::Auto => "Follow layout",
            TileShape::Square => "Square",
            TileShape::Hexagon => "Hexagon",
            TileShape::Triangle => "Triangle",
            TileShape::Circle => "Circle",
            TileShape::Star => "Star",
            TileShape::Custom => "Custom shape",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    Single,
    Alternating,
    Random,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pattern {
    pub name: String,
    pub layout: Layout,
    pub hex_orientation: HexOrientation,
    /// Horizontal shift of each brick row, as a fraction of the brick width.
    pub brick_offset: f64,
    /// Brick width / height.
    pub brick_aspect: f64,
    pub circle_spacing: CircleSpacing,
    pub tile_shape: TileShape,
    /// Normalised custom tile (centred, larger side = 1).
    pub custom_tile: Vec<Rings>,
    pub star_points: u32,
    pub star_inner_ratio: f64,
    /// Lattice spacing in mm (centre-to-centre distance of neighbouring cells).
    pub cell_size: f64,
    /// Gap between neighbouring tiles in mm.
    pub gap: f64,
    /// Extra per-tile scale, percent.
    pub tile_scale_pct: f64,
    pub color_mode: ColorMode,
    pub color_a: Color,
    pub color_b: Color,
    pub palette: Vec<Color>,
    pub seed: u64,
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            name: "Honeycomb".into(),
            layout: Layout::Honeycomb,
            hex_orientation: HexOrientation::FlatTop,
            brick_offset: 0.5,
            brick_aspect: 2.0,
            circle_spacing: CircleSpacing::Hex,
            tile_shape: TileShape::Auto,
            custom_tile: Vec::new(),
            star_points: 5,
            star_inner_ratio: 0.5,
            cell_size: 10.0,
            gap: 0.5,
            tile_scale_pct: 100.0,
            color_mode: ColorMode::Alternating,
            color_a: Color::rgb(0xf2, 0xa9, 0x00),
            color_b: Color::rgb(0x2b, 0x59, 0xc3),
            palette: default_palette(),
            seed: 1,
        }
    }
}

pub fn default_palette() -> Vec<Color> {
    vec![
        Color::rgb(0xe6, 0x39, 0x46),
        Color::rgb(0xf1, 0xfa, 0xee),
        Color::rgb(0xa8, 0xda, 0xdc),
        Color::rgb(0x45, 0x7b, 0x9d),
        Color::rgb(0x1d, 0x35, 0x57),
    ]
}

impl Pattern {
    /// A copy with every parameter clamped to a safe range.
    pub fn sanitized(&self) -> Pattern {
        let mut p = self.clone();
        p.cell_size = clamp_finite(p.cell_size, MIN_CELL, MAX_CELL, 10.0);
        p.gap = clamp_finite(p.gap, 0.0, p.cell_size * 0.95, 0.0);
        p.tile_scale_pct = clamp_finite(p.tile_scale_pct, 1.0, 400.0, 100.0);
        p.brick_offset = clamp_finite(p.brick_offset, 0.0, 1.0, 0.5);
        p.brick_aspect = clamp_finite(p.brick_aspect, 0.1, 10.0, 2.0);
        p.star_points = p.star_points.clamp(3, 100);
        p.star_inner_ratio = clamp_finite(p.star_inner_ratio, 0.05, 1.0, 0.5);
        if p.palette.is_empty() {
            p.palette = vec![p.color_a, p.color_b];
        }
        if p.tile_shape == TileShape::Custom && p.custom_tile.iter().all(|r| r.exterior.len() < 3) {
            p.tile_shape = TileShape::Auto;
        }
        p
    }

    fn color_for(&self, i: i64, j: i64, sub: u8, parity: i64) -> Color {
        match self.color_mode {
            ColorMode::Single => self.color_a,
            ColorMode::Alternating => {
                if parity.rem_euclid(2) == 0 {
                    self.color_a
                } else {
                    self.color_b
                }
            }
            ColorMode::Random => {
                if self.palette.is_empty() {
                    return self.color_a;
                }
                let h = hash_cell(self.seed, i, j, sub);
                self.palette[(h % self.palette.len() as u64) as usize]
            }
        }
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic per-cell hash (reproducible colours from a seed).
pub fn hash_cell(seed: u64, i: i64, j: i64, sub: u8) -> u64 {
    let a = splitmix64(seed ^ (i as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let b = splitmix64(a ^ (j as u64).wrapping_mul(0xA076_1D64_78BD_642F));
    splitmix64(b ^ u64::from(sub))
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Where the pattern sits in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PatternFrame {
    pub origin: [f64; 2],
    pub rotation_deg: f64,
    /// 1.0 = 100 %.
    pub scale: f64,
}

pub const MIN_PATTERN_SCALE: f64 = 0.01;
pub const MAX_PATTERN_SCALE: f64 = 100.0;

impl PatternFrame {
    pub fn sanitized(&self) -> Self {
        Self {
            origin: [
                clamp_finite(self.origin[0], -1.0e6, 1.0e6, 0.0),
                clamp_finite(self.origin[1], -1.0e6, 1.0e6, 0.0),
            ],
            rotation_deg: normalize_deg(finite_or(self.rotation_deg, 0.0)),
            scale: clamp_finite(self.scale, MIN_PATTERN_SCALE, MAX_PATTERN_SCALE, 1.0),
        }
    }

    pub fn to_world(&self, p: [f64; 2]) -> [f64; 2] {
        let r = rotate_vec([p[0] * self.scale, p[1] * self.scale], self.rotation_deg);
        [self.origin[0] + r[0], self.origin[1] + r[1]]
    }

    pub fn to_pattern(&self, p: [f64; 2]) -> [f64; 2] {
        let r = rotate_vec([p[0] - self.origin[0], p[1] - self.origin[1]], -self.rotation_deg);
        [r[0] / self.scale, r[1] / self.scale]
    }
}

/// The user-editable pattern transform (shown in the Pattern panel).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatternTransform {
    pub offset_x: f64,
    pub offset_y: f64,
    pub scale_pct: f64,
    pub rotation_deg: f64,
}

impl Default for PatternTransform {
    fn default() -> Self {
        Self {
            offset_x: 0.0,
            offset_y: 0.0,
            scale_pct: 100.0,
            rotation_deg: 0.0,
        }
    }
}

/// A pattern assigned to a shape (the live "pattern fill" state).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatternFill {
    pub pattern: Pattern,
    pub transform: PatternTransform,
    /// ON: offset is relative to the shape centre (in the shape's rotated axes),
    /// the pattern rotates with the shape and scales with its size.
    /// OFF: offset is an absolute world position; the shape moves over the pattern.
    pub transform_with_object: bool,
    /// Shape size when the pattern was attached (reference for scaling with the object).
    pub ref_w: f64,
    pub ref_h: f64,
    /// Keep the original shape (fill + stroke) underneath the pieces.
    #[serde(default)]
    pub keep_background: bool,
    /// The pattern also covers the shape's outline (stroke): the pattern area
    /// is the shape grown by half the stroke width, cut as one region so tiles
    /// run seamlessly from the interior into the outline.
    #[serde(default = "default_true")]
    pub include_outline: bool,
}

fn default_true() -> bool {
    true
}

impl PatternFill {
    pub fn new(pattern: Pattern, xf: &Transform) -> Self {
        let xf = xf.sanitized();
        Self {
            pattern,
            transform: PatternTransform::default(),
            transform_with_object: true,
            ref_w: xf.w,
            ref_h: xf.h,
            keep_background: false,
            include_outline: true,
        }
    }

    fn object_scale(&self, xf: &Transform) -> (f64, f64) {
        let sx = if self.ref_w > 1.0e-9 { xf.w / self.ref_w } else { 1.0 };
        let sy = if self.ref_h > 1.0e-9 { xf.h / self.ref_h } else { 1.0 };
        (
            clamp_finite(sx, 1.0e-6, 1.0e6, 1.0),
            clamp_finite(sy, 1.0e-6, 1.0e6, 1.0),
        )
    }

    /// The effective world placement for a shape with transform `xf`.
    pub fn world_frame(&self, xf: &Transform) -> PatternFrame {
        let xf = xf.sanitized();
        let t = &self.transform;
        let scale = finite_or(t.scale_pct, 100.0) / 100.0;
        let frame = if self.transform_with_object {
            let (sx, sy) = self.object_scale(&xf);
            let local = [finite_or(t.offset_x, 0.0) * sx, finite_or(t.offset_y, 0.0) * sy];
            PatternFrame {
                origin: xf.local_to_world(local),
                rotation_deg: xf.rotation_deg + finite_or(t.rotation_deg, 0.0),
                scale: scale * (sx * sy).sqrt(),
            }
        } else {
            PatternFrame {
                origin: [finite_or(t.offset_x, 0.0), finite_or(t.offset_y, 0.0)],
                rotation_deg: finite_or(t.rotation_deg, 0.0),
                scale,
            }
        };
        frame.sanitized()
    }

    /// Store `frame` (a world placement) back into the editable transform.
    pub fn set_world_frame(&mut self, xf: &Transform, frame: PatternFrame) {
        let xf = xf.sanitized();
        let frame = frame.sanitized();
        if self.transform_with_object {
            let (sx, sy) = self.object_scale(&xf);
            let local = xf.world_to_local(frame.origin);
            self.transform = PatternTransform {
                offset_x: local[0] / sx,
                offset_y: local[1] / sy,
                scale_pct: frame.scale / (sx * sy).sqrt() * 100.0,
                rotation_deg: normalize_deg(frame.rotation_deg - xf.rotation_deg),
            };
        } else {
            self.transform = PatternTransform {
                offset_x: frame.origin[0],
                offset_y: frame.origin[1],
                scale_pct: frame.scale * 100.0,
                rotation_deg: frame.rotation_deg,
            };
        }
    }

    /// Switch "Transform pattern with object" without moving the pattern.
    pub fn set_transform_with_object(&mut self, on: bool, xf: &Transform) {
        if on == self.transform_with_object {
            return;
        }
        let frame = self.world_frame(xf);
        self.transform_with_object = on;
        if on {
            let xf = xf.sanitized();
            self.ref_w = xf.w;
            self.ref_h = xf.h;
        }
        self.set_world_frame(xf, frame);
    }

    /// Pattern origin back at the shape centre, 100 %, 0°.
    pub fn reset_transform(&mut self, xf: &Transform) {
        let xf = xf.sanitized();
        if self.transform_with_object {
            self.ref_w = xf.w;
            self.ref_h = xf.h;
            self.transform = PatternTransform::default();
        } else {
            self.transform = PatternTransform {
                offset_x: xf.cx,
                offset_y: xf.cy,
                ..PatternTransform::default()
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Tile generation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Tile {
    /// World-space polygon.
    pub poly: Polygon<f64>,
    pub color: Color,
    /// Lattice cell indices.
    pub cell: [i64; 2],
    /// Index of the polygon inside the cell (custom tiles may have several).
    pub sub: u8,
    /// World-space centre of the cell.
    pub center: [f64; 2],
}

#[derive(Clone, Debug, Default)]
pub struct TileSet {
    pub tiles: Vec<Tile>,
    /// True if the tile count hit `max_tiles` and generation stopped early.
    pub capped: bool,
    /// Estimated number of cells covering the region.
    pub estimated: f64,
}

struct Gen<'a> {
    p: &'a Pattern,
    frame: PatternFrame,
    /// Flattening tolerance in pattern space.
    tol: f64,
    max: usize,
    out: TileSet,
}

impl Gen<'_> {
    fn full(&self) -> bool {
        self.out.tiles.len() >= self.max
    }

    /// Emit one cell: `auto` is the layout's natural tile (pattern space),
    /// `fit` the inscribed-circle diameter used for override shapes.
    fn emit(&mut self, cell: [i64; 2], center: [f64; 2], auto: Vec<[f64; 2]>, fit: f64, up: bool, parity: i64) {
        if self.full() {
            self.out.capped = true;
            return;
        }
        if fit <= 0.0 || !fit.is_finite() {
            return;
        }
        let shapes: Vec<Rings> = match self.p.tile_shape {
            TileShape::Auto => vec![Rings {
                exterior: auto,
                holes: vec![],
            }],
            other => override_shape(self.p, other, fit, up, self.tol)
                .into_iter()
                .map(|r| r.map(|q| [q[0] + center[0], q[1] + center[1]]))
                .collect(),
        };
        let world_center = self.frame.to_world(center);
        for (sub, rings) in shapes.iter().enumerate() {
            if rings.exterior.len() < 3 {
                continue;
            }
            let sub = sub.min(255) as u8;
            let w = rings.map(|q| self.frame.to_world(q));
            let poly = Polygon::new(
                LineString::from(w.exterior),
                w.holes.into_iter().map(LineString::from).collect(),
            );
            self.out.tiles.push(Tile {
                poly,
                color: self.p.color_for(cell[0], cell[1], sub, parity),
                cell,
                sub,
                center: world_center,
            });
        }
    }
}

/// Inclusive integer range covering `[lo, hi]`, clamped to avoid overflow.
fn irange(lo: f64, hi: f64) -> (i64, i64) {
    let lo = finite_or(lo, 0.0).floor().clamp(-1.0e9, 1.0e9) as i64;
    let hi = finite_or(hi, 0.0).ceil().clamp(-1.0e9, 1.0e9) as i64;
    (lo, hi.max(lo))
}

fn count(r: (i64, i64)) -> f64 {
    (r.1 - r.0 + 1) as f64
}

fn rect_pts(c: [f64; 2], w: f64, h: f64) -> Vec<[f64; 2]> {
    let (a, b) = (w / 2.0, h / 2.0);
    vec![
        [c[0] - a, c[1] - b],
        [c[0] + a, c[1] - b],
        [c[0] + a, c[1] + b],
        [c[0] - a, c[1] + b],
    ]
}

/// Hexagon with across-flats distance `d`.
fn hex_pts(c: [f64; 2], d: f64, orient: HexOrientation) -> Vec<[f64; 2]> {
    let r = d / SQRT3;
    let start = match orient {
        HexOrientation::FlatTop => 0.0,
        HexOrientation::PointyTop => 30.0_f64.to_radians(),
    };
    (0..6)
        .map(|k| {
            let a = start + PI / 3.0 * f64::from(k);
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

/// Equilateral triangle with inradius `r`, centroid at origin; apex up (negative y) if `up`.
fn triangle_pts(r: f64, up: bool) -> Vec<[f64; 2]> {
    let s = if up { -1.0 } else { 1.0 };
    // Circumradius = 2r.
    let a = r * SQRT3; // half side
    vec![[0.0, s * 2.0 * r], [a, -s * r], [-a, -s * r]]
}

/// Override shape centred on the origin, inscribed-circle diameter `d`.
fn override_shape(p: &Pattern, shape: TileShape, d: f64, up: bool, tol: f64) -> Vec<Rings> {
    let ring = |v: Vec<[f64; 2]>| {
        vec![Rings {
            exterior: v,
            holes: vec![],
        }]
    };
    match shape {
        TileShape::Auto | TileShape::Square => ring(rect_pts([0.0, 0.0], d, d)),
        TileShape::Hexagon => ring(hex_pts([0.0, 0.0], d, p.hex_orientation)),
        TileShape::Triangle => ring(triangle_pts(d / 2.0, up)),
        TileShape::Circle => ring(circle_points(d / 2.0, segments_for_radius(d / 2.0, tol))),
        TileShape::Star => ring(
            star_unit(p.star_points, p.star_inner_ratio)
                .into_iter()
                .map(|q| [q[0] * d, q[1] * d])
                .collect(),
        ),
        TileShape::Custom => p
            .custom_tile
            .iter()
            .map(|r| r.map(|q| [q[0] * d, q[1] * d]))
            .collect(),
    }
}

/// Generate every tile whose cell may touch the world-space `region`.
///
/// `tol` is the flattening tolerance in world mm; at most `max_tiles` tiles are produced.
pub fn generate_tiles(
    pattern: &Pattern,
    frame: &PatternFrame,
    region: Bounds,
    tol: f64,
    max_tiles: usize,
) -> TileSet {
    let p = pattern.sanitized();
    let frame = frame.sanitized();
    if region.iter().any(|v| !v.is_finite()) || region[2] < region[0] || region[3] < region[1] {
        return TileSet::default();
    }
    // Region in pattern space.
    let corners = [
        [region[0], region[1]],
        [region[2], region[1]],
        [region[2], region[3]],
        [region[0], region[3]],
    ]
    .map(|c| frame.to_pattern(c));
    let x0 = corners.iter().map(|c| c[0]).fold(f64::INFINITY, f64::min);
    let x1 = corners.iter().map(|c| c[0]).fold(f64::NEG_INFINITY, f64::max);
    let y0 = corners.iter().map(|c| c[1]).fold(f64::INFINITY, f64::min);
    let y1 = corners.iter().map(|c| c[1]).fold(f64::NEG_INFINITY, f64::max);

    let s = p.cell_size;
    let g = p.gap;
    let k = p.tile_scale_pct / 100.0;
    let t = (s - g) * k;
    let mut tg = Gen {
        p: &p,
        frame,
        tol: clamp_finite(tol, 1.0e-6, 1.0e3, 0.01) / frame.scale,
        max: max_tiles,
        out: TileSet::default(),
    };

    match p.layout {
        Layout::Square | Layout::Circle => {
            let hex = p.layout == Layout::Circle && p.circle_spacing == CircleSpacing::Hex;
            let dy = if hex { s * SQRT3 / 2.0 } else { s };
            let jr = irange(y0 / dy - 1.0, y1 / dy + 1.0);
            let ir = irange(x0 / s - 2.0, x1 / s + 2.0);
            tg.out.estimated = count(ir) * count(jr);
            let circle = p.layout == Layout::Circle;
            let n = segments_for_radius(t / 2.0, tg.tol);
            'rows: for j in jr.0..=jr.1 {
                let shift = if hex && j.rem_euclid(2) == 1 { s / 2.0 } else { 0.0 };
                for i in ir.0..=ir.1 {
                    let c = [i as f64 * s + shift, j as f64 * dy];
                    let auto = if circle {
                        circle_points(t / 2.0, n)
                            .into_iter()
                            .map(|q| [q[0] + c[0], q[1] + c[1]])
                            .collect()
                    } else {
                        rect_pts(c, t, t)
                    };
                    tg.emit([i, j], c, auto, t, true, i + j);
                    if tg.full() {
                        tg.out.capped = true;
                        break 'rows;
                    }
                }
            }
        }
        Layout::Honeycomb => {
            let along = s * SQRT3 / 2.0; // spacing of staggered rows/columns
            match p.hex_orientation {
                HexOrientation::FlatTop => {
                    // Columns spaced `along`, odd columns shifted down by s/2.
                    let jr = irange(y0 / s - 2.0, y1 / s + 1.0);
                    let ir = irange(x0 / along - 1.0, x1 / along + 1.0);
                    tg.out.estimated = count(ir) * count(jr);
                    'rows: for j in jr.0..=jr.1 {
                        for i in ir.0..=ir.1 {
                            let shift = if i.rem_euclid(2) == 1 { s / 2.0 } else { 0.0 };
                            let c = [i as f64 * along, j as f64 * s + shift];
                            tg.emit([i, j], c, hex_pts(c, t, HexOrientation::FlatTop), t, true, i + j);
                            if tg.full() {
                                tg.out.capped = true;
                                break 'rows;
                            }
                        }
                    }
                }
                HexOrientation::PointyTop => {
                    let jr = irange(y0 / along - 1.0, y1 / along + 1.0);
                    let ir = irange(x0 / s - 2.0, x1 / s + 1.0);
                    tg.out.estimated = count(ir) * count(jr);
                    'rows: for j in jr.0..=jr.1 {
                        let shift = if j.rem_euclid(2) == 1 { s / 2.0 } else { 0.0 };
                        for i in ir.0..=ir.1 {
                            let c = [i as f64 * s + shift, j as f64 * along];
                            tg.emit([i, j], c, hex_pts(c, t, HexOrientation::PointyTop), t, true, i + j);
                            if tg.full() {
                                tg.out.capped = true;
                                break 'rows;
                            }
                        }
                    }
                }
            }
        }
        Layout::Brick => {
            let w = s;
            let h = s / p.brick_aspect;
            let bw = (w - g) * k;
            let bh = (h - g).max(h * 0.05) * k;
            let jr = irange(y0 / h - 1.0, y1 / h + 1.0);
            let ir0 = irange(x0 / w - 2.0, x1 / w + 2.0);
            tg.out.estimated = count(ir0) * count(jr);
            'rows: for j in jr.0..=jr.1 {
                let shift = (j as f64 * p.brick_offset).rem_euclid(1.0) * w;
                let ir = irange((x0 - shift) / w - 1.0, (x1 - shift) / w + 1.0);
                for i in ir.0..=ir.1 {
                    let c = [i as f64 * w + shift, j as f64 * h];
                    tg.emit([i, j], c, rect_pts(c, bw, bh), bw.min(bh), true, i + j);
                    if tg.full() {
                        tg.out.capped = true;
                        break 'rows;
                    }
                }
            }
        }
        Layout::Triangle => {
            let a = s;
            let h = a * SQRT3 / 2.0;
            let r = a / (2.0 * SQRT3); // inradius of a cell
            let r_eff = ((r - g / 2.0) * k).max(0.0);
            let jr = irange(y0 / h - 1.0, y1 / h + 1.0);
            let kr = irange(x0 / (a / 2.0) - 2.0, x1 / (a / 2.0) + 2.0);
            tg.out.estimated = count(kr) * count(jr);
            'rows: for j in jr.0..=jr.1 {
                for kk in kr.0..=kr.1 {
                    let up = (kk + j).rem_euclid(2) == 0;
                    // Centroid: apex-up triangles have their centroid 2/3 down the row.
                    let cy = j as f64 * h + if up { 2.0 * h / 3.0 } else { h / 3.0 };
                    let c = [kk as f64 * a / 2.0, cy];
                    let auto = triangle_pts(r_eff, up)
                        .into_iter()
                        .map(|q| [q[0] + c[0], q[1] + c[1]])
                        .collect();
                    tg.emit([kk, j], c, auto, 2.0 * r_eff, up, i64::from(!up));
                    if tg.full() {
                        tg.out.capped = true;
                        break 'rows;
                    }
                }
            }
        }
        Layout::Diamond => {
            // Square grid rotated by 45°: centre = ((i - j) q, (i + j) q).
            let q = s / std::f64::consts::SQRT_2;
            let uv = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]].map(|c| {
                let (u, v) = (c[0] / q, c[1] / q);
                [(u + v) / 2.0, (v - u) / 2.0]
            });
            let ir = irange(
                uv.iter().map(|c| c[0]).fold(f64::INFINITY, f64::min) - 1.0,
                uv.iter().map(|c| c[0]).fold(f64::NEG_INFINITY, f64::max) + 1.0,
            );
            let jr = irange(
                uv.iter().map(|c| c[1]).fold(f64::INFINITY, f64::min) - 1.0,
                uv.iter().map(|c| c[1]).fold(f64::NEG_INFINITY, f64::max) + 1.0,
            );
            tg.out.estimated = count(ir) * count(jr) / 2.0;
            let half = t / std::f64::consts::SQRT_2;
            // Iterate by rows of constant (i + j) so z-order runs top to bottom.
            let (sr0, sr1) = (ir.0 + jr.0, ir.1 + jr.1);
            'rows: for sum in sr0..=sr1 {
                let cy = sum as f64 * q;
                if cy < y0 - s || cy > y1 + s {
                    continue;
                }
                let i_lo = ir.0.max(sum - jr.1);
                let i_hi = ir.1.min(sum - jr.0);
                for i in i_lo..=i_hi {
                    let j = sum - i;
                    let cx = (i - j) as f64 * q;
                    if cx < x0 - s || cx > x1 + s {
                        continue;
                    }
                    let c = [cx, cy];
                    let auto = vec![
                        [c[0], c[1] - half],
                        [c[0] + half, c[1]],
                        [c[0], c[1] + half],
                        [c[0] - half, c[1]],
                    ];
                    tg.emit([i, j], c, auto, t, true, i + j);
                    if tg.full() {
                        tg.out.capped = true;
                        break 'rows;
                    }
                }
            }
        }
    }
    tg.out
}

/// The cell spacing in world mm (used to enlarge regions by one cell).
pub fn world_cell_size(pattern: &Pattern, frame: &PatternFrame) -> f64 {
    let p = pattern.sanitized();
    let f = frame.sanitized();
    let k = (p.tile_scale_pct / 100.0).max(1.0);
    // Override triangles are larger than their cell (inscribed circle = cell).
    let oversize = if p.tile_shape == TileShape::Triangle { 2.0 } else { 1.0 };
    p.cell_size * f.scale * k * oversize
}

/// Built-in presets shown at the top of the preset list.
pub fn builtin_presets() -> Vec<Pattern> {
    let base = Pattern::default();
    vec![
        Pattern {
            name: "Honeycomb".into(),
            ..base.clone()
        },
        Pattern {
            name: "Honeycomb (pointy)".into(),
            hex_orientation: HexOrientation::PointyTop,
            color_mode: ColorMode::Single,
            color_a: Color::rgb(0x2a, 0x9d, 0x8f),
            ..base.clone()
        },
        Pattern {
            name: "Checkerboard".into(),
            layout: Layout::Square,
            gap: 0.0,
            cell_size: 8.0,
            color_a: Color::rgb(0x22, 0x22, 0x22),
            color_b: Color::rgb(0xee, 0xee, 0xee),
            ..base.clone()
        },
        Pattern {
            name: "Bricks".into(),
            layout: Layout::Brick,
            cell_size: 20.0,
            gap: 1.0,
            color_mode: ColorMode::Random,
            palette: vec![
                Color::rgb(0xb5, 0x4a, 0x2c),
                Color::rgb(0x9c, 0x3d, 0x22),
                Color::rgb(0xc8, 0x63, 0x3f),
            ],
            ..base.clone()
        },
        Pattern {
            name: "Triangles".into(),
            layout: Layout::Triangle,
            cell_size: 12.0,
            gap: 0.4,
            color_a: Color::rgb(0x26, 0x46, 0x53),
            color_b: Color::rgb(0xe9, 0xc4, 0x6a),
            ..base.clone()
        },
        Pattern {
            name: "Diamonds".into(),
            layout: Layout::Diamond,
            cell_size: 8.0,
            gap: 0.6,
            color_a: Color::rgb(0x6d, 0x59, 0x7a),
            color_b: Color::rgb(0xe5, 0x6b, 0x6f),
            ..base.clone()
        },
        Pattern {
            name: "Polka dots".into(),
            layout: Layout::Circle,
            circle_spacing: CircleSpacing::Hex,
            cell_size: 8.0,
            gap: 3.0,
            color_mode: ColorMode::Single,
            color_a: Color::rgb(0xd6, 0x28, 0x28),
            ..base.clone()
        },
        Pattern {
            name: "Confetti".into(),
            layout: Layout::Square,
            cell_size: 6.0,
            gap: 1.0,
            color_mode: ColorMode::Random,
            seed: 42,
            ..base
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Area, BoundingRect};

    fn frame() -> PatternFrame {
        PatternFrame {
            origin: [0.0, 0.0],
            rotation_deg: 0.0,
            scale: 1.0,
        }
    }

    fn pattern(layout: Layout) -> Pattern {
        Pattern {
            layout,
            gap: 0.0,
            cell_size: 10.0,
            ..Pattern::default()
        }
    }

    /// Every layout covers the region, all tiles are non-degenerate, and for
    /// space-filling layouts the tile area per cell matches the lattice.
    #[test]
    fn every_layout_generates_tiles() {
        let region = [0.0, 0.0, 100.0, 100.0];
        for layout in Layout::ALL {
            let mut p = pattern(layout);
            let ts = generate_tiles(&p, &frame(), region, 0.01, MAX_TILES);
            assert!(!ts.capped, "{layout:?} capped");
            assert!(ts.tiles.len() > 50, "{layout:?}: {}", ts.tiles.len());
            for t in &ts.tiles {
                assert!(t.poly.unsigned_area() > 0.0, "{layout:?} empty tile");
            }
            // Every point of the region must be covered by some tile's bbox centre neighbourhood:
            // check the region corners lie inside the union of tile bounds.
            let union = ts
                .tiles
                .iter()
                .filter_map(|t| t.poly.bounding_rect())
                .fold(None, |acc: Option<[f64; 4]>, r| {
                    let b = [r.min().x, r.min().y, r.max().x, r.max().y];
                    crate::shapes::bounds_union(acc, Some(b))
                })
                .unwrap();
            assert!(union[0] <= 0.0 && union[1] <= 0.0 && union[2] >= 100.0 && union[3] >= 100.0, "{layout:?}");
            // Hex orientation variant.
            if layout == Layout::Honeycomb {
                p.hex_orientation = HexOrientation::PointyTop;
                assert!(generate_tiles(&p, &frame(), region, 0.01, MAX_TILES).tiles.len() > 50);
            }
        }
    }

    #[test]
    fn tile_areas_match_lattice() {
        let s: f64 = 10.0;
        let cases = [
            (Layout::Square, s * s),
            (Layout::Honeycomb, s * s * SQRT3 / 2.0),
            (Layout::Brick, s * s / 2.0),
            (Layout::Triangle, s * s * SQRT3 / 4.0),
            (Layout::Diamond, s * s),
        ];
        for (layout, expected) in cases {
            let ts = generate_tiles(&pattern(layout), &frame(), [0.0, 0.0, 30.0, 30.0], 0.01, MAX_TILES);
            let a = ts.tiles[0].poly.unsigned_area();
            assert!((a - expected).abs() < 1.0e-9, "{layout:?}: {a} vs {expected}");
        }
    }

    #[test]
    fn origin_cell_is_centred_on_frame_origin() {
        let f = PatternFrame {
            origin: [13.3, -7.1],
            rotation_deg: 30.0,
            scale: 1.5,
        };
        let ts = generate_tiles(&pattern(Layout::Square), &f, [-20.0, -30.0, 40.0, 20.0], 0.01, MAX_TILES);
        let t = ts.tiles.iter().find(|t| t.cell == [0, 0]).unwrap();
        assert!((t.center[0] - 13.3).abs() < 1e-9 && (t.center[1] + 7.1).abs() < 1e-9);
        let a = t.poly.unsigned_area();
        assert!((a - 100.0 * 1.5 * 1.5).abs() < 1e-6);
    }

    #[test]
    fn bad_values_are_clamped_and_capped() {
        let p = Pattern {
            cell_size: 0.0,
            gap: -5.0,
            tile_scale_pct: f64::NAN,
            layout: Layout::Square,
            ..Pattern::default()
        };
        let ts = generate_tiles(&p, &frame(), [0.0, 0.0, 1000.0, 1000.0], 0.01, 5_000);
        assert!(ts.capped);
        assert_eq!(ts.tiles.len(), 5_000);
        // Inverted / NaN regions produce nothing instead of panicking.
        assert!(generate_tiles(&p, &frame(), [10.0, 0.0, 0.0, 1.0], 0.01, 10).tiles.is_empty());
        assert!(generate_tiles(&p, &frame(), [f64::NAN, 0.0, 1.0, 1.0], 0.01, 10).tiles.is_empty());
    }

    #[test]
    fn random_colors_are_reproducible() {
        let p = Pattern {
            color_mode: ColorMode::Random,
            seed: 7,
            ..pattern(Layout::Square)
        };
        let a = generate_tiles(&p, &frame(), [0.0, 0.0, 50.0, 50.0], 0.01, MAX_TILES);
        let b = generate_tiles(&p, &frame(), [0.0, 0.0, 50.0, 50.0], 0.01, MAX_TILES);
        assert!(a.tiles.iter().zip(&b.tiles).all(|(x, y)| x.color == y.color));
        let distinct: std::collections::HashSet<_> = a.tiles.iter().map(|t| t.color).collect();
        assert!(distinct.len() > 1);
    }

    #[test]
    fn world_frame_round_trips() {
        let xf = Transform {
            cx: 50.0,
            cy: 40.0,
            w: 30.0,
            h: 20.0,
            rotation_deg: 25.0,
        };
        for on in [true, false] {
            let mut fill = PatternFill::new(Pattern::default(), &xf);
            fill.set_transform_with_object(on, &xf);
            let f = PatternFrame {
                origin: [12.0, 7.0],
                rotation_deg: 40.0,
                scale: 2.0,
            };
            fill.set_world_frame(&xf, f);
            let g = fill.world_frame(&xf);
            assert!((g.origin[0] - 12.0).abs() < 1e-9 && (g.origin[1] - 7.0).abs() < 1e-9);
            assert!((g.rotation_deg - 40.0).abs() < 1e-9 && (g.scale - 2.0).abs() < 1e-9);
            // Toggling keeps the pattern in place.
            fill.set_transform_with_object(!on, &xf);
            let h = fill.world_frame(&xf);
            assert!((h.origin[0] - 12.0).abs() < 1e-9 && (h.scale - 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn pattern_follows_object_only_when_enabled() {
        let xf = Transform::new(50.0, 50.0, 20.0, 20.0);
        let moved = Transform::new(60.0, 50.0, 40.0, 40.0);
        let mut fill = PatternFill::new(Pattern::default(), &xf);
        let f = fill.world_frame(&moved);
        assert!((f.origin[0] - 60.0).abs() < 1e-9 && (f.scale - 2.0).abs() < 1e-9);
        fill.set_transform_with_object(false, &xf);
        let f = fill.world_frame(&moved);
        assert!((f.origin[0] - 50.0).abs() < 1e-9 && (f.scale - 1.0).abs() < 1e-9);
    }
}
