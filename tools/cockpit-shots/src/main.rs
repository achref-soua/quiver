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
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

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

// ── application icon ──────────────────────────────────────────────────────────
//
// Faithfully replicates the 3-D verdigris arrowhead from logo.rs (ADR-0036):
// same algorithm, same exact three-shade bevel, same open-top notch — just
// rasterised to a square PNG rather than half-block terminal cells.
//
// Colors mirror theme.rs exactly:
//   BG        #100C08  oak
//   ACCENT    #3FB6A8  verdigris base
//   ACCENT_HI #8EE9DC  lit left edge
//   ACCENT_LO #216B61  shadow right facet

fn build_arrowhead_grid() -> Vec<[Option<[u8; 3]>; 7]> {
    const VW: usize = 7;
    const SUBH: usize = 14;
    const VC: i32 = 3;
    const T: i32 = 2;

    let accent    = [0x3F_u8, 0xB6, 0xA8];
    let accent_hi = [0x8E_u8, 0xE9, 0xDC];
    let accent_lo = [0x21_u8, 0x6B, 0x61];

    let mut grid: Vec<[Option<[u8; 3]>; VW]> = vec![[None; VW]; SUBH];
    for (s, row) in grid.iter_mut().enumerate() {
        let hw = ((1.0 - s as f32 / (SUBH - 1) as f32) * 3.0).round() as i32;
        let oel = VC - hw;
        let oer = VC + hw;
        for c in oel.max(0)..=oer.min(VW as i32 - 1) {
            let from_left = c - oel;
            let from_right = oer - c;
            let color = if from_left < T && from_right < T {
                accent
            } else if from_left < T {
                if from_left == 0 { accent_hi } else { accent }
            } else if from_right < T {
                accent_lo
            } else {
                continue; // open notch
            };
            row[c as usize] = Some(color);
        }
    }
    grid
}

fn draw_icon(size: u32) -> RgbImage {
    const VW: usize = 7;
    const SUBH: usize = 14;
    let bg = [0x10_u8, 0x0C, 0x08];
    let grid = build_arrowhead_grid();

    // The arrowhead occupies the centre 80 % of the canvas (10 % padding each side).
    let padding = size as f32 * 0.10;
    let draw_w = size as f32 - 2.0 * padding;
    let draw_h = size as f32 - 2.0 * padding;
    let cell_w = draw_w / VW as f32;
    let cell_h = draw_h / SUBH as f32;

    let mut img = RgbImage::from_pixel(size, size, Rgb(bg));
    for y in 0..size {
        for x in 0..size {
            let lx = x as f32 - padding;
            let ly = y as f32 - padding;
            if lx < 0.0 || ly < 0.0 || lx >= draw_w || ly >= draw_h {
                continue;
            }
            let col = (lx / cell_w) as usize;
            let sub = (ly / cell_h) as usize;
            if col < VW && sub < SUBH {
                if let Some(color) = grid[sub][col] {
                    img.put_pixel(x, y, Rgb(color));
                }
            }
        }
    }
    img
}

fn gen_icons(out_dir: &str) {
    std::fs::create_dir_all(out_dir).expect("create icon dir");
    for size in [16u32, 32, 48, 64, 128, 256] {
        let path = format!("{out_dir}/quiver-{size}.png");
        draw_icon(size).save(&path).expect("save icon png");
        println!("wrote {path}  ({size}x{size})");
    }
}

// ── colour palette — exact 24-bit theme values ────────────────────────────────

const BRONZE: Color = Color::Rgb(205, 127, 50);     // #CD7F32  theme CHROME
const VERDIGRIS: Color = Color::Rgb(63, 182, 168);  // #3FB6A8  theme ACCENT
const DARK_GRAY: Color = Color::Rgb(90, 90, 90);
const LIGHT_GRAY: Color = Color::Rgb(180, 180, 180);
const GREEN: Color = Color::Rgb(143, 179, 57);      // #8FB339  theme OK
const WHITE: Color = Color::Rgb(230, 230, 230);

/// Banner with the V as a verdigris arrowhead, matching the TUI logo and
/// the terminal banners in demo.rs / update.rs / install scripts.
fn logo_lines() -> Vec<Line<'static>> {
    let b = Style::default().fg(BRONZE);
    let v = Style::default().fg(VERDIGRIS);
    vec![
        Line::from(vec![
            Span::styled("    ██████╗ ██╗   ██╗██╗", b),
            Span::styled("██╗   ██╗", v),
            Span::styled("███████╗██████╗ ", b),
        ]),
        Line::from(vec![
            Span::styled("   ██╔═══██╗██║   ██║██║", b),
            Span::styled("██║   ██║", v),
            Span::styled("██╔════╝██╔══██╗", b),
        ]),
        Line::from(vec![
            Span::styled("   ██║   ██║██║   ██║██║", b),
            Span::styled("╚██╗ ██╔╝", v),
            Span::styled("█████╗  ██████╔╝", b),
        ]),
        Line::from(vec![
            Span::styled("   ██║▄▄ ██║██║   ██║██║", b),
            Span::styled(" ╚████╔╝ ", v),
            Span::styled("██╔══╝  ██╔══██╗", b),
        ]),
        Line::from(vec![
            Span::styled("   ╚██████╔╝╚██████╔╝██║", b),
            Span::styled("  ╚██╔╝  ", v),
            Span::styled("███████╗██║  ██║", b),
        ]),
        Line::from(vec![
            Span::styled("    ╚══▀▀═╝  ╚═════╝ ╚═╝", b),
            Span::styled("   ╚═╝   ", v),
            Span::styled("╚══════╝╚═╝  ╚═╝", b),
        ]),
    ]
}

