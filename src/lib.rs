//! Pattern Expander library.
//!
//! The geometry modules (`shapes`, `pattern`, `expand`, `document`,
//! `svg_export`, `svg_import`, `io`) contain no egui code and can be reused from a
//! WebAssembly or server build. `canvas` and `app` are the egui front end.

pub mod ai_export;
pub mod app;
pub mod canvas;
pub mod clipboard;
pub mod document;
pub mod emf_import;
pub mod flatten;
pub mod expand;
pub mod io;
pub mod pattern;
pub mod shapes;
pub mod splash;
pub mod svg_export;
pub mod svg_import;
