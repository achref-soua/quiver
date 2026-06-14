// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver retro terminal cockpit — an API client that renders live status
//! and a collection browser over a running server's REST surface.
//!
//! The cockpit polls `/readyz` and `/v1/collections` and shows connection
//! health, aggregate counts, and a per-collection browser in a phosphor-amber
//! theme. Pressing `v`/`enter` on a collection opens the **constellation view**:
//! a 2-D random-projection scatter of its vector space (fetched via
//! `POST .../query` with vectors), with the query's nearest neighbour
//! highlighted and a cursor that can re-query around any point. Richer
//! Prometheus metrics land in a later phase (`docs/architecture/overview.md`,
//! ADR-0014). It connects over plaintext HTTP — the common loopback case where a
//! TLS bind is not required; a TLS-aware client is a later enhancement.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
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
/// How many points the constellation view samples per query.
const CONSTELLATION_K: usize = 256;
/// Selected-point colour in the constellation (bright, against amber points).
const SELECT: Color = Color::Rgb(255, 255, 255);

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

/// Fixed seed for the random-projection axes, so the constellation's layout is
/// stable across queries and runs (only the plotted point set changes).
const PROJECTION_SEED: u64 = 0x5151_1abe_1abe_5151;

/// A collection point projected to 2-D for the constellation scatter.
#[derive(Debug, Clone, PartialEq)]
pub struct StarPoint {
    /// The point's external id.
    pub id: String,
    /// Normalized x in `[0, 1]`.
    pub x: f64,
    /// Normalized y in `[0, 1]`.
    pub y: f64,
    /// Rank by distance to the query — `0` is the nearest neighbour.
    pub rank: usize,
}

/// A 2-D projection of a collection's vectors around a query, for the cockpit's
/// constellation view: the projected [`StarPoint`]s for rendering and the raw
/// vectors (parallel) so a point can become the next query.
#[derive(Debug, Clone)]
pub struct Constellation {
    /// The projected points, in distance order (nearest first).
    pub points: Vec<StarPoint>,
    /// The raw vectors, parallel to `points`, for re-querying around a point.
    pub vectors: Vec<Vec<f32>>,
}

// One search hit as returned by `POST /v1/collections/{name}/query`.
#[derive(Deserialize)]
struct MatchHit {
    id: String,
    #[serde(default)]
    vector: Option<Vec<f32>>,
}

#[derive(Deserialize)]
struct SearchHits {
    matches: Vec<MatchHit>,
}

/// Query `collection` for up to `k` points (with their vectors) around `query`,
/// and project them to 2-D for the constellation view. With `query = None` a
/// zero vector seeds it, returning the points nearest the origin. Hits come back
/// in distance order, so `points[0]` is the query's nearest neighbour.
pub async fn fetch_constellation(
    client: &reqwest::Client,
    options: &TuiOptions,
    collection: &str,
    dim: usize,
    query: Option<Vec<f32>>,
    k: usize,
) -> anyhow::Result<Constellation> {
    let vector = query.unwrap_or_else(|| vec![0.0; dim]);
    let mut request = client
        .post(format!(
            "{}/v1/collections/{collection}/query",
            options.base_url
        ))
        .json(&serde_json::json!({
            "vector": vector,
            "k": k,
            "with_vector": true,
            "with_payload": false,
        }));
    if let Some(key) = &options.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("unauthorized — set --api-key");
    }
    let hits = response.error_for_status()?.json::<SearchHits>().await?;
    let vectors: Vec<Vec<f32>> = hits
        .matches
        .iter()
        .filter_map(|m| m.vector.clone())
        .collect();
    let ids: Vec<String> = hits
        .matches
        .into_iter()
        .filter(|m| m.vector.is_some())
        .map(|m| m.id)
        .collect();
    let points = project_constellation(&ids, &vectors, dim);
    Ok(Constellation { points, vectors })
}

