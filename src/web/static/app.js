'use strict';

const $ = (selector, root = document) => root.querySelector(selector);
const pad = (n, width = 2) => String(n).padStart(width, '0');
const enc = encodeURIComponent;

function h(tag, attrs = {}, ...kids) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on') && typeof value === 'function') node.addEventListener(key.slice(2), value);
    else if (key === 'checked' || key === 'disabled' || key === 'selected') node[key] = Boolean(value);
    else node.setAttribute(key, value);
  }
  for (const kid of kids.flat(3)) {
    if (kid === null || kid === undefined || kid === false) continue;
    node.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return node;
}

function fmtBytes(bytes) {
  if (!bytes) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes, index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index += 1; }
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}
const fmtSpeed = (bytes) => (bytes > 0 ? `${fmtBytes(bytes)}/s` : '0 B/s');

function fmtEta(seconds) {
  if (!seconds || seconds <= 0 || !Number.isFinite(seconds)) return '—';
  const total = Math.round(seconds);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  return hours ? `${hours}:${pad(minutes)}:${pad(secs)}` : `${pad(minutes)}:${pad(secs)}`;
}

function fmtDate(seconds) {
  if (!seconds) return '—';
  return new Date(seconds * 1000).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}

const LANG_SHORT = {
  'German Dub': 'DE',
  'German Sub': 'DE-SUB',
  'English Dub': 'EN',
  'English Sub': 'EN-SUB',
};

async function api(path, { method = 'GET', body } = {}) {
  const response = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body ? { 'content-type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  let data = {};
  try { data = await response.json(); } catch { /* empty body */ }
  if (!response.ok || data.ok === false) {
    throw new Error(data.error || `${response.status} ${response.statusText}`);
  }
  return data;
}

let toastTimer = null;
function toast(message, kind = 'info') {
  const node = $('#toast');
  if (!node) return;
  node.textContent = message;
  node.classList.toggle('error', kind === 'error');
  node.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.remove('show'), kind === 'error' ? 6000 : 3500);
}

const errorBox = (message) => h('div', { class: 'error-box' }, message);

function busy(button, on) {
  if (!button) return;
  button.disabled = on;
  if (on) {
    button.dataset.label = button.textContent;
    button.textContent = '…';
  } else if (button.dataset.label) {
    button.textContent = button.dataset.label;
  }
}

async function refreshStatus() {
  try {
    const status = await api('/api/status');
    const active = status.counts.running + status.counts.queued;
    const counts = $('#stat-counts');
    if (counts) {
      counts.textContent = active
        ? `${status.counts.running}/${status.counts.queued} run/queue`
        : 'idle';
    }
    const speed = $('#stat-speed');
    if (speed) speed.textContent = fmtSpeed(status.speed);
    const badge = $('#queue-badge');
    if (badge) {
      badge.textContent = active ? String(active) : '';
      badge.hidden = active === 0;
    }
  } catch { /* header poll is best-effort */ }
}

function bindQuickSearch() {
  const form = $('#quick-search');
  if (!form) return;
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    const term = $('#quick-input').value.trim();
    if (!term) return;
    if (document.body.dataset.page === 'search') {
      $('#search-input').value = term;
      $('#search-form').requestSubmit();
    } else {
      location.href = `/?q=${enc(term)}`;
    }
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === '/' && !['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement.tagName)) {
      event.preventDefault();
      const input = $('#quick-input') || $('#search-input');
      if (input) { input.focus(); input.select(); }
    }
  });
}

/* ---------------- search ---------------- */

