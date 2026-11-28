//! Adobe Illustrator 3 (`.ai`) writer. No egui.
//!
//! AI 3 is the classic PostScript-based Illustrator format that Illustrator,
//! CorelDRAW, Inkscape and most sign / cutting software can open. The file has
//! the usual structure — DSC header, prolog, setup, script, trailer — and the
//! script uses the AI 3 path operators:
//!
//! | operator | meaning |
//! |---|---|
//! | `x y m` / `x y l` | move to / line to (points, origin bottom-left, y up) |
//! | `c m y k k` / `… K` | fill / stroke colour (CMYK, 0–1) |
//! | `w`, `j`, `J`, `M` | stroke width (pt), line join, line cap, miter limit |
//! | `f` `s` `b` | close and fill / stroke / fill + stroke |
//! | `*u` … `*U` | compound path (holes) |
//! | `u` … `U` | group |
//!
//! The prolog defines those operators as PostScript procedures, so the same file
//! is also valid EPS and prints or previews in any PostScript viewer.
//!
//! Units: 1 mm = 72 / 25.4 pt, so sizes are exact. AI 3 has no RGB and no
//! transparency: colours are converted to CMYK and drawn opaque.

use crate::document::{Document, ObjId, Object, ObjectKind, ShapeObj, pattern_pieces};
use crate::shapes::{Bounds, Color, LineCap, LineJoin, Style};
use crate::svg_export::{SvgOptions, export_objects, export_page, num};
use geo::{LineString, Polygon};
use std::fmt::Write as _;

/// Points per millimetre.
pub const PT_PER_MM: f64 = 72.0 / 25.4;

