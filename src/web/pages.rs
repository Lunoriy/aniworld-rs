//! Server-rendered page skeletons. Volatile data is fetched by `app.js`; these
//! strings never interpolate user or scraped input.

use axum::response::Html;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn shell(page: &str, title: &str, body: &str) -> String {
    let items = [
        ("/", "search", "Search"),
        ("/queue", "queue", "Queue"),
        ("/library", "library", "Library"),
        ("/autosync", "autosync", "Autosync"),
        ("/settings", "settings", "Settings"),
    ];
    let active_page = if page == "series" { "search" } else { page };
    let nav = items
        .iter()
        .map(|(href, id, label)| {
            let active = if *id == active_page { " active" } else { "" };
            let count = if *id == "queue" {
                r#"<span class="count" id="queue-badge" hidden></span>"#
            } else {
                ""
            };
            format!(r#"<a class="nav-item{active}" href="{href}"><span>{label}</span>{count}</a>"#)
        })
        .collect::<String>();

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · aniworld-rs</title>
<meta name="theme-color" content="#0e1014">
<link rel="stylesheet" href="/static/app.css">
<link rel="icon" href="/static/favicon.svg" type="image/svg+xml">
</head>
<body data-page="{page}">
<div class="shell">
  <aside class="sidebar">
    <div class="brand">aniworld-rs <span>v{VERSION}</span></div>
    <nav class="nav" aria-label="Main">{nav}</nav>
    <div class="sidebar-foot">
      <div class="status-line"><span id="stat-counts">idle</span><span id="stat-speed">0 B/s</span></div>
    </div>
  </aside>
  <div class="content">
    <div class="toolbar">
      <form id="quick-search" role="search">
        <input id="quick-input" type="search" placeholder="Search…" autocomplete="off" aria-label="Search series">
      </form>
      <span class="toolbar-hint">AniWorld · SerienStream</span>
    </div>
    <main class="page">
{body}
    </main>
  </div>
</div>
<div id="toast" class="toast" role="status" aria-live="polite"></div>
<script src="/static/app.js"></script>
</body>
</html>"##
    )
}

pub async fn home() -> Html<String> {
    let body = r#"
<div class="page-header">
  <div>
    <h1>Search</h1>
    <p class="page-sub">Find a series, open it, queue episodes.</p>
  </div>
</div>

<form id="search-form" class="searchrow" role="search">
  <input id="search-input" type="search" placeholder="Series title…" autocomplete="off" autofocus aria-label="Series title">
  <select id="search-site" aria-label="Site">
    <option value="all">Both sites</option>
    <option value="aniworld">AniWorld</option>
    <option value="sto">SerienStream</option>
  </select>
  <button class="primary" type="submit">Search</button>
</form>
<p class="meta" id="search-meta"></p>
<div id="search-results"></div>

<div class="section">
  <div class="section-head"><h2>Recent downloads</h2><a href="/library">Open library</a></div>
  <div id="recent"></div>
</div>

<div class="section">
  <div class="section-head"><h2>Autosync</h2><a href="/autosync">Manage</a></div>
  <div id="home-autosync" class="kv"></div>
</div>
"#;
    Html(shell("search", "Search", body))
}

pub async fn series_page() -> Html<String> {
    let body = r#"
<div id="series-view"><p class="muted">Loading…</p></div>
"#;
    Html(shell("series", "Series", body))
}

pub async fn queue_page() -> Html<String> {
    let body = r#"
<div class="page-header">
  <div>
    <h1>Queue</h1>
    <p class="page-sub">Download engine state, newest last.</p>
  </div>
  <button id="clear-finished" type="button">Clear finished</button>
</div>
<div id="queue-stats" class="statline"></div>
<div id="queue-list"></div>
"#;
    Html(shell("queue", "Queue", body))
}

pub async fn library_page() -> Html<String> {
    let body = r#"
<div class="page-header">
  <div>
    <h1>Library</h1>
    <p class="page-sub" id="library-meta">Scanning the download folder…</p>
  </div>
  <input id="library-filter" type="search" placeholder="Filter files…" aria-label="Filter library">
</div>
<div id="library-list"></div>

<dialog id="player" class="player">
  <div class="player-head">
    <span id="player-title">Playback</span>
    <button id="player-close" type="button">Close</button>
  </div>
  <video id="player-video" controls playsinline></video>
  <p class="hint" id="player-note"></p>
</dialog>
"#;
    Html(shell("library", "Library", body))
}