/// Project `vectors` (with their `ids`, in distance order) to 2-D via a fixed
/// seeded random projection, normalized to the unit square. Two stable random
/// axes keep the layout consistent; degenerate spreads collapse to the centre.
fn project_constellation(ids: &[String], vectors: &[Vec<f32>], dim: usize) -> Vec<StarPoint> {
    if ids.is_empty() || dim == 0 {
        return Vec::new();
    }
    let axis_x = projection_axis(PROJECTION_SEED, dim);
    let axis_y = projection_axis(PROJECTION_SEED ^ 0x9e37_79b9_7f4a_7c15, dim);
    let raw_x: Vec<f64> = vectors.iter().map(|v| dot(v, &axis_x)).collect();
    let raw_y: Vec<f64> = vectors.iter().map(|v| dot(v, &axis_y)).collect();
    let norm_x = normalize_axis(&raw_x);
    let norm_y = normalize_axis(&raw_y);
    ids.iter()
        .enumerate()
        .map(|(rank, id)| StarPoint {
            id: id.clone(),
            x: norm_x[rank],
            y: norm_y[rank],
            rank,
        })
        .collect()
}

// A `dim`-length random projection axis with components in `[-1, 1)`, derived
// deterministically from `seed` (SplitMix64).
fn projection_axis(seed: u64, dim: usize) -> Vec<f64> {
    let mut state = seed;
    (0..dim)
        .map(|_| (splitmix64(&mut state) as f64 / u64::MAX as f64) * 2.0 - 1.0)
        .collect()
}

fn dot(v: &[f32], axis: &[f64]) -> f64 {
    v.iter().zip(axis).map(|(&a, &b)| f64::from(a) * b).sum()
}

