//! Site scraping for aniworld.to and serienstream.to.
//!
//! Parsing is split from fetching so the selectors can be unit-tested against
//! saved fixtures without any network access.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

use crate::config::Language;
use crate::model::{Episode, SearchHit, Season, Series, Site, Target};
use crate::net::Http;

pub struct Scraper {
    http: Http,
}

impl Scraper {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Fetch a series page and parse it.
    pub async fn series(&self, section: &Target) -> Result<Series> {
        let html = self.http.text(section.url(), &[]).await?;
        Ok(parse_series(&html, section.site(), section.url()))
    }

    /// Fetch a season page and attach its episodes.
    pub async fn season(&self, url: &str, site: Site, number: u32, movies: bool) -> Result<Season> {
        let html = self.http.text(url, &[]).await?;
        Ok(parse_season(&html, site, url, number, movies))
    }

    /// Fetch an episode page and fill in the hoster table.
    pub async fn load_providers(&self, episode: &mut Episode) -> Result<()> {
        if !episode.providers.is_empty() {
            return Ok(());
        }
        let html = self.http.text(&episode.url, &[]).await?;
        episode.providers = parse_providers(&html, &episode.url);
        if !episode.providers.is_empty() {
            episode.languages = episode.providers.keys().copied().collect();
        }
        if episode.providers.is_empty() {
            return Err(anyhow!(
                "no stream hosters found on {} (the page may be behind a CAPTCHA or the title was removed)",
                episode.url
            ));
        }
        Ok(())
    }

    /// Site search. AniWorld exposes a JSON endpoint; SerienStream returns HTML.
    pub async fn search(&self, site: Site, query: &str) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        match site {
            Site::AniWorld => {
                let json = self
                    .http
                    .post_form_json(
                        "https://aniworld.to/ajax/search",
                        &[("keyword", query)],
                    )
                    .await?;
                Ok(parse_search_json(&json, site))
            }
            Site::SerienStream => {
                let url = format!(
                    "https://serienstream.to/suche?term={}",
                    urlencode(query)
                );
                let html = self.http.text(&url, &[]).await?;
                Ok(parse_search_html(&html, site))
            }
        }
    }
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}

fn text_of(node: ElementRef<'_>) -> String {
    html_escape::decode_html_entities(&node.text().collect::<String>())
        .trim()
        .to_string()
}

fn attr(node: ElementRef<'_>, name: &str) -> Option<String> {
    node.value()
        .attr(name)
        .map(|v| html_escape::decode_html_entities(v).to_string())
}

fn first_text(doc: &Html, css: &str) -> Option<String> {
    doc.select(&sel(css)).next().map(text_of).filter(|s| !s.is_empty())
}

fn first_attr(doc: &Html, css: &str, name: &str) -> Option<String> {
    doc.select(&sel(css)).next().and_then(|n| attr(n, name))
}

