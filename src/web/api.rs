//! JSON API used by the pages.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::config::Language;
use crate::model::{SearchHit, Site, Target};
use crate::web::{library, state};

use super::state::Web;

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"ok": false, "error": message.into()})),
    )
        .into_response()
}

fn ok(value: Value) -> Response {
    Json(value).into_response()
}

pub async fn health() -> Response {
    ok(json!({"ok": true}))
}

pub async fn status(State(web): State<Arc<Web>>) -> Response {
    let snapshot = web.snapshot().await;
    let settings = web.settings.read().await.clone();
    let next = web.next_autosync().await;
    ok(json!({
        "ok": true,
        "version": web.version,
        "started": web.started,
        "counts": snapshot.counts,
        "speed": snapshot.speed,
        "autosync": {
            "enabled": settings.autosync_enabled,
            "interval": settings.autosync_interval,
            "running": web.autosync_running.load(std::sync::atomic::Ordering::SeqCst),
            "next_in": next,
        },
        "settings": {
            "language": settings.language,
            "provider": settings.provider,
            "jobs": settings.jobs,
            "download_path": settings.download_path,
        }
    }))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub site: Option<String>,
}

pub async fn search(
    State(web): State<Arc<Web>>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let term = query.q.trim().to_string();
    if term.len() < 2 {
        return fail(StatusCode::BAD_REQUEST, "type at least two characters");
    }
    let sites = match query.site.as_deref().unwrap_or("all") {
        "aniworld" => vec![Site::AniWorld],
        "sto" | "serienstream" => vec![Site::SerienStream],
        _ => vec![Site::AniWorld, Site::SerienStream],
    };
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for site in sites {
        match web.scraper.search(site, &term).await {
            Ok(found) => hits.extend(found),
            Err(err) => errors.push(format!("{}: {err}", site.name())),
        }
    }
    let mut seen = std::collections::HashSet::new();
    hits.retain(|hit| seen.insert(hit.url.clone()));
    ok(json!({"ok": true, "query": term, "hits": hits, "errors": errors}))
}

#[derive(Debug, Deserialize)]
pub struct UrlQuery {
    pub url: String,
}