// Min-max scale to `[0, 1]`; a zero (or sub-epsilon) spread collapses to `0.5`.
fn normalize_axis(values: &[f64]) -> Vec<f64> {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    values
        .iter()
        .map(|&v| {
            if span > f64::EPSILON {
                (v - min) / span
            } else {
                0.5
            }
        })
        .collect()
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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

/// Which screen the cockpit is showing.
enum View {
    /// The status panel and collection browser (the default).
    Browser,
    /// A 2-D constellation scatter of one collection's vector space.
    Constellation(ConstellationView),
}

/// State of the constellation view: one collection's projected points, the raw
/// vectors behind them (to re-query), the cursor, and what was queried.
struct ConstellationView {
    collection: String,
    dim: usize,
    points: Vec<StarPoint>,
    vectors: Vec<Vec<f32>>,
    selected: usize,
    // The id the current scatter was queried around; `None` is the seed query.
    query_id: Option<String>,
    // A fetch error to surface instead of an empty scatter.
    error: Option<String>,
}

struct App {
    client: reqwest::Client,
    options: TuiOptions,
    status: ConnStatus,
    collections: Vec<Collection>,
    selected: usize,
    last_refresh: Option<Instant>,
    view: View,
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
            view: View::Browser,
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

    // Open the constellation view for the selected collection, seeded with a
    // zero query (the points nearest the origin). A fetch error is shown in the
    // view rather than dropped.
    async fn enter_constellation(&mut self) {
        let Some(collection) = self.collections.get(self.selected) else {
            return;
        };
        let name = collection.name.clone();
        let dim = collection.dim as usize;
        self.view = match fetch_constellation(
            &self.client,
            &self.options,
            &name,
            dim,
            None,
            CONSTELLATION_K,
        )
        .await
        {
            Ok(c) => View::Constellation(ConstellationView {
                collection: name,
                dim,
                points: c.points,
                vectors: c.vectors,
                selected: 0,
                query_id: None,
                error: None,
            }),
            Err(err) => View::Constellation(ConstellationView {
                collection: name,
                dim,
                points: Vec::new(),
                vectors: Vec::new(),
                selected: 0,
                query_id: None,
                error: Some(err.to_string()),
            }),
        };
    }

    // Re-centre the constellation on the selected point: query the collection
    // around that point's vector. This is the interactive-query interaction.
    async fn requery_constellation(&mut self) {
        let View::Constellation(view) = &self.view else {
            return;
        };
        let Some(vector) = view.vectors.get(view.selected).cloned() else {
            return;
        };
        let id = view.points.get(view.selected).map(|p| p.id.clone());
        let name = view.collection.clone();
        let dim = view.dim;
        match fetch_constellation(
            &self.client,
            &self.options,
            &name,
            dim,
            Some(vector),
            CONSTELLATION_K,
        )
        .await
        {
            Ok(c) => {
                self.view = View::Constellation(ConstellationView {
                    collection: name,
                    dim,
                    points: c.points,
                    vectors: c.vectors,
                    selected: 0,
                    query_id: id,
                    error: None,
                });
            }
            Err(err) => {
                if let View::Constellation(v) = &mut self.view {
                    v.error = Some(err.to_string());
                }
            }
        }
    }

    fn exit_constellation(&mut self) {
        self.view = View::Browser;
    }

    fn star_next(&mut self) {
        if let View::Constellation(v) = &mut self.view
            && !v.points.is_empty()
        {
            v.selected = (v.selected + 1) % v.points.len();
        }
    }

    fn star_prev(&mut self) {
        if let View::Constellation(v) = &mut self.view
            && !v.points.is_empty()
        {
            v.selected = (v.selected + v.points.len() - 1) % v.points.len();
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
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    let in_constellation = matches!(app.view, View::Constellation(_));
                    match key.code {
                        KeyCode::Char('q') => app.should_quit = true,
                        KeyCode::Esc => {
                            if in_constellation {
                                app.exit_constellation();
                            } else {
                                app.should_quit = true;
                            }
                        }
                        KeyCode::Char('r') => {
                            if in_constellation {
                                app.requery_constellation().await;
                            } else {
                                app.refresh().await;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if in_constellation {
                                app.star_next();
                            } else {
                                app.select_next();
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if in_constellation {
                                app.star_prev();
                            } else {
                                app.select_prev();
                            }
                        }
                        KeyCode::Char('v') => {
                            if !in_constellation {
                                app.enter_constellation().await;
                            }
                        }
                        KeyCode::Enter => {
                            if in_constellation {
                                app.requery_constellation().await;
                            } else {
                                app.enter_constellation().await;
                            }
                        }
                        _ => {}
                    }
                }
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

    match &app.view {
        View::Browser => {
            let body = Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)])
                .split(rows[1]);
            frame.render_widget(status_panel(app), body[0]);
            frame.render_widget(collections_panel(app), body[1]);
            frame.render_widget(footer(), rows[2]);
        }
        View::Constellation(view) => {
            render_constellation(frame, rows[1], view);
            frame.render_widget(constellation_footer(), rows[2]);
        }
    }
}

// Render the constellation scatter: a braille canvas of the projected points,
// with the query's nearest neighbour and the cursor highlighted.
fn render_constellation(frame: &mut Frame, area: Rect, view: &ConstellationView) {
    let query = view.query_id.as_deref().unwrap_or("origin (seed)");
    if let Some(err) = &view.error {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("query failed: {err}"),
            Style::new().fg(OFFLINE),
        )))
        .block(
            Block::bordered()
                .title(format!(" constellation · {} ", view.collection))
                .border_style(Style::new().fg(AMBER_DIM)),
        );
        frame.render_widget(p, area);
        return;
    }
    if view.points.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no points to plot — upsert some vectors first",
            Style::new().fg(AMBER_DIM),
        )))
        .block(
            Block::bordered()
                .title(format!(" constellation · {} ", view.collection))
                .border_style(Style::new().fg(AMBER_DIM)),
        );
        frame.render_widget(p, area);
        return;
    }

    let selected = view.points.get(view.selected);
    let sel_label = selected
        .map(|p| format!("{} (#{}) ", p.id, p.rank))
        .unwrap_or_default();
    let title = format!(
        " constellation · {} · {} pts · query={query} · ◆ {sel_label}",
        view.collection,
        view.points.len(),
    );
    // Points other than the nearest neighbour and the cursor.
    let plain: Vec<(f64, f64)> = view
        .points
        .iter()
        .enumerate()
        .filter(|(i, p)| p.rank != 0 && *i != view.selected)
        .map(|(_, p)| (p.x, p.y))
        .collect();
    let nn: Vec<(f64, f64)> = view
        .points
        .iter()
        .filter(|p| p.rank == 0)
        .map(|p| (p.x, p.y))
        .collect();
    let cursor: Vec<(f64, f64)> = selected.map(|p| vec![(p.x, p.y)]).unwrap_or_default();
    let canvas = Canvas::default()
        .block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(AMBER_DIM)),
        )
        .marker(ratatui::symbols::Marker::Braille)
        .x_bounds([-0.05, 1.05])
        .y_bounds([-0.05, 1.05])
        .paint(move |ctx| {
            ctx.draw(&Points {
                coords: &plain,
                color: AMBER_DIM,
            });
            ctx.draw(&Points {
                coords: &nn,
                color: AMBER,
            });
            ctx.draw(&Points {
                coords: &cursor,
                color: SELECT,
            });
        });
    frame.render_widget(canvas, area);
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
        " [q] quit   [r] refresh   [↑/↓] select   [v/enter] constellation ",
        Style::new().fg(AMBER_DIM),
    )))
}

