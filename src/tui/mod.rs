//! Interactive dashboard.
//!
//! This is the "new design": instead of an npyscreen menu or a web UI, the
//! native client draws a single focused screen with three live views — browse,
//! queue and system — and never blocks on the network while drawing.

use std::collections::VecDeque;
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Sparkline, Wrap,
};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, watch};

use crate::config::{Config, Language};
use crate::download::{Cancelled, Engine, Event, EVENT_BUFFER};
use crate::metrics::{self, format_bytes, format_duration, format_rate, Metrics};
use crate::model::{Episode, SearchHit, Series, Site, Target};
use crate::net::Http;
use crate::scrape::Scraper;

const BG: Color = Color::Rgb(10, 12, 19);
const PANEL: Color = Color::Rgb(17, 20, 30);
const ACCENT: Color = Color::Rgb(255, 184, 76);
const MINT: Color = Color::Rgb(106, 227, 185);
const DIM: Color = Color::Rgb(112, 120, 142);
const TEXT: Color = Color::Rgb(226, 232, 240);
const RED: Color = Color::Rgb(248, 113, 113);
const BLUE: Color = Color::Rgb(125, 166, 255);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Results,
    Episodes,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemState {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
struct QueueItem {
    id: u64,
    label: String,
    state: ItemState,
    done: u64,
    total: u64,
    bytes: u64,
    speed: f64,
    message: String,
}

impl QueueItem {
    fn percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64 * 100.0).clamp(0.0, 100.0)
        }
    }

    fn eta(&self) -> f64 {
        if self.speed <= 0.0 || self.total == 0 {
            return 0.0;
        }
        let remaining_segments = self.total.saturating_sub(self.done) as f64;
        let per_segment = self.bytes as f64 / self.done.max(1) as f64;
        (remaining_segments * per_segment) / self.speed
    }
}

enum UiMsg {
    Search(Vec<SearchHit>),
    Series(Box<Series>),
    Season(usize, Vec<Episode>),
    Status(String),
}

struct Ctx {
    scraper: Arc<Scraper>,
    engine: Arc<Engine>,
    ui: mpsc::Sender<UiMsg>,
    events: mpsc::Sender<Event>,
    /// Caps concurrent episodes, so queueing a season does not start a dozen
    /// downloads (and their segment workers) at once.
    slots: Arc<tokio::sync::Semaphore>,
}

struct App {
    site: Site,
    query: String,
    typing: bool,
    results: Vec<SearchHit>,
    result_state: ListState,
    series: Option<Series>,
    season_index: usize,
    loading_season: bool,
    episodes: Vec<Episode>,
    episode_state: ListState,
    queue: Vec<QueueItem>,
    queue_state: ListState,
    focus: Focus,
    language: Language,
    provider: String,
    status: String,
    log: VecDeque<String>,
    net_history: VecDeque<u64>,
    net_tick: Instant,
    metrics: Metrics,
    quit: bool,
    next_id: u64,
}

