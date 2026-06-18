// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver retro terminal cockpit — an API client that renders live status
//! and a collection browser over a running server's REST surface.
//!
//! The cockpit polls `/readyz` and `/v1/collections` and shows connection
//! health, aggregate counts, and a per-collection browser in the **Bronze Quiver**
//! retro theme (ADR-0036) — bronze chrome, a 3-D arrowhead logo, verdigris accents.
//! Pressing `v`/`enter` on a collection opens the **constellation view**:
//! a 2-D random-projection scatter of its vector space (fetched via
//! `POST .../query` with vectors), with the query's nearest neighbour
//! highlighted and a cursor that can re-query around any point. Richer
//! Prometheus metrics land in a later phase (`docs/architecture/overview.md`,
//! ADR-0014). It connects over plaintext HTTP — the common loopback case where a
//! TLS bind is not required; a TLS-aware client is a later enhancement.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
use ratatui::widgets::{Block, Paragraph, Widget};
use serde::Deserialize;
use tokio_stream::StreamExt;

mod decor;
mod logo;
mod theme;

pub use decor::Severity;

/// How often the cockpit polls the server.
const REFRESH: Duration = Duration::from_secs(2);
/// Per-request timeout for the polling client.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// How many points the constellation view samples per query.
const CONSTELLATION_K: usize = 256;

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
    /// Index kind (`hnsw`, `ivf`, `vamana`, `disk_vamana`, `colbert`), when the API
    /// reports it; `None` otherwise.
    #[serde(default)]
    pub index: Option<String>,
    /// Vector-encryption mode (`none`, `dcpe`, `client_side`), when the API reports
    /// it; `None` otherwise.
    #[serde(default)]
    pub vector_encryption: Option<String>,
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
    started: Instant,
    history: Vec<u64>,
    activity: Vec<Activity>,
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
            started: Instant::now(),
            history: Vec::new(),
            activity: Vec::new(),
            view: View::Browser,
            should_quit: false,
        })
    }

    // A short monotonic timestamp ("+12s") for the activity log.
    fn stamp(&self) -> String {
        format!("+{}s", self.started.elapsed().as_secs())
    }

    fn log(&mut self, severity: Severity, message: impl Into<String>) {
        let ts = self.stamp();
        self.activity.push(Activity {
            ts,
            severity,
            message: message.into(),
        });
        // Keep the tail bounded.
        let len = self.activity.len();
        if len > 64 {
            self.activity.drain(0..len - 64);
        }
    }

    async fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        match fetch_snapshot(&self.client, &self.options).await {
            Ok(snapshot) => {
                let total: u64 = snapshot.collections.iter().map(|c| c.count).sum();
                let n = snapshot.collections.len();
                let was_offline = matches!(self.status, ConnStatus::Offline(_));
                self.collections = snapshot.collections;
                if self.selected >= self.collections.len() {
                    self.selected = self.collections.len().saturating_sub(1);
                }
                self.status = ConnStatus::Online {
                    ready: snapshot.ready,
                };
                self.history.push(total);
                let hlen = self.history.len();
                if hlen > 64 {
                    self.history.drain(0..hlen - 64);
                }
                if self.activity.is_empty() || was_offline {
                    self.log(
                        Severity::Info,
                        format!("connected · {n} collections, {total} points"),
                    );
                } else {
                    self.log(Severity::Info, format!("refreshed · {total} points"));
                }
            }
            Err(err) => {
                self.status = ConnStatus::Offline(err.to_string());
                self.log(Severity::Error, format!("offline · {err}"));
            }
        }
    }

    // Build the renderable dashboard from the live state.
    fn dashboard(&self) -> Dashboard {
        let (ready, offline) = match &self.status {
            ConnStatus::Online { ready } => (*ready, None),
            ConnStatus::Offline(err) => (false, Some(err.clone())),
            ConnStatus::Connecting => (false, None),
        };
        let refreshed = self
            .last_refresh
            .map(|t| format!("{}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "—".to_owned());
        Dashboard {
            base_url: self.options.base_url.clone(),
            ready,
            offline,
            collections: self.collections.clone(),
            selected: self.selected,
            refreshed,
            history: self.history.clone(),
            activity: self.activity.clone(),
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

/// An activity-log entry shown in the cockpit's activity panel.
#[derive(Debug, Clone)]
pub struct Activity {
    /// A short relative timestamp, e.g. `+12s`.
    pub ts: String,
    /// The line's severity (drives its glyph and colour).
    pub severity: Severity,
    /// The message.
    pub message: String,
}

/// Everything the dashboard renders, decoupled from the live HTTP client so the
/// same view code drives the cockpit, the tests, and the screenshot tool (ADR-0036).
#[derive(Debug, Clone)]
pub struct Dashboard {
    /// The server URL shown in the header.
    pub base_url: String,
    /// Whether the server reported ready.
    pub ready: bool,
    /// An offline error message, if the server is unreachable.
    pub offline: Option<String>,
    /// The collections to list.
    pub collections: Vec<Collection>,
    /// The selected collection index.
    pub selected: usize,
    /// A human "refreshed N s ago" string.
    pub refreshed: String,
    /// Total-point history for the trend sparkline.
    pub history: Vec<u64>,
    /// Recent activity-log entries.
    pub activity: Vec<Activity>,
}

impl Dashboard {
    /// A rich, fixed demo dashboard for screenshots and tests — never fetched.
    #[must_use]
    pub fn demo() -> Self {
        let col = |name: &str, dim, metric: &str, count, index: &str, enc: &str| Collection {
            name: name.to_owned(),
            dim,
            metric: metric.to_owned(),
            count,
            index: Some(index.to_owned()),
            vector_encryption: Some(enc.to_owned()),
        };
        Dashboard {
            base_url: "http://127.0.0.1:6333".to_owned(),
            ready: true,
            offline: None,
            collections: vec![
                col("contacts", 768, "cosine", 48_213, "hnsw", "none"),
                col(
                    "documents",
                    1024,
                    "cosine",
                    1_284_098,
                    "disk_vamana",
                    "none",
                ),
                col("images", 512, "l2", 96_320, "ivf", "none"),
                col("vault", 8, "l2", 1_024, "hnsw", "dcpe"),
                col("sessions", 256, "dot", 7_740, "hnsw", "client_side"),
            ],
            selected: 1,
            refreshed: "2s ago".to_owned(),
            history: vec![
                120, 180, 240, 300, 420, 560, 700, 880, 1010, 1180, 1300, 1437,
            ],
            activity: vec![
                Activity {
                    ts: "+0s".to_owned(),
                    severity: Severity::Info,
                    message: "connected · 5 collections, 1437395 points".to_owned(),
                },
                Activity {
                    ts: "+2s".to_owned(),
                    severity: Severity::Info,
                    message: "refreshed · 1437395 points".to_owned(),
                },
                Activity {
                    ts: "+4s".to_owned(),
                    severity: Severity::Warn,
                    message: "documents · disk index warming".to_owned(),
                },
                Activity {
                    ts: "+6s".to_owned(),
                    severity: Severity::Info,
                    message: "vault · DCPE-encrypted vectors".to_owned(),
                },
            ],
        }
    }
}

/// Render the logo banner centred on an oak background to a fresh buffer (for the
/// README / docs logo image and the splash).
#[must_use]
pub fn render_logo(width: u16, height: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    Block::new()
        .style(Style::new().bg(theme::BG))
        .render(buf.area, &mut buf);
    let mut lines = logo::banner();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "a quiver of vectors",
        theme::dim(),
    )));
    let lh = lines.len() as u16;
    let y = height.saturating_sub(lh) / 2;
    Paragraph::new(lines)
        .alignment(ratatui::layout::Alignment::Center)
        .render(Rect::new(0, y, width, lh.min(height)), &mut buf);
    buf
}

/// Render the dashboard for `dash` to a fresh `width`×`height` buffer (used by the
/// screenshot tool and tests; the live cockpit renders into the frame's buffer).
#[must_use]
pub fn render_dashboard(width: u16, height: u16, dash: &Dashboard) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    draw_dashboard(buf.area, &mut buf, dash);
    buf
}

