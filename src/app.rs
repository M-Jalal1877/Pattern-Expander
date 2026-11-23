//! Application state, panels, menus, shortcuts and commands.

use crate::canvas::{Canvas, CanvasIo, Tool, now_ms, pattern_target};
use crate::document::{Document, History, ObjId, Object, ObjectKind, PaperPreset};
use crate::expand::ExpandMode;
use crate::io;
use crate::pattern::{
    CircleSpacing, ColorMode, HexOrientation, Layout as PatLayout, MAX_TILES, Pattern, PatternFill, TileShape,
    builtin_presets, default_palette, hash_cell,
};
use crate::shapes::{Color, ShapeGeom, normalized_tile};
use crate::svg_export::{SvgBounds, SvgOptions, export_svg};
use egui::{
    Align, Button, CollapsingHeader, Color32, ComboBox, DragValue, Grid, Key, KeyboardShortcut, Modifiers,
    RichText, ScrollArea, Ui, vec2,
};
use geo::MultiPolygon;
use std::path::PathBuf;

const NUDGE_KEY: u64 = 0x4e55_4447_4500;
const SETTINGS_KEY: u64 = 0x5345_5454_0000;

fn edit_key(tag: &str, id: ObjId) -> u64 {
    // FNV-1a over the tag, mixed with the id.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in tag.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

struct ExportDialog {
    only_selected: bool,
    bounds: SvgBounds,
    format: ExportFormat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Svg,
    /// Adobe Illustrator 3 (.ai)
    Ai3,
}

pub struct PatternExpanderApp {
    doc: Document,
    history: History,
    selection: Vec<ObjId>,
    tool: Tool,
    canvas: Canvas,
    /// Pattern used by "Add pattern fill" when no patterned shape is selected.
    template: Pattern,
    /// Pattern scale (%) used by "Add pattern fill" when no patterned shape is selected.
    template_scale_pct: f64,
    /// The floating Pattern window holding the full pattern settings.
    pattern_window: bool,
    /// Ctrl+V was held in the previous frame (edge detection for paste).
    paste_keys_were_down: bool,
    builtin_presets: Vec<Pattern>,
    user_presets: Vec<Pattern>,
    preset_name: String,
    status: String,
    project_path: Option<PathBuf>,
    export_dialog: Option<ExportDialog>,
    last_export_format: ExportFormat,
    /// Loading screen shown at start-up (None once it has faded out).
    splash: Option<crate::splash::Splash>,
}

impl PatternExpanderApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Light);
        Self {
            doc: Document::default(),
            history: History::default(),
            selection: Vec::new(),
            tool: Tool::Rect,
            canvas: Canvas::default(),
            template: Pattern::default(),
            template_scale_pct: 100.0,
            pattern_window: false,
            paste_keys_were_down: false,
            builtin_presets: builtin_presets(),
            // Filled in by the loading screen's background thread.
            user_presets: Vec::new(),
            preset_name: String::new(),
            status: "Draw a shape, add a pattern fill, then press Apply to expand it.".into(),
            project_path: None,
            export_dialog: None,
            last_export_format: ExportFormat::Svg,
            splash: Some(crate::splash::Splash::start()),
        }
    }

    // ---- commands ------------------------------------------------------------

    /// Drop selected ids that are no longer top-level objects. Runs every frame,
    /// so large selections (Select All on thousands of pieces) use a set.
    fn validate_selection(&mut self) {
        let doc = &self.doc;
        if self.selection.len() <= 16 {
            self.selection.retain(|id| doc.top_index(*id).is_some());
        } else {
            let top: std::collections::HashSet<ObjId> = doc.objects.iter().map(|o| o.id).collect();
            self.selection.retain(|id| top.contains(id));
        }
    }

    fn undo(&mut self) {
        if self.history.undo(&mut self.doc) {
            self.validate_selection();
            self.status = "Undo".into();
        }
    }

    fn redo(&mut self) {
        if self.history.redo(&mut self.doc) {
            self.validate_selection();
            self.status = "Redo".into();
        }
    }

    fn new_document(&mut self) {
        self.history.push(&self.doc);
        self.doc = Document::default();
        self.selection.clear();
        self.project_path = None;
        self.canvas.pattern_edit = false;
        self.canvas.fit_requested = true;
        self.status = "New document (A4, millimetres).".into();
    }

    fn open_project(&mut self) {
        let Some(path) = io::dialogs::open_project() else { return };
        match io::load_project(&path) {
            Ok(doc) => {
                self.history.push(&self.doc);
                self.doc = doc;
                self.selection.clear();
                self.canvas.pattern_edit = false;
                self.canvas.fit_requested = true;
                self.status = format!("Opened {}", path.display());
                self.project_path = Some(path);
            }
            Err(e) => self.status = format!("Open failed: {e}"),
        }
    }

    fn save_project(&mut self, save_as: bool) {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => io::dialogs::save_project(self.project_path.as_ref()),
        };
        let Some(path) = path else { return };
        match io::save_project(&path, &self.doc) {
            Ok(()) => {
                self.status = format!("Saved {}", path.display());
                self.project_path = Some(path);
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn import_svg(&mut self) {
        let Some(path) = io::dialogs::open_svg() else { return };
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                self.status = format!("{}: {e}", path.display());
                return;
            }
        };
        match crate::svg_import::import_svg(&data, self.doc.settings.tolerance()) {
            Ok(rep) => {
                self.history.push(&self.doc);
                let name = path.file_stem().map_or_else(|| "SVG import".into(), |s| s.to_string_lossy().into_owned());
                self.selection = crate::svg_import::add_to_document(&mut self.doc, rep.nodes, Some(&name));
                self.tool = Tool::Select;
                let mut msg = format!("Imported {} shape(s) from {}", rep.shapes, path.display());
                if rep.skipped > 0 {
                    msg.push_str(&format!(" ({} open line / text / image element(s) skipped)", rep.skipped));
                }
                self.status = msg;
            }
            Err(e) => self.status = format!("Import failed: {e}"),
        }
    }

    /// Paste vector graphics copied in another program (Illustrator: SVG,
    /// CorelDRAW: EMF), with their styling, centred on the artboard.
    fn paste(&mut self) {
        use crate::clipboard::ClipVector;
        let contents = match crate::clipboard::read() {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("Paste failed: {e}");
                return;
            }
        };
        let tol = self.doc.settings.tolerance();
        let mut last_err = None;
        for c in &contents.candidates {
            let (result, kind) = match c {
                ClipVector::Svg { data, dpi } => (crate::svg_import::import_svg_at_dpi(data, tol, *dpi), "SVG"),
                ClipVector::Emf(data) => (crate::emf_import::import_emf(data, tol), "metafile"),
            };
            match result {
                Ok(rep) => {
                    self.place_pasted(rep, kind);
                    return;
                }
                Err(e) => last_err = Some(e),
            }
        }
        self.status = match last_err {
            Some(e) => format!("Paste failed: {e}"),
            None if contents.formats.is_empty() => "Nothing to paste: the clipboard is empty.".into(),
            None => format!(
                "Nothing to paste: no vector graphics on the clipboard (formats: {}).",
                contents.formats.join(", ")
            ),
        };
    }

    fn place_pasted(&mut self, rep: crate::svg_import::ImportReport, kind: &str) {
        self.history.push(&self.doc);
        let ids = crate::svg_import::add_to_document(&mut self.doc, rep.nodes, None);
        self.doc.center_on_artboard(&ids);
        let n = ids.len();
        self.selection = ids;
        self.tool = Tool::Select;
        if self.doc.settings.auto_flatten_paste {
            // A separate undo step: one Ctrl+Z gives back the raw pasted objects.
            self.history.push(&self.doc);
            let ids = self.selection.clone();
            self.flatten(&ids, "Flattened paste", rep.skipped);
            return;
        }
        let mut msg = format!("Pasted {n} object(s) from the clipboard ({kind}), centred on the page.");
        if rep.skipped > 0 {
            msg.push_str(&format!(" {} text / image element(s) skipped.", rep.skipped));
        }
        self.status = msg;
    }

    /// Object → Flatten to Visible: the selection, or everything if nothing is selected.
    fn flatten_command(&mut self) {
        let ids: Vec<ObjId> = if self.selection.is_empty() {
            self.doc.objects.iter().map(|o| o.id).collect()
        } else {
            self.doc.top_indices(&self.selection).into_iter().map(|i| self.doc.objects[i].id).collect()
        };
        if ids.is_empty() {
            self.status = "Nothing to flatten.".into();
            return;
        }
        self.history.push(&self.doc);
        self.flatten(&ids, "Flattened", 0);
    }

    /// Replace `ids` by a group with only their visible parts (history already pushed).
    fn flatten(&mut self, ids: &[ObjId], group_name: &str, import_skipped: usize) {
        let opts = crate::flatten::FlattenOptions::from_settings(&self.doc.settings);
        let t0 = crate::canvas::now_ms();
        match crate::flatten::flatten_objects(&mut self.doc, ids, group_name, &opts) {
            Some(out) => {
                let ms = crate::canvas::now_ms() - t0;
                self.selection = out.group.into_iter().collect();
                self.status = out.report.message(ms, import_skipped);
            }
            None => {
                self.history.discard_last();
                self.status = "Nothing to flatten.".into();
            }
        }
    }

    fn export(&mut self, only_selected: bool, bounds: SvgBounds, format: ExportFormat) {
        let opts = SvgOptions { only_selected, bounds };
        let (path, text, paths) = match format {
            ExportFormat::Svg => {
                let Some(path) = io::dialogs::save_svg() else { return };
                let (svg, n) = export_svg(&self.doc, &self.selection, &opts);
                (path, svg, n)
            }
            ExportFormat::Ai3 => {
                let Some(path) = io::dialogs::save_ai() else { return };
                let title = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let (ai, n) = crate::ai_export::export_ai(&self.doc, &self.selection, &opts, &title);
                (path, ai, n)
            }
        };
        match io::write_text(&path, &text) {
            Ok(()) => self.status = format!("Exported {paths} paths to {}", path.display()),
            Err(e) => self.status = format!("Export failed: {e}"),
        }
    }

    fn delete_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.history.push(&self.doc);
        let n = self.doc.delete(&self.selection);
        self.selection.clear();
        self.canvas.pattern_edit = false;
        self.status = format!("Deleted {n} object(s)");
    }

    fn select_all(&mut self) {
        self.selection = self.doc.objects.iter().map(|o| o.id).collect();
    }

    fn group(&mut self) {
        if self.selection.len() < 2 {
            return;
        }
        self.history.push(&self.doc);
        match self.doc.group(&self.selection) {
            Some(g) => {
                self.selection = vec![g];
                self.status = "Grouped".into();
            }
            None => self.history.discard_last(),
        }
    }

    fn make_compound(&mut self) {
        self.history.push(&self.doc);
        match self.doc.make_compound(&self.selection) {
            Some(id) => {
                self.selection = vec![id];
                self.status = "Compound path made: overlapping areas became holes (even-odd).".into();
            }
            None => {
                self.history.discard_last();
                self.status = "Select two or more shapes (not groups) to make a compound path.".into();
            }
        }
    }

    /// The selected objects in stacking order. Selections only hold top-level
    /// objects, so this is one linear pass — fine for Select All on 200 000 pieces
    /// (a document search per selected id would be quadratic).
    fn selected(&self) -> impl Iterator<Item = &Object> + '_ {
        self.doc.top_indices(&self.selection).into_iter().map(|i| &self.doc.objects[i])
    }

    fn ungroup(&mut self) {
        let groups: Vec<ObjId> = self.selected().filter(|o| o.is_group()).map(|o| o.id).collect();
        if groups.is_empty() {
            self.status = "Select a group (e.g. an expanded pattern) to ungroup.".into();
            return;
        }
        self.history.push(&self.doc);
        let released = self.doc.ungroup(&groups);
        self.status = format!("Ungrouped into {} objects", released.len());
        self.selection = released;
    }

    fn apply(&mut self) {
        let ids: Vec<ObjId> = self.selected().filter(|o| o.has_pattern()).map(|o| o.id).collect();
        if ids.is_empty() {
            self.status = "Select a shape with a pattern fill, then press Apply.".into();
            return;
        }
        self.history.push(&self.doc);
        let t0 = now_ms();
        let report = self.doc.apply_patterns(&ids);
        let ms = now_ms() - t0;
        if report.shapes_expanded == 0 {
            self.history.discard_last();
            self.status = "Nothing to expand: the pattern produced no pieces inside the shape.".into();
            return;
        }
        self.selection = report.new_groups.clone();
        self.canvas.pattern_edit = false;
        let mut msg = format!("Expanded into {} objects in {:.0} ms", report.objects_created, ms);
        if report.capped {
            msg.push_str(&format!(" — warning: tile limit ({MAX_TILES}) reached, pattern is incomplete"));
        }
        if report.empty > 0 {
            msg.push_str(&format!(" — {} shape(s) produced no pieces", report.empty));
        }
        self.status = msg;
    }

    fn nudge(&mut self, dx: f64, dy: f64) {
        if self.selection.is_empty() {
            return;
        }
        self.history.push_coalesced(&self.doc, NUDGE_KEY);
        if self.canvas.pattern_edit
            && let Some(id) = pattern_target(&self.doc, &self.selection)
            && let Some(s) = self.doc.shape_mut(id)
        {
            let xf = s.xf.clone();
            if let Some(fill) = s.pattern.as_mut() {
                let mut f = fill.world_frame(&xf);
                f.origin = [f.origin[0] + dx, f.origin[1] + dy];
                fill.set_world_frame(&xf, f);
                s.touch();
            }
            return;
        }
        for id in self.selection.clone() {
            if let Some(o) = self.doc.get_mut(id) {
                o.translate(dx, dy);
            }
        }
    }

    fn toggle_pattern_edit(&mut self) {
        if self.canvas.pattern_edit {
            self.canvas.pattern_edit = false;
        } else if pattern_target(&self.doc, &self.selection).is_some() {
            self.canvas.pattern_edit = true;
            self.tool = Tool::Select;
            self.status = "Pattern Edit: drag inside the shape to move, use the square handle to scale, the round handle to rotate (Shift snaps). Esc or P to leave.".into();
        } else {
            self.status = "Select one shape with a pattern fill to edit its pattern.".into();
        }
    }

    fn canvas_io(&mut self) -> (&mut Canvas, CanvasIo<'_>) {
        (
            &mut self.canvas,
            CanvasIo {
                doc: &mut self.doc,
                history: &mut self.history,
                selection: &mut self.selection,
                tool: self.tool,
                status: &mut self.status,
            },
        )
    }

    // ---- keyboard ------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        if sc(Modifiers::COMMAND, Key::S) {
            self.save_project(false);
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if sc(cmd_shift, Key::F) {
            self.flatten_command();
        }
        if sc(cmd_shift, Key::G) {
            self.ungroup();
        } else if sc(Modifiers::COMMAND, Key::G) {
            self.group();
        }
        if sc(cmd_shift, Key::Z) || sc(Modifiers::COMMAND, Key::Y) {
            self.redo();
        } else if sc(Modifiers::COMMAND, Key::Z) {
            self.undo();
        }
        if sc(Modifiers::COMMAND, Key::O) {
            self.open_project();
        }
        if sc(Modifiers::COMMAND, Key::N) {
            self.new_document();
        }
        if sc(Modifiers::COMMAND, Key::E) {
            self.open_export_dialog();
        }
        if sc(Modifiers::COMMAND, Key::I) {
            self.import_svg();
        }
        // Ctrl+V: egui reports it as a Paste / PasteImage event when the clipboard
        // has text or a bitmap, and not at all otherwise (e.g. only a metafile from
        // CorelDRAW), so the key state is also polled. One press pastes once.
        let (event, focused) = ctx.input(|i| {
            let ev = i
                .events
                .iter()
                .any(|e| matches!(e, egui::Event::Paste(_) | egui::Event::PasteImage(_)));
            (ev, i.focused)
        });
        let keys = focused && crate::clipboard::paste_keys_down();
        let pressed = keys && !self.paste_keys_were_down;
        // Remember the press (also when it came as an event) so the next frame,
        // with the keys still held, does not paste a second time.
        self.paste_keys_were_down = keys || event;
        if event || pressed {
            self.paste();
        }
        if sc(Modifiers::COMMAND, Key::A) {
            self.select_all();
        }
        if sc(Modifiers::COMMAND, Key::Num8) {
            self.make_compound();
        }

        let (pressed, mods) = ctx.input(|i| {
            let keys = [
                Key::Delete,
                Key::Backspace,
                Key::Escape,
                Key::Enter,
                Key::P,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::ArrowDown,
            ];
            let pressed: Vec<Key> = keys.into_iter().filter(|k| i.key_pressed(*k)).collect();
            (pressed, i.modifiers)
        });
        let step = if mods.shift { 1.0 } else { 0.1 };
        for key in pressed {
            match key {
                Key::Backspace if !self.canvas.pen_points.is_empty() => {
                    self.canvas.pen_points.pop();
                }
                Key::Delete | Key::Backspace => self.delete_selection(),
                Key::Escape => {
                    if !self.canvas.pen_points.is_empty() {
                        self.canvas.pen_points.clear();
                    } else if self.canvas.pattern_edit {
                        self.canvas.pattern_edit = false;
                        self.status = "Left Pattern Edit mode.".into();
                    } else {
                        self.selection.clear();
                    }
                }
                Key::Enter if self.tool == Tool::Pen => {
                    let (canvas, mut cio) = self.canvas_io();
                    canvas.finish_pen(&mut cio);
                }
                Key::P if mods.is_none() => self.toggle_pattern_edit(),
                // Arrows nudge the selection; with nothing selected (or the Hand
                // tool active) they pan the view instead.
                Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown
                    if self.selection.is_empty() || self.tool == Tool::Hand =>
                {
                    let s = if mods.shift { 200.0 } else { 40.0 };
                    let (dx, dy) = match key {
                        Key::ArrowLeft => (s, 0.0),
                        Key::ArrowRight => (-s, 0.0),
                        Key::ArrowUp => (0.0, s),
                        _ => (0.0, -s),
                    };
                    self.canvas.pan_by(dx, dy);
                }
                Key::ArrowLeft => self.nudge(-step, 0.0),
                Key::ArrowRight => self.nudge(step, 0.0),
                Key::ArrowUp => self.nudge(0.0, -step),
                Key::ArrowDown => self.nudge(0.0, step),
                _ => {}
            }
        }
        if mods.is_none() {
            for t in Tool::ALL {
                if ctx.input(|i| i.key_pressed(t.key())) {
                    self.tool = t;
                    if t != Tool::Select {
                        self.canvas.pattern_edit = false;
                    }
                }
            }
        }
    }

    fn open_export_dialog(&mut self) {
        // Keep the last chosen format.
        let format = self.last_export_format;
        self.export_dialog = Some(ExportDialog {
            only_selected: !self.selection.is_empty(),
            bounds: SvgBounds::Objects,
            format,
        });
    }

    // ---- panels ----------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(Button::new("New").shortcut_text("Ctrl+N")).clicked() {
                    self.new_document();
                }
                if ui.add(Button::new("Open Project…").shortcut_text("Ctrl+O")).clicked() {
                    self.open_project();
                }
                if ui.add(Button::new("Save Project").shortcut_text("Ctrl+S")).clicked() {
                    self.save_project(false);
                }
                if ui.button("Save Project As…").clicked() {
                    self.save_project(true);
                }
                ui.separator();
                if ui.add(Button::new("Import SVG…").shortcut_text("Ctrl+I")).clicked() {
                    self.import_svg();
                }
                if ui.add(Button::new("Export (SVG / AI)…").shortcut_text("Ctrl+E")).clicked() {
                    self.open_export_dialog();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(self.history.can_undo(), Button::new("Undo").shortcut_text("Ctrl+Z"))
                    .clicked()
                {
                    self.undo();
                }
                if ui
                    .add_enabled(self.history.can_redo(), Button::new("Redo").shortcut_text("Ctrl+Y"))
                    .clicked()
                {
                    self.redo();
                }
                ui.separator();
                if ui
                    .add(Button::new("Paste").shortcut_text("Ctrl+V"))
                    .on_hover_text("Paste shapes copied in Illustrator, CorelDRAW, Inkscape… (centred on the page)")
                    .clicked()
                {
                    self.paste();
                }
                if ui.add(Button::new("Select All").shortcut_text("Ctrl+A")).clicked() {
                    self.select_all();
                }
                if ui.add(Button::new("Delete").shortcut_text("Del")).clicked() {
                    self.delete_selection();
                }
                if ui.add(Button::new("Group").shortcut_text("Ctrl+G")).clicked() {
                    self.group();
                }
                if ui.add(Button::new("Ungroup").shortcut_text("Ctrl+Shift+G")).clicked() {
                    self.ungroup();
                }
                ui.separator();
                if ui.add(Button::new("Make Compound Path").shortcut_text("Ctrl+8")).clicked() {
                    self.make_compound();
                }
            });
            ui.menu_button("Object", |ui| {
                if ui
                    .add(Button::new("Flatten to Visible").shortcut_text("Ctrl+Shift+F"))
                    .on_hover_text(
                        "Keep only what is visible: trim hidden parts, remove white shapes, merge equal colours. \
                         Runs on the selection, or on everything if nothing is selected.",
                    )
                    .clicked()
                {
                    self.flatten_command();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("Fit Artboard").clicked() {
                    self.canvas.fit(self.doc.artboard.width, self.doc.artboard.height);
                }
                for z in [0.5, 1.0, 2.0, 4.0] {
                    if ui.button(format!("Zoom {:.0} %", z * 100.0)).clicked() {
                        self.canvas.set_zoom(z);
                    }
                }
                ui.separator();
                if ui.add(Button::new("Hand tool (pan)").shortcut_text("H")).clicked() {
                    self.tool = Tool::Hand;
                    ui.close();
                }
                ui.checkbox(&mut self.canvas.wheel_scrolls, "Mouse wheel scrolls the view")
                    .on_hover_text(
                        "On: the wheel / two-finger scroll pans and Ctrl+wheel (or pinch) zooms.\n\
                         Off: the wheel zooms; Shift+wheel pans sideways, Alt+wheel pans up and down.",
                    );
                ui.separator();
                ui.label(RichText::new("Pan: Hand tool (H) · Space + drag · middle-button drag").small().weak());
                ui.label(RichText::new("Arrow keys pan when nothing is selected").small().weak());
            });
            ui.separator();
            if ui.add_enabled(self.history.can_undo(), Button::new("↶ Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(self.history.can_redo(), Button::new("↷ Redo")).clicked() {
                self.redo();
            }
            ui.separator();
            if ui.button("Export…").on_hover_text("SVG or Adobe Illustrator 3 (Ctrl+E)").clicked() {
                self.open_export_dialog();
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.vertical_centered_justified(|ui| {
            for t in Tool::ALL {
                let key = format!("{:?}", t.key());
                let r = ui
                    .add(Button::selectable(self.tool == t, t.label()).min_size(vec2(0.0, 28.0)))
                    .on_hover_text(format!("{} ({key})", t.label()));
                if r.clicked() {
                    self.tool = t;
                    if t != Tool::Select {
                        self.canvas.pattern_edit = false;
                    }
                }
            }
        });
        ui.add_space(8.0);
        ui.separator();
        let hint = match self.tool {
            Tool::Select => "Click to select, Shift+click to add, drag to move. Arrows nudge 0.1 mm (Shift 1 mm).",
            Tool::Pen => "Click to add points. Double-click, Enter or click the first point to close. Esc cancels.",
            Tool::Hand => "Drag to pan the view. Wheel zooms. Any tool: hold Space or the middle button and drag.",
            _ => "Drag to draw (Shift = square, Alt = from centre). Click for a 20 mm shape.",
        };
        ui.add(egui::Label::new(RichText::new(hint).small().weak()).wrap());
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(&self.status);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.label(format!("{} objects", self.doc.objects.len()));
                ui.separator();
                ui.label(format!("Zoom {:.0} %", self.canvas.zoom * 100.0));
                ui.separator();
                match self.canvas.cursor_mm {
                    Some(c) => ui.monospace(format!("X {:>9.2} mm   Y {:>9.2} mm", c[0], c[1])),
                    None => ui.monospace("X        –      Y        –   "),
                };
            });
        });
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        CollapsingHeader::new(RichText::new("Object").strong())
            .default_open(true)
            .show(ui, |ui| self.object_section(ui));
        CollapsingHeader::new(RichText::new("Pattern").strong())
            .default_open(true)
            .show(ui, |ui| self.pattern_section(ui));
        CollapsingHeader::new(RichText::new("Apply (Expand)").strong())
            .default_open(true)
            .show(ui, |ui| self.apply_section(ui));
        CollapsingHeader::new(RichText::new("Paste & Flatten").strong())
            .default_open(false)
            .show(ui, |ui| self.flatten_section(ui));
        CollapsingHeader::new(RichText::new("Document").strong())
            .default_open(false)
            .show(ui, |ui| self.document_section(ui));
    }

    /// Settings for Flatten to Visible (stored with the document).
    fn flatten_section(&mut self, ui: &mut Ui) {
        let mut s = self.doc.settings.clone();
        let mut changed = false;
        changed |= ui
            .checkbox(&mut s.auto_flatten_paste, "Auto-flatten pasted objects")
            .on_hover_text("After Ctrl+V, keep only what is visible (one Ctrl+Z gives back the raw paste)")
            .changed();
        changed |= ui
            .checkbox(&mut s.merge_same_color, "Merge same-color shapes")
            .on_hover_text("Visible areas of exactly the same colour that touch or overlap become one object")
            .changed();
        ui.horizontal(|ui| {
            ui.label("White threshold");
            changed |= ui
                .add(DragValue::new(&mut s.white_threshold).range(128..=255))
                .on_hover_text(
                    "A colour counts as white (the artboard) and is removed when every RGB channel is at least this \
                     value (250 by default; CorelDRAW's CMYK 0/0/0/0 is 255/255/255)",
                )
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("Minimum piece area");
            changed |= ui
                .add(DragValue::new(&mut s.min_piece_area).speed(0.01).range(0.0..=1.0e4).fixed_decimals(2).suffix(" mm²"))
                .on_hover_text("Smaller slivers are removed (shared with Apply)")
                .changed();
        });
        if changed {
            self.history.push_coalesced(&self.doc, SETTINGS_KEY ^ 2);
            self.doc.settings = s;
        }
        if ui
            .add(Button::new("Flatten to Visible (Ctrl+Shift+F)").min_size(vec2(ui.available_width(), 26.0)))
            .on_hover_text("Runs on the selection, or on everything if nothing is selected")
            .clicked()
        {
            self.flatten_command();
        }
    }

    fn object_section(&mut self, ui: &mut Ui) {
        let sel = self.selection.clone();
        match sel.as_slice() {
            [] => {
                ui.weak("Nothing selected.");
            }
            [id] => {
                if self.doc.get(*id).is_some_and(Object::is_group) {
                    self.group_props(ui, *id);
                } else {
                    self.shape_props(ui, *id);
                }
            }
            many => {
                ui.label(format!("{} objects selected", many.len()));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Group").clicked() {
                        self.group();
                    }
                    if ui
                        .button("Make compound")
                        .on_hover_text("Combine into one shape; overlaps become holes (Ctrl+8)")
                        .clicked()
                    {
                        self.make_compound();
                    }
                    if ui.button("Delete").clicked() {
                        self.delete_selection();
                    }
                });
            }
        }
    }

    fn shape_props(&mut self, ui: &mut Ui, id: ObjId) {
        let Some(obj) = self.doc.get(id) else { return };
        let mut name = obj.name.clone();
        let Some(mut s) = obj.as_shape().cloned() else { return };
        let mut changed = false;
        Grid::new("shape_props").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("Name");
            changed |= ui.text_edit_singleline(&mut name).changed();
            ui.end_row();
            ui.label("Type");
            ui.label(s.geom.kind_name());
            ui.end_row();
            changed |= mm_row(ui, "Centre X", &mut s.xf.cx, -1.0e5, 1.0e5);
            changed |= mm_row(ui, "Centre Y", &mut s.xf.cy, -1.0e5, 1.0e5);
            changed |= mm_row(ui, "Width", &mut s.xf.w, 0.01, 1.0e5);
            changed |= mm_row(ui, "Height", &mut s.xf.h, 0.01, 1.0e5);
            ui.label("Rotation");
            changed |= ui
                .add(DragValue::new(&mut s.xf.rotation_deg).speed(0.5).range(-360.0..=360.0).fixed_decimals(2).suffix(" °"))
                .changed();
            ui.end_row();
            match &mut s.geom {
                ShapeGeom::Polygon { sides } => {
                    ui.label("Sides");
                    changed |= ui.add(DragValue::new(sides).range(3..=1000)).changed();
                    ui.end_row();
                }
                ShapeGeom::Star { points, inner_ratio } => {
                    ui.label("Points");
                    changed |= ui.add(DragValue::new(points).range(3..=1000)).changed();
                    ui.end_row();
                    ui.label("Inner radius");
                    changed |= ui
                        .add(DragValue::new(inner_ratio).speed(0.005).range(0.01..=1.0).fixed_decimals(2))
                        .on_hover_text("Inner radius as a fraction of the outer radius")
                        .changed();
                    ui.end_row();
                }
                _ => {}
            }
            ui.label("Fill");
            changed |= optional_color(ui, &mut s.style.fill, Color::rgb(0xb8, 0xd4, 0xf0));
            ui.end_row();
            ui.label("Stroke");
            changed |= optional_color(ui, &mut s.style.stroke, Color::BLACK);
            ui.end_row();
            changed |= mm_row(ui, "Stroke width", &mut s.style.stroke_width, 0.0, 100.0);
        });
        if s.pattern.is_some() {
            ui.weak(
                "Pattern fill: the fill and stroke colours are hidden unless “Keep shape as background” is on. \
                 The stroke width still sets how far the pattern covers the outline.",
            );
        }
        if changed {
            self.history.push_coalesced(&self.doc, edit_key("shape", id));
            s.touch();
            if let Some(o) = self.doc.get_mut(id) {
                o.name = name;
                o.kind = ObjectKind::Shape(s);
            }
        }
    }

    fn group_props(&mut self, ui: &mut Ui, id: ObjId) {
        let tol = self.doc.settings.tolerance();
        let Some(obj) = self.doc.get(id) else { return };
        let count = obj.shape_count();
        let mut name = obj.name.clone();
        let Some(b) = self.canvas.object_bounds(obj, tol) else { return };
        let (mut x, mut y, mut w, mut h) = (b[0], b[1], b[2] - b[0], b[3] - b[1]);
        let mut changed = false;
        Grid::new("group_props").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("Name");
            changed |= ui.text_edit_singleline(&mut name).changed();
            ui.end_row();
            ui.label("Type");
            ui.label(format!("Group of {count} objects"));
            ui.end_row();
            changed |= mm_row(ui, "Left X", &mut x, -1.0e5, 1.0e5);
            changed |= mm_row(ui, "Top Y", &mut y, -1.0e5, 1.0e5);
            changed |= mm_row(ui, "Width", &mut w, 0.01, 1.0e5);
            changed |= mm_row(ui, "Height", &mut h, 0.01, 1.0e5);
        });
        if ui.button("Ungroup (Ctrl+Shift+G)").clicked() {
            self.ungroup();
            return;
        }
        if changed {
            self.history.push_coalesced(&self.doc, edit_key("group", id));
            if let Some(o) = self.doc.get_mut(id) {
                o.name = name;
                o.translate(x - b[0], y - b[1]);
                let (w0, h0) = (b[2] - b[0], b[3] - b[1]);
                if w0 > 0.0 && h0 > 0.0 && ((w - w0).abs() > 1e-9 || (h - h0).abs() > 1e-9) {
                    o.scale_about([x, y], w / w0, h / h0);
                }
            }
        }
    }

    fn pattern_section(&mut self, ui: &mut Ui) {
        let (mut patterned, mut plain) = (Vec::new(), Vec::new());
        for o in self.selected() {
            if o.has_pattern() {
                patterned.push(o.id);
            } else if !o.is_group() {
                plain.push(o.id);
            }
        }
        let target = patterned.first().copied();

        ui.horizontal_wrapped(|ui| {
            if !plain.is_empty()
                && ui
                    .button("Add pattern fill")
                    .on_hover_text("Assign the current pattern (preset + scale) to the selected shape(s)")
                    .clicked()
            {
                self.history.push(&self.doc);
                for id in &plain {
                    if let Some(s) = self.doc.shape_mut(*id) {
                        let mut fill = PatternFill::new(self.template.clone(), &s.xf);
                        fill.transform.scale_pct = self.template_scale_pct;
                        s.pattern = Some(fill);
                        s.touch();
                    }
                }
                self.status = "Pattern fill added (live preview). Press P to edit it on the canvas, Apply to expand.".into();
            }
            if !patterned.is_empty() && ui.button("Remove pattern").clicked() {
                self.history.push(&self.doc);
                for id in &patterned {
                    if let Some(s) = self.doc.shape_mut(*id) {
                        s.pattern = None;
                        s.touch();
                    }
                }
                self.canvas.pattern_edit = false;
            }
            if pattern_target(&self.doc, &self.selection).is_some() {
                let mut on = self.canvas.pattern_edit;
                if ui.toggle_value(&mut on, "Edit Pattern (P)").changed() {
                    self.toggle_pattern_edit();
                }
            }
        });
        ui.add_space(4.0);

        // Quick controls: preset + scale. Everything else lives in the Pattern window.
        let current = self.current_pattern(target);
        let mut scale = target
            .and_then(|id| self.doc.get(id))
            .and_then(Object::as_shape)
            .and_then(|s| s.pattern.as_ref())
            .map_or(self.template_scale_pct, |f| f.transform.scale_pct);
        let mut load = None;
        let mut scale_changed = false;
        Grid::new("pattern_quick").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("Preset");
            ComboBox::from_id_salt("preset_pick")
                .selected_text(current.name.as_str())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for p in &self.builtin_presets {
                        if ui.selectable_label(p.name == current.name, format!("◆ {}", p.name)).clicked() {
                            load = Some(p.clone());
                        }
                    }
                    for p in &self.user_presets {
                        if ui.selectable_label(p.name == current.name, &p.name).clicked() {
                            load = Some(p.clone());
                        }
                    }
                });
            ui.end_row();
            ui.label("Scale");
            scale_changed = ui
                .add(DragValue::new(&mut scale).speed(0.5).range(1.0..=10_000.0).fixed_decimals(1).suffix(" %"))
                .on_hover_text(if target.is_some() {
                    "Scale of the pattern inside the selected shape (Ctrl+wheel in Pattern Edit mode)"
                } else {
                    "Pattern scale used by the next “Add pattern fill”"
                })
                .changed();
            ui.end_row();
        });
        if let Some(p) = load {
            self.status = format!("Preset “{}” loaded.", p.name);
            self.load_preset(p, target);
        }
        if scale_changed {
            match target {
                Some(id) => {
                    self.history.push_coalesced(&self.doc, edit_key("pattern", id));
                    if let Some(s) = self.doc.shape_mut(id)
                        && let Some(fill) = s.pattern.as_mut()
                    {
                        fill.transform.scale_pct = scale;
                        s.touch();
                    }
                }
                None => self.template_scale_pct = scale,
            }
        }
        ui.add_space(4.0);
        let label = if target.is_some() { "Edit pattern…" } else { "New pattern…" };
        let mut open = self.pattern_window;
        if ui
            .add(Button::new(label).selected(open).min_size(vec2(ui.available_width(), 26.0)))
            .on_hover_text("Layout, tile shape, cell size, gap, colours, placement and preset management")
            .clicked()
        {
            open = !open;
        }
        self.pattern_window = open;
    }

    /// Floating window with the full pattern settings.
    fn pattern_window(&mut self, ctx: &egui::Context) {
        if !self.pattern_window {
            return;
        }
        let mut open = true;
        egui::Window::new("Pattern")
            .open(&mut open)
            .resizable([false, true])
            .vscroll(true)
            .default_pos(egui::pos2(140.0, 70.0))
            .default_height(ctx.content_rect().height() - 120.0)
            .show(ctx, |ui| self.pattern_details(ui));
        if !open {
            self.pattern_window = false;
        }
    }

    fn pattern_details(&mut self, ui: &mut Ui) {
        let patterned: Vec<ObjId> = self.selected().filter(|o| o.has_pattern()).map(|o| o.id).collect();
        let tile_source = self.selected().find(|o| !o.has_pattern()).map(|o| o.id);
        let target = patterned.first().copied();

        // Tile shape from an object: shown first, since it is the main way to
        // build a pattern from your own artwork (drawn or imported SVG).
        let current = self.current_pattern(target);
        let mut use_src = false;
        let mut back_to_layout = false;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                tile_thumbnail(ui, &current, 64.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Tile shape").strong());
                    if current.tile_shape == TileShape::Custom && !current.custom_tile.is_empty() {
                        ui.weak("Custom: from an object");
                    } else {
                        ui.weak(current.tile_shape.label());
                    }
                    let src_name = tile_source.and_then(|id| self.doc.get(id)).map(|o| o.name.clone());
                    let label = match &src_name {
                        Some(n) => format!("Use “{n}” as tile"),
                        None => "Use selected object as tile".into(),
                    };
                    use_src = ui
                        .add_enabled(src_name.is_some(), Button::new(label))
                        .on_hover_text(if target.is_some() {
                            "The selected object (shape, group or imported SVG) becomes the tile of this pattern fill."
                        } else {
                            "The selected object (shape, group or imported SVG) becomes the tile of the new pattern."
                        })
                        .on_disabled_hover_text(
                            "Select the object to use as the tile (a shape, group or imported SVG without a pattern fill). \
                             To change the tile of an existing fill, Shift+click the tile object as well.",
                        )
                        .clicked();
                    if current.tile_shape == TileShape::Custom {
                        back_to_layout = ui.small_button("Back to layout shape").clicked();
                    }
                });
            });
        });
        if use_src && let Some(src) = tile_source {
            self.use_shape_as_tile(src, &patterned);
        }
        if back_to_layout {
            let mut p = self.current_pattern(target);
            p.tile_shape = TileShape::Auto;
            self.load_preset(p, target);
        }

        match target {
            Some(id) => {
                let Some(obj) = self.doc.get(id) else { return };
                let Some(s) = obj.as_shape() else { return };
                let Some(mut fill) = s.pattern.clone() else { return };
                let xf = s.xf.clone();
                ui.label(RichText::new(format!("Editing pattern of “{}”", obj.name)).strong());
                let mut changed = pattern_editor(ui, &mut fill.pattern);
                ui.separator();
                changed |= transform_editor(ui, &mut fill, &xf);
                if changed {
                    self.history.push_coalesced(&self.doc, edit_key("pattern", id));
                    if let Some(s) = self.doc.shape_mut(id) {
                        s.pattern = Some(fill);
                        s.touch();
                    }
                }
            }
            None => {
                ui.label(RichText::new("New pattern").strong());
                ui.weak("Used by “Add pattern fill”. Select a patterned shape to edit its own pattern.");
                pattern_editor(ui, &mut self.template);
            }
        }
        ui.separator();
        self.presets_ui(ui, target);
    }

    fn use_shape_as_tile(&mut self, src: ObjId, targets: &[ObjId]) {
        let tol = self.doc.settings.tolerance();
        let Some(obj) = self.doc.get(src) else { return };
        let src_name = obj.name.clone();
        // Union the parts so overlapping shapes of a group give one clean outline.
        let mut acc = MultiPolygon::<f64>::new(vec![]);
        obj.for_each_shape(&mut |_, s| {
            let g = s.world_geometry(tol);
            acc = if acc.0.is_empty() { g } else { geo::BooleanOps::union(&acc, &g) };
        });
        let Some(tile) = normalized_tile(&acc) else {
            self.status = "That object cannot be used as a tile.".into();
            return;
        };
        if targets.is_empty() {
            self.template.custom_tile = tile;
            self.template.tile_shape = TileShape::Custom;
            self.status = format!(
                "New pattern uses “{src_name}” as its tile. Now select the shape to fill and click Add pattern fill."
            );
        } else {
            self.history.push(&self.doc);
            for id in targets {
                if let Some(s) = self.doc.shape_mut(*id)
                    && let Some(fill) = s.pattern.as_mut()
                {
                    fill.pattern.custom_tile = tile.clone();
                    fill.pattern.tile_shape = TileShape::Custom;
                    s.touch();
                }
            }
            self.status = format!("Tile set from the selected shape on {} pattern(s).", targets.len());
        }
    }

    fn current_pattern(&self, target: Option<ObjId>) -> Pattern {
        target
            .and_then(|id| self.doc.get(id))
            .and_then(Object::as_shape)
            .and_then(|s| s.pattern.as_ref())
            .map_or_else(|| self.template.clone(), |f| f.pattern.clone())
    }

    fn load_preset(&mut self, p: Pattern, target: Option<ObjId>) {
        match target {
            Some(id) => {
                self.history.push(&self.doc);
                if let Some(s) = self.doc.shape_mut(id)
                    && let Some(fill) = s.pattern.as_mut()
                {
                    fill.pattern = p;
                    s.touch();
                }
            }
            None => self.template = p,
        }
    }

    fn presets_ui(&mut self, ui: &mut Ui, target: Option<ObjId>) {
        ui.label(RichText::new("Presets").strong());
        let mut load = None;
        let mut delete = None;
        ScrollArea::vertical().id_salt("preset_list").max_height(170.0).show(ui, |ui| {
            for p in &self.builtin_presets {
                if ui.selectable_label(false, format!("◆ {}", p.name)).on_hover_text("Built-in preset").clicked() {
                    load = Some(p.clone());
                }
            }
            for (i, p) in self.user_presets.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.selectable_label(false, &p.name).clicked() {
                        load = Some(p.clone());
                    }
                    if ui.small_button("✖").on_hover_text("Delete preset").clicked() {
                        delete = Some(i);
                    }
                });
            }
        });
        if let Some(p) = load {
            self.status = format!("Preset “{}” loaded.", p.name);
            self.load_preset(p, target);
        }
        if let Some(i) = delete {
            let p = self.user_presets.remove(i);
            self.persist_presets(format!("Preset “{}” deleted.", p.name));
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text("Preset name").desired_width(140.0));
            if ui.button("Save preset").clicked() {
                let mut p = self.current_pattern(target);
                let name = self.preset_name.trim();
                p.name = if name.is_empty() { format!("Preset {}", self.user_presets.len() + 1) } else { name.to_string() };
                let msg = format!("Preset “{}” saved.", p.name);
                match self.user_presets.iter().position(|q| q.name == p.name) {
                    Some(i) => self.user_presets[i] = p,
                    None => self.user_presets.push(p),
                }
                self.persist_presets(msg);
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Import…").clicked()
                && let Some(path) = io::dialogs::open_preset()
            {
                match io::read_text(&path).and_then(|t| io::presets_from_json(&t)) {
                    Ok(list) => {
                        let n = list.len();
                        for p in list {
                            match self.user_presets.iter().position(|q| q.name == p.name) {
                                Some(i) => self.user_presets[i] = p,
                                None => self.user_presets.push(p),
                            }
                        }
                        self.persist_presets(format!("Imported {n} preset(s)."));
                    }
                    Err(e) => self.status = e,
                }
            }
            if ui.button("Export…").clicked() {
                let p = self.current_pattern(target);
                if let Some(path) = io::dialogs::save_preset(&p.name) {
                    self.status = match io::presets_to_json(std::slice::from_ref(&p)).and_then(|t| io::write_text(&path, &t)) {
                        Ok(()) => format!("Preset exported to {}", path.display()),
                        Err(e) => e,
                    };
                }
            }
        });
        if let Some(dir) = io::user_presets_dir()
            && ui
                .button("Open patterns folder")
                .on_hover_text(format!(
                    "Your saved patterns are files in:\n{}\n\nCopy this folder together with the program to take them to another PC.",
                    dir.display()
                ))
                .clicked()
        {
            self.status = match open_folder(&dir) {
                Ok(()) => format!("Opened {}", dir.display()),
                Err(e) => format!("Could not open {}: {e}", dir.display()),
            };
        }
    }

    fn persist_presets(&mut self, ok_msg: String) {
        self.status = match io::save_user_presets(&self.user_presets) {
            Ok(()) => ok_msg,
            Err(e) => format!("Could not save presets: {e}"),
        };
    }

    fn apply_section(&mut self, ui: &mut Ui) {
        let mut settings = self.doc.settings.clone();
        let mut changed = false;
        for m in ExpandMode::ALL {
            changed |= ui.radio_value(&mut settings.expand_mode, m, m.label()).changed();
        }
        ui.horizontal(|ui| {
            ui.label("Minimum piece area");
            changed |= ui
                .add(
                    DragValue::new(&mut settings.min_piece_area)
                        .speed(0.01)
                        .range(0.0..=1.0e4)
                        .fixed_decimals(2)
                        .suffix(" mm²"),
                )
                .on_hover_text(
                    "Pieces smaller than this are removed (avoids slivers). For Apply the limit never exceeds 2 % of a \
                     whole tile, so very small tiles are kept.",
                )
                .changed();
        });
        if changed {
            self.history.push_coalesced(&self.doc, SETTINGS_KEY);
            self.doc.settings = settings;
        }

        if let Some(id) = pattern_target(&self.doc, &self.selection)
            && let Some(st) = self.canvas.pattern_stats(id)
        {
            let text = if let Some(est) = st.too_fine {
                format!("Pattern finer than the screen at this zoom (≈{est:.0} tiles) — shown as a tint; zoom in to see tiles.")
            } else if st.computing {
                "Preview: computing in the background…".to_string()
            } else {
                let mut t = format!("Preview: {} pieces · {:.1} ms", st.pieces, st.ms);
                if !st.exact {
                    t.push_str(" · simplified while dragging");
                }
                t
            };
            ui.weak(text);
            if st.capped {
                ui.colored_label(
                    Color32::from_rgb(0xc6, 0x28, 0x28),
                    format!("⚠ Tile limit of {MAX_TILES} reached — increase the cell size."),
                );
            }
        }
        let n = self.selected().filter(|o| o.has_pattern()).count();
        let label = if n > 1 { format!("Apply (Expand {n} shapes)") } else { "Apply (Expand)".into() };
        let button = Button::new(RichText::new(label).strong().size(15.0)).min_size(vec2(ui.available_width(), 34.0));
        if ui
            .add_enabled(n > 0, button)
            .on_disabled_hover_text("Select a shape with a pattern fill")
            .clicked()
        {
            self.apply();
        }
        ui.weak("Replaces the patterned shape with a group of individual pieces, exactly as previewed. Ctrl+Z undoes it.");
        if self.selected().any(|o| o.is_group())
            && ui.button("Ungroup (Ctrl+Shift+G)").clicked()
        {
            self.ungroup();
        }
    }

    fn document_section(&mut self, ui: &mut Ui) {
        let mut ab = self.doc.artboard.clone();
        let mut tol = self.doc.settings.flatten_tolerance;
        let mut changed = false;
        Grid::new("doc_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("Artboard");
            ComboBox::from_id_salt("paper")
                .selected_text(ab.preset.label())
                .show_ui(ui, |ui| {
                    for p in PaperPreset::ALL {
                        if ui.selectable_value(&mut ab.preset, p, p.label()).changed() {
                            if let Some((w, h)) = p.size() {
                                // Keep the current orientation.
                                let landscape = ab.width > ab.height;
                                (ab.width, ab.height) = if landscape { (h, w) } else { (w, h) };
                            }
                            changed = true;
                        }
                    }
                });
            ui.end_row();
            let mut sized = mm_row(ui, "Width", &mut ab.width, 1.0, 100_000.0);
            sized |= mm_row(ui, "Height", &mut ab.height, 1.0, 100_000.0);
            if sized {
                ab.preset = PaperPreset::Custom;
                changed = true;
            }
            ui.label("");
            if ui.button("Swap orientation").clicked() {
                std::mem::swap(&mut ab.width, &mut ab.height);
                changed = true;
            }
            ui.end_row();
            ui.label("Curve tolerance");
            let r = ui
                .add(DragValue::new(&mut tol).speed(0.001).range(0.001..=1.0).fixed_decimals(3).suffix(" mm"))
                .on_hover_text("Maximum deviation when ellipses and circles are converted to polygons");
            ui.end_row();
            if r.changed() {
                changed = true;
            }
        });
        if changed {
            self.history.push_coalesced(&self.doc, SETTINGS_KEY ^ 1);
            self.doc.artboard = ab;
            self.doc.settings.flatten_tolerance = tol;
        }
        ui.horizontal(|ui| {
            if ui.button("Fit artboard").clicked() {
                self.canvas.fit(self.doc.artboard.width, self.doc.artboard.height);
            }
            if ui.button("100 %").clicked() {
                self.canvas.set_zoom(1.0);
            }
        });
        ui.weak("At 100 % zoom one millimetre on the artboard is one millimetre on a correctly scaled screen (96 dpi × OS scaling).");
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.export_dialog.as_mut() else { return };
        let mut open = true;
        let mut go = false;
        let mut cancel = false;
        let has_sel = !self.selection.is_empty();
        egui::Window::new("Export")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    ui.radio_value(&mut dlg.format, ExportFormat::Svg, "SVG");
                    ui.radio_value(&mut dlg.format, ExportFormat::Ai3, "Adobe Illustrator 3 (.ai)");
                });
                ui.add_enabled(has_sel, egui::Checkbox::new(&mut dlg.only_selected, "Only selected objects"));
                ui.horizontal(|ui| {
                    ui.label("Page:");
                    ui.radio_value(&mut dlg.bounds, SvgBounds::Objects, "Fit objects");
                    ui.radio_value(&mut dlg.bounds, SvgBounds::Artboard, "Artboard");
                });
                ui.weak(match dlg.format {
                    ExportFormat::Svg => "Size is written in mm (1 SVG unit = 1 mm); every piece is its own <path>.",
                    ExportFormat::Ai3 => {
                        "Classic Illustrator 3 format (also valid EPS): exact size in points, every piece its own path, \
                         holes as compound paths. AI 3 has no RGB or transparency: colours are converted to CMYK."
                    }
                });
                ui.horizontal(|ui| {
                    go = ui.button("Export…").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if go {
            let (only, bounds, format) = (dlg.only_selected && has_sel, dlg.bounds, dlg.format);
            self.export_dialog = None;
            self.last_export_format = format;
            self.export(only, bounds, format);
        } else if cancel || !open {
            self.export_dialog = None;
        }
    }
}

