//! Project / preset JSON and file dialogs.
//!
//! This is the only module with platform-specific code (native file dialogs and
//! the user preset location); on WebAssembly those functions are stubs.

use crate::document::Document;
use crate::pattern::Pattern;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROJECT_FORMAT: &str = "pattern-expander-project";
pub const PRESET_FORMAT: &str = "pattern-expander-presets";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: String,
    version: u32,
    /// Always "mm": every length in the document is in millimetres.
    units: String,
    document: Document,
}

#[derive(Serialize, Deserialize)]
struct PresetFile {
    format: String,
    version: u32,
    presets: Vec<Pattern>,
}

pub fn project_to_json(doc: &Document) -> Result<String, String> {
    serde_json::to_string_pretty(&ProjectFile {
        format: PROJECT_FORMAT.into(),
        version: VERSION,
        units: "mm".into(),
        document: doc.clone(),
    })
    .map_err(|e| e.to_string())
}

pub fn project_from_json(text: &str) -> Result<Document, String> {
    let file: ProjectFile = serde_json::from_str(text).map_err(|e| format!("Invalid project file: {e}"))?;
    if file.format != PROJECT_FORMAT {
        return Err(format!("Not a Pattern Expander project (format \"{}\")", file.format));
    }
    if file.units != "mm" {
        return Err(format!("Unsupported units \"{}\"", file.units));
    }
    let mut doc = file.document;
    doc.sanitize();
    Ok(doc)
}

pub fn presets_to_json(presets: &[Pattern]) -> Result<String, String> {
    serde_json::to_string_pretty(&PresetFile {
        format: PRESET_FORMAT.into(),
        version: VERSION,
        presets: presets.to_vec(),
    })
    .map_err(|e| e.to_string())
}

/// Strict: only a real preset file (`"format": "pattern-expander-presets"`),
/// as written by Save preset / Export. Used for the `patterns` folder so other
/// JSON files there (projects, notes) are never mistaken for patterns.
fn preset_file(text: &str) -> Option<Vec<Pattern>> {
    serde_json::from_str::<PresetFile>(text)
        .ok()
        .filter(|f| f.format == PRESET_FORMAT)
        .map(|f| f.presets)
}

/// Accepts a preset file or a single bare pattern object.
pub fn presets_from_json(text: &str) -> Result<Vec<Pattern>, String> {
    if let Ok(file) = serde_json::from_str::<PresetFile>(text) {
        return Ok(file.presets);
    }
    serde_json::from_str::<Pattern>(text)
        .map(|p| vec![p])
        .map_err(|e| format!("Invalid preset file: {e}"))
}

pub fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn write_text(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save_project(path: &Path, doc: &Document) -> Result<(), String> {
    write_text(path, &project_to_json(doc)?)
}

pub fn load_project(path: &Path) -> Result<Document, String> {
    project_from_json(&read_text(path)?)
}

/// Name of the preset folder next to the executable.
pub const PATTERNS_DIR: &str = "patterns";

/// Where the user's patterns are kept: a `patterns` folder next to the
/// executable, one JSON file per pattern. Copying the program's folder
/// (the `.exe` plus `patterns`) to another PC brings every pattern along.
#[cfg(not(target_arch = "wasm32"))]
pub fn user_presets_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(PATTERNS_DIR))
}

#[cfg(target_arch = "wasm32")]
pub fn user_presets_dir() -> Option<PathBuf> {
    None
}

/// The previous location (a single `presets.json` in the user profile), read
/// once to move existing patterns into the `patterns` folder.
#[cfg(not(target_arch = "wasm32"))]
fn legacy_presets_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PatternExpander").join("presets.json"))
}

#[cfg(target_arch = "wasm32")]
fn legacy_presets_path() -> Option<PathBuf> {
    None
}

/// Load the user's patterns. If the `patterns` folder holds none yet, patterns
/// saved by an earlier version (in the user profile) are moved into it.
pub fn load_user_presets() -> Vec<Pattern> {
    let Some(dir) = user_presets_dir() else { return Vec::new() };
    let found = load_presets_from(&dir);
    if !found.is_empty() {
        return found;
    }
    let legacy = legacy_presets_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| presets_from_json(&t).ok())
        .unwrap_or_default();
    if !legacy.is_empty() {
        // Best effort: if the folder is not writable they still load this time.
        let _ = save_presets_to(&dir, &legacy);
    }
    legacy
}

