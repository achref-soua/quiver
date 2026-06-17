// SPDX-License-Identifier: AGPL-3.0-only
//! The retro decoration vocabulary (ADR-0036): small, minimalist widgets that make
//! the cockpit's data legible at a glance — framed panels, a database icon, bar and
//! sparkline graphs, log lines, relationship trees, status badges, and arrow
//! dividers. Every piece draws only from [`crate::theme`].

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::theme;

/// A titled retro panel: a rounded leather border with a bold bronze title chip.
#[must_use]
pub fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(format!(" {title} "), theme::heading()))
}

/// The same panel, marked as the focused region (bright bronze border).
#[must_use]
pub fn panel_active(title: &str) -> Block<'static> {
    panel(title).border_style(theme::border_active())
}

/// A three-row database "drum" icon marking a collection / store.
#[must_use]
pub fn db_icon() -> [&'static str; 3] {
    ["╭─────╮", "╞═════╡", "╰─────╯"]
}

/// A compact single-cell database glyph.
pub const DB_GLYPH: &str = "⛁";

/// A bracketed status badge, e.g. `[ ONLINE ]`, in the given style.
#[must_use]
pub fn badge(label: &str, style: Style) -> Span<'static> {
    Span::styled(format!("[ {label} ]"), style)
}

/// A horizontal bar of exactly `width` cells for `value` against `max`, using
/// eighth-block characters for sub-cell precision (`█▉▊▋▌▍▎▏`).
#[must_use]
pub fn bar(value: u64, max: u64, width: usize) -> String {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    if width == 0 {
        return String::new();
    }
    let frac = if max == 0 {
        0.0
    } else {
        (value as f64 / max as f64).clamp(0.0, 1.0)
    };
    let eighths = (frac * (width * 8) as f64).round() as usize;
    let full = (eighths / 8).min(width);
    let mut s = "█".repeat(full);
    let mut len = full;
    if len < width {
        s.push(PARTIAL[eighths % 8]);
        len += 1;
    }
    while len < width {
        s.push(' ');
        len += 1;
    }
    s
}

/// A unicode sparkline (`▁▂▃▄▅▆▇█`) of a series, scaled between its min and max.
#[must_use]
pub fn sparkline(values: &[u64]) -> String {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let max = values.iter().copied().max().unwrap_or(0);
    let min = values.iter().copied().min().unwrap_or(0);
    let span = (max - min).max(1);
    values
        .iter()
        .map(|&v| {
            let idx = (((v - min) as f64 / span as f64) * 7.0).round() as usize;
            TICKS[idx.min(7)]
        })
        .collect()
}

/// The severity of a log/activity line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Routine information.
    Info,
    /// A warning.
    Warn,
    /// An error.
    Error,
}

/// A timestamped activity line with a severity glyph (`●` info, `▲` warn, `✕` error)
/// in the role colour, the timestamp dim, the message in body text.
#[must_use]
pub fn log_line(ts: &str, sev: Severity, msg: &str) -> Line<'static> {
    let (glyph, style) = match sev {
        Severity::Info => ("●", theme::accent()),
        Severity::Warn => ("▲", theme::chrome()),
        Severity::Error => ("✕", theme::alert()),
    };
    Line::from(vec![
        Span::styled(format!("{ts} "), theme::dim()),
        Span::styled(format!("{glyph} "), style),
        Span::styled(msg.to_owned(), theme::text()),
    ])
}

/// A relationship tree from `(depth, label)` rows, drawn with `├─►`/`╰─►`
/// connectors (the last child at a depth gets the rounded corner). Depth `0` is a
/// bronze root; deeper rows are body text.
#[must_use]
pub fn tree(rows: &[(usize, String)]) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len());
    for (i, (depth, label)) in rows.iter().enumerate() {
        if *depth == 0 {
            out.push(Line::from(vec![
                Span::styled(format!("{DB_GLYPH} "), theme::accent()),
                Span::styled(label.clone(), theme::heading()),
            ]));
            continue;
        }
        // The last child at this depth: the subtree ends next (a shallower row) or
        // the list ends. A same-depth next row is a sibling, so not the last.
        let is_last = rows.get(i + 1).map(|(d, _)| d < depth).unwrap_or(true);
        let indent = "   ".repeat(depth - 1);
        let conn = if is_last { "╰─► " } else { "├─► " };
        out.push(Line::from(vec![
            Span::styled(format!("{indent}{conn}"), theme::dim()),
            Span::styled(label.clone(), theme::text()),
        ]));
    }
    out
}

