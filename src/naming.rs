//! Jellyfin-style library layout, replacing the Python `{title}` template
//! formatting with a compile-checked renderer that always produces safe paths.

use std::path::{Path, PathBuf};

pub const DEFAULT_TEMPLATE: &str =
    "{title} ({year}) [imdbid-{imdbid}]/Season {season}/{title} S{season}E{episode}.mkv";

#[derive(Debug, Clone, Default)]
pub struct NamingValues {
    pub title: String,
    pub year: String,
    pub imdbid: String,
    pub season: u32,
    pub episode: u32,
    pub language: String,
    pub resolution: String,
}

/// Render a template like
/// `{title} ({year}) [imdbid-{imdbid}]/Season {season}/{title} S{season}E{episode}.mkv`
/// into a path below `root`.
pub fn render(root: &Path, template: &str, values: &NamingValues) -> PathBuf {
    let season = format!("{:02}", values.season);
    let episode = format!("{:03}", values.episode);
    let replacements = [
        ("{title}", values.title.as_str()),
        ("{year}", values.year.as_str()),
        ("{imdbid}", values.imdbid.as_str()),
        ("{season}", season.as_str()),
        ("{episode}", episode.as_str()),
        ("{language}", values.language.as_str()),
        ("{resolution}", values.resolution.as_str()),
    ];

    let mut path = root.to_path_buf();
    let parts: Vec<&str> = template.split('/').filter(|p| !p.is_empty()).collect();
    let Some((file, folders)) = parts.split_last() else {
        return path.join(sanitize_component(&values.title)).with_extension("mkv");
    };

    for folder in folders {
        let mut rendered = folder.to_string();
        for (key, value) in &replacements {
            rendered = rendered.replace(key, value);
        }
        let rendered = tidy(&rendered);
        if !rendered.is_empty() {
            path.push(sanitize_component(&rendered));
        }
    }

    let mut filename = file.to_string();
    for (key, value) in &replacements {
        filename = filename.replace(key, value);
    }
    let mut filename = tidy(&filename);
    if filename.is_empty() {
        filename = format!("S{season}E{episode}");
    }
    path.push(sanitize_component(&filename));
    path
}

/// Drop placeholders that resolved to nothing, e.g. `Goblin Slayer () [imdbid-]`.
fn tidy(value: &str) -> String {
    let mut out = value.to_string();
    // Only collapse decoration that is left empty by a missing value; keep
    // intentional separators such as "Title - 003" untouched.
    loop {
        let before = out.clone();
        for empty in ["()", "( )", "[]", "[ ]"] {
            out = out.replace(empty, " ");
        }
        for dead in [" [imdbid-]", "[imdbid-]", " (imdbid-)", "(imdbid-)"] {
            out = out.replace(dead, " ");
        }
        while out.contains("  ") {
            out = out.replace("  ", " ");
        }
        if out == before {
            break;
        }
    }
    out.trim()
        .trim_end_matches(|c: char| c.is_whitespace() || c == '-')
        .trim()
        .to_string()
}

/// Linux forbids `/` and NUL in a component. Everything else (colons, umlauts,
/// emoji) is legal, so it is kept to preserve readable titles.
pub fn sanitize_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '/' | '\0' => out.push('_'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|e| e.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> NamingValues {
        NamingValues {
            title: "Goblin Slayer".into(),
            year: "2018-2023".into(),
            imdbid: "tt8690728".into(),
            season: 1,
            episode: 3,
            language: "German Dub".into(),
            resolution: "unknown".into(),
        }
    }

    #[test]
    fn renders_default_layout() {
        let path = render(Path::new("/dl"), DEFAULT_TEMPLATE, &values());
        assert_eq!(
            path,
            PathBuf::from(
                "/dl/Goblin Slayer (2018-2023) [imdbid-tt8690728]/Season 01/Goblin Slayer S01E003.mkv"
            )
        );
    }

    #[test]
    fn missing_metadata_is_cleaned_up() {
        let mut v = values();
        v.year = String::new();
        v.imdbid = String::new();
        let path = render(Path::new("/dl"), DEFAULT_TEMPLATE, &v);
        assert_eq!(
            path,
            PathBuf::from("/dl/Goblin Slayer/Season 01/Goblin Slayer S01E003.mkv")
        );
    }

    #[test]
    fn template_without_folders_is_supported() {
        let path = render(Path::new("/dl"), "{title} - {episode}.mkv", &values());
        assert_eq!(path, PathBuf::from("/dl/Goblin Slayer - 003.mkv"));
    }

    #[test]
    fn path_separators_are_sanitized() {
        let mut v = values();
        v.title = "Fate/Grand Order".into();
        let path = render(Path::new("/dl"), "{title}/S{season}/{title}.mkv", &v);
        assert_eq!(
            path,
            PathBuf::from("/dl/Fate_Grand Order/S01/Fate_Grand Order.mkv")
        );
    }
}