fn box3(top: &'static str, mid: &'static str, bot: &'static str) -> Vec<Line<'static>> {
    let g = Style::default().fg(DARK_GRAY);
    vec![
        Line::styled(top, g),
        Line::styled(mid, g),
        Line::styled(bot, g),
    ]
}

fn step_line(icon: &'static str, msg: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(icon, Style::default().fg(VERDIGRIS)),
        Span::raw("  "),
        Span::styled(msg, Style::default().fg(WHITE)),
    ])
}

fn ok_line(msg: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled("✔", Style::default().fg(GREEN)),
        Span::raw("  "),
        Span::styled(msg, Style::default().fg(WHITE)),
    ])
}

fn render_text_to_buf(lines: Vec<Line<'static>>, w: u16, h: u16) -> Buffer {
    let rect = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(rect);
    let text = Text::from(lines);
    Paragraph::new(text).render(rect, &mut buf);
    buf
}

fn render_installer() -> Buffer {
    let mut lines = logo_lines();
    lines.push(Line::styled(
        "        security-first vector database  v0.17.1",
        Style::default().fg(VERDIGRIS).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));
    lines.extend(box3(
        "  ┌──────────────────────────────────────────────┐",
        "  │  encrypted · memory-frugal · self-hostable   │",
        "  └──────────────────────────────────────────────┘",
    ));
    lines.push(Line::raw(""));
    lines.push(step_line("⟳", "Checking latest release..."));
    lines.push(step_line("⬇", "Fetching v0.17.1 for linux/x86_64..."));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Downloading", Style::default().fg(VERDIGRIS)),
        Span::styled(" quiver-linux-x86_64 ", Style::default().fg(WHITE)),
        Span::styled("[4.2 MB in 1.8s]", Style::default().fg(DARK_GRAY)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("[", Style::default().fg(DARK_GRAY)),
        Span::styled(
            "##################################################",
            Style::default().fg(GREEN),
        ),
        Span::styled("] 100%", Style::default().fg(DARK_GRAY)),
    ]));
    lines.push(step_line("🔒", "Verifying SHA-256 checksum..."));
    lines.push(ok_line("Checksum verified."));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  ┌──────────────────────────────────────────────┐",
            Style::default().fg(DARK_GRAY),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  │  ", Style::default().fg(DARK_GRAY)),
        Span::styled("✔  Quiver v0.17.1 installed!", Style::default().fg(GREEN)),
        Span::styled(
            "                │",
            Style::default().fg(DARK_GRAY),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  │  ", Style::default().fg(DARK_GRAY)),
        Span::styled(
            "   /home/user/.local/bin/quiver              │",
            Style::default().fg(LIGHT_GRAY),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "  └──────────────────────────────────────────────┘",
            Style::default().fg(DARK_GRAY),
        ),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::styled("  Next steps:", Style::default().fg(WHITE)));
    for (cmd, comment) in [
        ("quiver demo  ", "# zero-config: seed vectors + open cockpit"),
        ("quiver serve ", "# start the server (gRPC + REST on :6333)"),
        ("quiver tui   ", "# open the retro cockpit"),
        ("quiver update", "# self-update to the latest release"),
        ("quiver --help", "# all commands"),
    ] {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(cmd, Style::default().fg(BRONZE)),
            Span::raw("              "),
            Span::styled(comment, Style::default().fg(DARK_GRAY)),
        ]));
    }
    render_text_to_buf(lines, 72, 30)
}

fn render_demo_start() -> Buffer {
    let mut lines = logo_lines();
    lines.push(Line::styled(
        "        demo  ·  v0.17.1  ·  :7333",
        Style::default().fg(VERDIGRIS).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));
    lines.extend(box3(
        "  ┌─────────────────────────────────────────────────┐",
        "  │  zero config  ·  press q in the cockpit to quit │",
        "  └─────────────────────────────────────────────────┘",
    ));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("⟳", Style::default().fg(VERDIGRIS)),
        Span::raw("  Seeding 1000 vectors into 'demo'... "),
        Span::styled("done", Style::default().fg(GREEN)),
    ]));
    lines.push(ok_line("1000 vectors ready in 'demo'."));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("⟳", Style::default().fg(VERDIGRIS)),
        Span::raw("  Starting server on :7333... "),
        Span::styled("done", Style::default().fg(GREEN)),
    ]));
    lines.push(ok_line("Server ready — http://127.0.0.1:7333"));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("API key  ", Style::default().fg(DARK_GRAY)),
        Span::styled("quiver-demo", Style::default().fg(VERDIGRIS)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Python   ", Style::default().fg(DARK_GRAY)),
        Span::styled("pip install quiver-client", Style::default().fg(WHITE)),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  Opening cockpit — press q to quit.",
        Style::default().fg(GREEN),
    ));
    render_text_to_buf(lines, 72, 22)
}

fn main() {
    let font_path = std::env::var("QUIVER_SHOTS_FONT")
        .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".to_owned());
    let data = std::fs::read(&font_path).expect("read the monospace font (set QUIVER_SHOTS_FONT)");
    let font = FontVec::try_from_vec(data).expect("parse the font");
    std::fs::create_dir_all("docs/assets/cockpit").expect("create docs/assets/cockpit");
    gen_icons("docs/assets/icon");

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

    render_png(&render_installer(), &font, "docs/assets/cockpit/installer.png");
    render_png(&render_demo_start(), &font, "docs/assets/cockpit/demo-start.png");

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
