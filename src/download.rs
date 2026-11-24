//! The download engine.
//!
//! Design goals over the Python original:
//! * segments are fetched concurrently but always written in playlist order;
//! * memory stays flat — at most `concurrency` segment buffers exist at once;
//! * partial downloads resume after a crash or a cancelled run;
//! * a token bucket enforces an optional bandwidth ceiling without extra
//!   threads, and the whole engine is cancellable from the TUI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, Mutex};

use crate::config::{Config, Language};
use crate::extract;
use crate::hls::{self, KeyMethod};
use crate::model::{Episode, Series};
use crate::mux;
use crate::naming::{self, NamingValues};
use crate::net::Http;
use crate::scrape::Scraper;

pub const EVENT_BUFFER: usize = 2048;

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Debug, Clone)]
pub enum Event {
    Stage {
        id: u64,
        text: String,
    },
    Progress {
        id: u64,
        label: String,
        done: u64,
        total: u64,
        bytes: u64,
        speed: f64,
    },
    Finished {
        id: u64,
        label: String,
        ok: bool,
        message: String,
        path: Option<PathBuf>,
    },
}

pub struct Engine {
    cfg: Arc<Config>,
    http: Arc<Http>,
    scraper: Arc<Scraper>,
    events: mpsc::Sender<Event>,
    cancel: Arc<AtomicBool>,
    limiter: Option<Arc<RateLimiter>>,
}

impl Engine {
    pub fn new(
        cfg: Arc<Config>,
        http: Arc<Http>,
        scraper: Arc<Scraper>,
        events: mpsc::Sender<Event>,
    ) -> Self {
        let limiter = cfg.bandwidth_limit.map(|rate| Arc::new(RateLimiter::new(rate as f64)));
        Self {
            cfg,
            http,
            scraper,
            events,
            cancel: Arc::new(AtomicBool::new(false)),
            limiter,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Download one episode with provider fallback, then mux it into place.
    pub async fn download_episode(
        &self,
        id: u64,
        series: &Series,
        episode: &Episode,
        output: Option<PathBuf>,
    ) -> Result<PathBuf> {
        self.download_episode_with_language(id, series, episode, output, self.cfg.language)
            .await
    }

    /// Same as [`Engine::download_episode`] but overriding the configured
    /// language, which the dashboard uses for per-queue-item choices.
    pub async fn download_episode_with_language(
        &self,
        id: u64,
        series: &Series,
        episode: &Episode,
        output: Option<PathBuf>,
        language: Language,
    ) -> Result<PathBuf> {
        let label = episode.label();
        let dest = output.unwrap_or_else(|| self.destination(series, episode, language));

        if dest.is_file() {
            let message = format!("already downloaded: {}", dest.display());
            self.finish(id, &label, true, &message, Some(dest.clone()));
            return Ok(dest);
        }

        let mut episode = episode.clone();
        if episode.providers.is_empty() {
            self.stage(id, "reading hoster list");
            self.scraper.load_providers(&mut episode).await?;
        }

        let available = episode.available_languages();
        if !available.contains(&language) {
            let names: Vec<&str> = available.iter().map(|l| l.label()).collect();
            bail!(
                "{} is not available for {} (available: {})",
                language.label(),
                label,
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        }

        let order = episode.attempt_order(language, &self.cfg.provider);
        if order.is_empty() {
            bail!("no hoster offers {language} for {label}");
        }

        let mut failures: Vec<String> = Vec::new();
        for provider in order {
            if self.is_cancelled() {
                return Err(anyhow::Error::new(Cancelled));
            }
            let Some(redirect) = episode.redirect_for(language, &provider).map(str::to_string) else {
                continue;
            };

            let attempt = async {
                self.stage(id, &format!("{provider}: resolving stream"));
                let provider_url = extract::provider_url(&self.http, &redirect, episode_url_host(&episode.url)).await?;
                let stream = extract::stream_url(&self.http, &provider_url, &provider).await?;
                self.stage(id, &format!("{provider}: downloading"));
                let fetched = self.download_stream(id, &label, &stream, &dest, Some(&provider_url)).await?;
                self.stage(id, &format!("{provider}: muxing"));
                mux::finalise(&self.cfg.ffmpeg, &fetched, &dest).await
            }
            .await;

            match attempt {
                Ok(path) => {
                    self.finish(
                        id,
                        &label,
                        true,
                        &format!("saved {}", path.display()),
                        Some(path.clone()),
                    );
                    return Ok(path);
                }
                Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                    return Err(err);
                }
                Err(err) => {
                    failures.push(format!("{provider}: {err}"));
                    self.stage(id, &format!("{provider} failed: {err}"));
                }
            }
        }

        let message = format!("all hosters failed for {label}: {}", failures.join("; "));
        self.finish(id, &label, false, &message, None);
        Err(anyhow!(message))
    }

    fn destination(&self, series: &Series, episode: &Episode, language: Language) -> PathBuf {
        destination_for(&self.cfg, series, episode, language)
    }

    /// Allow the web queue to cancel a running engine.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = flag;
    }

    /// Fetch a resolved stream into temp files. Public so the pipeline can be
    /// exercised against a local server without touching the live sites.
    pub async fn download_stream(
        &self,
        id: u64,
        label: &str,
        stream_url: &str,
        dest: &Path,
        referer: Option<&str>,
    ) -> Result<mux::Fetched> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let (temp_dir, prefix) = temp_prefix(dest, stream_url);
        tokio::fs::create_dir_all(&temp_dir).await.ok();

        let headers: Vec<(String, String)> = match referer {
            Some(referer) => vec![("Referer".to_string(), referer.to_string())],
            None => Vec::new(),
        };
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let text = self
            .http
            .text(stream_url, &header_refs)
            .await
            .context("fetching stream URL")?;

        if !text.trim_start().starts_with("#EXTM3U") {
            // A direct file (mp4/webm) rather than a playlist.
            let target = temp_dir.join(format!("{prefix}.direct"));
            self.download_direct(id, label, stream_url, &target, &header_refs)
                .await?;
            return Ok(mux::Fetched::Direct(target));
        }

        let mut video_playlist = stream_url.to_string();
        let mut audio_playlist: Option<String> = None;

        if hls::is_master(&text) {
            let (variants, renditions) = hls::parse_master(&text, stream_url)?;
            let best = hls::best_variant(&variants)
                .ok_or_else(|| anyhow!("master playlist has no variants"))?;
            video_playlist = best.uri.clone();
            if let Some(audio) =
                hls::select_audio(&renditions, &best.audio_group, self.cfg.language)
            {
                audio_playlist = audio.uri.clone();
            }
        }

        let video_path = self
            .download_media_playlist(
                id,
                label,
                &video_playlist,
                &temp_dir.join(format!("{prefix}.video")),
                &header_refs,
            )
            .await?;

        let audio_path = match audio_playlist {
            Some(uri) => Some(
                self.download_media_playlist(
                    id,
                    label,
                    &uri,
                    &temp_dir.join(format!("{prefix}.audio")),
                    &header_refs,
                )
                .await?,
            ),
            None => None,
        };

        Ok(mux::Fetched::Playlists {
            video: video_path,
            audio: audio_path,
        })
    }

