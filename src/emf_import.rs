//! Enhanced Metafile (EMF) import — the vector format CorelDRAW (and many other
//! Windows programs) put on the clipboard.
//!
//! Only the GDI drawing records that describe vector shapes are interpreted:
//! polygons, poly-polygons, polylines and Bézier curves (16- and 32-bit),
//! rectangles, rounded rectangles, ellipses and path brackets (`BeginPath` …
//! `FillPath` / `StrokePath` / `StrokeAndFillPath`), with pens, brushes, the
//! polygon fill mode, `SaveDC` / `RestoreDC`, mapping modes, window/viewport
//! origins and extents and the world transform. Text and bitmaps are skipped.
//! EMF+ records (inside GDI comments) are ignored; "dual" metafiles still carry
//! the equivalent GDI records.
//!
//! Size: logical coordinates are mapped to device units and device units to
//! millimetres with the header's reference-device size (`szlMillimeters` /
//! `szlDevice`, or micrometres when present), so objects keep their real size.

use crate::document::ShapeObj;
use crate::flatten::outline_stroke;
use crate::shapes::{Color, LineCap, LineJoin, Style, custom_from_world};
use crate::svg_import::{ImportNode, ImportReport, fill_region, push_cubic};
use geo::{Area, BooleanOps, MultiPolygon};

// Record types (MS-EMF 2.1.1).
const EMR_HEADER: u32 = 1;
const EMR_POLYBEZIER: u32 = 2;
const EMR_POLYGON: u32 = 3;
const EMR_POLYLINE: u32 = 4;
const EMR_POLYBEZIERTO: u32 = 5;
const EMR_POLYLINETO: u32 = 6;
const EMR_POLYPOLYLINE: u32 = 7;
const EMR_POLYPOLYGON: u32 = 8;
const EMR_SETWINDOWEXTEX: u32 = 9;
const EMR_SETWINDOWORGEX: u32 = 10;
const EMR_SETVIEWPORTEXTEX: u32 = 11;
const EMR_SETVIEWPORTORGEX: u32 = 12;
const EMR_EOF: u32 = 14;
const EMR_SETMAPMODE: u32 = 17;
const EMR_SETPOLYFILLMODE: u32 = 19;
const EMR_MOVETOEX: u32 = 27;
const EMR_EXCLUDECLIPRECT: u32 = 29;
const EMR_INTERSECTCLIPRECT: u32 = 30;
const EMR_SAVEDC: u32 = 33;
const EMR_RESTOREDC: u32 = 34;
const EMR_SETWORLDTRANSFORM: u32 = 35;
const EMR_MODIFYWORLDTRANSFORM: u32 = 36;
const EMR_SELECTOBJECT: u32 = 37;
const EMR_CREATEPEN: u32 = 38;
const EMR_CREATEBRUSHINDIRECT: u32 = 39;
const EMR_DELETEOBJECT: u32 = 40;
const EMR_ELLIPSE: u32 = 42;
const EMR_RECTANGLE: u32 = 43;
const EMR_ROUNDRECT: u32 = 44;
const EMR_LINETO: u32 = 54;
const EMR_SETMITERLIMIT: u32 = 58;
const EMR_BEGINPATH: u32 = 59;
const EMR_ENDPATH: u32 = 60;
const EMR_CLOSEFIGURE: u32 = 61;
const EMR_FILLPATH: u32 = 62;
const EMR_STROKEANDFILLPATH: u32 = 63;
const EMR_STROKEPATH: u32 = 64;
const EMR_SELECTCLIPPATH: u32 = 67;
const EMR_ABORTPATH: u32 = 68;
const EMR_EXTSELECTCLIPRGN: u32 = 75;
const EMR_BITBLT: u32 = 76;
const EMR_STRETCHBLT: u32 = 77;
const EMR_STRETCHDIBITS: u32 = 81;
const EMR_EXTTEXTOUTA: u32 = 83;
const EMR_EXTTEXTOUTW: u32 = 84;
const EMR_POLYBEZIER16: u32 = 85;
const EMR_POLYGON16: u32 = 86;
const EMR_POLYLINE16: u32 = 87;
const EMR_POLYBEZIERTO16: u32 = 88;
const EMR_POLYLINETO16: u32 = 89;
const EMR_POLYPOLYLINE16: u32 = 90;
const EMR_POLYPOLYGON16: u32 = 91;
const EMR_EXTCREATEPEN: u32 = 95;

const EMF_SIGNATURE: u32 = 0x464D_4520; // " EMF"

/// Little-endian reader over one record.
struct Rec<'a>(&'a [u8]);

impl Rec<'_> {
    fn u32(&self, off: usize) -> Option<u32> {
        self.0.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&self, off: usize) -> Option<i32> {
        self.u32(off).map(|v| v as i32)
    }
    fn i16(&self, off: usize) -> Option<i16> {
        self.0.get(off..off + 2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }
    fn f32(&self, off: usize) -> Option<f32> {
        self.u32(off).map(f32::from_bits)
    }
    fn point(&self, off: usize, short: bool) -> Option<[f64; 2]> {
        if short {
            Some([f64::from(self.i16(off)?), f64::from(self.i16(off + 2)?)])
        } else {
            Some([f64::from(self.i32(off)?), f64::from(self.i32(off + 4)?)])
        }
    }
    fn points(&self, off: usize, n: usize, short: bool) -> Option<Vec<[f64; 2]>> {
        let step = if short { 4 } else { 8 };
        // Guard against absurd counts before allocating.
        if n.checked_mul(step)?.checked_add(off)? > self.0.len() {
            return None;
        }
        (0..n).map(|i| self.point(off + i * step, short)).collect()
    }
}