function initHome() {
  const input = $('#search-input');
  const site = $('#search-site');
  const meta = $('#search-meta');
  const root = $('#search-results');

  async function runSearch(term) {
    if (term.length < 2) { toast('Type at least two characters', 'error'); return; }
    history.replaceState(null, '', `/?q=${enc(term)}`);
    meta.textContent = 'Searching…';
    root.replaceChildren();
    try {
      const data = await api(`/api/search?q=${enc(term)}&site=${site.value}`);
      const suffix = data.errors.length ? ` · ${data.errors.join(' · ')}` : '';
      meta.textContent = `${data.hits.length} result(s)${suffix}`;
      if (!data.hits.length) {
        root.append(h('div', { class: 'empty' }, 'No results. Try the other site or a shorter title.'));
        return;
      }
      root.append(h('div', { class: 'results' },
        ...data.hits.map((hit) => h('a', { class: 'result', href: `/series?url=${enc(hit.url)}` },
          h('span', { class: 'site' }, hit.site === 'SerienStream' ? 'serienstream' : 'aniworld'),
          h('div', {},
            h('h3', {}, hit.title),
            hit.description ? h('p', {}, hit.description) : null,
          ),
          h('span', { class: 'go' }, '→'),
        )),
      ));
    } catch (err) {
      meta.textContent = '';
      root.replaceChildren(errorBox(err.message));
    }
  }

  $('#search-form').addEventListener('submit', (event) => {
    event.preventDefault();
    runSearch(input.value.trim());
  });

  const initial = new URLSearchParams(location.search).get('q');
  if (initial) { input.value = initial; runSearch(initial.trim()); }

  api('/api/library').then((library) => {
    const recent = $('#recent');
    const files = library.files.slice(0, 6);
    if (!files.length) {
      recent.append(h('div', { class: 'empty' }, 'Nothing on disk yet.'));
      return;
    }
    recent.append(h('div', { class: 'results' },
      ...files.map((file) => h('a', { class: 'result', href: '/library' },
        h('span', { class: 'site' }, fmtBytes(file.size)),
        h('div', {},
          h('h3', {}, file.name),
          h('p', {}, `${file.title}${file.season ? ` · Season ${pad(file.season)}` : ''} · ${fmtDate(file.modified)}`),
        ),
        h('span', { class: 'go' }, '→'),
      )),
    ));
  }).catch(() => {});

  api('/api/autosync').then((data) => {
    const box = $('#home-autosync');
    const enabled = data.watched.filter((entry) => entry.enabled).length;
    box.append(
      h('span', {}, 'Status ', h('b', {}, data.enabled ? 'on' : 'off')),
      h('span', {}, 'Watched ', h('b', {}, String(data.watched.length)), ` (${enabled} on)`),
      h('span', {}, 'Next ', h('b', {}, data.next_in === null ? '—' : data.next_in === 0 ? 'due' : fmtEta(data.next_in))),
    );
  }).catch(() => {});
}

/* ---------------- series ---------------- */

function posterFor(series) {
  let src = series.poster || '';
  if (src.startsWith('//')) src = `https:${src}`;
  const image = h('img', { alt: '', loading: 'lazy' });
  if (src) image.src = `/img?url=${enc(src)}`;
  image.addEventListener('error', () => image.remove());
  return h('div', { class: 'poster' }, image);
}