impl eframe::App for PatternExpanderApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Loading screen: covers the app until start-up work is done, then fades out.
        if let Some(splash) = self.splash.as_mut() {
            if let Some(presets) = splash.take_presets() {
                self.user_presets = presets;
            }
            match splash.frame(&ctx) {
                Some(true) => return, // still covering: don't draw (or react to) the app yet
                Some(false) => {}     // fading out over the app
                None => self.splash = None,
            }
        }

        let idle = ctx.input(|i| !i.pointer.any_down()) && ctx.memory(|m| m.focused().is_none());
        if idle && !self.canvas.is_dragging() {
            self.history.end_coalescing();
        }
        self.shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("tools")
            .resizable(false)
            .exact_size(112.0)
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::right("properties")
            .resizable(true)
            .default_size(330.0)
            .min_size(280.0)
            .show(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let (canvas, mut cio) = self.canvas_io();
            canvas.ui(ui, &mut cio);
        });
        self.export_window(&ctx);
        self.pattern_window(&ctx);
        self.validate_selection();
    }
}

/// Show a folder in the system file manager (created first if missing).
fn open_folder(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    #[cfg(any(windows, unix))]
    {
        std::process::Command::new(cmd).arg(dir).spawn()?;
        Ok(())
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(std::io::Error::other("not supported on this platform"))
    }
}

