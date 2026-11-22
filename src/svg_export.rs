//! SVG 1.1 writer. No egui.
//!
//! The SVG is written in real-world size: `width="…mm" height="…mm"` with a
//! viewBox in millimetres, so one user unit is one millimetre and the file opens
//! at its physical size in Illustrator, Inkscape and browsers.

use crate::document::{Document, ObjId, Object, ObjectKind, ShapeObj, pattern_pieces};
use crate::shapes::{Bounds, Color, Style, bounds_of, bounds_union};
use geo::{LineString, Polygon};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvgBounds {
    /// viewBox fits the exported objects (including stroke width).
    Objects,
    /// viewBox is the artboard.
    Artboard,
}

#[derive(Clone, Copy, Debug)]
pub struct SvgOptions {
    pub only_selected: bool,
    pub bounds: SvgBounds,
}

impl Default for SvgOptions {
    fn default() -> Self {
        Self {
            only_selected: false,
            bounds: SvgBounds::Objects,
        }
    }
}

/// Format a coordinate with at most 4 decimals and no trailing zeros.
pub fn num(v: f64) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let mut s = format!("{:.4}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" { "0".into() } else { s }
}

fn ring_path(out: &mut String, ring: &LineString<f64>) {
    let pts = &ring.0;
    let n = if pts.len() > 1 && pts.first() == pts.last() { pts.len() - 1 } else { pts.len() };
    if n < 3 {
        return;
    }
    for (i, c) in pts[..n].iter().enumerate() {
        out.push(if i == 0 { 'M' } else { 'L' });
        out.push_str(&num(c.x));
        out.push(' ');
        out.push_str(&num(c.y));
    }
    out.push('Z');
}

fn path_data(polys: &[Polygon<f64>]) -> (String, usize) {
    let mut d = String::new();
    let mut rings = 0;
    for p in polys {
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            let before = d.len();
            ring_path(&mut d, r);
            if d.len() > before {
                rings += 1;
            }
        }
    }
    (d, rings)
}

struct Writer<'a> {
    doc: &'a Document,
    out: String,
    paths: usize,
    groups: usize,
    depth: usize,
}

