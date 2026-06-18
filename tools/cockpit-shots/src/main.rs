// SPDX-License-Identifier: AGPL-3.0-only
//! Dev-only: render the Quiver cockpit screens to PNG screenshots (ADR-0036).
//!
//! Each screen is rendered by `quiver-tui` to a real ratatui `Buffer` with demo
//! data, then rasterised cell-by-cell here (background rect + the cell's glyph in
//! its foreground colour) with a monospace font. Run with `just tui-shots`; the
//! PNGs land in `docs/assets/cockpit/` and are embedded in the README and docs.
//!
//! The font defaults to DejaVu Sans Mono (which carries the box-drawing, block, and
//! braille glyphs the cockpit uses); override with `QUIVER_SHOTS_FONT`.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgb, RgbImage};
use quiver_tui::{
    Activity, Dashboard, Severity, render_constellation_demo, render_dashboard, render_logo,
};
use ratatui::buffer::Buffer;
use ratatui::style::Color;

const OAK: Rgb<u8> = Rgb([0x10, 0x0C, 0x08]);
const PARCHMENT: Rgb<u8> = Rgb([0xE8, 0xD8, 0xB0]);

fn to_rgb(c: Color, fallback: Rgb<u8>) -> Rgb<u8> {
    match c {
        Color::Rgb(r, g, b) => Rgb([r, g, b]),
        _ => fallback,
    }
}

fn blend(bg: [u8; 3], fg: [u8; 3], a: f32) -> [u8; 3] {
    let a = a.clamp(0.0, 1.0);
    [0, 1, 2].map(|i| (bg[i] as f32 * (1.0 - a) + fg[i] as f32 * a).round() as u8)
}

// Rasterise a ratatui buffer to a PNG: each cell is a bg rect with its glyph drawn
// in the foreground colour, on a monospace grid.
fn render_png(buf: &Buffer, font: &FontVec, out: &str) {
    let area = buf.area;
    let scale = PxScale::from(30.0);
    let scaled = font.as_scaled(scale);
    let cw = scaled.h_advance(font.glyph_id(' ')).ceil() as u32;
    let ch = scale.y.ceil() as u32;
    let ascent = scaled.ascent();
    let (w, h) = (u32::from(area.width) * cw, u32::from(area.height) * ch);
    let mut img = RgbImage::from_pixel(w, h, OAK);

    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            let bg = to_rgb(cell.bg, OAK);
            let fg = to_rgb(cell.fg, PARCHMENT);
            let (cx0, cy0) = (u32::from(x) * cw, u32::from(y) * ch);
            for py in cy0..cy0 + ch {
                for px in cx0..cx0 + cw {
                    img.put_pixel(px, py, bg);
                }
            }
            let chr = cell.symbol().chars().next().unwrap_or(' ');
            if chr == ' ' {
                continue;
            }
            let glyph = font
                .glyph_id(chr)
                .with_scale_and_position(scale, point(cx0 as f32, cy0 as f32 + ascent));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let base = img.get_pixel(px as u32, py as u32).0;
                        img.put_pixel(px as u32, py as u32, Rgb(blend(base, fg.0, cov)));
                    }
                });
            }
        }
    }
    img.save(out).expect("save png");
    println!("wrote {out}  ({}x{} cells)", area.width, area.height);
}

fn main() {
    let font_path = std::env::var("QUIVER_SHOTS_FONT")
        .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".to_owned());
    let data = std::fs::read(&font_path).expect("read the monospace font (set QUIVER_SHOTS_FONT)");
    let font = FontVec::try_from_vec(data).expect("parse the font");
    std::fs::create_dir_all("docs/assets/cockpit").expect("create docs/assets/cockpit");

    render_png(&render_logo(58, 11), &font, "docs/assets/cockpit/logo.png");
    render_png(
        &render_dashboard(120, 36, &Dashboard::demo()),
        &font,
        "docs/assets/cockpit/dashboard.png",
    );
    render_png(
        &render_constellation_demo(120, 36),
        &font,
        "docs/assets/cockpit/constellation.png",
    );

    let mut offline = Dashboard::demo();
    offline.collections.clear();
    offline.selected = 0;
    offline.ready = false;
    offline.offline = Some("connection refused (os error 111)".to_owned());
    offline.activity.push(Activity {
        ts: "+8s".to_owned(),
        severity: Severity::Error,
        message: "offline · connection refused".to_owned(),
    });
    render_png(
        &render_dashboard(120, 36, &offline),
        &font,
        "docs/assets/cockpit/dashboard-offline.png",
    );
}