    async fn download_media_playlist(
        &self,
        id: u64,
        label: &str,
        playlist_url: &str,
        base_path: &Path,
        headers: &[(&str, &str)],
    ) -> Result<PathBuf> {
        let text = self.http.text(playlist_url, headers).await?;
        if hls::is_master(&text) {
            bail!("expected a media playlist, got a master playlist");
        }
        let playlist = hls::parse_media(&text, playlist_url)?;
        let path = base_path.with_extension(&playlist.container[1..]);

        let total = playlist.segments.len() as u64;
        let mut state = PartState::default();
        if self.cfg.resume {
            if let Some(existing) = PartState::load(&path) {
                if existing.url == playlist_url && existing.segments_done <= total {
                    state = existing;
                }
            }
        }

        let mut file = open_for_append(&path, state.bytes).await?;

        let mut bytes_written = state.bytes;
        let mut done = state.segments_done;
        if done > 0 {
            self.stage(id, &format!("resuming at segment {done}/{total}"));
        }

        let fetcher = Arc::new(SegmentFetcher {
            http: self.http.clone(),
            keys: Mutex::new(HashMap::new()),
            limiter: self.limiter.clone(),
            cancel: self.cancel.clone(),
        });

        if done == 0 {
            if let Some(init) = &playlist.init {
                let data = self.http.bytes(init, headers).await.context("init segment")?;
                file.write_all(&data).await?;
                bytes_written += data.len() as u64;
            }
        }

        let remaining: Vec<hls::Segment> = playlist
            .segments
            .iter()
            .skip(done as usize)
            .cloned()
            .collect();

        let concurrency = self.cfg.concurrency.max(1);
        let base_done = done;
        let mut stream = futures::stream::iter(remaining.into_iter().enumerate())
            .map(|(offset, segment)| {
                let fetcher = fetcher.clone();
                let headers: Vec<(String, String)> = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                async move {
                    let header_refs: Vec<(&str, &str)> = headers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    let data = fetcher.fetch(&segment, &header_refs).await?;
                    Ok::<_, anyhow::Error>((base_done + offset as u64 + 1, data))
                }
            })
            .buffered(concurrency);

        let mut last_tick = Instant::now();
        let mut last_bytes = bytes_written;
        let mut last_speed = 0.0_f64;

        while let Some(item) = stream.next().await {
            if self.is_cancelled() {
                state.segments_done = done;
                state.bytes = bytes_written;
                state.total = total;
                state.url = playlist_url.to_string();
                let _ = state.save(&path);
                file.flush().await.ok();
                return Err(anyhow::Error::new(Cancelled));
            }

            let (segments_done, data) = item?;
            file.write_all(&data).await?;
            bytes_written += data.len() as u64;
            done = segments_done;

            let elapsed = last_tick.elapsed().as_secs_f64();
            if elapsed >= 0.4 {
                last_speed = (bytes_written.saturating_sub(last_bytes)) as f64 / elapsed;
                last_tick = Instant::now();
                last_bytes = bytes_written;
            }

            self.progress(id, label, done, total, bytes_written, last_speed);

            if done % 32 == 0 || done == total {
                state.segments_done = done;
                state.bytes = bytes_written;
                state.total = total;
                state.url = playlist_url.to_string();
                let _ = state.save(&path);
            }
        }

        file.flush().await?;
        drop(file);
        PartState::remove(&path);
        Ok(path)
    }

