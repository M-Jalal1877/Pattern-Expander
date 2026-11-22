# Pattern Expander

A desktop vector tool that reproduces Illustrator's
**pattern fill → Expand → Pathfinder Crop → Ungroup** workflow:

1. Draw shapes.
2. Give a shape a repeating pattern fill. The pattern is a live preview; the shape is still a single object.
3. Move, scale and rotate the pattern right on the canvas (Pattern Edit mode).
4. Press **Apply**. The pattern becomes real, individual vector pieces clipped to the shape, exactly as previewed.
5. Ungroup to edit pieces one by one, then **Export SVG** at real-world size.

Every length in the app is in **millimetres**.

---

## Running

Requires Rust 1.88 or newer (stable).

```sh
cargo run            # debug build (already optimised enough for interactive use)
cargo run --release  # fastest, recommended for very dense patterns
cargo test           # unit tests
cargo clippy --all-targets
```

The executable is `Acme-Engineering-Hatching.exe` (in `target\debug` or `target\release`).
Cargo does not allow spaces in a binary name, so for distribution run

```powershell
powershell -ExecutionPolicy Bypass -File .\build-release.ps1
```

which builds the release version and saves it as `dist\Acme Engineering Hatching.exe`.

It runs on Windows (tested), Linux and macOS. The only OS-specific code is the native
file-dialog module in `src/io.rs` and the clipboard module in `src/clipboard.rs`.

At start-up a loading screen (`src/splash.rs`) shows the app name, a progress bar and the
current step. Meanwhile your preset library is read on a background thread. After at least
about one second it fades into the app; a click or key press skips the rest once loading has
finished.

---

## Workflow example

1. Choose **Star** (or press `S`) and drag on the artboard.
2. In the right panel, click **Add pattern fill**. A honeycomb appears inside the star.
3. Pick a **Preset** and a **Scale** in the panel. For layout, tile shape, cell size, gap and
   colours, click **Edit pattern…** (or **New pattern…** when no patterned shape is selected).
4. Press `P` for **Pattern Edit**. Drag inside the star to move the pattern, drag the square
   handle to scale it, and drag the round handle to rotate it.
5. Choose an Apply mode and click **Apply (Expand)**. The status bar shows something like
   *"Expanded into 214 objects in 12 ms"*.
6. `Ctrl+Shift+G` ungroups the result, so every piece can be selected, moved, recoloured or deleted.
7. `Ctrl+E` exports an SVG.

---

## Window layout

| Area | Contents |
|---|---|
| Top menu | File (New, Open Project, Save Project, Import SVG, Export SVG), Edit (Undo, Redo, Paste, Delete, Group, Ungroup, Make Compound Path), Object (Flatten to Visible), View (zoom) |
| Left toolbar | Select, Rectangle, Ellipse, Polygon, Star, Pen |
| Canvas | A4 artboard (white page) with a grey "ACME ENGINEERING" watermark, light mm grid, mm rulers on the top and left |
| Right panel | Object properties, Pattern (add/remove, Preset, Scale, **New/Edit pattern…** button), Apply, Document settings |
| Pattern window | Opened by **New/Edit pattern…**: layout, tile shape, cell size, gap, colours, pattern transform, preset save/import/export |
| Status bar | Last message, cursor position in mm, zoom, object count |

---

## Controls

### Tools

| Key | Tool | Use |
|---|---|---|
| `V` | Select | Click to select, Shift+click to add or remove, drag to move, drag on empty canvas for a marquee, drag handles to resize (Shift keeps proportions) |
| `R` | Rectangle | Drag to draw. Shift draws a square, Alt draws from the centre. A plain click makes a 20 × 20 mm shape. |
| `E` | Ellipse | Same as Rectangle |
| `O` | Regular polygon | Same as Rectangle; set the side count in the panel |
| `S` | Star | Same as Rectangle; set the point count and inner-radius ratio in the panel |
| `N` | Pen | Click to add points. Double-click, `Enter`, or a click on the first point closes the path. `Backspace` removes the last point, `Esc` cancels. |
| `H` | Hand | Drag to pan the view |

### View

| Input | Action |
|---|---|
| Mouse wheel | Zoom around the cursor |
| `Ctrl` + wheel, touchpad pinch | Zoom around the cursor |
| Hand tool (`H`) + drag, `Space` + drag, middle-button drag | Pan |
| `Shift` + wheel, tilt wheel, sideways touchpad scroll | Pan left / right |
| `Alt` + wheel | Pan up / down |
| Arrow keys (nothing selected, or Hand tool) | Pan (with Shift: further) |
| View menu → *Mouse wheel scrolls the view* | The wheel and two-finger scroll pan; `Ctrl` + wheel or pinch zooms |
| View menu / Document panel | Fit artboard, 50 / 100 / 200 / 400 % |