async function initSeries() {
  const view = $('#series-view');
  const target = new URLSearchParams(location.search).get('url');
  if (!target) { view.replaceChildren(errorBox('No series URL.')); return; }

  let series, settings;
  try {
    const [seriesData, settingsData] = await Promise.all([
      api(`/api/series?url=${enc(target)}`),
      api('/api/settings'),
    ]);
    series = seriesData.series;
    settings = settingsData.settings;
  } catch (err) {
    view.replaceChildren(errorBox(err.message));
    return;
  }
  document.title = `${series.title} · aniworld-rs`;

  const seasons = series.seasons;
  let current = null;
  let episodes = [];
  let selected = new Set();
  let language = settings.language;
  let provider = settings.provider;

  const tabs = h('div', { class: 'tabs' });
  const languageSelect = h('select', { 'aria-label': 'Language' });
  const providerInput = h('input', { value: provider, list: 'hosters', 'aria-label': 'Hoster', style: 'width:130px' });
  const count = h('span', { class: 'sel' }, '0 selected');
  const selectAll = h('input', { type: 'checkbox', 'aria-label': 'Select all episodes' });
  const downloadSelected = h('button', { class: 'primary', type: 'button', disabled: true }, 'Download selected');
  const downloadSeason = h('button', { type: 'button' }, 'Download season');
  const tbody = h('tbody');
  const table = h('table', {},
    h('thead', {}, h('tr', {},
      h('th', { class: 'w-check' }, ''),
      h('th', {}, 'Episode'),
      h('th', {}, 'Title'),
      h('th', {}, 'Languages'),
      h('th', { class: 'w-action' }, ''),
    )),
    tbody,
  );

  const toolbar = h('div', { class: 'toolbar-inline' },
    h('label', { class: 'check' }, selectAll, 'All'),
    languageSelect,
    providerInput,
    h('datalist', { id: 'hosters' }, ['VOE', 'Vidmoly', 'Filemoon', 'Streamtape', 'Filedon'].map((n) => h('option', {}, n))),
    count,
    h('span', { class: 'spacer' }),
    downloadSeason,
    downloadSelected,
  );

  view.replaceChildren(
    h('div', { class: 'series-head' },
      posterFor(series),
      h('div', {},
        h('h1', {}, series.title),
        h('div', { class: 'facts' },
          h('span', {}, series.year || '—'),
          series.imdb ? h('span', {}, `IMDb ${series.imdb}`) : null,
          ...(series.genres || []).slice(0, 5).map((genre) => h('span', {}, genre)),
          h('a', { href: series.url, target: '_blank', rel: 'noopener' }, 'site ↗'),
        ),
        series.description ? h('p', { class: 'desc' }, series.description) : null,
      ),
    ),
    tabs,
    toolbar,
    table,
  );

  function paint() {
    count.textContent = `${selected.size} selected`;
    downloadSelected.disabled = selected.size === 0;
    const all = episodes.length > 0 && selected.size === episodes.length;
    selectAll.checked = all;
    selectAll.indeterminate = selected.size > 0 && !all;
  }

  function renderEpisodes(data) {
    const languages = [...new Set(data.episodes.flatMap((episode) => episode.languages || []))];
    if (!languages.includes(language)) language = languages[0] || language;
    languageSelect.replaceChildren(...(languages.length ? languages : [language]).map((label) =>
      h('option', { value: label, selected: label === language }, label)));

    episodes = data.episodes;
    selected = new Set();
    tbody.replaceChildren(...episodes.map((episode) => {
      const checkbox = h('input', { type: 'checkbox', 'data-episode': episode.number, 'aria-label': `Select ${episode.label}` });
      checkbox.addEventListener('change', () => {
        if (checkbox.checked) selected.add(episode.number); else selected.delete(episode.number);
        checkbox.closest('tr').classList.toggle('selected', checkbox.checked);
        paint();
      });
      return h('tr', {},
        h('td', {}, checkbox),
        h('td', { class: 'nowrap' },
          h('span', { class: 'num' }, `S${pad(current.number)}E${pad(episode.absolute, 3)}`),
          episode.absolute !== episode.number
            ? h('div', { class: 'stage' }, `site E${pad(episode.number)}`)
            : null,
        ),
        h('td', { class: 'ep-title' }, episode.title || 'Untitled'),
        h('td', { class: 'nowrap' },
          ...(episode.languages || []).map((label) => h('span', { class: 'tag lang', style: 'margin-right:4px' }, LANG_SHORT[label] || label)),
        ),
        h('td', { class: 'actions' },
          h('button', { class: 'small', type: 'button', onclick: () => queue([episode.number], false, null) }, 'Download'),
        ),
      );
    }));
    if (!episodes.length) {
      tbody.replaceChildren(h('tr', {}, h('td', { colspan: 5 }, h('div', { class: 'empty' }, 'No episodes listed.'))));
    }
    paint();
  }

  async function queueDownload(numbers, all, button) {
    if (!current) return;
    busy(button, true);
    const payload = { url: current.url, language, provider, all };
    if (!all) payload.episodes = numbers;
    try {
      const result = await api('/api/download', { method: 'POST', body: payload });
      toast(`Queued ${result.queued} episode(s)${result.skipped.length ? `, skipped ${result.skipped.length}` : ''}`);
      setTimeout(() => { location.href = '/queue'; }, 500);
    } catch (err) {
      toast(err.message, 'error');
    } finally {
      busy(button, false);
    }
  }
  const queue = queueDownload;

  async function loadSeason(season) {
    if (!season) return;
    current = season;
    for (const [index, tab] of [...tabs.children].entries()) {
      tab.classList.toggle('active', seasons[index] === season);
    }
    tbody.replaceChildren(h('tr', {}, h('td', { colspan: 5 }, h('div', { class: 'loading' }, 'Loading episodes…'))));
    try {
      const data = await api(`/api/season?url=${enc(season.url)}`);
      renderEpisodes(data);
    } catch (err) {
      tbody.replaceChildren(h('tr', {}, h('td', { colspan: 5 }, errorBox(err.message))));
    }
  }

  tabs.addEventListener('click', (event) => {
    const tab = event.target.closest('.tab');
    if (tab) loadSeason(seasons[Number(tab.dataset.index)]);
  });
  languageSelect.addEventListener('change', () => { language = languageSelect.value; });
  providerInput.addEventListener('change', () => { provider = providerInput.value.trim(); });
  selectAll.addEventListener('change', () => {
    selected = selectAll.checked ? new Set(episodes.map((episode) => episode.number)) : new Set();
    for (const row of tbody.querySelectorAll('tr')) {
      const box = row.querySelector('input[data-episode]');
      if (!box) continue;
      box.checked = selected.has(Number(box.dataset.episode));
      row.classList.toggle('selected', box.checked);
    }
    paint();
  });
  downloadSelected.addEventListener('click', () => queueDownload([...selected], false, downloadSelected));
  downloadSeason.addEventListener('click', () => queueDownload([], true, downloadSeason));

  if (!seasons.length) {
    view.append(h('div', { class: 'empty' }, 'This series has no seasons.'));
    return;
  }
  seasons.forEach((season, index) => {
    tabs.append(h('button', {
      class: 'tab',
      type: 'button',
      'data-index': index,
    }, season.are_movies ? 'Movies' : `Season ${pad(season.number)}`));
  });
  loadSeason(seasons.find((season) => !season.are_movies) || seasons[0]);
}

