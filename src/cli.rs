//! Command line interface. A bare URL is treated as `download`, so the tool
//! stays drop-in compatible with `aniworld "https://aniworld.to/..."`.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc;

use crate::config::{parse_bandwidth, Config, Language};
use crate::download::{Engine, Event, EVENT_BUFFER};
use crate::metrics::format_rate;
use crate::model::{Episode, Series, Site, Target};
use crate::net::Http;
use crate::scrape::Scraper;
use crate::{mux, tui};

#[derive(Debug, Parser)]
#[command(
    name = "aniworld-rs",
    version,
    about = "Native AniWorld/SerienStream downloader with parallel HLS downloads",
    after_help = "EXAMPLES:\n  aniworld-rs https://aniworld.to/anime/stream/goblin-slayer\n  aniworld-rs download -l \"English Sub\" -j 16 <URL> <URL>\n  aniworld-rs search \"goblin slayer\"\n  aniworld-rs tui"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download series, seasons or single episodes
    Download(DownloadArgs),
    /// Show metadata for a URL without downloading
    Info {
        url: String,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Search a site and print matching series
    Search {
        query: String,
        #[arg(long, value_enum, default_value_t = SiteArg::Aniworld)]
        site: SiteArg,
        #[arg(long)]
        json: bool,
    },
    /// Open the interactive dashboard
    Tui {
        #[arg(long, value_enum, default_value_t = SiteArg::Aniworld)]
        site: SiteArg,
    },
    /// Serve the web interface
    #[command(alias = "serve")]
    Web {
        /// Port to listen on
        #[arg(short, long, default_value_t = 8080)]
        port: u16,
        /// Interface to bind; use 0.0.0.0 for LAN access
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    /// Series, season or episode URLs
    #[arg(required = true)]
    pub urls: Vec<String>,
    /// Language track
    #[arg(short, long)]
    pub language: Option<String>,
    /// Preferred hoster (falls back to the others)
    #[arg(short, long)]
    pub provider: Option<String>,
    /// Download directory
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Parallel segment downloads per episode
    #[arg(short = 'j', long)]
    pub concurrency: Option<usize>,
    /// HTTP retries per request
    #[arg(long)]
    pub retries: Option<u32>,
    /// Bandwidth ceiling, e.g. 20M or 512k
    #[arg(long)]
    pub limit: Option<String>,
    /// Episodes downloaded at the same time
    #[arg(long, default_value_t = 2)]
    pub jobs: usize,
    /// Naming template override
    #[arg(long)]
    pub naming: Option<String>,
    /// Only these seasons, e.g. --season 1 or --season 1,2
    #[arg(long)]
    pub season: Option<String>,
    /// Only these episodes, e.g. --episodes 1-3,7
    #[arg(long)]
    pub episodes: Option<String>,
    /// Print what would be downloaded without fetching media
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SiteArg {
    Aniworld,
    Sto,
}

impl SiteArg {
    pub fn to_site(self) -> Site {
        match self {
            SiteArg::Aniworld => Site::AniWorld,
            SiteArg::Sto => Site::SerienStream,
        }
    }
}

/// Insert the implicit `download` subcommand when the first argument is a URL.
pub fn normalise_args<I, T>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = T>,
    T: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    if args.len() > 1 {
        let first = &args[1];
        let is_url = first.starts_with("http://") || first.starts_with("https://");
        if is_url {
            let mut out = Vec::with_capacity(args.len() + 1);
            out.push(args[0].clone());
            out.push("download".to_string());
            out.extend(args[1..].iter().cloned());
            return out;
        }
    }
    args
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse_from(normalise_args(std::env::args()));
    let mut cfg = Config::from_env();

    match cli.command {
        Command::Download(args) => {
            apply_overrides(&mut cfg, &args)?;
            let cfg = Arc::new(cfg);
            download(cfg, args).await
        }
        Command::Info { url, json } => info(Arc::new(cfg), &url, json).await,
        Command::Search { query, site, json } => search(Arc::new(cfg), site.to_site(), &query, json).await,
        Command::Tui { site } => tui::run(Arc::new(cfg), site.to_site()).await,
        Command::Web { port, host } => crate::web::run(cfg, host, port).await,
    }
}

fn apply_overrides(cfg: &mut Config, args: &DownloadArgs) -> Result<()> {
    if let Some(language) = &args.language {
        cfg.language = Language::parse(language).ok_or_else(|| {
            anyhow!(
                "unknown language '{language}' (use: {})",
                Language::ALL
                    .iter()
                    .map(|l| l.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    }
    if let Some(provider) = &args.provider {
        if provider.trim().is_empty() {
            bail!("--provider cannot be empty");
        }
        cfg.provider = provider.trim().to_string();
    }
    if let Some(output) = &args.output {
        cfg.download_path = crate::config::expand_home(&output.to_string_lossy());
    }
    if let Some(concurrency) = args.concurrency {
        cfg.concurrency = concurrency.clamp(1, 64);
    }
    if let Some(retries) = args.retries {
        cfg.retries = retries.clamp(1, 20);
    }
    if let Some(limit) = &args.limit {
        cfg.bandwidth_limit = parse_bandwidth(limit);
    }
    if let Some(naming) = &args.naming {
        cfg.naming_template = naming.clone();
    }
    cfg.jobs = args.jobs.max(1);
    Ok(())
}

fn build(cfg: Arc<Config>) -> Result<(Arc<Http>, Arc<Scraper>)> {
    let http = Arc::new(Http::new(&cfg.user_agent, cfg.retries)?);
    let scraper = Arc::new(Scraper::new((*http).clone()));
    Ok((http, scraper))
}

async fn search(cfg: Arc<Config>, site: Site, query: &str, json: bool) -> Result<()> {
    let (_, scraper) = build(cfg)?;
    let hits = scraper.search(site, query).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("No results for \"{query}\" on {}.", site.name());
        return Ok(());
    }
    println!("{} result(s) on {}:\n", hits.len(), site.name());
    for (index, hit) in hits.iter().enumerate() {
        println!("{:>2}. {}", index + 1, hit.title);
        println!("    {}", hit.url);
        if let Some(description) = &hit.description {
            let short: String = description.chars().take(110).collect();
            println!("    {short}");
        }
    }
    Ok(())
}

async fn info(cfg: Arc<Config>, url: &str, json: bool) -> Result<()> {
    let (_, scraper) = build(cfg)?;
    let target = Target::parse(url)?;
    let series = scraper.series(&target).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&series)?);
        return Ok(());
    }

    println!("{} ({})", series.title, series.year.as_deref().unwrap_or("?"));
    println!("  site:     {}", series.site.name());
    println!("  imdb:     {}", series.imdb.as_deref().unwrap_or("-"));
    println!("  genres:   {}", series.genres.join(", "));
    if let Some(poster) = &series.poster {
        println!("  poster:   {poster}");
    }
    if let Some(description) = &series.description {
        println!("\n{}\n", wrap(description, 96));
    }

    for season in &series.seasons {
        let label = if season.are_movies {
            "Movies".to_string()
        } else {
            format!("Season {:02}", season.number)
        };
        let fetched = scraper
            .season(&season.url, series.site, season.number, season.are_movies)
            .await;
        match fetched {
            Ok(season) => {
                println!("{label}: {} episode(s)", season.episodes.len());
                for episode in season.episodes.iter().take(5) {
                    let languages: Vec<&str> = episode
                        .available_languages()
                        .iter()
                        .map(|l| l.label())
                        .collect();
                    println!(
                        "  E{:02} {}{}",
                        episode.number,
                        episode.title(),
                        if languages.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", languages.join(", "))
                        }
                    );
                }
                if season.episodes.len() > 5 {
                    println!("  … {} more", season.episodes.len() - 5);
                }
            }
            Err(err) => println!("{label}: could not load ({err})"),
        }
    }
    Ok(())
}