impl App {
    fn new(site: Site, cfg: &Config) -> Self {
        Self {
            site,
            query: String::new(),
            typing: false,
            results: Vec::new(),
            result_state: ListState::default(),
            series: None,
            season_index: 0,
            loading_season: false,
            episodes: Vec::new(),
            episode_state: ListState::default(),
            queue: Vec::new(),
            queue_state: ListState::default(),
            focus: Focus::Results,
            language: cfg.language,
            provider: cfg.provider.clone(),
            status: "Press / to search, or paste a URL with :open".to_string(),
            log: VecDeque::new(),
            net_history: VecDeque::new(),
            net_tick: Instant::now(),
            metrics: Metrics::default(),
            quit: false,
            next_id: 0,
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.status = text.clone();
        self.log.push_back(text);
        while self.log.len() > 200 {
            self.log.pop_front();
        }
    }

    fn active_downloads(&self) -> usize {
        self.queue.iter().filter(|q| q.state == ItemState::Running).count()
    }

    fn total_speed(&self) -> f64 {
        self.queue
            .iter()
            .filter(|q| q.state == ItemState::Running)
            .map(|q| q.speed)
            .sum()
    }

    async fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        if self.typing {
            match key.code {
                KeyCode::Enter => {
                    self.typing = false;
                    self.start_search(ctx);
                }
                KeyCode::Esc => {
                    self.typing = false;
                    self.note("search cancelled");
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) => self.query.push(c),
                _ => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') => {
                self.typing = true;
                self.query.clear();
                self.focus = Focus::Results;
                self.note("type a title, Enter to search");
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Results => Focus::Episodes,
                    Focus::Episodes => Focus::Queue,
                    Focus::Queue => Focus::Results,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Enter => match self.focus {
                Focus::Results => self.open_result(ctx),
                Focus::Episodes => self.queue_episode(ctx),
                Focus::Queue => {}
            },
            KeyCode::Char('a') => self.queue_season(ctx),
            KeyCode::Char('s') => self.next_season(ctx),
            KeyCode::Char('l') => {
                let index = Language::ALL
                    .iter()
                    .position(|l| *l == self.language)
                    .unwrap_or(0);
                self.language = Language::ALL[(index + 1) % Language::ALL.len()];
                self.note(format!("language: {}", self.language.label()));
            }
            KeyCode::Char('x') => {
                self.queue.retain(|q| !matches!(q.state, ItemState::Done | ItemState::Failed));
                self.clamp_queue();
            }
            KeyCode::Char('d') => {
                if !self.queue.is_empty() {
                    let index = self.queue_state.selected().unwrap_or(0);
                    if index < self.queue.len() {
                        let item = self.queue.remove(index);
                        self.clamp_queue();
                        self.note(format!("removed {}", item.label));
                    }
                }
            }
            KeyCode::Char('c') => {
                ctx.engine.cancel();
                self.note("cancelling all downloads");
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Results => {
                let len = self.results.len();
                if len == 0 {
                    return;
                }
                let current = self.result_state.selected().unwrap_or(0) as i32;
                let next = (current + delta).clamp(0, len as i32 - 1);
                self.result_state.select(Some(next as usize));
            }
            Focus::Episodes => {
                let len = self.episodes.len();
                if len == 0 {
                    return;
                }
                let current = self.episode_state.selected().unwrap_or(0) as i32;
                let next = (current + delta).clamp(0, len as i32 - 1);
                self.episode_state.select(Some(next as usize));
            }
            Focus::Queue => {
                let len = self.queue.len();
                if len == 0 {
                    return;
                }
                let current = self.queue_state.selected().unwrap_or(0) as i32;
                let next = (current + delta).clamp(0, len as i32 - 1);
                self.queue_state.select(Some(next as usize));
            }
        }
    }

    fn clamp_queue(&mut self) {
        if self.queue.is_empty() {
            self.queue_state.select(None);
        } else {
            let index = self
                .queue_state
                .selected()
                .unwrap_or(0)
                .min(self.queue.len() - 1);
            self.queue_state.select(Some(index));
        }
    }

    fn start_search(&mut self, ctx: &Ctx) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.note("empty search");
            return;
        }
        self.note(format!("searching {} for \"{query}\"…", self.site.name()));
        let scraper = ctx.scraper.clone();
        let ui = ctx.ui.clone();
        let site = self.site;
        tokio::spawn(async move {
            match scraper.search(site, &query).await {
                Ok(hits) => {
                    let _ = ui.send(UiMsg::Search(hits)).await;
                }
                Err(err) => {
                    let _ = ui
                        .send(UiMsg::Status(format!("search failed: {err}")))
                        .await;
                }
            }
        });
    }

    fn open_result(&mut self, ctx: &Ctx) {
        let Some(index) = self.result_state.selected() else {
            return;
        };
        let Some(hit) = self.results.get(index) else {
            return;
        };
        let Ok(target) = Target::parse(&hit.url) else {
            self.note("unsupported URL in result");
            return;
        };
        self.note(format!("loading {}…", hit.title));
        let scraper = ctx.scraper.clone();
        let ui = ctx.ui.clone();
        tokio::spawn(async move {
            match scraper.series(&target).await {
                Ok(series) => {
                    let _ = ui.send(UiMsg::Series(Box::new(series))).await;
                }
                Err(err) => {
                    let _ = ui.send(UiMsg::Status(format!("could not load: {err}"))).await;
                }
            }
        });
    }