// ---- widgets --------------------------------------------------------------------

/// A labelled millimetre field inside a 2-column grid (0.01 mm precision).
fn mm_row(ui: &mut Ui, label: &str, v: &mut f64, min: f64, max: f64) -> bool {
    ui.label(label);
    let r = ui.add(DragValue::new(v).speed(0.1).range(min..=max).fixed_decimals(2).suffix(" mm"));
    ui.end_row();
    r.changed()
}

fn color_edit(ui: &mut Ui, c: &mut Color) -> bool {
    let mut c32 = Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a);
    let changed = ui.color_edit_button_srgba(&mut c32).changed();
    if changed {
        let [r, g, b, a] = c32.to_srgba_unmultiplied();
        *c = Color { r, g, b, a };
    }
    changed
}

fn optional_color(ui: &mut Ui, c: &mut Option<Color>, default: Color) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let mut on = c.is_some();
        if ui.checkbox(&mut on, "").changed() {
            *c = if on { Some(c.unwrap_or(default)) } else { None };
            changed = true;
        }
        match c.as_mut() {
            Some(col) => changed |= color_edit(ui, col),
            None => {
                ui.weak("none");
            }
        }
    });
    changed
}

/// Small preview of a custom tile (filled with colour A); a dash for built-in tile shapes.
fn tile_thumbnail(ui: &mut Ui, p: &Pattern, size: f32) {
    use geo::TriangulateEarcut;
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    painter.rect_stroke(rect, 4.0, visuals.widgets.noninteractive.bg_stroke, egui::StrokeKind::Inside);
    if p.tile_shape != TileShape::Custom || p.custom_tile.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "—",
            egui::FontId::proportional(16.0),
            visuals.weak_text_color(),
        );
        return;
    }
    // Tiles are normalised to a unit box centred on the origin.
    let inner = (size - 10.0).max(1.0);
    let c = rect.center();
    let to_screen = |q: [f64; 2]| egui::pos2(c.x + q[0] as f32 * inner, c.y + q[1] as f32 * inner);
    let fill = Color32::from_rgba_unmultiplied(p.color_a.r, p.color_a.g, p.color_a.b, 255);
    let mut mesh = egui::Mesh::default();
    for rings in &p.custom_tile {
        let raw = rings.to_polygon().earcut_triangles_raw();
        let base = mesh.vertices.len() as u32;
        for v in &raw.vertices {
            mesh.colored_vertex(to_screen(*v), fill);
        }
        for t in raw.triangle_indices.as_chunks::<3>().0 {
            mesh.add_triangle(base + t[0] as u32, base + t[1] as u32, base + t[2] as u32);
        }
    }
    painter.add(mesh);
    let stroke = egui::Stroke::new(1.0, Color32::from_gray(60));
    for rings in &p.custom_tile {
        for ring in std::iter::once(&rings.exterior).chain(&rings.holes) {
            painter.add(egui::Shape::closed_line(ring.iter().map(|q| to_screen(*q)).collect(), stroke));
        }
    }
}