fn wrap(text: &str, width: usize) -> String {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + word.len() + 1 > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines.join("\n")
}

async fn download(cfg: Arc<Config>, args: DownloadArgs) -> Result<()> {
    cfg.ensure_download_path()?;
    let (http, scraper) = build(cfg.clone())?;

    if !mux::ffmpeg_available(&cfg.ffmpeg).await {
        eprintln!(
            "warning: '{}' was not found — streams will be kept in their raw container",
            cfg.ffmpeg
        );
    }

    let season_filter = parse_numbers(args.season.as_deref())?;
    let episode_filter = parse_numbers(args.episodes.as_deref())?;

    let mut work: Vec<(Series, Episode)> = Vec::new();
    for url in &args.urls {
        let items = expand_url(&scraper, url, &season_filter, &episode_filter).await?;
        if items.is_empty() {
            eprintln!("warning: nothing matched in {url}");
        }
        work.extend(items);
    }

    if work.is_empty() {
        bail!("no episodes to download");
    }

    println!(
        "{} episode(s) • {} • {} • {} segment workers",
        work.len(),
        cfg.language.label(),
        cfg.provider,
        cfg.concurrency
    );
    if let Some(limit) = cfg.bandwidth_limit {
        println!("bandwidth ceiling: {}", format_rate(limit as f64));
    }

    if args.dry_run {
        for (series, episode) in &work {
            println!("  {}", plan_line(cfg.as_ref(), series, episode));
        }
        return Ok(());
    }

    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let engine = Arc::new(Engine::new(cfg.clone(), http, scraper, tx));
    let renderer = tokio::spawn(render_progress(rx));

    let jobs = args.jobs.max(1);
    let mut results: Vec<(String, bool, String)> = Vec::new();
    for (index, chunk) in work.chunks(jobs).enumerate() {
        let futures = chunk.iter().enumerate().map(|(offset, (series, episode))| {
            let engine = engine.clone();
            let series = series.clone();
            let episode = episode.clone();
            let id = (index * jobs + offset) as u64;
            async move {
                let label = episode.label();
                match engine.download_episode(id, &series, &episode, None).await {
                    Ok(path) => (label, true, path.display().to_string()),
                    Err(err) => (label, false, err.to_string()),
                }
            }
        });
        results.extend(join_all(futures).await);
        if engine.is_cancelled() {
            break;
        }
    }

    drop(engine);
    let _ = renderer.await;

    let failed: Vec<&(String, bool, String)> = results.iter().filter(|r| !r.1).collect();
    println!();
    for (label, ok, message) in &results {
        println!("{} {}  {}", if *ok { "✓" } else { "✗" }, label, message);
    }
    if !failed.is_empty() {
        bail!("{} of {} episode(s) failed", failed.len(), results.len());
    }
    println!("done: {} episode(s) saved to {}", results.len(), cfg.download_path.display());
    Ok(())
}

