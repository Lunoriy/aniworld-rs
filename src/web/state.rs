//! Shared web state: persisted settings, the download queue and the autosync
//! watchlist.
//!
//! The queue deliberately builds one [`Engine`] per episode from a snapshot of
//! the current settings. That keeps a running download stable when the user
//! edits settings, and gives every job its own cancel flag.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};

use crate::config::{self, Config, Language};
use crate::download::{self, Cancelled, Engine, Event, EVENT_BUFFER};
use crate::model::{Episode, Series, Target};
use crate::net::Http;
use crate::scrape::Scraper;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Web-editable view of [`Config`]. Everything the original settings page could
/// change is here, and it round-trips through JSON on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub download_path: String,
    pub language: String,
    pub provider: String,
    pub concurrency: usize,
    pub retries: u32,
    pub bandwidth: String,
    pub naming_template: String,
    pub ffmpeg: String,
    pub jobs: usize,
    pub resume: bool,
    pub autosync_enabled: bool,
    pub autosync_interval: u64,
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            download_path: cfg.download_path.display().to_string(),
            language: cfg.language.label().to_string(),
            provider: cfg.provider.clone(),
            concurrency: cfg.concurrency,
            retries: cfg.retries,
            bandwidth: cfg
                .bandwidth_limit
                .map(|b| format!("{}k", b / 1024))
                .unwrap_or_default(),
            naming_template: cfg.naming_template.clone(),
            ffmpeg: cfg.ffmpeg.clone(),
            jobs: cfg.jobs,
            resume: cfg.resume,
            autosync_enabled: false,
            autosync_interval: 30,
        }
    }

    /// Validate and merge user input. Unknown fields are rejected by serde, so
    /// a typo cannot silently reset something.
    pub fn apply(&mut self, patch: &serde_json::Value) -> Result<()> {
        if let Some(value) = patch.get("download_path").and_then(|v| v.as_str()) {
            let path = config::expand_home(value.trim());
            if value.trim().is_empty() {
                return Err(anyhow!("download path cannot be empty"));
            }
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            self.download_path = path.display().to_string();
        }
        if let Some(value) = patch.get("language").and_then(|v| v.as_str()) {
            let lang = Language::parse(value).ok_or_else(|| anyhow!("unknown language '{value}'"))?;
            self.language = lang.label().to_string();
        }
        if let Some(value) = patch.get("provider").and_then(|v| v.as_str()) {
            if value.trim().is_empty() {
                return Err(anyhow!("provider cannot be empty"));
            }
            self.provider = value.trim().to_string();
        }
        if let Some(value) = patch.get("concurrency").and_then(|v| v.as_u64()) {
            self.concurrency = (value as usize).clamp(1, 64);
        }
        if let Some(value) = patch.get("retries").and_then(|v| v.as_u64()) {
            self.retries = (value as u32).clamp(1, 20);
        }
        if let Some(value) = patch.get("jobs").and_then(|v| v.as_u64()) {
            self.jobs = (value as usize).clamp(1, 16);
        }
        if let Some(value) = patch.get("bandwidth").and_then(|v| v.as_str()) {
            if !value.trim().is_empty() && config::parse_bandwidth(value).is_none() {
                return Err(anyhow!("bandwidth must look like 20M, 512k or be empty"));
            }
            self.bandwidth = value.trim().to_string();
        }
        if let Some(value) = patch.get("naming_template").and_then(|v| v.as_str()) {
            if value.trim().is_empty() {
                return Err(anyhow!("naming template cannot be empty"));
            }
            self.naming_template = value.trim().to_string();
        }
        if let Some(value) = patch.get("ffmpeg").and_then(|v| v.as_str()) {
            if value.trim().is_empty() {
                return Err(anyhow!("ffmpeg path cannot be empty"));
            }
            self.ffmpeg = value.trim().to_string();
        }
        if let Some(value) = patch.get("resume").and_then(|v| v.as_bool()) {
            self.resume = value;
        }
        if let Some(value) = patch.get("autosync_enabled").and_then(|v| v.as_bool()) {
            self.autosync_enabled = value;
        }
        if let Some(value) = patch.get("autosync_interval").and_then(|v| v.as_u64()) {
            self.autosync_interval = value.clamp(5, 24 * 60);
        }
        Ok(())
    }

    pub fn to_config(&self) -> Result<Config> {
        let mut cfg = Config::from_env();
        cfg.download_path = config::expand_home(&self.download_path);
        cfg.language = Language::parse(&self.language)
            .ok_or_else(|| anyhow!("unknown language '{}'", self.language))?;
        cfg.provider = self.provider.clone();
        cfg.concurrency = self.concurrency.clamp(1, 64);
        cfg.retries = self.retries.clamp(1, 20);
        cfg.bandwidth_limit = config::parse_bandwidth(&self.bandwidth);
        cfg.naming_template = self.naming_template.clone();
        cfg.ffmpeg = self.ffmpeg.clone();
        cfg.jobs = self.jobs.clamp(1, 16);
        cfg.resume = self.resume;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Watched {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub enabled: bool,
    pub last_checked: Option<u64>,
    pub last_added: u64,
    pub last_note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    settings: Option<Settings>,
    #[serde(default)]
    watched: Vec<Watched>,
    #[serde(default)]
    next_watch_id: u64,
}

fn write_store(path: &Path, store: &StoreFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(store)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

pub struct Job {
    pub id: u64,
    pub title: String,
    pub label: String,
    pub season: u32,
    pub number: u32,
    pub language: Language,
    pub provider: String,
    pub state: JobState,
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
    pub speed: f64,
    pub path: Option<String>,
    pub error: Option<String>,
    pub created: u64,
    pub finished: Option<u64>,
    pub cancel: Arc<AtomicBool>,
    pub series: Series,
    pub episode: Episode,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: u64,
    pub title: String,
    pub label: String,
    pub season: u32,
    pub number: u32,
    pub language: String,
    pub provider: String,
    pub state: JobState,
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
    pub speed: f64,
    pub percent: f64,
    pub eta: f64,
    pub path: Option<String>,
    pub error: Option<String>,
    pub created: u64,
    pub finished: Option<u64>,
}

impl Job {
    pub fn view(&self) -> JobView {
        let percent = if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64 * 100.0).clamp(0.0, 100.0)
        };
        let eta = if self.speed <= 0.0 || self.total == 0 || self.done == 0 {
            0.0
        } else {
            let per_segment = self.bytes as f64 / self.done.max(1) as f64;
            (self.total.saturating_sub(self.done) as f64 * per_segment) / self.speed
        };
        JobView {
            id: self.id,
            title: self.title.clone(),
            label: self.label.clone(),
            season: self.season,
            number: self.number,
            language: self.language.label().to_string(),
            provider: self.provider.clone(),
            state: self.state,
            stage: self.stage.clone(),
            done: self.done,
            total: self.total,
            bytes: self.bytes,
            speed: self.speed,
            percent,
            eta,
            path: self.path.clone(),
            error: self.error.clone(),
            created: self.created,
            finished: self.finished,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct QueueCounts {
    pub queued: u64,
    pub running: u64,
    pub done: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueSnapshot {
    pub counts: QueueCounts,
    pub speed: f64,
    pub jobs: Vec<JobView>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AutosyncReport {
    pub started: u64,
    pub finished: u64,
    pub series: u64,
    pub queued: u64,
    pub notes: Vec<String>,
}

pub struct Web {
    pub http: Arc<Http>,
    pub scraper: Arc<Scraper>,
    pub store_path: PathBuf,
    pub settings: RwLock<Settings>,
    pub jobs: Mutex<Vec<Job>>,
    pub next_id: AtomicU64,
    pub events: mpsc::Sender<Event>,
    pub watched: Mutex<Vec<Watched>>,
    pub next_watch_id: AtomicU64,
    pub sem: RwLock<Arc<Semaphore>>,
    /// Serialises job starts so queue order is preserved (a bare semaphore
    /// lets a later episode grab the free permit first).
    start_chain: Mutex<Option<oneshot::Receiver<()>>>,
    pub autosync_running: AtomicBool,
    pub autosync_last: Mutex<Option<AutosyncReport>>,
    pub autosync_last_run: Mutex<u64>,
    pub started: u64,
    pub version: &'static str,
}

impl Web {
    pub fn new(cfg: Config, version: &'static str, events: mpsc::Sender<Event>) -> Result<Arc<Self>> {
        let store_path = std::env::var("ANIWORLD_WEB_STATE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| config::expand_home(s.trim()))
            .unwrap_or_else(|| {
                config::home_dir().join(".config/aniworld-rs/web.json")
            });

        let raw = std::fs::read_to_string(&store_path).unwrap_or_default();
        let store = serde_json::from_str::<StoreFile>(&raw).unwrap_or_default();

        // Older builds stored an Argon2 hash for the login screen. Auth is gone;
        // rewrite the file once so the dead credential leaves the disk.
        if raw.contains("\"hash\"") || raw.contains("\"salt\"") {
            let cleaned = StoreFile {
                settings: store.settings.clone(),
                watched: store.watched.clone(),
                next_watch_id: store.next_watch_id,
            };
            let _ = write_store(&store_path, &cleaned);
        }

        let mut settings = store
            .settings
            .clone()
            .unwrap_or_else(|| Settings::from_config(&cfg));
        if settings.autosync_interval < 5 {
            settings.autosync_interval = 30;
        }
        let next_watch_id = store
            .next_watch_id
            .max(store.watched.iter().map(|w| w.id + 1).max().unwrap_or(1));

        let http = Arc::new(Http::new(&cfg.user_agent, cfg.retries)?);
        let scraper = Arc::new(Scraper::new((*http).clone()));
        Ok(Arc::new(Self {
            http,
            scraper,
            store_path,
            sem: RwLock::new(Arc::new(Semaphore::new(settings.jobs.max(1)))),
            start_chain: Mutex::new(None),
            settings: RwLock::new(settings),
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            events,
            watched: Mutex::new(store.watched),
            next_watch_id: AtomicU64::new(next_watch_id),
            autosync_running: AtomicBool::new(false),
            autosync_last: Mutex::new(None),
            autosync_last_run: Mutex::new(0),
            started: now(),
            version,
        }))
    }

    pub async fn save(&self) -> Result<()> {
        let store = StoreFile {
            settings: Some(self.settings.read().await.clone()),
            watched: self.watched.lock().await.clone(),
            next_watch_id: self.next_watch_id.load(Ordering::SeqCst),
        };
        write_store(&self.store_path, &store)
    }

    pub async fn set_jobs(&self, jobs: usize) {
        let jobs = jobs.clamp(1, 16);
        *self.sem.write().await = Arc::new(Semaphore::new(jobs));
    }

    /// Queue every episode, honouring the current `jobs` limit.
    pub async fn enqueue(
        self: &Arc<Self>,
        series: Series,
        episodes: Vec<Episode>,
        language: Language,
        provider: String,
    ) -> Vec<u64> {
        let cfg = Arc::new(
            self.settings
                .read()
                .await
                .to_config()
                .unwrap_or_else(|_| Config::from_env()),
        );
        let mut ids = Vec::new();
        for episode in episodes {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let cancel = Arc::new(AtomicBool::new(false));
            let (start_tx, start_rx) = oneshot::channel::<()>();
            let previous = {
                let mut chain = self.start_chain.lock().await;
                chain.replace(start_rx)
            };
            let (tx, mut rx) = mpsc::channel::<Event>(EVENT_BUFFER);
            let mut engine = Engine::new(
                cfg.clone(),
                self.http.clone(),
                self.scraper.clone(),
                tx,
            );
            engine.set_cancel_flag(cancel.clone());
            let engine = Arc::new(engine);

            let job = Job {
                id,
                title: series.title_clean.clone(),
                label: episode.label(),
                season: episode.season_number,
                number: episode.episode_number(),
                language,
                provider: provider.clone(),
                state: JobState::Queued,
                stage: "queued".into(),
                done: 0,
                total: 0,
                bytes: 0,
                speed: 0.0,
                path: None,
                error: None,
                created: now(),
                finished: None,
                cancel: cancel.clone(),
                series: series.clone(),
                episode: episode.clone(),
            };
            self.jobs.lock().await.push(job);
            ids.push(id);

            let web = self.clone();
            let series = series.clone();
            let episode = episode.clone();
            let sem = self.sem.read().await.clone();
            tokio::spawn(async move {
                // Wait for the episode queued before this one to take its
                // permit, then take ours; that keeps starts in queue order.
                if let Some(previous) = previous {
                    let _ = previous.await;
                }
                let permit = match sem.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return,
                };
                let _ = start_tx.send(());
                if cancel.load(Ordering::SeqCst) {
                    web.mark(id, |job| {
                        job.state = JobState::Cancelled;
                        job.stage = "cancelled".into();
                        job.finished = Some(now());
                    })
                    .await;
                    return;
                }
                web.mark(id, |job| {
                    job.state = JobState::Running;
                    job.stage = "starting".into();
                })
                .await;

                let relay = {
                    let web = web.clone();
                    tokio::spawn(async move {
                        while let Some(event) = rx.recv().await {
                            web.apply_event(event).await;
                        }
                    })
                };
                let result = engine
                    .download_episode_with_language(id, &series, &episode, None, language)
                    .await;
                relay.await.ok();

                match result {
                    Ok(path) => {
                        let path = path.display().to_string();
                        web.mark(id, |job| {
                            job.state = JobState::Done;
                            job.stage = "done".into();
                            job.path = Some(path.clone());
                            job.speed = 0.0;
                            job.finished = Some(now());
                            if job.total == 0 {
                                job.total = 1;
                                job.done = 1;
                            }
                        })
                        .await;
                    }
                    Err(err) => {
                        let cancelled = err.downcast_ref::<Cancelled>().is_some()
                            || cancel.load(Ordering::SeqCst);
                        let message = format!("{err:#}");
                        web.mark(id, |job| {
                            job.state = if cancelled {
                                JobState::Cancelled
                            } else {
                                JobState::Failed
                            };
                            job.stage = if cancelled { "cancelled" } else { "failed" }.into();
                            job.error = if cancelled { None } else { Some(message.clone()) };
                            job.speed = 0.0;
                            job.finished = Some(now());
                        })
                        .await;
                    }
                }
                drop(permit);
            });
        }
        ids
    }

    pub async fn mark(&self, id: u64, change: impl FnOnce(&mut Job)) {
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
            change(job);
        }
    }

    pub(crate) async fn apply_event(&self, event: Event) {
        let mut jobs = self.jobs.lock().await;
        let Some(job) = jobs.iter_mut().find(|job| {
            matches!(
                &event,
                Event::Stage { id, .. } | Event::Progress { id, .. } | Event::Finished { id, .. }
                    if *id == job.id
            )
        }) else {
            return;
        };
        match event {
            Event::Stage { text, .. } => job.stage = text,
            Event::Progress {
                done,
                total,
                bytes,
                speed,
                ..
            } => {
                job.state = JobState::Running;
                job.done = done;
                job.total = total;
                job.bytes = bytes;
                job.speed = speed;
            }
            Event::Finished {
                ok,
                message,
                path,
                ..
            } => {
                job.state = if ok { JobState::Done } else { JobState::Failed };
                job.stage = if ok { "done" } else { "failed" }.into();
                job.path = path.map(|p| p.display().to_string());
                job.error = if ok { None } else { Some(message) };
                job.speed = 0.0;
                job.finished = Some(now());
                if job.total == 0 {
                    job.total = 1;
                    job.done = 1;
                }
            }
        }
    }

    pub async fn cancel(&self, id: u64) {
        let jobs = self.jobs.lock().await;
        if let Some(job) = jobs.iter().find(|job| job.id == id) {
            job.cancel.store(true, Ordering::SeqCst);
            if job.state == JobState::Queued {
                drop(jobs);
                self.mark(id, |job| {
                    job.state = JobState::Cancelled;
                    job.stage = "cancelled".into();
                    job.finished = Some(now());
                })
                .await;
            }
        }
    }

    pub async fn retry(self: &Arc<Self>, id: u64) -> bool {
        let job = {
            let mut jobs = self.jobs.lock().await;
            let Some(index) = jobs.iter().position(|job| job.id == id) else {
                return false;
            };
            let job = jobs.remove(index);
            if !matches!(
                job.state,
                JobState::Failed | JobState::Cancelled | JobState::Done
            ) {
                jobs.insert(index, job);
                return false;
            }
            if let Some(path) = &job.path {
                let path = PathBuf::from(path);
                if path.is_file() {
                    jobs.insert(index, job);
                    return false;
                }
            }
            job
        };
        let ids = self
            .enqueue(
                job.series,
                vec![job.episode],
                job.language,
                job.provider,
            )
            .await;
        !ids.is_empty()
    }

    pub async fn remove(&self, id: u64) {
        self.cancel(id).await;
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|job| job.id != id);
    }

    pub async fn clear_finished(&self) {
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|job| matches!(job.state, JobState::Queued | JobState::Running));
    }

    pub async fn snapshot(&self) -> QueueSnapshot {
        let jobs = self.jobs.lock().await;
        let mut counts = QueueCounts::default();
        let mut speed = 0.0;
        for job in jobs.iter() {
            match job.state {
                JobState::Queued => counts.queued += 1,
                JobState::Running => {
                    counts.running += 1;
                    speed += job.speed;
                }
                JobState::Done => counts.done += 1,
                JobState::Failed => counts.failed += 1,
                JobState::Cancelled => counts.cancelled += 1,
            }
        }
        let mut views: Vec<JobView> = jobs.iter().map(Job::view).collect();
        views.sort_by_key(|job| job.id);
        QueueSnapshot {
            counts,
            speed,
            jobs: views,
        }
    }

    /// One autosync pass: check the newest season of every watched series and
    /// queue missing episodes (capped, so a first run cannot flood the disk).
    pub async fn autosync_once(self: &Arc<Self>) -> AutosyncReport {
        if self
            .autosync_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return AutosyncReport::default();
        }
        let mut report = AutosyncReport {
            started: now(),
            ..Default::default()
        };
        let settings = self.settings.read().await.clone();
        let cfg = match settings.to_config() {
            Ok(cfg) => cfg,
            Err(err) => {
                report.notes.push(format!("settings: {err}"));
                self.autosync_running.store(false, Ordering::SeqCst);
                return report;
            }
        };
        let language = cfg.language;

        let watched = self.watched.lock().await.clone();
        for entry in watched.into_iter().filter(|w| w.enabled) {
            report.series += 1;
            let target = match Target::parse(&entry.url) {
                Ok(target) => target,
                Err(err) => {
                    report.notes.push(format!("{}: {err}", entry.title));
                    continue;
                }
            };
            let series = match self.scraper.series(&target).await {
                Ok(series) => series,
                Err(err) => {
                    report.notes.push(format!("{}: {err}", entry.title));
                    continue;
                }
            };
            let season = series
                .seasons
                .iter()
                .filter(|s| !s.are_movies && !s.episodes.is_empty())
                .max_by_key(|s| s.number)
                .cloned();
            let Some(season) = season else {
                report
                    .notes
                    .push(format!("{}: no regular season found", entry.title));
                self.touch_watch(entry.id, 0, "no seasons").await;
                continue;
            };
            let loaded = match self
                .scraper
                .season(&season.url, series.site, season.number, false)
                .await
            {
                Ok(season) => season,
                Err(err) => {
                    report.notes.push(format!("{}: {err}", entry.title));
                    continue;
                }
            };
            let mut missing = Vec::new();
            for episode in loaded.episodes {
                let dest = download::destination_for(&cfg, &series, &episode, language);
                if !dest.is_file() && missing.len() < 5 {
                    missing.push(episode);
                }
            }
            let added = if missing.is_empty() {
                0
            } else {
                let count = missing.len() as u64;
                let this = Arc::clone(self);
                let ids = this
                    .enqueue(
                        series.clone(),
                        missing,
                        language,
                        settings.provider.clone(),
                    )
                    .await;
                report.queued += ids.len() as u64;
                count.min(ids.len() as u64)
            };
            let note = if added > 0 {
                format!("queued {added} new episode(s)")
            } else {
                "up to date".to_string()
            };
            self.touch_watch(entry.id, added, &note).await;
            report.notes.push(format!("{}: {note}", series.title_clean));
        }

        report.finished = now();
        *self.autosync_last.lock().await = Some(report.clone());
        *self.autosync_last_run.lock().await = report.finished;
        self.autosync_running.store(false, Ordering::SeqCst);
        report
    }

    async fn touch_watch(&self, id: u64, added: u64, note: &str) {
        let mut watched = self.watched.lock().await;
        if let Some(entry) = watched.iter_mut().find(|w| w.id == id) {
            entry.last_checked = Some(now());
            entry.last_added = added;
            entry.last_note = note.to_string();
        }
    }

    pub async fn add_watch(self: &Arc<Self>, url: &str) -> Result<Watched> {
        let target = Target::parse(url)?;
        let canonical = target.url().to_string();
        if let Some(existing) = self
            .watched
            .lock()
            .await
            .iter()
            .find(|w| w.url == canonical)
        {
            return Ok(existing.clone());
        }
        let series = self.scraper.series(&target).await?;
        let entry = Watched {
            id: self.next_watch_id.fetch_add(1, Ordering::SeqCst),
            url: canonical,
            title: series.title_clean,
            enabled: true,
            last_checked: None,
            last_added: 0,
            last_note: "not checked yet".into(),
        };
        self.watched.lock().await.push(entry.clone());
        self.save().await?;
        Ok(entry)
    }

    pub async fn remove_watch(&self, id: u64) {
        self.watched.lock().await.retain(|w| w.id != id);
        let _ = self.save().await;
    }

    /// Seconds until the next automatic pass, or `None` when disabled.
    pub async fn next_autosync(&self) -> Option<u64> {
        let settings = self.settings.read().await;
        if !settings.autosync_enabled {
            return None;
        }
        let last = *self.autosync_last_run.lock().await;
        let interval = settings.autosync_interval.max(5) * 60;
        let due = if last == 0 { now() } else { last + interval };
        Some(due.saturating_sub(now()))
    }
}

/// Path containment check shared by library deletion and streaming.
pub fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = relative.trim_start_matches('/');
    if relative.is_empty() || relative.contains('\0') {
        return None;
    }
    let candidate = root.join(relative);
    let canonical_root = root.canonicalize().ok()?;
    let canonical = candidate.canonicalize().ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::from_config(&Config::default())
    }

    #[test]
    fn rejects_unknown_language_and_empty_provider() {
        let mut s = settings();
        assert!(s
            .apply(&serde_json::json!({"language": "klingon"}))
            .is_err());
        assert!(s.apply(&serde_json::json!({"provider": "  "})).is_err());
        assert!(s
            .apply(&serde_json::json!({"bandwidth": "lots"}))
            .is_err());
        assert!(s.apply(&serde_json::json!({"jobs": 999})).is_ok());
        assert_eq!(s.jobs, 16);
    }

    #[test]
    fn library_paths_cannot_escape_the_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("Show/Season 1")).unwrap();
        std::fs::write(temp.path().join("Show/Season 1/ep.mkv"), b"x").unwrap();
        assert!(safe_join(temp.path(), "Show/Season 1/ep.mkv").is_some());
        assert!(safe_join(temp.path(), "../etc/passwd").is_none());
        assert!(safe_join(temp.path(), "/etc/passwd").is_none());
        assert!(safe_join(temp.path(), "Show/../Show/Season 1/ep.mkv").is_some());
    }
}
