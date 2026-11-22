//! Shape definitions and their conversion to `geo` polygons.
//!
//! Pure geometry, no egui. Units are millimetres (f64). The y axis points down
//! (same convention as SVG), so a positive rotation turns clockwise on screen.

use geo::{BoundingRect, LineString, MultiPolygon, Polygon};
use serde::{Deserialize, Serialize};
use std::f64::consts::{PI, TAU};

/// Smallest width/height a transform may have (avoids divisions by zero).
pub const MIN_EXTENT: f64 = 1.0e-6;
/// Largest absolute coordinate / size accepted anywhere (1 km).
pub const MAX_COORD: f64 = 1.0e6;
/// Default curve flattening tolerance (mm).
pub const DEFAULT_TOLERANCE: f64 = 0.01;
/// Upper bound on the vertex count of a single flattened curve.
pub const MAX_CURVE_SEGMENTS: usize = 8192;

// ---------------------------------------------------------------------------
// Colour & style
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    #[serde(default = "opaque")]
    pub a: u8,
}

fn opaque() -> u8 {
    255
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// `#rrggbb` (alpha is written separately as an opacity).
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    pub fn opacity(self) -> f64 {
        f64::from(self.a) / 255.0
    }

    /// Fully opaque?
    pub fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// Matches the white artboard: opaque, every RGB channel ≥ `min_channel`.
    /// (CorelDRAW's CMYK 0/0/0/0 arrives as RGB 255/255/255.)
    pub fn is_white(self, min_channel: u8) -> bool {
        self.is_opaque() && self.r >= min_channel && self.g >= min_channel && self.b >= min_channel
    }

    /// This colour with its alpha multiplied by `opacity` (0–1).
    pub fn with_opacity(mut self, opacity: f64) -> Self {
        let o = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        self.a = (f64::from(self.a) * o).round() as u8;
        self
    }
}

/// How stroke segments meet (SVG `stroke-linejoin`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// How open stroke ends look (SVG `stroke-linecap`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

fn default_miter_limit() -> f64 {
    4.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    /// Stroke width in mm.
    pub stroke_width: f64,
    #[serde(default)]
    pub line_join: LineJoin,
    #[serde(default)]
    pub line_cap: LineCap,
    /// SVG miter limit (ratio of miter length to stroke width), default 4.
    #[serde(default = "default_miter_limit")]
    pub miter_limit: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fill: Some(Color::rgb(0xb8, 0xd4, 0xf0)),
            stroke: Some(Color::rgb(0x30, 0x30, 0x30)),
            stroke_width: 0.25,
            line_join: LineJoin::Miter,
            line_cap: LineCap::Butt,
            miter_limit: 4.0,
        }
    }
}