/// Save the user's patterns into the `patterns` folder next to the executable.
pub fn save_user_presets(presets: &[Pattern]) -> Result<(), String> {
    let dir = user_presets_dir().ok_or("No location for user presets on this platform")?;
    save_presets_to(&dir, presets)
}

/// Every pattern in the `.json` files of `dir`, sorted by file name. A file may
/// hold one pattern or a whole preset library; unreadable files are skipped.
pub fn load_presets_from(dir: &Path) -> Vec<Pattern> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")))
        .collect();
    files.sort();
    let mut out: Vec<Pattern> = Vec::new();
    for f in files {
        let Some(list) = read_text(&f).ok().and_then(|t| preset_file(&t)) else { continue };
        for p in list {
            // Same name twice: the later file wins.
            match out.iter().position(|q| q.name == p.name) {
                Some(i) => out[i] = p,
                None => out.push(p),
            }
        }
    }
    out
}

/// Write one `<name>.json` per pattern into `dir` and remove `.json` files
/// that no longer belong to a pattern (deleted or renamed presets).
pub fn save_presets_to(dir: &Path, presets: &[Pattern]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut written: Vec<String> = Vec::new();
    for p in presets {
        let stem = file_stem_for(&p.name);
        // Two names that clean up to the same file name get a numbered suffix.
        let mut name = format!("{stem}.json");
        let mut k = 2;
        while written.iter().any(|w| w.eq_ignore_ascii_case(&name)) {
            name = format!("{stem} ({k}).json");
            k += 1;
        }
        write_text(&dir.join(&name), &presets_to_json(std::slice::from_ref(p))?)?;
        written.push(name);
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.filter_map(Result::ok) {
            let path = e.path();
            let is_json = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"));
            let name = e.file_name().to_string_lossy().into_owned();
            // Only files that really are pattern files are removed; anything
            // else someone put in the folder is left alone.
            let is_pattern_file = || read_text(&path).ok().and_then(|t| preset_file(&t)).is_some();
            if is_json && path.is_file() && !written.iter().any(|w| w.eq_ignore_ascii_case(&name)) && is_pattern_file() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// A safe file name for a pattern name (Windows-forbidden characters replaced).
fn file_stem_for(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    let stem: String = trimmed.chars().take(80).collect();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    if stem.is_empty() || reserved.iter().any(|r| r.eq_ignore_ascii_case(&stem)) {
        format!("pattern {stem}").trim().to_string()
    } else {
        stem
    }
}

/// Native file dialogs.
#[cfg(not(target_arch = "wasm32"))]
pub mod dialogs {
    use std::path::PathBuf;

    fn json() -> rfd::FileDialog {
        rfd::FileDialog::new().add_filter("JSON", &["json"])
    }

    pub fn open_project() -> Option<PathBuf> {
        json().set_title("Open project").pick_file()
    }

    pub fn save_project(current: Option<&PathBuf>) -> Option<PathBuf> {
        let name = current
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pattern-project.json".into());
        json().set_title("Save project").set_file_name(name).save_file()
    }

    pub fn save_svg() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("SVG", &["svg"])
            .set_title("Export SVG")
            .set_file_name("pattern.svg")
            .save_file()
    }

    pub fn open_preset() -> Option<PathBuf> {
        json().set_title("Import pattern presets").pick_file()
    }

    pub fn open_svg() -> Option<PathBuf> {
        rfd::FileDialog::new().add_filter("SVG", &["svg"]).set_title("Import SVG").pick_file()
    }

    pub fn save_cut(kind: &str, ext: &str) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter(kind, &[ext])
            .set_title(format!("Export {kind}"))
            .set_file_name(format!("pattern.{ext}"))
            .save_file()
    }

    pub fn save_ai() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("Adobe Illustrator 3", &["ai"])
            .set_title("Export Adobe Illustrator 3")
            .set_file_name("pattern.ai")
            .save_file()
    }

    pub fn save_preset(name: &str) -> Option<PathBuf> {
        json()
            .set_title("Export pattern preset")
            .set_file_name(format!("{name}.json"))
            .save_file()
    }
}