/* ---------------- queue ---------------- */

function initQueue() {
  const root = $('#queue-list');
  const stats = $('#queue-stats');
  let fallbackTimer = null;

  function buildJob(job) {
    const row = h('tr', { class: job.state, 'data-id': job.id, 'data-state': job.state });
    row.append(
      h('td', {},
        h('div', {}, job.title || 'Unknown series'),
        h('div', { class: 'stage' }, `S${pad(job.season)}E${pad(job.number, 3)} · `,
          h('span', { 'data-stage': '' }, job.stage)),
      ),
      h('td', {}, h('span', { class: 'tag lang' }, LANG_SHORT[job.language] || job.language)),
      h('td', {}, h('span', { class: `tag ${job.state}` }, job.state)),
      h('td', {},
        h('div', { class: 'progress' },
          h('span', { class: 'track' }, h('span', { class: 'fill', 'data-fill': '' })),
          h('span', { class: 'pct', 'data-pct': '' }),
        ),
      ),
      h('td', { class: 'num', 'data-speed': '' }),
      h('td', { class: 'num', 'data-eta': '' }),
      h('td', { class: 'actions' },
        ['running', 'queued'].includes(job.state)
          ? h('button', { class: 'small', type: 'button', 'data-action': 'cancel' }, 'Cancel')
          : null,
        ['failed', 'cancelled', 'done'].includes(job.state)
          ? h('button', { class: 'small', type: 'button', 'data-action': 'retry' }, 'Retry')
          : null,
        h('button', { class: 'small danger', type: 'button', 'data-action': 'remove' }, 'Remove'),
      ),
    );
    if (job.error) {
      row.querySelector('td').append(h('div', { class: 'error-line' }, job.error));
    }
    updateJob(row, job);
    return row;
  }

  function updateJob(row, job) {
    const fill = row.querySelector('[data-fill]');
    if (fill) fill.style.width = `${job.percent || 0}%`;
    const pct = row.querySelector('[data-pct]');
    if (pct) pct.textContent = `${(job.percent || 0).toFixed(0)}%`;
    const speed = row.querySelector('[data-speed]');
    if (speed) speed.textContent = job.state === 'running' ? fmtSpeed(job.speed) : '—';
    const eta = row.querySelector('[data-eta]');
    if (eta) eta.textContent = job.state === 'running' ? fmtEta(job.eta) : '—';
    const stage = row.querySelector('[data-stage]');
    if (stage) stage.textContent = job.stage;
  }

  function render(snapshot) {
    const jobs = snapshot.jobs || [];
    if (!jobs.length) {
      root.replaceChildren(h('div', { class: 'empty' }, 'Queue is empty.'));
    } else {
      let table = root.querySelector('table');
      if (!table) {
        table = h('table', {},
          h('thead', {}, h('tr', {},
            h('th', {}, 'Episode'),
            h('th', {}, 'Lang'),
            h('th', {}, 'State'),
            h('th', {}, 'Progress'),
            h('th', {}, 'Speed'),
            h('th', {}, 'ETA'),
            h('th', { class: 'w-action' }, ''),
          )),
          h('tbody'),
        );
        root.replaceChildren(table);
      }
      const body = table.querySelector('tbody');
      const seen = new Set();
      for (const job of jobs) {
        seen.add(String(job.id));
        let row = body.querySelector(`tr[data-id="${job.id}"]`);
        if (!row || row.dataset.state !== job.state) {
          const built = buildJob(job);
          if (row) row.replaceWith(built); else body.append(built);
        } else {
          updateJob(row, job);
        }
      }
      for (const row of [...body.querySelectorAll('tr')]) {
        if (!seen.has(row.dataset.id)) row.remove();
      }
    }

    const counts = snapshot.counts || {};
    stats.replaceChildren(
      h('span', {}, h('b', {}, String(counts.running || 0)), ' running'),
      h('span', {}, h('b', {}, String(counts.queued || 0)), ' queued'),
      h('span', {}, h('b', {}, String(counts.done || 0)), ' done'),
      h('span', {}, h('b', {}, String(counts.failed || 0)), ' failed'),
      h('span', { class: 'speed' }, fmtSpeed(snapshot.speed || 0)),
    );
  }

  api('/api/queue').then(render).catch((err) => root.replaceChildren(errorBox(err.message)));

  if (window.EventSource) {
    const source = new EventSource('/api/queue/events');
    source.addEventListener('queue', (event) => {
      try { render(JSON.parse(event.data)); } catch { /* ignore */ }
    });
    source.onerror = () => {
      if (fallbackTimer) return;
      fallbackTimer = setInterval(() => api('/api/queue').then(render).catch(() => {}), 3000);
    };
    source.onopen = () => {
      if (fallbackTimer) { clearInterval(fallbackTimer); fallbackTimer = null; }
    };
    window.addEventListener('beforeunload', () => source.close());
  } else {
    setInterval(() => api('/api/queue').then(render).catch(() => {}), 2000);
  }

  root.addEventListener('click', async (event) => {
    const button = event.target.closest('button[data-action]');
    if (!button) return;
    const id = button.closest('tr').dataset.id;
    busy(button, true);
    try {
      await api(`/api/queue/${id}/${button.dataset.action}`, { method: 'POST' });
      render(await api('/api/queue'));
    } catch (err) {
      toast(err.message, 'error');
    } finally {
      busy(button, false);
    }
  });

  $('#clear-finished').addEventListener('click', async () => {
    try {
      await api('/api/queue/clear', { method: 'POST' });
      render(await api('/api/queue'));
    } catch (err) {
      toast(err.message, 'error');
    }
  });
}