pub fn parse_series(html: &str, site: Site, url: &str) -> Series {
    let doc = Html::parse_document(html);

    let title = first_text(&doc, ".series-title h1 span")
        .or_else(|| first_text(&doc, ".series-title h1"))
        .or_else(|| regex_capture(html, r#"<meta property="og:title" content="([^"]+)""#))
        .unwrap_or_else(|| {
            url.rsplit('/')
                .next()
                .unwrap_or("unknown")
                .replace('-', " ")
        });

    let description = doc
        .select(&sel(".seri_des"))
        .next()
        .and_then(|n| attr(n, "data-full-description"))
        .or_else(|| first_attr(&doc, "meta[name='description']", "content"));

    let year = {
        let start = regex_capture(html, r#"itemprop="startDate"><a href="[^"]*?(\d{4})"#);
        let end = regex_capture(html, r#"itemprop="endDate"><a href="[^"]*?(\d{4})"#);
        match (start, end) {
            (Some(s), Some(e)) if s != e => Some(format!("{s}-{e}")),
            (Some(s), _) => Some(s),
            _ => None,
        }
    };

    let poster = first_attr(&doc, ".seriesCoverBox img", "data-src")
        .or_else(|| first_attr(&doc, ".seriesCoverBox img", "src"))
        .map(|src| absolutise(site, &src));

    let imdb = Regex::new(r#"data-imdb="(tt\d+)""#)
        .unwrap()
        .captures(html)
        .map(|c| c[1].to_string());

    let mut genres: Vec<String> = doc
        .select(&sel(".genres a[itemprop='genre']"))
        .map(text_of)
        .filter(|g| !g.is_empty())
        .collect();
    genres.dedup();

    let seasons = parse_season_links(html, site, url);
    let title_clean = title.trim().to_string();

    Series {
        site,
        url: url.to_string(),
        title: title_clean.clone(),
        title_clean,
        description,
        year,
        imdb,
        poster,
        genres,
        seasons,
    }
}

fn parse_season_links(html: &str, site: Site, url: &str) -> Vec<Season> {
    let prefix = regex::escape(site.series_prefix().trim_start_matches('/'));
    let page = Regex::new(&format!(
        r#"href="(?:https?://[^"]*)?/{prefix}[^"/]+/staffel-(\d+)"#
    ))
    .unwrap();

    let mut numbers: Vec<u32> = page
        .captures_iter(html)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();

    let mut seasons: Vec<Season> = numbers
        .into_iter()
        .map(|number| Season {
            url: format!("{}/staffel-{}", url.trim_end_matches('/'), number),
            number,
            are_movies: false,
            episodes: Vec::new(),
        })
        .collect();

    let has_movies = html.contains("/filme\"") || html.contains("/filme/\"");
    if has_movies {
        seasons.push(Season {
            url: format!("{}/filme", url.trim_end_matches('/')),
            number: 0,
            are_movies: true,
            episodes: Vec::new(),
        });
    }
    seasons
}

/// Parse the episode table of a season page.
pub fn parse_season(html: &str, site: Site, url: &str, number: u32, movies: bool) -> Season {
    let doc = Html::parse_document(html);
    let mut episodes = Vec::new();

    let row_sel = sel("tr[itemtype='http://schema.org/Episode']");
    let rows: Vec<ElementRef<'_>> = doc.select(&row_sel).collect();
    let rows = if rows.is_empty() {
        // Older/alternate markup: rows carry itemprop="episode".
        doc.select(&sel("tr[itemprop='episode']")).collect()
    } else {
        rows
    };

    for row in rows {
        let href = row
            .select(&sel("a[itemprop='url']"))
            .next()
            .and_then(|a| attr(a, "href"))
            .or_else(|| {
                row.select(&sel("a[href*='episode-']"))
                    .next()
                    .and_then(|a| attr(a, "href"))
            })
            .or_else(|| {
                row.select(&sel("a[href*='film-']"))
                    .next()
                    .and_then(|a| attr(a, "href"))
            });

        let Some(href) = href else { continue };
        let episode_url = absolutise(site, &href);

        let ep_number = row
            .select(&sel("meta[itemprop='episodeNumber']"))
            .next()
            .and_then(|m| attr(m, "content"))
            .and_then(|c| c.trim().parse::<u32>().ok())
            .or_else(|| attr(row, "data-episode-season-id").and_then(|v| v.parse().ok()))
            .or_else(|| trailing_number(&episode_url))
            .unwrap_or(0);

        let title_de = row.select(&sel(".seasonEpisodeTitle strong")).next().map(text_of);
        let title_en = row
            .select(&sel(".seasonEpisodeTitle span"))
            .next()
            .map(text_of)
            .or_else(|| title_de.clone());

        let languages: Vec<Language> = row
            .select(&sel("img"))
            .filter_map(|img| attr(img, "src"))
            .filter(|src| src.contains("/public/img/"))
            .filter_map(|src| {
                let stem = src.rsplit('/').next()?.trim_end_matches(".svg");
                language_from_flag(stem)
            })
            .collect();

        episodes.push(Episode {
            url: episode_url,
            number: ep_number,
            season_number: number,
            is_movie: movies,
            title_de,
            title_en,
            languages,
            providers: BTreeMap::new(),
        });
    }

    Season {
        url: url.to_string(),
        number,
        are_movies: movies,
        episodes,
    }
}

fn language_from_flag(stem: &str) -> Option<Language> {
    match stem {
        "german" => Some(Language::GermanDub),
        "japanese-german" => Some(Language::GermanSub),
        "english" => Some(Language::EnglishDub),
        "japanese-english" => Some(Language::EnglishSub),
        _ => None,
    }
}

/// Parse the `data-lang-key` hoster list on an episode page.
pub fn parse_providers(html: &str, page_url: &str) -> BTreeMap<Language, BTreeMap<String, String>> {
    let doc = Html::parse_document(html);
    let mut out: BTreeMap<Language, BTreeMap<String, String>> = BTreeMap::new();

    for item in doc.select(&sel("li[data-lang-key]")) {
        let Some(lang) = attr(item, "data-lang-key").and_then(|k| Language::from_key(&k)) else {
            continue;
        };
        let Some(anchor) = item.select(&sel("a.watchEpisode")).next() else {
            continue;
        };
        let href = attr(anchor, "href")
            .or_else(|| attr(item, "data-link-target"))
            .unwrap_or_default();
        if href.is_empty() {
            continue;
        }
        let provider = item
            .select(&sel("h4"))
            .next()
            .map(text_of)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                item.select(&sel("i.icon"))
                    .next()
                    .and_then(|i| attr(i, "title"))
                    .map(|t| t.trim_start_matches("Hoster ").to_string())
            });
        let Some(provider) = provider else { continue };
        out.entry(lang)
            .or_default()
            .insert(provider, absolutise_url(page_url, &href));
    }
    out
}

/// Search results highlight the match with markup (`<em>…</em>`), which does
/// not belong in a terminal table.
fn strip_tags(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_escape::decode_html_entities(&out).trim().to_string()
}

pub fn parse_search_json(value: &serde_json::Value, site: Site) -> Vec<SearchHit> {
    let items: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(_) => vec![value],
        _ => Vec::new(),
    };

    let mut hits = Vec::new();
    for item in items {
        let title = item
            .get("title")
            .or_else(|| item.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = strip_tags(title);
        let link = item
            .get("link")
            .or_else(|| item.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let url = absolutise(site, link);
        // Search also returns seasons/episodes; the browser only lists titles.
        let Ok(Target::Series { url, .. }) = Target::parse(&url) else {
            continue;
        };
        let description = item
            .get("description")
            .or_else(|| item.get("desc"))
            .and_then(|v| v.as_str())
            .map(strip_tags)
            .filter(|s| !s.is_empty());
        hits.push(SearchHit {
            title,
            url,
            description,
            site,
        });
    }
    hits
}

pub fn parse_search_html(html: &str, site: Site) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for anchor in doc.select(&sel("a[href]")) {
        let Some(href) = attr(anchor, "href") else { continue };
        let url = absolutise(site, &href);
        if !seen.insert(url.clone()) {
            continue;
        }
        if !matches!(Target::parse(&url), Ok(Target::Series { .. })) {
            continue;
        }
        let title = text_of(anchor);
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title,
            url,
            description: None,
            site,
        });
    }
    hits
}

fn regex_capture(haystack: &str, pattern: &str) -> Option<String> {
    Regex::new(pattern)
        .ok()?
        .captures(haystack)
        .map(|c| c[1].to_string())
}

fn trailing_number(url: &str) -> Option<u32> {
    Regex::new(r"(?:episode|film)-(\d+)")
        .unwrap()
        .captures(url)
        .and_then(|c| c[1].parse().ok())
}

fn absolutise(site: Site, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        let mut url = link.to_string();
        if let Some(stripped) = url.strip_prefix("http://") {
            url = format!("https://{stripped}");
        }
        return url;
    }
    format!(
        "{}{}",
        site.base(),
        if link.starts_with('/') {
            link.to_string()
        } else {
            format!("/{link}")
        }
    )
}

