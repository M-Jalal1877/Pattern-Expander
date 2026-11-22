//! Reading vector graphics that other programs put on the system clipboard.
//!
//! * Adobe Illustrator (and Inkscape, Figma, browsers…) offer **SVG** — as a
//!   registered `image/svg+xml`-style format and/or as plain text.
//! * CorelDRAW (and most Windows drawing programs) offer an **Enhanced Metafile**.
//!
//! Candidates are returned best first; the caller parses them in order and uses
//! the first one that yields shapes.

/// One piece of vector data found on the clipboard.
#[derive(Debug, Clone)]
pub enum ClipVector {
    /// SVG markup; `dpi` is how many unitless/px units make an inch
    /// (72 for Illustrator, which writes 1 px = 1 pt; 96 otherwise).
    Svg { data: Vec<u8>, dpi: f64 },
    /// Raw Enhanced Metafile bytes.
    Emf(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct ClipboardContents {
    pub candidates: Vec<ClipVector>,
    /// Names of all formats on the clipboard (for diagnostics).
    pub formats: Vec<String>,
}

/// SVG embedded in a text: from `<?xml` or `<svg` onwards.
pub fn svg_in_text(text: &str) -> Option<&str> {
    let start = text.find("<svg").or_else(|| text.find("<SVG"))?;
    let start = text[..start].rfind("<?xml").unwrap_or(start);
    Some(text[start..].trim_end_matches('\0'))
}

/// Decode clipboard bytes that may be UTF-8 or UTF-16LE text.
fn decode_text(bytes: &[u8]) -> String {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || (bytes.len() > 3 && bytes[1] == 0 && bytes[3] == 0);
    if utf16 {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units).trim_start_matches('\u{feff}').to_string()
    } else {
        String::from_utf8_lossy(bytes).trim_start_matches('\u{feff}').to_string()
    }
}

/// Build the candidate list from what the clipboard offers (platform independent).
fn candidates(formats: &[String], svg_blobs: Vec<Vec<u8>>, text: Option<String>, emf: Option<Vec<u8>>) -> Vec<ClipVector> {
    use crate::svg_import::{CSS_DPI, ILLUSTRATOR_DPI, is_illustrator_svg};
    let from_illustrator = formats.iter().any(|n| {
        let n = n.to_ascii_lowercase();
        n.contains("adobe") || n.contains("illustrator") || n.contains("aicb")
    });
    let dpi_for = |data: &[u8]| {
        if from_illustrator || is_illustrator_svg(data) { ILLUSTRATOR_DPI } else { CSS_DPI }
    };
    let mut out = Vec::new();
    for blob in svg_blobs {
        let text = decode_text(&blob);
        if let Some(svg) = svg_in_text(&text) {
            let data = svg.as_bytes().to_vec();
            out.push(ClipVector::Svg { dpi: dpi_for(&data), data });
        }
    }
    if let Some(svg) = text.as_deref().and_then(svg_in_text) {
        let data = svg.as_bytes().to_vec();
        out.push(ClipVector::Svg { dpi: dpi_for(&data), data });
    }
    if let Some(emf) = emf {
        out.push(ClipVector::Emf(emf));
    }
    out
}

#[cfg(windows)]
pub fn read() -> Result<ClipboardContents, String> {
    win::read()
}

#[cfg(not(windows))]
pub fn read() -> Result<ClipboardContents, String> {
    Ok(ClipboardContents::default())
}

/// Is Ctrl+V physically held right now?
///
/// egui-winit turns Ctrl+V into a text or image paste event and drops the key
/// press, so when the clipboard holds neither (e.g. only CorelDRAW's metafile)
/// the app would never hear about it. Polling the key state covers that case.
#[cfg(windows)]
pub fn paste_keys_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
    // SAFETY: plain Win32 query without pointers.
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
    down(i32::from(VK_CONTROL)) && down(i32::from(b'V'))
}

#[cfg(not(windows))]
pub fn paste_keys_down() -> bool {
    false
}

#[cfg(windows)]
mod win {
    use super::{ClipboardContents, candidates};
    use windows_sys::Win32::Graphics::Gdi::GetEnhMetaFileBits;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW, IsClipboardFormatAvailable,
        OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;
    const CF_ENHMETAFILE: u32 = 14;
    /// Refuse absurd clipboard payloads (64 MB).
    const MAX_BYTES: usize = 64 << 20;