/* ---------------- library ---------------- */

function initLibrary() {
  const root = $('#library-list');
  const meta = $('#library-meta');
  const filter = $('#library-filter');
  const dialog = $('#player');
  const video = $('#player-video');
  const note = $('#player-note');
  let files = [];

  function play(file) {
    $('#player-title').textContent = file.name;
    note.textContent = file.playable
      ? `${fmtBytes(file.size)} · ${file.path}`
      : 'Browsers cannot always decode MKV. Use Save and open it in VLC/MPV.';
    video.src = `/media?path=${enc(file.path)}`;
    dialog.showModal();
    video.play().catch(() => {});
  }

  function close() {
    video.pause();
    video.removeAttribute('src');
    video.load();
    dialog.close();
  }

  function render() {
    const term = filter.value.toLowerCase().trim();
    const shown = files.filter((file) => !term || `${file.title} ${file.name}`.toLowerCase().includes(term));
    if (!shown.length) {
      root.replaceChildren(h('div', { class: 'empty' }, files.length ? 'No matches.' : 'No media on disk.'));
      return;
    }
    root.replaceChildren(h('table', {},
      h('thead', {}, h('tr', {},
        h('th', {}, 'File'),
        h('th', {}, 'Series'),
        h('th', {}, 'Season'),
        h('th', { class: 'right' }, 'Size'),
        h('th', {}, 'Added'),
        h('th', { class: 'w-action' }, ''),
      )),
      h('tbody', {}, ...shown.map((file) => h('tr', {},
        h('td', {}, h('div', { class: 'file-name' }, file.name, h('small', {}, file.ext.toUpperCase()))),
        h('td', { class: 'nowrap' }, file.title),
        h('td', { class: 'num' }, file.season ? pad(file.season) : '—'),
        h('td', { class: 'right num' }, fmtBytes(file.size)),
        h('td', { class: 'num' }, fmtDate(file.modified)),
        h('td', { class: 'actions' },
          h('button', { class: 'small', type: 'button', onclick: () => play(file) }, 'Play'),
          h('a', { class: 'button small', href: `/media?download=1&path=${enc(file.path)}` }, 'Save'),
          h('button', { class: 'small danger', type: 'button', onclick: async () => {
            if (!confirm(`Delete ${file.name}?`)) return;
            try {
              await api('/api/library/delete', { method: 'POST', body: { path: file.path } });
              files = files.filter((entry) => entry.path !== file.path);
              render();
              toast('Deleted');
            } catch (err) { toast(err.message, 'error'); }
          } }, 'Delete'),
        ),
      ))),
    ));
  }

  $('#player-close').addEventListener('click', close);
  dialog.addEventListener('cancel', close);
  dialog.addEventListener('click', (event) => { if (event.target === dialog) close(); });
  filter.addEventListener('input', render);

  api('/api/library').then((library) => {
    files = library.files;
    meta.textContent = `${files.length} file(s) · ${fmtBytes(library.total_bytes)} · ${library.root}`;
    render();
  }).catch((err) => {
    meta.textContent = 'Scan failed';
    root.replaceChildren(errorBox(err.message));
  });
}