fn constellation_footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        " [↑/↓] move cursor   [enter] query around point   [r] re-query   [esc] back   [q] quit ",
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

    #[test]
    fn project_constellation_is_deterministic_and_normalized() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let vectors = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 1.0],
        ];
        let a = project_constellation(&ids, &vectors, 4);
        let b = project_constellation(&ids, &vectors, 4);
        assert_eq!(a, b, "projection is deterministic for fixed input");
        assert_eq!(a.len(), 3);
        for (rank, p) in a.iter().enumerate() {
            assert_eq!(p.rank, rank, "rank follows the distance order of the hits");
            assert!(
                (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y),
                "points are normalized to the unit square"
            );
        }
    }

    #[test]
    fn project_constellation_handles_degenerate_inputs() {
        // Empty input or zero dimensionality ⇒ nothing to plot.
        assert!(project_constellation(&[], &[], 4).is_empty());
        assert!(project_constellation(&["a".to_owned()], &[vec![1.0]], 0).is_empty());
        // A single point, or identical points, has no spread ⇒ collapses to centre.
        let one = project_constellation(&["a".to_owned()], &[vec![1.0, 2.0, 3.0, 4.0]], 4);
        assert_eq!((one[0].x, one[0].y), (0.5, 0.5));
        let same = project_constellation(
            &["a".to_owned(), "b".to_owned()],
            &[vec![1.0; 4], vec![1.0; 4]],
            4,
        );
        assert!(same.iter().all(|p| p.x == 0.5 && p.y == 0.5));
    }

    #[test]
    fn constellation_cursor_wraps_both_directions() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        app.view = View::Constellation(ConstellationView {
            collection: "c".to_owned(),
            dim: 4,
            points: vec![
                StarPoint {
                    id: "a".to_owned(),
                    x: 0.1,
                    y: 0.1,
                    rank: 0,
                },
                StarPoint {
                    id: "b".to_owned(),
                    x: 0.9,
                    y: 0.9,
                    rank: 1,
                },
            ],
            vectors: vec![vec![1.0; 4], vec![2.0; 4]],
            selected: 0,
            query_id: None,
            error: None,
        });
        let cursor = |app: &App| match &app.view {
            View::Constellation(v) => v.selected,
            View::Browser => panic!("expected the constellation view"),
        };
        app.star_prev(); // wraps to the last point
        assert_eq!(cursor(&app), 1);
        app.star_next(); // wraps back to the first
        assert_eq!(cursor(&app), 0);
    }
}