impl Writer<'_> {
    fn indent(&mut self) {
        for _ in 0..=self.depth {
            self.out.push_str("  ");
        }
    }

    fn paint_attrs(&mut self, fill: Option<Color>, style: Option<&Style>) {
        match fill {
            Some(c) => {
                let _ = write!(self.out, r#" fill="{}""#, c.to_hex());
                if c.a < 255 {
                    let _ = write!(self.out, r#" fill-opacity="{}""#, num(c.opacity()));
                }
            }
            None => self.out.push_str(r#" fill="none""#),
        }
        match style.and_then(|s| s.stroke.filter(|_| s.stroke_width > 0.0).map(|c| (c, s.stroke_width))) {
            Some((c, w)) => {
                let _ = write!(self.out, r#" stroke="{}" stroke-width="{}""#, c.to_hex(), num(w));
                if c.a < 255 {
                    let _ = write!(self.out, r#" stroke-opacity="{}""#, num(c.opacity()));
                }
            }
            None => self.out.push_str(r#" stroke="none""#),
        }
    }

    fn path(&mut self, polys: &[Polygon<f64>], fill: Option<Color>, style: Option<&Style>) {
        let (d, rings) = path_data(polys);
        if d.is_empty() {
            return;
        }
        self.paths += 1;
        self.indent();
        let _ = write!(self.out, r#"<path id="piece_{:04}" d="{}""#, self.paths, d);
        self.paint_attrs(fill, style);
        if rings > 1 {
            self.out.push_str(r#" fill-rule="evenodd""#);
        }
        self.out.push_str("/>\n");
    }

    fn open_group(&mut self, label: &str) {
        self.groups += 1;
        self.indent();
        let _ = writeln!(self.out, r#"<g id="group_{:04}"><title>{}</title>"#, self.groups, xml_escape(label));
        self.depth += 1;
    }

    fn close_group(&mut self) {
        self.depth -= 1;
        self.indent();
        self.out.push_str("</g>\n");
    }

    fn shape(&mut self, name: &str, s: &ShapeObj) {
        let tol = self.doc.settings.tolerance();
        let geom = s.world_geometry(tol);
        match &s.pattern {
            None => self.path(&geom.0, s.style.fill, Some(&s.style)),
            Some(fill) => {
                // A live pattern fill is exported exactly as previewed: expanded pieces.
                self.open_group(name);
                if fill.keep_background {
                    self.path(&geom.0, s.style.fill, Some(&s.style));
                }
                let res = pattern_pieces(s, &self.doc.settings, false);
                for p in res.pieces {
                    self.path(std::slice::from_ref(&p.poly), Some(p.color), None);
                }
                self.close_group();
            }
        }
    }

    fn object(&mut self, o: &Object) {
        match &o.kind {
            ObjectKind::Shape(s) => self.shape(&o.name, s),
            ObjectKind::Group(children) => {
                self.open_group(&o.name);
                for c in children.iter() {
                    self.object(c);
                }
                self.close_group();
            }
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Bounds of an object including half its stroke width.
pub(crate) fn visual_bounds(o: &Object, tol: f64) -> Option<Bounds> {
    let mut b = None;
    o.for_each_shape(&mut |_, s| {
        let mut sb = bounds_of(&s.world_geometry(tol));
        let hw = if s.style.stroke.is_some()
            && s.pattern.as_ref().is_none_or(|f| f.keep_background || f.include_outline)
        {
            s.style.stroke_width.max(0.0) / 2.0
        } else {
            0.0
        };
        if let Some(x) = sb.as_mut() {
            *x = [x[0] - hw, x[1] - hw, x[2] + hw, x[3] + hw];
        }
        b = bounds_union(b, sb);
    });
    b
}

/// The top-level objects that will be exported.
pub(crate) fn export_objects<'a>(doc: &'a Document, selection: &[ObjId], opts: &SvgOptions) -> Vec<&'a Object> {
    let selected: std::collections::HashSet<ObjId> = selection.iter().copied().collect();
    doc.objects
        .iter()
        .filter(|o| !opts.only_selected || selected.contains(&o.id))
        .collect()
}

/// The exported page in mm: the objects' visual bounds or the artboard.
pub(crate) fn export_page(doc: &Document, objs: &[&Object], bounds: SvgBounds) -> Bounds {
    let tol = doc.settings.tolerance();
    match bounds {
        SvgBounds::Artboard => None,
        SvgBounds::Objects => objs.iter().fold(None, |b, o| bounds_union(b, visual_bounds(o, tol))),
    }
    .filter(|b| b[2] > b[0] && b[3] > b[1])
    .unwrap_or([0.0, 0.0, doc.artboard.width.max(1.0), doc.artboard.height.max(1.0)])
}

/// Build the SVG document text. Returns the text and the number of `<path>` elements.
pub fn export_svg(doc: &Document, selection: &[ObjId], opts: &SvgOptions) -> (String, usize) {
    let objs = export_objects(doc, selection, opts);
    let vb = export_page(doc, &objs, opts.bounds);
    let (w, h) = (vb[2] - vb[0], vb[3] - vb[1]);

    let mut wr = Writer {
        doc,
        out: String::new(),
        paths: 0,
        groups: 0,
        depth: 0,
    };
    wr.out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n");
    let _ = writeln!(
        wr.out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" version="1.1" width="{w}mm" height="{h}mm" viewBox="{x} {y} {w} {h}">"#,
        w = num(w),
        h = num(h),
        x = num(vb[0]),
        y = num(vb[1]),
    );
    wr.out.push_str("  <desc>Created with Pattern Expander. Units: 1 user unit = 1 mm.</desc>\n");
    for o in objs {
        wr.object(o);
    }
    wr.out.push_str("</svg>\n");
    let n = wr.paths;
    (wr.out, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, ShapeObj};
    use crate::pattern::{Layout, Pattern, PatternFill};
    use crate::shapes::{ShapeGeom, Style, Transform};

    fn attr<'a>(svg: &'a str, tag_start: &str, name: &str) -> &'a str {
        let tag = &svg[svg.find(tag_start).unwrap()..];
        let key = format!(" {name}=\"");
        let s = tag.find(&key).unwrap() + key.len();
        let e = tag[s..].find('"').unwrap();
        &tag[s..s + e]
    }

    #[test]
    fn rect_50mm_exports_physical_size() {
        let mut doc = Document::default();
        let style = Style {
            stroke: None,
            ..Style::default()
        };
        doc.add_shape("r", ShapeObj::new(ShapeGeom::Rect, Transform::new(60.0, 70.0, 50.0, 50.0), style));
        let (svg, n) = export_svg(&doc, &[], &SvgOptions::default());
        assert_eq!(n, 1);
        assert_eq!(attr(&svg, "<svg", "width"), "50mm");
        assert_eq!(attr(&svg, "<svg", "height"), "50mm");
        assert_eq!(attr(&svg, "<svg", "viewBox"), "35 45 50 50");
        assert!(svg.contains(r#"d="M35 45L85 45L85 95L35 95Z""#));
    }

    #[test]
    fn expanded_pattern_exports_one_path_per_piece() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(
            ShapeGeom::Star {
                points: 5,
                inner_ratio: 0.5,
            },
            Transform::new(50.0, 50.0, 80.0, 80.0),
            Style::default(),
        );
        s.pattern = Some(PatternFill::new(
            Pattern {
                layout: Layout::Honeycomb,
                cell_size: 7.0,
                ..Pattern::default()
            },
            &s.xf,
        ));
        let id = doc.add_shape("Star", s);
        let other = doc.add_shape("Other", ShapeObj::new(ShapeGeom::Ellipse, Transform::new(150.0, 50.0, 10.0, 10.0), Style::default()));
        let report = doc.apply_patterns(&[id]);

        let (svg, n) = export_svg(&doc, &[], &SvgOptions::default());
        assert_eq!(n, report.objects_created + 1);
        assert_eq!(svg.matches("<path ").count(), n);
        assert_eq!(svg.matches("<g ").count(), 1);
        assert_eq!(svg.matches("<g ").count(), svg.matches("</g>").count());
        assert!(svg.starts_with("<?xml") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains(r#"id="piece_0001""#));
        // Unique ids.
        let ids: std::collections::HashSet<_> = svg.match_indices("id=\"piece_").map(|(i, _)| &svg[i..i + 14]).collect();
        assert_eq!(ids.len(), n);
        // Coordinates never have more than 4 decimals.
        for d in svg.split(" d=\"").skip(1) {
            let d = &d[..d.find('"').unwrap()];
            for numtxt in d.split(|c: char| c == ' ' || c.is_ascii_alphabetic()).filter(|t| !t.is_empty()) {
                if let Some((_, frac)) = numtxt.split_once('.') {
                    assert!(frac.len() <= 4, "{numtxt}");
                }
            }
        }

        // Only-selected export.
        let (svg_sel, n_sel) = export_svg(
            &doc,
            &[other],
            &SvgOptions {
                only_selected: true,
                ..SvgOptions::default()
            },
        );
        assert_eq!(n_sel, 1);
        assert!(!svg_sel.contains("<g "));
    }

    #[test]
    fn holes_use_evenodd() {
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
        doc.add_shape("frame", ShapeObj::new(geom, Transform::new(10.0, 10.0, 20.0, 20.0), Style::default()));
        let (svg, _) = export_svg(&doc, &[], &SvgOptions::default());
        assert!(svg.contains(r#"fill-rule="evenodd""#));
        let d = attr(&svg, "<path", "d");
        assert_eq!(d.matches('Z').count(), 2, "outer ring + hole in one path");
    }

    #[test]
    fn number_formatting() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(1.23456), "1.2346");
        assert_eq!(num(-0.00001), "0");
        assert_eq!(num(f64::NAN), "0");
        assert_eq!(num(-2.5), "-2.5");
    }
}
