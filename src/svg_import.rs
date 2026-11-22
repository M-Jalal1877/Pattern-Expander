//! SVG import.
//!
//! `usvg` resolves CSS, `<use>`, transforms, units and the viewBox, and turns
//! every basic shape into a path. Each path's curves are flattened to the document
//! tolerance and the result becomes an editable custom shape, in millimetres.
//!
//! Units: absolute units (`mm`, `in`, `pt`, …) are always exact, so an SVG
//! written as `width="50mm" viewBox="0 0 50 50"` imports at exactly 50 mm and
//! files exported by this app round-trip at their true size. Unitless / `px`
//! values are CSS pixels, 96 per inch — except in SVG written by Adobe
//! Illustrator, whose pixels are points (72 per inch); that is detected from the
//! generator comment (and, for clipboard data, from Illustrator's clipboard formats).

use crate::document::{Document, ObjId, Object, ObjectKind, ShapeObj, SpecialPaint};
use crate::flatten::outline_stroke;
use crate::shapes::{Color, LineCap, LineJoin, Style, custom_from_world};
use geo::{Area, BooleanOps, Coord, LineString, MultiPolygon, Polygon};

/// CSS / browser / Inkscape pixels per inch.
pub const CSS_DPI: f64 = 96.0;
/// Adobe Illustrator writes SVG with 1 px = 1 pt.
pub const ILLUSTRATOR_DPI: f64 = 72.0;

/// State inherited down the group tree.
#[derive(Clone)]
struct Ctx {
    tol: f64,
    mm_per_px: f64,
    /// Product of the ancestor groups' opacities.
    opacity: f64,
    /// Paint the app cannot reproduce exactly (blend mode, mask) from an ancestor.
    special: Option<SpecialPaint>,
    /// Intersection of the ancestor groups' clip paths, in mm.
    clip: Option<MultiPolygon<f64>>,
}
/// Colour used for gradient / pattern paints, which have no single colour.
const FALLBACK_FILL: Color = Color { r: 0x9e, g: 0x9e, b: 0x9e, a: 255 };

/// An imported item, before it gets document ids.
#[derive(Debug)]
pub enum ImportNode {
    Shape(String, Box<ShapeObj>),
    Group(String, Vec<ImportNode>),
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub nodes: Vec<ImportNode>,
    pub shapes: usize,
    /// Elements that could not be converted to shapes (text, images).
    pub skipped: usize,
}

/// Parse SVG data into shapes. `tol` is the curve flattening tolerance in mm.
pub fn import_svg(data: &[u8], tol: f64) -> Result<ImportReport, String> {
    let dpi = if is_illustrator_svg(data) { ILLUSTRATOR_DPI } else { CSS_DPI };
    import_svg_at_dpi(data, tol, dpi)
}

/// True if the SVG says it was written by Adobe Illustrator.
pub fn is_illustrator_svg(data: &[u8]) -> bool {
    let head = &data[..data.len().min(4096)];
    head.windows(17).any(|w| w == b"Adobe Illustrator")
}

