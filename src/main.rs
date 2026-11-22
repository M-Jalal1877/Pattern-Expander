//! Pattern Expander — build shapes, fill them with a repeating pattern, and
//! expand the pattern into individual vector pieces clipped to the shape.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Acme Engineering Hatching")
            .with_inner_size([1360.0, 820.0])
            .with_maximized(true)
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Pattern Expander",
        options,
        Box::new(|cc| Ok(Box::new(pattern_expander::app::PatternExpanderApp::new(cc)))),
    )
}

/// The WebAssembly entry point would use `eframe::WebRunner`; the geometry
/// modules are already platform independent.
#[cfg(target_arch = "wasm32")]
fn main() {}
