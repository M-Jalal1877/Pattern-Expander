//! The Apply / Expand algorithm: pattern tiles -> individual polygons clipped to a shape.
//!
//! Pure geometry, no egui. The live preview and "Apply" both call
//! [`expand_region`], so what is shown is exactly what gets expanded.

use crate::pattern::{MAX_TILES, Pattern, PatternFrame, generate_tiles, world_cell_size};
use crate::shapes::{Bounds, Color, bounds_of, bounds_overlap};
use geo::{Area, BooleanOps, BoundingRect, Contains, Intersects, MultiPolygon, Polygon};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpandMode {
    /// Intersect each tile with the shape.
    Crop,
    /// Keep every whole tile that touches the shape.
    WholeTouching,
    /// Keep only whole tiles fully inside the shape.
    WholeInside,
}

impl ExpandMode {
    pub const ALL: [ExpandMode; 3] = [ExpandMode::Crop, ExpandMode::WholeTouching, ExpandMode::WholeInside];

    pub fn label(self) -> &'static str {
        match self {
            ExpandMode::Crop => "Crop to shape",
            ExpandMode::WholeTouching => "Keep whole tiles – touching",
            ExpandMode::WholeInside => "Keep whole tiles – inside only",
        }
    }
}

/// The minimum-area filter never removes a piece bigger than this fraction of a
/// full tile, however small the tiles are.
pub const SLIVER_FRACTION: f64 = 0.02;

#[derive(Clone, Debug)]
pub struct ExpandOptions {
    pub mode: ExpandMode,
    /// Pieces smaller than this (mm²) are dropped — capped at
    /// `SLIVER_FRACTION` of a full tile, so tiny tiles are never wiped out.
    pub min_area: f64,
    pub max_tiles: usize,
    /// Curve flattening tolerance (mm).
    pub tolerance: f64,
    /// Fast, approximate preview: boundary tiles are skipped instead of clipped.
    pub fast_preview: bool,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            mode: ExpandMode::Crop,
            min_area: 0.01,
            max_tiles: MAX_TILES,
            tolerance: 0.01,
            fast_preview: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Piece {
    pub poly: Polygon<f64>,
    pub color: Color,
    /// Lattice cell this piece came from.
    pub cell: [i64; 2],
}

#[derive(Clone, Debug, Default)]
pub struct ExpandResult {
    pub pieces: Vec<Piece>,
    pub tiles_generated: usize,
    /// Tiles kept without running a boolean operation.
    pub tiles_whole: usize,
    /// Tiles clipped with a boolean intersection.
    pub tiles_clipped: usize,
    /// Pieces removed by the minimum-area filter.
    pub removed_small: usize,
    /// The minimum piece area actually applied (the setting, capped at
    /// `SLIVER_FRACTION` of a full tile).
    pub min_area_used: f64,
    /// Tile generation hit the cap.
    pub capped: bool,
    /// Boundary tiles were skipped (fast preview).
    pub approximate: bool,
}

impl ExpandResult {
    pub fn total_area(&self) -> f64 {
        self.pieces.iter().map(|p| p.poly.unsigned_area()).sum()
    }
}

/// Grid index over the shape's boundary segments, used to find tiles that the
/// boundary cannot cross (those are entirely inside or outside).
struct EdgeIndex {
    segs: Vec<[f64; 4]>,
    min: [f64; 2],
    cell: f64,
    nx: usize,
    ny: usize,
    buckets: Vec<Vec<u32>>,
    /// Segments overlapping each grid row (for fast even-odd ray casting).
    rows: Vec<Vec<u32>>,
}

impl EdgeIndex {
    fn new(mp: &MultiPolygon<f64>, bounds: Bounds, cell_hint: f64) -> Self {
        let mut segs = Vec::new();
        for poly in &mp.0 {
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                let pts = &ring.0;
                for w in pts.windows(2) {
                    segs.push([w[0].x, w[0].y, w[1].x, w[1].y]);
                }
                if let (Some(a), Some(b)) = (pts.last(), pts.first())
                    && a != b
                {
                    segs.push([a.x, a.y, b.x, b.y]);
                }
            }
        }
        let (w, h) = (bounds[2] - bounds[0], bounds[3] - bounds[1]);
        let extent = w.max(h).max(1.0e-9);
        let cell = cell_hint.max(extent / 256.0).max(1.0e-9);
        let nx = ((w / cell).ceil() as usize).clamp(1, 256);
        let ny = ((h / cell).ceil() as usize).clamp(1, 256);
        let mut idx = Self {
            segs,
            min: [bounds[0], bounds[1]],
            cell,
            nx,
            ny,
            buckets: vec![Vec::new(); nx * ny],
            rows: vec![Vec::new(); ny],
        };
        for (k, s) in idx.segs.iter().enumerate() {
            let (i0, j0, i1, j1) = idx.cell_range(&[s[0].min(s[2]), s[1].min(s[3]), s[0].max(s[2]), s[1].max(s[3])]);
            for j in j0..=j1 {
                idx.rows[j].push(k as u32);
                for i in i0..=i1 {
                    idx.buckets[j * nx + i].push(k as u32);
                }
            }
        }
        idx
    }

