//! Flatten to Visible — keep only what is actually seen on the white artboard.
//!
//! The objects are processed in stacking order (bottom → top):
//!
//! 1. Everything becomes filled areas: a shape's fill is its region (fill rules,
//!    holes and clips were already resolved on import), and its stroke is
//!    outlined into its own area (real width, joins, caps, miter limit — by
//!    `i_overlay`) lying just above the fill. Objects with neither are invisible
//!    and dropped.
//! 2. Trim: each area keeps only `area − ∪(areas above it)`, whatever their colour.
//! 3. White areas (the artboard colour) are deleted after trimming — their only
//!    effect is the holes they cut.
//! 4. Areas that end up empty are deleted.
//! 5. Merge (optional): visible areas of exactly the same colour are unioned.
//! 6. Slivers below the minimum area are dropped; outlines are simplified
//!    within the flattening tolerance.
//! 7. Every remaining connected piece becomes its own object.
//!
//! Shapes that cannot be flattened safely (pattern fills, transparency, gradients,
//! blend modes, masks) are kept unchanged; when fully opaque they still cut the
//! areas below them.
//!
//! Pure geometry (no egui), so it can be reused in a WebAssembly build.

use crate::document::{DocSettings, Document, ObjId, Object, ObjectKind, ShapeObj, SpecialPaint};
use crate::shapes::{Color, LineCap, LineJoin, Style, custom_from_world, ring_points};
use geo::{Area, BooleanOps, BoundingRect, LineString, MultiPolygon, Polygon, Rect, Simplify};
use i_overlay::mesh::stroke::offset::StrokeOffset;
use i_overlay::mesh::style::{LineCap as OvCap, LineJoin as OvJoin, StrokeStyle};

#[derive(Clone, Debug)]
pub struct FlattenOptions {
    pub merge_same_color: bool,
    /// White = every RGB channel ≥ this (opaque).
    pub white_threshold: u8,
    /// Pieces smaller than this (mm²) are dropped.
    pub min_area: f64,
    /// Curve / simplification tolerance (mm).
    pub tolerance: f64,
}

impl FlattenOptions {
    pub fn from_settings(s: &DocSettings) -> Self {
        Self {
            merge_same_color: s.merge_same_color,
            white_threshold: s.white_threshold,
            min_area: crate::shapes::clamp_finite(s.min_piece_area, 0.0, 1.0e6, 0.01),
            tolerance: s.tolerance(),
        }
    }
}

impl Default for FlattenOptions {
    fn default() -> Self {
        Self::from_settings(&DocSettings::default())
    }
}

#[derive(Clone, Debug, Default)]
pub struct FlattenReport {
    /// Shapes that went in (groups resolved).
    pub input_objects: usize,
    /// Objects that came out (visible pieces + kept shapes).
    pub output_objects: usize,
    /// White areas (fills and outlined strokes) removed.
    pub white_removed: usize,
    /// Objects deleted because nothing of them was visible (or they had no paint).
    pub hidden_deleted: usize,
    /// Shapes left unchanged because they cannot be flattened safely.
    pub skipped: usize,
    /// Why they were skipped ("gradient", "transparency", …), without duplicates.
    pub skipped_kinds: Vec<&'static str>,
}

impl FlattenReport {
    /// "Flattened: 38 objects → 21 visible objects, 9 white shapes removed, …".
    /// `extra_skipped` are elements the importer could not convert (text, images).
    pub fn message(&self, ms: f64, extra_skipped: usize) -> String {
        let mut s = format!(
            "Flattened: {} objects → {} visible objects, {} white shapes removed, {} hidden objects deleted ({:.0} ms)",
            self.input_objects, self.output_objects, self.white_removed, self.hidden_deleted, ms
        );
        let skipped = self.skipped + extra_skipped;
        if skipped > 0 {
            let mut kinds = self.skipped_kinds.clone();
            if extra_skipped > 0 {
                kinds.push("image/text");
            }
            s.push_str(&format!(", skipped: {skipped} ({})", kinds.join("/")));
        }
        s
    }
}