/// Render the constellation demo scatter to a fresh `width`×`height` buffer.
#[must_use]
pub fn render_constellation_demo(width: u16, height: u16) -> Buffer {
    let view = demo_constellation_view();
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    draw_constellation(buf.area, &mut buf, &view, "http://127.0.0.1:6333");
    buf
}

fn ui(frame: &mut Frame, app: &App) {
    match &app.view {
        View::Browser => draw_dashboard(frame.area(), frame.buffer_mut(), &app.dashboard()),
        View::Constellation(view) => {
            draw_constellation(
                frame.area(),
                frame.buffer_mut(),
                view,
                &app.options.base_url,
            );
        }
    }
}

// Lay out and render the whole dashboard into `buf`: the logo banner hero, a left
// column (status + relationships) and a right column (the collections table + the
// activity log), and the footer — all in the Bronze Quiver theme.
fn draw_dashboard(area: Rect, buf: &mut Buffer, dash: &Dashboard) {
    Block::new()
        .style(Style::new().bg(theme::BG))
        .render(area, buf);
    let rows = Layout::vertical([
        Constraint::Length(9),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    banner_header(&dash.base_url).render(rows[0], buf);
    let body =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).split(rows[1]);
    let left = Layout::vertical([Constraint::Length(9), Constraint::Min(0)]).split(body[0]);
    status_panel(dash).render(left[0], buf);
    relationships_panel(dash).render(left[1], buf);
    let right = Layout::vertical([Constraint::Min(0), Constraint::Length(8)]).split(body[1]);
    collections_panel(dash).render(right[0], buf);
    activity_panel(dash).render(right[1], buf);
    footer().render(rows[2], buf);
}

// Render the constellation view into `buf`: a compact header, a braille canvas of
// the projected points (the query's nearest neighbour and the cursor highlighted),
// and the footer.
fn draw_constellation(area: Rect, buf: &mut Buffer, view: &ConstellationView, url: &str) {
    Block::new()
        .style(Style::new().bg(theme::BG))
        .render(area, buf);
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    compact_header(url).render(rows[0], buf);
    let body = rows[1];
    let query = view.query_id.as_deref().unwrap_or("origin (seed)");

    if let Some(err) = &view.error {
        Paragraph::new(Line::from(Span::styled(
            format!("query failed: {err}"),
            theme::alert(),
        )))
        .block(decor::panel(&format!(
            "constellation · {}",
            view.collection
        )))
        .render(body, buf);
    } else if view.points.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "no points to plot — upsert some vectors first",
            theme::dim(),
        )))
        .block(decor::panel(&format!(
            "constellation · {}",
            view.collection
        )))
        .render(body, buf);
    } else {
        let selected = view.points.get(view.selected);
        let sel_label = selected
            .map(|p| format!("{} (#{})", p.id, p.rank))
            .unwrap_or_default();
        let title = format!(
            "constellation · {} · {} pts · query={query} · ◆ {sel_label}",
            view.collection,
            view.points.len(),
        );
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
        Canvas::default()
            .block(decor::panel(&title))
            .marker(ratatui::symbols::Marker::Braille)
            .x_bounds([-0.05, 1.05])
            .y_bounds([-0.05, 1.05])
            .paint(move |ctx| {
                ctx.draw(&Points {
                    coords: &plain,
                    color: theme::DIM,
                });
                ctx.draw(&Points {
                    coords: &nn,
                    color: theme::CHROME,
                });
                ctx.draw(&Points {
                    coords: &cursor,
                    color: theme::ACCENT_HI,
                });
            })
            .render(body, buf);
    }
    constellation_footer().render(rows[2], buf);
}