/// Defines the AI 3 operators as PostScript, so the file is printable EPS too.
/// Compound paths (`*u` … `*U`) collect their subpaths and are painted once,
/// with the even-odd rule, at `*U`.
const PROLOG: &str = r"%%BeginProlog
%%BeginResource: procset PatternExpander_AI3 1.0 0
/PE_AI3 40 dict def PE_AI3 begin
/_c false def /_paint {} def
/_fk [0 0 0 1] def /_sk [0 0 0 1] def
/k { 4 array astore /_fk exch def } bind def
/K { 4 array astore /_sk exch def } bind def
/_F { gsave _fk aload pop setcmykcolor eofill grestore } bind def
/_S { gsave _sk aload pop setcmykcolor stroke grestore } bind def
/_emit { /_paint exch def _c not { _paint newpath } if } bind def
/f { closepath {_F} _emit } bind def
/F { {_F} _emit } bind def
/s { closepath {_S} _emit } bind def
/S { {_S} _emit } bind def
/b { closepath {_F _S} _emit } bind def
/B { {_F _S} _emit } bind def
/n { _c not { newpath } if } bind def
/*u { /_c true def } bind def
/*U { /_c false def _paint newpath } bind def
/u {} def /U {} def
/m /moveto load def /l /lineto load def /c /curveto load def
/w /setlinewidth load def /j /setlinejoin load def /J /setlinecap load def /M /setmiterlimit load def
end
%%EndResource
%%EndProlog
";

/// Escape a PostScript string literal.
fn ps_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('(');
    for ch in s.chars() {
        match ch {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => out.push('_'),
        }
    }
    out.push(')');
    out
}

/// Naive RGB → CMYK (what Illustrator does without colour management).
pub fn rgb_to_cmyk(c: Color) -> [f64; 4] {
    let (r, g, b) = (f64::from(c.r) / 255.0, f64::from(c.g) / 255.0, f64::from(c.b) / 255.0);
    let k = 1.0 - r.max(g).max(b);
    if k >= 1.0 - 1e-9 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let d = 1.0 - k;
    [(1.0 - r - k) / d, (1.0 - g - k) / d, (1.0 - b - k) / d, k]
}

/// Options for the AI 3 writer.
#[derive(Clone, Copy, Debug, Default)]
pub struct AiOptions {
    /// Most basic AI 3, for laser / engraving / cutting software with simple
    /// importers (EZCAD and similar): no PostScript prolog, no groups, no
    /// compound paths; every outline is its own closed, stroked path (holes are
    /// simply further outlines, which hatching software fills even-odd anyway).
    pub laser_compatible: bool,
}

struct Writer<'a> {
    doc: &'a Document,
    out: String,
    /// Page (mm): x0, y0, x1, y1 — mapped to (0, 0)…(w, h) points.
    page: Bounds,
    paths: usize,
    simple: bool,
}

impl Writer<'_> {
    fn pt(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.page[0]) * PT_PER_MM, (self.page[3] - y) * PT_PER_MM)
    }

    /// One ring as `m` / `l` with the given orientation (true = counter-clockwise
    /// in page space). Returns false if degenerate.
    fn ring(&mut self, ring: &LineString<f64>, ccw: bool, op: &str) -> bool {
        let mut pts: Vec<(f64, f64)> = ring.0.iter().map(|c| self.pt(c.x, c.y)).collect();
        if pts.len() > 1 && pts.first() == pts.last() {
            pts.pop();
        }
        if pts.len() < 3 {
            return false;
        }
        let area2: f64 = (0..pts.len())
            .map(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum();
        if area2.abs() < 1e-12 {
            return false;
        }
        // Outer rings counter-clockwise, holes clockwise: holes are then right
        // under both the even-odd and the non-zero rule.
        if (area2 > 0.0) != ccw {
            pts.reverse();
        }
        for (i, (x, y)) in pts.iter().enumerate() {
            let _ = writeln!(self.out, "{} {} {}", num(*x), num(*y), if i == 0 { "m" } else { "l" });
        }
        // Closed paths repeat their start point as the last `l`, as Illustrator
        // writes them. Importers take that final segment as "back to the start"
        // and drop it — without it they would drop a real vertex (a hexagon
        // would come in with 5 corners). In PostScript it is a zero-length
        // segment before closepath, so rendering is unchanged.
        let (x0, y0) = pts[0];
        let _ = writeln!(self.out, "{} {} l", num(x0), num(y0));
        let _ = writeln!(self.out, "{op}");
        true
    }

    fn paint_state(&mut self, fill: Option<Color>, style: Option<&Style>) -> Option<&'static str> {
        let stroke = style.and_then(|s| s.stroke.filter(|_| s.stroke_width > 0.0).map(|c| (c, s)));
        if let Some(c) = fill {
            let [cc, m, y, k] = rgb_to_cmyk(c);
            let _ = writeln!(self.out, "{} {} {} {} k", num(cc), num(m), num(y), num(k));
        }
        if let Some((c, s)) = stroke {
            let [cc, m, y, k] = rgb_to_cmyk(c);
            let _ = writeln!(self.out, "{} {} {} {} K", num(cc), num(m), num(y), num(k));
            let join = match s.line_join {
                LineJoin::Miter => 0,
                LineJoin::Round => 1,
                LineJoin::Bevel => 2,
            };
            let cap = match s.line_cap {
                LineCap::Butt => 0,
                LineCap::Round => 1,
                LineCap::Square => 2,
            };
            let _ = writeln!(
                self.out,
                "{} w {join} j {cap} J {} M",
                num(s.stroke_width * PT_PER_MM),
                num(s.miter_limit.max(1.0))
            );
        }
        match (fill.is_some(), stroke.is_some()) {
            (true, true) => Some("b"),
            (true, false) => Some("f"),
            (false, true) => Some("s"),
            (false, false) => None,
        }
    }

    /// A filled / stroked area. Several rings (holes, separate parts) become one
    /// compound path, so the object stays one object.
    fn path(&mut self, polys: &[Polygon<f64>], fill: Option<Color>, style: Option<&Style>) {
        let rings: Vec<(&LineString<f64>, bool)> = polys
            .iter()
            .flat_map(|p| std::iter::once((p.exterior(), true)).chain(p.interiors().iter().map(|h| (h, false))))
            .filter(|(r, _)| r.0.len() >= 3)
            .collect();
        if rings.is_empty() {
            return;
        }
        if self.simple {
            // Laser-compatible: each outline is a separate closed, stroked path in
            // the object's colour (fill colour, else stroke colour).
            let Some(c) = fill.or_else(|| style.and_then(|s| s.stroke)) else { return };
            let [cc, m, y, k] = rgb_to_cmyk(c);
            let _ = writeln!(self.out, "{} {} {} {} K", num(cc), num(m), num(y), num(k));
            self.out.push_str("0.25 w\n");
            for (r, outer) in rings {
                if self.ring(r, outer, "s") {
                    self.paths += 1;
                }
            }
            return;
        }
        let Some(op) = self.paint_state(fill, style) else { return };
        let compound = rings.len() > 1;
        if compound {
            self.out.push_str("*u\n");
        }
        let mut any = false;
        for (r, outer) in rings {
            any |= self.ring(r, outer, op);
        }
        if compound {
            self.out.push_str("*U\n");
        }
        if any {
            self.paths += 1;
        }
    }

    fn shape(&mut self, s: &ShapeObj) {
        let tol = self.doc.settings.tolerance();
        let geom = s.world_geometry(tol);
        match &s.pattern {
            None => self.path(&geom.0, s.style.fill, Some(&s.style)),
            Some(fill) => {
                // A live pattern fill is exported exactly as previewed: expanded pieces.
                self.group_start();
                if fill.keep_background {
                    self.path(&geom.0, s.style.fill, Some(&s.style));
                }
                for p in pattern_pieces(s, &self.doc.settings, false).pieces {
                    self.path(std::slice::from_ref(&p.poly), Some(p.color), None);
                }
                self.group_end();
            }
        }
    }

    fn object(&mut self, o: &Object) {
        match &o.kind {
            ObjectKind::Shape(s) => self.shape(s),
            ObjectKind::Group(children) => {
                self.group_start();
                for c in children.iter() {
                    self.object(c);
                }
                self.group_end();
            }
        }
    }

    fn group_start(&mut self) {
        if !self.simple {
            self.out.push_str("u\n");
        }
    }

    fn group_end(&mut self) {
        if !self.simple {
            self.out.push_str("U\n");
        }
    }
}

/// Build the AI 3 file text. Returns the text and the number of paths written.
/// `title` goes into the header (usually the file name). Lines end in CR LF,
/// as Windows programs (and their AI importers) expect.
pub fn export_ai(doc: &Document, selection: &[ObjId], opts: &SvgOptions, title: &str) -> (String, usize) {
    export_ai_with(doc, selection, opts, title, &AiOptions::default())
}

/// [`export_ai`] with AI-specific options.
pub fn export_ai_with(
    doc: &Document,
    selection: &[ObjId],
    opts: &SvgOptions,
    title: &str,
    ai: &AiOptions,
) -> (String, usize) {
    let objs = export_objects(doc, selection, opts);
    let page = export_page(doc, &objs, opts.bounds);
    let (w, h) = ((page[2] - page[0]) * PT_PER_MM, (page[3] - page[1]) * PT_PER_MM);
    let simple = ai.laser_compatible;
    let mut wr = Writer { doc, out: String::new(), page, paths: 0, simple };

    // Header. The Creator line is how AI 3 files identify their format to
    // importers (Illustrator, CorelDRAW, cutting software).
    let _ = write!(
        wr.out,
        "%!PS-Adobe-2.0 EPSF-1.2\n\
         %%Creator:Adobe Illustrator(TM) 3.2\n\
         %%For:{for_} ()\n\
         %%Title:{title}\n\
         %%BoundingBox:0 0 {wc} {hc}\n\
         %%HiResBoundingBox:0 0 {w} {h}\n\
         %%DocumentProcessColors:Cyan Magenta Yellow Black\n\
         %%DocumentFonts:\n\
         {resources}\
         %%TemplateBox:0 0 {wc} {hc}\n\
         %%TileBox:0 0 {wc} {hc}\n\
         %%EndComments\n",
        for_ = ps_string("Pattern Expander"),
        title = ps_string(title),
        wc = w.ceil() as i64,
        hc = h.ceil() as i64,
        w = num(w),
        h = num(h),
        resources = if simple { "" } else { "%%DocumentSuppliedResources: procset PatternExpander_AI3 1.0 0\n" },
    );
    if simple {
        wr.out.push_str("%%EndProlog\n%%BeginSetup\n%%EndSetup\n");
    } else {
        wr.out.push_str("% Written by Pattern Expander. Units: points; 1 mm = 72/25.4 pt.\n");
        wr.out.push_str(PROLOG);
        wr.out.push_str("%%BeginSetup\nPE_AI3 begin\n%%EndSetup\n");
    }
    for o in objs {
        wr.object(o);
    }
    wr.out.push_str(if simple { "%%PageTrailer\n%%Trailer\n%%EOF\n" } else { "%%PageTrailer\n%%Trailer\nend\n%%EOF\n" });
    let n = wr.paths;
    // Windows line endings throughout.
    (wr.out.replace('\n', "\r\n"), n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, ShapeObj};
    use crate::pattern::{Layout, Pattern, PatternFill};
    use crate::shapes::{ShapeGeom, Transform};
    use crate::svg_export::SvgBounds;

    /// Export with LF line endings (the tests compare text line by line).
    fn export_lf(doc: &Document, opts: &SvgOptions) -> (String, usize) {
        let (ai, n) = export_ai(doc, &[], opts, "test.ai");
        (ai.replace("\r\n", "\n"), n)
    }

    /// Every line ends in CR LF (Windows importers such as EZCAD expect it).
    #[test]
    fn lines_end_in_crlf() {
        let mut doc = Document::default();
        doc.add_shape("r", ShapeObj::new(ShapeGeom::Rect, Transform::new(10.0, 10.0, 10.0, 10.0), Style::default()));
        for ai in [AiOptions::default(), AiOptions { laser_compatible: true }] {
            let (text, _) = export_ai_with(&doc, &[], &SvgOptions::default(), "x", &ai);
            let bare_lf = text.bytes().enumerate().filter(|&(i, b)| b == b'\n' && (i == 0 || text.as_bytes()[i - 1] != b'\r'));
            assert_eq!(bare_lf.count(), 0);
            assert!(text.ends_with("%%EOF\r\n"));
        }
    }

    /// Laser-compatible AI: no prolog, groups or compound paths; every outline a
    /// separate closed, stroked path (holes included), corners intact.
    #[test]
    fn laser_compatible_is_plain_outlines() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(ShapeGeom::Rect, Transform::new(50.0, 50.0, 60.0, 60.0), Style::default());
        s.pattern = Some(PatternFill::new(
            Pattern { layout: Layout::Honeycomb, cell_size: 10.0, gap: 1.0, ..Pattern::default() },
            &s.xf,
        ));
        let id = doc.add_shape("hex", s);
        doc.apply_patterns(&[id]);
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let frame = ShapeGeom::Custom {
            polys: std::sync::Arc::new(vec![crate::shapes::Rings {
                exterior: ring(-10.0, -10.0, 10.0, 10.0),
                holes: vec![ring(-5.0, -5.0, 5.0, 5.0)],
            }]),
            base_w: 20.0,
            base_h: 20.0,
        };
        doc.add_shape("frame", ShapeObj::new(frame, Transform::new(150.0, 50.0, 20.0, 20.0), Style::default()));

        let (ai, n) = export_ai_with(&doc, &[], &SvgOptions::default(), "x", &AiOptions { laser_compatible: true });
        let ai = ai.replace("\r\n", "\n");
        for banned in ["\nu\n", "\nU\n", "*u", "*U", "PE_AI3", "%%BeginResource", "\nf\n", "\nb\n"] {
            assert!(!ai.contains(banned), "laser mode must not contain {banned:?}");
        }
        assert!(ai.contains("%%EndProlog") && ai.contains("%%EndSetup"));
        assert_eq!(ai.matches("\ns\n").count(), n, "one stroked, closed path per outline");
        // Every path closed back to its start; whole hexagons keep 6 corners.
        let mut paths: Vec<Vec<String>> = Vec::new();
        for l in ai.lines() {
            let t: Vec<&str> = l.split_whitespace().collect();
            match t.as_slice() {
                [x, y, "m"] => paths.push(vec![format!("{x} {y}")]),
                [x, y, "l"] => paths.last_mut().unwrap().push(format!("{x} {y}")),
                _ => {}
            }
        }
        assert_eq!(paths.len(), n);
        assert!(paths.iter().all(|p| p.first() == p.last()));
        assert!(paths.iter().filter(|p| p.len() == 7).count() > 10, "hexagons: 6 corners + closing point");
        // The frame's hole is just another outline (outer + hole = 2 paths of 5 points).
        assert!(paths.iter().filter(|p| p.len() == 5).count() >= 2);
    }

    fn nums_before(text: &str, op: &str) -> Vec<Vec<f64>> {
        text.lines()
            .filter(|l| l.ends_with(&format!(" {op}")))
            .map(|l| l.split_whitespace().filter_map(|t| t.parse().ok()).collect())
            .collect()
    }

    #[test]
    fn rect_50mm_is_exact_in_points_with_y_up() {
        let mut doc = Document::default();
        let style = Style { fill: Some(Color::rgb(255, 0, 0)), stroke: None, ..Style::default() };
        doc.add_shape("r", ShapeObj::new(ShapeGeom::Rect, Transform::new(60.0, 70.0, 50.0, 20.0), style));
        let (ai, n) = export_lf(&doc, &SvgOptions::default());
        assert_eq!(n, 1);
        assert!(ai.starts_with("%!PS-Adobe-2.0 EPSF-1.2\n%%Creator:Adobe Illustrator(TM) 3.2"));
        assert!(ai.contains("%%HiResBoundingBox:0 0 141.7323 56.6929"), "50 × 20 mm in pt");
        assert!(ai.trim_end().ends_with("%%EOF"));
        // Red → CMYK 0 1 1 0, filled and closed.
        assert!(ai.contains("0 1 1 0 k\n"));
        assert!(ai.contains("\nf\n"));
        // All corners on the page, y up (origin bottom-left).
        let pts: Vec<Vec<f64>> = nums_before(&ai, "m").into_iter().chain(nums_before(&ai, "l")).collect();
        assert_eq!(pts.len(), 5, "4 corners + the start point repeated to close");
        assert_eq!(pts[0], pts[4]);
        for p in &pts {
            assert!(p[0].abs() < 1e-3 || (p[0] - 141.7323).abs() < 1e-3);
            assert!(p[1].abs() < 1e-3 || (p[1] - 56.6929).abs() < 1e-3);
        }
    }

    #[test]
    fn holes_are_compound_with_opposite_winding_and_strokes_keep_style() {
        let mut doc = Document::default();
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let geom = ShapeGeom::Custom {
            polys: std::sync::Arc::new(vec![crate::shapes::Rings {
                exterior: ring(-10.0, -10.0, 10.0, 10.0),
                holes: vec![ring(-5.0, -5.0, 5.0, 5.0)],
            }]),
            base_w: 20.0,
            base_h: 20.0,
        };
        let style = Style { stroke_width: 1.0, line_join: LineJoin::Round, ..Style::default() };
        doc.add_shape("frame", ShapeObj::new(geom, Transform::new(10.0, 10.0, 20.0, 20.0), style));
        let opts = SvgOptions { only_selected: false, bounds: SvgBounds::Artboard };
        let (ai, _) = export_lf(&doc, &opts);
        assert!(ai.contains("*u\n") && ai.contains("*U\n"));
        assert!(ai.contains("2.8346 w 1 j 0 J 4 M"), "1 mm stroke, round join");
        assert_eq!(ai.matches("\nb\n").count(), 2, "fill + stroke, both rings");
        // Opposite winding: signed areas of the two rings have opposite signs.
        let body = &ai[ai.find("*u\n").unwrap()..ai.find("*U\n").unwrap()];
        let mut areas = Vec::new();
        let mut cur: Vec<(f64, f64)> = Vec::new();
        for l in body.lines() {
            let t: Vec<&str> = l.split_whitespace().collect();
            match t.last() {
                Some(&"m") | Some(&"l") => cur.push((t[0].parse().unwrap(), t[1].parse().unwrap())),
                Some(&"b") => {
                    let a: f64 = (0..cur.len()).map(|i| cur[i].0 * cur[(i + 1) % cur.len()].1 - cur[(i + 1) % cur.len()].0 * cur[i].1).sum();
                    areas.push(a);
                    cur.clear();
                }
                _ => {}
            }
        }
        assert_eq!(areas.len(), 2);
        assert!(areas[0] > 0.0 && areas[1] < 0.0, "outer CCW, hole CW: {areas:?}");
    }

    /// Regression: an expanded honeycomb's hexagons must keep all 6 corners.
    /// Each closed path is `m`, the other corners as `l`, then the start point
    /// again as the final `l` (importers drop that last segment as the closer).
    #[test]
    fn expanded_hexagons_keep_six_corners() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(ShapeGeom::Rect, Transform::new(50.0, 50.0, 60.0, 60.0), Style::default());
        s.pattern = Some(PatternFill::new(
            Pattern { layout: Layout::Honeycomb, cell_size: 10.0, gap: 1.0, ..Pattern::default() },
            &s.xf,
        ));
        let id = doc.add_shape("hex", s);
        doc.apply_patterns(&[id]);
        let (ai, _) = export_lf(&doc, &SvgOptions::default());

        let mut paths: Vec<Vec<(String, String)>> = Vec::new();
        for l in ai.lines() {
            let t: Vec<&str> = l.split_whitespace().collect();
            match t.as_slice() {
                [x, y, "m"] => paths.push(vec![(x.to_string(), y.to_string())]),
                [x, y, "l"] => paths.last_mut().unwrap().push((x.to_string(), y.to_string())),
                _ => {}
            }
        }
        assert!(paths.len() > 20);
        let mut whole = 0;
        for p in &paths {
            assert_eq!(p.first(), p.last(), "closed paths repeat their start point");
            let mut corners = p[..p.len() - 1].to_vec();
            corners.dedup();
            if corners.len() == 6 {
                whole += 1;
            }
            assert!(corners.len() >= 3);
        }
        assert!(whole > 10, "interior hexagons have 6 distinct corners ({whole} found)");
    }

    #[test]
    fn live_pattern_is_exported_as_grouped_pieces() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(ShapeGeom::Ellipse, Transform::new(50.0, 50.0, 40.0, 40.0), Style::default());
        s.pattern = Some(PatternFill::new(
            Pattern { layout: Layout::Square, cell_size: 8.0, ..Pattern::default() },
            &s.xf,
        ));
        doc.add_shape("p", s.clone());
        let pieces = pattern_pieces(&s, &doc.settings, false).pieces.len();
        let (ai, n) = export_lf(&doc, &SvgOptions::default());
        assert_eq!(n, pieces);
        assert!(ai.contains("\nu\n") && ai.contains("\nU\n"));
    }

    /// Writes a sample file for manual checking in Illustrator / CorelDRAW:
    /// `set AI_SAMPLE=C:\path\sample.ai` then
    /// `cargo test --lib write_sample_ai -- --ignored`.
    #[test]
    #[ignore = "writes a file"]
    fn write_sample_ai() {
        let Ok(path) = std::env::var("AI_SAMPLE") else { return };
        let mut doc = Document::default();
        let mut star = ShapeObj::new(
            ShapeGeom::Star { points: 5, inner_ratio: 0.45 },
            Transform::new(60.0, 60.0, 80.0, 80.0),
            Style::default(),
        );
        star.pattern = Some(PatternFill::new(
            Pattern { layout: Layout::Honeycomb, cell_size: 6.0, gap: 0.6, ..Pattern::default() },
            &star.xf,
        ));
        doc.add_shape("star", star);
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let frame = ShapeGeom::Custom {
            polys: std::sync::Arc::new(vec![crate::shapes::Rings {
                exterior: ring(-25.0, -15.0, 25.0, 15.0),
                holes: vec![ring(-15.0, -7.0, 15.0, 7.0)],
            }]),
            base_w: 50.0,
            base_h: 30.0,
        };
        let style = Style {
            fill: Some(Color::rgb(30, 160, 60)),
            stroke: Some(Color::rgb(20, 40, 200)),
            stroke_width: 1.0,
            ..Style::default()
        };
        doc.add_shape("frame", ShapeObj::new(frame, Transform::new(150.0, 40.0, 50.0, 30.0), style));
        let red = Style { fill: Some(Color::rgb(220, 30, 30)), stroke: None, ..Style::default() };
        doc.add_shape("rect", ShapeObj::new(ShapeGeom::Rect, Transform::new(150.0, 90.0, 40.0, 20.0), red));
        let (ai, n) = export_ai(&doc, &[], &SvgOptions::default(), "sample.ai");
        std::fs::write(&path, ai).unwrap();
        println!("wrote {n} paths to {path}");
        // The same drawing as laser-compatible AI, next to it.
        let laser = path.replace(".ai", " (laser).ai");
        let (ai, n) = export_ai_with(&doc, &[], &SvgOptions::default(), "sample (laser).ai", &AiOptions { laser_compatible: true });
        std::fs::write(&laser, ai).unwrap();
        println!("wrote {n} paths to {laser}");
    }

    #[test]
    fn cmyk_and_titles() {
        assert_eq!(rgb_to_cmyk(Color::rgb(0, 0, 0)), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(rgb_to_cmyk(Color::rgb(255, 255, 255)), [0.0, 0.0, 0.0, 0.0]);
        let [c, m, y, k] = rgb_to_cmyk(Color::rgb(0, 128, 255));
        assert!((c - 1.0).abs() < 1e-9 && (m - (1.0 - 128.0 / 255.0)).abs() < 1e-9 && y.abs() < 1e-9 && k.abs() < 1e-9);
        assert_eq!(ps_string("a(b)\\c"), r"(a\(b\)\\c)");
    }
}