    async fn download_direct(
        &self,
        id: u64,
        label: &str,
        url: &str,
        target: &Path,
        headers: &[(&str, &str)],
    ) -> Result<()> {
        let mut response = self.http.stream(url, headers).await?;
        let total = response.content_length().unwrap_or(0);
        let mut file = BufWriter::new(File::create(target).await?);
        let mut written = 0u64;
        let mut last_tick = Instant::now();
        let mut last_bytes = 0u64;
        let mut speed = 0.0;

        while let Some(chunk) = response.chunk().await? {
            if self.is_cancelled() {
                return Err(anyhow::Error::new(Cancelled));
            }
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;

            if let Some(limiter) = &self.limiter {
                limiter.consume(chunk.len()).await;
            }
            if last_tick.elapsed().as_secs_f64() >= 0.4 {
                speed = (written - last_bytes) as f64 / last_tick.elapsed().as_secs_f64();
                last_tick = Instant::now();
                last_bytes = written;
            }
            self.progress(id, label, written, total.max(written), written, speed);
        }
        file.flush().await?;
        Ok(())
    }

    fn stage(&self, id: u64, text: &str) {
        let _ = self.events.try_send(Event::Stage {
            id,
            text: text.to_string(),
        });
    }

    fn progress(&self, id: u64, label: &str, done: u64, total: u64, bytes: u64, speed: f64) {
        let _ = self.events.try_send(Event::Progress {
            id,
            label: label.to_string(),
            done,
            total,
            bytes,
            speed,
        });
    }

    fn finish(&self, id: u64, label: &str, ok: bool, message: &str, path: Option<PathBuf>) {
        let _ = self.events.try_send(Event::Finished {
            id,
            label: label.to_string(),
            ok,
            message: message.to_string(),
            path,
        });
    }
}

/// Where an episode will be saved under the current naming template.
pub fn destination_for(cfg: &Config, series: &Series, episode: &Episode, language: Language) -> PathBuf {
    let title = if series.title_clean.is_empty() {
        series
            .url
            .rsplit('/')
            .next()
            .unwrap_or("unknown")
            .replace('-', " ")
    } else {
        series.title_clean.clone()
    };
    let values = NamingValues {
        title,
        year: series.year.clone().unwrap_or_default(),
        imdbid: series.imdb.clone().unwrap_or_default(),
        season: episode.season_number,
        episode: episode.episode_number(),
        language: language.label().to_string(),
        resolution: "unknown".to_string(),
    };
    naming::render(&cfg.download_path, &cfg.naming_template, &values)
}

fn episode_url_host(url: &str) -> &str {
    static HOSTS: [&str; 2] = ["aniworld.to", "serienstream.to"];
    HOSTS
        .iter()
        .find(|host| url.contains(**host))
        .copied()
        .unwrap_or("aniworld.to")
}

