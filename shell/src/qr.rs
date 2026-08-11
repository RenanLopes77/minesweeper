//! Renders the signalling link as a QR code, so the joiner can point a phone
//! at the host's screen instead of moving a URL between devices.
//!
//! Drawn to a canvas rather than built as SVG: it avoids `innerHTML`, and it
//! lets the crate come in with `default-features = false` (no `image`, no
//! `svg`). Only the host→joiner direction benefits — the reply still has to
//! travel back as text, because the host has no camera pointed at the phone.

use qrcode::{Color, QrCode};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::{CanvasRenderingContext2d as Ctx, HtmlCanvasElement};

/// A QR code needs a light margin of at least four modules or scanners will
/// not find its edges. The page is dark, so this is drawn explicitly.
const QUIET: u32 = 4;

/// Backing-store size to aim for. The canvas is displayed smaller via CSS;
/// rendering above display size keeps the modules crisp when the browser
/// scales it down.
const TARGET: u32 = 640;

/// Returns the module count, which is worth logging — a link near the format's
/// limit produces a dense code that phones struggle to read.
pub fn render(canvas: &HtmlCanvasElement, text: &str) -> Result<u32, JsValue> {
    let code =
        QrCode::new(text.as_bytes()).map_err(|e| JsValue::from_str(&format!("qr failed: {e}")))?;
    let modules = code.width() as u32;
    let total = modules + QUIET * 2;
    let scale = (TARGET / total).max(1);
    let size = total * scale;

    canvas.set_width(size);
    canvas.set_height(size);
    let ctx: Ctx = canvas
        .get_context("2d")?
        .ok_or("no 2d context")?
        .dyn_into()?;

    ctx.set_fill_style_str("#ffffff");
    ctx.fill_rect(0.0, 0.0, size as f64, size as f64);
    ctx.set_fill_style_str("#000000");

    for (i, c) in code.to_colors().iter().enumerate() {
        if *c == Color::Dark {
            let i = i as u32;
            let x = (i % modules + QUIET) * scale;
            let y = (i / modules + QUIET) * scale;
            ctx.fill_rect(x as f64, y as f64, scale as f64, scale as f64);
        }
    }
    Ok(modules)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder is the part that can refuse; canvas drawing needs a browser.
    /// This pins down that a realistically sized link still encodes, and how
    /// big it gets.
    #[test]
    fn a_full_size_link_still_encodes() {
        let link = format!(
            "https://example.github.io/minesweeper/#o={}",
            "A".repeat(1330)
        );
        let code = QrCode::new(link.as_bytes()).expect("1370-char link must fit in a QR");
        // Version 40 is the largest QR there is: 177 modules.
        assert!(code.width() <= 177, "width {}", code.width());
    }

    #[test]
    fn oversized_input_is_an_error_not_a_panic() {
        assert!(QrCode::new(vec![b'A'; 8000]).is_err());
    }
}
