//! Domain types shared by the scraper, downloader and TUI.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::config::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Site {
    AniWorld,
    SerienStream,
}

impl Default for Site {
    fn default() -> Self {
        Site::AniWorld
    }
}

impl Site {
    pub fn name(self) -> &'static str {
        match self {
            Site::AniWorld => "AniWorld",
            Site::SerienStream => "SerienStream",
        }
    }

    pub fn base(self) -> &'static str {
        match self {
            Site::AniWorld => "https://aniworld.to",
            Site::SerienStream => "https://serienstream.to",
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Site::AniWorld => "aniworld.to",
            Site::SerienStream => "serienstream.to",
        }
    }

    /// Series URLs live under `/anime/stream/<slug>` on AniWorld and
    /// `/serie/<slug>` on SerienStream.
    pub fn series_prefix(self) -> &'static str {
        match self {
            Site::AniWorld => "/anime/stream/",
            Site::SerienStream => "/serie/",
        }
    }

    pub fn from_host(host: &str) -> Option<Self> {
        let host = host.trim_start_matches("www.").to_ascii_lowercase();
        match host.as_str() {
            "aniworld.to" => Some(Site::AniWorld),
            "serienstream.to" | "serienstream.sx" => Some(Site::SerienStream),
            _ => None,
        }
    }
}

/// What a URL points at, with URLs normalised for fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Series {
        site: Site,
        url: String,
        slug: String,
    },
    Season {
        site: Site,
        url: String,
        slug: String,
        season: u32,
        movies: bool,
    },
    Episode {
        site: Site,
        url: String,
        slug: String,
        season: u32,
        movies: bool,
        number: u32,
    },
}

impl Target {
    pub fn site(&self) -> Site {
        match self {
            Target::Series { site, .. } | Target::Season { site, .. } | Target::Episode { site, .. } => *site,
        }
    }

    pub fn url(&self) -> &str {
        match self {
            Target::Series { url, .. } | Target::Season { url, .. } | Target::Episode { url, .. } => url,
        }
    }

    pub fn slug(&self) -> &str {
        match self {
            Target::Series { slug, .. } | Target::Season { slug, .. } | Target::Episode { slug, .. } => slug,
        }
    }

    /// Parse any supported series/season/episode URL.
    pub fn parse(input: &str) -> Result<Target> {
        let input = input.trim();
        let without_scheme = input
            .strip_prefix("https://")
            .or_else(|| input.strip_prefix("http://"))
            .unwrap_or(input);
        let mut parts = without_scheme.split('/');
        let host = parts.next().unwrap_or_default();
        let site = Site::from_host(host).ok_or_else(|| anyhow::anyhow!("unsupported host: {host}"))?;

        let segments: Vec<&str> = parts.filter(|s| !s.is_empty()).collect();
        // AniWorld: anime/stream/<slug>/... ; SerienStream: serie/<slug>/... (or serie/stream/<slug>)
        let (slug, idx) = match site {
            Site::AniWorld => {
                if segments.first() != Some(&"anime") || segments.get(1) != Some(&"stream") {
                    bail!("not an AniWorld series URL: {input}");
                }
                (segments.get(2).copied(), 3)
            }
            Site::SerienStream => {
                if segments.first() != Some(&"serie") {
                    bail!("not a SerienStream series URL: {input}");
                }
                if segments.get(1) == Some(&"stream") {
                    (segments.get(2).copied(), 3)
                } else {
                    (segments.get(1).copied(), 2)
                }
            }
        };
        let slug = slug.ok_or_else(|| anyhow::anyhow!("URL has no series slug: {input}"))?;
        if !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("invalid series slug '{slug}' in {input}");
        }
        let base = format!("{}{}{}", site.base(), site.series_prefix(), slug);

        let Some(first) = segments.get(idx) else {
            return Ok(Target::Series {
                site,
                url: base,
                slug: slug.to_string(),
            });
        };

        let (season, movies) = if *first == "filme" {
            (0, true)
        } else if let Some(num) = first.strip_prefix("staffel-") {
            let season: u32 = num
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid season number in {input}"))?;
            (season, false)
        } else {
            bail!("unsupported URL path '{first}' in {input}");
        };

        let season_url = format!("{}/{}", base, first);

        let Some(second) = segments.get(idx + 1) else {
            return Ok(Target::Season {
                site,
                url: season_url,
                slug: slug.to_string(),
                season,
                movies,
            });
        };

        let number = if let Some(num) = second.strip_prefix("episode-") {
            num.parse()
                .map_err(|_| anyhow::anyhow!("invalid episode number in {input}"))?
        } else if let Some(num) = second.strip_prefix("film-") {
            num.parse()
                .map_err(|_| anyhow::anyhow!("invalid film number in {input}"))?
        } else {
            bail!("unsupported URL path '{second}' in {input}");
        };