/// Pattern definition editor. Returns true if anything changed.
fn pattern_editor(ui: &mut Ui, p: &mut Pattern) -> bool {
    let mut c = false;
    Grid::new("pattern_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
        ui.label("Layout");
        ComboBox::from_id_salt("pattern_layout")
            .selected_text(p.layout.label())
            .show_ui(ui, |ui| {
                for l in PatLayout::ALL {
                    c |= ui.selectable_value(&mut p.layout, l, l.label()).changed();
                }
            });
        ui.end_row();
        match p.layout {
            PatLayout::Honeycomb => {
                ui.label("Orientation");
                ui.horizontal(|ui| {
                    c |= ui.radio_value(&mut p.hex_orientation, HexOrientation::FlatTop, "Flat-top").changed();
                    c |= ui.radio_value(&mut p.hex_orientation, HexOrientation::PointyTop, "Pointy-top").changed();
                });
                ui.end_row();
            }
            PatLayout::Brick => {
                ui.label("Row offset");
                c |= ui
                    .add(DragValue::new(&mut p.brick_offset).speed(0.01).range(0.0..=1.0).fixed_decimals(2))
                    .on_hover_text("Shift of each row as a fraction of the brick width (0.5 = running bond)")
                    .changed();
                ui.end_row();
                ui.label("Brick W : H");
                c |= ui
                    .add(DragValue::new(&mut p.brick_aspect).speed(0.05).range(0.1..=10.0).fixed_decimals(2))
                    .changed();
                ui.end_row();
            }
            PatLayout::Circle => {
                ui.label("Spacing");
                ui.horizontal(|ui| {
                    c |= ui.radio_value(&mut p.circle_spacing, CircleSpacing::Square, "Square").changed();
                    c |= ui.radio_value(&mut p.circle_spacing, CircleSpacing::Hex, "Hex").changed();
                });
                ui.end_row();
            }
            _ => {}
        }
        ui.label("Tile shape");
        ComboBox::from_id_salt("tile_shape")
            .selected_text(p.tile_shape.label())
            .show_ui(ui, |ui| {
                for t in TileShape::ALL {
                    let enabled = t != TileShape::Custom || !p.custom_tile.is_empty();
                    ui.add_enabled_ui(enabled, |ui| {
                        c |= ui.selectable_value(&mut p.tile_shape, t, t.label()).changed();
                    });
                }
            });
        ui.end_row();
        if p.tile_shape == TileShape::Star {
            ui.label("Star points");
            c |= ui.add(DragValue::new(&mut p.star_points).range(3..=100)).changed();
            ui.end_row();
            ui.label("Star inner ratio");
            c |= ui
                .add(DragValue::new(&mut p.star_inner_ratio).speed(0.005).range(0.05..=1.0).fixed_decimals(2))
                .changed();
            ui.end_row();
        }
        c |= mm_row(ui, "Cell size", &mut p.cell_size, crate::pattern::MIN_CELL, crate::pattern::MAX_CELL);
        let max_gap = p.cell_size * 0.95;
        c |= mm_row(ui, "Gap", &mut p.gap, 0.0, max_gap);
        ui.label("Tile scale");
        c |= ui
            .add(DragValue::new(&mut p.tile_scale_pct).speed(0.5).range(1.0..=400.0).fixed_decimals(1).suffix(" %"))
            .changed();
        ui.end_row();
        ui.label("Colours");
        ui.horizontal(|ui| {
            c |= ui.radio_value(&mut p.color_mode, ColorMode::Single, "Single").changed();
            c |= ui.radio_value(&mut p.color_mode, ColorMode::Alternating, "2 colours").changed();
            c |= ui.radio_value(&mut p.color_mode, ColorMode::Random, "Random").changed();
        });
        ui.end_row();
        match p.color_mode {
            ColorMode::Single => {
                ui.label("Colour");
                c |= color_edit(ui, &mut p.color_a);
                ui.end_row();
            }
            ColorMode::Alternating => {
                ui.label("Colour A / B");
                ui.horizontal(|ui| {
                    c |= color_edit(ui, &mut p.color_a);
                    c |= color_edit(ui, &mut p.color_b);
                });
                ui.end_row();
            }
            ColorMode::Random => {
                ui.label("Palette");
                ui.horizontal_wrapped(|ui| {
                    let mut remove = None;
                    for (i, col) in p.palette.iter_mut().enumerate() {
                        c |= color_edit(ui, col);
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove
                        && p.palette.len() > 1
                    {
                        p.palette.remove(i);
                        c = true;
                    }
                    if ui.small_button("+").on_hover_text("Add colour").clicked() {
                        let next = p.palette.last().copied().unwrap_or(p.color_a);
                        p.palette.push(next);
                        c = true;
                    }
                    if ui.small_button("Reset").clicked() {
                        p.palette = default_palette();
                        c = true;
                    }
                });
                ui.end_row();
                ui.label("Seed");
                ui.horizontal(|ui| {
                    c |= ui.add(DragValue::new(&mut p.seed).speed(1.0)).changed();
                    if ui.button("New seed").clicked() {
                        p.seed = hash_cell(p.seed, now_ms() as i64, 17, 0) % 1_000_000;
                        c = true;
                    }
                });
                ui.end_row();
            }
        }
    });
    c
}

