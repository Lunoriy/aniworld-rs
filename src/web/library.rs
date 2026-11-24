//! Read-only view of the download directory: scan, group and resolve files safely.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

const MEDIA_EXTENSIONS: [&str; 6] = ["mkv", "mp4", "webm", "avi", "mov", "m4v"];

#[derive(Debug, Clone, Serialize)]
pub struct MediaFile {
    /// Path relative to the download root, with `/` separators.
    pub path: String,
    pub name: String,
    pub title: String,
    pub season: Option<u32>,
    pub size: u64,
    pub modified: u64,
    pub ext: String,
    /// Browsers can play these directly; MKV usually needs an external player.
    pub playable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Library {
    pub root: String,
    pub files: Vec<MediaFile>,
    pub total_bytes: u64,
}

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| MEDIA_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn scan(cfg: &Config) -> Library {
    let root = cfg.download_path.clone();
    let mut files = Vec::new();
    walk(&root, &root, 0, &mut files);
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
    let total_bytes = files.iter().map(|f| f.size).sum();
    Library {
        root: root.display().to_string(),
        files,
        total_bytes,
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<MediaFile>) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            walk(root, &path, depth + 1, out);
            continue;
        }
        if !meta.is_file() || !is_media(&path) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative_display = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        let components: Vec<&str> = relative_display.split('/').collect();
        let title = components
            .first()
            .copied()
            .unwrap_or(&relative_display)
            .to_string();
        let season = components
            .iter()
            .find_map(|part| {
                part.to_ascii_lowercase()
                    .strip_prefix("season ")
                    .and_then(|n| n.trim().parse::<u32>().ok())
                    .or_else(|| {
                        part.strip_prefix('S')
                            .and_then(|rest| rest.split('E').next())
                            .and_then(|n| n.parse::<u32>().ok())
                    })
            })
            .or_else(|| {
                relative
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(season_from_name)
            });
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        out.push(MediaFile {
            path: relative_display,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            title,
            season,
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            ext: ext.clone(),
            playable: matches!(ext.as_str(), "mp4" | "webm" | "m4v"),
        });
    }
}

fn season_from_name(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] != b'S' {
            continue;
        }
        // A season marker starts a word, so "SHOW" is not mistaken for one.
        if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            continue;
        }
        // Require the episode marker too: "S02E05" parses, a bare "S02" does not.
        let episode = bytes.get(end) == Some(&b'E')
            && bytes.get(end + 1).map(|c| c.is_ascii_digit()).unwrap_or(false);
        if episode {
            return upper[start..end].parse().ok();
        }
    }
    None
}

pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "ts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "flv" => "video/x-flv",
        _ => "application/octet-stream",
    }
}

/// Remove a file, then prune directories that became empty, never above `root`.
pub fn delete(root: &Path, relative: &str) -> std::io::Result<()> {
    let Some(path) = super::state::safe_join(root, relative) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path escapes the download directory",
        ));
    };
    std::fs::remove_file(&path)?;
    let canonical_root = root.canonicalize()?;
    let mut dir = path.parent().map(PathBuf::from);
    while let Some(current) = dir {
        if current == canonical_root || !current.starts_with(&canonical_root) {
            break;
        }
        if std::fs::remove_dir(&current).is_err() {
            break;
        }
        dir = current.parent().map(PathBuf::from);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn season_parsing() {
        assert_eq!(season_from_name("Show S02E05.mkv"), Some(2));
        assert_eq!(season_from_name("Show S2E5.mkv"), Some(2));
        assert_eq!(season_from_name("Show S02.mkv"), None);
        assert_eq!(season_from_name("Show.mkv"), None);
    }

    #[test]
    fn scans_only_media_and_skips_hidden() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("Show/Season 2")).unwrap();
        std::fs::write(temp.path().join("Show/Season 2/Show S02E01.mkv"), b"abc").unwrap();
        std::fs::write(temp.path().join("Show/notes.txt"), b"x").unwrap();
        std::fs::write(temp.path().join("Show/.hidden.mkv"), b"x").unwrap();
        let mut cfg = Config::default();
        cfg.download_path = temp.path().to_path_buf();
        let library = scan(&cfg);
        assert_eq!(library.files.len(), 1);
        assert_eq!(library.files[0].title, "Show");
        assert_eq!(library.files[0].season, Some(2));
        assert_eq!(library.files[0].size, 3);
    }
}