pub async fn settings_page() -> Html<String> {
    let body = r#"
<div class="page-header">
  <div>
    <h1>Settings</h1>
    <p class="page-sub">Applies to new queue items; running downloads keep their snapshot.</p>
  </div>
</div>

<form id="settings-form">
  <fieldset>
    <legend>Downloads</legend>
    <div class="form-grid">
      <label class="field wide"><span>Download directory</span><input name="download_path" required></label>
      <label class="field"><span>Language</span><select name="language" id="settings-language"></select></label>
      <label class="field"><span>Preferred hoster</span><input name="provider" list="hosters" required></label>
      <label class="field wide"><span>Naming template</span><input name="naming_template" required></label>
      <p class="hint wide">Placeholders: {title} {year} {imdbid} {season} {episode} {language} {resolution}</p>
      <label class="field"><span>Episodes at once</span><input name="jobs" type="number" min="1" max="16"></label>
      <label class="field"><span>Segments per episode</span><input name="concurrency" type="number" min="1" max="64"></label>
      <label class="field"><span>HTTP retries</span><input name="retries" type="number" min="1" max="20"></label>
      <label class="field"><span>Bandwidth ceiling</span><input name="bandwidth" placeholder="e.g. 20M, 512k or empty"></label>
      <label class="field"><span>ffmpeg binary</span><input name="ffmpeg" required></label>
      <label class="check wide"><input name="resume" type="checkbox"> Resume partial downloads</label>
    </div>
    <datalist id="hosters"><option>VOE</option><option>Vidmoly</option><option>Filemoon</option><option>Streamtape</option><option>Filedon</option></datalist>
  </fieldset>

  <fieldset>
    <legend>Automation</legend>
    <div class="form-grid">
      <label class="check"><input name="autosync_enabled" type="checkbox"> Check watched series automatically</label>
      <label class="field"><span>Interval (minutes)</span><input name="autosync_interval" type="number" min="5" max="1440"></label>
    </div>
  </fieldset>

  <div class="form-footer">
    <button class="primary" type="submit">Save settings</button>
    <span class="meta" id="settings-saved"></span>
  </div>
</form>

<p class="hint" id="settings-store"></p>
"#;
    Html(shell("settings", "Settings", body))
}

pub async fn autosync_page() -> Html<String> {
    let body = r#"
<div class="page-header">
  <div>
    <h1>Autosync</h1>
    <p class="page-sub">Checks the newest season of each watched series and queues what is missing.</p>
  </div>
  <button id="autosync-run" class="primary" type="button">Check now</button>
</div>

<fieldset>
  <legend>Schedule</legend>
  <div class="inline-row">
    <label class="check"><input id="autosync-enabled" type="checkbox"> Enabled</label>
    <label class="field inline-field"><span>Interval (minutes)</span><input id="autosync-interval" type="number" min="5" max="1440"></label>
    <span class="meta" id="autosync-status"></span>
  </div>
</fieldset>

<fieldset>
  <legend>Watchlist</legend>
  <form id="watch-form" class="searchrow">
    <input id="watch-url" type="url" placeholder="https://aniworld.to/anime/stream/…" required>
    <button class="primary" type="submit">Add series</button>
  </form>
  <p class="meta" id="watch-meta"></p>
  <div id="watched-list"></div>
</fieldset>
"#;
    Html(shell("autosync", "Autosync", body))
}

pub fn error_page(status: u16, message: &str) -> Html<String> {
    let heading = match status {
        404 => "Page not found",
        500 => "Something went wrong",
        _ => "Request failed",
    };
    let body = format!(
        r#"<div class="page-header"><div><h1>{heading}</h1><p class="page-sub">{}</p></div></div>
<p><a href="/">Back to search</a></p>"#,
        html_escape::encode_text(message),
    );
    Html(shell("search", "Error", &body))
}