impl Style {
    /// A plain filled area (no stroke).
    pub fn fill_only(c: Color) -> Self {
        Self {
            fill: Some(c),
            stroke: None,
            stroke_width: 0.0,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

/// Placement of a shape: centre, size and rotation (all mm / degrees).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub cx: f64,
    pub cy: f64,
    pub w: f64,
    pub h: f64,
    pub rotation_deg: f64,
}

impl Transform {
    pub fn new(cx: f64, cy: f64, w: f64, h: f64) -> Self {
        Self {
            cx,
            cy,
            w,
            h,
            rotation_deg: 0.0,
        }
    }

    /// A copy with every value finite and within sane limits.
    pub fn sanitized(&self) -> Self {
        Self {
            cx: clamp_finite(self.cx, -MAX_COORD, MAX_COORD, 0.0),
            cy: clamp_finite(self.cy, -MAX_COORD, MAX_COORD, 0.0),
            w: clamp_finite(self.w, MIN_EXTENT, MAX_COORD, 1.0),
            h: clamp_finite(self.h, MIN_EXTENT, MAX_COORD, 1.0),
            rotation_deg: normalize_deg(finite_or(self.rotation_deg, 0.0)),
        }
    }

    pub fn center(&self) -> [f64; 2] {
        [self.cx, self.cy]
    }

    /// Local (centre-relative, unrotated, real size) point to world.
    pub fn local_to_world(&self, p: [f64; 2]) -> [f64; 2] {
        let r = rotate_vec(p, self.rotation_deg);
        [self.cx + r[0], self.cy + r[1]]
    }

    pub fn world_to_local(&self, p: [f64; 2]) -> [f64; 2] {
        rotate_vec([p[0] - self.cx, p[1] - self.cy], -self.rotation_deg)
    }
}

pub fn finite_or(v: f64, default: f64) -> f64 {
    if v.is_finite() { v } else { default }
}

pub fn clamp_finite(v: f64, lo: f64, hi: f64, default: f64) -> f64 {
    finite_or(v, default).clamp(lo, hi)
}

/// Normalise an angle to (-180, 180].
pub fn normalize_deg(d: f64) -> f64 {
    let mut r = d % 360.0;
    if r > 180.0 {
        r -= 360.0;
    } else if r <= -180.0 {
        r += 360.0;
    }
    r
}

pub fn rotate_vec(v: [f64; 2], deg: f64) -> [f64; 2] {
    if deg == 0.0 {
        return v;
    }
    let (s, c) = deg.to_radians().sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

// ---------------------------------------------------------------------------
// Rings (serialisable polygon data)
// ---------------------------------------------------------------------------

/// One polygon: an exterior ring plus optional holes. Rings are open
/// (the closing point is implicit).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rings {
    pub exterior: Vec<[f64; 2]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holes: Vec<Vec<[f64; 2]>>,
}

impl Rings {
    pub fn to_polygon(&self) -> Polygon<f64> {
        Polygon::new(
            LineString::from(self.exterior.clone()),
            self.holes
                .iter()
                .map(|h| LineString::from(h.clone()))
                .collect(),
        )
    }

    pub fn from_polygon(p: &Polygon<f64>) -> Self {
        Self {
            exterior: ring_points(p.exterior()),
            holes: p.interiors().iter().map(ring_points).collect(),
        }
    }

    pub fn map(&self, f: impl Fn([f64; 2]) -> [f64; 2]) -> Self {
        Self {
            exterior: self.exterior.iter().map(|&p| f(p)).collect(),
            holes: self
                .holes
                .iter()
                .map(|h| h.iter().map(|&p| f(p)).collect())
                .collect(),
        }
    }
}

/// Points of a ring without the duplicated closing point.
pub fn ring_points(ls: &LineString<f64>) -> Vec<[f64; 2]> {
    let mut v: Vec<[f64; 2]> = ls.coords().map(|c| [c.x, c.y]).collect();
    if v.len() > 1 && v.first() == v.last() {
        v.pop();
    }
    v
}

// ---------------------------------------------------------------------------
// Shape geometry
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ShapeGeom {
    Rect,
    Ellipse,
    /// Regular polygon inscribed in the ellipse of the bounding box.
    Polygon { sides: u32 },
    Star { points: u32, inner_ratio: f64 },
    /// Arbitrary polygons (pen paths, expanded pieces). Coordinates are
    /// centre-relative at size `base_w` x `base_h`; the transform's w/h scale them.
    /// The outlines are immutable and shared (`Arc`), so undo snapshots and
    /// copies of documents with many pieces do not duplicate geometry.
    Custom {
        polys: std::sync::Arc<Vec<Rings>>,
        base_w: f64,
        base_h: f64,
    },
}

impl ShapeGeom {
    pub fn kind_name(&self) -> &'static str {
        match self {
            ShapeGeom::Rect => "Rectangle",
            ShapeGeom::Ellipse => "Ellipse",
            ShapeGeom::Polygon { .. } => "Polygon",
            ShapeGeom::Star { .. } => "Star",
            ShapeGeom::Custom { .. } => "Path",
        }
    }
}

/// Number of segments so a circle of `radius` deviates at most `tol` from the true curve.
pub fn segments_for_radius(radius: f64, tol: f64) -> usize {
    let tol = clamp_finite(tol, 1.0e-6, 1.0e3, DEFAULT_TOLERANCE);
    let radius = finite_or(radius, 0.0).abs();
    if radius <= tol {
        return 8;
    }
    // Sagitta of a chord spanning angle a: r * (1 - cos(a/2)) <= tol.
    let half = (1.0 - tol / radius).clamp(-1.0, 1.0).acos();
    if half <= 0.0 {
        return MAX_CURVE_SEGMENTS;
    }
    let n = (PI / half).ceil();
    (n as usize).clamp(8, MAX_CURVE_SEGMENTS)
}

/// Unit-box (radius 0.5) regular polygon, first vertex at the top.
pub fn regular_polygon_unit(sides: u32) -> Vec<[f64; 2]> {
    let n = sides.clamp(3, 1000);
    (0..n)
        .map(|k| {
            let a = -PI / 2.0 + TAU * f64::from(k) / f64::from(n);
            [0.5 * a.cos(), 0.5 * a.sin()]
        })
        .collect()
}

/// Unit-box star: outer radius 0.5, inner radius 0.5 * ratio.
pub fn star_unit(points: u32, inner_ratio: f64) -> Vec<[f64; 2]> {
    let n = points.clamp(3, 1000);
    let ratio = clamp_finite(inner_ratio, 0.01, 1.0, 0.5);
    (0..2 * n)
        .map(|k| {
            let a = -PI / 2.0 + PI * f64::from(k) / f64::from(n);
            let r = if k % 2 == 0 { 0.5 } else { 0.5 * ratio };
            [r * a.cos(), r * a.sin()]
        })
        .collect()
}

/// Circle of radius `r` around the origin with `n` vertices.
pub fn circle_points(r: f64, n: usize) -> Vec<[f64; 2]> {
    let n = n.max(3);
    (0..n)
        .map(|k| {
            let a = TAU * k as f64 / n as f64;
            [r * a.cos(), r * a.sin()]
        })
        .collect()
}

/// Geometry in local coordinates (centre-relative, unrotated, at real size).
pub fn local_rings(geom: &ShapeGeom, xf: &Transform, tol: f64) -> Vec<Rings> {
    let xf = xf.sanitized();
    let (w, h) = (xf.w, xf.h);
    let scale_unit = |pts: Vec<[f64; 2]>| -> Vec<Rings> {
        vec![Rings {
            exterior: pts.into_iter().map(|p| [p[0] * w, p[1] * h]).collect(),
            holes: vec![],
        }]
    };
    match geom {
        ShapeGeom::Rect => scale_unit(vec![[-0.5, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5]]),
        ShapeGeom::Ellipse => {
            let n = segments_for_radius(w.max(h) / 2.0, tol);
            scale_unit(circle_points(0.5, n))
        }
        ShapeGeom::Polygon { sides } => scale_unit(regular_polygon_unit(*sides)),
        ShapeGeom::Star {
            points,
            inner_ratio,
        } => scale_unit(star_unit(*points, *inner_ratio)),
        ShapeGeom::Custom {
            polys,
            base_w,
            base_h,
        } => {
            let sx = if *base_w > 1.0e-9 { w / base_w } else { 1.0 };
            let sy = if *base_h > 1.0e-9 { h / base_h } else { 1.0 };
            polys
                .iter()
                .map(|r| r.map(|p| [p[0] * sx, p[1] * sy]))
                .collect()
        }
    }
}

/// World-space polygons of a shape.
pub fn world_geometry(geom: &ShapeGeom, xf: &Transform, tol: f64) -> MultiPolygon<f64> {
    let xf = xf.sanitized();
    let polys = local_rings(geom, &xf, tol)
        .into_iter()
        .filter(|r| r.exterior.len() >= 3)
        .map(|r| r.map(|p| xf.local_to_world(p)).to_polygon())
        .collect();
    MultiPolygon::new(polys)
}

/// Turn world polygons into a `Custom` shape placed at their bounding-box centre.
pub fn custom_from_world(mp: &MultiPolygon<f64>) -> Option<(ShapeGeom, Transform)> {
    let bb = mp.bounding_rect()?;
    let (w, h) = (bb.width(), bb.height());
    if !(w.is_finite() && h.is_finite()) {
        return None;
    }
    let c = bb.center();
    let polys = std::sync::Arc::new(
        mp.0.iter().map(|p| Rings::from_polygon(p).map(|q| [q[0] - c.x, q[1] - c.y])).collect(),
    );
    Some((
        ShapeGeom::Custom {
            polys,
            base_w: w,
            base_h: h,
        },
        Transform::new(c.x, c.y, w.max(MIN_EXTENT), h.max(MIN_EXTENT)),
    ))
}

/// Normalise world polygons so they are centred on the origin and the larger
/// bounding-box side is 1. Used for "use selected shape as tile".
pub fn normalized_tile(mp: &MultiPolygon<f64>) -> Option<Vec<Rings>> {
    let bb = mp.bounding_rect()?;
    let size = bb.width().max(bb.height());
    if !(size.is_finite() && size > 1.0e-9) {
        return None;
    }
    let c = bb.center();
    Some(
        mp.0.iter()
            .map(|p| Rings::from_polygon(p).map(|q| [(q[0] - c.x) / size, (q[1] - c.y) / size]))
            .collect(),
    )
}

/// Remove slivers: polygons and holes smaller than `min_area` (mm²). Boolean
/// operations on fonts with overlapping contours leave hairline slits behind.
pub fn drop_tiny_parts(mp: MultiPolygon<f64>, min_area: f64) -> MultiPolygon<f64> {
    use geo::Area;
    let min_area = if min_area.is_finite() { min_area.max(1e-9) } else { 1e-9 };
    MultiPolygon::new(
        mp.0.into_iter()
            .filter(|p| Polygon::new(p.exterior().clone(), vec![]).unsigned_area() >= min_area)
            .map(|p| {
                let (ext, holes) = p.into_inner();
                let holes = holes
                    .into_iter()
                    .filter(|h| Polygon::new(h.clone(), vec![]).unsigned_area() >= min_area)
                    .collect();
                Polygon::new(ext, holes)
            })
            .collect(),
    )
}

/// Axis-aligned bounds `[min_x, min_y, max_x, max_y]`.
pub type Bounds = [f64; 4];

pub fn bounds_of(mp: &MultiPolygon<f64>) -> Option<Bounds> {
    mp.bounding_rect()
        .map(|r| [r.min().x, r.min().y, r.max().x, r.max().y])
}

pub fn bounds_union(a: Option<Bounds>, b: Option<Bounds>) -> Option<Bounds> {
    match (a, b) {
        (Some(a), Some(b)) => Some([a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]),
        (a, None) => a,
        (None, b) => b,
    }
}

pub fn bounds_overlap(a: &Bounds, b: &Bounds) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

/// Even-odd point-in-polygon test over every ring of a multipolygon.
pub fn point_in_evenodd(p: [f64; 2], mp: &MultiPolygon<f64>) -> bool {
    let mut inside = false;
    for poly in &mp.0 {
        for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
            let pts = &ring.0;
            let n = pts.len();
            if n < 3 {
                continue;
            }
            let mut j = n - 1;
            for i in 0..n {
                let (a, b) = (pts[i], pts[j]);
                if (a.y > p[1]) != (b.y > p[1]) {
                    let x = a.x + (p[1] - a.y) * (b.x - a.x) / (b.y - a.y);
                    if p[0] < x {
                        inside = !inside;
                    }
                }
                j = i;
            }
        }
    }
    inside
}