/* ---------------- settings ---------------- */

function initSettings() {
  const form = $('#settings-form');
  const saved = $('#settings-saved');
  const languageSelect = $('#settings-language');

  api('/api/settings').then((data) => {
    languageSelect.replaceChildren(...data.languages.map((language) =>
      h('option', { value: language.label }, language.label)));
    for (const [name, value] of Object.entries(data.settings)) {
      const field = form.elements.namedItem(name);
      if (!field) continue;
      if (field.type === 'checkbox') field.checked = Boolean(value);
      else field.value = value ?? '';
    }
    $('#settings-store').textContent = `Stored in ${data.store}`;
  }).catch((err) => toast(err.message, 'error'));

  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const button = form.querySelector('button[type="submit"]');
    busy(button, true);
    const patch = {};
    for (const name of ['download_path', 'language', 'provider', 'naming_template', 'ffmpeg', 'bandwidth']) {
      patch[name] = form.elements.namedItem(name).value.trim();
    }
    for (const name of ['jobs', 'concurrency', 'retries', 'autosync_interval']) {
      patch[name] = Number(form.elements.namedItem(name).value);
    }
    patch.resume = form.elements.namedItem('resume').checked;
    patch.autosync_enabled = form.elements.namedItem('autosync_enabled').checked;
    try {
      await api('/api/settings', { method: 'PUT', body: patch });
      saved.textContent = `Saved ${new Date().toLocaleTimeString()}`;
      toast('Settings saved');
    } catch (err) {
      toast(err.message, 'error');
    } finally {
      busy(button, false);
    }
  });
}

/* ---------------- autosync ---------------- */

