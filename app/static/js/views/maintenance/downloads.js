// Maintenance downloads view: the Pawchive subscription download settings
// (master switch, JDownloader dispatch, auto ingest, interval and file-type
// filters), the custom naming templates with their variable chip strip, the
// per-artist subscription list, the manual check/sync actions and the event log.

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy } from '../../store.js';
import { $, $$, escHtml, formatServerTime, isAbortError } from '../../utils.js';
import { toast, logUiAction } from '../../logging.js';
import { applyMode } from '../../events.js';
import { loadArtists, selectArtist } from '../../router.js';
import { selectFolder } from '../sidebar.js';
import { ensureNetdiskDispatchReady } from './netdisk.js';

// Follow the sync round until it finishes. The backend runs it on a background
// task, so the panel polls instead of holding one long request open.
const SYNC_POLL_MS = 2000;
const SYNC_POLL_MAX = 300;

// The log panel is a tail, not a full history: the backend retains 500 rows and
// keeps its own file log, so the panel only needs enough to see the last round.
const DOWNLOAD_LOG_LIMIT = 50;

// The 全部作品 view is one page at a time with a cursor, like the API's own
// contract: the cursor is the sort key, so a work discovered while the user reads
// cannot shift the pages already read.
export const DOWNLOAD_WORKS_PAGE_SIZE = 50;

// How long after the last keystroke the search filter re-reads. The list is a
// request per keystroke otherwise, and each one re-derives per-row evidence.
const DOWNLOAD_WORKS_SEARCH_DEBOUNCE_MS = 300;

export const DOWNLOAD_INTERVAL_HOURS = [1, 3, 6, 12, 24];

// The file-type segments, in panel order. `key` is the settings field
// suffix (`download_${key}`), so render and form-read stay in one place.
export const DOWNLOAD_TYPES = [
  {key: 'images', label: '图片'},
  {key: 'videos', label: '视频'},
  {key: 'audio', label: '音频'},
  {key: 'archives', label: '压缩包'},
  {key: 'others', label: '其他文件'},
  {key: 'content', label: '正文'},
  {key: 'links', label: '外链'},
];

export function downloadTypeSegments() {
  return $$('[data-download-type]');
}

export function downloadTypeEnabled(key) {
  return Boolean(state.downloadSettings?.[`download_${key}`]);
}

export function toggleDownloadType(key) {
  if (!state.downloadSettings || state.downloadLoading) return;
  const field = `download_${key}`;
  state.downloadSettings = {...state.downloadSettings, [field]: !state.downloadSettings[field]};
  markDownloadSettingsDirty();
  renderDownloadTypeSegments();
}

// Flag that the settings form holds unsaved edits. The page auto-refresh
// reloads the whole panel; repopulating the settings controls from the server
// on every tick would silently undo whatever the user just changed, so the
// form is left alone until it is saved.
export function markDownloadSettingsDirty() {
  state.downloadSettingsDirty = true;
}

const TEMPLATE_INPUTS = {
  folder: '#downloadFolderTemplateInput',
  image: '#downloadImageTemplateInput',
  attachment: '#downloadAttachmentTemplateInput',
};

export function downloadTemplateInputSelector(inputId) {
  return TEMPLATE_INPUTS[inputId] || '';
}

function downloadTemplateInput(inputId) {
  const selector = downloadTemplateInputSelector(inputId);
  return selector ? $(selector) : null;
}

export async function loadDownloadsPanel(options = {}) {
  state.downloadLoading = true;
  renderDownloadsPanel();
  try {
    const fetchOptions = options.signal ? {signal: options.signal} : {};
    const [settingsResult, subscriptionsResult] = await Promise.all([
      API.get('/api/pawchive/settings', fetchOptions),
      API.get('/api/pawchive/subscriptions', fetchOptions),
    ]);
    // Keep the user's unsaved edits instead of overwriting them with the
    // server copy; the form re-reads the server once the save clears the flag.
    if (!state.downloadSettingsDirty) {
      state.downloadSettings = settingsResult?.settings || null;
    }
    state.downloadDefaults = settingsResult?.defaults || null;
    state.downloadSubscriptions = Array.isArray(subscriptionsResult?.subscriptions)
      ? subscriptionsResult.subscriptions
      : [];
    // The artist list only backs the "绑定画师文件夹" picker and barely changes;
    // fetch it once so the page's auto-refresh does not re-pull it every tick.
    if (!state.downloadArtists.length) await loadDownloadArtists(fetchOptions);
    // Same for the media roots: they back the picker's root select and only
    // change when the media configuration does.
    if (!state.downloadArtistRoots.length) await loadDownloadArtistRoots(fetchOptions);
    renderDownloadArtistFolder();
    renderDownloadsPanel();
    // The log and the status are independent of the settings read: neither
    // failure may blank the settings and subscriptions the user just loaded.
    try {
      const eventsResult = await API.get(
        '/api/pawchive/events?limit=' + DOWNLOAD_LOG_LIMIT,
        fetchOptions
      );
      state.downloadEvents = Array.isArray(eventsResult?.events) ? eventsResult.events : [];
    } catch (e) {
      if (isAbortError(e)) throw e;
      state.downloadEvents = [];
    }
    try {
      const statusResult = await API.get('/api/pawchive/status', fetchOptions);
      applySyncStatus(statusResult?.status || null);
    } catch (e) {
      if (isAbortError(e)) throw e;
    }
    // The 全部作品 view is a paged list the user builds a selection in, so it is
    // read when the panel is opened and not on the 10s auto-refresh tick: that
    // tick would fight both the paging and the in-progress selection.
    if (state.downloadAllWorks && state.downloadAllWorksRevision !== state.downloadAllWorksLoadedRevision) {
      state.downloadAllWorksLoadedRevision = state.downloadAllWorksRevision;
      await loadDownloadAllWorks({signal: options.signal});
    }
  } catch (error) {
    if (isAbortError(error)) throw error;
    state.downloadSubscriptions = [];
    state.downloadEvents = [];
    renderDownloadsPanel();
    toast('读取下载设置失败：' + (error.message || error), 'error');
  } finally {
    state.downloadLoading = false;
    renderDownloadsPanel();
  }
}

async function loadDownloadArtists(fetchOptions) {
  try {
    const artists = await API.get('/api/artists', fetchOptions);
    state.downloadArtists = Array.isArray(artists)
      ? artists.filter(row => row && typeof row === 'object')
      : [];
  } catch (e) {
    if (isAbortError(e)) throw e;
    state.downloadArtists = [];
  }
}

async function loadDownloadArtistRoots(fetchOptions) {
  try {
    const result = await API.get('/api/media-roots', fetchOptions);
    state.downloadArtistRoots = Array.isArray(result?.roots) ? result.roots : [];
  } catch (e) {
    if (isAbortError(e)) throw e;
    state.downloadArtistRoots = [];
  }
}

function downloadArtistRoot() {
  const roots = Array.isArray(state.downloadArtistRoots) ? state.downloadArtistRoots : [];
  return roots.find(root => Number(root.index) === Number(state.downloadArtistRootIndex)) || roots[0] || null;
}

function downloadArtistRootPath() {
  const root = downloadArtistRoot();
  return String(root?.label || root?.path || '').replace(/\/+$/, '');
}

// The folder a new subscription binds to: the picked media root, the parent
// directory picked inside it, and the name typed in the artist box. Without a
// root list this degrades to `parent/name`, and with nothing picked at all to
// the bare name — which the backend resolves against its first media root.
export function downloadArtistNewFolderPath(name) {
  const trimmed = String(name || '').trim();
  if (!trimmed) return '';
  const parent = String(state.downloadArtistParentPath || '').replace(/^\/+|\/+$/g, '');
  return [downloadArtistRootPath(), parent, trimmed].filter(Boolean).join('/');
}

// The picker's root select and the chosen parent. Rendered with the panel so a
// reloaded page does not lose which root the browser will walk.
export function renderDownloadArtistFolder() {
  const rootSelect = $('#downloadSubscriptionRoot');
  const parentOutput = $('#downloadSubscriptionFolderParent');
  const browseButton = $('#downloadSubscriptionFolderBrowseBtn');
  const roots = Array.isArray(state.downloadArtistRoots) ? state.downloadArtistRoots : [];
  if (!roots.some(root => Number(root.index) === Number(state.downloadArtistRootIndex))) {
    state.downloadArtistRootIndex = roots.length ? Number(roots[0].index) : 0;
  }
  if (rootSelect) {
    rootSelect.innerHTML = roots.map(root => (
      `<option value="${Number(root.index)}">${escHtml(root.label || root.path || `目录 ${Number(root.index) + 1}`)}</option>`
    )).join('');
    rootSelect.value = String(state.downloadArtistRootIndex);
    rootSelect.disabled = !roots.length;
  }
  if (parentOutput) {
    parentOutput.textContent = state.downloadArtistParentPath || '根目录';
    // The field is one line in a narrow column, so the full path is also on the
    // element: an ellipsised parent is still readable on hover.
    parentOutput.title = state.downloadArtistParentPath || '根目录';
  }
  if (browseButton) browseButton.disabled = !roots.length;
}

function downloadArtistRows() {
  const artists = Array.isArray(state.downloadArtists) ? state.downloadArtists : [];
  return artists.filter(artist => artist && artist.path);
}

// The artist picker is a searchable combobox, not a native select: the library
// can hold hundreds of artists, and a dropdown nobody can filter is why the
// binding step felt broken. The visible input is both the search box and the
// name of a folder the library does not have yet; the hidden input carries the
// chosen path (empty means "按作者 ID 新建目录").
function downloadArtistQuery() {
  return String($('#downloadSubscriptionArtistInput')?.value || '').trim().toLowerCase();
}

function matchingDownloadArtists(limit = 500) {
  const query = downloadArtistQuery();
  const rows = downloadArtistRows();
  if (!query) return limit ? rows.slice(0, limit) : rows;
  return rows
    .filter(artist => {
      const haystack = `${artist.name || ''} ${artist.path || ''} ${artist.id || ''}`.toLowerCase();
      return query.split(/\s+/).filter(Boolean).every(part => haystack.includes(part));
    })
    .slice(0, limit || rows.length);
}

export function renderDownloadArtistCombo() {
  const input = $('#downloadSubscriptionArtistInput');
  const select = $('#downloadSubscriptionArtistSelect');
  const listbox = $('#downloadSubscriptionArtistListbox');
  if (!input || !select || !listbox) return;
  const matches = matchingDownloadArtists();
  // The current pick stays visible even when the query no longer matches it.
  const picked = String(select.value || '');
  if (picked && !matches.some(artist => String(artist.path) === picked)) {
    const artist = downloadArtistRows().find(row => String(row.path) === picked);
    if (artist) matches.unshift(artist);
  }
  const query = String(input.value || '').trim();
  // The first row is the only way to bind a folder the library does not have
  // yet. Text in the box names that folder, so the row carries the full target
  // (picked media root + picked parent + name) as its value; an empty box keeps
  // the author-ID default. Typing and then adding without picking therefore
  // creates the folder the user named, in the place the user picked, instead of
  // silently falling back to the numeric source id under the first root.
  const createValue = downloadArtistNewFolderPath(query);
  const createLabel = query ? `新建目录“${query}”` : '按作者 ID 新建目录';
  listbox.innerHTML = `<li role="option" data-download-artist-value="${escHtml(createValue)}" class="create-new">${escHtml(createLabel)}</li>`
    + matches.map(artist => {
      const name = String(artist.name || '');
      const path = String(artist.path || '');
      const selected = select.value === path ? ' active' : '';
      return `<li role="option" data-download-artist-value="${escHtml(path)}" class="${selected}" title="${escHtml(path)}">${escHtml(name)}</li>`;
    }).join('');
}

export function openDownloadArtistCombo() {
  const input = $('#downloadSubscriptionArtistInput');
  const listbox = $('#downloadSubscriptionArtistListbox');
  if (!input || !listbox) return;
  renderDownloadArtistCombo();
  listbox.hidden = false;
  input.setAttribute('aria-expanded', 'true');
}

export function closeDownloadArtistCombo() {
  const input = $('#downloadSubscriptionArtistInput');
  const listbox = $('#downloadSubscriptionArtistListbox');
  if (!input || !listbox) return;
  listbox.hidden = true;
  input.setAttribute('aria-expanded', 'false');
}

export function pickDownloadArtist(value) {
  const select = $('#downloadSubscriptionArtistSelect');
  const input = $('#downloadSubscriptionArtistInput');
  if (!select || !input) return;
  const path = String(value || '');
  select.value = path;
  if (!path) return;
  const artist = downloadArtistRows().find(row => String(row.path) === path);
  // Echo the chosen artist's name so the box reads as a selection, not a query.
  if (artist) input.value = String(artist.name || path);
  closeDownloadArtistCombo();
}

function renderDownloadTypeSegments() {
  const settings = state.downloadSettings;
  for (const button of downloadTypeSegments()) {
    const key = button.dataset.downloadType;
    const enabled = Boolean(settings?.[`download_${key}`]);
    button.classList.toggle('active', enabled);
    button.setAttribute('aria-checked', enabled ? 'true' : 'false');
  }
}

