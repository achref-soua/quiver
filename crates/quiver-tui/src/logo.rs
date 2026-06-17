// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver wordmark (ADR-0036): block-letter **QUIVER** in bronze whose **V is a
//! 3-D verdigris arrowhead**, open in the top-middle so it reads as a V and an
//! arrowtip at once. Rendered with half-block characters (`▀▄█`) so the arrowhead's
//! diagonals and bevel are crisp.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

// 5x7 block letters; '#' = on. The Q has a connected tail so it is not an O.
fn letter(c: char) -> [&'static str; 7] {
    match c {
        'Q' => [
            ".###.", "#...#", "#...#", "#...#", "#..#.", ".###.", "...##",
        ],
        'U' => [
            "#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
        'I' => [
            "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####",
        ],
        'E' => [
            "#####", "#....", "#....", "####.", "#....", "#....", "#####",
        ],
        'R' => [
            "####.", "#...#", "#...#", "####.", "#.##.", "#..#.", "#...#",
        ],
        _ => ["     "; 7],
    }
}

const VW: usize = 7; // arrowhead slot width
const VC: i32 = 3; // ridge column
const SUBH: usize = 14; // 7 cell-rows at 2x vertical resolution

// The 3-D open-V arrowhead as a `SUBH`-row grid of optional colours: two bevelled
// arms (lit left, shadowed right) following the outer edges, empty in the
// top-middle (the notch), converging to a solid tip.
fn arrowhead() -> Vec<[Option<Color>; VW]> {
    let mut g = vec![[None; VW]; SUBH];
    let t = 2i32; // arm thickness
    for (s, row) in g.iter_mut().enumerate() {
        let hw = ((1.0 - s as f32 / (SUBH - 1) as f32) * 3.0).round() as i32; // 3..0
        let oel = VC - hw;
        let oer = VC + hw;
        for c in oel.max(0)..=oer.min(VW as i32 - 1) {
            let from_left = c - oel;
            let from_right = oer - c;
            let color = if from_left < t && from_right < t {
                theme::ACCENT // arms meet → the tip
            } else if from_left < t {
                if from_left == 0 {
                    theme::ACCENT_HI // bright lit edge
                } else {
                    theme::ACCENT
                }
            } else if from_right < t {
                theme::ACCENT_LO // shadow facet
            } else {
                continue; // the open notch
            };
            row[c as usize] = Some(color);
        }
    }
    g
}

// Build the full wordmark as a `SUBH`-row grid of optional cell colours.
fn banner_grid() -> Vec<Vec<Option<Color>>> {
    let mut grid: Vec<Vec<Option<Color>>> = vec![Vec::new(); SUBH];
    let gap = 2usize;
    let push_letter = |grid: &mut Vec<Vec<Option<Color>>>, ch: char| {
        let rows = letter(ch);
        for (s, line) in grid.iter_mut().enumerate() {
            for px in rows[s / 2].chars() {
                line.push((px == '#').then_some(theme::CHROME));
            }
        }
    };
    let push_arrow = |grid: &mut Vec<Vec<Option<Color>>>| {
        let head = arrowhead();
        for (s, line) in grid.iter_mut().enumerate() {
            for cell in head[s] {
                line.push(cell);
            }
        }
    };
    let push_gap = |grid: &mut Vec<Vec<Option<Color>>>, n: usize| {
        for line in grid.iter_mut() {
            for _ in 0..n {
                line.push(None);
            }
        }
    };
    push_letter(&mut grid, 'Q');
    push_gap(&mut grid, gap);
    push_letter(&mut grid, 'U');
    push_gap(&mut grid, gap);
    push_letter(&mut grid, 'I');
    push_gap(&mut grid, gap);
    push_arrow(&mut grid);
    push_gap(&mut grid, gap);
    push_letter(&mut grid, 'E');
    push_gap(&mut grid, gap);
    push_letter(&mut grid, 'R');
    grid
}

/// The full 7-row logo banner, collapsing each pair of subrows into half-block
/// cells (`█▀▄` and space) with the right foreground/background colours.
#[must_use]
pub fn banner() -> Vec<Line<'static>> {
    let grid = banner_grid();
    let cols = grid[0].len();
    let mut lines = Vec::with_capacity(SUBH / 2);
    for k in 0..SUBH / 2 {
        let top = &grid[2 * k];
        let bot = &grid[2 * k + 1];
        // (char, style) per cell.
        let cells: Vec<(char, Style)> = (0..cols)
            .map(|c| match (top[c], bot[c]) {
                (Some(t), Some(b)) if t == b => ('█', Style::new().fg(t)),
                (Some(t), Some(b)) => ('▀', Style::new().fg(t).bg(b)),
                (Some(t), None) => ('▀', Style::new().fg(t)),
                (None, Some(b)) => ('▄', Style::new().fg(b)),
                (None, None) => (' ', Style::new()),
            })
            .collect();
        // Merge runs of identical style into spans.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::new();
        for (ch, style) in cells {
            if !run.is_empty() && style == run_style {
                run.push(ch);
            } else {
                if !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                run.push(ch);
                run_style = style;
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A compact one-line wordmark for tight headers: bold bronze `QUIVER` with the V
/// drawn as a small verdigris arrowhead, plus a verdigris fletched-arrow flourish.
#[must_use]
pub fn compact() -> Line<'static> {
    let chrome = Style::new().fg(theme::CHROME).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("QUI", chrome),
        Span::styled(
            "▼",
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("ER", chrome),
        Span::styled("  »──►", Style::new().fg(theme::ACCENT)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_is_seven_rows_of_blocks() {
        let lines = banner();
        assert_eq!(lines.len(), 7, "7 cell-rows tall");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains('█') || text.contains('▀') || text.contains('▄'),
            "rendered with block characters"
        );
    }

    #[test]
    fn arrowhead_uses_the_verdigris_bevel_and_is_open_at_the_top() {
        let head = arrowhead();
        // The very top row has the two shoulders but an empty middle (the notch).
        assert!(
            head[0][0].is_some() && head[0][VW - 1].is_some(),
            "shoulders"
        );
        assert!(head[0][VC as usize].is_none(), "open top-middle notch");
        // The tip converges to a single filled cell at the centre.
        let last = &head[SUBH - 1];
        assert!(last[VC as usize].is_some(), "solid tip");
        // All three bevel shades appear somewhere.
        let colors: Vec<Color> = head.iter().flatten().flatten().copied().collect();
        assert!(colors.contains(&theme::ACCENT_HI), "lit edge");
        assert!(colors.contains(&theme::ACCENT), "base");
        assert!(colors.contains(&theme::ACCENT_LO), "shadow");
    }

    #[test]
    fn banner_spans_carry_bronze_letters_and_a_verdigris_arrow() {
        let lines = banner();
        let mut saw_bronze = false;
        let mut saw_verdigris = false;
        for line in &lines {
            for span in &line.spans {
                if span.style.fg == Some(theme::CHROME) {
                    saw_bronze = true;
                }
                if matches!(
                    span.style.fg,
                    Some(theme::ACCENT) | Some(theme::ACCENT_HI) | Some(theme::ACCENT_LO)
                ) {
                    saw_verdigris = true;
                }
            }
        }
        assert!(saw_bronze, "bronze letters present");
        assert!(saw_verdigris, "verdigris arrowhead present");
    }

    #[test]
    fn compact_keeps_the_word_and_an_arrow() {
        let line = compact();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("QUI") && text.contains("ER"));
        assert!(text.contains('▼') || text.contains('►'));
    }
}
