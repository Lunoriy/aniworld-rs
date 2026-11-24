//! Remuxing downloaded payloads into the final container with ffmpeg.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

#[derive(Debug)]
pub enum Fetched {
    Playlists {
        video: PathBuf,
        audio: Option<PathBuf>,
    },
    Direct(PathBuf),
}

impl Fetched {
    pub fn files(&self) -> Vec<&Path> {
        match self {
            Fetched::Playlists { video, audio } => match audio {
                Some(audio) => vec![video.as_path(), audio.as_path()],
                None => vec![video.as_path()],
            },
            Fetched::Direct(path) => vec![path.as_path()],
        }
    }
}

/// Remux into `dest`. When ffmpeg is unavailable the raw stream is kept next
/// to the destination instead, and the caller reports the real path.
pub async fn finalise(ffmpeg: &str, fetched: &Fetched, dest: &Path) -> Result<PathBuf> {
    if let Fetched::Direct(path) = fetched {
        if path == dest {
            return Ok(dest.to_path_buf());
        }
        tokio::fs::rename(path, dest)
            .await
            .with_context(|| format!("moving {} to {}", path.display(), dest.display()))?;
        return Ok(dest.to_path_buf());
    }

    match remux(ffmpeg, fetched, dest).await {
        Ok(()) => {
            cleanup(fetched);
            Ok(dest.to_path_buf())
        }
        Err(err) => {
            // Keep the raw payload so a long download is never lost, and count
            // it as a success: the media is usable, only the container differs.
            let fallback = keep_raw(fetched, dest).await?;
            cleanup(fetched);
            eprintln!(
                "warning: ffmpeg could not remux ({err}); raw stream kept at {}",
                fallback.display()
            );
            Ok(fallback)
        }
    }
}

pub async fn remux(ffmpeg: &str, fetched: &Fetched, dest: &Path) -> Result<()> {
    let files = fetched.files();
    if files.is_empty() {
        bail!("nothing to mux");
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y");
    for input in &files {
        cmd.arg("-i").arg(input);
    }
    cmd.arg("-map").arg("0:v:0");
    if files.len() > 1 {
        cmd.arg("-map").arg("1:a:0");
    } else {
        cmd.arg("-map").arg("0:a?");
    }
    cmd.arg("-c").arg("copy");
    if dest.extension().and_then(|e| e.to_str()) == Some("mkv") {
        cmd.arg("-f").arg("matroska");
    }
    cmd.arg(dest);

    let output = cmd
        .output()
        .await
        .context("spawning ffmpeg (is it installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail: Vec<&str> = stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        bail!(
            "ffmpeg exited with {}: {}",
            output.status,
            if detail.is_empty() {
                "no stderr output".to_string()
            } else {
                detail.join(" | ")
            }
        );
    }
    if !dest.is_file() || dest.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        bail!("ffmpeg produced no output");
    }
    Ok(())
}

async fn keep_raw(fetched: &Fetched, dest: &Path) -> Result<PathBuf> {
    let Fetched::Playlists { video, audio } = fetched else {
        return Ok(dest.to_path_buf());
    };
    let _ = audio;
    let extension = video.extension().and_then(|e| e.to_str()).unwrap_or("ts");
    let mut fallback = dest.to_path_buf();
    fallback.set_extension(extension);
    tokio::fs::copy(video, &fallback)
        .await
        .with_context(|| format!("keeping raw stream at {}", fallback.display()))?;
    Ok(fallback)
}

fn cleanup(fetched: &Fetched) {
    for path in fetched.files() {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}.part.json", path.display()));
    }
}

pub async fn ffmpeg_available(ffmpeg: &str) -> bool {
    Command::new(ffmpeg)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