/// One output item, in stacking order.
#[derive(Clone, Debug)]
pub enum FlatItem {
    /// A visible, connected piece with a single colour.
    Piece { area: Polygon<f64>, color: Color },
    /// A shape kept unchanged (could not be flattened safely).
    Kept(Box<ShapeObj>),
}

// ---- stroke outlining ---------------------------------------------------------------

/// Outline strokes into filled areas (mm). `paths` are polylines with a flag for
/// closed ones; `width` is the full stroke width. Uses i_overlay's stroker, which
/// handles joins, caps and the miter limit like SVG / PostScript renderers.
pub fn outline_stroke(
    paths: &[(Vec<[f64; 2]>, bool)],
    width: f64,
    join: LineJoin,
    cap: LineCap,
    miter_limit: f64,
    tol: f64,
) -> MultiPolygon<f64> {
    if !(width.is_finite() && width > 1e-9) {
        return MultiPolygon::new(vec![]);
    }
    let r = width / 2.0;
    // Round joins/caps: segment angle so the chord deviates < tol from the arc.
    let arc_step = (8.0 * tol.max(1e-6) / r).sqrt();
    let ov_join = match join {
        // SVG bevels a corner when 1/sin(θ/2) > miter limit, i.e. θ < 2·asin(1/limit).
        LineJoin::Miter => OvJoin::Miter(2.0 * (1.0 / miter_limit.max(1.0)).asin()),
        LineJoin::Round => OvJoin::Round(arc_step),
        LineJoin::Bevel => OvJoin::Bevel,
    };
    let ov_cap = || match cap {
        LineCap::Butt => OvCap::Butt,
        LineCap::Round => OvCap::Round(arc_step),
        LineCap::Square => OvCap::Square,
    };
    let style = |c| StrokeStyle::new(width).line_join(ov_join_clone(&ov_join)).start_cap(c).end_cap(ov_cap());
    let mut out = MultiPolygon::new(vec![]);
    for closed in [true, false] {
        let group: Vec<Vec<[f64; 2]>> = paths
            .iter()
            .filter(|(p, c)| *c == closed && p.len() >= 2)
            .map(|(p, _)| dedup(p))
            .filter(|p| p.len() >= 2)
            .collect();
        if group.is_empty() {
            continue;
        }
        let shapes = group.stroke(style(ov_cap()), closed);
        let mp = shapes_to_multipolygon(shapes);
        out = if out.0.is_empty() { mp } else { out.union(&mp) };
    }
    out
}

fn ov_join_clone(j: &OvJoin<f64>) -> OvJoin<f64> {
    match j {
        OvJoin::Bevel => OvJoin::Bevel,
        OvJoin::Miter(a) => OvJoin::Miter(*a),
        OvJoin::Round(a) => OvJoin::Round(*a),
    }
}

/// Drop consecutive duplicate points (they confuse join directions).
fn dedup(p: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(p.len());
    for q in p {
        if out.last().is_none_or(|l| (l[0] - q[0]).abs() > 1e-12 || (l[1] - q[1]).abs() > 1e-12) {
            out.push(*q);
        }
    }
    out
}

fn shapes_to_multipolygon(shapes: Vec<Vec<Vec<[f64; 2]>>>) -> MultiPolygon<f64> {
    MultiPolygon::new(
        shapes
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|mut s| {
                let outer = s.remove(0);
                Polygon::new(LineString::from(outer), s.into_iter().map(LineString::from).collect())
            })
            .collect(),
    )
}

/// The outlined stroke of a shape (along every ring of its region).
pub fn shape_stroke_area(s: &ShapeObj, tol: f64) -> MultiPolygon<f64> {
    let geom = s.world_geometry(tol);
    let rings: Vec<(Vec<[f64; 2]>, bool)> = geom
        .0
        .iter()
        .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
        .map(|ls| (ring_points(ls), true))
        .collect();
    outline_stroke(&rings, s.style.stroke_width, s.style.line_join, s.style.line_cap, s.style.miter_limit, tol)
}