/// Parse SVG data, reading unitless / px values at `dpi` pixels per inch.
pub fn import_svg_at_dpi(data: &[u8], tol: f64, dpi: f64) -> Result<ImportReport, String> {
    let dpi = if dpi.is_finite() && dpi > 1.0 { dpi } else { CSS_DPI };
    let opt = usvg::Options {
        dpi: dpi as f32,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(data, &opt).map_err(|e| format!("Invalid SVG: {e}"))?;
    let ctx = Ctx {
        tol: if tol.is_finite() && tol > 0.0 { tol } else { 0.01 },
        mm_per_px: 25.4 / dpi,
        opacity: 1.0,
        special: None,
        clip: None,
    };
    let mut report = ImportReport::default();
    let mut counter = 0;
    report.nodes = convert_group(tree.root(), &ctx, &mut report, &mut counter);
    if report.shapes == 0 {
        return Err("The SVG contains no filled or closed shapes.".into());
    }
    Ok(report)
}

fn convert_group(g: &usvg::Group, ctx: &Ctx, rep: &mut ImportReport, counter: &mut usize) -> Vec<ImportNode> {
    let mut out = Vec::new();
    for node in g.children() {
        match node {
            usvg::Node::Group(child) => {
                let mut cctx = ctx.clone();
                cctx.opacity *= f64::from(child.opacity().get());
                if child.blend_mode() != usvg::BlendMode::Normal {
                    cctx.special = Some(SpecialPaint::BlendMode);
                }
                if child.mask().is_some() {
                    cctx.special = Some(SpecialPaint::Mask);
                }
                // Clipping mask: apply the clip to everything inside; the clip
                // shape itself is not imported.
                if let Some(cp) = child.clip_path() {
                    let region = clip_region(cp, child.abs_transform(), &cctx);
                    cctx.clip = Some(match cctx.clip.take() {
                        Some(c) => c.intersection(&region),
                        None => region,
                    });
                }
                let mut items = convert_group(child, &cctx, rep, counter);
                // Keep only groups the author named; usvg adds anonymous ones for transforms.
                if child.id().is_empty() || items.len() < 2 {
                    out.append(&mut items);
                } else {
                    out.push(ImportNode::Group(child.id().to_string(), items));
                }
            }
            usvg::Node::Path(p) => {
                let shapes = convert_path(p, ctx);
                let base = if p.id().is_empty() {
                    *counter += 1;
                    format!("SVG path {counter}")
                } else {
                    p.id().to_string()
                };
                let n = shapes.len();
                for (shape, part) in shapes {
                    rep.shapes += 1;
                    let name = if n > 1 { format!("{base} {part}") } else { base.clone() };
                    out.push(ImportNode::Shape(name, Box::new(shape)));
                }
            }
            // Images and (with text support compiled out) text have no vector area.
            _ => rep.skipped += 1,
        }
    }
    out
}

/// Colour of a paint, and whether it is a gradient / pattern (approximated).
fn paint_color(paint: &usvg::Paint, opacity: f32) -> (Color, bool) {
    let (mut c, approx) = match paint {
        usvg::Paint::Color(c) => (Color { r: c.red, g: c.green, b: c.blue, a: 255 }, false),
        _ => (FALLBACK_FILL, true),
    };
    c.a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    (c, approx)
}

/// Maps usvg user-space points through `ts` into millimetres.
fn mapper(ts: usvg::Transform, k: f64) -> impl Fn(usvg::tiny_skia_path::Point) -> [f64; 2] {
    move |pt| {
        let (x, y) = (f64::from(pt.x), f64::from(pt.y));
        [
            (f64::from(ts.sx) * x + f64::from(ts.kx) * y + f64::from(ts.tx)) * k,
            (f64::from(ts.ky) * x + f64::from(ts.sy) * y + f64::from(ts.ty)) * k,
        ]
    }
}

/// The area of a clip path, in mm. `base` is the absolute transform of the
/// element that references it (clip paths live in their own subtree).
fn clip_region(cp: &usvg::ClipPath, base: usvg::Transform, ctx: &Ctx) -> MultiPolygon<f64> {
    fn collect(g: &usvg::Group, t: usvg::Transform, ctx: &Ctx, acc: &mut MultiPolygon<f64>) {
        for node in g.children() {
            match node {
                usvg::Node::Group(c) => collect(c, t, ctx, acc),
                usvg::Node::Path(p) => {
                    let map = mapper(t.pre_concat(p.abs_transform()), ctx.mm_per_px);
                    let subpaths = flatten(p.data(), &map, ctx.tol);
                    let rings: Vec<&Vec<[f64; 2]>> = subpaths.iter().filter(|(s, _)| s.len() >= 3).map(|(s, _)| s).collect();
                    // usvg stores clip-rule as the fill rule of clip children.
                    let even_odd = p.fill().is_some_and(|f| f.rule() == usvg::FillRule::EvenOdd);
                    let r = fill_region(&rings, even_odd);
                    *acc = if acc.0.is_empty() { r } else { acc.union(&r) };
                }
                _ => {}
            }
        }
    }
    let t = base.pre_concat(cp.transform());
    let mut region = MultiPolygon::new(vec![]);
    collect(cp.root(), t, ctx, &mut region);
    if let Some(inner) = cp.clip_path() {
        region = region.intersection(&clip_region(inner, base, ctx));
    }
    region
}

fn line_join(j: usvg::LineJoin) -> LineJoin {
    match j {
        usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => LineJoin::Miter,
        usvg::LineJoin::Round => LineJoin::Round,
        usvg::LineJoin::Bevel => LineJoin::Bevel,
    }
}

fn line_cap(c: usvg::LineCap) -> LineCap {
    match c {
        usvg::LineCap::Butt => LineCap::Butt,
        usvg::LineCap::Round => LineCap::Round,
        usvg::LineCap::Square => LineCap::Square,
    }
}

fn shape_from(region: MultiPolygon<f64>, style: Style, special: Option<SpecialPaint>) -> Option<ShapeObj> {
    if region.unsigned_area() <= 1e-12 {
        return None;
    }
    let (geom, xf) = custom_from_world(&region)?;
    let mut s = ShapeObj::new(geom, xf, style);
    s.special = special;
    Some(s)
}

/// One SVG path → shapes, each with a label ("fill" / "stroke") for naming.
///
/// Normally a single shape that keeps its stroke as an editable style. When the
/// stroke cannot be drawn along the filled region's outline — open subpaths
/// (lines) or a clipping mask cutting through it — the stroke is outlined into a
/// separate filled shape instead, so what you see stays exactly the same.
fn convert_path(p: &usvg::Path, ctx: &Ctx) -> Vec<(ShapeObj, &'static str)> {
    if !p.is_visible() {
        return vec![];
    }
    let ts = p.abs_transform();
    let k = ctx.mm_per_px;
    let subpaths = flatten(p.data(), &mapper(ts, k), ctx.tol);
    let scale = (f64::from(ts.sx) * f64::from(ts.sy) - f64::from(ts.kx) * f64::from(ts.ky)).abs().sqrt();

    let mut special = ctx.special;
    let fill = p.fill().map(|f| {
        let (c, approx) = paint_color(f.paint(), f.opacity().get());
        if approx {
            special.get_or_insert(SpecialPaint::Gradient);
        }
        (c.with_opacity(ctx.opacity), f.rule() == usvg::FillRule::EvenOdd)
    });
    let stroke = p.stroke().map(|s| {
        let (c, approx) = paint_color(s.paint(), s.opacity().get());
        if approx {
            special.get_or_insert(SpecialPaint::Gradient);
        }
        Style {
            fill: None,
            stroke: Some(c.with_opacity(ctx.opacity)),
            stroke_width: f64::from(s.width().get()) * scale * k,
            line_join: line_join(s.linejoin()),
            line_cap: line_cap(s.linecap()),
            miter_limit: f64::from(s.miterlimit().get()),
        }
    });
    let clip = |mp: MultiPolygon<f64>| match &ctx.clip {
        Some(c) => mp.intersection(c),
        None => mp,
    };
    let even_odd = fill.is_some_and(|f| f.1);
    let any_open = subpaths.iter().any(|(_, closed)| !closed);
    let separate_stroke = stroke.is_some() && (ctx.clip.is_some() || any_open);

    let mut out = Vec::new();
    if !separate_stroke {
        // Fill closes every subpath; a stroke alone only encloses closed ones.
        let rings: Vec<&Vec<[f64; 2]>> = subpaths
            .iter()
            .filter(|(pts, closed)| pts.len() >= 3 && (fill.is_some() || *closed))
            .map(|(pts, _)| pts)
            .collect();
        let mut style = stroke.clone().unwrap_or(Style { stroke: None, stroke_width: 0.0, ..Style::default() });
        style.fill = fill.map(|f| f.0);
        if let Some(s) = shape_from(clip(fill_region(&rings, even_odd)), style, special) {
            out.push((s, "fill"));
        }
        return out;
    }
    if let Some((c, _)) = fill {
        let rings: Vec<&Vec<[f64; 2]>> = subpaths.iter().filter(|(pts, _)| pts.len() >= 3).map(|(pts, _)| pts).collect();
        if let Some(s) = shape_from(clip(fill_region(&rings, even_odd)), Style::fill_only(c), special) {
            out.push((s, "fill"));
        }
    }
    if let Some(st) = stroke
        && let Some(color) = st.stroke
    {
        let area = outline_stroke(&subpaths, st.stroke_width, st.line_join, st.line_cap, st.miter_limit, ctx.tol);
        if let Some(s) = shape_from(clip(area), Style::fill_only(color), special) {
            // paint-order="stroke" draws the stroke below the fill.
            if p.paint_order() == usvg::PaintOrder::StrokeAndFill {
                out.insert(0, (s, "stroke"));
            } else {
                out.push((s, "stroke"));
            }
        }
    }
    out
}

/// Flatten a path into polylines (in mm). Returns (points, explicitly closed).
fn flatten(
    path: &usvg::tiny_skia_path::Path,
    map: &dyn Fn(usvg::tiny_skia_path::Point) -> [f64; 2],
    tol: f64,
) -> Vec<(Vec<[f64; 2]>, bool)> {
    use usvg::tiny_skia_path::PathSegment as S;
    let mut out = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    let finish = |cur: &mut Vec<[f64; 2]>, closed: bool, out: &mut Vec<(Vec<[f64; 2]>, bool)>| {
        if cur.len() > 1 && cur.first() == cur.last() {
            cur.pop();
        }
        if !cur.is_empty() {
            out.push((std::mem::take(cur), closed));
        }
    };
    for seg in path.segments() {
        match seg {
            S::MoveTo(p) => {
                finish(&mut cur, false, &mut out);
                cur.push(map(p));
            }
            S::LineTo(p) => cur.push(map(p)),
            S::QuadTo(p1, p2) => {
                let p0 = cur.last().copied().unwrap_or_else(|| map(p1));
                let (p1, p2) = (map(p1), map(p2));
                let d = len(sub(add(p0, p2), scale(p1, 2.0)));
                let n = segments(d / 4.0, tol);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    cur.push(add(add(scale(p0, u * u), scale(p1, 2.0 * u * t)), scale(p2, t * t)));
                }
            }
            S::CubicTo(p1, p2, p3) => {
                let p0 = cur.last().copied().unwrap_or_else(|| map(p1));
                push_cubic(&mut cur, p0, map(p1), map(p2), map(p3), tol);
            }
            S::Close => {
                let start = cur.first().copied();
                finish(&mut cur, true, &mut out);
                // A segment after Z without a MoveTo starts at the subpath's start.
                if let Some(s) = start {
                    cur.push(s);
                }
            }
        }
    }
    finish(&mut cur, false, &mut out);
    // Drop the lone restart points left behind by Close.
    out.retain(|(pts, _)| pts.len() > 1);
    out
}