/// COLORREF 0x00BBGGRR.
fn colorref(v: u32) -> Color {
    Color::rgb((v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8)
}

/// 2-D affine transform in the GDI XFORM convention (row vector: p' = p · M).
#[derive(Clone, Copy, Debug)]
struct Xform {
    m11: f64,
    m12: f64,
    m21: f64,
    m22: f64,
    dx: f64,
    dy: f64,
}

impl Xform {
    const IDENTITY: Xform = Xform { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: 0.0, dy: 0.0 };

    fn read(r: &Rec, off: usize) -> Option<Xform> {
        let v = |i: usize| r.f32(off + i * 4).map(f64::from);
        let x = Xform { m11: v(0)?, m12: v(1)?, m21: v(2)?, m22: v(3)?, dx: v(4)?, dy: v(5)? };
        [x.m11, x.m12, x.m21, x.m22, x.dx, x.dy].iter().all(|f| f.is_finite()).then_some(x)
    }

    fn apply(&self, p: [f64; 2]) -> [f64; 2] {
        [p[0] * self.m11 + p[1] * self.m21 + self.dx, p[0] * self.m12 + p[1] * self.m22 + self.dy]
    }

    /// `self` applied first, then `b`.
    fn then(&self, b: &Xform) -> Xform {
        Xform {
            m11: self.m11 * b.m11 + self.m12 * b.m21,
            m12: self.m11 * b.m12 + self.m12 * b.m22,
            m21: self.m21 * b.m11 + self.m22 * b.m21,
            m22: self.m21 * b.m12 + self.m22 * b.m22,
            dx: self.dx * b.m11 + self.dy * b.m21 + b.dx,
            dy: self.dx * b.m12 + self.dy * b.m22 + b.dy,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Pen {
    color: Option<Color>,
    /// Logical units; `None` = cosmetic (one device pixel).
    width: Option<f64>,
    join: LineJoin,
    cap: LineCap,
}

impl Pen {
    /// GDI's default for pens without explicit end/join styles is round.
    const fn simple(color: Option<Color>, width: Option<f64>) -> Self {
        Pen { color, width, join: LineJoin::Round, cap: LineCap::Round }
    }
}

#[derive(Clone, Copy, Debug)]
enum GdiObj {
    Pen(Pen),
    Brush(Option<Color>),
}

/// Device-context state (saved / restored by SaveDC / RestoreDC).
#[derive(Clone, Debug)]
struct Dc {
    world: Xform,
    map_mode: u32,
    win_org: [f64; 2],
    win_ext: [f64; 2],
    vp_org: [f64; 2],
    vp_ext: [f64; 2],
    even_odd: bool,
    pen: Pen,
    brush: Option<Color>,
    /// Current position, logical units.
    pos: [f64; 2],
    /// GDI miter limit (default 10).
    miter_limit: f64,
    /// Clip region in mm (None = no clipping).
    clip: Option<MultiPolygon<f64>>,
}

impl Default for Dc {
    fn default() -> Self {
        Self {
            world: Xform::IDENTITY,
            map_mode: 1, // MM_TEXT
            win_org: [0.0; 2],
            win_ext: [1.0; 2],
            vp_org: [0.0; 2],
            vp_ext: [1.0; 2],
            even_odd: true, // ALTERNATE
            pen: Pen::simple(Some(Color::rgb(0, 0, 0)), None),
            brush: Some(Color::rgb(255, 255, 255)),
            pos: [0.0; 2],
            miter_limit: 10.0,
            clip: None,
        }
    }
}

struct Parser {
    tol: f64,
    mm_per_dev: [f64; 2],
    dc: Dc,
    saved: Vec<Dc>,
    objects: std::collections::HashMap<u32, GdiObj>,
    /// Inside BeginPath … EndPath.
    in_path: bool,
    /// Path figures in mm: (points, closed).
    figures: Vec<(Vec<[f64; 2]>, bool)>,
    cur: Vec<[f64; 2]>,
    report: ImportReport,
}

impl Parser {
    /// Logical → device scale for the current mapping mode.
    fn log_to_dev_scale(&self) -> [f64; 2] {
        let metric = |unit_mm: f64| [unit_mm / self.mm_per_dev[0], -unit_mm / self.mm_per_dev[1]];
        match self.dc.map_mode {
            2 => metric(0.1),           // MM_LOMETRIC
            3 => metric(0.01),          // MM_HIMETRIC
            4 => metric(0.254),         // MM_LOENGLISH
            5 => metric(0.0254),        // MM_HIENGLISH
            6 => metric(25.4 / 1440.0), // MM_TWIPS
            7 | 8 => {
                // MM_ISOTROPIC / MM_ANISOTROPIC
                let s = |v: f64, w: f64| if w.abs() > 1e-12 { v / w } else { 1.0 };
                [s(self.dc.vp_ext[0], self.dc.win_ext[0]), s(self.dc.vp_ext[1], self.dc.win_ext[1])]
            }
            _ => [1.0, 1.0], // MM_TEXT
        }
    }

    /// Logical coordinates → millimetres.
    fn mm(&self, p: [f64; 2]) -> [f64; 2] {
        let w = self.dc.world.apply(p);
        let s = self.log_to_dev_scale();
        let d = [
            self.dc.vp_org[0] + (w[0] - self.dc.win_org[0]) * s[0],
            self.dc.vp_org[1] + (w[1] - self.dc.win_org[1]) * s[1],
        ];
        [d[0] * self.mm_per_dev[0], d[1] * self.mm_per_dev[1]]
    }

    /// Millimetres per logical unit (for pen widths).
    fn linear_scale(&self) -> f64 {
        let o = self.mm([0.0, 0.0]);
        let a = self.mm([1.0, 0.0]);
        let b = self.mm([0.0, 1.0]);
        let (ax, ay, bx, by) = (a[0] - o[0], a[1] - o[1], b[0] - o[0], b[1] - o[1]);
        (ax * by - ay * bx).abs().sqrt()
    }

    fn pen_width_mm(&self) -> f64 {
        match self.dc.pen.width {
            Some(w) if w > 0.0 => w * self.linear_scale(),
            _ => self.mm_per_dev[0].abs(), // cosmetic: one device pixel
        }
    }

    // ---- path building --------------------------------------------------------

    fn finish_figure(&mut self, closed: bool) {
        let pts = std::mem::take(&mut self.cur);
        if pts.len() > 1 {
            self.figures.push((pts, closed));
        }
    }

    fn start_at_pos(&mut self) {
        if self.cur.is_empty() {
            let p = self.mm(self.dc.pos);
            self.cur.push(p);
        }
    }

    fn line_to(&mut self, p: [f64; 2]) {
        if self.in_path {
            self.start_at_pos();
            let q = self.mm(p);
            self.cur.push(q);
        }
        self.dc.pos = p;
    }

    /// Bézier segments continuing from the current position (3 points each).
    fn bezier_to(&mut self, pts: &[[f64; 2]]) {
        if self.in_path {
            self.start_at_pos();
            for c in pts.as_chunks::<3>().0 {
                let p0 = *self.cur.last().unwrap_or(&self.mm(self.dc.pos));
                let (p1, p2, p3) = (self.mm(c[0]), self.mm(c[1]), self.mm(c[2]));
                push_cubic(&mut self.cur, p0, p1, p2, p3, self.tol);
            }
        }
        if let Some(last) = pts.len().checked_sub(pts.len() % 3 + 1).and_then(|i| pts.get(i)) {
            self.dc.pos = *last;
        }
    }

    /// A figure given in logical coordinates (already flattened or polygonal).
    fn figure_mm(&self, pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
        pts.iter().map(|p| self.mm(*p)).collect()
    }

    fn bezier_figure(&self, pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
        let Some(first) = pts.first() else { return vec![] };
        let mut out = vec![self.mm(*first)];
        for c in pts[1..].as_chunks::<3>().0 {
            let p0 = *out.last().unwrap_or(&out[0]);
            push_cubic(&mut out, p0, self.mm(c[0]), self.mm(c[1]), self.mm(c[2]), self.tol);
        }
        out
    }

    /// Ellipse inscribed in a logical box, as four cubic arcs.
    fn ellipse_figure(&self, b: [f64; 4]) -> Vec<[f64; 2]> {
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let (rx, ry) = ((b[2] - b[0]).abs() / 2.0, (b[3] - b[1]).abs() / 2.0);
        self.rounded_figure([cx - rx, cy - ry, cx + rx, cy + ry], rx, ry)
    }

    /// Box with elliptical corners of radii (rx, ry), clockwise in logical space.
    fn rounded_figure(&self, b: [f64; 4], rx: f64, ry: f64) -> Vec<[f64; 2]> {
        const K: f64 = 0.552_284_749_830_793_6; // 4/3·(√2−1)
        let (l, t, r, bt) = (b[0].min(b[2]), b[1].min(b[3]), b[0].max(b[2]), b[1].max(b[3]));
        let rx = rx.clamp(0.0, (r - l) / 2.0);
        let ry = ry.clamp(0.0, (bt - t) / 2.0);
        let m = |p: [f64; 2]| self.mm(p);
        let mut out = vec![m([l + rx, t])];
        let line = |out: &mut Vec<[f64; 2]>, p: [f64; 2]| {
            let q = m(p);
            if out.last() != Some(&q) {
                out.push(q);
            }
        };
        let arc = |out: &mut Vec<[f64; 2]>, c1: [f64; 2], c2: [f64; 2], e: [f64; 2]| {
            if rx > 0.0 && ry > 0.0 {
                let p0 = *out.last().unwrap_or(&m(e));
                push_cubic(out, p0, m(c1), m(c2), m(e), self.tol);
            }
        };
        line(&mut out, [r - rx, t]);
        arc(&mut out, [r - rx + K * rx, t], [r, t + ry - K * ry], [r, t + ry]);
        line(&mut out, [r, bt - ry]);
        arc(&mut out, [r, bt - ry + K * ry], [r - rx + K * rx, bt], [r - rx, bt]);
        line(&mut out, [l + rx, bt]);
        arc(&mut out, [l + rx - K * rx, bt], [l, bt - ry + K * ry], [l, bt - ry]);
        line(&mut out, [l, t + ry]);
        arc(&mut out, [l, t + ry - K * ry], [l + rx - K * rx, t], [l + rx, t]);
        if out.len() > 1 && out.first() == out.last() {
            out.pop();
        }
        out
    }

    /// Either add closed figures to the open path, or draw them right away.
    fn closed_shapes(&mut self, figs: Vec<Vec<[f64; 2]>>) {
        if self.in_path {
            for f in figs {
                self.finish_figure(false);
                self.figures.push((f, true));
            }
        } else {
            let figs = figs.into_iter().map(|f| (f, true)).collect();
            self.emit(figs, true, true);
        }
    }

    fn open_shapes(&mut self, figs: Vec<Vec<[f64; 2]>>) {
        if self.in_path {
            for f in figs {
                self.finish_figure(false);
                self.figures.push((f, false));
            }
        } else {
            // A stand-alone polyline / curve is drawn with the pen only.
            let figs = figs.into_iter().map(|f| (f, false)).collect();
            self.emit(figs, false, true);
        }
    }

    fn push_shape(&mut self, region: MultiPolygon<f64>, style: Style) {
        let region = match &self.dc.clip {
            Some(c) => region.intersection(c),
            None => region,
        };
        if region.unsigned_area() <= 1e-9 {
            return;
        }
        let Some((geom, xf)) = custom_from_world(&region) else { return };
        self.report.shapes += 1;
        let name = format!("EMF shape {}", self.report.shapes);
        self.report.nodes.push(ImportNode::Shape(name, Box::new(ShapeObj::new(geom, xf, style))));
    }

    /// Turn figures into shapes with the current brush and/or pen.
    ///
    /// Normally one shape whose stroke stays an editable style. When the pen
    /// cannot be drawn along the filled outline — open figures (lines) or a clip
    /// region cutting through it — the stroke is outlined into its own filled
    /// shape (real width, joins, caps, miter limit), so nothing visible changes.
    fn emit(&mut self, figs: Vec<(Vec<[f64; 2]>, bool)>, fill: bool, stroke: bool) {
        let fill_color = if fill { self.dc.brush } else { None };
        let stroke_color = if stroke { self.dc.pen.color } else { None };
        if fill_color.is_none() && stroke_color.is_none() {
            return;
        }
        let pen = self.dc.pen;
        let width = self.pen_width_mm();
        let any_open = figs.iter().any(|(_, closed)| !closed);
        let separate_stroke = stroke_color.is_some() && (self.dc.clip.is_some() || any_open);
        if !separate_stroke {
            // Filling closes every figure; a stroke alone only encloses closed ones.
            let rings: Vec<&Vec<[f64; 2]>> = figs
                .iter()
                .filter(|(p, closed)| p.len() >= 3 && (fill_color.is_some() || *closed))
                .map(|(p, _)| p)
                .collect();
            let region = fill_region(&rings, self.dc.even_odd);
            let style = Style {
                fill: fill_color,
                stroke: stroke_color,
                stroke_width: if stroke_color.is_some() { width } else { 0.0 },
                line_join: pen.join,
                line_cap: pen.cap,
                miter_limit: self.dc.miter_limit,
            };
            self.push_shape(region, style);
            return;
        }
        if let Some(c) = fill_color {
            let rings: Vec<&Vec<[f64; 2]>> = figs.iter().filter(|(p, _)| p.len() >= 3).map(|(p, _)| p).collect();
            let region = fill_region(&rings, self.dc.even_odd);
            self.push_shape(region, Style::fill_only(c));
        }
        if let Some(c) = stroke_color {
            let area = outline_stroke(&figs, width, pen.join, pen.cap, self.dc.miter_limit, self.tol);
            self.push_shape(area, Style::fill_only(c));
        }
    }

    /// Logical rectangle → polygon in mm (respects rotation in the world transform).
    fn rect_mm(&self, b: [f64; 4]) -> MultiPolygon<f64> {
        let pts = [[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]]].map(|p| self.mm(p));
        fill_region(&[&pts.to_vec()], true)
    }

    /// Combine a new region into the clip (GDI RGN_* modes).
    fn combine_clip(&mut self, region: MultiPolygon<f64>, mode: u32) {
        self.dc.clip = match (mode, self.dc.clip.take()) {
            (5, _) | (1, None) => Some(region),               // RGN_COPY, AND with "everything"
            (1, Some(c)) => Some(c.intersection(&region)),    // RGN_AND
            (2, Some(c)) => Some(c.union(&region)),           // RGN_OR
            (3, Some(c)) => Some(c.xor(&region)),             // RGN_XOR
            (4, Some(c)) => Some(c.difference(&region)),      // RGN_DIFF
            // OR / XOR / DIFF with an unclipped (infinite) region stay unclipped.
            (_, other) => other,
        };
    }

    fn take_path(&mut self) -> Vec<(Vec<[f64; 2]>, bool)> {
        self.finish_figure(false);
        std::mem::take(&mut self.figures)
    }

    // ---- records --------------------------------------------------------------------

    fn record(&mut self, kind: u32, r: &Rec) -> Option<()> {
        match kind {
            EMR_SETMAPMODE => self.dc.map_mode = r.u32(8)?,
            EMR_SETWINDOWEXTEX => self.dc.win_ext = r.point(8, false)?,
            EMR_SETWINDOWORGEX => self.dc.win_org = r.point(8, false)?,
            EMR_SETVIEWPORTEXTEX => self.dc.vp_ext = r.point(8, false)?,
            EMR_SETVIEWPORTORGEX => self.dc.vp_org = r.point(8, false)?,
            EMR_SETPOLYFILLMODE => self.dc.even_odd = r.u32(8)? != 2,
            EMR_SETWORLDTRANSFORM => self.dc.world = Xform::read(r, 8)?,
            EMR_MODIFYWORLDTRANSFORM => {
                let x = Xform::read(r, 8)?;
                self.dc.world = match r.u32(32)? {
                    1 => Xform::IDENTITY,
                    2 => x.then(&self.dc.world), // MWT_LEFTMULTIPLY
                    3 => self.dc.world.then(&x), // MWT_RIGHTMULTIPLY
                    4 => x,                      // MWT_SET
                    _ => self.dc.world,
                };
            }
            EMR_SAVEDC => self.saved.push(self.dc.clone()),
            EMR_RESTOREDC => {
                let n = r.i32(8)?;
                let keep = if n < 0 {
                    self.saved.len().checked_sub(n.unsigned_abs() as usize)?
                } else {
                    (n as usize).checked_sub(1)?
                };
                if keep < self.saved.len() {
                    self.saved.truncate(keep + 1);
                    self.dc = self.saved.pop()?;
                }
            }
            EMR_CREATEPEN => {
                let style = r.u32(12)?;
                let width = f64::from(r.i32(16)?);
                let color = (style & 0xF != 5).then(|| colorref(r.u32(24).unwrap_or(0)));
                let pen = Pen::simple(color, (width > 0.0).then_some(width));
                self.objects.insert(r.u32(8)?, GdiObj::Pen(pen));
            }
            EMR_EXTCREATEPEN => {
                let style = r.u32(28)?;
                let width = f64::from(r.u32(32)?);
                let brush_null = r.u32(36)? == 1;
                let geometric = style & 0x0001_0000 != 0;
                let color = (style & 0xF != 5 && !brush_null).then(|| colorref(r.u32(40).unwrap_or(0)));
                let cap = match style & 0x0F00 {
                    0x0100 => LineCap::Square, // PS_ENDCAP_SQUARE
                    0x0200 => LineCap::Butt,   // PS_ENDCAP_FLAT
                    _ => LineCap::Round,
                };
                let join = match style & 0xF000 {
                    0x1000 => LineJoin::Bevel, // PS_JOIN_BEVEL
                    0x2000 => LineJoin::Miter, // PS_JOIN_MITER
                    _ => LineJoin::Round,
                };
                let pen = Pen { color, width: (geometric && width > 0.0).then_some(width), join, cap };
                self.objects.insert(r.u32(8)?, GdiObj::Pen(pen));
            }
            EMR_SETMITERLIMIT => {
                // Specified as an integer, but some writers store a float.
                let v = r.u32(8)?;
                let limit = if v > 1000 { f64::from(f32::from_bits(v)) } else { f64::from(v) };
                if limit.is_finite() && limit >= 1.0 {
                    self.dc.miter_limit = limit;
                }
            }
            EMR_INTERSECTCLIPRECT | EMR_EXCLUDECLIPRECT => {
                let b = [f64::from(r.i32(8)?), f64::from(r.i32(12)?), f64::from(r.i32(16)?), f64::from(r.i32(20)?)];
                let rect = self.rect_mm(b);
                if kind == EMR_INTERSECTCLIPRECT {
                    self.combine_clip(rect, 1);
                } else if let Some(c) = self.dc.clip.take() {
                    self.dc.clip = Some(c.difference(&rect));
                }
            }
            EMR_EXTSELECTCLIPRGN => {
                let cb = r.u32(8)? as usize;
                let mode = r.u32(12)?;
                if cb < 32 {
                    if mode == 5 {
                        self.dc.clip = None; // RGN_COPY with no region: clipping off
                    }
                } else {
                    // RGNDATA: header (32 bytes) then nCount device-unit rectangles.
                    let n = r.u32(24)? as usize;
                    if n > (cb - 32) / 16 {
                        return None;
                    }
                    let mut region = MultiPolygon::new(vec![]);
                    for i in 0..n {
                        let o = 48 + i * 16;
                        let d = |k: usize, axis: usize| r.i32(o + k * 4).map(|v| f64::from(v) * self.mm_per_dev[axis]);
                        let (l, t, rr, bb) = (d(0, 0)?, d(1, 1)?, d(2, 0)?, d(3, 1)?);
                        let poly = vec![[l, t], [rr, t], [rr, bb], [l, bb]];
                        region = region.union(&fill_region(&[&poly], true));
                    }
                    self.combine_clip(region, mode);
                }
            }
            EMR_CREATEBRUSHINDIRECT => {
                let color = (r.u32(12)? != 1).then(|| colorref(r.u32(16).unwrap_or(0)));
                self.objects.insert(r.u32(8)?, GdiObj::Brush(color));
            }
            EMR_DELETEOBJECT => {
                self.objects.remove(&r.u32(8)?);
            }
            EMR_SELECTOBJECT => {
                let h = r.u32(8)?;
                let obj = if h & 0x8000_0000 != 0 {
                    stock_object(h & 0x7FFF_FFFF)
                } else {
                    self.objects.get(&h).copied()
                };
                match obj {
                    Some(GdiObj::Pen(p)) => self.dc.pen = p,
                    Some(GdiObj::Brush(b)) => self.dc.brush = b,
                    None => {}
                }
            }
            EMR_MOVETOEX => {
                let p = r.point(8, false)?;
                if self.in_path {
                    self.finish_figure(false);
                }
                self.dc.pos = p;
            }
            EMR_LINETO => self.line_to(r.point(8, false)?),
            EMR_POLYLINETO | EMR_POLYLINETO16 => {
                let short = kind == EMR_POLYLINETO16;
                let n = r.u32(24)? as usize;
                for p in r.points(28, n, short)? {
                    self.line_to(p);
                }
            }
            EMR_POLYBEZIERTO | EMR_POLYBEZIERTO16 => {
                let short = kind == EMR_POLYBEZIERTO16;
                let n = r.u32(24)? as usize;
                let pts = r.points(28, n, short)?;
                self.bezier_to(&pts);
            }
            EMR_POLYGON | EMR_POLYGON16 => {
                let short = kind == EMR_POLYGON16;
                let n = r.u32(24)? as usize;
                let pts = r.points(28, n, short)?;
                let fig = self.figure_mm(&pts);
                self.closed_shapes(vec![fig]);
            }
            EMR_POLYLINE | EMR_POLYLINE16 => {
                let short = kind == EMR_POLYLINE16;
                let n = r.u32(24)? as usize;
                let pts = r.points(28, n, short)?;
                let fig = self.figure_mm(&pts);
                self.open_shapes(vec![fig]);
            }
            EMR_POLYBEZIER | EMR_POLYBEZIER16 => {
                let short = kind == EMR_POLYBEZIER16;
                let n = r.u32(24)? as usize;
                let pts = r.points(28, n, short)?;
                let fig = self.bezier_figure(&pts);
                // A curve that ends where it starts is a closed outline.
                let closed = pts.len() > 3 && pts.first() == pts.last();
                if closed { self.closed_shapes(vec![fig]) } else { self.open_shapes(vec![fig]) }
            }
            EMR_POLYPOLYGON | EMR_POLYPOLYGON16 | EMR_POLYPOLYLINE | EMR_POLYPOLYLINE16 => {
                let short = matches!(kind, EMR_POLYPOLYGON16 | EMR_POLYPOLYLINE16);
                let n_polys = r.u32(24)? as usize;
                let total = r.u32(28)? as usize;
                if n_polys > r.0.len() / 4 {
                    return None;
                }
                let counts: Vec<usize> = (0..n_polys).map(|i| r.u32(32 + i * 4).map(|c| c as usize)).collect::<Option<_>>()?;
                let pts = r.points(32 + n_polys * 4, total, short)?;
                let mut figs = Vec::with_capacity(n_polys);
                let mut i = 0usize;
                for c in counts {
                    let part = pts.get(i..i.checked_add(c)?)?;
                    figs.push(self.figure_mm(part));
                    i += c;
                }
                if matches!(kind, EMR_POLYPOLYGON | EMR_POLYPOLYGON16) {
                    if self.in_path {
                        self.closed_shapes(figs);
                    } else {
                        // One shape: the polygons are rings of a single area (holes!).
                        let figs = figs.into_iter().map(|f| (f, true)).collect();
                        self.emit(figs, true, true);
                    }
                } else {
                    self.open_shapes(figs);
                }
            }
            EMR_RECTANGLE | EMR_ELLIPSE | EMR_ROUNDRECT => {
                let b = [
                    f64::from(r.i32(8)?),
                    f64::from(r.i32(12)?),
                    f64::from(r.i32(16)?),
                    f64::from(r.i32(20)?),
                ];
                let fig = match kind {
                    EMR_ELLIPSE => self.ellipse_figure(b),
                    EMR_ROUNDRECT => {
                        let (w, h) = (f64::from(r.i32(24)?), f64::from(r.i32(28)?));
                        self.rounded_figure(b, w / 2.0, h / 2.0)
                    }
                    _ => self.rounded_figure(b, 0.0, 0.0),
                };
                self.closed_shapes(vec![fig]);
            }
            EMR_BEGINPATH => {
                self.in_path = true;
                self.figures.clear();
                self.cur.clear();
            }
            EMR_ENDPATH => {
                self.finish_figure(false);
                self.in_path = false;
            }
            EMR_CLOSEFIGURE => self.finish_figure(true),
            EMR_SELECTCLIPPATH => {
                // The current path becomes (part of) the clip region.
                self.in_path = false;
                let figs = self.take_path();
                let rings: Vec<&Vec<[f64; 2]>> = figs.iter().filter(|(p, _)| p.len() >= 3).map(|(p, _)| p).collect();
                let region = fill_region(&rings, self.dc.even_odd);
                self.combine_clip(region, r.u32(8)?);
            }
            EMR_ABORTPATH => {
                self.in_path = false;
                self.figures.clear();
                self.cur.clear();
            }
            EMR_FILLPATH | EMR_STROKEANDFILLPATH | EMR_STROKEPATH => {
                self.in_path = false;
                let figs = self.take_path();
                self.emit(figs, kind != EMR_STROKEPATH, kind != EMR_FILLPATH);
            }
            EMR_BITBLT | EMR_STRETCHBLT | EMR_STRETCHDIBITS | EMR_EXTTEXTOUTA | EMR_EXTTEXTOUTW => {
                self.report.skipped += 1;
            }
            _ => {}
        }
        Some(())
    }
}

/// GDI stock objects (index without the 0x80000000 flag).
fn stock_object(i: u32) -> Option<GdiObj> {
    let gray = |v: u8| Some(Color::rgb(v, v, v));
    Some(match i {
        0 => GdiObj::Brush(gray(255)),
        1 => GdiObj::Brush(gray(0xC0)),
        2 => GdiObj::Brush(gray(0x80)),
        3 => GdiObj::Brush(gray(0x40)),
        4 => GdiObj::Brush(gray(0)),
        5 => GdiObj::Brush(None),
        6 => GdiObj::Pen(Pen::simple(gray(255), None)),
        7 => GdiObj::Pen(Pen::simple(gray(0), None)),
        8 => GdiObj::Pen(Pen::simple(None, None)),
        _ => return None,
    })
}

/// Millimetres per device unit from the EMF header.
fn header_scale(h: &Rec) -> [f64; 2] {
    let dev = [h.i32(72).unwrap_or(0), h.i32(76).unwrap_or(0)];
    let size = h.u32(4).unwrap_or(0) as usize;
    // szlMicrometers (header extension 2) is more precise than szlMillimeters.
    let um = if size >= 108 { [h.i32(100).unwrap_or(0), h.i32(104).unwrap_or(0)] } else { [0, 0] };
    let mm = [h.i32(80).unwrap_or(0), h.i32(84).unwrap_or(0)];
    let axis = |i: usize| {
        if dev[i] > 0 && um[i] > 0 {
            f64::from(um[i]) / 1000.0 / f64::from(dev[i])
        } else if dev[i] > 0 && mm[i] > 0 {
            f64::from(mm[i]) / f64::from(dev[i])
        } else {
            25.4 / 96.0
        }
    };
    let scale = [axis(0), axis(1)];

    // Sanity check against the picture frame (in 0.01 mm) and the drawing's
    // bounds (device units). Normally they agree with the reference-device
    // scale; programs that are not DPI-aware can record a header for the
    // physical display while drawing in scaled pixels. If the two disagree by
    // more than 1.5× (consistently on both axes), trust frame ÷ bounds.
    let r = |o: usize| f64::from(h.i32(o).unwrap_or(0));
    let frame_mm = [(r(32) - r(24)) / 100.0, (r(36) - r(28)) / 100.0];
    let bounds_dev = [r(16) - r(8) + 1.0, r(20) - r(12) + 1.0];
    if frame_mm.iter().chain(&bounds_dev).all(|v| v.is_finite() && *v > 1.0) {
        let fs = [frame_mm[0] / bounds_dev[0], frame_mm[1] / bounds_dev[1]];
        let ratio = [fs[0] / scale[0], fs[1] / scale[1]];
        let off = |q: f64| !(1.0 / 1.5..=1.5).contains(&q);
        if off(ratio[0]) && off(ratio[1]) && (ratio[0] / ratio[1] - 1.0).abs() < 0.2 {
            return fs;
        }
    }
    scale
}

/// Parse EMF bytes into shapes. `tol` is the curve flattening tolerance in mm.
pub fn import_emf(data: &[u8], tol: f64) -> Result<ImportReport, String> {
    let header = Rec(data);
    if header.u32(0) != Some(EMR_HEADER) || header.u32(40) != Some(EMF_SIGNATURE) {
        return Err("Not an Enhanced Metafile.".into());
    }
    let mut p = Parser {
        tol: if tol.is_finite() && tol > 0.0 { tol } else { 0.01 },
        mm_per_dev: header_scale(&header),
        dc: Dc::default(),
        saved: Vec::new(),
        objects: Default::default(),
        in_path: false,
        figures: Vec::new(),
        cur: Vec::new(),
        report: ImportReport::default(),
    };
    let mut off = 0usize;
    while off + 8 <= data.len() {
        let rec = Rec(&data[off..]);
        let (Some(kind), Some(size)) = (rec.u32(0), rec.u32(4)) else { break };
        let size = size as usize;
        if size < 8 || !size.is_multiple_of(4) || off + size > data.len() {
            break; // malformed: keep what was read so far
        }
        if kind == EMR_EOF {
            break;
        }
        // A malformed record is skipped rather than aborting the import.
        let _ = p.record(kind, &Rec(&data[off..off + size]));
        off += size;
    }
    if p.report.shapes == 0 {
        return Err("The metafile contains no filled or closed vector shapes.".into());
    }
    Ok(p.report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal EMF writer for tests.
    struct Emf(Vec<u8>);

    impl Emf {
        /// Reference device: 1000 × 1000 units = 100 × 100 mm (0.1 mm per unit).
        fn new() -> Self {
            let mut h = vec![0u8; 108];
            let put = |h: &mut Vec<u8>, off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
            put(&mut h, 0, EMR_HEADER);
            put(&mut h, 4, 108);
            put(&mut h, 40, EMF_SIGNATURE);
            put(&mut h, 44, 0x10000);
            put(&mut h, 72, 1000);
            put(&mut h, 76, 1000);
            put(&mut h, 80, 100);
            put(&mut h, 84, 100);
            put(&mut h, 100, 100_000);
            put(&mut h, 104, 100_000);
            Emf(h)
        }
        fn rec(&mut self, kind: u32, body: &[u32]) -> &mut Self {
            self.0.extend_from_slice(&kind.to_le_bytes());
            self.0.extend_from_slice(&((8 + body.len() * 4) as u32).to_le_bytes());
            for v in body {
                self.0.extend_from_slice(&v.to_le_bytes());
            }
            self
        }
        fn finish(&mut self) -> Vec<u8> {
            self.rec(EMR_EOF, &[0, 0, 0]);
            self.0.clone()
        }
    }

    fn i(v: i32) -> u32 {
        v as u32
    }
    fn pts16(p: &[(i16, i16)]) -> Vec<u32> {
        p.iter().map(|&(x, y)| (x as u16 as u32) | ((y as u16 as u32) << 16)).collect()
    }
    fn only(rep: &ImportReport) -> &ShapeObj {
        match &rep.nodes[..] {
            [ImportNode::Shape(_, s)] => s,
            other => panic!("expected one shape, got {other:?}"),
        }
    }

    #[test]
    fn rectangle_with_brush_and_pen_keeps_size_and_style() {
        let data = Emf::new()
            .rec(EMR_CREATEBRUSHINDIRECT, &[1, 0, 0x0000_00FF, 0]) // red
            .rec(EMR_SELECTOBJECT, &[1])
            .rec(EMR_CREATEPEN, &[2, 0, 10, 0, 0x00FF_0000]) // blue, 10 units = 1 mm
            .rec(EMR_SELECTOBJECT, &[2])
            .rec(EMR_RECTANGLE, &[100, 200, 600, 500])
            .finish();
        let rep = import_emf(&data, 0.01).unwrap();
        let s = only(&rep);
        assert!((s.xf.w - 50.0).abs() < 1e-9 && (s.xf.h - 30.0).abs() < 1e-9);
        assert!((s.xf.cx - 35.0).abs() < 1e-9 && (s.xf.cy - 35.0).abs() < 1e-9);
        assert_eq!(s.style.fill, Some(Color::rgb(255, 0, 0)));
        assert_eq!(s.style.stroke, Some(Color::rgb(0, 0, 255)));
        assert!((s.style.stroke_width - 1.0).abs() < 1e-9);
    }

    #[test]
    fn anisotropic_mapping_like_coreldraw() {
        // Logical units of 0.01 mm mapped onto the 0.1 mm reference device.
        let mut body = vec![0, 0, 0, 0, 4];
        body.extend(pts16(&[(0, 0), (5000, 0), (5000, 2000), (0, 2000)]));
        let data = Emf::new()
            .rec(EMR_SETMAPMODE, &[8])
            .rec(EMR_SETWINDOWEXTEX, &[1000, 1000])
            .rec(EMR_SETVIEWPORTEXTEX, &[100, 100])
            .rec(EMR_SELECTOBJECT, &[0x8000_0004]) // black brush
            .rec(EMR_SELECTOBJECT, &[0x8000_0008]) // null pen
            .rec(EMR_POLYGON16, &body)
            .finish();
        let s = only(&import_emf(&data, 0.01).unwrap()).clone();
        assert!((s.xf.w - 50.0).abs() < 1e-9 && (s.xf.h - 20.0).abs() < 1e-9, "{} × {}", s.xf.w, s.xf.h);
        assert_eq!(s.style.stroke, None);
        assert_eq!(s.style.fill, Some(Color::rgb(0, 0, 0)));
    }

    #[test]
    fn polypolygon_makes_holes() {
        let mut body = vec![0, 0, 0, 0, 2, 8, 4, 4];
        body.extend(pts16(&[(0, 0), (600, 0), (600, 600), (0, 600), (200, 200), (400, 200), (400, 400), (200, 400)]));
        let data = Emf::new().rec(EMR_POLYPOLYGON16, &body).finish();
        let g = only(&import_emf(&data, 0.01).unwrap()).world_geometry(0.01);
        assert!((g.unsigned_area() - (3600.0 - 400.0)).abs() < 1e-6);
    }

    #[test]
    fn path_with_beziers_and_world_transform() {
        // Circle of radius 100 units (10 mm) drawn as 4 Béziers inside a path,
        // scaled ×2 by the world transform → radius 20 mm.
        let k = 55; // ≈ 0.5523 · 100
        let mut bez = vec![0, 0, 0, 0, 12];
        bez.extend(pts16(&[
            (100, k), (k, 100), (0, 100),
            (-k, 100), (-100, k), (-100, 0),
            (-100, -k), (-k, -100), (0, -100),
            (k, -100), (100, -k), (100, 0),
        ]));
        let two = 2.0f32.to_bits();
        let data = Emf::new()
            .rec(EMR_SETWORLDTRANSFORM, &[two, 0, 0, two, 0, 0])
            .rec(EMR_BEGINPATH, &[])
            .rec(EMR_MOVETOEX, &[100, 0])
            .rec(EMR_POLYBEZIERTO16, &bez)
            .rec(EMR_CLOSEFIGURE, &[])
            .rec(EMR_ENDPATH, &[])
            .rec(EMR_FILLPATH, &[0, 0, 0, 0])
            .finish();
        let s = only(&import_emf(&data, 0.01).unwrap()).clone();
        assert!((s.xf.w - 40.0).abs() < 0.05 && (s.xf.h - 40.0).abs() < 0.05, "{} × {}", s.xf.w, s.xf.h);
        let area = s.world_geometry(0.01).unsigned_area();
        let exact = std::f64::consts::PI * 400.0;
        assert!((area - exact).abs() / exact < 0.01, "area {area}");
        assert_eq!(s.style.stroke, None, "FillPath does not stroke");
    }

    #[test]
    fn save_restore_dc_and_lines_outlined() {
        let mut line = vec![0, 0, 0, 0, 2];
        line.extend(pts16(&[(0, 0), (100, 0)]));
        let data = Emf::new()
            .rec(EMR_SAVEDC, &[])
            .rec(EMR_SELECTOBJECT, &[0x8000_0005]) // null brush
            .rec(EMR_RESTOREDC, &[i(-1)]) // back to the white default brush
            // Geometric pen, 20 units = 2 mm, flat caps, black.
            .rec(EMR_EXTCREATEPEN, &[3, 0, 0, 0, 0, 0x0001_0200, 20, 0, 0, 0, 0])
            .rec(EMR_SELECTOBJECT, &[3])
            .rec(EMR_POLYLINE16, &line)
            .rec(EMR_SELECTOBJECT, &[0x8000_0007]) // black cosmetic pen
            .rec(EMR_ELLIPSE, &[0, 0, 200, 100])
            .finish();
        let rep = import_emf(&data, 0.01).unwrap();
        assert_eq!(rep.shapes, 2);
        assert_eq!(rep.skipped, 0);
        let ImportNode::Shape(_, line) = &rep.nodes[0] else { panic!() };
        assert_eq!(line.style.fill, Some(Color::rgb(0, 0, 0)), "line outlined into a filled area");
        assert!((line.world_geometry(0.01).unsigned_area() - 20.0).abs() < 1e-6, "10 mm × 2 mm, flat caps");
        let ImportNode::Shape(_, s) = &rep.nodes[1] else { panic!() };
        assert_eq!(s.style.fill, Some(Color::rgb(255, 255, 255)));
        assert!((s.xf.w - 20.0).abs() < 1e-6 && (s.xf.h - 10.0).abs() < 1e-6);
    }

    #[test]
    fn clip_rect_and_clip_path_cut_shapes() {
        let data = Emf::new()
            .rec(EMR_SELECTOBJECT, &[0x8000_0008]) // null pen
            .rec(EMR_SAVEDC, &[])
            .rec(EMR_INTERSECTCLIPRECT, &[0, 0, 100, 100]) // 10 × 10 mm
            .rec(EMR_RECTANGLE, &[50, 50, 300, 300])
            .rec(EMR_RESTOREDC, &[i(-1)]) // clip gone again
            .rec(EMR_RECTANGLE, &[400, 0, 500, 100])
            .finish();
        let rep = import_emf(&data, 0.01).unwrap();
        assert_eq!(rep.shapes, 2);
        let ImportNode::Shape(_, a) = &rep.nodes[0] else { panic!() };
        assert!((a.world_geometry(0.01).unsigned_area() - 25.0).abs() < 1e-6, "5 × 5 mm left after clipping");
        let ImportNode::Shape(_, b) = &rep.nodes[1] else { panic!() };
        assert!((b.world_geometry(0.01).unsigned_area() - 100.0).abs() < 1e-6, "unclipped after RestoreDC");
    }

    /// A real metafile recorded by GDI+ in a non-DPI-aware process on a 4K
    /// screen: the header claims 3840 px = 344 mm, but the shapes were drawn in
    /// scaled pixels. The frame/bounds check recovers a plausible size (the
    /// red rectangle is 40 × 20 mm; frame margins leave some error).
    #[test]
    fn real_gdiplus_metafile_with_inconsistent_header() {
        let data = include_bytes!("../tests/data/gdiplus_dpi_unaware.emf");
        let rep = import_emf(data, 0.01).unwrap();
        assert_eq!(rep.shapes, 4, "rectangle, circle, frame fill, frame outline");
        let shapes: Vec<&ShapeObj> = rep
            .nodes
            .iter()
            .map(|n| match n {
                ImportNode::Shape(_, s) => s.as_ref(),
                ImportNode::Group(..) => panic!("no groups expected"),
            })
            .collect();
        let red = shapes.iter().find(|s| s.style.fill == Some(Color::rgb(220, 30, 30))).unwrap();
        assert!((red.xf.w / red.xf.h - 2.0).abs() < 0.05, "aspect kept");
        assert!((red.xf.w - 40.0).abs() < 8.0, "width {}", red.xf.w);
        // The orange frame keeps its hole (FillMode.Alternate).
        let frame = shapes.iter().find(|s| s.style.fill == Some(Color::rgb(240, 160, 20))).unwrap();
        assert_eq!(frame.world_geometry(0.01).0[0].interiors().len(), 1);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(import_emf(b"", 0.01).is_err());
        assert!(import_emf(&[0u8; 200], 0.01).is_err());
        let mut d = Emf::new().rec(EMR_POLYGON16, &[0, 0, 0, 0, 0xFFFF_FFFF]).finish();
        assert!(import_emf(&d, 0.01).is_err());
        d.truncate(120);
        assert!(import_emf(&d, 0.01).is_err());
    }
}
