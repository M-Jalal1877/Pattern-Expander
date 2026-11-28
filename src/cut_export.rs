//! DXF (AutoCAD R12) and PLT (HPGL) writers for laser, engraving and cutting
//! software (EZCAD, LightBurn, plotters, CAD). No egui.
//!
//! Both formats carry outlines only: every ring of every exported shape —
//! holes included — becomes one closed polyline in real millimetres. Live
//! pattern fills are exported as their pieces, exactly as previewed. Marking
//! software fills nested outlines even-odd, so holes stay holes.
//!
//! * **DXF R12**: `POLYLINE` / `VERTEX` / `SEQEND` entities (closed flag set),
//!   units mm (`$INSUNITS` = 4), y pointing up, colour as the nearest AutoCAD
//!   colour index and a layer per colour (`RGB_rrggbb`).
//! * **PLT / HPGL**: 40 plotter units per mm, origin bottom-left. Every distinct
//!   colour gets its own pen (`SP1`, `SP2`, …) in order of appearance, so marking
//!   software can assign laser settings per colour.

use crate::document::{Document, ObjId, Object, ObjectKind, pattern_pieces};
use crate::shapes::{Bounds, Color, ring_points};
use crate::svg_export::{SvgOptions, export_objects, export_page, num};
use geo::Polygon;
use std::fmt::Write as _;

/// One closed outline in page millimetres (y pointing down, like the document).
#[derive(Clone, Debug)]
pub struct Outline {
    pub color: Color,
    pub points: Vec<[f64; 2]>,
}

/// All outlines that would be exported, and the page (mm) they sit on.
pub fn collect_outlines(doc: &Document, selection: &[ObjId], opts: &SvgOptions) -> (Bounds, Vec<Outline>) {
    let objs = export_objects(doc, selection, opts);
    let page = export_page(doc, &objs, opts.bounds);
    let mut out = Vec::new();
    for o in objs {
        walk(o, doc, &mut out);
    }
    (page, out)
}

fn push_rings(polys: &[Polygon<f64>], color: Color, out: &mut Vec<Outline>) {
    for p in polys {
        for ring in std::iter::once(p.exterior()).chain(p.interiors()) {
            let pts = ring_points(ring);
            if pts.len() >= 3 {
                out.push(Outline { color, points: pts });
            }
        }
    }
}

fn walk(o: &Object, doc: &Document, out: &mut Vec<Outline>) {
    let tol = doc.settings.tolerance();
    match &o.kind {
        ObjectKind::Group(children) => {
            for c in children.iter() {
                walk(c, doc, out);
            }
        }
        ObjectKind::Shape(s) => {
            let base = s.style.fill.or(s.style.stroke).unwrap_or(Color::BLACK);
            match &s.pattern {
                None => push_rings(&s.world_geometry(tol).0, base, out),
                Some(fill) => {
                    if fill.keep_background {
                        push_rings(&s.world_geometry(tol).0, base, out);
                    }
                    for piece in pattern_pieces(s, &doc.settings, false).pieces {
                        push_rings(std::slice::from_ref(&piece.poly), piece.color, out);
                    }
                }
            }
        }
    }
}

/// Page mm (y down) → output mm relative to the page's bottom-left, y up.
fn to_page(page: &Bounds, p: [f64; 2]) -> [f64; 2] {
    [p[0] - page[0], page[3] - p[1]]
}

// ---- DXF ------------------------------------------------------------------------

/// Nearest of the basic AutoCAD colour indices (1–9).
fn aci(c: Color) -> u8 {
    const TABLE: [(u8, [u8; 3]); 9] = [
        (1, [255, 0, 0]),
        (2, [255, 255, 0]),
        (3, [0, 255, 0]),
        (4, [0, 255, 255]),
        (5, [0, 0, 255]),
        (6, [255, 0, 255]),
        (7, [0, 0, 0]), // "white/black": shown black on a white background
        (8, [128, 128, 128]),
        (9, [192, 192, 192]),
    ];
    let d = |t: [u8; 3]| -> i32 {
        let (r, g, b) = (i32::from(c.r) - i32::from(t[0]), i32::from(c.g) - i32::from(t[1]), i32::from(c.b) - i32::from(t[2]));
        r * r + g * g + b * b
    };
    TABLE.iter().min_by_key(|(_, rgb)| d(*rgb)).map_or(7, |(i, _)| *i)
}

fn layer_name(c: Color) -> String {
    format!("RGB_{:02X}{:02X}{:02X}", c.r, c.g, c.b)
}