/// Append a flattened cubic Bézier (without its start point `p0`) to `out`.
pub(crate) fn push_cubic(out: &mut Vec<[f64; 2]>, p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2], tol: f64) {
    let d = len(sub(add(p0, p2), scale(p1, 2.0))).max(len(sub(add(p1, p3), scale(p2, 2.0))));
    let n = segments(d * 0.75, tol);
    for i in 1..=n {
        let t = i as f64 / n as f64;
        let u = 1.0 - t;
        out.push(add(
            add(scale(p0, u * u * u), scale(p1, 3.0 * u * u * t)),
            add(scale(p2, 3.0 * u * t * t), scale(p3, t * t * t)),
        ));
    }
}

/// Number of line segments so the deviation from the curve stays below `tol`
/// (`k` is the curve's second-difference magnitude scaled for the curve degree).
fn segments(k: f64, tol: f64) -> usize {
    let n = (k / tol).sqrt().ceil();
    if n.is_finite() { (n as usize).clamp(1, 1000) } else { 1 }
}

fn add(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] + b[0], a[1] + b[1]]
}
fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn scale(a: [f64; 2], s: f64) -> [f64; 2] {
    [a[0] * s, a[1] * s]
}
fn len(a: [f64; 2]) -> f64 {
    a[0].hypot(a[1])
}

