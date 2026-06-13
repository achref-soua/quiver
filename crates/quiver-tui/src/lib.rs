// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver retro terminal cockpit — an API client that renders live status
//! and a collection browser over a running server's REST surface.
//!
//! The Phase 1 cockpit polls `/readyz` and `/v1/collections` and shows
//! connection health, aggregate counts, and a per-collection browser in a
//! phosphor-amber theme. Richer Prometheus metrics and the constellation view of
//! the vector space land in later phases (`docs/architecture/overview.md`,
//! ADR-0014). It connects over plaintext HTTP — the common loopback case where a
//! TLS bind is not required; a TLS-aware client is a later enhancement.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use serde::Deserialize;
use tokio_stream::StreamExt;

/// Phosphor amber — the cockpit's primary colour.
const AMBER: Color = Color::Rgb(255, 176, 0);
/// A dimmer amber for secondary text and borders.
const AMBER_DIM: Color = Color::Rgb(170, 117, 0);
/// A soft red for an unreachable server.
const OFFLINE: Color = Color::Rgb(220, 90, 80);

/// How often the cockpit polls the server.
const REFRESH: Duration = Duration::from_secs(2);
/// Per-request timeout for the polling client.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// How to reach the server.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// REST base URL, e.g. `http://127.0.0.1:6333`.
    pub base_url: String,
    /// API key presented as a bearer token, if the server requires one.
    pub api_key: Option<String>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:6333".to_owned(),
            api_key: None,
        }
    }
}

/// One collection as reported by `GET /v1/collections`.
#[derive(Debug, Clone, Deserialize)]
pub struct Collection {
    /// Collection name.
    pub name: String,
    /// Vector dimensionality.
    pub dim: u32,
    /// Distance metric (`l2`, `cosine`, `dot`).
    pub metric: String,
    /// Number of live points.
    pub count: u64,
}

/// A point-in-time view of the server: readiness plus the collection list. This
/// is the data the cockpit renders, exposed so it can be polled (and tested)
/// without driving the terminal UI.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Whether `/readyz` reported the server ready.
    pub ready: bool,
    /// The collections from `/v1/collections`.
    pub collections: Vec<Collection>,
}

/// Poll the server once for a [`Snapshot`]. Readiness failures degrade to
/// `ready = false`; a failure to list collections (network error, or a `401`
/// when an API key is required) is returned as an error.
pub async fn fetch_snapshot(
    client: &reqwest::Client,
    options: &TuiOptions,
) -> anyhow::Result<Snapshot> {
    let ready = (client
        .get(format!("{}/readyz", options.base_url))
        .send()
        .await)
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    let mut request = client.get(format!("{}/v1/collections", options.base_url));
    if let Some(key) = &options.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("unauthorized — set --api-key");
    }
    let collections = response
        .error_for_status()?
        .json::<Vec<Collection>>()
        .await?;
    Ok(Snapshot { ready, collections })
}

/// Aggregate figures derived from the collection list.
#[derive(Debug, PartialEq, Eq)]
struct Aggregate {
    collections: usize,
    total_points: u64,
}

fn aggregate(collections: &[Collection]) -> Aggregate {
    Aggregate {
        collections: collections.len(),
        total_points: collections.iter().map(|c| c.count).sum(),
    }
}

/// Connection state shown in the status panel.
#[derive(Debug, Clone)]
enum ConnStatus {
    Connecting,
    Online { ready: bool },
    Offline(String),
}

struct App {
    client: reqwest::Client,
    options: TuiOptions,
    status: ConnStatus,
    collections: Vec<Collection>,
    selected: usize,
    last_refresh: Option<Instant>,
    should_quit: bool,
}

impl App {
    fn new(options: TuiOptions) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
        Ok(Self {
            client,
            options,
            status: ConnStatus::Connecting,
            collections: Vec::new(),
            selected: 0,
            last_refresh: None,
            should_quit: false,
        })
    }

    async fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        match fetch_snapshot(&self.client, &self.options).await {
            Ok(snapshot) => {
                self.collections = snapshot.collections;
                if self.selected >= self.collections.len() {
                    self.selected = self.collections.len().saturating_sub(1);
                }
                self.status = ConnStatus::Online {
                    ready: snapshot.ready,
                };
            }
            Err(err) => self.status = ConnStatus::Offline(err.to_string()),
        }
    }

    fn select_next(&mut self) {
        if !self.collections.is_empty() {
            self.selected = (self.selected + 1) % self.collections.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.collections.is_empty() {
            self.selected = (self.selected + self.collections.len() - 1) % self.collections.len();
        }
    }
}

/// Launch the cockpit against `options`, returning when the user quits.
///
/// Sets up the alternate screen and raw mode (restoring them, and on panic via
/// ratatui's hook, before returning) and renders until `q`/`Esc`.
pub async fn run(options: TuiOptions) -> anyhow::Result<()> {
    let mut app = App::new(options)?;
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app).await;
    ratatui::restore();
    result
}

async fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(REFRESH);
    app.refresh().await;

    while !app.should_quit {
        terminal.draw(|frame| ui(frame, app))?;
        tokio::select! {
            _ = ticker.tick() => app.refresh().await,
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('r') => app.refresh().await,
                    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                    KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                    _ => {}
                },
                Some(Err(err)) => return Err(err.into()),
                None => app.should_quit = true,
                _ => {}
            }
        }
    }
    Ok(())
}

fn ui(frame: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());
    frame.render_widget(header(app), rows[0]);

    let body =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)]).split(rows[1]);
    frame.render_widget(status_panel(app), body[0]);
    frame.render_widget(collections_panel(app), body[1]);

    frame.render_widget(footer(), rows[2]);
}

fn header(app: &App) -> Paragraph<'_> {
    let line = Line::from(vec![
        Span::styled(
            "QUIVER",
            Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ▸  cockpit   ", Style::new().fg(AMBER_DIM)),
        Span::styled(app.options.base_url.clone(), Style::new().fg(AMBER)),
    ]);
    Paragraph::new(line).block(Block::bordered().border_style(Style::new().fg(AMBER_DIM)))
}

fn status_panel(app: &App) -> Paragraph<'_> {
    let agg = aggregate(&app.collections);
    let (dot, label, color) = match &app.status {
        ConnStatus::Connecting => ("◌", "connecting…".to_owned(), AMBER_DIM),
        ConnStatus::Online { ready: true } => ("●", "online · ready".to_owned(), AMBER),
        ConnStatus::Online { ready: false } => ("●", "online · not ready".to_owned(), AMBER_DIM),
        ConnStatus::Offline(err) => ("●", format!("offline · {err}"), OFFLINE),
    };
    let refreshed = app
        .last_refresh
        .map(|t| format!("{}s ago", t.elapsed().as_secs()))
        .unwrap_or_else(|| "—".to_owned());

    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{dot} "), Style::new().fg(color)),
            Span::styled(label, Style::new().fg(color)),
        ]),
        Line::from(""),
        kv("collections", &agg.collections.to_string()),
        kv("total points", &agg.total_points.to_string()),
        kv("refreshed", &refreshed),
        Line::from(""),
        Line::from(Span::styled(
            "encryption-at-rest: on",
            Style::new().fg(AMBER_DIM),
        )),
    ];
    Paragraph::new(lines).block(
        Block::bordered()
            .title(" status ")
            .border_style(Style::new().fg(AMBER_DIM)),
    )
}

fn kv<'a>(key: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:<13}"), Style::new().fg(AMBER_DIM)),
        Span::styled(value.to_owned(), Style::new().fg(AMBER)),
    ])
}

fn collections_panel(app: &App) -> Paragraph<'_> {
    let lines: Vec<Line> = if app.collections.is_empty() {
        vec![Line::from(Span::styled(
            "no collections yet",
            Style::new().fg(AMBER_DIM),
        ))]
    } else {
        app.collections
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let selected = i == app.selected;
                let marker = if selected { "▸ " } else { "  " };
                let style = if selected {
                    Style::new().fg(AMBER).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(AMBER)
                };
                Line::from(Span::styled(
                    format!(
                        "{marker}{:<24} dim={:<6} metric={:<8} count={}",
                        c.name, c.dim, c.metric, c.count
                    ),
                    style,
                ))
            })
            .collect()
    };
    Paragraph::new(lines).block(
        Block::bordered()
            .title(" collections ")
            .border_style(Style::new().fg(AMBER_DIM)),
    )
}

fn footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        " [q] quit   [r] refresh   [↑/↓] select ",
        Style::new().fg(AMBER_DIM),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(name: &str, count: u64) -> Collection {
        Collection {
            name: name.to_owned(),
            dim: 4,
            metric: "l2".to_owned(),
            count,
        }
    }

    #[test]
    fn aggregate_sums_points_across_collections() {
        let cols = vec![collection("a", 10), collection("b", 32), collection("c", 0)];
        assert_eq!(
            aggregate(&cols),
            Aggregate {
                collections: 3,
                total_points: 42,
            }
        );
        assert_eq!(
            aggregate(&[]),
            Aggregate {
                collections: 0,
                total_points: 0,
            }
        );
    }

    #[test]
    fn default_options_target_loopback_rest() {
        assert_eq!(TuiOptions::default().base_url, "http://127.0.0.1:6333");
        assert!(TuiOptions::default().api_key.is_none());
    }

    #[test]
    fn selection_wraps_both_directions() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        app.collections = vec![collection("a", 1), collection("b", 2)];
        assert_eq!(app.selected, 0);
        app.select_prev(); // wraps to last
        assert_eq!(app.selected, 1);
        app.select_next(); // wraps to first
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn empty_collections_selection_is_safe() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        app.select_next();
        app.select_prev();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn collection_deserializes_from_rest_json() {
        let json = r#"[{"name":"items","dim":8,"metric":"cosine","count":5}]"#;
        let cols: Vec<Collection> = serde_json::from_str(json).unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].name, "items");
        assert_eq!(cols[0].dim, 8);
        assert_eq!(cols[0].count, 5);
    }
}