/// Distance from point `p` to segment `a`-`b`.
pub fn dist_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (qx, qy) = (a[0] + t * dx, a[1] + t * dy);
    ((p[0] - qx).powi(2) + (p[1] - qy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    #[test]
    fn ellipse_respects_tolerance() {
        let xf = Transform::new(0.0, 0.0, 100.0, 100.0);
        let mp = world_geometry(&ShapeGeom::Ellipse, &xf, 0.01);
        let area = mp.unsigned_area();
        let exact = PI * 50.0 * 50.0;
        // Inscribed polygon: error bounded by perimeter * tolerance.
        assert!((exact - area) > 0.0 && (exact - area) < TAU * 50.0 * 0.01);
    }

    #[test]
    fn degenerate_transform_does_not_panic() {
        let xf = Transform {
            cx: f64::NAN,
            cy: f64::INFINITY,
            w: 0.0,
            h: -5.0,
            rotation_deg: f64::NAN,
        };
        for g in [
            ShapeGeom::Rect,
            ShapeGeom::Ellipse,
            ShapeGeom::Polygon { sides: 0 },
            ShapeGeom::Star {
                points: 1,
                inner_ratio: -1.0,
            },
        ] {
            let mp = world_geometry(&g, &xf, 0.0);
            assert!(!mp.0.is_empty());
        }
    }

    #[test]
    fn evenodd_respects_holes() {
        let mp = MultiPolygon::new(vec![Rings {
            exterior: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            holes: vec![vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]]],
        }
        .to_polygon()]);
        assert!(point_in_evenodd([1.0, 1.0], &mp));
        assert!(!point_in_evenodd([5.0, 5.0], &mp));
        assert!(!point_in_evenodd([11.0, 5.0], &mp));
    }
}