fn ring_polygon(pts: &[[f64; 2]]) -> Polygon<f64> {
    let coords: Vec<Coord<f64>> = pts.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();
    Polygon::new(LineString::new(coords), vec![])
}

/// The filled area of a set of rings.
///
/// Even-odd is exact (symmetric difference). Non-zero is approximated the way
/// almost all real files use it: rings wound like the largest ring add area,
/// rings wound the opposite way cut holes.
pub(crate) fn fill_region(rings: &[&Vec<[f64; 2]>], even_odd: bool) -> MultiPolygon<f64> {
    let polys: Vec<(Polygon<f64>, f64)> = rings
        .iter()
        .map(|r| {
            let p = ring_polygon(r);
            let a = p.signed_area();
            (p, a)
        })
        .filter(|(_, a)| a.abs() > 1e-12 && a.is_finite())
        .collect();
    let mut acc = MultiPolygon::new(vec![]);
    if even_odd {
        for (p, _) in &polys {
            acc = acc.xor(&MultiPolygon::new(vec![p.clone()]));
        }
        return acc;
    }
    let main_sign = polys
        .iter()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map_or(1.0, |(_, a)| a.signum());
    let mut holes = MultiPolygon::new(vec![]);
    for (p, a) in &polys {
        let mp = MultiPolygon::new(vec![p.clone()]);
        if a.signum() == main_sign {
            acc = acc.union(&mp);
        } else {
            holes = holes.union(&mp);
        }
    }
    if holes.0.is_empty() { acc } else { acc.difference(&holes) }
}