    fn load_season(&mut self, index: usize, ctx: &Ctx) {
        let Some(series) = self.series.clone() else {
            return;
        };
        let Some(season) = series.seasons.get(index).cloned() else {
            return;
        };
        self.loading_season = true;
        self.note(if season.are_movies {
            "loading movies…".to_string()
        } else {
            format!("loading season {:02}…", season.number)
        });
        let scraper = ctx.scraper.clone();
        let ui = ctx.ui.clone();
        let site = series.site;
        tokio::spawn(async move {
            match scraper
                .season(&season.url, site, season.number, season.are_movies)
                .await
            {
                Ok(loaded) => {
                    let _ = ui.send(UiMsg::Season(index, loaded.episodes)).await;
                }
                Err(err) => {
                    let _ = ui.send(UiMsg::Status(format!("season failed: {err}"))).await;
                }
            }
        });
    }

    fn next_season(&mut self, ctx: &Ctx) {
        let Some(series) = &self.series else { return };
        if series.seasons.is_empty() {
            return;
        }
        self.season_index = (self.season_index + 1) % series.seasons.len();
        self.episodes.clear();
        self.episode_state.select(None);
        self.load_season(self.season_index, ctx);
    }

    fn queue_episode(&mut self, ctx: &Ctx) {
        let Some(index) = self.episode_state.selected() else {
            self.note("select an episode first");
            return;
        };
        let Some(episode) = self.episodes.get(index).cloned() else {
            return;
        };
        let Some(series) = self.series.clone() else {
            return;
        };
        self.enqueue(series, episode, ctx);
    }

    fn queue_season(&mut self, ctx: &Ctx) {
        let Some(series) = self.series.clone() else {
            return;
        };
        if self.episodes.is_empty() {
            self.note("no episodes loaded");
            return;
        }
        for episode in self.episodes.clone() {
            self.enqueue(series.clone(), episode, ctx);
        }
        self.note(format!(
            "queued {} episode(s) of season {:02}",
            self.episodes.len(),
            series
                .seasons
                .get(self.season_index)
                .map(|s| s.number)
                .unwrap_or(0)
        ));
    }

    fn enqueue(&mut self, series: Series, episode: Episode, ctx: &Ctx) {
        let id = self.next_id;
        self.next_id += 1;
        let label = episode.label();
        self.queue.push(QueueItem {
            id,
            label: label.clone(),
            state: ItemState::Queued,
            done: 0,
            total: 0,
            bytes: 0,
            speed: 0.0,
            message: String::new(),
        });
        self.queue_state
            .select(Some(self.queue.len().saturating_sub(1)));

        let engine = ctx.engine.clone();
        let events = ctx.events.clone();
        let slots = ctx.slots.clone();
        let language = self.language;
        tokio::spawn(async move {
            let _permit = match slots.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let mut series = series;
            series.title_clean = if series.title_clean.is_empty() {
                series.title.clone()
            } else {
                series.title_clean.clone()
            };
            let result = engine_with_language(&engine, id, &series, &episode, language).await;
            if let Err(err) = result {
                let message = if err.downcast_ref::<Cancelled>().is_some() {
                    "cancelled".to_string()
                } else {
                    err.to_string()
                };
                let _ = events
                    .send(Event::Finished {
                        id,
                        label,
                        ok: false,
                        message,
                        path: None,
                    })
                    .await;
            }
        });
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Stage { id, text } => {
                if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
                    item.message = text.clone();
                }
                self.note(text);
            }
            Event::Progress {
                id,
                done,
                total,
                bytes,
                speed,
                ..
            } => {
                if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
                    item.state = ItemState::Running;
                    item.done = done;
                    item.total = total;
                    item.bytes = bytes;
                    item.speed = speed;
                }
            }
            Event::Finished {
                id,
                label,
                ok,
                message,
                ..
            } => {
                if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
                    item.state = if ok { ItemState::Done } else { ItemState::Failed };
                    item.message = message.clone();
                    item.speed = 0.0;
                    if ok {
                        item.done = item.total.max(item.done);
                    }
                } else {
                    self.queue.push(QueueItem {
                        id,
                        label,
                        state: if ok { ItemState::Done } else { ItemState::Failed },
                        done: 0,
                        total: 0,
                        bytes: 0,
                        speed: 0.0,
                        message: message.clone(),
                    });
                }
                self.note(format!("{} {}", if ok { "✓" } else { "✗" }, message));
            }
        }
    }

    /// Returns a season index that the caller must load.
    fn on_ui(&mut self, msg: UiMsg) -> Option<usize> {
        let mut load = None;
        match msg {
            UiMsg::Search(hits) => {
                self.note(format!("{} result(s)", hits.len()));
                self.results = hits;
                self.result_state
                    .select(if self.results.is_empty() { None } else { Some(0) });
                self.series = None;
                self.episodes.clear();
                self.episode_state.select(None);
                self.focus = Focus::Results;
            }
            UiMsg::Series(series) => {
                self.note(format!("{} — {} season(s)", series.title, series.seasons.len()));
                self.series = Some(*series);
                self.season_index = 0;
                self.episodes.clear();
                self.episode_state.select(None);
                self.focus = Focus::Episodes;
                self.loading_season = true;
                load = Some(0);
            }
            UiMsg::Season(index, episodes) => {
                self.loading_season = false;
                if index == self.season_index {
                    self.note(format!("{} episode(s) loaded", episodes.len()));
                    self.episodes = episodes;
                    self.episode_state
                        .select(if self.episodes.is_empty() { None } else { Some(0) });
                }
            }
            UiMsg::Status(text) => {
                self.loading_season = false;
                self.note(text)
            }
        }
        load
    }
}