/// A fletched-arrow section divider `»────►` of the given width.
#[must_use]
pub fn divider(width: usize) -> Line<'static> {
    if width < 2 {
        return Line::from(Span::styled("─".repeat(width), theme::dim()));
    }
    Line::from(vec![
        Span::styled("»", theme::accent()),
        Span::styled("─".repeat(width - 2), theme::dim()),
        Span::styled("►", theme::accent()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn bar_is_exactly_width_chars_and_tracks_the_fraction() {
        assert_eq!(bar(0, 10, 8).chars().count(), 8);
        assert_eq!(bar(10, 10, 8).chars().count(), 8);
        assert_eq!(bar(5, 10, 8).chars().count(), 8);
        assert_eq!(bar(10, 10, 8), "████████", "full");
        assert_eq!(bar(0, 10, 8).trim(), "", "empty");
        assert!(bar(5, 10, 8).starts_with("████"), "half full ≈ 4 cells");
        // A zero max never divides by zero and renders empty.
        assert_eq!(bar(3, 0, 4).chars().count(), 4);
        assert_eq!(bar(3, 0, 4).trim(), "");
        assert_eq!(bar(1, 2, 0), "");
    }

    #[test]
    fn sparkline_maps_min_to_max_across_the_ticks() {
        assert_eq!(sparkline(&[]), "");
        let s = sparkline(&[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(s.chars().count(), 8);
        assert!(s.starts_with('▁'), "min → lowest tick");
        assert!(s.ends_with('█'), "max → highest tick");
        // A flat series collapses to the lowest tick (no divide-by-zero).
        assert_eq!(sparkline(&[5, 5, 5]), "▁▁▁");
    }

    #[test]
    fn log_line_carries_the_severity_glyph_and_colour() {
        let info = log_line("12:00", Severity::Info, "ready");
        assert!(text_of(&info).contains('●') && text_of(&info).contains("ready"));
        let warn = log_line("12:01", Severity::Warn, "slow");
        assert!(text_of(&warn).contains('▲'));
        let err = log_line("12:02", Severity::Error, "boom");
        assert!(text_of(&err).contains('✕'));
        // The glyph span uses the severity's role colour.
        assert_eq!(err.spans[1].style.fg, Some(theme::ALERT));
        assert_eq!(info.spans[1].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn tree_draws_connectors_and_marks_the_last_child() {
        let rows = vec![
            (0, "vault".to_owned()),
            (1, "index: hnsw".to_owned()),
            (1, "metric: cosine".to_owned()),
        ];
        let lines = tree(&rows);
        assert_eq!(lines.len(), 3);
        assert!(text_of(&lines[0]).contains("vault"));
        assert!(text_of(&lines[1]).contains("├─►"), "non-last child");
        assert!(text_of(&lines[2]).contains("╰─►"), "last child");
    }

    #[test]
    fn divider_is_a_fletched_arrow() {
        let d = divider(10);
        let t = text_of(&d);
        assert!(t.starts_with('»') && t.ends_with('►'));
        assert_eq!(t.chars().count(), 10);
    }

    #[test]
    fn badge_and_db_icon_render() {
        let b = badge("ONLINE", theme::ok());
        assert_eq!(b.content.as_ref(), "[ ONLINE ]");
        assert_eq!(b.style.fg, Some(theme::OK));
        assert_eq!(db_icon().len(), 3);
    }

    #[test]
    fn panels_use_rounded_leather_and_bright_borders() {
        // Render a panel into a buffer and assert the rounded corner + title appear.
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        panel("status").render(area, &mut buf);
        let top: String = (0..20).map(|x| buf[(x, 0)].symbol().to_owned()).collect();
        assert!(top.contains('╭'), "rounded corner");
        assert!(top.contains("status"), "title chip");
    }
}