/// Insert imported items at the top of the document. With more than one
/// top-level item and a `group_name`, they are wrapped in a group of that name.
/// Returns the ids of the new top-level objects.
pub fn add_to_document(doc: &mut Document, nodes: Vec<ImportNode>, group_name: Option<&str>) -> Vec<ObjId> {
    fn build(doc: &mut Document, node: ImportNode) -> Object {
        match node {
            ImportNode::Shape(name, shape) => Object { id: doc.new_id(), name, kind: ObjectKind::Shape(*shape) },
            ImportNode::Group(name, children) => {
                let id = doc.new_id();
                let children = children.into_iter().map(|c| build(doc, c)).collect();
                Object::group(id, name, children)
            }
        }
    }
    let mut objs: Vec<Object> = nodes.into_iter().map(|n| build(doc, n)).collect();
    if let Some(name) = group_name
        && objs.len() > 1
    {
        let id = doc.new_id();
        objs = vec![Object::group(id, name, objs)];
    }
    let ids = objs.iter().map(|o| o.id).collect();
    doc.objects.extend(objs);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(svg: &str) -> ImportReport {
        import_svg(svg.as_bytes(), 0.01).unwrap()
    }

    fn only_shape(rep: &ImportReport) -> &ShapeObj {
        match &rep.nodes[..] {
            [ImportNode::Shape(_, s)] => s,
            other => panic!("expected one shape, got {other:?}"),
        }
    }

    #[test]
    fn mm_svg_imports_at_true_size() {
        let rep = import(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100">
                <rect x="10" y="20" width="50" height="30" fill="#ff0000"/></svg>"##,
        );
        let s = only_shape(&rep);
        assert!((s.xf.w - 50.0).abs() < 1e-3, "width {}", s.xf.w);
        assert!((s.xf.h - 30.0).abs() < 1e-3);
        assert!((s.xf.cx - 35.0).abs() < 1e-3);
        assert!((s.xf.cy - 35.0).abs() < 1e-3);
        assert_eq!(s.style.fill, Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn px_units_convert_at_96_dpi() {
        let rep = import(r#"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="96"><rect width="96" height="48"/></svg>"#);
        let s = only_shape(&rep);
        assert!((s.xf.w - 25.4).abs() < 1e-3);
        assert!((s.xf.h - 12.7).abs() < 1e-3);
    }

    #[test]
    fn circle_is_flattened_within_tolerance() {
        let rep = import(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100">
                <circle cx="50" cy="50" r="20"/></svg>"#,
        );
        let area = only_shape(&rep).world_geometry(0.01).unsigned_area();
        let exact = std::f64::consts::PI * 400.0;
        assert!((area - exact).abs() / exact < 0.002, "area {area}");
    }

    #[test]
    fn holes_survive_both_fill_rules() {
        for rule in ["evenodd", "nonzero"] {
            // Outer clockwise, inner counter-clockwise: a hole under both rules.
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100">
                <path fill-rule="{rule}" d="M0 0 H60 V60 H0 Z M20 20 V40 H40 V20 Z"/></svg>"#
            );
            let rep = import(&svg);
            let g = only_shape(&rep).world_geometry(0.01);
            // usvg stores coordinates as f32, hence the 0.01 mm² allowance.
            assert!((g.unsigned_area() - (3600.0 - 400.0)).abs() < 1e-2,"{rule}: {}", g.unsigned_area());
        }
    }

    #[test]
    fn named_groups_are_kept_and_open_lines_outlined() {
        let rep = import(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100">
                <g id="pair"><rect width="10" height="10"/><rect x="20" width="10" height="10"/></g>
                <line x1="0" y1="50" x2="100" y2="50" stroke="black" stroke-width="2"/>
                <rect y="80" width="5" height="5"/></svg>"#,
        );
        assert_eq!(rep.shapes, 4);
        assert_eq!(rep.skipped, 0);
        assert!(matches!(&rep.nodes[0], ImportNode::Group(name, c) if name == "pair" && c.len() == 2));
        // The line became a filled 100 × 2 mm area in the stroke colour.
        let ImportNode::Shape(_, line) = &rep.nodes[1] else { panic!() };
        assert_eq!(line.style.fill, Some(Color::rgb(0, 0, 0)));
        assert_eq!(line.style.stroke, None);
        assert!((line.world_geometry(0.01).unsigned_area() - 200.0).abs() < 1e-3);

        let mut doc = Document::default();
        let ids = add_to_document(&mut doc, rep.nodes, Some("drawing"));
        assert_eq!(ids.len(), 1, "several top-level items get wrapped in one group");
        assert!(doc.get(ids[0]).unwrap().is_group());
    }

    /// Illustrator-style clipping mask, group opacity, gradient and stroke style.
    #[test]
    fn clip_opacity_gradient_and_stroke_style() {
        let rep = import(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100">
              <defs>
                <clipPath id="c"><rect x="10" y="10" width="20" height="20"/></clipPath>
                <linearGradient id="g"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></linearGradient>
              </defs>
              <g clip-path="url(#c)"><circle cx="10" cy="10" r="15" fill="#0a0" stroke="#000" stroke-width="2"/></g>
              <g opacity="0.5"><rect x="50" y="0" width="10" height="10" fill="#00f"/></g>
              <rect x="70" y="0" width="10" height="10" fill="url(#g)"/>
              <rect x="0" y="60" width="20" height="20" fill="none" stroke="#f00" stroke-width="4" stroke-linejoin="round"/>
            </svg>"##,
        );
        let shapes: Vec<&ShapeObj> = rep
            .nodes
            .iter()
            .map(|n| match n {
                ImportNode::Shape(_, s) => s.as_ref(),
                ImportNode::Group(..) => panic!("unexpected group"),
            })
            .collect();
        // Clipped circle: fill = quarter disc ∩ 20 × 20 square; the stroke is a
        // separate outlined shape, also clipped. The clip rectangle is not imported.
        assert_eq!(shapes.len(), 5);
        let fill = shapes[0].world_geometry(0.01);
        let b = geo::BoundingRect::bounding_rect(&fill).unwrap();
        assert!((b.min().x - 10.0).abs() < 1e-3 && (b.min().y - 10.0).abs() < 1e-3);
        assert!(b.max().x <= 25.0 + 1e-3, "clipped to the circle (r 15 around 10,10)");
        let quarter = std::f64::consts::PI * 225.0 / 4.0;
        assert!((fill.unsigned_area() - quarter).abs() / quarter < 0.005);
        assert_eq!(shapes[1].style.fill, Some(Color::rgb(0, 0, 0)), "outlined stroke");
        // Group opacity multiplies into the colour.
        assert_eq!(shapes[2].style.fill.unwrap().a, 128);
        // Gradients are flagged.
        assert_eq!(shapes[3].special, Some(SpecialPaint::Gradient));
        // Closed stroke-only path keeps its stroke style.
        assert_eq!(shapes[4].style.line_join, LineJoin::Round);
        assert!((shapes[4].style.stroke_width - 4.0).abs() < 1e-6);
    }

    #[test]
    fn export_then_import_round_trips_in_mm() {
        use crate::shapes::{ShapeGeom, Transform};
        use crate::svg_export::{SvgBounds, SvgOptions, export_svg};
        let mut doc = Document::default();
        doc.add_shape("r", ShapeObj::new(ShapeGeom::Rect, Transform::new(40.0, 60.0, 50.0, 25.0), Style::default()));
        let opts = SvgOptions { only_selected: false, bounds: SvgBounds::Artboard };
        let (svg, _) = export_svg(&doc, &[], &opts);
        let rep = import(&svg);
        let s = only_shape(&rep);
        assert!((s.xf.w - 50.0).abs() < 1e-3 && (s.xf.h - 25.0).abs() < 1e-3);
        assert!((s.xf.cx - 40.0).abs() < 1e-3 && (s.xf.cy - 60.0).abs() < 1e-3);
    }

    #[test]
    fn illustrator_svg_pixels_are_points() {
        // Illustrator writes 1 px = 1 pt: a 72 px square is one inch.
        let svg = r#"<?xml version="1.0"?>
            <!-- Generator: Adobe Illustrator 28.0.0, SVG Export Plug-In . SVG Version: 6.00 Build 0)  -->
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200"><rect width="72" height="36"/></svg>"#;
        let s = only_shape(&import(svg)).clone();
        assert!((s.xf.w - 25.4).abs() < 1e-3 && (s.xf.h - 12.7).abs() < 1e-3, "{}", s.xf.w);
        // Absolute units stay exact at any dpi.
        let mm = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="0 0 100 100"><rect width="40" height="10"/></svg>"#;
        let s = only_shape(&import_svg_at_dpi(mm.as_bytes(), 0.01, ILLUSTRATOR_DPI).unwrap()).clone();
        assert!((s.xf.w - 40.0).abs() < 1e-3);
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(import_svg(b"not svg", 0.01).is_err());
        assert!(import_svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#, 0.01).is_err());
    }
}