// ---- the algorithm ----------------------------------------------------------------------

struct Layer {
    area: MultiPolygon<f64>,
    bbox: Option<Rect<f64>>,
    color: Color,
    /// Index of the source shape.
    obj: usize,
    /// A kept (unflattenable) shape: not trimmed, not deleted, not merged.
    keep: bool,
    /// Does this layer hide what is below it?
    cuts: bool,
}

fn overlaps(a: &Rect<f64>, b: &Rect<f64>) -> bool {
    a.min().x <= b.max().x && b.min().x <= a.max().x && a.min().y <= b.max().y && b.min().y <= a.max().y
}

/// Why a shape cannot be flattened safely, if it cannot.
fn skip_reason(s: &ShapeObj) -> Option<&'static str> {
    if s.pattern.is_some() {
        return Some("pattern fill");
    }
    if let Some(sp) = s.special {
        return Some(sp.label());
    }
    let translucent = |c: Option<Color>| c.is_some_and(|c| !c.is_opaque());
    if translucent(s.style.fill) || (s.style.stroke_width > 0.0 && translucent(s.style.stroke)) {
        return Some("transparency");
    }
    None
}

/// Flatten shapes given bottom → top. Returns the visible result, bottom → top.
pub fn flatten_shapes(shapes: &[ShapeObj], opts: &FlattenOptions) -> (Vec<FlatItem>, FlattenReport) {
    let tol = opts.tolerance;
    let mut report = FlattenReport { input_objects: shapes.len(), ..Default::default() };
    let mut layers: Vec<Layer> = Vec::new();
    let mk = |area: MultiPolygon<f64>, color, obj, keep, cuts| {
        let bbox = area.bounding_rect();
        Layer { area, bbox, color, obj, keep, cuts }
    };

    // 1. Areas.
    for (i, s) in shapes.iter().enumerate() {
        let fill = s.style.fill.filter(|c| c.a > 0);
        let stroke = s.style.stroke.filter(|c| c.a > 0 && s.style.stroke_width > 1e-9);
        if fill.is_none() && stroke.is_none() && s.pattern.is_none() {
            report.hidden_deleted += 1; // no paint at all: invisible
            continue;
        }
        if let Some(reason) = skip_reason(s) {
            report.skipped += 1;
            if !report.skipped_kinds.contains(&reason) {
                report.skipped_kinds.push(reason);
            }
            // Fully opaque parts of gradients / partly transparent shapes still hide
            // what is below; patterns (gaps), blend modes and masks do not.
            let may_cut = matches!(s.special, None | Some(SpecialPaint::Gradient)) && s.pattern.is_none();
            let mut cut = MultiPolygon::new(vec![]);
            if may_cut {
                if fill.is_some_and(Color::is_opaque) {
                    cut = s.world_geometry(tol);
                }
                if stroke.is_some_and(Color::is_opaque) {
                    let st = shape_stroke_area(s, tol);
                    cut = if cut.0.is_empty() { st } else { cut.union(&st) };
                }
            }
            let cuts = !cut.0.is_empty();
            layers.push(mk(cut, Color::BLACK, i, true, cuts));
            continue;
        }
        if let Some(c) = fill {
            layers.push(mk(s.world_geometry(tol), c, i, false, true));
        }
        if let Some(c) = stroke {
            layers.push(mk(shape_stroke_area(s, tol), c, i, false, true));
        }
    }

    // 2. Trim, top → bottom: subtract everything above (bounding boxes first).
    let mut visible: Vec<MultiPolygon<f64>> = vec![MultiPolygon::new(vec![]); layers.len()];
    let mut cutters: Vec<(usize, Rect<f64>)> = Vec::new();
    for idx in (0..layers.len()).rev() {
        let l = &layers[idx];
        if !l.keep
            && let Some(b) = l.bbox
        {
            let mut v = l.area.clone();
            for &(ci, cb) in &cutters {
                if overlaps(&b, &cb) {
                    v = v.difference(&layers[ci].area);
                    if v.0.is_empty() {
                        break;
                    }
                }
            }
            visible[idx] = v;
        }
        if l.cuts
            && let Some(b) = l.bbox
        {
            cutters.push((idx, b));
        }
    }

    // 3./4. Drop white and empty areas; count per source object.
    let mut obj_visible = vec![false; shapes.len()];
    let mut obj_nonwhite = vec![false; shapes.len()];
    let mut survivors: Vec<(usize, MultiPolygon<f64>)> = Vec::new(); // (layer, area)
    for (idx, l) in layers.iter().enumerate() {
        if l.keep {
            obj_visible[l.obj] = true;
            continue;
        }
        if l.color.is_white(opts.white_threshold) {
            report.white_removed += 1;
            continue;
        }
        obj_nonwhite[l.obj] = true;
        let v = std::mem::replace(&mut visible[idx], MultiPolygon::new(vec![]));
        if v.unsigned_area() > opts.min_area.max(1e-12) {
            obj_visible[l.obj] = true;
            survivors.push((idx, v));
        }
    }
    report.hidden_deleted += (0..shapes.len()).filter(|&i| obj_nonwhite[i] && !obj_visible[i]).count();

    // 5. Merge equal colours. Kept shapes split the stack into bands; merging only
    // happens within a band, so pieces never jump across a kept shape in z-order.
    let mut band = vec![0usize; layers.len()];
    let mut b = 0;
    for (idx, l) in layers.iter().enumerate() {
        band[idx] = b;
        if l.keep {
            b += 1;
        }
    }
    // (z, colour, area)
    let mut groups: Vec<(usize, Color, MultiPolygon<f64>)> = Vec::new();
    if opts.merge_same_color {
        let mut keys: Vec<(usize, Color)> = Vec::new();
        let mut members: Vec<(usize, Vec<Polygon<f64>>)> = Vec::new(); // (max z, polygons)
        for (idx, area) in survivors {
            let key = (band[idx], layers[idx].color);
            let k = match keys.iter().position(|q| *q == key) {
                Some(k) => k,
                None => {
                    keys.push(key);
                    members.push((idx, Vec::new()));
                    keys.len() - 1
                }
            };
            members[k].0 = members[k].0.max(idx);
            members[k].1.extend(area.0);
        }
        for ((_, color), (z, polys)) in keys.into_iter().zip(members) {
            let merged = if polys.len() == 1 { MultiPolygon::new(polys) } else { geo::unary_union(&polys) };
            groups.push((z, color, merged));
        }
    } else {
        groups = survivors.into_iter().map(|(idx, a)| (idx, layers[idx].color, a)).collect();
    }

    // 6./7. Simplify, drop slivers, one object per connected piece; kept shapes
    // go back at their own position in the stack.
    let mut items: Vec<(usize, FlatItem)> = Vec::new();
    for (z, color, area) in groups {
        // Hairline holes / fragments (e.g. where a font's contours overlap) go too.
        let area = crate::shapes::drop_tiny_parts(area, opts.min_area);
        for poly in area.0 {
            let simple = poly.simplify(tol);
            let poly = if simple.unsigned_area() > 0.0 { simple } else { poly };
            if poly.unsigned_area() >= opts.min_area.max(1e-12) {
                items.push((z, FlatItem::Piece { area: poly, color }));
            }
        }
    }
    for (idx, l) in layers.iter().enumerate() {
        if l.keep {
            items.push((idx, FlatItem::Kept(Box::new(shapes[l.obj].clone()))));
        }
    }
    items.sort_by_key(|(z, _)| *z);
    report.output_objects = items.len();
    (items.into_iter().map(|(_, it)| it).collect(), report)
}

