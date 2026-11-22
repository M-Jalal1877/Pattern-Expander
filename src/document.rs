//! Document model: objects, groups, IDs, undo/redo and document-level edits.
//!
//! No egui here. All values are millimetres.

use crate::expand::{ExpandMode, ExpandOptions, ExpandResult, expand_region};
use crate::pattern::{MAX_TILES, PatternFill};
use crate::shapes::{
    Bounds, ShapeGeom, Style, Transform, bounds_of, bounds_union, custom_from_world, world_geometry,
};
use geo::{BooleanOps, MultiPolygon};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub type ObjId = u64;

static REVISION: AtomicU64 = AtomicU64::new(1);

/// A globally unique revision number. Every content change of a shape gets a
/// fresh one, so renderers can cache per (id, revision).
pub fn next_rev() -> u64 {
    REVISION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShapeObj {
    pub geom: ShapeGeom,
    pub xf: Transform,
    pub style: Style,
    #[serde(default)]
    pub pattern: Option<PatternFill>,
    /// Set by importers when the source paint could not be represented exactly
    /// (the shape then carries an approximation). Flatten to Visible leaves such
    /// shapes untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub special: Option<SpecialPaint>,
    #[serde(skip, default = "next_rev")]
    pub rev: u64,
}

/// Source paint this app only approximates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialPaint {
    /// Gradient or pattern paint (imported as a flat colour).
    Gradient,
    /// A blend mode other than normal.
    BlendMode,
    /// An SVG mask.
    Mask,
}

impl SpecialPaint {
    pub fn label(self) -> &'static str {
        match self {
            SpecialPaint::Gradient => "gradient",
            SpecialPaint::BlendMode => "blend mode",
            SpecialPaint::Mask => "mask",
        }
    }
}

impl ShapeObj {
    pub fn new(geom: ShapeGeom, xf: Transform, style: Style) -> Self {
        Self {
            geom,
            xf,
            style,
            pattern: None,
            special: None,
            rev: next_rev(),
        }
    }

    /// Mark the shape as changed.
    pub fn touch(&mut self) {
        self.rev = next_rev();
    }

    pub fn world_geometry(&self, tol: f64) -> MultiPolygon<f64> {
        world_geometry(&self.geom, &self.xf, tol)
    }
}