/// WebAssembly build: no native dialogs (a browser download/upload UI would go here).
#[cfg(target_arch = "wasm32")]
pub mod dialogs {
    use std::path::PathBuf;
    pub fn open_project() -> Option<PathBuf> {
        None
    }
    pub fn save_project(_: Option<&PathBuf>) -> Option<PathBuf> {
        None
    }
    pub fn save_svg() -> Option<PathBuf> {
        None
    }
    pub fn open_preset() -> Option<PathBuf> {
        None
    }
    pub fn open_svg() -> Option<PathBuf> {
        None
    }
    pub fn save_ai() -> Option<PathBuf> {
        None
    }
    pub fn save_cut(_: &str, _: &str) -> Option<PathBuf> {
        None
    }
    pub fn save_preset(_: &str) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ShapeObj;
    use crate::pattern::PatternFill;
    use crate::shapes::{ShapeGeom, Style, Transform};

    #[test]
    fn project_roundtrip_keeps_mm_values() {
        let mut doc = Document::default();
        let mut s = ShapeObj::new(ShapeGeom::Ellipse, Transform::new(12.34, 56.78, 40.0, 25.5), Style::default());
        s.pattern = Some(PatternFill::new(Pattern::default(), &s.xf));
        let id = doc.add_shape("e", s);
        doc.group(&[id]); // single object: no-op
        let json = project_to_json(&doc).unwrap();
        assert!(json.contains("\"units\": \"mm\""));
        let back = project_from_json(&json).unwrap();
        let s = back.get(id).unwrap().as_shape().unwrap();
        assert_eq!(s.xf.cx, 12.34);
        assert_eq!(s.xf.h, 25.5);
        assert!(s.pattern.is_some());
        assert!(back.next_id > id);
        assert!(project_from_json("{}").is_err());
        assert!(project_from_json("not json").is_err());
    }

    #[test]
    fn presets_roundtrip() {
        let presets = crate::pattern::builtin_presets();
        let json = presets_to_json(&presets).unwrap();
        let back = presets_from_json(&json).unwrap();
        assert_eq!(back, presets);
        let single = serde_json::to_string(&presets[0]).unwrap();
        assert_eq!(presets_from_json(&single).unwrap().len(), 1);
    }

    /// The `patterns` folder: one file per pattern, renames / deletions remove
    /// stale files, other JSON files are never touched or loaded.
    #[test]
    fn patterns_folder_one_file_per_pattern() {
        let dir = std::env::temp_dir().join(format!("pe_patterns_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Pattern { name: "Acme.1".into(), ..Pattern::default() };
        let b = Pattern { name: "Bad/Name: 2?".into(), cell_size: 4.0, ..Pattern::default() };
        save_presets_to(&dir, &[a.clone(), b.clone()]).unwrap();
        assert!(dir.join("Acme.1.json").is_file());
        assert!(dir.join("Bad_Name_ 2_.json").is_file(), "unsafe characters replaced");

        // A foreign JSON file in the folder is neither loaded nor deleted.
        std::fs::write(dir.join("notes.json"), "{\"hello\": 1}").unwrap();
        let loaded = load_presets_from(&dir);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.iter().find(|p| p.name == "Bad/Name: 2?").unwrap().cell_size, 4.0);

        // Deleting a pattern removes its file; renaming replaces it.
        let mut a2 = a.clone();
        a2.name = "Acme.2".into();
        save_presets_to(&dir, &[a2]).unwrap();
        assert!(!dir.join("Acme.1.json").exists() && !dir.join("Bad_Name_ 2_.json").exists());
        assert!(dir.join("Acme.2.json").is_file());
        assert!(dir.join("notes.json").is_file(), "foreign file kept");
        assert_eq!(load_presets_from(&dir).len(), 1);

        // A shared file dropped into the folder (an exported library) is picked up.
        let lib = presets_to_json(&crate::pattern::builtin_presets()[..2]).unwrap();
        std::fs::write(dir.join("from colleague.json"), lib).unwrap();
        assert_eq!(load_presets_from(&dir).len(), 3);

        assert!(file_stem_for("  ").starts_with("pattern"));
        assert_eq!(file_stem_for("CON"), "pattern CON");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
