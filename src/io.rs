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

/// Where the user's preset library is kept.
#[cfg(not(target_arch = "wasm32"))]
pub fn user_presets_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PatternExpander").join("presets.json"))
}

#[cfg(target_arch = "wasm32")]
pub fn user_presets_path() -> Option<PathBuf> {
    None
}

pub fn load_user_presets() -> Vec<Pattern> {
    user_presets_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| presets_from_json(&t).ok())
        .unwrap_or_default()
}

pub fn save_user_presets(presets: &[Pattern]) -> Result<(), String> {
    let path = user_presets_path().ok_or("No location for user presets on this platform")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    write_text(&path, &presets_to_json(presets)?)
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
}