    /// Closes the clipboard when dropped.
    struct Open;
    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: only constructed after OpenClipboard succeeded.
            unsafe { CloseClipboard() };
        }
    }

    fn open() -> Result<Open, String> {
        // Another program may hold the clipboard for a moment; retry briefly.
        for _ in 0..20 {
            // SAFETY: a null owner window is allowed.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err("The clipboard is in use by another program.".into())
    }

    fn format_name(f: u32) -> String {
        let standard = match f {
            1 => "CF_TEXT",
            2 => "CF_BITMAP",
            3 => "CF_METAFILEPICT",
            7 => "CF_OEMTEXT",
            8 => "CF_DIB",
            13 => "CF_UNICODETEXT",
            14 => "CF_ENHMETAFILE",
            15 => "CF_HDROP",
            16 => "CF_LOCALE",
            17 => "CF_DIBV5",
            _ => "",
        };
        if !standard.is_empty() {
            return standard.into();
        }
        let mut buf = [0u16; 256];
        // SAFETY: buffer and its length are valid.
        let n = unsafe { GetClipboardFormatNameW(f, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { format!("#{f}") }
    }

    /// Copy the bytes of an HGLOBAL-based clipboard format.
    fn global_bytes(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the handle is owned by the clipboard and
        // only read between GlobalLock and GlobalUnlock.
        unsafe {
            let h = GetClipboardData(format);
            if h.is_null() {
                return None;
            }
            let size = GlobalSize(h);
            if size == 0 || size > MAX_BYTES {
                return None;
            }
            let p = GlobalLock(h) as *const u8;
            if p.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(p, size).to_vec();
            GlobalUnlock(h);
            Some(bytes)
        }
    }

    fn unicode_text() -> Option<String> {
        let bytes = global_bytes(CF_UNICODETEXT)?;
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }

    fn emf_bytes() -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the metafile handle stays owned by it.
        unsafe {
            let h = GetClipboardData(CF_ENHMETAFILE);
            if h.is_null() {
                return None;
            }
            let n = GetEnhMetaFileBits(h, 0, std::ptr::null_mut()) as usize;
            if n == 0 || n > MAX_BYTES {
                return None;
            }
            let mut buf = vec![0u8; n];
            let got = GetEnhMetaFileBits(h, n as u32, buf.as_mut_ptr()) as usize;
            buf.truncate(got);
            (got > 0).then_some(buf)
        }
    }

    pub fn read() -> Result<ClipboardContents, String> {
        let _open = open()?;
        let mut formats = Vec::new();
        let mut svg_blobs = Vec::new();
        let mut f = 0;
        loop {
            // SAFETY: the clipboard is open.
            f = unsafe { EnumClipboardFormats(f) };
            if f == 0 {
                break;
            }
            let name = format_name(f);
            if name.to_ascii_lowercase().contains("svg")
                && let Some(b) = global_bytes(f)
            {
                svg_blobs.push(b);
            }
            formats.push(name);
        }
        // SAFETY: the clipboard is open.
        let text = (unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } != 0).then(unicode_text).flatten();
        let emf = (unsafe { IsClipboardFormatAvailable(CF_ENHMETAFILE) } != 0).then(emf_bytes).flatten();
        Ok(ClipboardContents { candidates: candidates(&formats, svg_blobs, text, emf), formats })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_svg_in_text_and_ranks_svg_before_emf() {
        assert!(svg_in_text("hello").is_none());
        assert_eq!(svg_in_text("junk <svg a=\"1\"/>\0"), Some("<svg a=\"1\"/>"));
        assert!(svg_in_text("<?xml version=\"1.0\"?>\n<svg/>").unwrap().starts_with("<?xml"));

        let fm = vec!["CF_UNICODETEXT".to_string(), "Adobe AI3".to_string()];
        let c = candidates(&fm, vec![], Some("<svg/>".into()), Some(vec![1, 2, 3]));
        assert!(matches!(&c[0], ClipVector::Svg { dpi, .. } if *dpi == 72.0), "Illustrator → 72 dpi");
        assert!(matches!(&c[1], ClipVector::Emf(_)));

        let c = candidates(&["image/svg+xml".into()], vec!["<svg/>".encode_utf16().flat_map(u16::to_le_bytes).collect()], None, None);
        assert!(matches!(&c[0], ClipVector::Svg { dpi, data } if *dpi == 96.0 && data == b"<svg/>"), "UTF-16 blob decoded");
    }

    /// The whole paste pipeline minus the OS clipboard: Illustrator-style SVG
    /// text (points, CSS classes, several objects) → shapes at true size with
    /// their styling, as separate objects, centred on the artboard.
    #[test]
    fn illustrator_paste_keeps_size_style_and_is_centred() {
        use crate::document::Document;
        use crate::shapes::Color;
        use crate::svg_import::{add_to_document, import_svg_at_dpi};
        let text = r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 120">
  <defs><style>.cls-1{fill:#e6007e;}.cls-2{fill:#009fe3;stroke:#1d1d1b;stroke-width:2.83465px;}</style></defs>
  <rect class="cls-1" x="10" y="10" width="113.3858" height="56.6929"/>
  <circle class="cls-2" cx="190" cy="50" r="42.5197"/>
</svg>"#;
        // Illustrator's own clipboard formats tell us its px are points.
        let formats = vec!["Adobe AI3".to_string(), "CF_UNICODETEXT".to_string()];
        let c = candidates(&formats, vec![], Some(text.into()), None);
        let ClipVector::Svg { data, dpi } = &c[0] else { panic!("expected SVG") };
        let rep = import_svg_at_dpi(data, 0.01, *dpi).unwrap();

        let mut doc = Document::default();
        let ids = add_to_document(&mut doc, rep.nodes, None);
        assert_eq!(ids.len(), 2, "separate objects, not wrapped in a group");
        doc.center_on_artboard(&ids);

        let rect = doc.get(ids[0]).unwrap().as_shape().unwrap();
        assert!((rect.xf.w - 40.0).abs() < 1e-3 && (rect.xf.h - 20.0).abs() < 1e-3, "{} × {}", rect.xf.w, rect.xf.h);
        assert_eq!(rect.style.fill, Some(Color::rgb(0xe6, 0x00, 0x7e)));
        let circle = doc.get(ids[1]).unwrap().as_shape().unwrap();
        assert!((circle.xf.w - 30.0).abs() < 0.01, "circle Ø {}", circle.xf.w);
        assert_eq!(circle.style.stroke, Some(Color::rgb(0x1d, 0x1d, 0x1b)));
        assert!((circle.style.stroke_width - 1.0).abs() < 1e-3, "stroke {} mm", circle.style.stroke_width);

        let b = doc.bounds_of_ids(&ids).unwrap();
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        assert!((cx - doc.artboard.width / 2.0).abs() < 1e-9 && (cy - doc.artboard.height / 2.0).abs() < 1e-9);
    }
}