/// Result of flattening objects in a document.
#[derive(Debug)]
pub struct FlattenOutcome {
    /// The new group holding the visible pieces (None if nothing was visible).
    pub group: Option<ObjId>,
    pub report: FlattenReport,
}

/// Replace the given top-level objects by a group `group_name` holding only
/// what is visible of them. Returns None if none of the ids is a top-level object.
pub fn flatten_objects(doc: &mut Document, ids: &[ObjId], group_name: &str, opts: &FlattenOptions) -> Option<FlattenOutcome> {
    let idx = doc.top_indices(ids);
    if idx.is_empty() {
        return None;
    }
    let mut shapes = Vec::new();
    for &i in &idx {
        doc.objects[i].for_each_shape(&mut |_, s| shapes.push(s.clone()));
    }
    let (items, report) = flatten_shapes(&shapes, opts);

    let insert_at = idx[idx.len() - 1] + 1 - idx.len();
    for &i in idx.iter().rev() {
        doc.objects.remove(i);
    }
    let mut children = Vec::with_capacity(items.len());
    for item in items {
        let (name, shape) = match item {
            FlatItem::Piece { area, color } => {
                let Some((geom, xf)) = custom_from_world(&MultiPolygon::new(vec![area])) else { continue };
                (format!("Visible {}", color.to_hex()), ShapeObj::new(geom, xf, Style::fill_only(color)))
            }
            FlatItem::Kept(s) => ("Kept (not flattened)".to_string(), *s),
        };
        children.push(Object { id: doc.new_id(), name, kind: ObjectKind::Shape(shape) });
    }
    let group = if children.is_empty() {
        None
    } else {
        let id = doc.new_id();
        doc.objects.insert(insert_at, Object::group(id, group_name, children));
        Some(id)
    };
    Some(FlattenOutcome { group, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::History;
    use crate::shapes::{ShapeGeom, Transform};

    const RED: Color = Color::rgb(220, 20, 20);
    const BLUE: Color = Color::rgb(20, 40, 200);
    const GREEN: Color = Color::rgb(20, 160, 60);

    fn shape(geom: ShapeGeom, cx: f64, cy: f64, w: f64, h: f64, fill: Option<Color>, stroke: Option<(Color, f64)>) -> ShapeObj {
        let style = Style {
            fill,
            stroke: stroke.map(|s| s.0),
            stroke_width: stroke.map_or(0.0, |s| s.1),
            ..Style::default()
        };
        ShapeObj::new(geom, Transform::new(cx, cy, w, h), style)
    }

    fn pieces(items: &[FlatItem]) -> Vec<(&Polygon<f64>, Color)> {
        items
            .iter()
            .filter_map(|i| match i {
                FlatItem::Piece { area, color } => Some((area, *color)),
                FlatItem::Kept(_) => None,
            })
            .collect()
    }

    #[test]
    fn white_circle_on_red_square_becomes_a_hole() {
        let shapes = [
            shape(ShapeGeom::Rect, 50.0, 50.0, 40.0, 40.0, Some(RED), None),
            shape(ShapeGeom::Ellipse, 50.0, 50.0, 20.0, 20.0, Some(Color::WHITE), None),
        ];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        let p = pieces(&items);
        assert_eq!(p.len(), 1, "one red square, no white object");
        assert_eq!(p[0].1, RED);
        assert_eq!(p[0].0.interiors().len(), 1, "circular hole");
        let expected = 1600.0 - std::f64::consts::PI * 100.0;
        assert!((p[0].0.unsigned_area() - expected).abs() / expected < 0.002);
        assert_eq!(rep.white_removed, 1);
        assert_eq!(rep.output_objects, 1);
    }

    #[test]
    fn white_stroke_cuts_a_ring() {
        // No-fill circle Ø 20 with a 2 mm white stroke on a red square.
        let shapes = [
            shape(ShapeGeom::Rect, 50.0, 50.0, 40.0, 40.0, Some(RED), None),
            shape(ShapeGeom::Ellipse, 50.0, 50.0, 20.0, 20.0, None, Some((Color::WHITE, 2.0))),
        ];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        let p = pieces(&items);
        let red_area: f64 = p.iter().map(|(a, _)| a.unsigned_area()).sum();
        let ring = std::f64::consts::PI * (11.0f64.powi(2) - 9.0f64.powi(2)); // 2 mm ring at r = 10
        let cut = 1600.0 - red_area;
        assert!((cut - ring).abs() / ring < 0.005, "ring {cut} vs {ring}");
        assert!(p.iter().all(|(_, c)| *c == RED));
        assert_eq!(p.len(), 2, "outer part and the disc inside the ring");
        assert_eq!(rep.white_removed, 1);
    }

    #[test]
    fn white_under_blue_is_deleted_and_blue_unchanged() {
        let blue = shape(ShapeGeom::Rect, 50.0, 50.0, 40.0, 30.0, Some(BLUE), None);
        let shapes = [shape(ShapeGeom::Rect, 50.0, 50.0, 20.0, 10.0, Some(Color::WHITE), None), blue.clone()];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        let p = pieces(&items);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1, BLUE);
        assert!((p[0].0.unsigned_area() - 1200.0).abs() < 1e-6);
        assert_eq!(p[0].0.bounding_rect(), blue.world_geometry(0.01).bounding_rect());
        assert_eq!(rep.white_removed, 1);
    }

    #[test]
    fn half_covered_blue_keeps_only_visible_part() {
        let shapes = [
            shape(ShapeGeom::Rect, 50.0, 50.0, 20.0, 20.0, Some(BLUE), None),
            shape(ShapeGeom::Rect, 60.0, 50.0, 20.0, 20.0, Some(GREEN), None),
        ];
        let (items, _) = flatten_shapes(&shapes, &FlattenOptions::default());
        let p = pieces(&items);
        let blue = p.iter().find(|(_, c)| *c == BLUE).unwrap();
        let green = p.iter().find(|(_, c)| *c == GREEN).unwrap();
        assert!((blue.0.unsigned_area() - 200.0).abs() < 1e-6, "left half only");
        let bb = blue.0.bounding_rect().unwrap();
        assert!((bb.min().x - 40.0).abs() < 1e-9 && (bb.max().x - 50.0).abs() < 1e-9);
        assert!((green.0.unsigned_area() - 400.0).abs() < 1e-6, "green unchanged");
        // Bottom to top: blue below green.
        assert!(matches!(&items[0], FlatItem::Piece { color, .. } if *color == BLUE));
    }

    #[test]
    fn overlapping_same_colour_merges_only_when_enabled() {
        let shapes = [
            shape(ShapeGeom::Ellipse, 40.0, 50.0, 30.0, 30.0, Some(RED), None),
            shape(ShapeGeom::Ellipse, 60.0, 50.0, 30.0, 30.0, Some(RED), None),
        ];
        let (items, _) = flatten_shapes(&shapes, &FlattenOptions::default());
        assert_eq!(pieces(&items).len(), 1, "merged");
        let off = FlattenOptions { merge_same_color: false, ..FlattenOptions::default() };
        let (items, _) = flatten_shapes(&shapes, &off);
        assert_eq!(pieces(&items).len(), 2, "not merged");
    }

    #[test]
    fn lone_white_object_is_deleted() {
        let shapes = [shape(ShapeGeom::Rect, 50.0, 50.0, 20.0, 20.0, Some(Color::rgb(252, 253, 255)), Some((Color::WHITE, 1.0)))];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        assert!(items.is_empty());
        assert_eq!(rep.white_removed, 2, "white fill and white stroke");
        // Slightly darker than the threshold is a real colour.
        let strict = FlattenOptions { white_threshold: 254, ..FlattenOptions::default() };
        let (items, _) = flatten_shapes(&shapes[..1], &strict);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn invisible_and_fully_hidden_objects_are_deleted() {
        let shapes = [
            shape(ShapeGeom::Rect, 50.0, 50.0, 10.0, 10.0, Some(GREEN), None), // hidden below blue
            shape(ShapeGeom::Rect, 50.0, 50.0, 10.0, 10.0, None, None),        // no paint
            shape(ShapeGeom::Rect, 50.0, 50.0, 30.0, 30.0, Some(BLUE), None),
        ];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        assert_eq!(pieces(&items).len(), 1);
        assert_eq!(rep.hidden_deleted, 2);
    }

    #[test]
    fn stroke_sits_above_its_fill_and_has_real_width() {
        // Blue square with a 4 mm green miter stroke: green band 4 mm wide around
        // the edge, blue keeps the inner 36 × 36 (sharp corners).
        let shapes = [shape(ShapeGeom::Rect, 50.0, 50.0, 40.0, 40.0, Some(BLUE), Some((GREEN, 4.0)))];
        let (items, _) = flatten_shapes(&shapes, &FlattenOptions::default());
        let p = pieces(&items);
        let blue = p.iter().find(|(_, c)| *c == BLUE).unwrap();
        let green = p.iter().find(|(_, c)| *c == GREEN).unwrap();
        assert!((blue.0.unsigned_area() - 36.0 * 36.0).abs() < 1e-6);
        assert!((green.0.unsigned_area() - (44.0 * 44.0 - 36.0 * 36.0)).abs() < 1e-6);
    }

    #[test]
    fn transparent_and_gradient_shapes_are_kept_and_reported() {
        let mut grad = shape(ShapeGeom::Rect, 50.0, 50.0, 10.0, 10.0, Some(Color::rgb(128, 128, 128)), None);
        grad.special = Some(SpecialPaint::Gradient);
        let glass = shape(ShapeGeom::Rect, 70.0, 50.0, 10.0, 10.0, Some(Color { a: 128, ..GREEN }), None);
        let shapes = [shape(ShapeGeom::Rect, 60.0, 50.0, 40.0, 20.0, Some(RED), None), grad, glass];
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        assert_eq!(rep.skipped, 2);
        assert_eq!(rep.skipped_kinds, vec!["gradient", "transparency"]);
        let red: f64 = pieces(&items).iter().map(|(a, _)| a.unsigned_area()).sum();
        assert!((red - (800.0 - 100.0)).abs() < 1e-6, "opaque gradient cuts, transparent does not");
        assert_eq!(items.iter().filter(|i| matches!(i, FlatItem::Kept(_))).count(), 2);
        assert!(matches!(items.last(), Some(FlatItem::Kept(s)) if s.style.fill.unwrap().a == 128), "z-order kept");
        let msg = rep.message(3.0, 1);
        assert!(msg.contains("skipped: 3 (gradient/transparency/image/text)"), "{msg}");
    }

    /// Paste flow: one Ctrl+Z after an auto-flattened paste gives back the raw
    /// pasted objects, exactly.
    #[test]
    fn undo_after_auto_flattened_paste_restores_raw_objects() {
        let mut doc = Document::default();
        let mut hist = History::default();
        doc.add_shape("existing", shape(ShapeGeom::Rect, 20.0, 20.0, 10.0, 10.0, Some(GREEN), None));
        let before_paste = serde_json::to_string(&doc).unwrap();

        // Paste (as the app does it): snapshot, add, snapshot, flatten.
        hist.push(&doc);
        let ids = vec![
            doc.add_shape("a", shape(ShapeGeom::Rect, 100.0, 100.0, 40.0, 40.0, Some(RED), None)),
            doc.add_shape("b", shape(ShapeGeom::Ellipse, 100.0, 100.0, 20.0, 20.0, Some(Color::WHITE), None)),
        ];
        let raw = serde_json::to_string(&doc).unwrap();
        hist.push(&doc);
        let out = flatten_objects(&mut doc, &ids, "Flattened paste", &FlattenOptions::default()).unwrap();
        let g = out.group.unwrap();
        assert_eq!(doc.get(g).unwrap().name, "Flattened paste");
        assert_eq!(doc.objects.len(), 2, "existing object + the flattened group");

        assert!(hist.undo(&mut doc));
        assert_eq!(serde_json::to_string(&doc).unwrap(), raw, "raw pasted objects back");
        assert!(hist.undo(&mut doc));
        assert_eq!(serde_json::to_string(&doc).unwrap(), before_paste);
    }

    /// `cargo test --release flatten_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn flatten_timing() {
        let palette = [RED, BLUE, GREEN, Color::WHITE, Color::rgb(240, 160, 20), Color::rgb(90, 0, 120), Color::rgb(0, 0, 0), Color::rgb(0, 150, 160)];
        let mut shapes = Vec::new();
        for i in 0..600u32 {
            let x = f64::from((i * 37) % 180) + 15.0;
            let y = f64::from((i * 53) % 260) + 15.0;
            let geom = if i % 3 == 0 { ShapeGeom::Ellipse } else { ShapeGeom::Rect };
            let stroke = (i % 4 == 0).then(|| (palette[(i as usize + 3) % palette.len()], 0.8));
            shapes.push(shape(geom, x, y, 14.0 + f64::from(i % 7), 10.0 + f64::from(i % 5), Some(palette[i as usize % palette.len()]), stroke));
        }
        let t = std::time::Instant::now();
        let (items, rep) = flatten_shapes(&shapes, &FlattenOptions::default());
        println!("{} → {} objects in {:.1} ms ({:?})", rep.input_objects, items.len(), t.elapsed().as_secs_f64() * 1e3, rep);
    }

    /// Text converted to curves can carry hairline holes where contours overlap;
    /// they are invisible slivers and are removed (real holes stay).
    #[test]
    fn hairline_holes_are_removed_real_holes_kept() {
        use crate::shapes::{Rings, custom_from_world};
        let poly = Rings {
            exterior: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            holes: vec![
                vec![[2.0, 5.0], [8.0, 5.0], [8.0, 5.0001], [2.0, 5.0001]], // 0.0006 mm² slit
                vec![[3.0, 1.0], [5.0, 1.0], [5.0, 3.0], [3.0, 3.0]],       // real 4 mm² hole
            ],
        }
        .to_polygon();
        let (geom, xf) = custom_from_world(&MultiPolygon::new(vec![poly])).unwrap();
        let s = ShapeObj::new(geom, xf, Style::fill_only(RED));
        let (items, _) = flatten_shapes(&[s], &FlattenOptions::default());
        let p = pieces(&items);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0.interiors().len(), 1, "slit gone, real hole kept");
        assert!((p[0].0.unsigned_area() - 96.0).abs() < 1e-3);
    }

    #[test]
    fn outline_stroke_open_path_caps() {
        let line = [(vec![[0.0, 0.0], [10.0, 0.0]], false)];
        let butt = outline_stroke(&line, 2.0, LineJoin::Miter, LineCap::Butt, 4.0, 0.01);
        assert!((butt.unsigned_area() - 20.0).abs() < 1e-9);
        let square = outline_stroke(&line, 2.0, LineJoin::Miter, LineCap::Square, 4.0, 0.01);
        assert!((square.unsigned_area() - 24.0).abs() < 1e-9);
        let round = outline_stroke(&line, 2.0, LineJoin::Round, LineCap::Round, 4.0, 0.01);
        // Arcs are flattened within tol: the area error is at most tol × arc length (2π).
        assert!((round.unsigned_area() - (20.0 + std::f64::consts::PI)).abs() < 0.01 * 2.0 * std::f64::consts::PI);
        assert!(outline_stroke(&line, 0.0, LineJoin::Miter, LineCap::Butt, 4.0, 0.01).0.is_empty());
    }
}