pub async fn series(State(web): State<Arc<Web>>, Query(query): Query<UrlQuery>) -> Response {
    let target = match Target::parse(&query.url) {
        Ok(target) => target,
        Err(err) => return fail(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let series_target = match Target::parse(&format!(
        "{}{}{}",
        target.site().base(),
        target.site().series_prefix(),
        target.slug()
    )) {
        Ok(target) => target,
        Err(err) => return fail(StatusCode::BAD_REQUEST, err.to_string()),
    };
    match web.scraper.series(&series_target).await {
        Ok(series) => ok(json!({"ok": true, "series": series})),
        Err(err) => fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    }
}

pub async fn season(State(web): State<Arc<Web>>, Query(query): Query<UrlQuery>) -> Response {
    let target = match Target::parse(&query.url) {
        Ok(target) => target,
        Err(err) => return fail(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let (url, site, number, movies) = match &target {
        Target::Season {
            url,
            site,
            season,
            movies,
            ..
        } => (url.clone(), *site, *season, *movies),
        Target::Episode {
            url: _,
            site,
            season,
            movies,
            ..
        } => {
            let season_url = query
                .url
                .rsplit_once('/')
                .map(|(base, _)| base.to_string())
                .unwrap_or_else(|| query.url.clone());
            (season_url, *site, *season, *movies)
        }
        Target::Series { .. } => {
            return fail(
                StatusCode::BAD_REQUEST,
                "pick a season URL, e.g. .../staffel-1",
            )
        }
    };
    match web.scraper.season(&url, site, number, movies).await {
        Ok(season) => {
            let episodes: Vec<Value> = season
                .episodes
                .iter()
                .map(|episode| {
                    json!({
                        "number": episode.number,
                        "absolute": episode.episode_number(),
                        "label": episode.label(),
                        "title": episode.title(),
                        "title_en": episode.title_en,
                        "title_de": episode.title_de,
                        "url": episode.url,
                        "languages": episode.available_languages().iter().map(|l| l.label()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            ok(json!({
                "ok": true,
                "url": season.url,
                "number": season.number,
                "movies": season.are_movies,
                "episodes": episodes,
            }))
        }
        Err(err) => fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    #[serde(default)]
    pub episodes: Option<Vec<u32>>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub all: bool,
}

pub async fn download(
    State(web): State<Arc<Web>>,
    Json(body): Json<DownloadRequest>,
) -> Response {
    let target = match Target::parse(&body.url) {
        Ok(target) => target,
        Err(err) => return fail(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let settings = web.settings.read().await.clone();
    let language = match body.language.as_deref().map(Language::parse) {
        Some(Some(language)) => language,
        Some(None) => return fail(StatusCode::BAD_REQUEST, "unknown language"),
        None => match Language::parse(&settings.language) {
            Some(language) => language,
            None => return fail(StatusCode::INTERNAL_SERVER_ERROR, "invalid stored language"),
        },
    };
    let provider = body
        .provider
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(settings.provider.clone());

    let series_target = match Target::parse(&format!(
        "{}{}{}",
        target.site().base(),
        target.site().series_prefix(),
        target.slug()
    )) {
        Ok(target) => target,
        Err(err) => return fail(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let series = match web.scraper.series(&series_target).await {
        Ok(series) => series,
        Err(err) => return fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    };

    let (season_url, season_number, movies) = match &target {
        Target::Season {
            url,
            season,
            movies,
            ..
        } => (url.clone(), *season, *movies),
        Target::Episode {
            site,
            season,
            movies,
            ..
        } => {
            let Some(known) = series
                .seasons
                .iter()
                .find(|s| s.number == *season && s.are_movies == *movies)
            else {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("season {season} is not listed for {}", series.title_clean),
                );
            };
            let _ = site;
            (known.url.clone(), *season, *movies)
        }
        Target::Series { .. } => {
            let Some(latest) = series
                .seasons
                .iter()
                .filter(|s| !s.are_movies)
                .max_by_key(|s| s.number)
            else {
                return fail(StatusCode::BAD_REQUEST, "this series has no seasons");
            };
            (latest.url.clone(), latest.number, latest.are_movies)
        }
    };

    let season = match web
        .scraper
        .season(&season_url, target.site(), season_number, movies)
        .await
    {
        Ok(season) => season,
        Err(err) => return fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    };

    let wanted: Option<Vec<u32>> = match &target {
        Target::Episode { number, .. } => Some(vec![*number]),
        _ if body.all => None,
        _ => body.episodes.clone(),
    };
    let offered: Vec<&'static str> = season
        .episodes
        .first()
        .map(|episode| {
            episode
                .available_languages()
                .iter()
                .map(|language| language.label())
                .collect()
        })
        .unwrap_or_default();
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for episode in season.episodes {
        if let Some(numbers) = &wanted {
            if !numbers.contains(&episode.number) {
                continue;
            }
        }
        let available = episode.available_languages();
        if !available.is_empty() && !available.contains(&language) {
            skipped.push(format!(
                "{} (no {})",
                episode.label(),
                language.label()
            ));
            continue;
        }
        selected.push(episode);
    }
    if selected.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            format!(
                "no episode offers {}. Available: {}",
                language.label(),
                if offered.is_empty() {
                    "unknown".to_string()
                } else {
                    offered.join(", ")
                }
            ),
        );
    }

    let count = selected.len();
    let ids = web
        .enqueue(series.clone(), selected, language, provider)
        .await;
    ok(json!({
        "ok": true,
        "queued": ids.len(),
        "ids": ids,
        "skipped": skipped,
        "episodes": count,
        "series": series.title_clean,
    }))
}

pub async fn queue(State(web): State<Arc<Web>>) -> Response {
    ok(serde_json::to_value(web.snapshot().await).unwrap_or_else(|_| json!({})))
}

pub async fn queue_events(
    State(web): State<Arc<Web>>,
) -> Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = futures::stream::unfold(web, |web| async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let snapshot = web.snapshot().await;
        let data = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into());
        let event = SseEvent::default().event("queue").data(data);
        Some((Ok::<_, Infallible>(event), web))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn queue_cancel(State(web): State<Arc<Web>>, Path(id): Path<u64>) -> Response {
    web.cancel(id).await;
    ok(json!({"ok": true}))
}

pub async fn queue_retry(State(web): State<Arc<Web>>, Path(id): Path<u64>) -> Response {
    if web.retry(id).await {
        ok(json!({"ok": true}))
    } else {
        fail(
            StatusCode::BAD_REQUEST,
            "only finished items can be retried (and not ones already on disk)",
        )
    }
}

pub async fn queue_remove(State(web): State<Arc<Web>>, Path(id): Path<u64>) -> Response {
    web.remove(id).await;
    ok(json!({"ok": true}))
}

pub async fn queue_clear(State(web): State<Arc<Web>>) -> Response {
    web.clear_finished().await;
    ok(json!({"ok": true}))
}

pub async fn library_index(State(web): State<Arc<Web>>) -> Response {
    let cfg = match web.settings.read().await.to_config() {
        Ok(cfg) => cfg,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let library = library::scan(&cfg);
    ok(serde_json::to_value(library).unwrap_or_else(|_| json!({})))
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub path: String,
}

pub async fn library_delete(
    State(web): State<Arc<Web>>,
    Json(body): Json<DeleteRequest>,
) -> Response {
    let cfg = match web.settings.read().await.to_config() {
        Ok(cfg) => cfg,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    if let Err(err) = library::delete(&cfg.download_path, &body.path) {
        return fail(StatusCode::BAD_REQUEST, err.to_string());
    }
    ok(json!({"ok": true, "deleted": body.path}))
}

#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    pub url: String,
}

/// Posters live on the site's CDN, which blocks cross-origin browser requests
/// (ORB) and hotlinking. Fetch them server-side and cache for a day.
pub async fn image(State(web): State<Arc<Web>>, Query(query): Query<ImageQuery>) -> Response {
    let parsed = match url::Url::parse(&query.url) {
        Ok(parsed) => parsed,
        Err(_) => return fail(StatusCode::BAD_REQUEST, "invalid image URL"),
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let site = if host == "aniworld.to" || host.ends_with(".aniworld.to") {
        "https://aniworld.to"
    } else if host == "serienstream.to" || host.ends_with(".serienstream.to") {
        "https://serienstream.to"
    } else {
        return fail(StatusCode::FORBIDDEN, "host not allowed");
    };
    let bytes = match web
        .http
        .bytes(&query.url, &[("Referer", site)])
        .await
    {
        Ok(bytes) => bytes,
        Err(err) => return fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    };
    let content_type = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "application/octet-stream"
    };
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    response
}

#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    pub path: String,
    #[serde(default)]
    pub download: Option<u8>,
}

pub async fn media(
    State(web): State<Arc<Web>>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> Response {
    let cfg = match web.settings.read().await.to_config() {
        Ok(cfg) => cfg,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let Some(path) = state::safe_join(&cfg.download_path, &query.path) else {
        return fail(StatusCode::NOT_FOUND, "file not found");
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return fail(StatusCode::NOT_FOUND, "file not found");
    };
    if !meta.is_file() {
        return fail(StatusCode::NOT_FOUND, "not a file");
    }
    let total = meta.len();
    let content_type = library::content_type(&path);
    let disposition = if query.download.unwrap_or(0) == 1 {
        "attachment"
    } else {
        "inline"
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().replace('"', ""))
        .unwrap_or_else(|| "video".into());

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total));

    let (start, end) = range_header.unwrap_or((0, total.saturating_sub(1)));
    if total == 0 {
        return fail(StatusCode::RANGE_NOT_SATISFIABLE, "empty file");
    }
    let length = end.saturating_sub(start) + 1;

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => return fail(StatusCode::NOT_FOUND, err.to_string()),
    };
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "cannot seek");
    }
    let stream = ReaderStream::new(file.take(length));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = if range_header.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("inline")),
    );
    if range_header.is_some() {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
        );
    }
    response
}

/// `bytes=START-END`, `bytes=START-` and `bytes=-SUFFIX` (RFC 9110).
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = value.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        let suffix: u64 = end.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }
    let start: u64 = start.trim().parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end.trim().is_empty() {
        total - 1
    } else {
        end.trim().parse::<u64>().ok()?.min(total - 1)
    };
    (start <= end).then_some((start, end))
}

pub async fn settings_get(State(web): State<Arc<Web>>) -> Response {
    let settings = web.settings.read().await.clone();
    ok(json!({
        "ok": true,
        "settings": settings,
        "languages": Language::ALL.iter().map(|l| json!({"label": l.label(), "key": l.key()})).collect::<Vec<_>>(),
        "version": web.version,
        "store": web.store_path.display().to_string(),
    }))
}

pub async fn settings_put(
    State(web): State<Arc<Web>>,
    Json(patch): Json<Value>,
) -> Response {
    let mut settings = web.settings.write().await;
    if let Err(err) = settings.apply(&patch) {
        return fail(StatusCode::BAD_REQUEST, err.to_string());
    }
    if let Err(err) = settings.to_config() {
        return fail(StatusCode::BAD_REQUEST, err.to_string());
    }
    let jobs = settings.jobs;
    let clone = settings.clone();
    drop(settings);
    web.set_jobs(jobs).await;
    if let Err(err) = web.save().await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("saving settings: {err}"),
        );
    }
    ok(json!({"ok": true, "settings": clone}))
}

pub async fn autosync_get(State(web): State<Arc<Web>>) -> Response {
    let settings = web.settings.read().await.clone();
    let watched = web.watched.lock().await.clone();
    let last = web.autosync_last.lock().await.clone();
    let next = web.next_autosync().await;
    ok(json!({
        "ok": true,
        "enabled": settings.autosync_enabled,
        "interval": settings.autosync_interval,
        "running": web.autosync_running.load(std::sync::atomic::Ordering::SeqCst),
        "next_in": next,
        "watched": watched,
        "last": last,
    }))
}

pub async fn autosync_put(
    State(web): State<Arc<Web>>,
    Json(patch): Json<Value>,
) -> Response {
    {
        let mut settings = web.settings.write().await;
        if let Err(err) = settings.apply(&patch) {
            return fail(StatusCode::BAD_REQUEST, err.to_string());
        }
    }
    if let Err(err) = web.save().await {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
    }
    autosync_get(State(web)).await
}

pub async fn autosync_run(State(web): State<Arc<Web>>) -> Response {
    if web
        .autosync_running
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return fail(StatusCode::CONFLICT, "autosync is already running");
    }
    let report = web.autosync_once().await;
    ok(serde_json::to_value(report).unwrap_or_else(|_| json!({})))
}