fn plan_line(cfg: &Config, series: &Series, episode: &Episode) -> String {
    let values = crate::naming::NamingValues {
        title: series.title_clean.clone(),
        year: series.year.clone().unwrap_or_default(),
        imdbid: series.imdb.clone().unwrap_or_default(),
        season: episode.season_number,
        episode: episode.episode_number(),
        language: cfg.language.label().to_string(),
        resolution: "unknown".to_string(),
    };
    crate::naming::render(&cfg.download_path, &cfg.naming_template, &values).display().to_string()
}

async fn render_progress(mut rx: mpsc::Receiver<Event>) {
    // When the output is piped or logged there is no terminal to draw bars on,
    // so print sparse progress lines instead of staying silent for minutes.
    if !std::io::stdout().is_terminal() {
        render_plain_progress(rx).await;
        return;
    }

    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{msg:48} {bar:32.cyan/238} {percent:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar());
    let mut bars: HashMap<u64, ProgressBar> = HashMap::new();

    while let Some(event) = rx.recv().await {
        match event {
            Event::Stage { id, text } => {
                let bar = bars.entry(id).or_insert_with(|| multi.add(ProgressBar::new(0)));
                bar.set_message(text);
            }
            Event::Progress {
                id, done, total, ..
            } => {
                let bar = bars.entry(id).or_insert_with(|| {
                    let bar = multi.add(ProgressBar::new(0));
                    bar.set_style(style.clone());
                    bar
                });
                if bar.length() != Some(total) {
                    bar.set_length(total);
                }
                bar.set_position(done);
            }
            Event::Finished {
                id, label, ok, message, ..
            } => {
                if let Some(bar) = bars.remove(&id) {
                    if ok {
                        bar.finish_and_clear();
                    } else {
                        bar.abandon_with_message(format!("✗ {label}: {message}"));
                    }
                } else if ok {
                    println!("✓ {label}: {message}");
                } else {
                    println!("✗ {label}: {message}");
                }
            }
        }
    }
}

async fn render_plain_progress(mut rx: mpsc::Receiver<Event>) {
    let mut last_report: HashMap<u64, std::time::Instant> = HashMap::new();
    while let Some(event) = rx.recv().await {
        match event {
            Event::Stage { id, text } => {
                println!("[{id}] {text}");
            }
            Event::Progress {
                id, label, done, total, bytes, speed,
            } => {
                let now = std::time::Instant::now();
                let due = last_report
                    .get(&id)
                    .map(|when| now.duration_since(*when).as_secs_f64() >= 5.0)
                    .unwrap_or(true);
                if due {
                    last_report.insert(id, now);
                    let percent = if total == 0 {
                        0.0
                    } else {
                        done as f64 / total as f64 * 100.0
                    };
                    println!(
                        "[{id}] {label}: {percent:.0}% ({done}/{total} segments, {} downloaded, {})",
                        crate::metrics::format_bytes(bytes),
                        format_rate(speed)
                    );
                }
            }
            Event::Finished { id, label, ok, message, .. } => {
                println!("[{id}] {} {label}: {message}", if ok { "OK" } else { "FAILED" });
            }
        }
    }
}

