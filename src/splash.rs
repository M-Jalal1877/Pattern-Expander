//! Loading screen shown while the application starts.
//!
//! It covers the window from the very first frame while start-up work runs —
//! the user's preset library is read on a background thread — then fades into
//! the app. It stays at least [`MIN_SECONDS`] so it never just flickers, and a
//! click or key press skips the rest once loading has finished.

use crate::pattern::Pattern;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Shape, Stroke, pos2, vec2};
use std::sync::mpsc::{Receiver, TryRecvError};

/// Minimum time the loading screen is visible (seconds).
const MIN_SECONDS: f64 = 1.1;
/// Fade-out duration (seconds).
const FADE_SECONDS: f64 = 0.35;

const BG: Color32 = Color32::from_rgb(0xf8, 0xfa, 0xfc);
const INK: Color32 = Color32::from_rgb(0x1f, 0x29, 0x37);
const MUTED: Color32 = Color32::from_rgb(0x6b, 0x72, 0x80);
const TRACK: Color32 = Color32::from_rgb(0xe2, 0xe8, 0xf0);
const BLUE: Color32 = Color32::from_rgb(0x1e, 0x88, 0xe5);
const ORANGE: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);

pub struct Splash {
    /// egui time of the first frame.
    start: Option<f64>,
    /// User presets being loaded in the background.
    presets: Option<Receiver<Vec<Pattern>>>,
    loaded: bool,
    /// When the fade-out started.
    fade_from: Option<f64>,
}

impl Splash {
    /// Start the loading screen and the background start-up work.
    pub fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            let _ = tx.send(crate::io::load_user_presets());
        });
        #[cfg(target_arch = "wasm32")]
        let _ = tx.send(crate::io::load_user_presets());
        Self { start: None, presets: Some(rx), loaded: false, fade_from: None }
    }

    /// The user presets, once they have been loaded (returned exactly once).
    pub fn take_presets(&mut self) -> Option<Vec<Pattern>> {
        let rx = self.presets.as_ref()?;
        match rx.try_recv() {
            Ok(p) => {
                self.presets = None;
                self.loaded = true;
                Some(p)
            }
            Err(TryRecvError::Disconnected) => {
                // The loader failed: carry on with no user presets.
                self.presets = None;
                self.loaded = true;
                None
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Advance and paint one frame. Returns `Some(true)` while the screen fully
    /// covers the app (skip drawing the app), `Some(false)` while it fades out
    /// over the app, and `None` once it is gone.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<bool> {
        let now = ctx.input(|i| i.time);
        let start = *self.start.get_or_insert(now);
        let elapsed = now - start;

        if self.fade_from.is_none() && self.loaded {
            let skipped = ctx.input(|i| i.pointer.any_click() || i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. })));
            if elapsed >= MIN_SECONDS || skipped {
                self.fade_from = Some(now);
            }
        }
        let alpha = match self.fade_from {
            Some(f) => 1.0 - ((now - f) / FADE_SECONDS).clamp(0.0, 1.0),
            None => 1.0,
        };
        if alpha <= 0.0 {
            return None;
        }
        self.paint(ctx, elapsed, alpha as f32);
        ctx.request_repaint();
        Some(self.fade_from.is_none())
    }

    fn progress(&self, elapsed: f64) -> f32 {
        let t = (elapsed / MIN_SECONDS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        // Hold just short of the end until the background work is done.
        let cap = if self.loaded { 1.0 } else { 0.9 };
        (eased * cap) as f32
    }

    fn status(&self, elapsed: f64) -> &'static str {
        if elapsed < 0.25 {
            "Starting…"
        } else if !self.loaded {
            "Loading presets…"
        } else if self.fade_from.is_some() || elapsed >= MIN_SECONDS * 0.85 {
            "Ready"
        } else {
            "Preparing canvas…"
        }
    }

    fn paint(&self, ctx: &egui::Context, elapsed: f64, alpha: f32) {
        // The debug painter draws above every panel and window.
        let painter = ctx.debug_painter();
        let screen = ctx.content_rect();
        let a = |c: Color32| c.gamma_multiply(alpha);
        painter.rect_filled(screen, CornerRadius::ZERO, a(BG));

        let c = screen.center();
        paint_logo(&painter, pos2(c.x, c.y - 95.0), elapsed, alpha);

        painter.text(pos2(c.x, c.y + 2.0), Align2::CENTER_CENTER, "Acme Engineering", FontId::proportional(34.0), a(INK));
        painter.text(
            pos2(c.x, c.y + 36.0),
            Align2::CENTER_CENTER,
            "Pattern fill · Expand · Crop — in real millimetres",
            FontId::proportional(15.0),
            a(MUTED),
        );

        // Progress bar.
        let bar = Rect::from_center_size(pos2(c.x, c.y + 78.0), vec2(300.0, 6.0));
        painter.rect_filled(bar, CornerRadius::same(3), a(TRACK));
        let p = self.progress(elapsed);
        if p > 0.0 {
            let fill = Rect::from_min_size(bar.min, vec2(bar.width() * p, bar.height()));
            painter.rect_filled(fill, CornerRadius::same(3), a(BLUE));
        }
        painter.text(pos2(c.x, c.y + 100.0), Align2::CENTER_CENTER, self.status(elapsed), FontId::proportional(12.5), a(MUTED));

        // Footer.
        painter.text(
            pos2(c.x, screen.bottom() - 46.0),
            Align2::CENTER_CENTER,
            "ACME ENGINEERING",
            FontId::proportional(12.0),
            a(Color32::from_rgb(0x94, 0xa3, 0xb8)),
        );
        painter.text(
            pos2(c.x, screen.bottom() - 28.0),
            Align2::CENTER_CENTER,
            concat!("Version ", env!("CARGO_PKG_VERSION")),
            FontId::proportional(11.0),
            a(Color32::from_rgb(0xa8, 0xb3, 0xc2)),
        );
    }
}

/// A cluster of seven hexagons (the app's honeycomb pattern); a highlight
/// travels around the ring while loading.
fn paint_logo(painter: &egui::Painter, center: Pos2, elapsed: f64, alpha: f32) {
    const R: f32 = 24.0; // circumradius
    let hex = |c: Pos2, r: f32| -> Vec<Pos2> {
        (0..6)
            .map(|k| {
                let ang = std::f32::consts::FRAC_PI_3 * k as f32;
                pos2(c.x + r * ang.cos(), c.y + r * ang.sin())
            })
            .collect()
    };
    let lit = ((elapsed * 7.0) as usize) % 6;
    let gap = 2.2;
    let mut cells = vec![(center, 0usize)];
    for k in 0..6 {
        let ang = std::f32::consts::FRAC_PI_6 + std::f32::consts::FRAC_PI_3 * k as f32;
        let d = R * 3f32.sqrt();
        cells.push((pos2(center.x + d * ang.cos(), center.y + d * ang.sin()), k + 1));
    }
    for (c, i) in cells {
        let base = if i % 2 == 0 { BLUE } else { ORANGE };
        let color = if i > 0 && i - 1 == lit { base.linear_multiply(0.55) } else { base };
        painter.add(Shape::convex_polygon(hex(c, R - gap), color.gamma_multiply(alpha), Stroke::NONE));
    }
}