/// The engine reads its language from the config, so switch it per queue item
/// with a copy of the config rather than mutating shared state.
async fn engine_with_language(
    engine: &Engine,
    id: u64,
    series: &Series,
    episode: &Episode,
    language: Language,
) -> Result<std::path::PathBuf> {
    engine.download_episode_with_language(id, series, episode, None, language).await
}

pub async fn run(cfg: Arc<Config>, site: Site) -> Result<()> {
    let http = Arc::new(Http::new(&cfg.user_agent, cfg.retries)?);
    let scraper = Arc::new(Scraper::new((*http).clone()));

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_BUFFER);
    let engine = Arc::new(Engine::new(
        cfg.clone(),
        http,
        scraper.clone(),
        event_tx.clone(),
    ));
    let (ui_tx, mut ui_rx) = mpsc::channel(64);
    let metrics_rx = metrics::spawn(Duration::from_secs(1));

    let ctx = Ctx {
        scraper,
        engine: engine.clone(),
        ui: ui_tx,
        events: event_tx,
        slots: Arc::new(tokio::sync::Semaphore::new(cfg.jobs.max(1))),
    };

    enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    execute!(stdout(), EnterAlternateScreen)?;

    let result = event_loop(&mut terminal, &ctx, &mut event_rx, &mut ui_rx, metrics_rx, &cfg, site).await;

    disable_raw_mode().ok();
    execute!(stdout(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    engine.cancel();
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ctx: &Ctx,
    event_rx: &mut mpsc::Receiver<Event>,
    ui_rx: &mut mpsc::Receiver<UiMsg>,
    mut metrics_rx: watch::Receiver<Metrics>,
    cfg: &Config,
    site: Site,
) -> Result<()> {
    let mut app = App::new(site, cfg);

    loop {
        app.metrics = *metrics_rx.borrow_and_update();
        // One sample per second: 120 points is then really two minutes, as the
        // chart label says, instead of the 7 seconds a 60 ms tick produced.
        if app.net_tick.elapsed() >= Duration::from_secs(1) {
            app.net_tick = Instant::now();
            app.net_history.push_back(app.metrics.net_rx_rate as u64);
            while app.net_history.len() > 120 {
                app.net_history.pop_front();
            }
        }

        terminal.draw(|frame| draw(frame, &mut app))?;

        while event::poll(Duration::from_millis(0))? {
            if let CrosstermEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key, ctx).await;
                }
            }
        }

        if app.quit {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(60)) => {}
            Some(event) = event_rx.recv() => app.on_event(event),
            Some(msg) = ui_rx.recv() => {
                if let Some(index) = app.on_ui(msg) {
                    app.load_season(index, ctx);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                app.quit = true;
            }
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, rows[0], app);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(rows[1]);

    draw_browse(frame, columns[0], app);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(12)])
        .split(columns[1]);

    draw_queue(frame, right[0], app);
    draw_system(frame, right[1], app);

    draw_footer(frame, rows[2], app);
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Rgb(96, 110, 148)
        } else {
            Color::Rgb(52, 58, 78)
        }))
        .style(Style::default().bg(PANEL))
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM).add_modifier(Modifier::BOLD)
            },
        ))
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let active = app.active_downloads();
    let queued = app.queue.iter().filter(|q| q.state == ItemState::Queued).count();
    let done = app.queue.iter().filter(|q| q.state == ItemState::Done).count();
    let failed = app.queue.iter().filter(|q| q.state == ItemState::Failed).count();

    let line1 = Line::from(vec![
        Span::styled(" ANIWORLD", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" // ", Style::default().fg(DIM)),
        Span::styled("native", Style::default().fg(MINT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {}  •  {}  •  {}",
                app.site.name(),
                app.language.label(),
                app.provider
            ),
            Style::default().fg(DIM),
        ),
    ]);

    let line2 = Line::from(vec![
        Span::styled(" active ", Style::default().fg(DIM)),
        Span::styled(active.to_string(), Style::default().fg(MINT)),
        Span::styled("   queued ", Style::default().fg(DIM)),
        Span::styled(queued.to_string(), Style::default().fg(BLUE)),
        Span::styled("   done ", Style::default().fg(DIM)),
        Span::styled(done.to_string(), Style::default().fg(TEXT)),
        Span::styled("   failed ", Style::default().fg(DIM)),
        Span::styled(failed.to_string(), Style::default().fg(RED)),
        Span::styled("   ↓ ", Style::default().fg(DIM)),
        Span::styled(
            format_rate(app.total_speed()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::Rgb(52, 58, 78)))
        .style(Style::default().bg(BG));
    frame.render_widget(Paragraph::new(vec![line1, line2]).block(block), area);
}

fn draw_browse(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = match &app.series {
        Some(series) => format!("BROWSE — {}", truncate(&series.title, 26)),
        None => format!("BROWSE — {}", app.site.name()),
    };
    let focused = matches!(app.focus, Focus::Results | Focus::Episodes);
    let block = panel(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(series) = app.series.clone() else {
        let mut lines: Vec<Line> = Vec::new();
        if app.typing {
            lines.push(Line::from(vec![
                Span::styled("/ ", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{}_", app.query),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(""));
        }
        if app.results.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no results yet",
                Style::default().fg(DIM),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  /  search a title",
                Style::default().fg(DIM),
            )));
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                inner,
            );
            return;
        }

        let items: Vec<ListItem> = app
            .results
            .iter()
            .map(|hit| {
                ListItem::new(vec![
                    Line::from(Span::styled(
                        truncate(&hit.title, 44),
                        Style::default().fg(TEXT),
                    )),
                    Line::from(Span::styled(
                        format!("  {}", hit.site.name()),
                        Style::default().fg(DIM),
                    )),
                ])
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(30, 34, 48))
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▍");
        frame.render_stateful_widget(list, inner, &mut app.result_state);
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(3)])
        .split(inner);

    let mut season_spans = vec![Span::styled(" seasons  ", Style::default().fg(DIM))];
    for (index, season) in series.seasons.iter().enumerate() {
        let label = if season.are_movies {
            "movies".to_string()
        } else {
            format!("{:02}", season.number)
        };
        let style = if index == app.season_index {
            Style::default().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        season_spans.push(Span::styled(format!(" {label} "), style));
        season_spans.push(Span::raw(" "));
    }
    let meta = format!(
        "{}  •  {}",
        series.year.as_deref().unwrap_or("year unknown"),
        series.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(season_spans),
            Line::from(Span::styled(meta, Style::default().fg(DIM))),
        ])
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    if app.episodes.is_empty() {
        let season_label = series
            .seasons
            .get(app.season_index)
            .map(|season| {
                if season.are_movies {
                    "movies".to_string()
                } else {
                    format!("season {:02}", season.number)
                }
            })
            .unwrap_or_else(|| "season".to_string());
        let text = if app.loading_season {
            format!("  loading {season_label}…")
        } else {
            format!("  no episodes listed for {season_label}")
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::default().fg(DIM)))),
            rows[1],
        );
        return;
    }

    let items: Vec<ListItem> = app
        .episodes
        .iter()
        .map(|episode| {
            let languages: Vec<&str> = episode
                .available_languages()
                .iter()
                .map(|l| match l {
                    Language::GermanDub => "DE",
                    Language::EnglishSub => "EN",
                    Language::GermanSub => "DESUB",
                    Language::EnglishDub => "ENDUB",
                })
                .collect();
            let effective = episode.episode_number();
            let marker = if effective != episode.number {
                format!("E{:02}→{effective} ", episode.number)
            } else {
                format!("E{:02} ", episode.number)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(BLUE)),
                Span::styled(truncate(&episode.title(), 38), Style::default().fg(TEXT)),
                Span::styled(
                    format!("  {}", languages.join("·")),
                    Style::default().fg(DIM),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(30, 34, 48))
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▍");
    frame.render_stateful_widget(list, rows[1], &mut app.episode_state);
}

fn draw_queue(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = format!("QUEUE — {} item(s)", app.queue.len());
    let block = panel(&title, app.focus == Focus::Queue);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.queue.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("  queue is empty", Style::default().fg(DIM))),
                Line::from(""),
                Line::from(Span::styled(
                    "  Enter on an episode to download",
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    "  a  queue whole season",
                    Style::default().fg(DIM),
                )),
            ]),
            inner,
        );
        return;
    }

    // Fit every column to the panel instead of letting the row overflow and
    // clip the ETA (the first thing that got cut in a real terminal).
    let width = inner.width as usize;
    let label_w = 24.min(width.saturating_sub(38)).max(10);
    let state_w = 7;
    let detail_w = 24.min(width.saturating_sub(label_w + state_w + 10)).max(8);
    let bar_w = width
        .saturating_sub(label_w + 1 + state_w + 1 + detail_w + 3)
        .clamp(4, 24);

    let items: Vec<ListItem> = app
        .queue
        .iter()
        .map(|item| {
            let (color, state) = match item.state {
                ItemState::Queued => (DIM, "queued"),
                ItemState::Running => (MINT, "running"),
                ItemState::Done => (BLUE, "done"),
                ItemState::Failed => (RED, "failed"),
            };
            let percent = item.percent();
            let (filled, empty) = progress_parts(percent, bar_w);
            let detail = match item.state {
                ItemState::Running => format!(
                    "{:>3.0}% {:>7} eta {}",
                    percent,
                    compact_rate(item.speed),
                    format_duration(item.eta())
                ),
                ItemState::Done => format_bytes(item.bytes),
                ItemState::Failed => truncate(&item.message, detail_w),
                ItemState::Queued => "waiting".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<label_w$}", truncate(&item.label, label_w)),
                    Style::default().fg(TEXT),
                ),
                Span::raw(" "),
                Span::styled("█".repeat(filled), Style::default().fg(color)),
                Span::styled(
                    "░".repeat(empty),
                    Style::default().fg(Color::Rgb(58, 66, 90)),
                ),
                Span::raw(" "),
                Span::styled(format!("{state:<state_w$}"), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    format!("{detail:<detail_w$}"),
                    Style::default().fg(DIM),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Rgb(30, 34, 48)))
        .highlight_symbol("▍");
    frame.render_stateful_widget(list, inner, &mut app.queue_state);
}

