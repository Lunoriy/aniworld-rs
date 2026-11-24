//! End-to-end exercise of the segment engine against a local HTTP server:
//! ordering, concurrency and resume, with no access to the real sites.

use std::sync::Arc;

use aniworld_rs::config::Config;
use aniworld_rs::download::{temp_prefix, Engine};
use aniworld_rs::mux::Fetched;
use aniworld_rs::net::Http;
use aniworld_rs::scrape::Scraper;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn segment(index: usize) -> Vec<u8> {
    let body = format!("SEG{index:03}-{}", "x".repeat(700));
    body.into_bytes()
}

async fn serve(segments: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let segments = segments.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                let body: Vec<u8> = match path.as_str() {
                    "/master.m3u8" => {
                        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4000000\nv.m3u8\n".into()
                    }
                    "/v.m3u8" => {
                        let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
                        for index in 0..segments.len() {
                            text.push_str("#EXTINF:4.0,\n");
                            text.push_str(&format!("seg{index}.ts\n"));
                        }
                        text.push_str("#EXT-X-ENDLIST\n");
                        text.into_bytes()
                    }
                    other if other.starts_with("/seg") => {
                        let index: usize = other
                            .trim_start_matches("/seg")
                            .trim_end_matches(".ts")
                            .parse()
                            .unwrap_or(0);
                        segments.get(index).cloned().unwrap_or_default()
                    }
                    _ => Vec::new(),
                };

                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    format!("http://{addr}")
}

fn engine(concurrency: usize) -> Engine {
    let cfg = Arc::new(Config {
        concurrency,
        retries: 2,
        resume: true,
        ..Config::default()
    });
    let http = Arc::new(Http::new(&cfg.user_agent, cfg.retries).unwrap());
    let scraper = Arc::new(Scraper::new((*http).clone()));
    let (events, _rx) = mpsc::channel(256);
    Engine::new(cfg, http, scraper, events)
}

#[tokio::test]
async fn fetches_every_segment_in_playlist_order() {
    let segments: Vec<Vec<u8>> = (0..24).map(segment).collect();
    let base = serve(segments.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Episode S01E001.mkv");

    let fetched = engine(8)
        .download_stream(1, "S01E001", &format!("{base}/master.m3u8"), &dest, None)
        .await
        .unwrap();

    let Fetched::Playlists { video, audio } = fetched else {
        panic!("expected playlist output");
    };
    assert!(audio.is_none());
    let data = tokio::fs::read(&video).await.unwrap();
    let expected: Vec<u8> = segments.concat();
    assert_eq!(data.len(), expected.len());
    assert!(data == expected, "segments were not written in order");
}

#[tokio::test]
async fn resumes_from_a_saved_partial_download() {
    let segments: Vec<Vec<u8>> = (0..16).map(segment).collect();
    let base = serve(segments.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Episode S01E002.mkv");
    let stream_url = format!("{base}/master.m3u8");

    let (temp_dir, prefix) = temp_prefix(&dest, &stream_url);
    tokio::fs::create_dir_all(&temp_dir).await.unwrap();
    let video = temp_dir.join(format!("{prefix}.video.ts"));

    let already: Vec<u8> = segments[..6].concat();
    tokio::fs::write(&video, &already).await.unwrap();
    let state = serde_json::json!({
        "url": format!("{base}/v.m3u8"),
        "segments_done": 6,
        "bytes": already.len(),
        "total": 16,
    });
    tokio::fs::write(
        temp_dir.join(format!("{prefix}.video.ts.part.json")),
        serde_json::to_vec(&state).unwrap(),
    )
    .await
    .unwrap();

    let fetched = engine(4)
        .download_stream(2, "S01E002", &stream_url, &dest, None)
        .await
        .unwrap();
    let Fetched::Playlists { video, .. } = fetched else {
        panic!("expected playlist output");
    };
    let data = tokio::fs::read(&video).await.unwrap();
    assert_eq!(data, segments.concat());
}