// A reproducible demo scatter for screenshots: clustered points around the seed.
fn demo_constellation_view() -> ConstellationView {
    let mut s = 0x5151_1abe_1abe_5151u64;
    let mut rng = || {
        s = splitmix64(&mut s);
        (s >> 40) as f64 / (1u64 << 24) as f64
    };
    let mut points = Vec::new();
    let mut vectors = Vec::new();
    for i in 0..180 {
        let x = (rng() * 0.9 + 0.05).clamp(0.0, 1.0);
        let y = (rng() * 0.9 + 0.05).clamp(0.0, 1.0);
        vectors.push(vec![x as f32, y as f32]);
        points.push(StarPoint {
            id: format!("doc-{i}"),
            x,
            y,
            rank: i,
        });
    }
    ConstellationView {
        collection: "documents".to_owned(),
        dim: 1024,
        points,
        vectors,
        selected: 7,
        query_id: None,
        error: None,
    }
}

// The dashboard hero: the QUIVER logo banner (the V is a 3-D arrowhead) in a
// rounded frame titled with the tagline and the server URL.
fn banner_header(url: &str) -> Paragraph<'static> {
    Paragraph::new(logo::banner()).block(
        decor::panel("the cockpit")
            .title(Line::from(Span::styled(format!(" {url} "), theme::accent())).right_aligned()),
    )
}