fn absolutise_url(page_url: &str, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    let base = page_url.split('/').take(3).collect::<Vec<_>>().join("/");
    format!(
        "{}{}",
        base,
        if link.starts_with('/') {
            link.to_string()
        } else {
            format!("/{link}")
        }
    )
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Convenience for callers that only have a URL string.
pub async fn load_episode(scraper: &Scraper, url: &str) -> Result<Episode> {
    let target = Target::parse(url).context("parsing episode URL")?;
    let (season, movies, number) = match target {
        Target::Episode {
            season, movies, number, ..
        } => (season, movies, number),
        _ => return Err(anyhow!("not an episode URL: {url}")),
    };
    let mut episode = Episode {
        url: url.to_string(),
        number,
        season_number: season,
        is_movie: movies,
        ..Default::default()
    };
    scraper.load_providers(&mut episode).await?;
    Ok(episode)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES_HTML: &str = r#"
    <html><head><meta property="og:title" content="Goblin Slayer"></head><body>
    <div class="series-title">
      <h1 itemprop="name"><span>Goblin Slayer</span></h1>
      <small>(<span itemprop="startDate"><a href="https://aniworld.to/animes/jahr/2018">2018</a></span>
      - <span itemprop="endDate"><a href="https://aniworld.to/animes/jahr/2023">2023</a></span>)</small>
      <a href="https://www.imdb.com/title/tt8690728" data-imdb="tt8690728" class="imdb-link">IMDB</a>
    </div>
    <div class="seriesCoverBox"><img src="/public/img/cover/goblin.jpg" data-src="/public/img/cover/goblin.jpg"></div>
    <p class="seri_des" data-full-description="Goblins are the weakest monsters.">Goblins…</p>
    <div class="genres"><ul>
      <li><a class="genreButton" itemprop="genre">Action</a></li>
      <li><a class="genreButton" itemprop="genre">Fantasy</a></li>
    </ul></div>
    <ul class="navi-season">
      <li><a href="/anime/stream/goblin-slayer/staffel-1">Staffel 1</a></li>
      <li><a href="/anime/stream/goblin-slayer/staffel-2">Staffel 2</a></li>
      <li><a href="/anime/stream/goblin-slayer/filme" title="Alle Filme">Filme</a></li>
    </ul>
    </body></html>"#;

    const SEASON_HTML: &str = r#"
    <table><tbody>
    <tr itemprop="episode" itemscope itemtype="http://schema.org/Episode" data-episode-season-id="1">
      <td class="season1EpisodeID"><meta itemprop="episodeNumber" content="1">
        <a itemprop="url" href="/anime/stream/goblin-slayer/staffel-1/episode-1">Folge 1</a></td>
      <td class="seasonEpisodeTitle">
        <a href="/anime/stream/goblin-slayer/staffel-1/episode-1">
          <strong>Das Schicksal der Abenteurer</strong> - <span>The Fate of Particular Adventurers</span>
        </a></td>
      <td class="editFunctions">
        <img class="flag" src="/public/img/german.svg">
        <img class="flag" src="/public/img/japanese-english.svg">
      </td>
    </tr>
    <tr itemprop="episode" itemscope itemtype="http://schema.org/Episode" data-episode-season-id="2">
      <td><meta itemprop="episodeNumber" content="2">
        <a itemprop="url" href="/anime/stream/goblin-slayer/staffel-1/episode-2">Folge 2</a></td>
      <td class="seasonEpisodeTitle"><a href="/x"><strong>Dämonentöter</strong> - <span>Goblin Slayer</span></a></td>
      <td class="editFunctions"><img src="/public/img/german.svg"></td>
    </tr>
    </tbody></table>"#;

    const EPISODE_HTML: &str = r#"
    <ul class="row">
      <li class="col-md-3 episodeLink1" data-lang-key="1" data-link-target="/redirect/2526098">
        <div><a class="watchEpisode" itemprop="url" href="/redirect/2526098">
          <i class="icon VOE" title="Hoster VOE"></i><h4>VOE</h4></a></div></li>
      <li class="col-md-3 episodeLink2" data-lang-key="1" data-link-target="/redirect/2883363">
        <div><a class="watchEpisode" itemprop="url" href="/redirect/2883363">
          <i class="icon Filemoon" title="Hoster Filemoon"></i><h4>Filemoon</h4></a></div></li>
      <li class="col-md-3 episodeLink3" data-lang-key="2" data-link-target="/redirect/1791080">
        <div><a class="watchEpisode" itemprop="url" href="/redirect/1791080">
          <i class="icon VOE" title="Hoster VOE"></i><h4>VOE</h4></a></div></li>
    </ul>"#;

    #[test]
    fn series_metadata_and_seasons() {
        let series = parse_series(
            SERIES_HTML,
            Site::AniWorld,
            "https://aniworld.to/anime/stream/goblin-slayer",
        );
        assert_eq!(series.title, "Goblin Slayer");
        assert_eq!(series.year.as_deref(), Some("2018-2023"));
        assert_eq!(series.imdb.as_deref(), Some("tt8690728"));
        assert_eq!(series.genres, vec!["Action", "Fantasy"]);
        assert_eq!(
            series.poster.as_deref(),
            Some("https://aniworld.to/public/img/cover/goblin.jpg")
        );
        assert_eq!(series.seasons.len(), 3);
        assert_eq!(series.seasons[0].number, 1);
        assert!(series.seasons[2].are_movies);
        assert_eq!(
            series.seasons[1].url,
            "https://aniworld.to/anime/stream/goblin-slayer/staffel-2"
        );
    }

    #[test]
    fn season_episodes_and_languages() {
        let season = parse_season(
            SEASON_HTML,
            Site::AniWorld,
            "https://aniworld.to/anime/stream/goblin-slayer/staffel-1",
            1,
            false,
        );
        assert_eq!(season.episodes.len(), 2);
        let ep = &season.episodes[0];
        assert_eq!(ep.number, 1);
        assert_eq!(ep.title_de.as_deref(), Some("Das Schicksal der Abenteurer"));
        assert_eq!(
            ep.title_en.as_deref(),
            Some("The Fate of Particular Adventurers")
        );
        assert_eq!(
            ep.available_languages(),
            vec![Language::GermanDub, Language::EnglishSub]
        );
        assert!(ep.url.ends_with("/staffel-1/episode-1"));
    }

    #[test]
    fn hoster_table_is_grouped_by_language() {
        let providers = parse_providers(
            EPISODE_HTML,
            "https://aniworld.to/anime/stream/goblin-slayer/staffel-1/episode-1",
        );
        let dub = providers.get(&Language::GermanDub).unwrap();
        assert_eq!(
            dub.get("VOE").map(String::as_str),
            Some("https://aniworld.to/redirect/2526098")
        );
        assert_eq!(dub.len(), 2);
        assert_eq!(
            providers[&Language::EnglishSub]["VOE"],
            "https://aniworld.to/redirect/1791080"
        );
    }

    #[test]
    fn search_json_filters_non_series() {
        let json = serde_json::json!([
            {"title": "Goblin Slayer", "link": "/anime/stream/goblin-slayer", "description": "x"},
            {"title": "Goblin Slayer II", "link": "/anime/stream/goblin-slayer-ii/staffel-1"},
            {"title": "Other", "link": "/anime/stream/other"}
        ]);
        let hits = parse_search_json(&json, Site::AniWorld);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://aniworld.to/anime/stream/goblin-slayer");
    }

    #[test]
    fn search_html_dedupes_series_links() {
        let html = r#"<div><a href="/serie/dark">Dark</a><a href="/serie/dark">Dark</a>
                     <a href="/serie/dark/staffel-1">Staffel 1</a><a href="/serie/1899">1899</a></div>"#;
        let hits = parse_search_html(html, Site::SerienStream);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://serienstream.to/serie/dark");
    }
}