struct SegmentFetcher {
    http: Arc<Http>,
    keys: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    limiter: Option<Arc<RateLimiter>>,
    cancel: Arc<AtomicBool>,
}

impl SegmentFetcher {
    async fn fetch(&self, segment: &hls::Segment, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(Cancelled));
        }
        let data = self.http.bytes(&segment.uri, headers).await?;
        let Some(key) = &segment.key else {
            if let Some(limiter) = &self.limiter {
                limiter.consume(data.len()).await;
            }
            return Ok(data.to_vec());
        };
        if key.method != KeyMethod::Aes128 {
            bail!("unsupported key method");
        }
        let uri = key
            .uri
            .as_ref()
            .ok_or_else(|| anyhow!("AES-128 key without URI"))?;

        let key_bytes = {
            let mut cache = self.keys.lock().await;
            match cache.get(uri) {
                Some(cached) => cached.clone(),
                None => {
                    let bytes = self.http.bytes(uri, headers).await?;
                    if bytes.len() != 16 {
                        bail!("AES key has {} bytes, expected 16", bytes.len());
                    }
                    let shared = Arc::new(bytes.to_vec());
                    cache.insert(uri.clone(), shared.clone());
                    shared
                }
            }
        };

        let iv = hls::resolve_iv(key.iv.as_deref(), segment.sequence);
        let plain = hls::decrypt_aes128(&data, &key_bytes, &iv)?;
        if let Some(limiter) = &self.limiter {
            limiter.consume(plain.len()).await;
        }
        Ok(plain)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PartState {
    url: String,
    segments_done: u64,
    bytes: u64,
    total: u64,
}

impl PartState {
    fn path(media: &Path) -> PathBuf {
        let name = media
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("part");
        media.with_file_name(format!("{name}.part.json"))
    }

    fn load(media: &Path) -> Option<Self> {
        let data = std::fs::read(Self::path(media)).ok()?;
        serde_json::from_slice(&data).ok()
    }

    fn save(&self, media: &Path) -> std::io::Result<()> {
        let data = serde_json::to_vec(self)?;
        std::fs::write(Self::path(media), data)
    }

    fn remove(media: &Path) {
        let _ = std::fs::remove_file(Self::path(media));
    }
}

async fn open_for_append(path: &Path, resume_bytes: u64) -> Result<BufWriter<File>> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    file.set_len(resume_bytes).await?;
    file.seek(std::io::SeekFrom::Start(resume_bytes)).await?;
    Ok(BufWriter::with_capacity(256 * 1024, file))
}

/// Directory and file prefix used for an episode's temporary payloads.
pub fn temp_prefix(dest: &Path, stream_url: &str) -> (PathBuf, String) {
    let temp_dir = dest
        .parent()
        .unwrap_or(Path::new("."))
        .join(".aniworld-tmp");
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    (temp_dir, format!("{stem}-{}", short_hash(stream_url)))
}

fn short_hash(value: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Simple token bucket: one second of burst, then a steady ceiling.
pub struct RateLimiter {
    rate: f64,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(rate: f64) -> Self {
        Self {
            rate,
            state: Mutex::new(LimiterState {
                tokens: rate,
                last: Instant::now(),
            }),
        }
    }

    pub async fn consume(&self, bytes: usize) {
        let wait = {
            let mut state = self.state.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(state.last).as_secs_f64();
            state.tokens = (state.tokens + elapsed * self.rate).min(self.rate);
            state.last = now;
            state.tokens -= bytes as f64;
            if state.tokens < 0.0 {
                -state.tokens / self.rate
            } else {
                0.0
            }
        };
        if wait > 0.0 {
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("episode.video.ts");
        let state = PartState {
            url: "https://cdn/index.m3u8".into(),
            segments_done: 12,
            bytes: 3456,
            total: 100,
        };
        state.save(&media).unwrap();
        let loaded = PartState::load(&media).unwrap();
        assert_eq!(loaded.segments_done, 12);
        assert_eq!(loaded.bytes, 3456);
        PartState::remove(&media);
        assert!(PartState::load(&media).is_none());
    }

    #[tokio::test]
    async fn rate_limiter_delays_when_exhausted() {
        let limiter = RateLimiter::new(10_000.0);
        let start = Instant::now();
        limiter.consume(5_000).await;
        limiter.consume(10_000).await;
        assert!(start.elapsed().as_millis() >= 400);
    }

    #[test]
    fn hashes_are_stable() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
    }
}