// A compact one-line header for the constellation view.
fn compact_header(url: &str) -> Paragraph<'static> {
    let mut spans = logo::compact().spans;
    spans.push(Span::styled("   ", theme::dim()));
    spans.push(Span::styled(url.to_owned(), theme::accent()));
    Paragraph::new(Line::from(spans)).block(Block::bordered().border_style(theme::border()))
}

fn status_panel(dash: &Dashboard) -> Paragraph<'static> {
    let agg = aggregate(&dash.collections);
    let (badge_label, style) = if dash.offline.is_some() {
        ("OFFLINE", theme::alert())
    } else if dash.ready {
        ("ONLINE · READY", theme::ok())
    } else {
        ("CONNECTING", theme::dim())
    };
    let mut lines = vec![
        Line::from(decor::badge(badge_label, style)),
        Line::from(""),
        kv("collections", &agg.collections.to_string()),
        kv("points", &agg.total_points.to_string()),
        kv("refreshed", &dash.refreshed),
    ];
    if !dash.history.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("trend        ", theme::dim()),
            Span::styled(decor::sparkline(&dash.history), theme::accent()),
        ]));
    }
    if let Some(err) = &dash.offline {
        lines.push(Line::from(Span::styled(truncate(err, 30), theme::alert())));
    }
    Paragraph::new(lines).block(decor::panel("status"))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<13}"), theme::dim()),
        Span::styled(value.to_owned(), theme::text()),
    ])
}

// The selected collection's structure as a relationship tree, atop a small drum.
fn relationships_panel(dash: &Dashboard) -> Paragraph<'static> {
    let mut lines: Vec<Line> = decor::db_icon()
        .iter()
        .map(|row| Line::from(Span::styled(format!("  {row}"), theme::dim())))
        .collect();
    if let Some(c) = dash.collections.get(dash.selected) {
        let rows = vec![
            (0usize, c.name.clone()),
            (
                1,
                format!(
                    "index    {}",
                    c.index.clone().unwrap_or_else(|| "hnsw".to_owned())
                ),
            ),
            (1, format!("metric   {}", c.metric)),
            (
                1,
                format!(
                    "vectors  {}",
                    c.vector_encryption
                        .clone()
                        .unwrap_or_else(|| "plaintext".to_owned())
                ),
            ),
            (1, format!("points   {}", c.count)),
        ];
        lines.extend(decor::tree(&rows));
    } else {
        lines.push(Line::from(Span::styled(
            "no collection selected",
            theme::dim(),
        )));
    }
    Paragraph::new(lines).block(decor::panel("relationships"))
}

fn collections_panel(dash: &Dashboard) -> Paragraph<'static> {
    let maxc = dash
        .collections
        .iter()
        .map(|c| c.count)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "  {:<16}{:>4}  {:<8}{:<12}{:>9}",
                "collection", "dim", "metric", "index", "points"
            ),
            theme::dim(),
        )),
        decor::divider(58),
    ];
    if dash.collections.is_empty() {
        lines.push(Line::from(Span::styled(
            "no collections yet — import or upsert some vectors",
            theme::dim(),
        )));
    }
    for (i, c) in dash.collections.iter().enumerate() {
        let selected = i == dash.selected;
        let name_style = if selected {
            theme::selected()
        } else {
            theme::text()
        };
        let marker = if selected { "▸" } else { " " };
        let idx = c.index.clone().unwrap_or_else(|| "hnsw".to_owned());
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), theme::accent()),
            Span::styled(format!("{:<16}", truncate(&c.name, 16)), name_style),
            Span::styled(format!("{:>4}  ", c.dim), theme::text()),
            Span::styled(format!("{:<8}", c.metric), theme::dim()),
            Span::styled(format!("{:<12}", idx), theme::dim()),
            Span::styled(format!("{:>9}  ", c.count), theme::text()),
            Span::styled(decor::bar(c.count, maxc, 8), theme::accent()),
        ]));
    }
    Paragraph::new(lines).block(decor::panel_active("collections"))
}