/// Build a DXF R12 file. Returns the text and the number of outlines.
pub fn export_dxf(doc: &Document, selection: &[ObjId], opts: &SvgOptions) -> (String, usize) {
    let (page, outlines) = collect_outlines(doc, selection, opts);
    let (w, h) = (page[2] - page[0], page[3] - page[1]);
    let mut s = String::new();
    let mut pair = |code: i32, value: &str| {
        let _ = write!(s, "{code}\r\n{value}\r\n");
    };
    // Header: R12, millimetres, extents.
    pair(0, "SECTION");
    pair(2, "HEADER");
    pair(9, "$ACADVER");
    pair(1, "AC1009");
    pair(9, "$INSUNITS");
    pair(70, "4");
    pair(9, "$MEASUREMENT");
    pair(70, "1");
    pair(9, "$EXTMIN");
    pair(10, "0.0");
    pair(20, "0.0");
    pair(9, "$EXTMAX");
    pair(10, &num(w));
    pair(20, &num(h));
    pair(0, "ENDSEC");
    // Layers: one per colour.
    let mut colors: Vec<Color> = Vec::new();
    for o in &outlines {
        if !colors.contains(&o.color) {
            colors.push(o.color);
        }
    }
    pair(0, "SECTION");
    pair(2, "TABLES");
    pair(0, "TABLE");
    pair(2, "LAYER");
    pair(70, &(colors.len() + 1).to_string());
    for (name, ci) in std::iter::once(("0".to_string(), 7)).chain(colors.iter().map(|c| (layer_name(*c), aci(*c)))) {
        pair(0, "LAYER");
        pair(2, &name);
        pair(70, "0");
        pair(62, &ci.to_string());
        pair(6, "CONTINUOUS");
    }
    pair(0, "ENDTAB");
    pair(0, "ENDSEC");
    // Entities: one closed POLYLINE per outline.
    pair(0, "SECTION");
    pair(2, "ENTITIES");
    for o in &outlines {
        let layer = layer_name(o.color);
        let ci = aci(o.color).to_string();
        pair(0, "POLYLINE");
        pair(8, &layer);
        pair(62, &ci);
        pair(66, "1");
        pair(10, "0.0");
        pair(20, "0.0");
        pair(30, "0.0");
        pair(70, "1"); // closed
        for p in &o.points {
            let q = to_page(&page, *p);
            pair(0, "VERTEX");
            pair(8, &layer);
            pair(10, &num(q[0]));
            pair(20, &num(q[1]));
            pair(30, "0.0");
        }
        pair(0, "SEQEND");
        pair(8, &layer);
    }
    pair(0, "ENDSEC");
    pair(0, "EOF");
    (s, outlines.len())
}

// ---- PLT / HPGL -------------------------------------------------------------------

/// HPGL plotter units per millimetre.
pub const PLU_PER_MM: f64 = 40.0;