function applySyncStatus(status) {
  state.downloadSyncStatus = status;
  renderDownloadSyncStatus();
  if (status && status.running) startDownloadSyncPolling();
}

export function renderDownloadsPanel() {
  const settings = state.downloadSettings;
  // Not gated on `downloadLoading`: the auto-refresh would grey the whole form
  // and re-enable it every tick. Unsaved edits are protected by the dirty flag
  // instead, so a background read must not disable anything.
  const disabled = !settings;
  // Unsaved edits win over the fetched values: this runs on every page
  // auto-refresh, and repopulating here is what would silently undo a change
  // the user has not saved yet.
  if (!state.downloadSettingsDirty) {
    const enabledToggle = $('#downloadEnabledToggle');
    if (enabledToggle) enabledToggle.checked = Boolean(settings?.enabled);
    const jdownloaderToggle = $('#downloadJdownloaderToggle');
    if (jdownloaderToggle) jdownloaderToggle.checked = Boolean(settings?.auto_netdisk);
    const autoIngestToggle = $('#downloadAutoIngestToggle');
    if (autoIngestToggle) autoIngestToggle.checked = Boolean(settings?.auto_ingest);
    const intervalSelect = $('#downloadIntervalSelect');
    if (intervalSelect && settings) intervalSelect.value = String(settings.interval_hours);
    renderDownloadTypeSegments();

    const folderInput = $('#downloadFolderTemplateInput');
    if (folderInput && settings) folderInput.value = String(settings.folder_template || '');
    const imageInput = $('#downloadImageTemplateInput');
    if (imageInput && settings) imageInput.value = String(settings.image_template || '');
    const attachmentInput = $('#downloadAttachmentTemplateInput');
    if (attachmentInput && settings) attachmentInput.value = String(settings.attachment_template || '');
  }

  for (const selector of Object.values(TEMPLATE_INPUTS)) {
    const input = $(selector);
    if (input) input.disabled = disabled;
  }
  const saveBtn = $('#downloadSettingsSaveBtn');
  if (saveBtn) saveBtn.disabled = disabled || isActionBusy('downloadSettingsSave');
  const resetBtn = $('#downloadTemplateResetBtn');
  if (resetBtn) resetBtn.disabled = disabled;

  renderDownloadSubscriptions();
  renderDownloadLog();
  renderDownloadSyncStatus();
  renderDownloadAllWorks();
}

export const SUBSCRIPTION_FOLD_THRESHOLD = 5;
export const DOWNLOAD_LOG_FOLD_THRESHOLD = 5;

export function isSubscriptionComplete(row) {
  const coverage = Array.isArray(row?.coverage) ? row.coverage : [];
  if (!coverage.length) return false;
  const days = coverage.filter(day => day && day.day && Number(day.total_posts || 0) > 0);
  if (!days.length) return false;
  return days.every(day => dayComplete(day));
}

export function setDownloadSubscriptionFilter(filter) {
  state.downloadSubscriptionFilter = ['pending', 'completed'].includes(filter) ? filter : 'all';
  state.downloadSubscriptionsExpanded = false;
  renderDownloadSubscriptions();
}

export function toggleDownloadSubscriptionsFold() {
  state.downloadSubscriptionsExpanded = !state.downloadSubscriptionsExpanded;
  renderDownloadSubscriptions();
}

export function toggleDownloadLogFold() {
  state.downloadLogExpanded = !state.downloadLogExpanded;
  renderDownloadLog();
}

export function renderDownloadSubscriptions() {
  const list = $('#downloadSubscriptionList');
  const count = $('#downloadSubscriptionCount');
  const subscriptions = Array.isArray(state.downloadSubscriptions) ? state.downloadSubscriptions : [];

  const searchInput = $('#downloadSubscriptionSearch');
  const searchQuery = String(searchInput?.value || state.downloadSubscriptionSearch || '').trim().toLowerCase();

  let visibleSubs = subscriptions;
  if (searchQuery) {
    visibleSubs = subscriptions.filter(row => {
      const haystack = `${row.artist_name || ''} ${row.user_id || ''} ${row.target_folder || ''} ${row.service || ''}`.toLowerCase();
      return searchQuery.split(/\s+/).filter(Boolean).every(part => haystack.includes(part));
    });
  }

  if (count) {
    if (!subscriptions.length) {
      count.textContent = '';
    } else if (searchQuery) {
      count.textContent = `找到 ${visibleSubs.length} / 共 ${subscriptions.length} 个订阅`;
    } else {
      count.textContent = `共 ${subscriptions.length} 个订阅`;
    }
  }

  if (!list) return;
  if (state.downloadLoading && !subscriptions.length) {
    list.innerHTML = '<div class="move-empty small">读取订阅中</div>';
    return;
  }
  if (!subscriptions.length) {
    list.innerHTML = '<div class="move-empty small">还没有订阅。粘贴画师主页链接即可添加</div>';
    return;
  }
  if (!visibleSubs.length) {
    list.innerHTML = '<div class="move-empty small">未找到匹配的画师订阅</div>';
    return;
  }

  list.innerHTML = visibleSubs.map(row => {
    const id = Number(row.id);
    // One round runs at a time across the whole panel, so a running round
    // disables every per-artist action: offering one would only get a 409.
    const roundBusy = isActionBusy('downloadSubscriptionRound', id)
      || Boolean(state.downloadSyncStatus?.running);
    const name = String(row.artist_name || row.user_id || '');
    const service = String(row.service || '');
    const target = String(row.target_dir || '');
    const since = row.since_date ? String(row.since_date) : '不限';
    const lastSync = row.last_discovery_at ? formatServerTime(row.last_discovery_at) : '尚未同步';
    const error = row.last_error ? String(row.last_error) : '';
    const coverage = downloadCoverageText(row.coverage);
    const isComplete = isSubscriptionComplete(row);
    const calendarHtml = downloadCalendarHtml(row.coverage, id);
    const dayPostsHtml = downloadDayPostsHtml(id);
    const calendarDetails = calendarHtml ? `
      <details class="download-subscription-calendar-details" ${isComplete ? '' : 'open'}>
        <summary class="download-subscription-calendar-summary">
          <span>作品日历</span>
        </summary>
        ${calendarHtml}
      </details>` : '';
    return `
      <div class="download-subscription-item" data-download-subscription="${id}">
        <div class="download-subscription-main">
          <div class="download-subscription-title">${escHtml(name)}</div>
          <div class="download-subscription-meta">${escHtml(service)} &#183; ${escHtml(String(row.user_id || ''))} &#183; 起始 ${escHtml(since)} &#183; 上次同步 ${escHtml(lastSync)}</div>
          <div class="download-subscription-path" title="${escHtml(target)}">${escHtml(target)}</div>
          ${coverage ? `<div class="download-subscription-coverage">${escHtml(coverage)}</div>` : ''}
          ${calendarDetails}
          ${dayPostsHtml}
          ${error ? `<div class="download-subscription-error">${escHtml(error)}</div>` : ''}
        </div>
        <div class="download-subscription-item-actions">
          <label class="download-subscription-mode" title="自动：由轮次按已有记录补缺；手动：从全部作品里自己选着下">
            <span>模式</span>
            <select data-download-subscription-mode="${id}">
              <option value="manual" ${String(row.mode || 'manual') === 'manual' ? 'selected' : ''}>手动</option>
              <option value="auto" ${String(row.mode || 'manual') === 'auto' ? 'selected' : ''}>自动</option>
            </select>
          </label>
          <label class="maintenance-auto-toggle" title="停用后不再检查这个画师的新作品">
            <input type="checkbox" data-download-subscription-toggle="${id}" ${row.enabled ? 'checked' : ''}>
            <span>启用</span>
          </label>
          <button class="btn btn-ghost" type="button" data-download-subscription-check="${id}" ${roundBusy ? 'disabled' : ''} title="只发现这个画师的新作品并重算每天的缺失，不下载文件">检查缺失</button>
          <button class="btn btn-ghost" type="button" data-download-subscription-reconcile="${id}" ${roundBusy ? 'disabled' : ''} title="只读取这个画师的本地目录与账本，重算每篇作品的状态">核对本地</button>
          <button class="btn btn-danger" type="button" data-download-subscription-delete="${id}">删除</button>
        </div>
      </div>`;
  }).join('');
}

// The event's artist, or '' for a round-level line. Kept separate from the
// message so the row can join the two with an escaped middle-dot entity: this
// codebase writes that separator as an entity to avoid mojibake, and a value
// that goes through `escHtml` would carry a literal entity through as text.
export function downloadEventWho(event) {
  return String(event?.artist_name || '').trim();
}

// `发现 3 个帖子，2 天待补（2026-09-11 缺 3 个）`
export function downloadEventText(event) {
  const message = String(event?.message || '').trim();
  const detail = String(event?.detail || '').trim();
  return detail ? `${message}（${detail}）` : message;
}

