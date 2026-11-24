//! Remux coverage. Skipped automatically on machines without ffmpeg, since
//! the binary is an external runtime dependency rather than a crate.

use std::path::Path;
use std::process::Command;

use aniworld_rs::mux::{self, Fetched};

fn ffmpeg_path() -> String {
    std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

async fn make_media(ffmpeg: &str, path: &Path, lavfi: &str, codec: &str) {
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            lavfi,
            "-c",
            codec,
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawning ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate {lavfi}");
}

#[tokio::test]
async fn remuxes_video_and_audio_into_mkv() {
    let ffmpeg = ffmpeg_path();
    if !mux::ffmpeg_available(&ffmpeg).await {
        eprintln!("skipping: ffmpeg not available (set FFMPEG=/path/to/ffmpeg)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("video.ts");
    let audio = dir.path().join("audio.ts");
    make_media(&ffmpeg, &video, "testsrc=size=128x72:rate=10:duration=1", "mpeg2video").await;
    make_media(&ffmpeg, &audio, "sine=frequency=440:duration=1", "mp2").await;

    let dest = dir.path().join("Episode.mkv");
    let fetched = Fetched::Playlists {
        video: video.clone(),
        audio: Some(audio.clone()),
    };
    mux::remux(&ffmpeg, &fetched, &dest).await.unwrap();

    assert!(dest.is_file());
    assert!(std::fs::metadata(&dest).unwrap().len() > 0);

    // `finalise` also removes the intermediate playlist payloads.
    let dest2 = dir.path().join("Episode2.mkv");
    mux::finalise(&ffmpeg, &fetched, &dest2).await.unwrap();
    assert!(dest2.is_file());
    assert!(!video.exists(), "video temp file was not cleaned up");
    assert!(!audio.exists(), "audio temp file was not cleaned up");
}

#[tokio::test]
async fn direct_payloads_are_moved_into_place() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("stream.part");
    std::fs::write(&source, b"payload").unwrap();
    let dest = dir.path().join("Episode.mp4");

    let path = mux::finalise("ffmpeg-does-not-matter", &Fetched::Direct(source.clone()), &dest)
        .await
        .unwrap();
    assert_eq!(path, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
    assert!(!source.exists());
}
