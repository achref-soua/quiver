// SPDX-License-Identifier: AGPL-3.0-only
//! The **Bronze Quiver** palette and semantic styles (ADR-0036).
//!
//! The cockpit wears the colour of a quiver: aged bronze chrome, leather borders,
//! parchment body text, an oxidised-copper verdigris accent, rust-red alerts, and
//! moss for healthy state, on near-black oak. This module is the single source of
//! truth — every widget draws from a semantic role here, never a raw colour.

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

/// Bright bronze chrome (headings, the logo wordmark).
#[must_use]
pub fn heading() -> Style {
    Style::new().fg(CHROME).add_modifier(Modifier::BOLD)
}

/// Primary chrome (titles, bright text).
#[must_use]
pub fn chrome() -> Style {
    Style::new().fg(CHROME)
}

/// Parchment body text and values.
#[must_use]
pub fn text() -> Style {
    Style::new().fg(TEXT)
}

/// Leather labels and secondary text.
#[must_use]
pub fn dim() -> Style {
    Style::new().fg(DIM)
}

/// Verdigris accent (highlights, links).
#[must_use]
pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

/// Rust-red alert (offline, errors).
#[must_use]
pub fn alert() -> Style {
    Style::new().fg(ALERT)
}

/// Moss healthy/ready.
#[must_use]
pub fn ok() -> Style {
    Style::new().fg(OK)
}

/// A faint (leather) border.
#[must_use]
pub fn border() -> Style {
    Style::new().fg(DIM)
}

/// A bright (bronze) border for the focused region.
#[must_use]
pub fn border_active() -> Style {
    Style::new().fg(CHROME)
}

/// A selected row: bold verdigris.
#[must_use]
pub fn selected() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
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
}