// Seconds-resolution stamp for the log rows: these are all within the last few
// rounds, so the date alone would waste the column.
export function formatDownloadEventTime(value) {
  const date = new Date(String(value || ''));
  if (Number.isNaN(date.getTime())) return '';
  const pad = part => String(part).padStart(2, '0');
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function renderDownloadLog() {
  const list = $('#downloadLogList');
  const foldWrap = $('#downloadLogFoldWrap');
  const foldBtn = $('#downloadLogFoldBtn');
  if (!list) return;
  const events = Array.isArray(state.downloadEvents) ? state.downloadEvents : [];
  if (state.downloadLoading && !events.length) {
    list.innerHTML = '<div class="move-empty small">读取日志中</div>';
    if (foldWrap) foldWrap.hidden = true;
    return;
  }
  if (!events.length) {
    list.innerHTML = '<div class="move-empty small">暂无记录。点「检查缺哪些」或「立即同步全部」后会写入这里</div>';
    if (foldWrap) foldWrap.hidden = true;
    return;
  }

  const shouldFold = events.length > DOWNLOAD_LOG_FOLD_THRESHOLD;
  const isExpanded = Boolean(state.downloadLogExpanded);
  const visibleEvents = (shouldFold && !isExpanded)
    ? events.slice(0, DOWNLOAD_LOG_FOLD_THRESHOLD)
    : events;

  if (foldWrap && foldBtn) {
    if (shouldFold) {
      foldWrap.hidden = false;
      const hiddenCount = events.length - DOWNLOAD_LOG_FOLD_THRESHOLD;
      foldBtn.textContent = isExpanded ? '收起' : `展开剩余 ${hiddenCount} 条日志`;
    } else {
      foldWrap.hidden = true;
    }
  }

  list.innerHTML = visibleEvents.map(event => {
    const level = String(event?.level || 'info').toLowerCase();
    const who = downloadEventWho(event);
    return `
      <div class="download-log-row" data-download-log-level="${escHtml(level)}">
        <span class="download-log-time">${escHtml(formatDownloadEventTime(event?.created_at))}</span>
        <span class="download-log-text">${who ? `<span class="download-log-who">${escHtml(who)}</span> &#183; ` : ''}${escHtml(downloadEventText(event))}</span>
      </div>`;
  }).join('');
}

// Re-read just the log, for the panel's own refresh button. `loadDownloadsPanel`
// already refreshes it as part of the round polling.
export async function refreshDownloadLog() {
  try {
    const result = await API.get('/api/pawchive/events?limit=' + DOWNLOAD_LOG_LIMIT);
    state.downloadEvents = Array.isArray(result?.events) ? result.events : [];
    renderDownloadLog();
  } catch (error) {
    if (isAbortError(error)) return;
    toast('读取下载日志失败：' + (error.message || error), 'error');
  }
}

// One-line per-day coverage digest. Shows the newest days with a gap first so
// a missing day reads as "2026-09-11 缺 3 篇", and collapses fully covered days
// into "近 N 天已齐" instead of a wall of dates.
export function downloadCoverageText(coverage) {
  if (!Array.isArray(coverage) || !coverage.length) return '';
  const days = coverage.filter(day => day && day.day);
  const gaps = days.filter(day => !dayComplete(day));
  const covered = days.length - gaps.length;
  // Lead with how many days are short, not with the samples. Listing three days
  // next to "36 天已齐" read as if only three were short while the window held
  // fifty-odd gaps, and the counts have to add up to the window before the
  // examples mean anything.
  const head = gaps.length ? `${gaps.length} 天缺` : '全部已齐';
  const parts = [head];
  if (covered && gaps.length) parts.push(`${covered} 天已齐`);
  if (gaps.length) parts.push(`共 ${days.length} 天`);
  // A confirmation is a user decision, not a verification, so it is named
  // rather than folded into the covered count.
  const confirmed = days.reduce((sum, day) => sum + Number(day.confirmed_posts || 0), 0);
  if (confirmed) parts.push(`${confirmed} 篇人工确认`);
  const idle = days.reduce((sum, day) => sum + Number(day.not_required_posts || 0), 0);
  if (idle) parts.push(`${idle} 篇无需下载`);
  const samples = gaps.slice(0, 3).map(day => `${day.day} 缺 ${downloadDayMissing(day)} 篇`);
  return samples.length ? `${parts.join('，')}（最近 ${samples.join('、')}）` : parts.join('，');
}

// Posts on a day that are neither proven nor outside the current range. A day
// the user needs nothing from is not a gap. Unverified content still is — but
// only the residue the evaluator could not settle on its own: a folder that is
// missing, unreadable, or shared with a work whose files cannot be told apart.
export function downloadDayMissing(day) {
  const total = Number(day?.total_posts || 0);
  const settled = Number(day?.downloaded_posts || 0) + Number(day?.not_required_posts || 0);
  return Math.max(0, total - settled);
}

function dayComplete(day) {
  return Number(day?.total_posts || 0) > 0 && downloadDayMissing(day) === 0;
}

// Contribution-graph grid for one subscription, in the panel's own vermilion
// family rather than the usual green ramp. Colour carries the work left to do:
// an untinted cell is a day with nothing discovered, the lightest wash is a day
// that is fully downloaded, and the fill deepens with the number of files still
// missing, so the eye lands on the days that actually need attention.
const DOWNLOAD_CALENDAR_WEEKS = 53;
// Row order matches the Sunday-start columns. Only Monday/Wednesday/Friday are
// labelled, like the original: a seven-row caption column is visual noise next
// to a grid whose rows are 10px tall, and the unlabelled rows stay readable by
// counting from the labelled ones.
const DOWNLOAD_CALENDAR_WEEKDAYS = ['', '一', '', '三', '', '五', ''];
const DOWNLOAD_CALENDAR_LEVELS = [
  {level: 1, label: '已下齐'},
  {level: 2, label: '缺 1-2'},
  {level: 3, label: '缺 3-5'},
  {level: 4, label: '缺 6+'},
];

function calendarLevel(entry) {
  const total = Number(entry?.total_posts || 0);
  if (total <= 0) return 0;
  const missing = downloadDayMissing(entry);
  if (missing === 0) return 1;
  if (missing <= 2) return 2;
  if (missing <= 5) return 3;
  return 4;
}

function localDayKey(value) {
  const pad = part => String(part).padStart(2, '0');
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())}`;
}

function localDayShift(value, days) {
  return new Date(value.getFullYear(), value.getMonth(), value.getDate() + days);
}

// Pure grid builder: 53 week columns, weekday rows, right edge on today. Days
// after today are null so the last column can render ragged like the original.
// Coverage older than the grid is counted rather than dropped silently.
export function downloadCalendar(coverage, today = new Date()) {
  const byDay = new Map();
  for (const entry of Array.isArray(coverage) ? coverage : []) {
    if (entry && typeof entry.day === 'string' && entry.day) byDay.set(entry.day, entry);
  }
  const lastDay = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  // Weeks run Sunday..Saturday, so the grid ends on the coming Saturday.
  const gridEnd = localDayShift(lastDay, 6 - lastDay.getDay());
  const gridStart = localDayShift(gridEnd, -(DOWNLOAD_CALENDAR_WEEKS * 7 - 1));

  const weeks = [];
  let activeDays = 0;
  let missingDays = 0;
  for (let week = 0; week < DOWNLOAD_CALENDAR_WEEKS; week += 1) {
    const cells = [];
    for (let weekday = 0; weekday < 7; weekday += 1) {
      const date = localDayShift(gridStart, week * 7 + weekday);
      if (date > lastDay) {
        cells.push(null);
        continue;
      }
      const day = localDayKey(date);
      const entry = byDay.get(day);
      const total = Number(entry?.total_posts || 0);
      const level = calendarLevel(entry);
      if (level > 0) activeDays += 1;
      if (level > 1) missingDays += 1;
      cells.push({
        day,
        level,
        total_posts: total,
        missing: downloadDayMissing(entry),
      });
    }
    weeks.push(cells);
  }

  let olderDays = 0;
  const gridStartKey = localDayKey(gridStart);
  for (const day of byDay.keys()) {
    if (day < gridStartKey) olderDays += 1;
  }

  // Month captions are runs of whole columns; the renderer gives each run a
  // flex weight equal to its column count so the labels line up with the grid.
  const months = [];
  let currentKey = '';
  for (const cells of weeks) {
    const first = cells.find(cell => cell);
    const key = first ? first.day.slice(0, 7) : currentKey;
    if (key && key === currentKey) {
      months[months.length - 1].columns += 1;
      continue;
    }
    if (!key) continue;
    currentKey = key;
    months.push({label: `${Number(key.slice(5, 7))}月`, columns: 1});
  }

  return {weeks, months, activeDays, missingDays, olderDays};
}

function downloadCalendarHtml(coverage, subscriptionId) {
  const calendar = downloadCalendar(coverage);
  if (!calendar.activeDays) return '';
  const openDay = downloadOpenDayKey(subscriptionId);
  const months = calendar.months
    .map(month => `<span class="download-calendar-month" style="flex:${month.columns}">${escHtml(month.label)}</span>`)
    .join('');
  const weeks = calendar.weeks.map(cells => `<div class="download-calendar-week">${cells.map(cell => {
    if (!cell) return '<span class="download-calendar-cell is-future"></span>';
    const title = cell.total_posts
      ? `${cell.day}：共 ${cell.total_posts} 篇，缺 ${cell.missing} 篇`
      : `${cell.day}：无作品`;
    // A day with works opens its post list. Without one the cell stays inert,
    // so the grid is not 300-odd dead buttons.
    const attributes = cell.total_posts
      ? ` role="button" tabindex="0" data-download-day="${escHtml(cell.day)}" aria-expanded="${openDay === cell.day ? 'true' : 'false'}"`
      : '';
    const current = openDay === cell.day ? ' is-open' : '';
    return `<span class="download-calendar-cell${current}" data-download-calendar-level="${cell.level}" title="${escHtml(title)}"${attributes}></span>`;
  }).join('')}</div>`).join('');
  const keys = DOWNLOAD_CALENDAR_LEVELS
    .map(item => `<span class="download-calendar-key"><i data-download-calendar-level="${item.level}"></i>${escHtml(item.label)}</span>`)
    .join('');
  const summary = [`近 ${DOWNLOAD_CALENDAR_WEEKS} 周`, `${calendar.activeDays} 天有作品`];
  if (calendar.missingDays) summary.push(`${calendar.missingDays} 天缺作品`);
  if (calendar.olderDays) summary.push(`另有 ${calendar.olderDays} 天更早`);
  const weekdays = DOWNLOAD_CALENDAR_WEEKDAYS
    .map(day => `<span class="download-calendar-weekday">${escHtml(day)}</span>`)
    .join('');
  return `
    <div class="download-calendar">
      <div class="download-calendar-grid-wrap">
        <div class="download-calendar-months">${months}</div>
        <div class="download-calendar-weekdays">${weekdays}</div>
        <div class="download-calendar-grid">${weeks}</div>
      </div>
      <div class="download-calendar-legend">
        <span class="download-calendar-summary">${escHtml(summary.join('，'))}</span>
        ${keys}
      </div>
    </div>`;
}

// The evaluator's verdicts, in the panel's wording. Exhaustive on purpose: a
// state the backend adds later shows as its own token rather than as a blank.
export const DOWNLOAD_POST_STATES = {
  verified: '已核验',
  confirmed: '已确认',
  ignored: '已忽略',
  partial: '部分完成',
  pending: '待下载',
  unverified: '待核验',
  unavailable: '源站不可用',
  external: '待外部交付',
  not_required: '无需下载',
};

export function downloadPostStateLabel(state) {
  const key = String(state || '');
  return DOWNLOAD_POST_STATES[key] || key;
}

function downloadPostCounts(post) {
  const missing = Number(post?.missing_assets || 0);
  const gapLabel = missing > 0 && (post?.date_skipped || post?.state === 'confirmed')
    ? '未逐项核验' : '缺';
  return `需要 ${Number(post?.required_assets || 0)}，已核验 ${Number(post?.proven_assets || 0)}，${gapLabel} ${missing}`;
}

// The state of one explicit request. Kept separate from the work's own state:
// "这次要求下载的结果" and "这篇作品现在算不算已下过" are different questions.
const DOWNLOAD_ATTEMPT_STATES = {
  queued: '已排队',
  running: '进行中',
  done: '已完成',
  failed: '未完成',
  conflict: '清单已变更',
  cancelled: '已取消',
};

export function downloadAttemptStateLabel(state) {
  const key = String(state || '');
  return DOWNLOAD_ATTEMPT_STATES[key] || key;
}

// Ask the backend to fetch this one work explicitly. The work's existing
// records are not rewritten: a new task is created, and the delivered files are
// preserved because the publisher never overwrites.
//
// The preview runs first so the user learns the cost before the task exists: a
// work whose resources are all external links is reported here as
// `no_direct_resources`, and committing it would otherwise look accepted and
// then fetch nothing.
export async function downloadPostByHand(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return null;
  if (isActionBusy('downloadSelection')) return null;
  let preview = null;
  try {
    preview = await previewDownloadSelection([id], []);
  } catch (error) {
    toast('无法预览这次下载：' + (error.message || error), 'error');
    return null;
  }
  if (preview && !preview.resources) {
    const external = Array.isArray(preview.no_direct_resources) && preview.no_direct_resources.includes(id);
    toast(external ? '这篇只有外部链接，没有可直连的文件' : '这篇没有可下载的文件', 'warn');
    return null;
  }
  const attempt = await startDownloadSelection([id], []);
  if (attempt) {
    toast(`已发起下载：${Number(attempt.posts || 0)} 篇，${Number(attempt.resources || 0)} 个文件`, 'info');
    await loadDownloadDayPosts();
  }
  return attempt;
}

// Only these two can be withdrawn or replaced; the rest are still being
// resolved by the reader and have nothing to undo.
function downloadPostCanDecide(state) {
  return state === 'confirmed' || state === 'ignored';
}

function downloadOpenDayKey(subscriptionId) {
  const open = state.downloadOpenDay;
  if (!open || Number(open.subscriptionId) !== Number(subscriptionId)) return '';
  return String(open.day || '');
}

export function downloadOpenDay() {
  return state.downloadOpenDay || null;
}

// Drill-down under one subscription: the posts of the clicked day and what the
// evaluator concluded about each of them, with the reversible decisions.
function downloadDayPostsHtml(subscriptionId) {
  const day = downloadOpenDayKey(subscriptionId);
  if (!day) return '';
  const view = state.downloadDayPosts;
  if (!view || view.loading) {
    return `<div class="download-day-posts"><div class="move-empty small">读取 ${escHtml(day)} 的作品中</div></div>`;
  }
  if (view.error) {
    return `<div class="download-day-posts"><div class="download-subscription-error">${escHtml(view.error)}</div></div>`;
  }
  if (!view.posts.length) {
    return `<div class="download-day-posts"><div class="move-empty small">${escHtml(day)} 没有已发现的作品</div></div>`;
  }
  const rows = view.posts.map(post => {
    const id = Number(post.id);
    const title = String(post.title || post.post_id || '');
    const stateLabel = post.date_skipped ? '已有' : downloadPostStateLabel(post.state);
    const counts = downloadPostCounts(post);
    const target = post.source_url ? ` href="${escHtml(post.source_url)}" target="_blank" rel="noopener noreferrer"` : '';
    const attempt = String(post.last_attempt_state || '');
    const attemptLine = attempt
      ? `<div class="download-day-post-attempt" data-download-post-attempt="${id}">手动请求：${escHtml(downloadAttemptStateLabel(attempt))}</div>`
      : '';
    return `
      <div class="download-day-post" data-download-post="${id}">
        <div class="download-day-post-main">
          <div class="download-day-post-title">${target ? `<a${target}>${escHtml(title)}</a>` : escHtml(title)}</div>
          <div class="download-day-post-meta">${escHtml(counts)} &#183; ${escHtml(post.reason || '')}</div>
          ${attemptLine}
          ${downloadPostFilesHtml(id)}
          ${downloadCandidatesHtml(id)}
        </div>
        <div class="download-day-post-actions">
          <span class="download-day-post-state" data-download-post-state="${escHtml(String(post.state || ''))}">${escHtml(stateLabel)}</span>
          <button class="btn btn-ghost" type="button" data-download-post-fetch="${id}" title="按当前选择重新获取这篇的作品文件，不覆盖已有文件">下载所选</button>
          <button class="btn btn-ghost" type="button" data-download-post-files="${id}" title="列出这篇的文件，逐个重试">文件</button>
          <button class="btn btn-ghost" type="button" data-download-post-import="${id}" title="把这篇已核验的文件绑入库内条目">入库</button>
          <button class="btn btn-ghost" type="button" data-download-post-stop="${id}" title="停止这篇正在进行的手动请求">停止</button>
          <button class="btn btn-ghost" type="button" data-download-post-verify="${id}" title="按来源哈希校验本地文件">校验</button>
          <button class="btn btn-ghost" type="button" data-download-post-candidates="${id}" title="列出本画师目录里可能已有的文件">候选</button>
          ${downloadPostCanDecide(post.state)
            ? `<button class="btn btn-ghost" type="button" data-download-post-decision="revoke" data-download-post-id="${id}">撤销</button>`
            : `<button class="btn btn-ghost" type="button" data-download-post-decision="confirm" data-download-post-id="${id}">已有</button>
               <button class="btn btn-ghost" type="button" data-download-post-decision="ignore" data-download-post-id="${id}">忽略</button>`}
        </div>
      </div>`;
  }).join('');
  const more = view.next_cursor
    ? `<button class="btn btn-ghost" type="button" data-download-day-more>加载更多</button>`
    : '';
  return `
    <div class="download-day-posts">
      <div class="download-day-posts-head">${escHtml(day)} &#183; 共 ${Number(view.total || 0)} 篇</div>
      ${rows}
      ${more}
    </div>`;
}

// Open (or close) one day's post list.
export async function openDownloadDay(subscriptionId, day) {
  const id = Number(subscriptionId);
  const key = String(day || '');
  if (!Number.isFinite(id) || !key) return;
  const open = state.downloadOpenDay;
  if (open && Number(open.subscriptionId) === id && String(open.day) === key) {
    state.downloadOpenDay = null;
    state.downloadDayPosts = null;
    renderDownloadsPanel();
    return;
  }
  state.downloadOpenDay = {subscriptionId: id, day: key};
  state.downloadDayPosts = {loading: true, posts: [], total: 0, next_cursor: null, error: ''};
  renderDownloadsPanel();
  await loadDownloadDayPosts();
}

// Re-read the open day. Called after a decision so the list shows the state
// the backend just derived, rather than an optimistic guess of it.
export async function loadDownloadDayPosts(options = {}) {
  const open = state.downloadOpenDay;
  if (!open) return;
  const cursor = options.cursor ?? null;
  try {
    const query = `/api/pawchive/subscriptions/${open.subscriptionId}/posts?day=${encodeURIComponent(open.day)}&limit=50`
      + (cursor ? `&cursor=${encodeURIComponent(cursor)}` : '');
    const result = await API.get(query);
    const posts = Array.isArray(result?.posts) ? result.posts : [];
    const previous = cursor ? (state.downloadDayPosts?.posts || []) : [];
    state.downloadDayPosts = {
      loading: false,
      posts: [...previous, ...posts],
      total: Number(result?.total || 0),
      next_cursor: result?.next_cursor ?? null,
      error: '',
    };
  } catch (error) {
    if (isAbortError(error)) return;
    state.downloadDayPosts = {
      ...(state.downloadDayPosts || {}),
      loading: false,
      error: '读取作品失败：' + (error.message || error),
    };
  }
  renderDownloadsPanel();
}

export function loadMoreDownloadDayPosts() {
  const cursor = state.downloadDayPosts?.next_cursor;
  if (!cursor) return;
  return loadDownloadDayPosts({cursor});
}

// Record a reversible decision. The expected manifest version goes with the
// request so a list that changed since it was displayed is refused instead of
// being answered for a post the user never saw.
export async function decideDownloadPost(postId, action) {
  const id = Number(postId);
  if (!Number.isFinite(id) || !action) return;
  const post = (state.downloadDayPosts?.posts || []).find(row => Number(row.id) === id);
  if (!post) return;
  try {
    await API.postJson(`/api/pawchive/posts/${id}/decisions`, {
      action,
      expected_manifest_version: Number(post.manifest_version || 0),
    });
    logUiAction('download_post_decision', {id, action});
  } catch (error) {
    toast('更新作品状态失败：' + (error.message || error), 'error');
  }
  // Re-read either way: a conflict changed the list under the click, and the
  // server's version is the one worth showing.
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

function renderDownloadSyncStatus() {
  const el = $('#downloadSyncStatus');
  const btn = $('#downloadSyncBtn');
  const checkBtn = $('#downloadCheckBtn');
  const reconcileBtn = $('#downloadReconcileBtn');
  const status = state.downloadSyncStatus;
  const running = Boolean(status && status.running);
  if (btn) btn.disabled = running || !state.downloadSettings?.enabled;
  // A check shares the round's single slot and its master switch, so it is
  // gated the same way: offering it while a round runs would only 409.
  if (checkBtn) checkBtn.disabled = running || !state.downloadSettings?.enabled;
  // A reconcile reads only the local library, so the master switch does not
  // apply to it; the single round slot still does.
  if (reconcileBtn) reconcileBtn.disabled = running || isActionBusy('downloadReconcile');
  if (!el) return;
  // A check and a sync share one status slot, so the label follows the round's
  // own trigger rather than assuming every round downloads.
  const check = String(status?.trigger || '') === 'check';
  if (running) {
    el.textContent = check ? '检查中' : '同步中';
    return;
  }
  if (status?.last_error) {
    el.textContent = (check ? '上次检查失败：' : '上次同步失败：') + String(status.last_error);
    return;
  }
  // `last_summary` is the sync round's own counts payload. It is deliberately
  // not named after the legacy hash worker's raw result key, which the static
  // UI contract test bans from the frontend bundle.
  const result = status?.last_summary;
  if (!result) {
    el.textContent = '';
    return;
  }
  if (result.enabled === false) {
    el.textContent = '订阅下载未启用';
    return;
  }
  // A reconcile pass reports what the library holds, not what a round fetched.
  if (result.claimable !== undefined) {
    el.textContent = `上次核对：${Number(result.posts || 0)} 篇作品，${Number(result.verified || 0)} 篇已核验，`
      + `${Number(result.confirmed || 0)} 篇已确认，${Number(result.unverified || 0)} 篇待核验，`
      + `${Number(result.claimable || 0)} 篇可下载`;
    return;
  }
  const subscriptions = Number(result.synced_subscriptions || 0);
  const discovered = Number(result.discovered_posts || 0);
  const missing = Number(result.missing_days || 0);
  const isCheck = String(result.trigger || '') === 'check';
  el.textContent = isCheck
    ? `上次检查：订阅 ${subscriptions} 个，新增 ${discovered} 篇，${missing} 天待补`
    : `上次同步：订阅 ${subscriptions} 个，新增 ${discovered} 篇，下载 ${Number(result.downloaded_files || 0)} 个文件`;
}

function readDownloadsSettingsFromForm() {
  const form = {
    enabled: Boolean($('#downloadEnabledToggle')?.checked),
    auto_netdisk: Boolean($('#downloadJdownloaderToggle')?.checked),
    auto_ingest: Boolean($('#downloadAutoIngestToggle')?.checked),
    interval_hours: Number($('#downloadIntervalSelect')?.value || 6),
    folder_template: String($('#downloadFolderTemplateInput')?.value || ''),
    image_template: String($('#downloadImageTemplateInput')?.value || ''),
    attachment_template: String($('#downloadAttachmentTemplateInput')?.value || ''),
  };
  // The segments write straight into state on toggle, so re-reading the DOM
  // would lose the pending change before the save posts it.
  for (const {key} of DOWNLOAD_TYPES) {
    form[`download_${key}`] = downloadTypeEnabled(key);
  }
  return form;
}

export async function saveDownloadsSettings() {
  if (isActionBusy('downloadSettingsSave')) return;
  // Never post a form that was never populated. When the settings read failed
  // every segment reads off and every template reads empty, so saving would
  // overwrite the stored configuration with blanks — which is how a live
  // install lost its templates and every file type.
  if (!state.downloadSettings) {
    toast('下载设置尚未读取，暂时无法保存', 'error');
    return;
  }
  setActionBusy('downloadSettingsSave', '', true);
  const statusEl = $('#downloadSettingsStatus');
  if (statusEl) statusEl.textContent = '保存中';
  try {
    const result = await API.putJson('/api/pawchive/settings', readDownloadsSettingsFromForm());
    state.downloadSettings = result?.settings || state.downloadSettings;
    // The form now matches the server again, so the panel may repopulate.
    state.downloadSettingsDirty = false;
    if (statusEl) statusEl.textContent = '已保存';
    logUiAction('download_settings_save', {enabled: Boolean(state.downloadSettings?.enabled)});
  } catch (error) {
    if (statusEl) statusEl.textContent = '';
    toast('保存下载设置失败：' + (error.message || error), 'error');
  } finally {
    setActionBusy('downloadSettingsSave', '', false);
    renderDownloadsPanel();
  }
}

export function resetDownloadTemplates() {
  const defaults = state.downloadDefaults;
  if (!defaults) return;
  const folderInput = $('#downloadFolderTemplateInput');
  if (folderInput) folderInput.value = String(defaults.folder_template || '');
  const imageInput = $('#downloadImageTemplateInput');
  if (imageInput) imageInput.value = String(defaults.image_template || '');
  const attachmentInput = $('#downloadAttachmentTemplateInput');
  if (attachmentInput) attachmentInput.value = String(defaults.attachment_template || '');
  // The form no longer matches the server, so the panel must not repopulate
  // it until the user saves.
  markDownloadSettingsDirty();
  logUiAction('download_templates_reset', {});
}

export function rememberDownloadTemplateInput(inputId) {
  if (downloadTemplateInputSelector(inputId)) state.downloadActiveTemplateInput = inputId;
}

// Insert a variable into whichever template input the user last touched, at the
// caret. Falls back to the folder template so a chip click is never a no-op.
export function insertDownloadTemplateToken(value) {
  if (!value) return;
  const inputId = state.downloadActiveTemplateInput || 'folder';
  const input = downloadTemplateInput(inputId) || downloadTemplateInput('folder');
  if (!input || input.disabled) return;
  const start = input.selectionStart ?? input.value.length;
  const end = input.selectionEnd ?? input.value.length;
  input.value = input.value.slice(0, start) + value + input.value.slice(end);
  input.focus();
  input.setSelectionRange(start + value.length, start + value.length);
  rememberDownloadTemplateInput(inputId);
}

export async function addDownloadSubscription() {
  if (isActionBusy('downloadSubscriptionAdd')) return;
  const urlInput = $('#downloadSubscriptionUrl');
  const url = String(urlInput?.value || '').trim();
  const statusEl = $('#downloadSubscriptionStatus');
  if (!url) {
    if (statusEl) statusEl.textContent = '请先填写作者主页链接';
    return;
  }
  // A picked artist wins. Otherwise the text left in the box is the name of the
  // folder to bind, under the media root and parent picked above; only an empty
  // box keeps the author-ID default.
  const pickedDir = String($('#downloadSubscriptionArtistSelect')?.value || '').trim();
  const typedDir = String($('#downloadSubscriptionArtistInput')?.value || '').trim();
  const targetDir = pickedDir || downloadArtistNewFolderPath(typedDir);
  const sinceDate = String($('#downloadSubscriptionSince')?.value || '').trim();
  setActionBusy('downloadSubscriptionAdd', '', true);
  if (statusEl) statusEl.textContent = '添加中';
  try {
    const result = await API.postJson('/api/pawchive/subscriptions', {
      url,
      target_dir: targetDir || null,
      since_date: sinceDate || null,
    });
    if (urlInput) urlInput.value = '';
    resetDownloadArtistCombo();
    if (statusEl) {
      const name = result?.subscription?.artist_name || result?.subscription?.user_id || '';
      // The status sits beside the button now: it names the subscription that
      // just landed and points at where to find it in the list below.
      statusEl.textContent = name ? `已添加 ${name}，见下方列表` : '已添加，见下方列表';
    }
    logUiAction('download_subscription_add', {
      service_bound: Boolean(pickedDir),
      typed_folder: Boolean(!pickedDir && typedDir),
    });
    await loadDownloadsPanel();
    // A brand-new subscription has discovered nothing yet, so 全部作品 is empty
    // for it until a round runs. Run this subscription's check now instead of
    // making the user wait for the scheduled round or press a second button.
    const addedId = Number(result?.subscription?.id);
    if (Number.isFinite(addedId)) startSubscriptionRound(addedId, 'check');
  } catch (error) {
    if (statusEl) statusEl.textContent = '';
    toast('添加订阅失败：' + (error.message || error), 'error');
  } finally {
    setActionBusy('downloadSubscriptionAdd', '', false);
    renderDownloadsPanel();
  }
}

export function resetDownloadArtistCombo() {
  const select = $('#downloadSubscriptionArtistSelect');
  const input = $('#downloadSubscriptionArtistInput');
  if (select) select.value = '';
  if (input) input.value = '';
  closeDownloadArtistCombo();
}

export async function deleteDownloadSubscription(id) {
  const subscriptionId = Number(id);
  if (!Number.isFinite(subscriptionId)) return;
  if (!window.confirm('删除这个订阅？已下载的文件不会删除。')) return;
  try {
    await API.del(`/api/pawchive/subscriptions/${subscriptionId}`);
    logUiAction('download_subscription_delete', {id: subscriptionId});
    await loadDownloadsPanel();
  } catch (error) {
    toast('删除订阅失败：' + (error.message || error), 'error');
  }
}

export async function toggleDownloadSubscription(id, enabled) {
  const subscriptionId = Number(id);
  if (!Number.isFinite(subscriptionId)) return;
  try {
    await API.postJson(`/api/pawchive/subscriptions/${subscriptionId}/toggle`, {enabled: Boolean(enabled)});
    logUiAction('download_subscription_toggle', {id: subscriptionId, enabled: Boolean(enabled)});
    await loadDownloadsPanel();
  } catch (error) {
    toast('更新订阅状态失败：' + (error.message || error), 'error');
    await loadDownloadsPanel();
  }
}

// Switch a subscription between 自动 and 手动. The round only claims 自动
// subscriptions, so this is what decides whether missing resources are fetched
// on their own or the user picks works from the list. It never starts a
// download and never touches what was already recorded: only the claim rule.
//
// One switch per subscription at a time. Two in flight would race on the same
// row, and the response that lands last would win regardless of which one the
// user chose last — on a control whose whole purpose is to stop automatic
// claiming, silently ending up back on 自动 is the failure that matters.
export async function setDownloadSubscriptionMode(id, mode) {
  const subscriptionId = Number(id);
  if (!Number.isFinite(subscriptionId)) return;
  if (isActionBusy('downloadSubscriptionMode', id)) return;
  setActionBusy('downloadSubscriptionMode', id, true);
  const next = String(mode) === 'auto' ? 'auto' : 'manual';
  try {
    await API.postJson(`/api/pawchive/subscriptions/${subscriptionId}/mode`, {mode: next});
    logUiAction('download_subscription_mode', {id: subscriptionId, mode: next});
    await loadDownloadsPanel();
  } catch (error) {
    // Say that the control is about to move back on its own. The toast is
    // short-lived and the panel re-reads the server value, so without this the
    // only trace of the failure is a select that snapped back.
    toast('切换下载模式失败，已恢复原设置：' + (error.message || error), 'error');
    await loadDownloadsPanel();
  } finally {
    setActionBusy('downloadSubscriptionMode', id, false);
    renderDownloadsPanel();
  }
}

// The works part of a selection body. A selection names its works one of two
// ways and never both: an explicit id list, or the 全部作品 filter that stands for
// them. The filter form is what lets "全选当前筛选" be one request — posting the
// tens of thousands of ids behind the filter would be the very body the filter
// exists to avoid — so both the preview and the freeze take this shape.
export function downloadSelectionWorks(postIds, options = {}) {
  const body = {post_ids: downloadIdList(postIds), file_ids: downloadIdList(options.fileIds)};
  if (options.filter) {
    body.filter = options.filter;
    body.subscription_id = options.subscriptionId ?? null;
  }
  return body;
}

// Ask what the selected works would fetch, without recording anything. The
// preview is what the panel shows before the user commits, and it is also how a
// work whose resources are all external links is answered: the backend reports
// those in `no_direct_resources` instead of accepting a request that fetches 0.
export async function previewDownloadSelection(postIds, fileIds = []) {
  if (isActionBusy('downloadSelection')) return null;
  return API.postJson(
    '/api/pawchive/selections/preview',
    {request_id: '', ...downloadSelectionWorks(postIds, {fileIds})}
  );
}

// A caller that passes a bare id or `undefined` must not turn into a TypeError
// at `.map`: every entry point in this panel is a click handler or a DOM data
// attribute, so the input is only ever "whatever the markup carried".
function downloadIdList(value) {
  const list = Array.isArray(value) ? value : [value];
  return list.map(Number).filter(Number.isFinite);
}

// Freeze the selection, then create and run the task it was made for.
//
// The request id is generated once per press: re-sending the same id is the
// same task, while pressing again later makes a new one, which is what "下载所
// 选" has to mean for a work that is already recorded as fetched. `crypto`
// randomUUID is used when the page has it, with a timestamp fallback so the
// button still works in a plain non-secure context.
function newDownloadRequestId() {
  if (globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function') {
    return globalThis.crypto.randomUUID();
  }
  return `req-${Date.now()}-${Math.floor(Math.random() * 1e9)}`;
}

export async function startDownloadSelection(postIds, fileIds = []) {
  const posts = downloadIdList(postIds);
  if (!posts.length) {
    toast('先选中要下载的作品', 'warn');
    return null;
  }
  // One press, one task. Without this guard a double click (or a retry while the
  // first request is still in flight) mints a second request id, so the
  // backend's idempotency key sees two different tasks and fetches the same
  // resources twice.
  if (isActionBusy('downloadSelection')) return null;
  setActionBusy('downloadSelection', '', true);
  const requestId = newDownloadRequestId();
  try {
    const selection = await API.postJson(
      '/api/pawchive/selections',
      {request_id: requestId, ...downloadSelectionWorks(posts, {fileIds})}
    );
    const attempt = await API.postJson('/api/pawchive/attempts', {
      selection_id: selection.selection_id,
      request_id: requestId,
    });
    logUiAction('download_selection_start', {
      posts: posts.length,
      resources: attempt.resources,
    });
    return attempt;
  } catch (error) {
    toast('发起下载失败：' + (error.message || error), 'error');
    return null;
  } finally {
    setActionBusy('downloadSelection', '', false);
    renderDownloadsPanel();
  }
}

// The attempts a post has, newest first, so the panel can say which work was
// asked for by hand and how each request ended.
//
// A read failure is reported as one. Answering `[]` turned "the read failed" into
// "this work was never requested by hand", which is the opposite conclusion and
// the one the panel then showed.
export async function loadDownloadPostAttempts(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return {attempts: [], error: ''};
  try {
    const result = await API.get(`/api/pawchive/posts/${id}/attempts`);
    return {
      attempts: Array.isArray(result?.attempts) ? result.attempts : [],
      error: '',
    };
  } catch (error) {
    if (isAbortError(error)) return {attempts: [], error: ''};
    return {attempts: [], error: '读取手动请求记录失败：' + (error.message || error)};
  }
}

// One attempt's line under its work, in this work's own numbers: how many files
// this request delivered for it, and the failure it recorded when it delivered
// none. The attempt's total is not shown here — on a row for a work the request
// could not touch, that total reads as a delivery that never happened.
export function downloadAttemptText(attempt) {
  const state = String(attempt?.state || '');
  const label = downloadAttemptStateLabel(state);
  const fetched = Number(attempt?.fetched || 0);
  const error = String(attempt?.error || '').trim();
  if (error) return `${label}，${error}`;
  if (state === 'done') return fetched ? `${label}，${fetched} 个文件` : label;
  return label;
}

export async function startDownloadSync() {
  if (isActionBusy('downloadSync')) return;
  setActionBusy('downloadSync', '', true);
  try {
    const result = await API.postJson('/api/pawchive/sync', {});
    applySyncStatus(result?.status || {running: true});
    logUiAction('download_sync_start', {});
  } catch (error) {
    // The route answers 409 when a round is already in flight (the background
    // loop, or another tab). That is not a failure the user caused: re-read the
    // status and follow the running round instead of raising an error toast.
    let status = null;
    try {
      const result = await API.get('/api/pawchive/status');
      status = result?.status || null;
    } catch (e) {
      // Keep status null so the original failure is still reported below.
    }
    if (status?.running) {
      applySyncStatus(status);
    } else {
      toast('启动同步失败：' + (error.message || error), 'error');
    }
  } finally {
    setActionBusy('downloadSync', '', false);
    renderDownloadSyncStatus();
  }
}

// Ask for a discovery pass that recomputes coverage without downloading.
export async function checkDownloadMissing() {
  if (isActionBusy('downloadCheck')) return;
  setActionBusy('downloadCheck', '', true);
  try {
    const result = await API.postJson('/api/pawchive/check', {});
    applySyncStatus(result?.status || {running: true, trigger: 'check'});
    logUiAction('download_check_start', {});
  } catch (error) {
    // Same 409 handling as a sync: a round already in flight is not a failure.
    let status = null;
    try {
      const result = await API.get('/api/pawchive/status');
      status = result?.status || null;
    } catch (e) {
      // Keep status null so the original failure is still reported below.
    }
    if (status?.running) {
      applySyncStatus(status);
    } else {
      toast('启动检查失败：' + (error.message || error), 'error');
    }
  } finally {
    setActionBusy('downloadCheck', '', false);
    renderDownloadSyncStatus();
  }
}

// Re-read the local library and re-derive every post, without touching the
// network or downloading anything. This is the action that answers "what do we
// actually have?" before any download is authorised.
export async function reconcileDownloadLibrary() {
  if (isActionBusy('downloadReconcile')) return;
  setActionBusy('downloadReconcile', '', true);
  try {
    const result = await API.postJson('/api/pawchive/reconcile', {});
    applySyncStatus(result?.status || {running: true, trigger: 'check'});
    logUiAction('download_reconcile_start', {});
  } catch (error) {
    // Same 409 handling as a sync: a round already in flight is not a failure.
    let status = null;
    try {
      const result = await API.get('/api/pawchive/status');
      status = result?.status || null;
    } catch (e) {
      // Keep status null so the original failure is still reported below.
    }
    if (status?.running) {
      applySyncStatus(status);
    } else {
      toast('启动核对失败：' + (error.message || error), 'error');
    }
  } finally {
    setActionBusy('downloadReconcile', '', false);
    renderDownloadSyncStatus();
  }
}

// 检查缺失 / 核对本地 for one subscription: the same two rounds as the buttons in
// 立即操作, narrowed to that artist. They share the single round slot, so the
// 409 handling is the same and the panel's status line keeps describing
// whichever round actually won the slot.
async function startSubscriptionRound(subscriptionId, action) {
  const label = action === 'reconcile' ? '核对' : '检查';
  if (isActionBusy('downloadSubscriptionRound', subscriptionId)) return;
  setActionBusy('downloadSubscriptionRound', subscriptionId, true);
  try {
    const result = await API.postJson(
      `/api/pawchive/subscriptions/${subscriptionId}/${action}`,
      {}
    );
    applySyncStatus(result?.status || {running: true, trigger: 'check'});
    logUiAction(action === 'reconcile' ? 'download_reconcile_start' : 'download_check_start', {
      id: subscriptionId,
    });
  } catch (error) {
    // Same 409 handling as the panel-wide actions: a round already in flight is
    // not a failure, so follow that round instead of raising an error toast.
    let status = null;
    try {
      const result = await API.get('/api/pawchive/status');
      status = result?.status || null;
    } catch (e) {
      // Keep status null so the original failure is still reported below.
    }
    if (status?.running) {
      applySyncStatus(status);
    } else {
      toast(`启动${label}失败：` + (error.message || error), 'error');
    }
  } finally {
    setActionBusy('downloadSubscriptionRound', subscriptionId, false);
    renderDownloadsPanel();
  }
}

export function checkSubscriptionMissing(id) {
  const subscriptionId = Number(id);
  if (Number.isFinite(subscriptionId)) startSubscriptionRound(subscriptionId, 'check');
}

export function reconcileSubscriptionLibrary(id) {
  const subscriptionId = Number(id);
  if (Number.isFinite(subscriptionId)) startSubscriptionRound(subscriptionId, 'reconcile');
}

// The candidate list for one post, once its row has been asked for it.
//
// Candidates are files the library already indexes: the post's own rendered
// folder first, then the same artist's files from the same days. Choosing one
// records a normal confirmation bound to it — the list itself never settles the
// post.
function downloadCandidatesHtml(postId) {
  const view = state.downloadCandidates?.[postId];
  if (!view) return '';
  if (view.loading) return '<div class="download-candidates"><span class="small">读取候选中</span></div>';
  if (view.error) return `<div class="download-candidates"><span class="download-subscription-error">${escHtml(view.error)}</span></div>`;
  if (!view.items.length) return '<div class="download-candidates"><span class="small">没有找到候选文件</span></div>';
  const rows = view.items.map(item => `
      <div class="download-candidate" data-download-candidate="${item.item_id}">
        <span class="download-candidate-path" title="${escHtml(item.file_path)}">${escHtml(item.file_path)}</span>
        <span class="download-candidate-size">${escHtml(downloadCandidateReason(item.reason))}</span>
        <button class="btn btn-ghost" type="button" data-download-post-candidate-bind="${postId}" data-download-candidate-path="${escHtml(item.file_path)}">绑定</button>
      </div>`).join('');
  return `<div class="download-candidates">${rows}</div>`;
}

export function downloadCandidateReason(reason) {
  return String(reason || '') === 'folder' ? '本作品目录' : '同画师近日';
}

// Ask for the candidates of one post.
export async function loadDownloadCandidates(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  state.downloadCandidates = {...(state.downloadCandidates || {}), [id]: {loading: true, items: [], error: ''}};
  renderDownloadsPanel();
  try {
    const result = await API.get(`/api/pawchive/posts/${id}/candidates?limit=20`);
    const items = Array.isArray(result?.candidates) ? result.candidates : [];
    state.downloadCandidates = {...(state.downloadCandidates || {}), [id]: {loading: false, items, error: ''}};
  } catch (error) {
    if (isAbortError(error)) return;
    state.downloadCandidates = {
      ...(state.downloadCandidates || {}),
      [id]: {loading: false, items: [], error: '读取候选失败：' + (error.message || error)},
    };
  }
  renderDownloadsPanel();
}

// Confirm one post, binding the chosen file so the record says where the
// content was seen.
export async function bindDownloadCandidate(postId, path) {
  const id = Number(postId);
  const bound = String(path || '');
  if (!Number.isFinite(id) || !bound) return;
  const post = (state.downloadDayPosts?.posts || []).find(row => Number(row.id) === id);
  try {
    await API.postJson(`/api/pawchive/posts/${id}/decisions`, {
      action: 'confirm',
      bound_path: bound,
      expected_manifest_version: Number(post?.manifest_version || 0),
    });
    logUiAction('download_post_candidate_bind', {id});
  } catch (error) {
    toast('绑定候选失败：' + (error.message || error), 'error');
  }
  if (state.downloadCandidates) delete state.downloadCandidates[id];
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

// Hash a post's delivered files against the source's own hashes.
// ---------------------------------------------------------------------------
// 停止 / 单文件重试 / 手动导入.
//
// The routes have existed since 1.0.374; these three are the entries the plan
// keeps asking for. Each one is an explicit instruction about one work or one
// resource, so none of them is gated on the subscription switch — and none of
// them claims more than the server answered.
// ---------------------------------------------------------------------------

// Attempt states the user can still stop. Anything else has already finished,
// and the route answers 409 rather than pretending to cancel it.
const CANCELLABLE_ATTEMPT_STATES = ['queued', 'running'];

export async function stopDownloadPost(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  try {
    const body = await API.get(`/api/pawchive/posts/${id}/attempts`);
    const attempts = Array.isArray(body?.attempts) ? body.attempts : [];
    const live = attempts.find(item => CANCELLABLE_ATTEMPT_STATES.includes(String(item.state || '')));
    if (!live) {
      // Saying "stopped" here would be the panel inventing an outcome.
      toast('这篇作品没有正在进行的手动请求', 'info');
      return;
    }
    await API.post(`/api/v1/attempts/${encodeURIComponent(String(live.attempt_id))}/cancel`);
    toast('已请求停止，正在下载的文件会写完当前这一个', 'info');
    logUiAction('download_post_stop', {id, attemptId: live.attempt_id});
  } catch (error) {
    toast('停止失败：' + (error.message || error), 'error');
  }
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

// Read one work's resources. `has_evidence` is what decides whether 重试 is
// offered at all: a file already bound to a library item has nothing to retry.
export async function openDownloadPostFiles(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  const current = state.downloadPostFiles[id];
  if (current && !current.error) {
    delete state.downloadPostFiles[id];
    renderDownloadsPanel();
    return;
  }
  const request = {loading: true, files: []};
  state.downloadPostFiles[id] = request;
  renderDownloadsPanel();
  try {
    const [filesResult, factsResult] = await Promise.allSettled([
      API.get(`/api/pawchive/posts/${id}/files`),
      API.get(`/api/pawchive/posts/${id}/acquisition`),
    ]);
    // Closing and reopening the list must not let an older response replace it.
    if (state.downloadPostFiles[id] !== request) return;
    if (filesResult.status === 'rejected') throw filesResult.reason;
    const body = filesResult.value;
    state.downloadPostFiles[id] = {
      loading: false,
      files: Array.isArray(body?.files) ? body.files : [],
      facts: factsResult.status === 'fulfilled' ? factsResult.value : null,
      factsError: factsResult.status === 'rejected'
        ? String(factsResult.reason?.message || factsResult.reason) : '',
    };
  } catch (error) {
    if (state.downloadPostFiles[id] !== request) return;
    state.downloadPostFiles[id] = {loading: false, error: String(error.message || error), files: []};
  }
  renderDownloadsPanel();
}

export function downloadPostFiles(postId) {
  return state.downloadPostFiles[Number(postId)] || null;
}

export async function retryDownloadFile(postId, fileId) {
  const id = Number(postId);
  const file = Number(fileId);
  if (!Number.isFinite(id) || !Number.isFinite(file)) return;
  try {
    await API.post(`/api/pawchive/posts/${id}/files/${file}/retry`);
    toast('已重新获取这一个文件', 'info');
    logUiAction('download_file_retry', {id, fileId: file});
  } catch (error) {
    // 404 here is "this file is not this work's", 409 is "a live pass holds
    // it" — both are the server's own words and are shown as they come back.
    toast('重试失败：' + (error.message || error), 'error');
  }
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

export async function importDownloadPost(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  try {
    const body = await API.post(`/api/pawchive/posts/${id}/import`);
    const linked = Number(body?.linked || 0);
    const total = Number(body?.total || 0);
    // Reported as "linked / total" rather than as success: a work whose files
    // are not all on disk yet is not imported, and saying "入库完成" would be
    // the panel's own conclusion rather than the server's answer.
    toast(total ? `已绑入库内 ${linked}/${total} 个文件` : '这篇作品还没有可入库的文件', 'info');
    logUiAction('download_post_import', {id, linked, total});
  } catch (error) {
    toast('入库失败：' + (error.message || error), 'error');
  }
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

function downloadPostFilesHtml(postId) {
  const view = state.downloadPostFiles[postId];
  if (!view) return '';
  if (view.loading) {
    return '<div class="download-post-files"><div class="move-empty small">读取文件中</div></div>';
  }
  if (view.error) {
    return `<div class="download-post-files"><div class="download-subscription-error">${escHtml(view.error)}</div></div>`;
  }
  const facts = downloadPostFactsHtml(view);
  if (!view.files.length) {
    return `<div class="download-post-files">${facts}<div class="move-empty small">这篇作品还没有已知文件</div></div>`;
  }
  const rows = view.files.map(file => {
    const fileId = Number(file.file_id);
    const name = escHtml(String(file.file_name || file.file_id));
    const meta = [escHtml(String(file.status || ''))];
    if (Number(file.expected_length || 0) > 0) meta.push(`${Number(file.expected_length)} 字节`);
    if (file.error) meta.push(escHtml(String(file.error)));
    // A file already bound to a library item is not a retry candidate: the
    // route refuses it, so the button is not offered.
    const retry = file.has_evidence
      ? ''
      : `<button class="btn btn-ghost" type="button" data-download-file-retry="${fileId}" data-download-file-post="${postId}">重试</button>`;
    const mark = file.has_evidence ? '<span class="download-file-linked">已入库</span>' : '';
    return `
      <div class="download-file-row" data-download-file="${fileId}">
        <div class="download-file-main">
          <div class="download-file-name">${name}</div>
          <div class="download-file-meta">${meta.filter(Boolean).join(' &#183; ')}</div>
        </div>
        <div class="download-file-actions">${mark}${retry}</div>
      </div>`;
  }).join('');
  return `<div class="download-post-files">${facts}${rows}</div>`;
}

function downloadPostFactsHtml(view) {
  if (view.factsError) {
    return `<div class="download-subscription-error">状态读取失败：${escHtml(view.factsError)}</div>`;
  }
  const facts = view.facts;
  if (!facts || !Array.isArray(facts.requested_resources)) return '';
  const count = value => Array.isArray(value) ? value.length : 0;
  const amount = value => Number.isFinite(Number(value)) ? Math.max(0, Number(value)) : 0;
  const integrity = facts.integrity || {};
  const demand = facts.needs_confirmation || facts.blocked_by_ambiguity
    ? '待确认' : `待获取 ${count(facts.requires_fetch)} 项`;
  const lines = [
    `获取记录：已获取 ${count(facts.acquired_resources)} / 需要 ${count(facts.requested_resources)}`,
    `目录关联：${count(facts.association)} 组`,
    `资源需求：${demand}${facts.manifest_complete ? '' : '，清单未完整'}`,
    `完整性：未核验 ${amount(integrity.unverified_assets)}，缺失 ${amount(integrity.missing_assets)}，异常 ${amount(integrity.integrity_failed_assets)}`,
  ];
  const reason = facts.blocked_reason || integrity.reason;
  return `<div data-download-post-facts>${lines.map(line => `<div class="download-file-meta">${escHtml(line)}</div>`).join('')}${reason ? `<div class="download-file-meta">${escHtml(String(reason))}</div>` : ''}</div>`;
}

export async function verifyDownloadPost(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  try {
    const result = await API.postJson(`/api/pawchive/posts/${id}/verify`, {});
    const report = result?.report || {};
    const checked = Number(report.checked || 0);
    const matched = Number(report.matched || 0);
    const mismatched = Number(report.mismatched || 0);
    const skipped = Number(report.skipped || 0);
    // "Checked 0" is a real answer: the source published no hash to compare
    // with, so nothing was verified and the message must not imply otherwise.
    toast(
      checked
        ? `校验完成：${matched} 个一致，${mismatched} 个不一致${skipped ? `，${skipped} 个无来源哈希` : ''}`
        : '没有可比对的来源哈希，未做校验',
      mismatched ? 'error' : 'info'
    );
    logUiAction('download_post_verify', {id, matched, mismatched, skipped});
  } catch (error) {
    toast('校验失败：' + (error.message || error), 'error');
  }
  await loadDownloadDayPosts();
  await loadDownloadsPanel();
}

function startDownloadSyncPolling() {
  if (state.downloadSyncPolling) return;
  state.downloadSyncPolling = true;
  let attempts = 0;
  const tick = async () => {
    attempts += 1;
    if (attempts > SYNC_POLL_MAX) {
      stopDownloadSyncPolling();
      toast('同步仍在进行，请稍后刷新查看结果', 'error');
      return;
    }
    try {
      const result = await API.get('/api/pawchive/status');
      const status = result?.status || null;
      state.downloadSyncStatus = status;
      renderDownloadSyncStatus();
      if (!status || !status.running) {
        stopDownloadSyncPolling();
        // New posts and file statuses changed while the round ran. 全部作品 is
        // what a round's discovery fills, so an open list re-reads too instead
        // of showing the pre-round page until it is reopened.
        state.downloadAllWorksRevision = Number(state.downloadAllWorksRevision || 0) + 1;
        await loadDownloadsPanel();
        return;
      }
    } catch (e) {
      // A transient status failure is not a sync failure; keep polling.
    }
    if (state.downloadSyncPolling) {
      state.downloadSyncPollTimer = setTimeout(tick, SYNC_POLL_MS);
    }
  };
  state.downloadSyncPollTimer = setTimeout(tick, SYNC_POLL_MS);
}

export function stopDownloadSyncPolling() {
  state.downloadSyncPolling = false;
  if (state.downloadSyncPollTimer) {
    clearTimeout(state.downloadSyncPollTimer);
    state.downloadSyncPollTimer = null;
  }
}

// ---- 全部作品：跨订阅的稳定游标列表与跨页选择 ----
//
// The per-subscription day lists answer "what is this creator missing"; this view
// answers "what is in the library at all", which is the question a manual
// snapshot is made from. It is the same API the panel's other lists use
// (`/api/pawchive/posts` → `list_artist_posts_page`), so the total, the cursor and
// the state column are the backend's, not a second interpretation of them.
//
// Selection is two different things and the panel keeps them apart: a set of
// explicitly ticked works, and "all works of the current filter". The second is
// not a set of ids — it is the filter, which is why the request that freezes it
// sends the filter rather than the thousands of ids it stands for.

function selectedDownloadWorks() {
  return state.downloadAllSelection instanceof Set ? state.downloadAllSelection : new Set();
}

// Whether the 全部作品 view is narrowed at all. Selection lives under the filter,
// so what the view knows is a selection *of this filter* and nothing else.
export function downloadWorksFilterActive() {
  const filter = downloadWorksFilter();
  return Boolean(
    filter.day || filter.state || filter.search || filter.artist_id || downloadWorksSubscriptionId()
  );
}

export function downloadWorksSelected() {
  return state.downloadAllSelectAll ? null : [...selectedDownloadWorks()];
}

export function downloadWorksSelectAllActive() {
  return Boolean(state.downloadAllSelectAll);
}

// The filter the list is showing, in the shape the API takes. Kept as one
// function so the request that reads the list and the request that freezes it
// cannot describe two different filters.
export function downloadWorksFilter() {
  const filter = state.downloadAllFilter || {};
  const text = value => {
    const trimmed = String(value || '').trim();
    return trimmed ? trimmed : null;
  };
  return {
    day: text(filter.day),
    state: text(filter.state),
    search: text(filter.search),
    artist_id: Number.isFinite(Number(filter.artistId)) && filter.artistId ? Number(filter.artistId) : null,
  };
}

function downloadWorksSubscriptionId() {
  const value = Number(state.downloadAllFilter?.subscriptionId);
  return Number.isFinite(value) && value ? value : null;
}

export function downloadWorksQuery(cursor, limit = DOWNLOAD_WORKS_PAGE_SIZE) {
  const filter = downloadWorksFilter();
  const parts = [`limit=${encodeURIComponent(String(limit))}`];
  const subscriptionId = downloadWorksSubscriptionId();
  if (subscriptionId) parts.push(`subscription_id=${encodeURIComponent(String(subscriptionId))}`);
  if (filter.day) parts.push(`day=${encodeURIComponent(filter.day)}`);
  if (filter.state) parts.push(`state=${encodeURIComponent(filter.state)}`);
  if (filter.search) parts.push(`search=${encodeURIComponent(filter.search)}`);
  if (filter.artist_id) parts.push(`artist_id=${encodeURIComponent(String(filter.artist_id))}`);
  if (cursor) parts.push(`cursor=${encodeURIComponent(cursor)}`);
  return '/api/pawchive/posts?' + parts.join('&');
}

export function downloadWorksDayPosts(result) {
  return Array.isArray(result?.posts) ? result.posts : [];
}

function downloadWorksArtistQuery() {
  return String($('#downloadWorksArtistInput')?.value || '').trim().toLowerCase();
}

function matchingDownloadWorksArtists(limit = 500) {
  const query = downloadWorksArtistQuery();
  const artists = Array.isArray(state.downloadArtists) ? state.downloadArtists : [];
  const rows = artists.filter(artist => artist && (artist.id || artist.name));
  if (!query) return limit ? rows.slice(0, limit) : rows;
  return rows
    .filter(artist => {
      const haystack = `${artist.name || ''} ${artist.path || ''} ${artist.id || ''}`.toLowerCase();
      return query.split(/\s+/).filter(Boolean).every(part => haystack.includes(part));
    })
    .slice(0, limit || rows.length);
}

export function renderDownloadWorksArtistCombo() {
  const input = $('#downloadWorksArtistInput');
  const hidden = $('#downloadWorksArtist');
  const listbox = $('#downloadWorksArtistListbox');
  if (!input || !hidden || !listbox) return;
  const matches = matchingDownloadWorksArtists();
  const picked = String(hidden.value || '');
  if (picked && !matches.some(artist => String(artist.id) === picked)) {
    const artist = downloadArtistRows().find(row => String(row.id) === picked);
    if (artist) matches.unshift(artist);
  }
  const allSelected = !picked ? ' active' : '';
  listbox.innerHTML = `<li role="option" data-download-works-artist-value="" class="${allSelected}">全部画师</li>`
    + matches.map(artist => {
      const name = String(artist.name || artist.path || artist.id);
      const idStr = String(artist.id);
      const selected = picked === idStr ? ' active' : '';
      return `<li role="option" data-download-works-artist-value="${escHtml(idStr)}" class="${selected}" title="${escHtml(name)}">${escHtml(name)}</li>`;
    }).join('');
}

export function openDownloadWorksArtistCombo() {
  const input = $('#downloadWorksArtistInput');
  const listbox = $('#downloadWorksArtistListbox');
  if (!input || !listbox) return;
  renderDownloadWorksArtistCombo();
  listbox.hidden = false;
  input.setAttribute('aria-expanded', 'true');
}

export function closeDownloadWorksArtistCombo() {
  const input = $('#downloadWorksArtistInput');
  const listbox = $('#downloadWorksArtistListbox');
  if (!input || !listbox) return;
  listbox.hidden = true;
  input.setAttribute('aria-expanded', 'false');
}

export function pickDownloadWorksArtist(value, label) {
  const hidden = $('#downloadWorksArtist');
  const input = $('#downloadWorksArtistInput');
  if (!hidden || !input) return;
  const idStr = String(value || '');
  hidden.value = idStr;
  if (!idStr) {
    input.value = '';
  } else {
    const artist = downloadArtistRows().find(row => String(row.id) === idStr);
    input.value = String(label || artist?.name || artist?.path || idStr);
  }
  closeDownloadWorksArtistCombo();
  applyDownloadWorksFilter({artistId: idStr ? Number(idStr) : null});
}

// Keep the filter controls showing the filter that is actually in force: a
// debounced search or a programmatic change updates state, and a control that
// kept the old value would describe a different list than the one on screen.
// The search box is left alone while it has focus, so a re-render never moves the
// caret to the end of a field the user is typing in.
function syncDownloadWorksFilterControls(searchFocused = false) {
  const filter = state.downloadAllFilter || {};
  const search = $('#downloadWorksSearch');
  if (search && !searchFocused) search.value = String(filter.search || '');
  const day = $('#downloadWorksDay');
  if (day) day.value = String(filter.day || '');
  const stateSelect = $('#downloadWorksState');
  if (stateSelect) stateSelect.value = String(filter.state || '');
  const artist = $('#downloadWorksArtist');
  const artistInput = $('#downloadWorksArtistInput');
  if (artist) {
    const wanted = filter.artistId ? String(filter.artistId) : '';
    if (artist.tagName === 'SELECT') {
      const options = downloadArtistRows()
        .map(row => `<option value="${escHtml(String(row.id))}">${escHtml(String(row.name || row.path || row.id))}</option>`)
        .join('');
      artist.innerHTML = `<option value="">全部画师</option>${options}`;
      artist.value = wanted;
      if (artist.value !== wanted) artist.value = '';
    } else {
      artist.value = wanted;
    }
    if (artistInput && document.activeElement !== artistInput) {
      if (wanted) {
        const artistRow = downloadArtistRows().find(row => String(row.id) === wanted);
        artistInput.value = artistRow ? String(artistRow.name || artistRow.path || artistRow.id) : wanted;
      } else {
        artistInput.value = '';
      }
    }
  }
}

// Re-read the first page, or append the next one.
//
// The read is a refresh, not a filter change: a selection already made is kept,
// and only a work the active filter no longer holds is dropped from it. Clearing
// the selection here instead would undo a cross-page choice every time a
// download finished and the list re-read itself.
export async function loadDownloadAllWorks(options = {}) {
  const cursor = options.cursor ? String(options.cursor) : '';
  const previous = cursor ? (state.downloadAllWorks?.posts || []) : [];
  state.downloadAllWorks = {
    loading: true,
    posts: previous,
    total: cursor ? Number(state.downloadAllWorks?.total || 0) : 0,
    next_cursor: cursor || null,
    truncated: Boolean(state.downloadAllWorks?.truncated),
    snapshot_revision: cursor ? String(state.downloadAllWorks?.snapshot_revision || '') : '',
    error: '',
  };
  renderDownloadAllWorks();
  try {
    const fetchOptions = options.signal ? {signal: options.signal} : {};
    const result = await API.get(downloadWorksQuery(cursor), fetchOptions);
    const posts = downloadWorksDayPosts(result);
    // A page the earlier read already holds is not appended twice when a refresh
    // lands after a "load more": the walk keeps its order and its rows.
    const merged = cursor ? [...previous, ...posts.filter(post => !previous.some(row => Number(row.id) === Number(post.id)))] : posts;
    state.downloadAllWorks = {
      loading: false,
      posts: merged,
      total: Number(result?.total || 0),
      next_cursor: result?.next_cursor ?? null,
      truncated: result?.truncated === true,
      snapshot_revision: String(result?.snapshot_revision || ''),
      error: '',
    };
    pruneDownloadWorksSelection(merged);
  } catch (error) {
    if (isAbortError(error)) return;
    state.downloadAllWorks = {
      ...(state.downloadAllWorks || {}),
      loading: false,
      error: '读取作品失败：' + (error.message || error),
    };
  }
  renderDownloadAllWorks();
}

export function loadMoreDownloadWorks() {
  const cursor = state.downloadAllWorks?.next_cursor;
  if (!cursor) return undefined;
  return loadDownloadAllWorks({cursor});
}

// Selection lives under the filter, so a ticked work only survives while a
// narrower filter still contains it. When the filter is the whole list, a work
// that fell out of it is gone: applying it later would fetch something the user
// can no longer see in the list they selected from.
function pruneDownloadWorksSelection(rows) {
  const filterActive = downloadWorksFilterActive();
  if (!filterActive || !state.downloadAllSelection?.size) return;
  const visible = new Set((Array.isArray(rows) ? rows : []).map(row => Number(row.id)));
  const kept = new Set([...selectedDownloadWorks()].filter(id => visible.has(Number(id))));
  if (kept.size !== state.downloadAllSelection.size) state.downloadAllSelection = kept;
}

// Read the list because the panel was opened, not because a tick fired. The
// revision makes "opened" idempotent while a refresh is already in flight: two
// loaders racing on one view re-read once, not twice.
export function openDownloadAllWorks() {
  state.downloadAllWorksRevision = Number(state.downloadAllWorksRevision || 0) + 1;
  if (state.downloadAllWorksLoadedRevision === state.downloadAllWorksRevision) return undefined;
  state.downloadAllWorksLoadedRevision = state.downloadAllWorksRevision;
  return loadDownloadAllWorks();
}

export function toggleDownloadWorkSelection(postId, selected) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return;
  const next = new Set(selectedDownloadWorks());
  if (selected) next.add(id);
  else next.delete(id);
  state.downloadAllSelection = next;
  // A ticked row after "all works of this filter" means the two are no longer
  // the same statement, so the explicit set takes over from the filter.
  if (state.downloadAllSelectAll) state.downloadAllSelectAll = false;
  renderDownloadAllWorks();
}

// 全选当前筛选 is the filter, not a page: the checkbox says so in its own label,
// and the counter below it names the works it covers so a click that selected
// more than the user could see is never silent.
export function toggleDownloadWorksSelectAll(selected) {
  if (selected) {
    const loaded = state.downloadAllWorks?.posts?.length || 0;
    if (!loaded) {
      toast('当前筛选下没有可选择的作品', 'warn');
      return;
    }
    state.downloadAllSelectAll = true;
    state.downloadAllSelection = new Set();
  } else {
    state.downloadAllSelectAll = false;
  }
  renderDownloadAllWorks();
}

// Clear both halves of the selection and redraw: the checkbox, the counter and
// the row highlights are the only places the state is visible, so a clear that
// left them showing the old selection would be a lie the next click acts on.
export function clearDownloadWorksSelection() {
  state.downloadAllSelection = new Set();
  state.downloadAllSelectAll = false;
  renderDownloadAllWorks();
}

export function setDownloadWorksFilter(patch) {
  state.downloadAllFilter = {...(state.downloadAllFilter || {}), ...patch};
}

// A filter change re-reads from the first page with the cursor dropped, and drops
// the selection with it: the rows a user selected may not be in the new result at
// all, and a selection that silently keeps them is worse than one that clears.
export async function applyDownloadWorksFilter(patch) {
  setDownloadWorksFilter(patch);
  clearDownloadWorksSelection();
  renderDownloadAllWorks();
  await loadDownloadAllWorks();
}

export function scheduleDownloadWorksSearch(value) {
  const search = String(value || '');
  if (state.downloadAllSearchTimer) clearTimeout(state.downloadAllSearchTimer);
  state.downloadAllSearchTimer = setTimeout(() => {
    state.downloadAllSearchTimer = null;
    setDownloadWorksFilter({search});
    // The typed value is already in state; only the read is deferred.
    loadDownloadAllWorks().catch(() => {});
  }, DOWNLOAD_WORKS_SEARCH_DEBOUNCE_MS);
}

function downloadWorksSelectionText() {
  const view = state.downloadAllWorks || {};
  const total = Number(view.total || 0);
  if (state.downloadAllSelectAll) {
    return total ? `已全选当前筛选的 ${total} 篇作品` : '已全选当前筛选的作品';
  }
  const count = selectedDownloadWorks().size;
  if (!count) return '未选择作品';
  return total ? `已选 ${count} 篇（当前筛选共 ${total} 篇）` : `已选 ${count} 篇`;
}

export function findFolderMatchingDay(node, day, title = '') {
  if (!node || typeof node !== 'object') return null;
  const dayTrim = typeof day === 'string' ? day.trim() : '';
  const dayCompact = dayTrim ? dayTrim.replace(/-/g, '') : '';
  const titleTrim = typeof title === 'string' ? title.trim().toLowerCase() : '';
  if (!dayTrim && !titleTrim) return null;

  const matches = [];
  function walk(curr) {
    if (!curr || typeof curr !== 'object') return;
    const path = String(curr.path || '');
    const name = String(curr.name || '');
    const nameLower = name.toLowerCase();
    const pathLower = path.toLowerCase();
    if (path) {
      if (dayTrim) {
        if (name === dayTrim || path === dayTrim) {
          matches.push({path, priority: 1});
        } else if (name.startsWith(dayTrim)) {
          matches.push({path, priority: 2});
        } else if (path.split('/').some(p => p === dayTrim || p.startsWith(dayTrim))) {
          matches.push({path, priority: 3});
        } else if (dayCompact && (name === dayCompact || name.startsWith(dayCompact))) {
          matches.push({path, priority: 4});
        } else if (name.includes(dayTrim) || path.includes(dayTrim)) {
          matches.push({path, priority: 5});
        } else if (dayCompact && (name.includes(dayCompact) || path.includes(dayCompact))) {
          matches.push({path, priority: 6});
        }
      }
      if (titleTrim && titleTrim.length >= 2) {
        if (nameLower === titleTrim || pathLower === titleTrim) {
          matches.push({path, priority: 7});
        } else if (nameLower.startsWith(titleTrim)) {
          matches.push({path, priority: 8});
        } else if (nameLower.includes(titleTrim) || pathLower.includes(titleTrim)) {
          matches.push({path, priority: 9});
        }
      }
    }
    if (Array.isArray(curr.children)) {
      for (const child of curr.children) walk(child);
    }
  }
  walk(node);
  if (!matches.length) return null;
  matches.sort((a, b) => a.priority - b.priority || a.path.length - b.path.length);
  return matches[0].path;
}

export async function jumpToDownloadWorkFolder(artistId, day, title = '') {
  const aid = Number(artistId);
  if (!Number.isFinite(aid) || aid <= 0) {
    toast('作品未关联画师', 'warn');
    return;
  }
  if (!day || !String(day).trim()) {
    toast('未找到对应文件夹', 'info');
    return;
  }
  if (!state.artists.length) await loadArtists();
  const artist = state.artists.find(item => Number(item.id) === aid);
  if (!artist) {
    toast('目标画师不存在', 'error');
    return;
  }

  let folders = (state.currentArtist && Number(state.currentArtist.id) === aid && state.folders)
    ? state.folders
    : null;
  if (!folders) {
    try {
      folders = await API.get(`/api/folders?artist_id=${aid}`);
    } catch {
      folders = null;
    }
  }

  const folderPath = findFolderMatchingDay(folders, day, title);
  if (!folderPath) {
    toast('未找到对应文件夹', 'info');
    return;
  }

  state.returnToView = 'downloads';
  applyMode('browse');
  await selectArtist(aid, {loadItems: false});
  selectFolder(folderPath);
}

export async function deliverPostToNetdisk(postId, options = {}) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return null;
  if (!(await ensureNetdiskDispatchReady())) return null;
  try {
    const payload = { post_id: id, ...options };
    const res = await API.postJson('/api/netdisk/jobs', payload);
    if (res.auto_start) {
      toast('已投递到 JDownloader，任务将自动开始', 'info');
    } else {
      toast('已投递到 JDownloader 收集器，请在网盘面板或链接收集器中确认开始', 'info');
    }
    return res;
  } catch (err) {
    if (err.status === 409 || err.message?.includes('409') || err.message?.includes('已有活跃投递任务') || err.message?.includes('进行中')) {
      const retry = window.confirm('该作品已有投递任务正在进行中，是否重新投递？');
      if (retry) {
        return deliverPostToNetdisk(id, { ...options, force: true });
      }
      return null;
    }
    toast('投递网盘失败：' + (err.message || err), 'error');
    return null;
  }
}

function downloadWorksRowHtml(post) {
  const id = Number(post?.id);
  const title = String(post?.title || post?.post_id || '');
  const stateLabel = downloadPostStateLabel(post?.state);
  const artist = String(post?.artist_name || '');
  const day = String(post?.day || '') || '无日期';
  const missing = Number(post?.missing_assets || 0);
  const covered = Boolean(post?.date_skipped || post?.state === 'confirmed');
  const isMissing = !covered && (missing > 0 || post?.state === 'pending' || post?.state === 'partial');
  const isComplete = covered || post?.state === 'verified';
  const badgeLabel = post?.date_skipped ? '已有' : isMissing
    ? (missing > 0 ? `缺 ${missing} 项` : (post?.state === 'pending' ? '未下载' : stateLabel))
    : (isComplete ? '已齐' : stateLabel);
  const badgeClass = isMissing ? 'is-missing' : (isComplete ? 'is-complete' : '');
  const counts = downloadPostCounts(post);
  const selected = state.downloadAllSelectAll || selectedDownloadWorks().has(id);
  const current = String(post?.state || '');
  const target = post?.source_url ? ` href="${escHtml(post.source_url)}" target="_blank" rel="noopener noreferrer"` : '';
  const dayText = escHtml(day);
  const dayHtml = post?.artist_id && post?.day
    ? `<button class="download-works-day-jump" type="button" data-download-works-jump-artist="${escHtml(String(post.artist_id))}" data-download-works-jump-day="${escHtml(post.day)}" data-download-works-jump-title="${escHtml(post.title || '')}" title="在画库中定位对应文件夹">${dayText}</button>`
    : dayText;

  const hasExt = Boolean(post?.has_external_links);
  const hasDirect = Boolean(post?.has_direct_files || (!post?.has_external_links && Number(post?.required_assets || 0) > 0));
  const extCount = Number(post?.external_link_count || 0);
  const extBadge = hasExt
    ? `<span class="download-works-badge is-external" title="这篇作品的外部网盘链接数量">含网盘资源${extCount > 1 ? ` ${extCount} 个链接` : ''}</span>`
    : '';
  let actionButtons = '';
  if (hasExt && !hasDirect) {
    actionButtons = `<button class="btn btn-ghost" type="button" data-download-works-netdisk="${id}" title="投递至下载器/网盘任务">投递网盘</button>`;
  } else if (hasExt && hasDirect) {
    actionButtons = `<button class="btn btn-ghost" type="button" data-download-works-fetch="${id}" title="按当前选择只下载这篇，不覆盖已有文件">下载</button>
      <button class="btn btn-ghost" type="button" data-download-works-netdisk="${id}" title="投递至下载器/网盘任务">投递网盘</button>`;
  } else {
    actionButtons = `<button class="btn btn-ghost" type="button" data-download-works-fetch="${id}" title="按当前选择只下载这篇，不覆盖已有文件">下载</button>`;
  }

  return `
      <div class="download-works-row${selected ? ' is-selected' : ''}" data-download-works-row="${id}">
        <span class="download-works-pick">
          <input type="checkbox" data-download-works-pick="${id}" aria-label="选择这篇作品" ${selected ? 'checked' : ''}>
        </span>
        <div class="download-works-main">
          <div class="download-works-title">${target ? `<a${target}>${escHtml(title)}</a>` : escHtml(title)}</div>
          <div class="download-works-meta">${escHtml(artist)} &#183; ${dayHtml} &#183; <span class="download-works-counts">${escHtml(counts)}</span></div>
        </div>
        <div class="download-works-side">
          <span class="download-works-badge ${badgeClass}" data-download-post-state="${escHtml(current)}">${escHtml(badgeLabel)}</span>
          ${extBadge}
          ${actionButtons}
        </div>
      </div>`;
}

export function renderDownloadAllWorks() {
  const list = $('#downloadWorksList');
  const countEl = $('#downloadWorksCount');
  const selectionEl = $('#downloadWorksSelection');
  const selectAll = $('#downloadWorksSelectAll');
  const view = state.downloadAllWorks;
  if (selectAll) selectAll.checked = Boolean(state.downloadAllSelectAll);
  if (selectionEl) selectionEl.textContent = downloadWorksSelectionText();
  syncDownloadWorksFilterControls();
  if (countEl) {
    const total = Number(view?.total || 0);
    countEl.textContent = total ? `共 ${total} 篇作品` : '';
  }
  if (!list) return;
  if (!view) {
    list.innerHTML = '<div class="move-empty small">打开这一页即可列出全部作品</div>';
    return;
  }
  // A missing element is skipped rather than thrown on: this render runs from the
  // request paths too, and a DOM error there would abort a download that had
  // already been accepted by the server.
  if (view.error) {
    list.innerHTML = `<div class="download-works-error">${escHtml(view.error)}</div>`;
    return;
  }
  if (view.loading && !view.posts.length) {
    list.innerHTML = '<div class="move-empty small">读取作品中</div>';
    return;
  }
  if (!view.posts.length) {
    list.innerHTML = '<div class="move-empty small">当前筛选下没有作品</div>';
    return;
  }
  list.innerHTML = view.posts.map(downloadWorksRowHtml).join('');
}

// Freeze what the 全部作品 view has selected and run it.
//
// The argument is the view's own answer to "what is selected now": an array of
// ids when works were ticked, and `null` when the selection is the filter itself
// (`downloadWorksSelected()` returns exactly that). `undefined` means nothing was
// selected at all, which is refused rather than read as "the whole library".
//
// `frozen` is either the explicit id list or the filter. The filter form is what
// makes "全选当前筛选" one request instead of a body holding every id behind it,
// and the backend's answer says when the filter held more than one request may
// freeze — that is reported rather than passed over, because a snapshot holding a
// silent prefix of the filter is a request the user cannot verify.
export async function downloadWorksSelection(frozen) {
  if (isActionBusy('downloadSelection')) return null;
  if (frozen === undefined) {
    toast('先选中要下载的作品', 'warn');
    return null;
  }
  const explicit = Array.isArray(frozen) ? frozen : null;
  if (explicit && !explicit.length) {
    toast('先选中要下载的作品', 'warn');
    return null;
  }
  // An empty filter is the whole list, so the button cannot reach it by accident:
  // without this, "nothing selected, nothing filtered" would silently mean "every
  // work in the library".
  if (!explicit && !downloadWorksFilterActive()) {
    toast('先选中要下载的作品，或先按画师、日期、状态筛选', 'warn');
    return null;
  }
  setActionBusy('downloadSelection', '', true);
  const requestId = newDownloadRequestId();
  const works = explicit
    ? {post_ids: downloadIdList(explicit), file_ids: []}
    : {post_ids: [], file_ids: [], filter: downloadWorksFilter(), subscription_id: downloadWorksSubscriptionId()};
  try {
    const preview = await API.postJson('/api/pawchive/selections/preview', {
      request_id: '',
      ...works,
    });
    const direct = Number(preview?.resources || 0);
    const externalPosts = Array.isArray(preview?.external_posts) ? preview.external_posts : [];

    if (!direct && !externalPosts.length) {
      toast('所选作品没有可下载的文件或外链', 'warn');
      return null;
    }

    // Case 1: Pure external selection (no direct files)
    if (!direct && externalPosts.length > 0) {
      if (explicit && explicit.length === 1) {
        return await deliverPostToNetdisk(explicit[0]);
      }
      const ok = window.confirm(`已选择 ${externalPosts.length} 篇作品（均无直连文件），将投递至网盘下载器。是否继续？`);
      if (!ok) return null;
      if (!(await ensureNetdiskDispatchReady())) return null;
      const job = await API.postJson('/api/netdisk/jobs', { post_ids: externalPosts });
      const submitted = Number(job?.submitted_count ?? (job?.job_ids?.length || externalPosts.length));
      toast(`已投递网盘：${submitted} 篇作品`, 'info');
      clearDownloadWorksSelection();
      await loadDownloadAllWorks();
      return job;
    }

    // Case 2: Mixed selection (has direct files AND external posts)
    if (direct > 0 && externalPosts.length > 0) {
      const total = explicit ? explicit.length : Number(preview?.posts || 0);
      const directPosts = preview?.plans ? preview.plans.filter(p => p.resources && p.resources.length > 0).length : (total - externalPosts.length);
      const confirmMsg = `已选择 ${total ? total + ' 篇' : ''}作品：${directPosts > 0 ? directPosts + ' 篇' : ''}将下载直连文件（共 ${direct} 个），${externalPosts.length} 篇将投递至网盘下载器。是否继续？`;
      const ok = window.confirm(confirmMsg);
      if (!ok) return null;
      if (!(await ensureNetdiskDispatchReady())) return null;

      let attempt = null;
      try {
        const selection = await API.postJson('/api/pawchive/selections', {request_id: requestId, ...works});
        attempt = await API.postJson('/api/pawchive/attempts', {
          selection_id: selection.selection_id,
          request_id: requestId,
        });
      } catch (e) {
        toast('发起直连下载失败：' + (e.message || e), 'error');
      }

      let job = null;
      try {
        job = await API.postJson('/api/netdisk/jobs', { post_ids: externalPosts });
      } catch (e) {
        toast('投递网盘失败：' + (e.message || e), 'error');
      }

      const parts = [];
      if (attempt) parts.push(`直连下载 ${Number(attempt.posts || 0)} 篇（${Number(attempt.resources || 0)} 个文件）`);
      if (job) parts.push(`网盘投递 ${Number(job.submitted_count ?? (job.job_ids?.length || externalPosts.length))} 篇`);
      if (parts.length) {
        toast(`已发起：${parts.join('，')}`, 'info');
      }
      clearDownloadWorksSelection();
      await loadDownloadAllWorks();
      return { attempt, job };
    }

    // Case 3: Pure direct files
    const selection = await API.postJson('/api/pawchive/selections', {request_id: requestId, ...works});
    const attempt = await API.postJson('/api/pawchive/attempts', {
      selection_id: selection.selection_id,
      request_id: requestId,
    });
    logUiAction('download_selection_start', {
      posts: Number(attempt.posts || 0),
      resources: Number(attempt.resources || 0),
      filtered: !explicit,
    });
    if (selection.truncated) {
      toast(`当前筛选的作品超过一次能冻结的数量，本次只创建了前 ${Number(selection.matched || 0)} 篇的任务，其余请再点一次`, 'warn');
    } else {
      toast(`已发起下载：${Number(attempt.posts || 0)} 篇，${Number(attempt.resources || 0)} 个文件`, 'info');
    }
    clearDownloadWorksSelection();
    await loadDownloadAllWorks();
    return attempt;
  } catch (error) {
    toast('发起下载失败：' + (error.message || error), 'error');
    return null;
  } finally {
    setActionBusy('downloadSelection', '', false);
    // The request has already been accepted by this point, so a redraw failure
    // must not turn it into a thrown error the caller reads as "nothing started".
    try {
      renderDownloadAllWorks();
    } catch (e) {}
  }
}

// The row's own 下载 button uses the same two-step path as the batch: preview
// first, so a work whose resources are all external links is answered here rather
// than accepted and then fetching nothing.
export async function downloadWorksPost(postId) {
  const id = Number(postId);
  if (!Number.isFinite(id)) return null;
  return downloadWorksSelection([id]);
}