In Pattern Edit mode, `Ctrl` + wheel and `Alt` + wheel keep scaling and rotating the pattern.

### Editing

| Keys | Action |
|---|---|
| `Ctrl+Z` | Undo |
| `Ctrl+Y` or `Ctrl+Shift+Z` | Redo |
| `Delete` / `Backspace` | Delete selection |
| Arrow keys | Move the selection 0.1 mm (with Shift: 1 mm); with nothing selected they pan the view |
| `Ctrl+A` | Select all |
| `Ctrl+G` / `Ctrl+Shift+G` | Group / Ungroup |
| `Ctrl+8` | Make Compound Path: combines the selected shapes, and areas where they overlap become holes. For example, a circle drawn on a rectangle becomes a hole in it. |
| `Ctrl+N`, `Ctrl+O`, `Ctrl+S`, `Ctrl+E` | New, Open project, Save project, Export SVG |
| `Ctrl+I` | Import SVG |
| `Ctrl+V` | Paste shapes copied in Illustrator / CorelDRAW / Inkscape (centred on the page, auto-flattened) |
| `Ctrl+Shift+F` | Flatten to Visible (selection, or everything) |
| `Esc` | Cancel the pen path, leave Pattern Edit, or clear the selection |

Every edit can be undone, including Apply, pattern transforms and panel changes. Undo uses
document snapshots (up to 100 steps), and one slider drag counts as one step.

### Pattern Edit mode (`P` or the **Edit Pattern** button)

Pattern Edit needs exactly one selected shape that has a pattern fill. While it is active, the
mouse changes the **pattern** instead of the shape:

| Input | Action |
|---|---|
| Drag inside the shape | Move the pattern (changes Offset X/Y) |
| Drag the square handle, or `Ctrl` + wheel | Scale around the pattern origin. Add `Shift` to snap to 5 % steps. |
| Drag the round handle, or `Alt` + wheel | Rotate. Add `Shift` to snap to 15°. |
| Arrow keys | Nudge the pattern 0.1 mm (with Shift: 1 mm) |
| `Esc` or `P` | Leave Pattern Edit |

The magenta crosshair marks the pattern origin. The same values appear as number fields in
the Pattern window (Offset X/Y in mm, Scale %, Rotation °), next to a **Reset pattern transform**
button. Scale is also in the right panel.

**Transform pattern with object** (on by default): moving, resizing or rotating the shape
carries the pattern with it, and Offset is measured from the shape centre. When it is off, the
pattern stays fixed on the page, the shape moves over it, and Offset is an absolute position.
Switching the option never moves the pattern.

The preview is cached and is only rebuilt when something changes. If an exact preview takes
longer than about 14 ms, the canvas shows a simplified preview while you drag (uncut tiles
only). It draws the exact result again when you release the mouse.

---

## Units and real size

* The document stores millimetres (f64). Every field shows mm with 0.01 mm precision; rotation
  is in degrees and scales are in %.
* The screen mapping is `pixels = mm × zoom × DPI / 25.4`. egui works in 1/96-inch points that
  already include Windows display scaling, so at **100 % zoom a 10 mm square is about 10 mm
  on screen**, assuming the monitor reports its scaling correctly.
* Artboard presets: A3, A4 (default, 210 × 297 mm), A5, Letter, or Custom. A Swap orientation
  button switches portrait and landscape.
* Curves (ellipses, circular tiles) are flattened to polygons with a **curve tolerance**
  (default 0.01 mm, set under Document). This is the largest allowed distance between the
  polygon and the true curve.

---

## Patterns

A pattern is a **layout** (the lattice of cell centres) plus a **tile shape** placed in each cell.

### Parameters