// Shapes are stored inline on purpose: nearly every object is a shape, and an
// expanded pattern can hold 200 000 of them — boxing would add one heap
// allocation per piece.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ObjectKind {
    Shape(ShapeObj),
    /// Children, shared copy-on-write: undo snapshots of a document with big
    /// groups (an expanded pattern) share them instead of copying every piece.
    /// Mutate through [`Object::children_mut`].
    Group(Arc<Vec<Object>>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Object {
    pub id: ObjId,
    pub name: String,
    pub kind: ObjectKind,
}

impl Object {
    pub fn as_shape(&self) -> Option<&ShapeObj> {
        match &self.kind {
            ObjectKind::Shape(s) => Some(s),
            ObjectKind::Group(_) => None,
        }
    }

    pub fn as_shape_mut(&mut self) -> Option<&mut ShapeObj> {
        match &mut self.kind {
            ObjectKind::Shape(s) => Some(s),
            ObjectKind::Group(_) => None,
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self.kind, ObjectKind::Group(_))
    }

    /// A group object owning `children`.
    pub fn group(id: ObjId, name: impl Into<String>, children: Vec<Object>) -> Self {
        Self { id, name: name.into(), kind: ObjectKind::Group(Arc::new(children)) }
    }

    /// Mutable children of a group (copied first if an undo snapshot shares them).
    pub fn children_mut(&mut self) -> Option<&mut Vec<Object>> {
        match &mut self.kind {
            ObjectKind::Group(c) => Some(Arc::make_mut(c)),
            ObjectKind::Shape(_) => None,
        }
    }

    pub fn has_pattern(&self) -> bool {
        self.as_shape().is_some_and(|s| s.pattern.is_some())
    }

    pub fn for_each_shape<'a>(&'a self, f: &mut impl FnMut(&'a Object, &'a ShapeObj)) {
        match &self.kind {
            ObjectKind::Shape(s) => f(self, s),
            ObjectKind::Group(children) => children.iter().for_each(|c| c.for_each_shape(f)),
        }
    }

    pub fn for_each_shape_mut(&mut self, f: &mut impl FnMut(&mut ShapeObj)) {
        match &mut self.kind {
            ObjectKind::Shape(s) => f(s),
            ObjectKind::Group(children) => Arc::make_mut(children).iter_mut().for_each(|c| c.for_each_shape_mut(f)),
        }
    }

    pub fn shape_count(&self) -> usize {
        let mut n = 0;
        self.for_each_shape(&mut |_, _| n += 1);
        n
    }

    pub fn bounds(&self, tol: f64) -> Option<Bounds> {
        let mut b = None;
        self.for_each_shape(&mut |_, s| b = bounds_union(b, bounds_of(&s.world_geometry(tol))));
        b
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        if !(dx.is_finite() && dy.is_finite()) || (dx == 0.0 && dy == 0.0) {
            return;
        }
        self.for_each_shape_mut(&mut |s| {
            s.xf.cx += dx;
            s.xf.cy += dy;
            s.touch();
        });
    }

    /// Scale about `anchor` by (sx, sy) in world axes. Exact for unrotated shapes
    /// and multiples of 90°; other rotations get the closest size change.
    pub fn scale_about(&mut self, anchor: [f64; 2], sx: f64, sy: f64) {
        if !(sx.is_finite() && sy.is_finite()) || sx <= 0.0 || sy <= 0.0 {
            return;
        }
        self.for_each_shape_mut(&mut |s| {
            s.xf.cx = anchor[0] + (s.xf.cx - anchor[0]) * sx;
            s.xf.cy = anchor[1] + (s.xf.cy - anchor[1]) * sy;
            let r = s.xf.rotation_deg.rem_euclid(180.0);
            if r.abs() < 1e-9 || (r - 180.0).abs() < 1e-9 {
                s.xf.w *= sx;
                s.xf.h *= sy;
            } else if (r - 90.0).abs() < 1e-9 {
                s.xf.w *= sy;
                s.xf.h *= sx;
            } else {
                let u = (sx * sy).sqrt();
                s.xf.w *= u;
                s.xf.h *= u;
            }
            s.style.stroke_width *= (sx * sy).sqrt();
            s.touch();
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperPreset {
    A3,
    A4,
    A5,
    Letter,
    Custom,
}

impl PaperPreset {
    pub const ALL: [PaperPreset; 5] = [
        PaperPreset::A3,
        PaperPreset::A4,
        PaperPreset::A5,
        PaperPreset::Letter,
        PaperPreset::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaperPreset::A3 => "A3 (297 × 420 mm)",
            PaperPreset::A4 => "A4 (210 × 297 mm)",
            PaperPreset::A5 => "A5 (148 × 210 mm)",
            PaperPreset::Letter => "Letter (215.9 × 279.4 mm)",
            PaperPreset::Custom => "Custom",
        }
    }

    /// Portrait size in mm.
    pub fn size(self) -> Option<(f64, f64)> {
        match self {
            PaperPreset::A3 => Some((297.0, 420.0)),
            PaperPreset::A4 => Some((210.0, 297.0)),
            PaperPreset::A5 => Some((148.0, 210.0)),
            PaperPreset::Letter => Some((215.9, 279.4)),
            PaperPreset::Custom => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artboard {
    pub preset: PaperPreset,
    pub width: f64,
    pub height: f64,
}

impl Default for Artboard {
    fn default() -> Self {
        Self {
            preset: PaperPreset::A4,
            width: 210.0,
            height: 297.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DocSettings {
    /// Curve flattening tolerance (mm).
    pub flatten_tolerance: f64,
    pub expand_mode: ExpandMode,
    /// Minimum piece area (mm²), for Apply and Flatten to Visible.
    pub min_piece_area: f64,
    /// Run Flatten to Visible automatically on every paste.
    pub auto_flatten_paste: bool,
    /// Flatten to Visible unions touching/overlapping areas of the same colour.
    pub merge_same_color: bool,
    /// A colour counts as white (the artboard) if every RGB channel is ≥ this.
    pub white_threshold: u8,
}

impl Default for DocSettings {
    fn default() -> Self {
        Self {
            flatten_tolerance: 0.01,
            expand_mode: ExpandMode::Crop,
            min_piece_area: 0.01,
            auto_flatten_paste: true,
            merge_same_color: true,
            white_threshold: 250,
        }
    }
}

impl DocSettings {
    pub fn tolerance(&self) -> f64 {
        crate::shapes::clamp_finite(self.flatten_tolerance, 0.0005, 10.0, 0.01)
    }

    pub fn expand_options(&self, fast_preview: bool) -> ExpandOptions {
        ExpandOptions {
            mode: self.expand_mode,
            min_area: crate::shapes::clamp_finite(self.min_piece_area, 0.0, 1.0e6, 0.01),
            max_tiles: MAX_TILES,
            tolerance: self.tolerance(),
            fast_preview,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub objects: Vec<Object>,
    pub next_id: ObjId,
    #[serde(default)]
    pub artboard: Artboard,
    #[serde(default)]
    pub settings: DocSettings,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            objects: Vec::new(),
            next_id: 1,
            artboard: Artboard::default(),
            settings: DocSettings::default(),
        }
    }
}

/// The pattern pieces of a patterned shape — used by the live preview, by
/// Apply and by SVG export, so all three always agree.
pub fn pattern_pieces(shape: &ShapeObj, settings: &DocSettings, fast_preview: bool) -> ExpandResult {
    let Some(fill) = &shape.pattern else {
        return ExpandResult::default();
    };
    let geom = pattern_region(shape, settings.tolerance());
    let frame = fill.world_frame(&shape.xf);
    expand_region(&geom, &fill.pattern, &frame, &settings.expand_options(fast_preview))
}

/// SVG's default miter limit is 4: corners sharper than 2·asin(1/4) ≈ 29° are bevelled.
const MITER_MIN_ANGLE: f64 = 0.505_360_510_284_157;

/// The area a pattern fill covers: the shape itself, plus its outline when the
/// fill includes it. The outline is a stroke centred on the edge, so shape ∪
/// stroke = the shape grown outward by half the stroke width (mitred corners,
/// as SVG draws them). Using one region keeps the pattern seamless across the
/// fill/outline boundary: a tile on the edge is cut once, not twice.
pub fn pattern_region(shape: &ShapeObj, tol: f64) -> MultiPolygon<f64> {
    use geo::algorithm::buffer::{Buffer, BufferStyle, LineJoin};
    let geom = shape.world_geometry(tol);
    let half = shape.style.stroke_width / 2.0;
    let outlined = shape.pattern.as_ref().is_some_and(|f| f.include_outline)
        && shape.style.stroke.is_some()
        && half.is_finite()
        && half > 1.0e-6;
    if !outlined || geom.0.is_empty() {
        return geom;
    }
    let grown = geom.buffer_with_style(BufferStyle::new(half).line_join(LineJoin::Miter(MITER_MIN_ANGLE)));
    if grown.0.is_empty() { geom } else { grown }
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    pub objects_created: usize,
    pub shapes_expanded: usize,
    pub new_groups: Vec<ObjId>,
    pub capped: bool,
    pub empty: usize,
}

impl Document {
    pub fn new_id(&mut self) -> ObjId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_shape(&mut self, name: impl Into<String>, shape: ShapeObj) -> ObjId {
        let id = self.new_id();
        self.objects.push(Object {
            id,
            name: name.into(),
            kind: ObjectKind::Shape(shape),
        });
        id
    }

    pub fn get(&self, id: ObjId) -> Option<&Object> {
        fn find(list: &[Object], id: ObjId) -> Option<&Object> {
            for o in list {
                if o.id == id {
                    return Some(o);
                }
                if let ObjectKind::Group(c) = &o.kind
                    && let Some(f) = find(c, id)
                {
                    return Some(f);
                }
            }
            None
        }
        find(&self.objects, id)
    }

    pub fn get_mut(&mut self, id: ObjId) -> Option<&mut Object> {
        fn holds(list: &[Object], id: ObjId) -> bool {
            list.iter().any(|o| o.id == id || matches!(&o.kind, ObjectKind::Group(c) if holds(c, id)))
        }
        fn find(list: &mut [Object], id: ObjId) -> Option<&mut Object> {
            for o in list {
                if o.id == id {
                    return Some(o);
                }
                // Only unshare (copy-on-write) the group that actually holds the target.
                if matches!(&o.kind, ObjectKind::Group(c) if holds(c, id)) {
                    return find(o.children_mut()?, id);
                }
            }
            None
        }
        find(&mut self.objects, id)
    }

    pub fn shape_mut(&mut self, id: ObjId) -> Option<&mut ShapeObj> {
        self.get_mut(id).and_then(Object::as_shape_mut)
    }

    pub fn top_index(&self, id: ObjId) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    /// Sorted, de-duplicated top-level indices of `ids` (ids that are not
    /// top-level objects are skipped). Linear time even for 200 000 ids.
    pub fn top_indices(&self, ids: &[ObjId]) -> Vec<usize> {
        let mut idx: Vec<usize> = if ids.len() <= 16 {
            ids.iter().filter_map(|&id| self.top_index(id)).collect()
        } else {
            let want: std::collections::HashSet<ObjId> = ids.iter().copied().collect();
            self.objects.iter().enumerate().filter(|(_, o)| want.contains(&o.id)).map(|(i, _)| i).collect()
        };
        idx.sort_unstable();
        idx.dedup();
        idx
    }

    /// Remove objects (at any depth). Returns how many were removed.
    pub fn delete(&mut self, ids: &[ObjId]) -> usize {
        use std::collections::HashSet;
        fn holds_any(list: &[Object], ids: &HashSet<ObjId>) -> bool {
            list.iter().any(|o| ids.contains(&o.id) || matches!(&o.kind, ObjectKind::Group(c) if holds_any(c, ids)))
        }
        fn retain(list: &mut Vec<Object>, ids: &HashSet<ObjId>) -> usize {
            let before = list.len();
            list.retain(|o| !ids.contains(&o.id));
            let mut n = before - list.len();
            for o in list.iter_mut() {
                if matches!(&o.kind, ObjectKind::Group(c) if holds_any(c, ids))
                    && let Some(c) = o.children_mut()
                {
                    n += retain(c, ids);
                }
            }
            n
        }
        // A set: deleting many objects (e.g. all pieces of an expansion) stays linear.
        let ids: HashSet<ObjId> = ids.iter().copied().collect();
        let n = retain(&mut self.objects, &ids);
        // Drop groups that became empty.
        self.objects.retain(|o| !matches!(&o.kind, ObjectKind::Group(c) if c.is_empty()));
        n
    }

    pub fn bounds_of_ids(&self, ids: &[ObjId]) -> Option<Bounds> {
        let tol = self.settings.tolerance();
        ids.iter()
            .filter_map(|&id| self.get(id))
            .fold(None, |b, o| bounds_union(b, o.bounds(tol)))
    }

    pub fn all_bounds(&self) -> Option<Bounds> {
        let tol = self.settings.tolerance();
        self.objects.iter().fold(None, |b, o| bounds_union(b, o.bounds(tol)))
    }

    /// Move objects (together, keeping their layout) so the centre of their
    /// combined bounds is the centre of the artboard.
    pub fn center_on_artboard(&mut self, ids: &[ObjId]) {
        let Some(b) = self.bounds_of_ids(ids) else { return };
        let dx = self.artboard.width / 2.0 - (b[0] + b[2]) / 2.0;
        let dy = self.artboard.height / 2.0 - (b[1] + b[3]) / 2.0;
        for id in ids {
            if let Some(o) = self.get_mut(*id) {
                o.translate(dx, dy);
            }
        }
    }

    /// Group top-level objects (kept in z-order) into a new group.
    pub fn group(&mut self, ids: &[ObjId]) -> Option<ObjId> {
        let idx = self.top_indices(ids);
        if idx.len() < 2 {
            return None;
        }
        let insert_at = *idx.last()? + 1 - idx.len();
        let mut children = Vec::with_capacity(idx.len());
        for &i in idx.iter().rev() {
            children.push(self.objects.remove(i));
        }
        children.reverse();
        let id = self.new_id();
        self.objects.insert(insert_at, Object::group(id, format!("Group {id}"), children));
        Some(id)
    }

    /// Ungroup top-level groups; returns the ids of the released children.
    pub fn ungroup(&mut self, ids: &[ObjId]) -> Vec<ObjId> {
        let ids: std::collections::HashSet<ObjId> = ids.iter().copied().collect();
        let mut released = Vec::new();
        let mut i = 0;
        while i < self.objects.len() {
            if ids.contains(&self.objects[i].id) && self.objects[i].is_group() {
                let g = self.objects.remove(i);
                if let ObjectKind::Group(children) = g.kind {
                    // Take the children (copy only if an undo snapshot shares them).
                    let children = Arc::try_unwrap(children).unwrap_or_else(|a| (*a).clone());
                    let n = children.len();
                    released.extend(children.iter().map(|c| c.id));
                    // One splice instead of n inserts (n can be 200 000).
                    self.objects.splice(i..i, children);
                    i += n;
                    continue;
                }
            }
            i += 1;
        }
        released
    }

    /// Combine top-level shapes into one compound shape using the even-odd
    /// rule (overlaps become holes), like Illustrator's "Make Compound Path".
    /// The result takes the style of the bottom-most shape.
    pub fn make_compound(&mut self, ids: &[ObjId]) -> Option<ObjId> {
        let tol = self.settings.tolerance();
        let idx: Vec<usize> =
            self.top_indices(ids).into_iter().filter(|&i| self.objects[i].as_shape().is_some()).collect();
        if idx.len() < 2 {
            return None;
        }
        let mut acc = MultiPolygon::<f64>::new(vec![]);
        for &i in &idx {
            let g = self.objects[i].as_shape()?.world_geometry(tol);
            acc = acc.xor(&g);
        }
        // Edges that coincide (touching pieces) leave hairline slivers in an XOR.
        let acc = crate::shapes::drop_tiny_parts(acc, self.settings.min_piece_area.min(1e-3));
        let (geom, xf) = custom_from_world(&acc)?;
        let style = self.objects[idx[0]].as_shape()?.style.clone();
        let insert_at = *idx.last()? + 1 - idx.len();
        for &i in idx.iter().rev() {
            self.objects.remove(i);
        }
        let id = self.new_id();
        self.objects.insert(
            insert_at,
            Object {
                id,
                name: format!("Compound {id}"),
                kind: ObjectKind::Shape(ShapeObj::new(geom, xf, style)),
            },
        );
        Some(id)
    }

    /// Expand the pattern fill of each given top-level shape into a group of
    /// individual pieces ("Expand → Crop → keep as group").
    pub fn apply_patterns(&mut self, ids: &[ObjId]) -> ApplyReport {
        let mut report = ApplyReport::default();
        for &id in ids {
            let Some(idx) = self.top_index(id) else { continue };
            let Some(shape) = self.objects[idx].as_shape().cloned() else { continue };
            let Some(fill) = shape.pattern.clone() else { continue };
            let res = pattern_pieces(&shape, &self.settings, false);
            report.capped |= res.capped;
            let src_name = self.objects[idx].name.clone();
            let mut children = Vec::with_capacity(res.pieces.len() + 1);
            if fill.keep_background {
                let mut bg = shape.clone();
                bg.pattern = None;
                bg.touch();
                let bid = self.new_id();
                children.push(Object {
                    id: bid,
                    name: format!("{src_name} background"),
                    kind: ObjectKind::Shape(bg),
                });
            }
            for piece in res.pieces {
                let mp = MultiPolygon::new(vec![piece.poly]);
                let Some((geom, xf)) = custom_from_world(&mp) else { continue };
                let style = Style::fill_only(piece.color);
                let pid = self.new_id();
                children.push(Object {
                    id: pid,
                    name: format!("piece_{:04}", children.len() + 1),
                    kind: ObjectKind::Shape(ShapeObj::new(geom, xf, style)),
                });
            }
            let pieces = children.len() - usize::from(fill.keep_background);
            if pieces == 0 {
                report.empty += 1;
                continue;
            }
            let gid = self.new_id();
            self.objects[idx] = Object::group(gid, format!("{src_name} (expanded)"), children);
            report.objects_created += pieces;
            report.shapes_expanded += 1;
            report.new_groups.push(gid);
        }
        report
    }

    /// Make the document safe after loading from disk.
    pub fn sanitize(&mut self) {
        fn max_id(list: &[Object]) -> ObjId {
            list.iter()
                .map(|o| match &o.kind {
                    ObjectKind::Group(c) => o.id.max(max_id(c)),
                    ObjectKind::Shape(_) => o.id,
                })
                .max()
                .unwrap_or(0)
        }
        self.next_id = self.next_id.max(max_id(&self.objects) + 1);
        let a = &mut self.artboard;
        a.width = crate::shapes::clamp_finite(a.width, 1.0, 100_000.0, 210.0);
        a.height = crate::shapes::clamp_finite(a.height, 1.0, 100_000.0, 297.0);
        for o in &mut self.objects {
            o.for_each_shape_mut(&mut |s| {
                s.xf = s.xf.sanitized();
                s.touch();
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Undo / redo (snapshot stack)
// ---------------------------------------------------------------------------

/// Undo keeps at least this many steps…
const MIN_UNDO_STEPS: usize = 5;
/// …and otherwise at most this many top-level objects across all snapshots.
const UNDO_OBJECT_BUDGET: usize = 2_000_000;

pub struct History {
    undo: Vec<Document>,
    redo: Vec<Document>,
    coalesce: Option<u64>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: None,
            limit: 100,
        }
    }
}

impl History {
    /// Record `doc` (the state *before* an edit).
    pub fn push(&mut self, doc: &Document) {
        self.coalesce = None;
        self.push_inner(doc);
    }

    /// Like [`push`](Self::push), but consecutive calls with the same key
    /// (e.g. one slider drag) produce a single undo step.
    pub fn push_coalesced(&mut self, doc: &Document, key: u64) {
        if self.coalesce == Some(key) {
            return;
        }
        self.push_inner(doc);
        self.coalesce = Some(key);
    }

    /// End the current coalescing run.
    pub fn end_coalescing(&mut self) {
        self.coalesce = None;
    }

    fn push_inner(&mut self, doc: &Document) {
        // Cheap: groups (and custom geometry) are shared with the live document;
        // only top-level object headers are copied.
        self.undo.push(doc.clone());
        // Keep at most `limit` steps, and bound memory for documents with very
        // many loose top-level objects (e.g. 200 000 ungrouped pieces).
        let weight = |d: &Document| d.objects.len();
        let mut total: usize = self.undo.iter().map(weight).sum();
        let mut drop = self.undo.len().saturating_sub(self.limit);
        while self.undo.len() - drop > MIN_UNDO_STEPS && total > UNDO_OBJECT_BUDGET {
            total -= weight(&self.undo[drop]);
            drop += 1;
        }
        if drop > 0 {
            self.undo.drain(..drop);
        }
        self.redo.clear();
    }

    /// Drop the last pushed snapshot (an edit that turned out to change nothing).
    pub fn discard_last(&mut self) {
        self.undo.pop();
        self.coalesce = None;
    }

    pub fn undo(&mut self, doc: &mut Document) -> bool {
        self.coalesce = None;
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(doc, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, doc: &mut Document) -> bool {
        self.coalesce = None;
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(doc, next));
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.coalesce = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{Layout, Pattern, PatternFrame, generate_tiles};

    impl Object {
        fn kind_group(&self) -> Arc<Vec<Object>> {
            match &self.kind {
                ObjectKind::Group(c) => c.clone(),
                ObjectKind::Shape(_) => panic!("not a group"),
            }
        }
    }
    use crate::shapes::{Rings, rotate_vec};
    use geo::Area;

    fn patterned_doc() -> (Document, ObjId) {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(
            ShapeGeom::Star {
                points: 6,
                inner_ratio: 0.5,
            },
            Transform::new(100.0, 120.0, 90.0, 80.0),
            Style::default(),
        );
        let p = Pattern {
            layout: Layout::Honeycomb,
            cell_size: 8.0,
            gap: 0.4,
            ..Pattern::default()
        };
        s.pattern = Some(PatternFill::new(p, &s.xf));
        let id = doc.add_shape("Star", s);
        (doc, id)
    }

    fn world_rings(o: &Object, tol: f64) -> Vec<Rings> {
        let s = o.as_shape().unwrap();
        s.world_geometry(tol).0.iter().map(Rings::from_polygon).collect()
    }

    fn same_ring(a: &[[f64; 2]], b: &[[f64; 2]]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(p, q)| (p[0] - q[0]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6)
    }

    /// WYSIWYG: after moving/scaling/rotating the pattern, Apply yields exactly
    /// the preview geometry, and tiles sit where the pattern frame says.
    #[test]
    fn apply_matches_preview_after_pattern_transform() {
        let (mut doc, id) = patterned_doc();
        {
            let s = doc.shape_mut(id).unwrap();
            let fill = s.pattern.as_mut().unwrap();
            let mut f = fill.world_frame(&s.xf);
            f.origin = [f.origin[0] + 3.7, f.origin[1] - 1.2]; // move
            f.scale *= 1.35; // scale
            f.rotation_deg += 22.5; // rotate
            fill.set_world_frame(&s.xf, f);
            s.touch();
        }
        let shape = doc.get(id).unwrap().as_shape().unwrap().clone();
        let preview = pattern_pieces(&shape, &doc.settings, false);
        assert!(preview.pieces.len() > 20);

        // Tile positions match the frame: cell (0,0) is at the frame origin and
        // cell (1,0) one column further along the rotated lattice.
        let frame = shape.pattern.as_ref().unwrap().world_frame(&shape.xf);
        let expected_origin = [100.0 + 3.7, 120.0 - 1.2];
        assert!((frame.origin[0] - expected_origin[0]).abs() < 1e-9);
        let ts = generate_tiles(&shape.pattern.as_ref().unwrap().pattern, &frame, [0.0, 0.0, 200.0, 240.0], 0.01, MAX_TILES);
        let c00 = ts.tiles.iter().find(|t| t.cell == [0, 0]).unwrap().center;
        assert!((c00[0] - expected_origin[0]).abs() < 1e-9 && (c00[1] - expected_origin[1]).abs() < 1e-9);
        let c10 = ts.tiles.iter().find(|t| t.cell == [1, 0]).unwrap().center;
        let along = 8.0 * 3.0_f64.sqrt() / 2.0 * 1.35;
        let d = rotate_vec([along, 8.0 * 1.35 / 2.0], 22.5);
        assert!((c10[0] - (c00[0] + d[0])).abs() < 1e-9 && (c10[1] - (c00[1] + d[1])).abs() < 1e-9);

        // Apply and compare piece by piece.
        let report = doc.apply_patterns(&[id]);
        assert_eq!(report.objects_created, preview.pieces.len());
        let group = &doc.objects[0];
        let ObjectKind::Group(children) = &group.kind else { panic!("expected group") };
        assert_eq!(children.len(), preview.pieces.len());
        let tol = doc.settings.tolerance();
        for (child, piece) in children.iter().zip(&preview.pieces) {
            let got = world_rings(child, tol);
            let want = Rings::from_polygon(&piece.poly);
            assert_eq!(got.len(), 1);
            assert!(same_ring(&got[0].exterior, &want.exterior));
            assert_eq!(child.as_shape().unwrap().style.fill, Some(piece.color));
        }
    }

    /// The pattern covers the outline too, as one seamless region.
    #[test]
    fn pattern_covers_outline_seamlessly() {
        let settings = DocSettings::default();
        let style = Style {
            stroke_width: 2.0,
            ..Style::default()
        };
        let mut s = ShapeObj::new(ShapeGeom::Rect, Transform::new(10.0, 10.0, 20.0, 20.0), style);
        let p = Pattern {
            layout: Layout::Square,
            cell_size: 7.0,
            gap: 0.0,
            ..Pattern::default()
        };
        s.pattern = Some(PatternFill::new(p, &s.xf));

        // Gap 0: the pieces tile shape + outline exactly. The 20 mm square with a
        // 2 mm stroke covers 22 × 22 mm (sharp mitred corners, like SVG).
        let res = pattern_pieces(&s, &settings, false);
        let area: f64 = res.pieces.iter().map(|p| p.poly.unsigned_area()).sum();
        assert!((area - 22.0 * 22.0).abs() < 1e-6, "area {area}");
        let b = bounds_of(&MultiPolygon::new(res.pieces.iter().map(|p| p.poly.clone()).collect())).unwrap();
        assert!((b[0] - -1.0).abs() < 1e-9 && (b[2] - 21.0).abs() < 1e-9);

        // Seamless: a tile crossing the shape edge stays ONE piece (it is not cut
        // into an interior part and an outline part).
        let mut cells: Vec<[i64; 2]> = res.pieces.iter().map(|p| p.cell).collect();
        let n = cells.len();
        cells.sort_unstable();
        cells.dedup();
        assert_eq!(cells.len(), n, "every cell gives exactly one piece");

        // Switched off, or without a stroke, the pattern stops at the shape edge.
        let mut off = s.clone();
        off.pattern.as_mut().unwrap().include_outline = false;
        let a_off: f64 = pattern_pieces(&off, &settings, false).pieces.iter().map(|p| p.poly.unsigned_area()).sum();
        assert!((a_off - 400.0).abs() < 1e-6);
        let mut no_stroke = s.clone();
        no_stroke.style.stroke = None;
        let a_ns: f64 = pattern_pieces(&no_stroke, &settings, false).pieces.iter().map(|p| p.poly.unsigned_area()).sum();
        assert!((a_ns - 400.0).abs() < 1e-6);

        // Apply produces the same outline-covering pieces as the preview.
        let mut doc = Document::default();
        let id = doc.add_shape("r", s);
        let report = doc.apply_patterns(&[id]);
        assert_eq!(report.objects_created, n);
    }

    /// Undo snapshots share big groups instead of copying every piece; an edit
    /// copies only the group it touches, and undo still restores it exactly.
    #[test]
    fn undo_snapshots_share_groups_copy_on_write() {
        let (mut doc, id) = patterned_doc();
        doc.apply_patterns(&[id]);
        let other = doc.add_shape("other", ShapeObj::new(ShapeGeom::Rect, Transform::new(5.0, 5.0, 4.0, 4.0), Style::default()));
        let group_arc = |d: &Document| match &d.objects[0].kind {
            ObjectKind::Group(c) => c.clone(),
            ObjectKind::Shape(_) => panic!("expected the expanded group"),
        };
        let mut hist = History::default();
        hist.push(&doc);
        assert!(Arc::ptr_eq(&group_arc(&doc), &hist.undo[0].objects[0].kind_group()), "snapshot shares the group");

        // Editing another object does not copy the group.
        doc.get_mut(other).unwrap().translate(1.0, 0.0);
        assert!(Arc::ptr_eq(&group_arc(&doc), &hist.undo[0].objects[0].kind_group()));

        // Editing a piece copies that group once; the snapshot keeps the original.
        let piece = group_arc(&doc)[0].id;
        let cx0 = doc.get(piece).unwrap().as_shape().unwrap().xf.cx;
        doc.get_mut(piece).unwrap().translate(5.0, 0.0);
        assert!(!Arc::ptr_eq(&group_arc(&doc), &hist.undo[0].objects[0].kind_group()));
        assert_eq!(doc.get(piece).unwrap().as_shape().unwrap().xf.cx, cx0 + 5.0);
        assert!(hist.undo(&mut doc));
        assert_eq!(doc.get(piece).unwrap().as_shape().unwrap().xf.cx, cx0);

        // Deleting and ungrouping large selections works (linear-time paths).
        let gid = doc.objects[0].id;
        let released = doc.ungroup(&[gid]);
        assert!(released.len() > 20);
        assert_eq!(doc.top_indices(&released).len(), released.len());
        assert_eq!(doc.delete(&released), released.len());
        assert_eq!(doc.objects.len(), 1, "only `other` is left");
    }

    #[test]
    fn apply_is_undoable_and_ungroup_releases_pieces() {
        let (mut doc, id) = patterned_doc();
        let mut hist = History::default();
        let star_area = doc.get(id).unwrap().as_shape().unwrap().world_geometry(0.01).unsigned_area();
        hist.push(&doc);
        let report = doc.apply_patterns(&[id]);
        assert!(report.objects_created > 0);
        assert!(doc.objects[0].is_group());
        let gid = report.new_groups[0];

        hist.push(&doc);
        let released = doc.ungroup(&[gid]);
        assert_eq!(released.len(), report.objects_created);
        assert_eq!(doc.objects.len(), report.objects_created);
        let total: f64 = doc
            .objects
            .iter()
            .map(|o| o.as_shape().unwrap().world_geometry(0.01).unsigned_area())
            .sum();
        assert!(total < star_area); // gaps between tiles

        assert!(hist.undo(&mut doc));
        assert!(doc.objects[0].is_group());
        assert!(hist.undo(&mut doc));
        assert!(doc.objects[0].has_pattern());
        assert!(hist.redo(&mut doc));
        assert!(doc.objects[0].is_group());
    }

    #[test]
    fn group_ungroup_roundtrip() {
        let mut doc = Document::default();
        let a = doc.add_shape("a", ShapeObj::new(ShapeGeom::Rect, Transform::new(0.0, 0.0, 1.0, 1.0), Style::default()));
        let b = doc.add_shape("b", ShapeObj::new(ShapeGeom::Ellipse, Transform::new(5.0, 0.0, 1.0, 1.0), Style::default()));
        let c = doc.add_shape("c", ShapeObj::new(ShapeGeom::Rect, Transform::new(9.0, 0.0, 1.0, 1.0), Style::default()));
        let g = doc.group(&[c, a]).unwrap();
        assert_eq!(doc.objects.len(), 2);
        assert_eq!(doc.objects[0].id, b);
        assert_eq!(doc.objects[1].id, g);
        let r = doc.ungroup(&[g]);
        assert_eq!(r, vec![a, c]);
        assert_eq!(doc.delete(&[a, b]), 2);
        assert_eq!(doc.objects.len(), 1);
    }

    #[test]
    fn compound_path_makes_holes_and_expands() {
        let mut doc = Document::default();
        let outer = doc.add_shape("r", ShapeObj::new(ShapeGeom::Rect, Transform::new(50.0, 50.0, 80.0, 60.0), Style::default()));
        let inner = doc.add_shape("e", ShapeObj::new(ShapeGeom::Ellipse, Transform::new(50.0, 50.0, 30.0, 30.0), Style::default()));
        let id = doc.make_compound(&[inner, outer]).unwrap();
        assert_eq!(doc.objects.len(), 1);
        let s = doc.get(id).unwrap().as_shape().unwrap().clone();
        let g = s.world_geometry(0.01);
        assert_eq!(g.0.len(), 1);
        assert_eq!(g.0[0].interiors().len(), 1, "the ellipse becomes a hole");
        let ring_area = 80.0 * 60.0 - std::f64::consts::PI * 15.0 * 15.0;
        assert!((g.unsigned_area() - ring_area).abs() / ring_area < 0.001);

        // Expanding a pattern on the holed shape keeps the hole empty.
        let mut s2 = s;
        s2.pattern = Some(PatternFill::new(
            Pattern {
                layout: Layout::Square,
                cell_size: 6.0,
                gap: 0.0,
                ..Pattern::default()
            },
            &s2.xf,
        ));
        let res = pattern_pieces(&s2, &doc.settings, false);
        // The pattern covers the shape plus its outline (the hole shrinks by
        // half the stroke width but stays empty).
        let region = pattern_region(&s2, doc.settings.tolerance());
        assert_eq!(region.0.len(), 1);
        assert_eq!(region.0[0].interiors().len(), 1, "the hole survives the outline");
        assert!(region.unsigned_area() > g.unsigned_area());
        assert!((res.total_area() - region.unsigned_area()).abs() / ring_area < 0.005);
    }

    #[test]
    fn pattern_frame_used_by_preview_is_the_frame_api() {
        let (doc, id) = patterned_doc();
        let s = doc.get(id).unwrap().as_shape().unwrap();
        let f: PatternFrame = s.pattern.as_ref().unwrap().world_frame(&s.xf);
        assert_eq!(f.origin, [100.0, 120.0]);
    }
}