#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    pub url: String,
}

pub async fn autosync_add(
    State(web): State<Arc<Web>>,
    Json(body): Json<WatchRequest>,
) -> Response {
    match web.add_watch(&body.url).await {
        Ok(entry) => ok(json!({"ok": true, "watched": entry})),
        Err(err) => fail(StatusCode::BAD_REQUEST, format!("{err:#}")),
    }
}

pub async fn autosync_remove(State(web): State<Arc<Web>>, Path(id): Path<u64>) -> Response {
    web.remove_watch(id).await;
    ok(json!({"ok": true}))
}

pub async fn autosync_toggle(State(web): State<Arc<Web>>, Path(id): Path<u64>) -> Response {
    {
        let mut watched = web.watched.lock().await;
        if let Some(entry) = watched.iter_mut().find(|w| w.id == id) {
            entry.enabled = !entry.enabled;
        } else {
            return fail(StatusCode::NOT_FOUND, "not watched");
        }
    }
    if let Err(err) = web.save().await {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
    }
    ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn range_requests() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("nope", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn settings_config_round_trip() {
        let mut cfg = crate::config::Config::default();
        cfg.download_path = PathBuf::from("/tmp/aniworld-test");
        let mut settings = state::Settings::from_config(&cfg);
        settings.apply(&serde_json::json!({"jobs": 3, "language": "English Sub"})).unwrap();
        let cfg = settings.to_config().unwrap();
        assert_eq!(cfg.jobs, 3);
        assert_eq!(cfg.language, Language::EnglishSub);
    }
}