async fn expand_url(
    scraper: &Scraper,
    url: &str,
    seasons: &Option<Vec<u32>>,
    episodes: &Option<Vec<u32>>,
) -> Result<Vec<(Series, Episode)>> {
    let target = Target::parse(url).with_context(|| format!("unsupported URL: {url}"))?;
    let mut out = Vec::new();

    match target {
        Target::Series { url, .. } => {
            let series = scraper.series(&Target::parse(&url)?).await?;
            for season in series.seasons.clone() {
                if let Some(filter) = seasons {
                    if !filter.contains(&season.number) {
                        continue;
                    }
                }
                let loaded = scraper
                    .season(&season.url, series.site, season.number, season.are_movies)
                    .await?;
                for episode in loaded.episodes {
                    if let Some(filter) = episodes {
                        if !filter.contains(&episode.number) {
                            continue;
                        }
                    }
                    out.push((series.clone(), episode));
                }
            }
        }
        Target::Season {
            url,
            site,
            slug,
            season,
            movies,
        } => {
            if let Some(filter) = seasons {
                if !filter.contains(&season) {
                    return Ok(Vec::new());
                }
            }
            let series = scraper
                .series(&Target::parse(&format!("{}{}{}", site.base(), site.series_prefix(), slug))?)
                .await?;
            let loaded = scraper.season(&url, site, season, movies).await?;
            for episode in loaded.episodes {
                if let Some(filter) = episodes {
                    if !filter.contains(&episode.number) {
                        continue;
                    }
                }
                out.push((series.clone(), episode));
            }
        }
        Target::Episode {
            url,
            site,
            slug,
            season,
            movies,
            number,
        } => {
            if let Some(filter) = seasons {
                if !filter.contains(&season) {
                    return Ok(Vec::new());
                }
            }
            if let Some(filter) = episodes {
                if !filter.contains(&number) {
                    return Ok(Vec::new());
                }
            }
            let series = scraper
                .series(&Target::parse(&format!("{}{}{}", site.base(), site.series_prefix(), slug))?)
                .await?;
            let loaded = scraper.season(
                &format!(
                    "{}{}{}/{}",
                    site.base(),
                    site.series_prefix(),
                    slug,
                    if movies { "filme".to_string() } else { format!("staffel-{season}") }
                ),
                site,
                season,
                movies,
            )
            .await;
            let episode = loaded
                .ok()
                .and_then(|season| season.episodes.into_iter().find(|e| e.url == url))
                .unwrap_or_else(|| Episode {
                    url: url.clone(),
                    number,
                    season_number: season,
                    is_movie: movies,
                    ..Default::default()
                });
            out.push((series, episode));
        }
    }

    Ok(out)
}

fn parse_numbers(value: Option<&str>) -> Result<Option<Vec<u32>>> {
    let Some(value) = value else { return Ok(None) };
    let mut numbers = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start.trim().parse().context("invalid range start")?;
            let end: u32 = end.trim().parse().context("invalid range end")?;
            if start > end {
                bail!("invalid range {part}");
            }
            numbers.extend(start..=end);
        } else {
            numbers.push(part.parse().context("invalid number")?);
        }
    }
    Ok(Some(numbers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_become_download_subcommand() {
        let args = normalise_args(["aniworld-rs", "https://aniworld.to/anime/stream/x"]);
        assert_eq!(
            args,
            vec![
                "aniworld-rs",
                "download",
                "https://aniworld.to/anime/stream/x"
            ]
        );
        let args = normalise_args(["aniworld-rs", "search", "goblin"]);
        assert_eq!(args, vec!["aniworld-rs", "search", "goblin"]);
    }

    #[test]
    fn parses_number_filters() {
        assert_eq!(parse_numbers(Some("1-3,7")).unwrap(), Some(vec![1, 2, 3, 7]));
        assert_eq!(parse_numbers(None).unwrap(), None);
        assert!(parse_numbers(Some("x")).is_err());
    }

    #[test]
    fn wraps_long_text() {
        let text = "one two three four five six";
        assert_eq!(wrap(text, 10), "one two\nthree four\nfive six");
    }
}
