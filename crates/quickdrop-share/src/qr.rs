//! QR code rendering for share URLs.
//!
//! Two renderers from one source URL:
//! - [`svg`]: a self-contained `<svg>` string the desktop UI embeds
//!   directly (no image encoder, no base64, scales crisply).
//! - [`terminal`]: a unicode block rendering for the standalone CLI /
//!   logs, so the feature can be tested without any UI at all.

use qrcode::render::{svg, unicode};
use qrcode::{EcLevel, QrCode};

use crate::error::{Error, Result};

/// Render `data` as a standalone SVG document string.
pub fn svg(data: &str) -> Result<String> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .map_err(|e| Error::Qr(e.to_string()))?;
    let image = code
        .render::<svg::Color<'_>>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .dark_color(svg::Color("#0f172a"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(image)
}

/// Render `data` as unicode half-blocks for a terminal.
pub fn terminal(data: &str) -> Result<String> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .map_err(|e| Error::Qr(e.to_string()))?;
    let image = code
        .render::<unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_contains_svg_tag() {
        let s = svg("http://192.168.1.5:8080/share/abc").unwrap();
        assert!(s.contains("<svg"));
        assert!(s.contains("</svg>"));
    }

    #[test]
    fn terminal_is_non_empty() {
        let t = terminal("http://192.168.1.5:8080/share/abc").unwrap();
        assert!(!t.trim().is_empty());
    }
}