        Ok(Target::Episode {
            site,
            url: format!("{}/{}", season_url, second),
            slug: slug.to_string(),
            season,
            movies,
            number,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Series {
    pub site: Site,
    pub url: String,
    pub title: String,
    pub title_clean: String,
    pub description: Option<String>,
    pub year: Option<String>,
    pub imdb: Option<String>,
    pub poster: Option<String>,
    pub genres: Vec<String>,
    pub seasons: Vec<Season>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Season {
    pub url: String,
    pub number: u32,
    pub are_movies: bool,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Episode {
    pub url: String,
    pub number: u32,
    pub season_number: u32,
    pub is_movie: bool,
    pub title_de: Option<String>,
    pub title_en: Option<String>,
    /// Languages advertised by the season table, before hosters are resolved.
    pub languages: Vec<Language>,
    /// Language -> provider -> redirect URL on the site.
    pub providers: BTreeMap<Language, BTreeMap<String, String>>,
}

impl Episode {
    pub fn title(&self) -> String {
        self.title_en
            .as_deref()
            .filter(|t| !t.is_empty())
            .or(self.title_de.as_deref())
            .unwrap_or("")
            .to_string()
    }

    /// Absolute episode number used for file names and queue labels.
    ///
    /// Long series encode it in the English title (`[Episode 1157]`); the
    /// site's seasonal index is only the fallback. See [`crate::numbering`].
    pub fn episode_number(&self) -> u32 {
        crate::numbering::effective_number(
            self.title_en.as_deref(),
            self.is_movie,
            self.number,
            crate::numbering::keep_seasonal_numbers(),
        )
    }

    pub fn label(&self) -> String {
        let title = self.title();
        let number = self.episode_number();
        if title.is_empty() {
            format!("S{:02}E{:03}", self.season_number, number)
        } else {
            format!("S{:02}E{:03} {}", self.season_number, number, title)
        }
    }

    pub fn available_languages(&self) -> Vec<Language> {
        if self.providers.is_empty() {
            let mut languages = self.languages.clone();
            languages.sort_unstable();
            languages.dedup();
            languages
        } else {
            self.providers
                .iter()
                .filter(|(_, providers)| !providers.is_empty())
                .map(|(lang, _)| *lang)
                .collect()
        }
    }

    pub fn providers_for(&self, language: Language) -> Vec<&str> {
        self.providers
            .get(&language)
            .map(|map| map.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default()
    }

    pub fn redirect_for(&self, language: Language, provider: &str) -> Option<&str> {
        self.providers.get(&language)?.get(provider).map(|s| s.as_str())
    }

    /// Preferred provider first, then every other hoster the episode offers.
    pub fn attempt_order(&self, language: Language, preferred: &str) -> Vec<String> {
        let available = self.providers_for(language);
        let mut order = Vec::new();
        if available.iter().any(|p| *p == preferred) {
            order.push(preferred.to_string());
        }
        for name in available {
            if !name.eq_ignore_ascii_case(preferred) {
                order.push(name.to_string());
            }
        }
        order
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub site: Site,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aniworld_targets() {
        let series = Target::parse("https://aniworld.to/anime/stream/goblin-slayer").unwrap();
        assert_eq!(
            series,
            Target::Series {
                site: Site::AniWorld,
                url: "https://aniworld.to/anime/stream/goblin-slayer".into(),
                slug: "goblin-slayer".into()
            }
        );

        let season =
            Target::parse("https://www.aniworld.to/anime/stream/goblin-slayer/staffel-2/").unwrap();
        assert!(matches!(season, Target::Season { season: 2, .. }));

        let episode = Target::parse(
            "http://aniworld.to/anime/stream/goblin-slayer/staffel-1/episode-7",
        )
        .unwrap();
        assert!(matches!(
            episode,
            Target::Episode {
                season: 1,
                number: 7,
                movies: false,
                ..
            }
        ));

        let movie =
            Target::parse("https://aniworld.to/anime/stream/goblin-slayer/filme/film-2").unwrap();
        assert!(matches!(
            movie,
            Target::Episode {
                number: 2,
                movies: true,
                season: 0,
                ..
            }
        ));
    }

    #[test]
    fn parses_serienstream_targets() {
        let t = Target::parse("https://serienstream.to/serie/stream/dark/staffel-1/episode-1")
            .unwrap();
        assert_eq!(
            t.url(),
            "https://serienstream.to/serie/dark/staffel-1/episode-1"
        );
        assert!(Target::parse("https://example.com/anime/stream/x").is_err());
    }

    #[test]
    fn labels_use_the_absolute_episode_number() {
        let mut providers = BTreeMap::new();
        providers.insert(Language::GermanDub, BTreeMap::new());
        let episode = Episode {
            url: "https://aniworld.to/anime/stream/one-piece/staffel-21/episode-1".into(),
            number: 1,
            season_number: 21,
            title_en: Some("[Episode 892] The Land of Wano".into()),
            providers,
            ..Default::default()
        };
        // Tests run in parallel, so only assert the default (marker) behaviour.
        if !crate::numbering::keep_seasonal_numbers() {
            assert_eq!(episode.episode_number(), 892);
            assert!(episode.label().starts_with("S21E892"));
        }
    }

    #[test]
    fn attempt_order_falls_back() {
        let mut providers = BTreeMap::new();
        let mut langs = BTreeMap::new();
        langs.insert("Vidmoly".to_string(), "a".to_string());
        langs.insert("VOE".to_string(), "b".to_string());
        providers.insert(Language::GermanDub, langs);
        let ep = Episode {
            providers,
            ..Default::default()
        };
        assert_eq!(ep.attempt_order(Language::GermanDub, "VOE"), vec!["VOE", "Vidmoly"]);
        assert_eq!(ep.attempt_order(Language::GermanDub, "Nope"), vec!["VOE", "Vidmoly"]);
    }
}