// Recent activity-log lines (the latest few), each with a severity glyph.
fn activity_panel(dash: &Dashboard) -> Paragraph<'static> {
    let lines: Vec<Line> = if dash.activity.is_empty() {
        vec![Line::from(Span::styled(
            "waiting for the first poll…",
            theme::dim(),
        ))]
    } else {
        let n = dash.activity.len();
        dash.activity[n.saturating_sub(6)..]
            .iter()
            .map(|a| decor::log_line(&a.ts, a.severity, &a.message))
            .collect()
    };
    Paragraph::new(lines).block(decor::panel("activity"))
}

// Truncate to `max` chars, adding an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        " [q] quit   [r] refresh   [↑/↓] select   [v/enter] constellation ",
        theme::dim(),
    )))
}

fn constellation_footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        " [↑/↓] move cursor   [enter] query around point   [r] re-query   [esc] back   [q] quit ",
        theme::dim(),
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
            index: None,
            vector_encryption: None,
        }
    }

    // Flatten a rendered buffer to a string of its cell symbols.
    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn dashboard_demo_renders_the_logo_table_relationships_and_activity() {
        let buf = render_dashboard(110, 34, &Dashboard::demo());
        let text = buffer_text(&buf);
        // The collections table and its columns.
        assert!(
            text.contains("collection") && text.contains("points"),
            "table header"
        );
        assert!(
            text.contains("documents") && text.contains("disk_vamana"),
            "a table row"
        );
        // The relationships tree and the activity log.
        assert!(
            text.contains("relationships") && text.contains("╰─►"),
            "relationship tree"
        );
        assert!(
            text.contains("activity") && text.contains('●'),
            "activity log"
        );
        // The status badge and a load bar.
        assert!(text.contains("ONLINE"), "status badge");
        assert!(text.contains('█'), "logo / bar blocks");
    }

    #[test]
    fn dashboard_handles_an_empty_and_offline_state() {
        let mut dash = Dashboard::demo();
        dash.collections.clear();
        dash.selected = 0;
        dash.offline = Some("connection refused".to_owned());
        dash.ready = false;
        let text = buffer_text(&render_dashboard(110, 34, &dash));
        assert!(text.contains("OFFLINE"), "offline badge");
        assert!(text.contains("no collections yet"), "empty table");
    }

    #[test]
    fn constellation_demo_renders_a_scatter() {
        let text = buffer_text(&render_constellation_demo(110, 34));
        assert!(text.contains("constellation") && text.contains("documents"));
    }

    #[test]
    fn logo_renders_centred_with_blocks_and_tagline() {
        let text = buffer_text(&render_logo(60, 11));
        assert!(text.contains('█'), "block letters");
        assert!(text.contains("a quiver of vectors"), "tagline");
    }

    #[test]
    fn dashboard_reflects_connecting_online_and_offline() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        // Initial: connecting (not ready, not offline).
        let d = app.dashboard();
        assert!(!d.ready && d.offline.is_none());
        // Online and ready, with collections and a history.
        app.status = ConnStatus::Online { ready: true };
        app.collections = vec![collection("a", 5), collection("b", 7)];
        app.selected = 1;
        app.history = vec![1, 2, 3];
        let d = app.dashboard();
        assert!(d.ready && d.offline.is_none());
        assert_eq!(d.collections.len(), 2);
        assert_eq!(d.selected, 1);
        assert_eq!(d.history, vec![1, 2, 3]);
        // Offline carries the error message.
        app.status = ConnStatus::Offline("connection refused".to_owned());
        let d = app.dashboard();
        assert!(!d.ready);
        assert_eq!(d.offline.as_deref(), Some("connection refused"));
    }

    #[test]
    fn activity_log_records_with_a_stamp_and_stays_bounded() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        assert!(app.stamp().starts_with('+'));
        for i in 0..80 {
            app.log(Severity::Info, format!("event {i}"));
        }
        assert_eq!(app.activity.len(), 64, "the activity tail is bounded");
        assert!(app.activity.last().unwrap().message.contains("event 79"));
        assert_eq!(app.activity[0].severity, Severity::Info);
    }

    #[test]
    fn exit_constellation_returns_to_the_browser() {
        let mut app = App::new(TuiOptions::default()).unwrap();
        app.view = View::Constellation(ConstellationView {
            collection: "c".to_owned(),
            dim: 4,
            points: Vec::new(),
            vectors: Vec::new(),
            selected: 0,
            query_id: None,
            error: None,
        });
        app.exit_constellation();
        assert!(matches!(app.view, View::Browser));
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