/// Pattern placement editor (mirrors Pattern Edit mode). Returns true if changed.
fn transform_editor(ui: &mut Ui, fill: &mut PatternFill, xf: &crate::shapes::Transform) -> bool {
    let mut c = false;
    ui.label(RichText::new("Pattern transform").strong());
    Grid::new("pattern_xf").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
        c |= mm_row(ui, "Offset X", &mut fill.transform.offset_x, -1.0e5, 1.0e5);
        c |= mm_row(ui, "Offset Y", &mut fill.transform.offset_y, -1.0e5, 1.0e5);
        ui.label("Scale");
        c |= ui
            .add(DragValue::new(&mut fill.transform.scale_pct).speed(0.5).range(1.0..=10_000.0).fixed_decimals(1).suffix(" %"))
            .changed();
        ui.end_row();
        ui.label("Rotation");
        c |= ui
            .add(DragValue::new(&mut fill.transform.rotation_deg).speed(0.5).range(-360.0..=360.0).fixed_decimals(2).suffix(" °"))
            .changed();
        ui.end_row();
    });
    ui.weak(if fill.transform_with_object {
        "Offset is measured from the shape centre, along the shape's axes."
    } else {
        "Offset is the absolute position of the pattern origin."
    });
    let mut with_obj = fill.transform_with_object;
    if ui
        .checkbox(&mut with_obj, "Transform pattern with object")
        .on_hover_text("ON: moving, scaling or rotating the shape carries the pattern along. OFF: the pattern stays in place.")
        .changed()
    {
        fill.set_transform_with_object(with_obj, xf);
        c = true;
    }
    c |= ui
        .checkbox(&mut fill.include_outline, "Pattern covers outline")
        .on_hover_text(
            "The pattern also fills the shape's stroke, continuing seamlessly from the interior \
             (the shape grows by half the stroke width). Needs a stroke on the shape.",
        )
        .changed();
    c |= ui
        .checkbox(&mut fill.keep_background, "Keep shape as background")
        .on_hover_text("Keep the shape's own fill and stroke under the pieces (also kept by Apply)")
        .changed();
    if ui.button("Reset pattern transform").clicked() {
        fill.reset_transform(xf);
        c = true;
    }
    let f = fill.world_frame(xf);
    ui.weak(format!(
        "Pattern origin at ({:.2}, {:.2}) mm, {:.1} %, {:.2}°",
        f.origin[0],
        f.origin[1],
        f.scale * 100.0,
        f.rotation_deg
    ));
    c
}