| Parameter | Meaning |
|---|---|
| Cell size (mm) | Lattice spacing: the centre-to-centre distance of neighbouring cells |
| Gap (mm) | Space between neighbouring tiles (each tile shrinks by gap / 2 on every side) |
| Tile scale (%) | Extra scale applied to each tile around its own centre |
| Offset X/Y, Scale, Rotation | Pattern placement (see Pattern Edit mode) |
| Colours | **Single**; **2 colours** alternating; **Random** from a palette, using a seed so the result is reproducible (colours are hashed per cell, so they don't change when you move the shape) |

### Layouts

`s` is the cell size. Pattern-space cell (0, 0) is centred on the pattern origin.

| Layout | How it works | Natural tile |
|---|---|---|
| **Square grid** | Centres at `(i·s, j·s)` | Square of side `s − gap` |
| **Honeycomb**, flat-top | Columns `s·√3/2` apart; odd columns shifted down by `s/2` | Hexagon, `s − gap` across the flats |
| **Honeycomb**, pointy-top | Rows `s·√3/2` apart; odd rows shifted right by `s/2` | Hexagon, `s − gap` across the flats |
| **Brick / offset grid** | Bricks `s` wide and `s / ratio` high (ratio defaults to 2:1). Each row is shifted by *offset ratio* × width, cumulatively; the default 0.5 is a running bond. | Rectangle, minus the gap |
| **Triangle grid** | Rows of equilateral triangles with side `s`, alternating apex-up and apex-down | Triangle, inset by gap / 2 |
| **Diamond grid** | A square grid rotated by 45°: centres at `((i − j)·s/√2, (i + j)·s/√2)` | Square of side `s − gap` standing on a corner |
| **Circle grid** | Square spacing, or hex spacing (rows `s·√3/2` apart, odd rows shifted by `s/2`) | Circle of diameter `s − gap` |

With gap 0 and tile scale 100 %, square, honeycomb, brick, triangle and diamond tiles cover the
plane exactly, with no gaps and no overlaps.

### Tile shape override

**Follow layout** uses the natural tile from the table above. The other options are
**Square, Hexagon, Triangle, Circle, Star**, or **Custom shape**. Every override shape is sized
so that its inscribed circle matches the cell's (diameter `s − gap`, times tile scale). The star
uses that diameter as its outer diameter.

**Using your own object as the tile.** Any shape, group or imported SVG without a pattern fill
can be the tile. The **Tile shape** box at the top of the Pattern window shows a thumbnail of
the current tile and a **Use “name” as tile** button:

* *New pattern*: select the object (for example an imported SVG), click **New pattern…**, then
  **Use “name” as tile**. Now select the shape to fill and click **Add pattern fill**.
* *Existing fill*: select the patterned shape and Shift+click the tile object, then click the
  button in **Edit pattern…**.

The parts of a group are merged into one outline, then scaled so the larger side matches the
cell. **Back to layout shape** switches to the layout's natural tile.

### Presets

The preset list holds built-in presets (◆) followed by your own. **Save preset** stores the
current pattern under a name, in `%APPDATA%\PatternExpander\presets.json` on Windows
(`~/.config/PatternExpander/presets.json` elsewhere). **Import…** and **Export…** read and write
preset JSON files.

---

## Apply (Expand)

For every selected shape that has a pattern fill:

1. The shape's bounding box is enlarged by one cell.
2. Every tile covering that box is generated, with the current offset, scale and rotation.
3. Tiles whose bounding box misses the shape are skipped.
4. Each remaining tile is classified with a grid index of the shape's outline. A tile that no
   boundary edge crosses lies entirely inside or entirely outside, and is decided with one
   point test, without a boolean operation.
5. Mode:
   * **Crop to shape** (default): boundary tiles are intersected with the shape
     (`geo` BooleanOps). Interior tiles are kept as they are.
   * **Keep whole tiles – touching**: every uncut tile that intersects the shape.
   * **Keep whole tiles – inside only**: only uncut tiles fully inside the shape.
6. Pieces smaller than **Minimum piece area** (default 0.01 mm²) are dropped to avoid slivers.
   A sliver is small *relative to the tile*, so the limit never exceeds 2 % of a whole tile:
   with tiny tiles (small objects, small pattern scale) the cut edge pieces and the tiles
   themselves are kept.
7. A tile cut into several parts becomes several objects.
8. The shape is replaced by a group of pieces, each with its own fill colour. Turn on **Keep
   shape as background** to keep the original shape beneath them.

**Outline.** With **Pattern covers outline** on (the default, in the Pattern window), the pattern
also fills the shape's stroke. The stroke is centred on the edge, so the patterned area is the
shape grown outward by half the stroke width (mitred corners, SVG miter limit 4; holes shrink
by the same amount). Interior and outline are cut as **one** region, so the pattern runs
seamlessly across the edge: a tile on the border stays one piece. Without a stroke, or with
the option off, the pattern stops at the shape's edge.

Shapes with holes (use `Ctrl+8`) and concave shapes (stars, pen paths) are handled with the
even-odd rule. The live preview, Apply and SVG export all call the same function
(`document::pattern_pieces`), so the result always matches the preview. Tile generation is
capped at 200,000 tiles per shape; if the cap is hit, a warning appears in the panel and the
status bar. Every number is clamped, so zero sizes, a cell size of 0, NaN values and huge
counts don't crash the app.

---

## SVG export

`Ctrl+E` opens a small dialog with two options: *Only selected objects*, and a viewBox that
either fits the objects (default) or the artboard. It then shows the native save dialog.

* SVG 1.1, with `width="…mm" height="…mm"` and a viewBox in mm, so **1 SVG unit = 1 mm**. The
  file opens at its exact physical size in Illustrator, Inkscape and browsers.
* Every object is its own `<path>` with a unique id (`piece_0001`, …), its own `fill` and
  `stroke` (stroke widths in mm), and `fill-rule="evenodd"` when it has holes.
* Groups become `<g>`. A shape whose pattern is still live is exported exactly as previewed,
  as a group of pieces.
* Coordinates are written with at most 4 decimals.

## Adobe Illustrator 3 export (.ai)

In the export dialog (`Ctrl+E`), choose **Adobe Illustrator 3 (.ai)**. It uses the same options
as SVG export: only selected objects, and fit objects or the artboard as the page.

* The classic PostScript-based AI 3 format, which Illustrator, CorelDRAW, Inkscape and most sign
  and cutting software open. The file also defines its operators as PostScript, so it is valid
  EPS and previews or prints in any PostScript viewer.
* Exact size: 1 mm = 72/25.4 pt; the page is the bounding box, origin at the bottom left.
* Closed paths repeat their start point as the last line segment, as Illustrator writes them.
  Importers treat that segment as the closing one, so every corner survives: a hexagon keeps
  all 6.
* Every piece is its own path. Shapes with holes or several parts become **compound paths**
  (`*u … *U`), with outer rings counter-clockwise and holes clockwise, so holes are correct
  under either fill rule. Groups (and live pattern fills, exported as their pieces exactly as
  previewed) become AI groups (`u … U`).
* Strokes keep their width, join, cap and miter limit.
* AI 3 has no RGB colour and no transparency. Colours are converted to CMYK (the plain
  conversion Illustrator uses without colour management) and drawn opaque.

## SVG import

**File → Import SVG…** (`Ctrl+I`) adds the shapes of an SVG file to the document, selected and
grouped if there are several (named groups in the file are kept as groups).

* Sizes are exact: an SVG with `width="50mm"` imports at 50 mm. Unitless/px values are read at
  96 per inch (1 px = 0.2646 mm), the convention of Illustrator, Inkscape and browsers. Files
  exported by this app import back at the same size and position.
* `<path>`, `<rect>`, `<circle>`, `<ellipse>`, `<polygon>`, `<use>`, transforms and CSS styles
  are supported (parsed by `usvg`). Curves are flattened to the document's curve tolerance.
* Fill and stroke colours, opacity (including group opacity), stroke width, line joins, caps
  and miter limit are kept. Gradients become grey and are marked as such.
* `fill-rule="evenodd"` and `nonzero` holes are both kept.
* Clipping masks are applied (the clip shape itself is not imported).
* Open lines are outlined into filled shapes in the stroke colour, so they keep their look.
  The same happens to a stroke cut by a clipping mask.
* Text and images have no vector area and are skipped; the status bar says how many.

Imported objects are ordinary editable shapes: they can take a pattern fill, be expanded, or
become a pattern tile.

## Paste from Illustrator / CorelDRAW (`Ctrl+V`)

Copy objects in another program and press `Ctrl+V` (or **Edit → Paste**). The objects arrive
with their fill and stroke colours, stroke widths and holes, as separate objects, selected, and
centred on the page as a whole (their layout relative to each other is kept). With
**Auto-flatten pasted objects** on (the default) they are then flattened — see below.

| Source | Clipboard data used | Size |
|---|---|---|
| Adobe Illustrator | SVG markup (registered SVG format or text) | Illustrator's px are points (72 / inch), detected from its clipboard formats / generator comment |
| Inkscape, Figma, browsers | SVG | 96 px / inch, or the file's absolute units |
| CorelDRAW, other Windows apps | Enhanced Metafile (EMF) | the metafile's reference-device size (as Windows/Office play it back) |

SVG is preferred when both are offered. The EMF reader (`emf_import.rs`) understands the GDI
vector records: polygons, poly-polygons (holes), Bézier paths, rectangles, rounded rectangles,
ellipses, path brackets, pens (width, joins, caps, miter limit), brushes, fill mode, clip
regions (CorelDRAW PowerClip), mapping modes and world transforms. Text and bitmaps are skipped
(the status bar says how many). If nothing usable is on the clipboard, the status bar lists the
clipboard formats that were found.

## Flatten to Visible

After a paste the document should hold **only what is visible on the white artboard**, as clean
separate objects — the manual *Pathfinder Trim + Merge* work in Illustrator / CorelDRAW, done
automatically.

**When it runs**
* On every paste when **Auto-flatten pasted objects** is on (default; *Paste & Flatten*
  section of the side panel).
* **Object → Flatten to Visible** (`Ctrl+Shift+F`, or the button in *Paste & Flatten*): on the
  selected objects, or on everything if nothing is selected.
* It is one undo step. After an auto-flattened paste, the first `Ctrl+Z` gives back the raw
  pasted objects, the second removes the paste.
* The status bar reports e.g. *"Flattened: 38 objects → 21 visible objects, 9 white shapes
  removed, 4 hidden objects deleted (15 ms), skipped: 2 (gradient/transparency)"*.

**Rules** (objects are processed in stacking order, bottom to top)
1. Everything becomes filled areas. Groups are resolved; compound paths, fill rules and
   clipping masks were already applied when pasting. Every stroke is outlined into its own
   area in the stroke colour — real width, joins, caps and miter limit, using `i_overlay`'s
   stroker — lying just above its fill. Objects with neither fill nor stroke are deleted.
2. **Trim:** each area keeps only what no area above it covers, whatever that area's colour.
   A white shape or a white stroke on top therefore cuts its shape out of everything below.
   For speed, only areas whose bounding boxes overlap are tested.
3. **White is the artboard:** after trimming, all white areas are deleted; their only effect
   is the holes they cut. White = opaque with every RGB channel ≥ the **White threshold**
   (250 by default, adjustable). CorelDRAW's CMYK 0/0/0/0 arrives as RGB 255/255/255.
4. Objects that end up completely hidden are deleted.
5. **Merge** (*Merge same-color shapes*, default on): visible areas of exactly the same colour
   that touch or overlap are unioned into one object, like Illustrator's Merge.
6. Slivers smaller than the **Minimum piece area** (0.01 mm²) are removed — pieces and
   hairline holes, such as the slits text-to-curves leaves where a font's contours overlap — and
   outlines are simplified within the curve tolerance (0.01 mm). Sizes and positions stay
   exactly as pasted.
7. Every visible piece is its own object (one colour, no stroke) inside a group named
   *Flattened paste* (or *Flattened*), which `Ctrl+Shift+G` ungroups.

**Left alone:** shapes that cannot be flattened safely keep their original form. These are
gradients, transparency below 100 % (including group opacity), blend modes, masks and pattern
fills. They are neither trimmed, deleted nor merged. Where fully opaque (gradients, opaque
parts), they still cut the objects below them. They are counted as *skipped* in the status
message, along with text and images, which are not pasted at all. Merging never moves a piece
across such a shape in the stacking order.

The algorithm is `src/flatten.rs` (pure geometry, no egui).

## Project files

**Save Project** writes JSON (`"units": "mm"`) containing the full document: objects, groups,
pattern fills with their transforms, the artboard and the settings. **Open Project** validates
the file and repairs out-of-range values.

---

## Code structure

```
src/
  main.rs        starts eframe
  lib.rs         module list (the geometry modules are reusable without egui)
  app.rs         app state, panels, menus, shortcuts, commands
  canvas.rs      drawing, zoom/pan, rulers, hit testing, selection, handles, tools, Pattern Edit
  document.rs    Document, Object/Group, IDs, undo/redo, group/ungroup, compound, Apply
  shapes.rs      shape definitions -> geo polygons, colours, transforms
  pattern.rs     Pattern, layouts, tile generation, pattern placement (frame)
  expand.rs      the Apply/Expand algorithm
  svg_export.rs  SVG writer
  ai_export.rs   Adobe Illustrator 3 (.ai / EPS) writer
  svg_import.rs  SVG reader (usvg) -> editable shapes in mm
  emf_import.rs  Enhanced Metafile reader (CorelDRAW clipboard)
  flatten.rs     Flatten to Visible: stroke outlining, trim, white removal, merge
  clipboard.rs   Windows clipboard access (SVG / EMF candidates)
  io.rs          project / preset JSON, file dialogs (the only platform-specific module)
```

`shapes`, `pattern`, `expand`, `document` and `svg_export` contain no egui code, so they can be
reused in a WebAssembly or server build.

## Tests

`cargo test` runs 69 unit tests, including:

* tile generation for every layout (coverage, exact tile areas, origin placement, the tile cap,
  NaN input, reproducible random colours);
* expand in all three modes: a star filled with honeycomb in crop mode gives pieces whose
  total area is within 0.5 % of the star's area; shapes with holes; concave cuts producing
  separate pieces; the minimum-area filter;
* **WYSIWYG**: after moving, scaling and rotating a pattern, the applied pieces match the
  preview geometry vertex for vertex, and the tiles sit where the pattern frame says;
* SVG: a 50 × 50 mm rectangle exports with `width="50mm" height="50mm"` and a matching viewBox;
  the number of `<path>` elements equals the number of pieces; ids are unique; at most 4
  decimals; even-odd fill for holes;
* project and preset JSON round-trips, undo/redo of Apply, group/ungroup, compound paths;
* SVG / EMF import and paste: true mm sizes (Illustrator points, metafile mapping modes),
  holes, clipping masks, group opacity, gradients flagged, open lines outlined, a real GDI+
  metafile;
* **Flatten to Visible**: a white circle on a red square becomes a hole; a 2 mm white stroke
  cuts a ring (area within 0.5 %); white under blue is deleted, blue unchanged; a half-covered
  square keeps only its visible half; two overlapping red circles merge (merge on) or stay two
  (merge off); a lone white object is deleted; strokes outlined with their real width; gradients
  and transparency are kept and reported; one undo after an auto-flattened paste restores the
  raw pasted objects exactly.

Benchmarks are included but ignored by default:
* `cargo test --release -- --ignored --nocapture large_pattern_timing`: an A4 ellipse with a
  2 mm honeycomb (≈13,000 pieces) expands in about 37 ms, and the simplified drag preview takes
  about 14 ms.
* `cargo test --release -- --ignored --nocapture flatten_timing`: 600 overlapping shapes
  (with strokes and white shapes) flatten in about 100 ms.
* `cargo test --release -- --ignored --nocapture fine_pattern_on_text_timing`: a 130 mm line of
  text (≈16 000 vertices) with a 0.3 / 0.1 / 0.05 mm honeycomb expands in 0.17 / 0.4 / 1.0 s
  (it took 12 / 16 s before the optimisations below).

## Performance with fine patterns and many pieces

* **Local clipping.** Tiles on the shape's edge are cut against a small local piece of the
  shape (a lazily built grid of clipped neighbourhoods), never against the whole outline, and
  tiles are generated only near the shape's parts, so text and thin shapes don't waste the tile
  budget. A test checks the result piece for piece against the straightforward method.
* **Nothing hidden is computed.** When tiles would be smaller than 1.5 px on screen, the
  shape is drawn tinted with the pattern's average colour and coverage, and no tiles are
  computed until you zoom in. Apply always computes the exact pieces.
* **No waiting.** Previews expected to be slow (more than 20 000 tiles, or a previous run over
  30 ms) are computed on a background thread. Meanwhile the shape shows the tint and the panel
  says *computing in the background…*. Dragging a number field (such as Scale) uses the quick
  preview, like dragging on the canvas.
* **Drawing.** Pattern meshes are split into spatial chunks, and only chunks on screen are
  drawn. Moving a shape shifts its cached triangles instead of re-triangulating them.
* **Undo.** Snapshots share groups and outlines (copy-on-write), so a 200 000-piece expansion
  is not copied on every edit. History is limited to 100 steps and 2 million top-level objects
  in total, always keeping at least 5 steps.
* **Large selections.** Select All on thousands of loose pieces, followed by move, delete,
  ungroup, marquee or the panel, all run in linear time.

## Known limitations

* Resizing a **group** whose children are rotated by an angle that isn't a multiple of 90°
  only approximates non-uniform scaling, because each shape keeps a rotated-rectangle
  transform.
* With *Transform pattern with object* on, a non-uniform resize scales the pattern uniformly
  (by the geometric mean), so tiles are never distorted.
* The pen draws straight segments only; there are no Bézier curves.
* The WebAssembly entry point (`eframe::WebRunner`) is not implemented yet; the code is
  structured for it.