/// Build a PLT (HPGL) file. Returns the text and the number of outlines.
pub fn export_plt(doc: &Document, selection: &[ObjId], opts: &SvgOptions) -> (String, usize) {
    let (page, outlines) = collect_outlines(doc, selection, opts);
    let mut pens: Vec<Color> = Vec::new();
    let mut s = String::from("IN;\r\n");
    let mut current_pen = 0;
    let plu = |p: [f64; 2]| -> (i64, i64) {
        let q = to_page(&page, p);
        ((q[0] * PLU_PER_MM).round() as i64, (q[1] * PLU_PER_MM).round() as i64)
    };
    for o in &outlines {
        let pen = match pens.iter().position(|c| *c == o.color) {
            Some(i) => i + 1,
            None => {
                pens.push(o.color);
                pens.len()
            }
        };
        if pen != current_pen {
            let _ = write!(s, "SP{pen};\r\n");
            current_pen = pen;
        }
        let (x0, y0) = plu(o.points[0]);
        let _ = write!(s, "PU{x0},{y0};PD");
        for (i, p) in o.points[1..].iter().chain(std::iter::once(&o.points[0])).enumerate() {
            let (x, y) = plu(*p);
            let _ = write!(s, "{}{x},{y}", if i == 0 { "" } else { "," });
        }
        s.push_str(";\r\n");
    }
    s.push_str("PU;SP0;\r\n");
    (s, outlines.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ShapeObj;
    use crate::pattern::{Layout, Pattern, PatternFill};
    use crate::shapes::{Rings, ShapeGeom, Style, Transform};

    fn frame_doc() -> Document {
        let mut doc = Document::default();
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let geom = ShapeGeom::Custom {
            polys: std::sync::Arc::new(vec![Rings { exterior: ring(-10.0, -5.0, 10.0, 5.0), holes: vec![ring(-2.0, -2.0, 2.0, 2.0)] }]),
            base_w: 20.0,
            base_h: 10.0,
        };
        let style = Style::fill_only(Color::rgb(255, 0, 0));
        doc.add_shape("frame", ShapeObj::new(geom, Transform::new(50.0, 50.0, 20.0, 10.0), style));
        doc
    }

    #[test]
    fn dxf_closed_polylines_in_mm_with_y_up() {
        let doc = frame_doc();
        let (dxf, n) = export_dxf(&doc, &[], &SvgOptions::default());
        assert_eq!(n, 2, "outer ring + hole");
        assert!(dxf.starts_with("0\r\nSECTION\r\n2\r\nHEADER\r\n9\r\n$ACADVER\r\n1\r\nAC1009\r\n"));
        assert!(dxf.trim_end().ends_with("0\r\nEOF"));
        assert_eq!(dxf.matches("\r\nPOLYLINE\r\n").count(), 2);
        assert_eq!(dxf.matches("\r\nSEQEND\r\n").count(), 2);
        assert!(dxf.contains("8\r\nRGB_FF0000\r\n62\r\n1\r\n"), "red layer, colour 1");
        // Page = object bounds (20 × 10 mm): every x in [0, 20], y in [0, 10].
        let lines: Vec<&str> = dxf.split("\r\n").collect();
        // DXF is strictly (group code, value) line pairs.
        let vals = |code: &str| -> Vec<f64> {
            lines.as_chunks::<2>().0.iter().filter(|w| w[0] == code).filter_map(|w| w[1].parse().ok()).collect()
        };
        let xs = vals("10");
        let ys = vals("20");
        assert!(xs.iter().all(|x| (-1e-9..=20.0 + 1e-9).contains(x)), "{xs:?}");
        assert!(ys.iter().all(|y| (-1e-9..=10.0 + 1e-9).contains(y)));
        assert!(xs.contains(&8.0) && xs.contains(&12.0), "hole at 8–12 mm");
    }

    #[test]
    fn plt_pens_per_colour_and_40_units_per_mm() {
        let mut doc = frame_doc();
        let blue = Style::fill_only(Color::rgb(0, 0, 255));
        doc.add_shape("b", ShapeObj::new(ShapeGeom::Rect, Transform::new(80.0, 50.0, 10.0, 10.0), blue));
        let (plt, n) = export_plt(&doc, &[], &SvgOptions::default());
        assert_eq!(n, 3);
        assert!(plt.starts_with("IN;\r\nSP1;\r\n"));
        assert!(plt.contains("SP2;"), "second colour → second pen");
        // Closed: each PD run ends where its PU started.
        for line in plt.lines().filter(|l| l.starts_with("PU") && l.contains("PD")) {
            let start = &line[2..line.find(';').unwrap()];
            let pd = &line[line.find("PD").unwrap() + 2..line.len() - 1];
            assert!(pd.ends_with(start), "{line}");
        }
        // Page width = 0..45 mm (frame from 40 to 60, blue rect to 85) = 1800 plotter units.
        assert!(plt.contains("1800,"), "{plt}");
    }

    #[test]
    fn live_patterns_are_exported_as_pieces() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(ShapeGeom::Ellipse, Transform::new(50.0, 50.0, 40.0, 40.0), Style::default());
        s.pattern = Some(PatternFill::new(Pattern { layout: Layout::Honeycomb, cell_size: 8.0, ..Pattern::default() }, &s.xf));
        let pieces = pattern_pieces(&s, &doc.settings, false).pieces.len();
        doc.add_shape("p", s);
        let (_, n) = export_dxf(&doc, &[], &SvgOptions::default());
        assert!(n >= pieces && pieces > 10);
    }

    /// `set CUT_SAMPLE_DIR=…` then `cargo test --lib write_cut_samples -- --ignored`
    #[test]
    #[ignore = "writes files"]
    fn write_cut_samples() {
        let Ok(dir) = std::env::var("CUT_SAMPLE_DIR") else { return };
        let mut doc = frame_doc();
        let mut star = ShapeObj::new(
            ShapeGeom::Star { points: 5, inner_ratio: 0.45 },
            Transform::new(20.0, 50.0, 30.0, 30.0),
            Style::default(),
        );
        star.pattern = Some(PatternFill::new(
            Pattern { layout: Layout::Honeycomb, cell_size: 3.0, gap: 0.3, ..Pattern::default() },
            &star.xf,
        ));
        doc.add_shape("star", star);
        let (dxf, n) = export_dxf(&doc, &[], &SvgOptions::default());
        std::fs::write(format!("{dir}/sample.dxf"), dxf).unwrap();
        let (plt, _) = export_plt(&doc, &[], &SvgOptions::default());
        std::fs::write(format!("{dir}/sample.plt"), plt).unwrap();
        println!("wrote {n} outlines to {dir}/sample.dxf and sample.plt");
    }
}