    /// Even-odd point-in-shape test using only the segments of the point's row.
    fn contains(&self, p: [f64; 2]) -> bool {
        let max_y = self.min[1] + self.cell * self.ny as f64;
        if p[1] < self.min[1] || p[1] > max_y {
            return false;
        }
        let j = (((p[1] - self.min[1]) / self.cell).floor().max(0.0) as usize).min(self.ny - 1);
        let mut inside = false;
        for &k in &self.rows[j] {
            let s = &self.segs[k as usize];
            if (s[1] > p[1]) != (s[3] > p[1]) {
                let x = s[0] + (p[1] - s[1]) * (s[2] - s[0]) / (s[3] - s[1]);
                if p[0] < x {
                    inside = !inside;
                }
            }
        }
        inside
    }

    fn cell_range(&self, b: &Bounds) -> (usize, usize, usize, usize) {
        let f = |v: f64, o: f64, n: usize| (((v - o) / self.cell).floor().max(0.0) as usize).min(n - 1);
        (
            f(b[0], self.min[0], self.nx),
            f(b[1], self.min[1], self.ny),
            f(b[2], self.min[0], self.nx),
            f(b[3], self.min[1], self.ny),
        )
    }

    /// Does any boundary segment pass through the rectangle `b`?
    fn crosses(&self, b: &Bounds) -> bool {
        let (i0, j0, i1, j1) = self.cell_range(b);
        for j in j0..=j1 {
            for i in i0..=i1 {
                for &k in &self.buckets[j * self.nx + i] {
                    if segment_hits_rect(&self.segs[k as usize], b) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Liang–Barsky segment / axis-aligned rectangle test (inclusive).
fn segment_hits_rect(s: &[f64; 4], b: &Bounds) -> bool {
    let (x0, y0) = (s[0], s[1]);
    let (dx, dy) = (s[2] - s[0], s[3] - s[1]);
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;
    for (p, q) in [
        (-dx, x0 - b[0]),
        (dx, b[2] - x0),
        (-dy, y0 - b[1]),
        (dy, b[3] - y0),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

fn poly_bounds(p: &Polygon<f64>) -> Option<Bounds> {
    p.bounding_rect().map(|r| [r.min().x, r.min().y, r.max().x, r.max().y])
}

/// Expand `pattern` placed at `frame` inside `shape` (world mm).
pub fn expand_region(
    shape: &MultiPolygon<f64>,
    pattern: &Pattern,
    frame: &PatternFrame,
    opts: &ExpandOptions,
) -> ExpandResult {
    let mut res = ExpandResult::default();
    let Some(sb) = bounds_of(shape) else {
        return res;
    };
    if sb.iter().any(|v| !v.is_finite()) || shape.unsigned_area() <= 1.0e-12 {
        return res;
    }
    let min_area_setting = if opts.min_area.is_finite() { opts.min_area.max(0.0) } else { 0.0 };

    // 1. Bounding box enlarged by one cell, 2. all tiles covering it.
    let cell = world_cell_size(pattern, frame);
    let tiles = tiles_for_shape(pattern, frame, shape, sb, cell, opts.tolerance, opts.max_tiles.min(MAX_TILES));
    res.tiles_generated = tiles.tiles.len();

    // A "sliver" is small relative to the tile: with tiny tiles (small objects,
    // small pattern scale) an absolute limit alone would drop every cut piece —
    // or every tile. So the limit never exceeds a small fraction of a full tile.
    let tile_area = tiles.tiles.iter().take(64).map(|t| t.poly.unsigned_area()).fold(0.0, f64::max);
    let min_area = min_area_setting.min(tile_area * SLIVER_FRACTION);
    res.min_area_used = min_area;
    res.capped = tiles.capped;
    res.approximate = opts.fast_preview;

    let index = EdgeIndex::new(shape, sb, cell);
    // Tiles on the boundary are clipped against a small local piece of the
    // shape, never against the whole shape (thousands of vertices for text).
    let tile_extent = tiles
        .tiles
        .iter()
        .take(256)
        .filter_map(|t| poly_bounds(&t.poly))
        .map(|b| (b[2] - b[0]).max(b[3] - b[1]))
        .fold(0.0, f64::max);
    let mut local = LocalClip::new(shape, sb, tile_extent);
    let push =|res: &mut ExpandResult, poly: Polygon<f64>, color: Color, cell: [i64; 2]| {
        if poly.unsigned_area() < min_area {
            res.removed_small += 1;
        } else {
            res.pieces.push(Piece { poly, color, cell });
        }
    };

    for tile in tiles.tiles {
        // 3. Skip tiles whose bounding box misses the shape.
        let Some(tb) = poly_bounds(&tile.poly) else { continue };
        if !bounds_overlap(&tb, &sb) {
            continue;
        }
        // Classify: if no boundary segment crosses the tile's bbox, the tile is
        // entirely inside or entirely outside and no boolean op is needed.
        if !index.crosses(&tb) {
            let probe = [(tb[0] + tb[2]) / 2.0, (tb[1] + tb[3]) / 2.0];
            if index.contains(probe) {
                res.tiles_whole += 1;
                push(&mut res, tile.poly, tile.color, tile.cell);
            }
            continue;
        }
        if opts.fast_preview {
            continue;
        }
        // The shape restricted to a neighbourhood that contains the whole tile:
        // tile ∩ shape == tile ∩ local, at a fraction of the cost.
        let local = local.region(&tb);
        match opts.mode {
            ExpandMode::Crop => {
                res.tiles_clipped += 1;
                // 6. A tile cut into several parts yields several pieces.
                for part in tile.poly.intersection(local) {
                    push(&mut res, part, tile.color, tile.cell);
                }
            }
            ExpandMode::WholeTouching => {
                if tile.poly.intersects(local) {
                    push(&mut res, tile.poly, tile.color, tile.cell);
                }
            }
            ExpandMode::WholeInside => {
                if local.contains(&tile.poly) {
                    push(&mut res, tile.poly, tile.color, tile.cell);
                }
            }
        }
    }
    res
}

/// Tiles covering the shape, without wasting the tile budget on empty space.
///
/// The region is cut into roughly square blocks and tiles are generated only
/// in blocks that touch a part of the shape. This matters for thin shapes with
/// a rotated pattern (the lattice would otherwise be scanned over the rotated
/// bounding box of the whole strip) and for text (gaps between letters and
/// lines). Tiles are identified by lattice cell, so blocks never duplicate a
/// tile and the result is deterministic.
fn tiles_for_shape(
    pattern: &Pattern,
    frame: &PatternFrame,
    shape: &MultiPolygon<f64>,
    sb: Bounds,
    cell: f64,
    tol: f64,
    max: usize,
) -> crate::pattern::TileSet {
    let grow = |b: Bounds| [b[0] - cell, b[1] - cell, b[2] + cell, b[3] + cell];
    let full = grow(sb);
    let (w, h) = (full[2] - full[0], full[3] - full[1]);
    // Square blocks of the short side (≥ 32 cells), at most 64 per axis.
    let side = w.min(h).max(cell * 32.0).max(w.max(h) / 64.0);
    let (nx, ny) = (((w / side).ceil() as usize).max(1), ((h / side).ceil() as usize).max(1));
    let parts: Vec<Bounds> = shape.0.iter().filter_map(poly_bounds).map(grow).collect();
    if nx * ny == 1 || parts.len() > 20_000 {
        return generate_tiles(pattern, frame, full, tol, max);
    }
    let mut out = crate::pattern::TileSet::default();
    let mut seen = std::collections::HashSet::new();
    'blocks: for j in 0..ny {
        for i in 0..nx {
            let b = [
                full[0] + i as f64 * side,
                full[1] + j as f64 * side,
                (full[0] + (i + 1) as f64 * side).min(full[2]),
                (full[1] + (j + 1) as f64 * side).min(full[3]),
            ];
            if !parts.iter().any(|p| bounds_overlap(p, &b)) {
                continue; // empty space
            }
            let room = max.saturating_sub(out.tiles.len());
            if room == 0 {
                out.capped = true;
                break 'blocks;
            }
            let ts = generate_tiles(pattern, frame, b, tol, room);
            out.estimated += ts.estimated;
            out.capped |= ts.capped;
            out.tiles.extend(ts.tiles.into_iter().filter(|t| seen.insert((t.cell, t.sub))));
        }
    }
    out
}

/// The shape cut into a coarse grid of local pieces, built lazily.
///
/// A tile is looked up by the grid square containing its bounding-box centre;
/// that square's piece is `shape ∩ (square grown by one tile extent)`, so it
/// contains the whole neighbourhood of the tile and `tile ∩ shape` equals
/// `tile ∩ piece` exactly. Pieces are cut with Sutherland–Hodgman (only the
/// outlines near the square are touched), which is linear and allocation-light.
struct LocalClip<'a> {
    shape: &'a MultiPolygon<f64>,
    part_bounds: Vec<Bounds>,
    min: [f64; 2],
    size: f64,
    nx: usize,
    ny: usize,
    margin: f64,
    cache: std::collections::HashMap<usize, MultiPolygon<f64>>,
}

impl<'a> LocalClip<'a> {
    fn new(shape: &'a MultiPolygon<f64>, sb: Bounds, tile_extent: f64) -> Self {
        let (w, h) = (sb[2] - sb[0], sb[3] - sb[1]);
        let extent = w.max(h).max(1e-9);
        // A few tiles per square, but never more than 128 × 128 squares.
        let size = (tile_extent * 6.0).max(extent / 128.0).max(1e-9);
        let nx = ((w / size).ceil() as usize).clamp(1, 129);
        let ny = ((h / size).ceil() as usize).clamp(1, 129);
        Self {
            shape,
            part_bounds: shape.0.iter().map(|p| poly_bounds(p).unwrap_or([0.0; 4])).collect(),
            min: [sb[0], sb[1]],
            size,
            nx,
            ny,
            margin: tile_extent.max(1e-9) * 1.01,
            cache: Default::default(),
        }
    }

    fn region(&mut self, tb: &Bounds) -> &MultiPolygon<f64> {
        let c = [(tb[0] + tb[2]) / 2.0, (tb[1] + tb[3]) / 2.0];
        let i = (((c[0] - self.min[0]) / self.size).floor().max(0.0) as usize).min(self.nx - 1);
        let j = (((c[1] - self.min[1]) / self.size).floor().max(0.0) as usize).min(self.ny - 1);
        let key = j * self.nx + i;
        if !self.cache.contains_key(&key) {
            let x0 = self.min[0] + i as f64 * self.size;
            let y0 = self.min[1] + j as f64 * self.size;
            // Edge squares stretch to infinity-ish so tiles past the grid are covered.
            let lo = |k: usize, v: f64| if k == 0 { f64::MIN / 4.0 } else { v - self.margin };
            let hi = |k: usize, n: usize, v: f64| if k + 1 == n { f64::MAX / 4.0 } else { v + self.margin };
            let r = [lo(i, x0), lo(j, y0), hi(i, self.nx, x0 + self.size), hi(j, self.ny, y0 + self.size)];
            let piece = self.cut(&r);
            self.cache.insert(key, piece);
        }
        &self.cache[&key]
    }

    /// `shape ∩ r` for an axis-aligned rectangle.
    fn cut(&self, r: &Bounds) -> MultiPolygon<f64> {
        let inside = |b: &Bounds| b[0] >= r[0] && b[1] >= r[1] && b[2] <= r[2] && b[3] <= r[3];
        let mut out = Vec::new();
        for (p, pb) in self.shape.0.iter().zip(&self.part_bounds) {
            if !bounds_overlap(pb, r) {
                continue;
            }
            if inside(pb) {
                out.push(p.clone());
                continue;
            }
            let Some(ext) = clip_ring(p.exterior(), r) else { continue };
            let holes = p
                .interiors()
                .iter()
                .filter_map(|h| {
                    let hb = h.bounding_rect().map(|b| [b.min().x, b.min().y, b.max().x, b.max().y])?;
                    if !bounds_overlap(&hb, r) {
                        None
                    } else if inside(&hb) {
                        Some(h.clone())
                    } else {
                        clip_ring(h, r)
                    }
                })
                .collect();
            out.push(Polygon::new(ext, holes));
        }
        MultiPolygon::new(out)
    }
}

/// Sutherland–Hodgman clip of a closed ring against an axis-aligned rectangle.
/// Concave rings may produce zero-width bridges along the rectangle's edges;
/// boolean operations treat those as nothing (they cancel out).
fn clip_ring(ring: &geo::LineString<f64>, r: &Bounds) -> Option<geo::LineString<f64>> {
    let mut pts: Vec<[f64; 2]> = ring.0.iter().map(|c| [c.x, c.y]).collect();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    // (axis, bound, keep the side ≥ bound?)
    for (axis, bound, keep_ge) in [(0, r[0], true), (0, r[2], false), (1, r[1], true), (1, r[3], false)] {
        if pts.is_empty() {
            return None;
        }
        let ins = |p: &[f64; 2]| if keep_ge { p[axis] >= bound } else { p[axis] <= bound };
        let mut out = Vec::with_capacity(pts.len() + 4);
        for k in 0..pts.len() {
            let (a, b) = (pts[(k + pts.len() - 1) % pts.len()], pts[k]);
            let (ia, ib) = (ins(&a), ins(&b));
            if ia != ib {
                let t = (bound - a[axis]) / (b[axis] - a[axis]);
                let mut q = [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
                q[axis] = bound;
                out.push(q);
            }
            if ib {
                out.push(b);
            }
        }
        pts = out;
    }
    if pts.len() < 3 {
        return None;
    }
    let ls = geo::LineString::from(pts);
    (Polygon::new(ls.clone(), vec![]).unsigned_area() > 1e-15).then_some(ls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::Layout;
    use crate::shapes::{Rings, ShapeGeom, Transform, world_geometry};

    fn star() -> MultiPolygon<f64> {
        world_geometry(
            &ShapeGeom::Star {
                points: 5,
                inner_ratio: 0.45,
            },
            &Transform::new(100.0, 100.0, 120.0, 120.0),
            0.01,
        )
    }

    fn honeycomb() -> Pattern {
        Pattern {
            layout: Layout::Honeycomb,
            cell_size: 9.0,
            gap: 0.0,
            ..Pattern::default()
        }
    }

    fn frame() -> PatternFrame {
        PatternFrame {
            origin: [3.0, 1.5],
            rotation_deg: 12.0,
            scale: 1.0,
        }
    }

    #[test]
    fn crop_star_honeycomb_preserves_area() {
        let shape = star();
        let res = expand_region(&shape, &honeycomb(), &frame(), &ExpandOptions::default());
        assert!(!res.pieces.is_empty());
        assert!(res.tiles_whole > 0 && res.tiles_clipped > 0);
        let a = res.total_area();
        let s = shape.unsigned_area();
        assert!((a - s).abs() / s < 0.005, "pieces {a} vs star {s}");
        // Every piece lies inside the star (up to floating-point noise on the cut edges).
        for p in &res.pieces {
            assert!(p.poly.difference(&shape).unsigned_area() < 1e-6);
        }
    }

    #[test]
    fn crop_works_for_shapes_with_holes() {
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let shape = MultiPolygon::new(vec![
            Rings {
                exterior: ring(0.0, 0.0, 100.0, 100.0),
                holes: vec![ring(30.0, 30.0, 70.0, 70.0)],
            }
            .to_polygon(),
        ]);
        let p = Pattern {
            layout: Layout::Triangle,
            cell_size: 7.0,
            gap: 0.0,
            ..Pattern::default()
        };
        let res = expand_region(&shape, &p, &frame(), &ExpandOptions::default());
        let expected = 100.0 * 100.0 - 40.0 * 40.0;
        assert!((res.total_area() - expected).abs() / expected < 0.005);
        // Nothing inside the hole.
        assert!(res.pieces.iter().all(|pc| !pc.poly.contains(&geo::Point::new(50.0, 50.0))));
    }

    #[test]
    fn whole_tile_modes() {
        let shape = star();
        let opts = |mode| ExpandOptions {
            mode,
            ..ExpandOptions::default()
        };
        let crop = expand_region(&shape, &honeycomb(), &frame(), &opts(ExpandMode::Crop));
        let touching = expand_region(&shape, &honeycomb(), &frame(), &opts(ExpandMode::WholeTouching));
        let inside = expand_region(&shape, &honeycomb(), &frame(), &opts(ExpandMode::WholeInside));
        assert!(!inside.pieces.is_empty());
        assert!(inside.pieces.len() < touching.pieces.len());
        // Inside-only tiles are entirely within the shape, touching covers more than the shape.
        assert!(inside.total_area() < shape.unsigned_area());
        assert!(touching.total_area() > shape.unsigned_area());
        for p in &inside.pieces {
            assert!(shape.contains(&p.poly));
        }
        // Whole tiles are uncut: all have the full hexagon area.
        let hex = 9.0_f64 * 9.0 * 3.0_f64.sqrt() / 2.0;
        for p in touching.pieces.iter().chain(&inside.pieces) {
            assert!((p.poly.unsigned_area() - hex).abs() < 1e-6);
        }
        assert!(crop.pieces.len() >= inside.pieces.len());
    }

    #[test]
    fn min_area_filter_removes_slivers() {
        let shape = star();
        let keep_all = expand_region(
            &shape,
            &honeycomb(),
            &frame(),
            &ExpandOptions {
                min_area: 0.0,
                ..ExpandOptions::default()
            },
        );
        let filtered = expand_region(
            &shape,
            &honeycomb(),
            &frame(),
            &ExpandOptions {
                min_area: 5.0,
                ..ExpandOptions::default()
            },
        );
        let limit = filtered.min_area_used;
        assert!(limit > 0.0 && limit <= 5.0);
        let small = keep_all.pieces.iter().filter(|p| p.poly.unsigned_area() < limit).count();
        assert!(small > 0, "test needs some small pieces");
        assert_eq!(filtered.pieces.len(), keep_all.pieces.len() - small);
        assert_eq!(filtered.removed_small, small);
        assert!(filtered.pieces.iter().all(|p| p.poly.unsigned_area() >= limit));
    }

    /// Tiny tiles (small object / small pattern scale): the absolute minimum
    /// piece area must not wipe out cut edge pieces or whole tiles.
    #[test]
    fn tiny_tiles_are_not_filtered_away() {
        // A 2 × 1.5 mm shape with a 0.08 mm honeycomb: whole tiles ≈ 0.0055 mm²,
        // below the default 0.01 mm² minimum piece area.
        let shape = MultiPolygon::new(vec![
            Rings { exterior: vec![[0.0, 0.0], [2.0, 0.0], [2.0, 1.5], [0.0, 1.5]], holes: vec![] }.to_polygon(),
        ]);
        let p = Pattern {
            layout: Layout::Honeycomb,
            cell_size: 0.08,
            gap: 0.0,
            ..Pattern::default()
        };
        let res = expand_region(&shape, &p, &frame(), &ExpandOptions::default());
        assert!(res.pieces.len() > 300, "{} pieces", res.pieces.len());
        let covered = res.total_area() / 3.0;
        assert!(covered > 0.995, "pattern covers {:.1} % of the shape", covered * 100.0);
        assert!(res.min_area_used < 0.01);
    }

    #[test]
    fn concave_cut_produces_separate_pieces() {
        // A U shape: one big tile covering both arms is split into two pieces.
        let shape = MultiPolygon::new(vec![
            Rings {
                exterior: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 30.0], [20.0, 30.0], [20.0, 0.0], [30.0, 0.0], [30.0, 40.0], [0.0, 40.0]],
                holes: vec![],
            }
            .to_polygon(),
        ]);
        let p = Pattern {
            layout: Layout::Square,
            cell_size: 100.0,
            gap: 0.0,
            ..Pattern::default()
        };
        let f = PatternFrame {
            origin: [15.0, 20.0],
            rotation_deg: 0.0,
            scale: 1.0,
        };
        let res = expand_region(&shape, &p, &f, &ExpandOptions::default());
        assert_eq!(res.pieces.len(), 1); // one U-shaped piece
        let f2 = PatternFrame { origin: [15.0, -35.0], ..f };
        let res2 = expand_region(&shape, &p, &f2, &ExpandOptions::default());
        // Cell (0,0) covers y in [-85, 15]: it crosses both arms and is cut into two pieces.
        assert_eq!(res2.pieces.iter().filter(|p| p.cell == [0, 0]).count(), 2);
        // Cell (0,1) below it holds the rest of the U as one piece.
        assert_eq!(res2.pieces.iter().filter(|p| p.cell == [0, 1]).count(), 1);
        let total: f64 = res2.total_area();
        assert!((total - shape.unsigned_area()).abs() < 1e-6);
    }

    /// Timing on a dense A4 pattern. Run with:
    /// `cargo test --release -- --ignored --nocapture large_pattern_timing`
    #[test]
    #[ignore = "benchmark"]
    fn large_pattern_timing() {
        let shape = world_geometry(&ShapeGeom::Ellipse, &Transform::new(105.0, 148.5, 200.0, 287.0), 0.01);
        let p = Pattern {
            layout: Layout::Honeycomb,
            cell_size: 2.0,
            gap: 0.2,
            ..Pattern::default()
        };
        for fast in [false, true] {
            let t = std::time::Instant::now();
            let res = expand_region(
                &shape,
                &p,
                &frame(),
                &ExpandOptions {
                    fast_preview: fast,
                    ..ExpandOptions::default()
                },
            );
            println!(
                "fast={fast}: {} pieces ({} whole, {} clipped) from {} tiles in {:.1} ms",
                res.pieces.len(),
                res.tiles_whole,
                res.tiles_clipped,
                res.tiles_generated,
                t.elapsed().as_secs_f64() * 1000.0
            );
            assert!(res.pieces.len() > 10_000);
        }
    }

    /// A line of "text" (40 letters with holes, ≈16 000 vertices, 130 × 3.6 mm)
    /// with a very fine honeycomb — the case that made the app hang.
    fn text_line() -> MultiPolygon<f64> {
        text_line_n(40)
    }

    fn text_line_n(letters: usize) -> MultiPolygon<f64> {
        let mut polys = Vec::new();
        for k in 0..letters {
            let cx = 2.0 + k as f64 * 3.2;
            let ring = |r: f64| -> Vec<[f64; 2]> {
                (0..200)
                    .map(|i| {
                        let a = i as f64 / 200.0 * std::f64::consts::TAU;
                        // wobbly outline like a glyph
                        let rr = r * (1.0 + 0.08 * (5.0 * a).sin());
                        [cx + rr * a.cos(), 3.0 + rr * 1.2 * a.sin()]
                    })
                    .collect()
            };
            polys.push(Rings { exterior: ring(1.5), holes: vec![ring(1.0)] }.to_polygon());
        }
        MultiPolygon::new(polys)
    }

    /// The fast path (per-part tiles, local clipping) gives exactly the pieces of
    /// the straightforward method: every tile intersected with the whole shape.
    #[test]
    fn local_clipping_matches_brute_force() {
        use std::collections::HashMap;
        // (The brute-force reference is slow, so the text is kept short.)
        for (shape, cell) in [(star(), 9.0), (text_line_n(6), 0.4)] {
            let p = Pattern { layout: Layout::Honeycomb, cell_size: cell, gap: 0.0, ..Pattern::default() };
            let opts = ExpandOptions { min_area: 0.0, ..ExpandOptions::default() };
            let fast = expand_region(&shape, &p, &frame(), &opts);

            let sb = bounds_of(&shape).unwrap();
            let c = world_cell_size(&p, &frame());
            let all = generate_tiles(&p, &frame(), [sb[0] - c, sb[1] - c, sb[2] + c, sb[3] + c], 0.01, MAX_TILES);
            let mut want: HashMap<[i64; 2], (usize, f64)> = HashMap::new();
            for t in &all.tiles {
                for part in t.poly.intersection(&shape) {
                    let e = want.entry(t.cell).or_default();
                    e.0 += 1;
                    e.1 += part.unsigned_area();
                }
            }
            let mut got: HashMap<[i64; 2], (usize, f64)> = HashMap::new();
            for piece in &fast.pieces {
                let e = got.entry(piece.cell).or_default();
                e.0 += 1;
                e.1 += piece.poly.unsigned_area();
            }
            assert_eq!(got.len(), want.len(), "same tiles produce pieces");
            for (cell_id, (n, a)) in &want {
                let (gn, ga) = got.get(cell_id).copied().unwrap_or_default();
                assert_eq!(gn, *n, "piece count of cell {cell_id:?}");
                // i_overlay snaps to a grid scaled to its inputs, so the smaller
                // local input differs only in the last digits.
                assert!((ga - a).abs() < 1e-6 * a.max(1.0), "area of cell {cell_id:?}: {ga} vs {a}");
            }
        }
    }

    /// `cargo test --release -- --ignored --nocapture fine_pattern_on_text_timing`
    #[test]
    #[ignore = "benchmark"]
    fn fine_pattern_on_text_timing() {
        let shape = text_line();
        for cell in [0.3, 0.1, 0.05] {
            let p = Pattern { layout: Layout::Honeycomb, cell_size: cell, gap: 0.0, ..Pattern::default() };
            let t = std::time::Instant::now();
            let res = expand_region(&shape, &p, &frame(), &ExpandOptions::default());
            println!(
                "cell {cell} mm: {} pieces ({} whole, {} clipped) from {} tiles in {:.1} ms",
                res.pieces.len(),
                res.tiles_whole,
                res.tiles_clipped,
                res.tiles_generated,
                t.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    #[test]
    fn edge_index_contains_matches_reference() {
        let shape = star();
        let idx = EdgeIndex::new(&shape, bounds_of(&shape).unwrap(), 9.0);
        for i in 0..60 {
            for j in 0..60 {
                let p = [38.3 + i as f64 * 2.07, 37.9 + j as f64 * 2.11];
                assert_eq!(idx.contains(p), crate::shapes::point_in_evenodd(p, &shape), "{p:?}");
            }
        }
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let empty = MultiPolygon::<f64>::new(vec![]);
        assert!(expand_region(&empty, &honeycomb(), &frame(), &ExpandOptions::default()).pieces.is_empty());
        let flat = world_geometry(&ShapeGeom::Rect, &Transform::new(0.0, 0.0, 0.0, 0.0), 0.01);
        assert!(expand_region(&flat, &honeycomb(), &frame(), &ExpandOptions::default()).pieces.is_empty());
        let bad = Pattern {
            cell_size: 0.0,
            ..honeycomb()
        };
        let r = expand_region(
            &star(),
            &bad,
            &PatternFrame {
                origin: [f64::NAN, 0.0],
                rotation_deg: f64::INFINITY,
                scale: 0.0,
            },
            &ExpandOptions {
                max_tiles: 1000,
                ..ExpandOptions::default()
            },
        );
        assert!(r.capped);
    }
}