fn compact_rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1024.0 * 1024.0 {
        format!("{:.1}M/s", bytes_per_second / (1024.0 * 1024.0))
    } else if bytes_per_second >= 1024.0 {
        format!("{:.0}K/s", bytes_per_second / 1024.0)
    } else {
        format!("{:.0}B/s", bytes_per_second.max(0.0))
    }
}

fn draw_system(frame: &mut Frame, area: Rect, app: &App) {
    let block = panel("SYSTEM", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);

    let cpu = app.metrics.cpu_percent.clamp(0.0, 100.0);
    frame.render_widget(
        ratatui::widgets::LineGauge::default()
            .ratio((cpu as f64 / 100.0).clamp(0.0, 1.0))
            .label(format!("CPU  {cpu:>5.1}%"))
            .line_set(ratatui::symbols::line::THICK)
            .filled_style(Style::default().fg(if cpu > 85.0 { RED } else { MINT }))
            .unfilled_style(Style::default().fg(Color::Rgb(52, 60, 82))),
        rows[0],
    );

    let mem = app.metrics.mem_percent();
    frame.render_widget(
        ratatui::widgets::LineGauge::default()
            .ratio((mem as f64 / 100.0).clamp(0.0, 1.0))
            .label(format!(
                "MEM  {} / {}",
                format_bytes(app.metrics.mem_used),
                format_bytes(app.metrics.mem_total)
            ))
            .line_set(ratatui::symbols::line::THICK)
            .filled_style(Style::default().fg(if mem > 90.0 { RED } else { BLUE }))
            .unfilled_style(Style::default().fg(Color::Rgb(52, 60, 82))),
        rows[1],
    );

    let mut history: Vec<u64> = app.net_history.iter().copied().collect();
    if history.is_empty() {
        history.push(0);
    }
    let peak = history.iter().copied().max().unwrap_or(0).max(4096);
    let chart = Block::default()
        .title(Span::styled(
            " NETWORK · last 2 min ",
            Style::default().fg(DIM),
        ))
        .style(Style::default().bg(PANEL));
    let chart_inner = chart.inner(rows[3]);
    frame.render_widget(chart, rows[3]);
    frame.render_widget(
        Sparkline::default()
            .data(&history)
            .max(peak)
            .style(Style::default().fg(ACCENT)),
        chart_inner,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ↓ ", Style::default().fg(DIM)),
            Span::styled(
                format_rate(app.metrics.net_rx_rate),
                Style::default().fg(ACCENT),
            ),
            Span::styled("   ↑ ", Style::default().fg(DIM)),
            Span::styled(
                format_rate(app.metrics.net_tx_rate),
                Style::default().fg(BLUE),
            ),
            Span::styled(
                format!(
                    "   {} transfer(s)   {} socket(s)",
                    app.active_downloads(),
                    app.metrics.open_sockets
                ),
                Style::default().fg(DIM),
            ),
        ])),
        rows[4],
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let hints = Line::from(vec![
        Span::styled(" / ", Style::default().fg(ACCENT)),
        Span::styled("search  ", Style::default().fg(DIM)),
        Span::styled("tab ", Style::default().fg(ACCENT)),
        Span::styled("pane  ", Style::default().fg(DIM)),
        Span::styled("↵ ", Style::default().fg(ACCENT)),
        Span::styled("open/queue  ", Style::default().fg(DIM)),
        Span::styled("a ", Style::default().fg(ACCENT)),
        Span::styled("season  ", Style::default().fg(DIM)),
        Span::styled("s ", Style::default().fg(ACCENT)),
        Span::styled("next season  ", Style::default().fg(DIM)),
        Span::styled("l ", Style::default().fg(ACCENT)),
        Span::styled("language  ", Style::default().fg(DIM)),
        Span::styled("c ", Style::default().fg(ACCENT)),
        Span::styled("cancel  ", Style::default().fg(DIM)),
        Span::styled("q ", Style::default().fg(ACCENT)),
        Span::styled("quit", Style::default().fg(DIM)),
    ]);

    let status = Line::from(vec![
        Span::styled(" » ", Style::default().fg(MINT)),
        Span::styled(truncate(&app.status, area.width.saturating_sub(8) as usize), Style::default().fg(TEXT)),
    ]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(52, 58, 78)))
        .style(Style::default().bg(BG));
    frame.render_widget(Paragraph::new(vec![hints, status]).block(block), area);
}

fn progress_parts(percent: f64, width: usize) -> (usize, usize) {
    let width = width.max(4);
    let filled = (((percent / 100.0) * width as f64).round() as usize).min(width);
    (filled, width - filled)
}

#[cfg(test)]
fn progress_bar(percent: f64, width: usize) -> String {
    let (filled, empty) = progress_parts(percent, width);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn truncate(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in value.chars() {
        if out.chars().count() >= max.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_have_fixed_width() {
        assert_eq!(progress_bar(0.0, 10).chars().count(), 10);
        assert_eq!(progress_bar(100.0, 10), "██████████");
        assert_eq!(progress_bar(50.0, 10), "█████░░░░░");
        assert_eq!(progress_bar(120.0, 6), "██████");
    }

    #[test]
    fn truncation_keeps_unicode_intact() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("äöüß", 10), "äöüß");
    }

    #[test]
    fn eta_scales_with_speed() {
        let item = QueueItem {
            id: 1,
            label: "x".into(),
            state: ItemState::Running,
            done: 5,
            total: 10,
            bytes: 500,
            speed: 100.0,
            message: String::new(),
        };
        assert!((item.percent() - 50.0).abs() < f64::EPSILON);
        assert!((item.eta() - 5.0).abs() < 0.001);
    }
}
