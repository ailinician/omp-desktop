//! The pairing QR code as an SVG string.

use qrcode::render::svg;
use qrcode::QrCode;

pub fn svg(text: &str) -> Result<String, String> {
    let code = QrCode::new(text.as_bytes()).map_err(|error| format!("render QR code: {error}"))?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::svg;

    #[test]
    fn renders_an_svg_document() {
        let out = svg("https://192.168.1.2:7470/#pair=abc").unwrap();
        assert!(out.contains("<svg"), "{out}");
        assert!(out.contains("</svg>"));
    }
}