function initAutosync() {
  const enabled = $('#autosync-enabled');
  const interval = $('#autosync-interval');
  const status = $('#autosync-status');
  const root = $('#watched-list');
  const meta = $('#watch-meta');
  let watched = [];

  function renderStatus(data) {
    const parts = [data.enabled ? 'enabled' : 'paused'];
    if (data.running) parts.push('checking now');
    else if (data.next_in === null) parts.push('no automatic checks');
    else parts.push(data.next_in === 0 ? 'due' : `next in ${fmtEta(data.next_in)}`);
    if (data.last) parts.push(`last pass queued ${data.last.queued}`);
    status.textContent = parts.join(' · ');
  }

  function renderWatched() {
    if (!watched.length) {
      root.replaceChildren(h('div', { class: 'empty' }, 'Watchlist is empty.'));
      return;
    }
    root.replaceChildren(h('table', {},
      h('thead', {}, h('tr', {},
        h('th', {}, 'Series'),
        h('th', {}, 'Last check'),
        h('th', {}, 'Last result'),
        h('th', { class: 'w-action' }, ''),
      )),
      h('tbody', {}, ...watched.map((entry) => h('tr', { class: entry.enabled ? '' : 'muted' },
        h('td', {},
          h('div', {}, entry.title || entry.url),
          h('div', { class: 'stage' }, entry.url),
        ),
        h('td', { class: 'num' }, entry.last_checked ? fmtDate(entry.last_checked) : '—'),
        h('td', { class: 'muted' }, entry.last_note || '—'),
        h('td', { class: 'actions' },
          h('button', { class: 'small', type: 'button', onclick: async () => {
            try {
              await api(`/api/autosync/series/${entry.id}/toggle`, { method: 'POST' });
              await refresh();
            } catch (err) { toast(err.message, 'error'); }
          } }, entry.enabled ? 'Pause' : 'Resume'),
          h('button', { class: 'small danger', type: 'button', onclick: async () => {
            try {
              await api(`/api/autosync/series/${entry.id}`, { method: 'DELETE' });
              await refresh();
            } catch (err) { toast(err.message, 'error'); }
          } }, 'Remove'),
        ),
      ))),
    ));
  }

  async function refresh() {
    const data = await api('/api/autosync');
    watched = data.watched;
    enabled.checked = data.enabled;
    interval.value = data.interval;
    renderStatus(data);
    renderWatched();
  }

  async function save(patch) {
    try {
      const data = await api('/api/autosync', { method: 'PUT', body: patch });
      renderStatus(data);
      toast('Autosync updated');
    } catch (err) {
      toast(err.message, 'error');
    }
  }

  enabled.addEventListener('change', () => save({ autosync_enabled: enabled.checked }));
  interval.addEventListener('change', () => save({ autosync_interval: Number(interval.value) }));

  $('#watch-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const button = event.target.querySelector('button');
    const url = $('#watch-url').value.trim();
    busy(button, true);
    try {
      await api('/api/autosync/series', { method: 'POST', body: { url } });
      $('#watch-url').value = '';
      meta.textContent = 'Added.';
      await refresh();
    } catch (err) {
      meta.textContent = '';
      toast(err.message, 'error');
    } finally {
      busy(button, false);
    }
  });

  $('#autosync-run').addEventListener('click', async (event) => {
    busy(event.target, true);
    try {
      const report = await api('/api/autosync/run', { method: 'POST' });
      toast(`Checked ${report.series} series, queued ${report.queued}`);
      if (report.notes?.length) meta.textContent = report.notes.join(' · ');
      await refresh();
    } catch (err) {
      toast(err.message, 'error');
    } finally {
      busy(event.target, false);
    }
  });

  refresh().catch((err) => toast(err.message, 'error'));
}

/* ---------------- boot ---------------- */

function boot() {
  bindQuickSearch();
  refreshStatus();
  setInterval(refreshStatus, 5000);
  switch (document.body.dataset.page) {
    case 'search': initHome(); break;
    case 'series': initSeries(); break;
    case 'queue': initQueue(); break;
    case 'library': initLibrary(); break;
    case 'settings': initSettings(); break;
    case 'autosync': initAutosync(); break;
  }
}

boot();
