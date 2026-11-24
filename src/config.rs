//! Runtime configuration, read from the same `ANIWORLD_*` environment variables
//! the Python original used so existing deployments keep working.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// The four language tracks AniWorld exposes, in provider-key order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub enum Language {
    GermanDub,
    EnglishSub,
    GermanSub,
    EnglishDub,
}

impl Language {
    pub const ALL: [Language; 4] = [
        Language::GermanDub,
        Language::EnglishSub,
        Language::GermanSub,
        Language::EnglishDub,
    ];

    /// `data-lang-key` on an episode page.
    pub fn key(self) -> &'static str {
        match self {
            Language::GermanDub => "1",
            Language::EnglishSub => "2",
            Language::GermanSub => "3",
            Language::EnglishDub => "4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Language::GermanDub => "German Dub",
            Language::EnglishSub => "English Sub",
            Language::GermanSub => "German Sub",
            Language::EnglishDub => "English Dub",
        }
    }

    /// ffmpeg/ISO-639 prefix used to pick a separate HLS audio rendition.
    pub fn hls_audio(self) -> &'static str {
        match self {
            Language::GermanDub | Language::GermanSub => "deu",
            Language::EnglishSub | Language::EnglishDub => "eng",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim() {
            "1" => Some(Language::GermanDub),
            "2" => Some(Language::EnglishSub),
            "3" => Some(Language::GermanSub),
            "4" => Some(Language::EnglishDub),
            _ => None,
        }
    }

    /// Accepts the CLI labels (`"German Dub"`), keys (`"1"`) and short codes
    /// (`de`, `en`, `gersub`, `engsub`, ...).
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.trim().to_ascii_lowercase().replace([' ', '-', '_'], "");
        match v.as_str() {
            "1" | "gerdub" | "germandub" | "de" | "deu" | "ger" | "dub" | "deutsch" => {
                Some(Language::GermanDub)
            }
            "2" | "engsub" | "englishsub" | "ensub" | "sub" => Some(Language::EnglishSub),
            "3" | "gersub" | "germansub" | "desub" => Some(Language::GermanSub),
            "4" | "engdub" | "englishdub" | "endub" => Some(Language::EnglishDub),
            _ => None,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Everything the engine needs, resolved once at start-up.
#[derive(Debug, Clone)]
pub struct Config {
    pub download_path: PathBuf,
    pub language: Language,
    pub provider: String,
    pub concurrency: usize,
    pub retries: u32,
    pub bandwidth_limit: Option<u64>,
    pub naming_template: String,
    pub ffmpeg: String,
    pub user_agent: String,
    pub resume: bool,
    pub jobs: usize,
}

impl Default for Config {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            download_path: home_dir().join("Downloads"),
            language: Language::GermanDub,
            provider: "VOE".to_string(),
            // Enough parallelism to saturate a normal line without flooding the
            // hoster: the original defaulted to 8 threads regardless of cores.
            concurrency: (cpus * 2).clamp(4, 16),
            retries: 4,
            bandwidth_limit: None,
            naming_template:
                "{title} ({year}) [imdbid-{imdbid}]/Season {season}/{title} S{season}E{episode}.mkv"
                    .to_string(),
            ffmpeg: "ffmpeg".to_string(),
            user_agent: concat!(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 ",
                "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
            )
            .to_string(),
            resume: true,
            jobs: 2,
        }
    }
}

impl Config {
    /// Environment defaults first; CLI flags are applied on top by `cli::run`.
    pub fn from_env() -> Self {
        let mut cfg = Config::default();

        if let Some(path) = env_path("ANIWORLD_DOWNLOAD_PATH") {
            cfg.download_path = path;
        }
        if let Ok(value) = env::var("ANIWORLD_LANGUAGE") {
            if let Some(lang) = Language::parse(&value) {
                cfg.language = lang;
            }
        }
        if let Ok(value) = env::var("ANIWORLD_PROVIDER") {
            if !value.trim().is_empty() {
                cfg.provider = value.trim().to_string();
            }
        }
        if let Some(value) = env_number::<usize>("ANIWORLD_HLS_CONCURRENCY") {
            cfg.concurrency = value.clamp(1, 64);
        }
        if let Some(value) = env_number::<u32>("ANIWORLD_RETRIES") {
            cfg.retries = value.min(20);
        }
        if let Ok(value) = env::var("ANIWORLD_NAMING_TEMPLATE") {
            if !value.trim().is_empty() {
                cfg.naming_template = value;
            }
        }
        if let Ok(value) = env::var("ANIWORLD_FFMPEG") {
            if !value.trim().is_empty() {
                cfg.ffmpeg = value;
            }
        }
        if let Ok(value) = env::var("ANIWORLD_BANDWIDTH_LIMIT") {
            cfg.bandwidth_limit = parse_bandwidth(&value);
        }
        if let Some(value) = env_number::<usize>("ANIWORLD_JOBS") {
            cfg.jobs = value.clamp(1, 16);
        }
        if let Ok(value) = env::var("ANIWORLD_RESUME") {
            cfg.resume = !matches!(value.trim(), "0" | "false" | "no" | "off");
        }

        cfg
    }

    /// Downloads always live under a directory we create up front, so a missing
    /// mount point fails loudly instead of silently writing into `/`.
    pub fn ensure_download_path(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.download_path).with_context(|| {
            format!(
                "creating download directory {}",
                self.download_path.display()
            )
        })?;
        Ok(self.download_path.clone())
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    let raw = env::var(key).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(expand_home(raw))
}

pub fn home_dir() -> PathBuf {
    // `HOME` is the POSIX convention; Windows shells only set the others, and a
    // packaged build has to find the user profile there too.
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = env::var_os(key) {
            let path = PathBuf::from(value);
            if !path.as_os_str().is_empty() {
                return path;
            }
        }
    }
    if let (Some(drive), Some(rest)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
        let mut path = PathBuf::from(drive);
        path.push(rest);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    PathBuf::from(".")
}

pub fn expand_home(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn env_number<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok()?.trim().parse::<T>().ok()
}

/// `20M`, `512k`, `1048576` and `off` all work.
pub fn parse_bandwidth(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || matches!(value.as_str(), "0" | "off" | "none" | "unlimited") {
        return None;
    }
    let (number, multiplier) = match value.chars().last()? {
        'k' => (&value[..value.len() - 1], 1024),
        'm' => (&value[..value.len() - 1], 1024 * 1024),
        'g' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value.as_str(), 1),
    };
    let parsed: f64 = number.trim().parse().ok()?;
    if parsed <= 0.0 {
        return None;
    }
    Some((parsed * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_round_trip() {
        for lang in Language::ALL {
            assert_eq!(Language::parse(lang.label()), Some(lang));
            assert_eq!(Language::from_key(lang.key()), Some(lang));
        }
        assert_eq!(Language::parse("de"), Some(Language::GermanDub));
        assert_eq!(Language::parse("engsub"), Some(Language::EnglishSub));
        assert_eq!(Language::parse("nope"), None);
    }

    #[test]
    fn bandwidth_units() {
        assert_eq!(parse_bandwidth("20M"), Some(20 * 1024 * 1024));
        assert_eq!(parse_bandwidth("512k"), Some(512 * 1024));
        assert_eq!(parse_bandwidth("1000"), Some(1000));
        assert_eq!(parse_bandwidth("off"), None);
    }
}
