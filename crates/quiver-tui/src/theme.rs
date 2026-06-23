// SPDX-License-Identifier: AGPL-3.0-only
//! The **Bronze Quiver** palette and semantic styles (ADR-0036).
//!
//! The cockpit wears the colour of a quiver: aged bronze chrome, leather borders,
//! parchment body text, an oxidised-copper verdigris accent, rust-red alerts, and
//! moss for healthy state, on near-black oak. This module is the single source of
//! truth — every widget draws from a semantic role here, never a raw colour.

use std::cell::Cell;

use ratatui::style::{Color, Modifier, Style};

/// Near-black oak — the cockpit background.
pub const BG: Color = Color::Rgb(0x10, 0x0C, 0x08);
/// Aged bronze — logo, headings, bright chrome and active borders.
pub const CHROME: Color = Color::Rgb(0xCD, 0x7F, 0x32);
/// Parchment — body text and values.
pub const TEXT: Color = Color::Rgb(0xE8, 0xD8, 0xB0);
/// Leather — labels, faint borders, secondary text.
pub const DIM: Color = Color::Rgb(0x8A, 0x5A, 0x2B);
/// Verdigris — selection, highlights, links, and the logo arrowhead.
pub const ACCENT: Color = Color::Rgb(0x3F, 0xB6, 0xA8);
/// Bright verdigris — the lit facet / ridge of the 3-D arrowhead.
pub const ACCENT_HI: Color = Color::Rgb(0x8E, 0xE9, 0xDC);
/// Dark verdigris — the shadowed facet of the 3-D arrowhead.
pub const ACCENT_LO: Color = Color::Rgb(0x21, 0x6B, 0x61);
/// Rust-red — offline and errors.
pub const ALERT: Color = Color::Rgb(0xD2, 0x55, 0x2F);
/// Moss — ready and healthy.
pub const OK: Color = Color::Rgb(0x8F, 0xB3, 0x39);

/// One colour per semantic role. The cockpit ships two palettes and toggles
/// between them live (ADR-0060); the style functions below read the active one.
#[derive(Clone, Copy)]
struct Palette {
    chrome: Color,
    text: Color,
    dim: Color,
    accent: Color,
    alert: Color,
    ok: Color,
}

/// The warm **Bronze Quiver** brand (ADR-0036) — the default. Its fields are the
/// brand consts above, so the default thread renders byte-for-byte as before.
const BRONZE: Palette = Palette {
    chrome: CHROME,
    text: TEXT,
    dim: DIM,
    accent: ACCENT,
    alert: ALERT,
    ok: OK,
};

/// A cool **Slate** alternate — steel chrome, ice body, slate labels, cyan
/// accent — for high-contrast / cool-light preference. Background and the raw
/// arrowhead facets stay oak+verdigris; this recolours the chrome/text surface.
const SLATE: Palette = Palette {
    chrome: Color::Rgb(0x9F, 0xB3, 0xC8),
    text: Color::Rgb(0xE6, 0xED, 0xF3),
    dim: Color::Rgb(0x5B, 0x6B, 0x7E),
    accent: Color::Rgb(0x6C, 0xC6, 0xE6),
    alert: Color::Rgb(0xE3, 0x6A, 0x6A),
    ok: Color::Rgb(0x7E, 0xC8, 0x99),
};

/// Which palette is active. A plain discriminant — selection is a `match`, not a
/// pointer-identity comparison, so the palettes stay `const` with no address
/// footgun (ADR-0060).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Theme {
    Bronze,
    Slate,
}

impl Theme {
    fn palette(self) -> &'static Palette {
        match self {
            Theme::Bronze => &BRONZE,
            Theme::Slate => &SLATE,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Bronze => "bronze",
            Theme::Slate => "slate",
        }
    }

    fn next(self) -> Theme {
        match self {
            Theme::Bronze => Theme::Slate,
            Theme::Slate => Theme::Bronze,
        }
    }
}

thread_local! {
    // ponytail: thread-local active theme — the cockpit renders on one thread,
    // so this avoids threading a palette param through ~30 render call sites.
    // Thread a param instead if the renderer ever goes multi-threaded.
    static ACTIVE: Cell<Theme> = const { Cell::new(Theme::Bronze) };
}

fn active() -> &'static Palette {
    ACTIVE.with(Cell::get).palette()
}

/// Toggle the active palette between Bronze and Slate (the `Ctrl-t` action).
pub fn cycle() {
    ACTIVE.with(|a| a.set(a.get().next()));
}

/// The active palette's name, for the status line.
#[must_use]
pub fn name() -> &'static str {
    ACTIVE.with(Cell::get).label()
}

/// Bright chrome headings, the logo wordmark.
#[must_use]
pub fn heading() -> Style {
    Style::new()
        .fg(active().chrome)
        .add_modifier(Modifier::BOLD)
}

/// Primary chrome (titles, bright text).
#[must_use]
pub fn chrome() -> Style {
    Style::new().fg(active().chrome)
}

/// Body text and values.
#[must_use]
pub fn text() -> Style {
    Style::new().fg(active().text)
}

/// Labels and secondary text.
#[must_use]
pub fn dim() -> Style {
    Style::new().fg(active().dim)
}

/// Accent (highlights, links).
#[must_use]
pub fn accent() -> Style {
    Style::new().fg(active().accent)
}

/// Alert (offline, errors).
#[must_use]
pub fn alert() -> Style {
    Style::new().fg(active().alert)
}

/// Healthy/ready.
#[must_use]
pub fn ok() -> Style {
    Style::new().fg(active().ok)
}

/// A faint border.
#[must_use]
pub fn border() -> Style {
    Style::new().fg(active().dim)
}

/// A bright border for the focused region.
#[must_use]
pub fn border_active() -> Style {
    Style::new().fg(active().chrome)
}

/// A selected row: bold accent.
#[must_use]
pub fn selected() -> Style {
    Style::new()
        .fg(active().accent)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_the_bronze_quiver_brand() {
        assert_eq!(CHROME, Color::Rgb(0xCD, 0x7F, 0x32), "bronze");
        assert_eq!(ACCENT, Color::Rgb(0x3F, 0xB6, 0xA8), "verdigris");
        assert_eq!(TEXT, Color::Rgb(0xE8, 0xD8, 0xB0), "parchment");
        assert_eq!(BG, Color::Rgb(0x10, 0x0C, 0x08), "oak-black");
    }

    #[test]
    fn semantic_styles_carry_their_role_colour() {
        assert_eq!(heading().fg, Some(CHROME));
        assert!(heading().add_modifier.contains(Modifier::BOLD));
        assert_eq!(text().fg, Some(TEXT));
        assert_eq!(dim().fg, Some(DIM));
        assert_eq!(accent().fg, Some(ACCENT));
        assert_eq!(alert().fg, Some(ALERT));
        assert_eq!(ok().fg, Some(OK));
        assert_eq!(border().fg, Some(DIM));
        assert_eq!(border_active().fg, Some(CHROME));
        assert_eq!(selected().fg, Some(ACCENT));
        assert!(selected().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cycle_swaps_the_palette_and_returns_to_bronze() {
        // Default is Bronze (this test's own thread-local).
        assert_eq!(name(), "bronze");
        assert_eq!(accent().fg, Some(ACCENT));
        cycle();
        assert_eq!(name(), "slate");
        assert_ne!(accent().fg, Some(ACCENT), "the accent recolours");
        assert_eq!(accent().fg, Some(SLATE.accent));
        cycle();
        assert_eq!(name(), "bronze", "cycles back");
        assert_eq!(accent().fg, Some(ACCENT));
    }
}
