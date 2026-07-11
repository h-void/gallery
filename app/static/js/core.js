const API_TIMEOUT_MS = 15000;

function requestOptionsWithTimeout(options = {}) {
  if (options.signal || typeof AbortSignal === 'undefined' || !AbortSignal.timeout) return options;
  return {...options, signal: AbortSignal.timeout(API_TIMEOUT_MS)};
}

function fetchWithTimeout(path, options = {}) {
  return fetch(path, requestOptionsWithTimeout(options));
}

function asArray(value) {
  // Bare array (historical FastAPI) or common wrappers from Rust-primary JSON.
  if (Array.isArray(value)) {
    return value.filter(row => row && typeof row === 'object');
  }
  if (value && typeof value === 'object') {
    for (const key of ['artists', 'tags', 'items', 'candidates', 'history', 'references', 'groups']) {
      if (Array.isArray(value[key])) {
        return value[key].filter(row => row && typeof row === 'object');
      }
    }
  }
  return [];
}

const API = {
  async parseResponse(r) {
    let body = {};
    try {
      body = await r.json();
    } catch (e) {
      body = {};
    }
    if (!r.ok) {
      const detail = body.detail || body.message || body.error;
      const message = (detail && typeof detail === 'object')
        ? (detail.error || detail.message || r.statusText || `HTTP ${r.status}`)
        : (detail || r.statusText || `HTTP ${r.status}`);
      const error = new Error(message);
      error.status = r.status;
      error.detail = detail;
      error.body = body;
      throw error;
    }
    return body;
  },
  async get(path, options = {}) {
    const r = await fetchWithTimeout(path, options);
    return this.parseResponse(r);
  },
  async post(path) {
    const r = await fetchWithTimeout(path, {method:'POST', keepalive:true});
    return this.parseResponse(r);
  },
  async put(path) {
    const r = await fetchWithTimeout(path, {method:'PUT'});
    return this.parseResponse(r);
  },
  async putJson(path, data) {
    const r = await fetchWithTimeout(path, {
      method:'PUT',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(data || {})
    });
    return this.parseResponse(r);
  },
  async postJson(path, data) {
    const r = await fetchWithTimeout(path, {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(data || {}),
      keepalive:true
    });
    return this.parseResponse(r);
  },
  async del(path) {
    const r = await fetchWithTimeout(path, {method:'DELETE'});
    return this.parseResponse(r);
  },
  fileUrl(filePath, version) {
    const params = new URLSearchParams({path: filePath});
    if (version) params.set('v', version);
    return '/api/file?' + params.toString();
  },
  previewUrl(filePath, version, maxEdge) {
    const params = new URLSearchParams({path: filePath});
    if (version) params.set('v', version);
    if (maxEdge) params.set('max', String(maxEdge));
    return '/api/file/preview?' + params.toString();
  },
  streamUrl(filePath) {
    return '/api/file/stream?path=' + encodeURIComponent(filePath);
  },
  videoFrameUrl(filePath, version) {
    const params = new URLSearchParams({path: filePath, t: '0.1'});
    if (version) params.set('v', version);
    return '/api/file/video-frame?' + params.toString();
  },
  videoCompatibleUrl(filePath) {
    return '/api/file/video-compatible?path=' + encodeURIComponent(filePath);
  },
  videoHlsUrl(filePath) {
    return '/api/file/video-hls?path=' + encodeURIComponent(filePath);
  },
  videoTranscodeUrl(filePath) {
    return '/api/file/video-transcode?path=' + encodeURIComponent(filePath);
  },
  videoTranscodeStatusUrl(filePath) {
    return '/api/file/video-transcode-status?path=' + encodeURIComponent(filePath);
  },
  videoTranscodedUrl(filePath) {
    return '/api/file/video-transcoded?path=' + encodeURIComponent(filePath);
  },
  textUrl(filePath) {
    return '/api/file/text?path=' + encodeURIComponent(filePath);
  },
  deleteFileUrl(filePath) {
    return '/api/file/delete?path=' + encodeURIComponent(filePath);
  }
};

const LIGHTBOX_ZOOM_MIN = 0.5;
const LIGHTBOX_ZOOM_MAX = 4;
const LIGHTBOX_ZOOM_STEP = 0.15;
const LIGHTBOX_DOUBLE_TAP_ZOOM = 2;
const LIGHTBOX_DOUBLE_TAP_DELAY_MS = 320;
const LIGHTBOX_DOUBLE_TAP_DISTANCE_PX = 36;
const LIGHTBOX_WHEEL_NAV_DELAY = 180;
const UI_FIELD_SEPARATOR = ' \u00b7 ';
const BUTTON_ICONS = {
  close: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12"></path><path d="M18 6 6 18"></path></svg>',
  trash: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"></path><path d="M8 6V4h8v2"></path><path d="M19 6l-1 14H6L5 6"></path><path d="M10 11v5"></path><path d="M14 11v5"></path></svg>',
  download: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v10"></path><path d="M8 9l4 4 4-4"></path><path d="M5 21h14"></path></svg>',
  refresh: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 1-15.5 6.2"></path><path d="M3 12A9 9 0 0 1 18.5 5.8"></path><path d="M18 2v4h4"></path><path d="M6 22v-4H2"></path></svg>',
  chevronDown: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"></path></svg>',
};

let state = {
  artists: [],
  currentArtist: null,
  stats: null,
  mode: 'browse',
  view: 'grid',
  maintenanceView: 'overview',
  activeRole: null,
  activeFolder: null,
  search: '',
  searchScope: 'auto',
  searchTarget: 'all',
  searchOptionsOpen: false,
  duplicatesOnly: false,
  scanRunning: false,
  lastScanState: null,
  selectedIds: new Set(),
  selectionMarquee: null,
  selectionModifierDown: false,
  suppressNextGridClick: false,
  allItems: [],
  itemsOffset: 0,
  hasMoreItems: false,
  loadingItems: false,
  loadingMoreItems: false,
  itemLoadSeq: 0,
  artistLoadSeq: 0,
  scanRefreshSeq: 0,
  modeSwitchSeq: 0,
  modeSwitchAnchor: null,
  selectionRestoreSeq: 0,
  maintenanceLoadSeq: 0,
  actionBusy: new Set(),
  lightboxIndex: -1,
  lastFocusedBeforeLightbox: null,
  lightboxZoom: 1,
  lightboxPanX: 0,
  lightboxPanY: 0,
  lightboxPanActive: false,
  lightboxPanPointerX: 0,
  lightboxPanPointerY: 0,
  lightboxPanStartX: 0,
  lightboxPanStartY: 0,
  lightboxPointers: new Map(),
  lightboxPinchActive: false,
  lightboxPinchStartDistance: 0,
  lightboxPinchStartZoom: 1,
  lightboxTapPointerId: null,
  lightboxTapStartX: 0,
  lightboxTapStartY: 0,
  lightboxTapMoved: false,
  lightboxLastTapAt: 0,
  lightboxLastTapX: 0,
  lightboxLastTapY: 0,
  lightboxWheelLastAt: 0,
  lightboxLoadToken: 0,
  tags: [],
  tagSearchResults: [],
  editContextArtistId: null,
  editTagContextLoading: false,
  editGlobalTagResults: [],
  editGlobalTagSearchLoading: false,
  characterTagSuggestions: [],
  characterSuggestionLoading: false,
  characterSuggestionStatus: 'idle',
  characterSuggestionMessage: '',
  characterSuggestionSampleTotal: 0,
  characterSuggestionSampleLimit: 0,
  characterSuggestionCache: new Map(),
  characterSuggestionSeq: 0,
  characterSuggestionPageKey: '',
  characterSuggestionScheduleSeq: 0,
  characterSuggestionScheduleTimer: null,
  characterSuggestionScheduleFrame: null,
  artistSuggestions: [],
  artistSuggestionLoading: false,
  artistSuggestionStatus: 'idle',
  artistSuggestionMessage: '',
  artistSuggestionSeq: 0,
  artistSuggestionPageKey: '',
  artistSuggestionScheduleSeq: 0,
  artistSuggestionScheduleTimer: null,
  artistSuggestionScheduleFrame: null,
  selectedEditTagIds: new Set(),
  selectedEditTagNames: new Set(),
  editTagQuery: '',
  folders: null,
  moveCandidates: [],
  movePendingTotal: 0,
  moveCandidateGroups: [],
  moveWaitingHashCount: 0,
  moveHistory: [],
  folderRenameAuto: null,
  characterLibrary: null,
  characterLibraryLoading: false,
  characterLibrarySelectedCharacterId: null,
  characterImportJob: null,
  characterImportJobTimer: null,
  characterImportFinishedJobId: null,
  hashStatus: null,
  healthSummary: null,
  operationLog: null,
  duplicateFolders: [],
  filterDrawerOpen: false,
  mobileHeaderToolsOpen: false,
  duplicateFoldersOpen: false,
  mobileColumns: 2,
  sidebarWidth: 260,
};

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);
const ITEM_PAGE_LIMIT = 120;
const INFINITE_SCROLL_THRESHOLD = 700;
const IMAGE_PREVIEW_MAX_EDGE = 512;
const LIGHTBOX_VIDEO_FALLBACK_DELAY_MS = 12000;
const VIDEO_TRANSCODE_POLL_INTERVAL_MS = 1000;
const VIDEO_TRANSCODE_WAIT_TIMEOUT_MS = 120000;
const SIDEBAR_WIDTH_STORAGE_KEY = 'gallery.sidebarWidthPx';
const SIDEBAR_WIDTH_DEFAULT = 260;
const SIDEBAR_WIDTH_MIN = 180;
const SIDEBAR_WIDTH_MAX = 520;
const MOBILE_COLUMNS_STORAGE_KEY = 'gallery.mobileColumns';
const MOBILE_COLUMNS_DEFAULT = 2;
const MAX_IMAGE_LOADS = 2;
const IMAGE_OBSERVER_ROOT_MARGIN = '480px';
const IMAGE_LOAD_TIMEOUT_MS = 12000;
const MAX_VIDEO_PREVIEW_LOADS = 1;
const VIDEO_PREVIEW_HOVER_DELAY_MS = 250;
const VIDEO_PREVIEW_LOAD_TIMEOUT_MS = 8000;
const MAX_ARTIST_DROPDOWN_RESULTS = 1000;
const FRONTEND_ERROR_LOG_LIMIT = 20;
const FRONTEND_ERROR_DEDUPE_MS = 30000;
const MAINTENANCE_AUTO_REFRESH_MS = 10000;
const MAINTENANCE_IDLE_REFRESH_MS = 60000;
const CHARACTER_IMPORT_POLL_MS = 1000;
const CHARACTER_SUGGESTION_UNSELECTED_LIMIT = 6;
const CHARACTER_SUGGESTION_SELECTED_LIMIT = 3;
const CHARACTER_SUGGESTION_DELAY_MS = 120;
// Artist-folder library: never show AI artist-suggestion UI (recognition stays off).
const ARTIST_SUGGESTIONS_VISIBLE = false;
const ARTIST_SUGGESTION_DELAY_MS = 120;
const mergeTagsByNameCollator = new Intl.Collator(undefined, {numeric: true, sensitivity: 'base'});
let activeVideoPreviewLoads = 0;
const pendingVideoPreviews = [];
let videoPreviewObserver = null;
let activeImageLoads = 0;
const pendingImageLoads = [];
let imageObserver = null;
let editTagContextLoadToken = 0;
let editGlobalTagSearchToken = 0;
let frontendErrorLogCount = 0;
let maintenanceAutoRefreshTimer = null;
let maintenanceAutoRefreshInFlight = false;
let activeMaintenanceController = null;
let activeMaintenanceRequest = null;
let maintenanceConsecutiveRefreshFailures = 0;
const frontendErrorLastSeen = new Map();

function nextRequestSeq(name) {
  state[name] = Number(state[name] || 0) + 1;
  return state[name];
}

function isCurrentRequestSeq(name, seq) {
  return Number(state[name] || 0) === Number(seq);
}

function actionBusyKey(name, id = '') {
  return id ? `${name}:${id}` : name;
}

function isActionBusy(name, id = '') {
  return state.actionBusy.has(actionBusyKey(name, id));
}

function setActionBusy(name, id = '', busy = true) {
  const key = actionBusyKey(name, id);
  if (busy) state.actionBusy.add(key);
  else state.actionBusy.delete(key);
}

function compareNameParts(a = '', b = '') {
  return mergeTagsByNameCollator.compare(a || '', b || '');
}

function compareCharacterNames(a = '', b = '') {
  return compareNameParts(a, b);
}

function searchableTextMatches(query, ...values) {
  const needle = String(query || '').trim().toLowerCase();
  if (!needle) return true;
  const compactNeedle = needle.replace(/\s+/g, '');
  return values.some(value => {
    const text = String(value || '').toLowerCase();
    const compactText = text.replace(/\s+/g, '');
    return text.includes(needle) || (compactNeedle && compactText.includes(compactNeedle));
  });
}

function logUiAction(event, data = {}) {
  const payload = JSON.stringify({event, data});
  try {
    if (navigator.sendBeacon) {
      const blob = new Blob([payload], {type:'application/json'});
      if (navigator.sendBeacon('/api/ui-log', blob)) return;
    }
  } catch (e) {}
  fetch('/api/ui-log', {
    method:'POST',
    headers:{'Content-Type':'application/json'},
    body: payload,
    keepalive:true,
  }).catch(() => {});
}

function collectUiLogContext(extra = {}) {
  const grid = $('#grid');
  const container = $('#gridContainer');
  return Object.assign({
    mode: state.mode,
    artist_id: state.currentArtist ? state.currentArtist.id : null,
    artist_name: state.currentArtist ? state.currentArtist.name : '',
    folder: state.activeFolder || '',
    search_scope: state.searchScope,
    search_target: state.searchTarget,
    loaded_count: state.allItems.length,
    card_count: grid ? grid.querySelectorAll('.card').length : 0,
    has_more: state.hasMoreItems,
    mobile_columns: state.mobileColumns,
    viewport: `${window.innerWidth}x${window.innerHeight}`,
    scroll_top: container ? Math.round(container.scrollTop) : Math.round(window.scrollY || 0),
    user_agent: navigator.userAgent,
  }, extra);
}

function collectSelectionLayoutLogContext(extra = {}) {
  const editBar = $('#editBar');
  const container = $('#gridContainer');
  const containerRect = container ? container.getBoundingClientRect() : null;
  const cards = [...$$('#grid .card[data-id]')];
  const firstVisible = containerRect ? cards.find(card => {
    const rect = card.getBoundingClientRect();
    return rect.bottom > containerRect.top && rect.top < containerRect.bottom;
  }) : null;
  return collectUiLogContext(Object.assign({
    edit_bar_height: editBar ? Math.round(editBar.getBoundingClientRect().height) : 0,
    grid_scroll_top: container ? Math.round(container.scrollTop) : Math.round(window.scrollY || 0),
    grid_client_height: container ? Math.round(container.clientHeight) : Math.round(window.innerHeight || 0),
    first_visible_id: firstVisible ? Number(firstVisible.dataset.id) : null,
    selected_item_ids: [...state.selectedIds],
  }, extra));
}

function frontendErrorText(value) {
  if (!value) return '';
  if (value.message) return String(value.message);
  try {
    return typeof value === 'string' ? value : JSON.stringify(value);
  } catch (e) {
    return String(value);
  }
}

function frontendErrorStack(value) {
  if (!value || !value.stack) return '';
  return String(value.stack);
}

function joinUiMeta(parts) {
  return parts.map(part => String(part || '').trim()).filter(Boolean).join(UI_FIELD_SEPARATOR);
}

function buttonIcon(name) {
  const icon = BUTTON_ICONS[name];
  return icon ? `<span class="btn-glyph" aria-hidden="true">${icon}</span>` : '';
}

function logFrontendError(event, data) {
  if (frontendErrorLogCount >= FRONTEND_ERROR_LOG_LIMIT) return;
  const key = `${event}:${data.message || data.reason || ''}:${data.source || ''}:${data.line || ''}`;
  const now = Date.now();
  const lastSeen = frontendErrorLastSeen.get(key) || 0;
  if (now - lastSeen < FRONTEND_ERROR_DEDUPE_MS) return;
  frontendErrorLastSeen.set(key, now);
  frontendErrorLogCount += 1;
  logUiAction(event, collectUiLogContext(data));
}

function installFrontendErrorLogging() {
  window.addEventListener('error', event => {
    logFrontendError('frontend_error', {
      message: frontendErrorText(event.error) || String(event.message || ''),
      source: event.filename || '',
      line: event.lineno || 0,
      column: event.colno || 0,
      stack: frontendErrorStack(event.error),
    });
  });
  window.addEventListener('unhandledrejection', event => {
    const reason = event.reason;
    logFrontendError('frontend_rejection', {
      reason: frontendErrorText(reason),
      stack: frontendErrorStack(reason),
    });
  });
}

async function init() {
  loadSidebarWidth();
  loadMobileColumns();
  bindEvents();
  document.body.classList.toggle('mode-moves', state.mode === 'moves');
  document.body.classList.toggle('mode-edit', state.mode === 'edit');
  document.body.classList.toggle('mode-browse', state.mode !== 'moves' && state.mode !== 'edit');
  if (typeof syncFilterDrawer === 'function') syncFilterDrawer();
  connectWS();
  await loadArtists();
  if (!state.currentArtist) clearUI();
  else if (typeof renderLibraryEmptyState === 'function') renderLibraryEmptyState();
}

async function loadArtists() {
  try {
    const artists = await API.get('/api/artists');
    // Python returned a bare array; Rust briefly wrapped {artists:[]}. asArray accepts both.
    state.artists = asArray(artists);
    await loadDuplicateFolders();
    if (state.currentArtist) {
      state.currentArtist = state.artists.find(a => a.id === state.currentArtist.id) || null;
    }
    if (state.artists.length === 0) {
      toast('画师列表为空', 'error');
    }
  } catch (e) {
    state.artists = [];
    state.duplicateFolders = [];
    renderDuplicateFolders();
    toast('加载画师失败', 'error');
  }
  setArtistSearchLabel();
  renderLibraryEmptyState();
}

async function loadDuplicateFolders() {
  const duplicates = await API.get('/api/artists/duplicates');
  state.duplicateFolders = asArray(duplicates.groups);
  renderDuplicateFolders();
}

async function selectArtist(id, options = {}) {
  const seq = nextRequestSeq('artistLoadSeq');
  const preservedArtistChangeScrollTop = state.mode === 'moves' ? movePanelScrollTop() : null;
  state.currentArtist = id ? asArray(state.artists).find(a => a.id === parseInt(id)) : null;
  syncSearchOptionsControl();
  setArtistSearchLabel();
  closeArtistDropdown();
  state.activeRole = options.tagId ? String(options.tagId) : null;
  state.activeFolder = null;
  state.selectedIds.clear();
  state.editContextArtistId = state.currentArtist ? state.currentArtist.id : null;
  state.editContextKey = state.currentArtist ? String(state.currentArtist.id) : '';
  state.selectedEditTagIds.clear();
  state.selectedEditTagNames.clear();
  state.editTagQuery = '';
  resetCharacterTagSuggestions();
  resetArtistSuggestions();
  state.duplicatesOnly = false;
  state.tagSearchResults = [];
  state.search = '';
  $('#searchInput').value = '';
  const editTagSearch = $('#editTagSearch');
  if (editTagSearch) editTagSearch.value = '';

  if (!state.currentArtist) {
    clearUI();
    return;
  }
  renderLibraryEmptyState();
  const artistId = state.currentArtist.id;

  try {
    const [stats, tags, folders] = await Promise.all([
      API.get(`/api/artists/${artistId}/stats`),
      API.get(`/api/tags?artist_id=${artistId}`),
      API.get(`/api/folders?artist_id=${artistId}`),
    ]);
    if (!isCurrentRequestSeq('artistLoadSeq', seq)) return;
    state.stats = (stats && typeof stats === 'object' && !Array.isArray(stats)) ? stats : null;
    state.tags = asArray(tags);
    state.editContextKey = String(artistId);
    state.folders = (folders && typeof folders === 'object' && !Array.isArray(folders)) ? folders : null;
    renderSidebar();
    renderFolderTree();
    renderEditTagPicker();
    renderToolbar();
    if (state.mode === 'moves') {
      await loadMoveWorkbench({preserveScroll: true});
      restoreMovePanelScroll(preservedArtistChangeScrollTop);
      return;
    }
  } catch (e) {
    if (!isCurrentRequestSeq('artistLoadSeq', seq)) return;
    clearUI();
    toast('加载画师数据失败', 'error');
    return;
  }
  scrollToItemsTop();
  loadItems();
}

function artistOptionLabel(artist) {
  return `${artist.name} (${artist.item_count})`;
}

function setArtistSearchLabel(value = null) {
  const input = $('#artistSearch');
  if (!input) return;
  input.value = value !== null ? value : (state.currentArtist ? artistOptionLabel(state.currentArtist) : '');
}

function artistMatchesQuery(artist, query) {
  if (!artist || typeof artist !== 'object') return false;
  return searchableTextMatches(query, artist.name, artist.search_text);
}

function renderArtistDropdown(query = '') {
  const dropdown = $('#artistDropdown');
  if (!dropdown) return;
  const results = asArray(state.artists).filter(a => artistMatchesQuery(a, query.trim())).slice(0, MAX_ARTIST_DROPDOWN_RESULTS);
  $('#artistPicker').classList.add('open');
  dropdown.classList.add('open');
  if (results.length === 0) {
    dropdown.innerHTML = '<div class="artist-empty">没有匹配的画师</div>';
    return;
  }
  dropdown.innerHTML = results.map(a => `
    <button class="artist-option" type="button" data-artist-id="${a.id}" title="${escHtml(a.path || a.name)}">
      <span>${escHtml(a.name)}</span>
      <strong>${a.item_count || 0}</strong>
    </button>
  `).join('');
  $$('#artistDropdown .artist-option').forEach(btn => {
    btn.addEventListener('click', () => selectArtist(btn.dataset.artistId));
  });
}

function closeArtistDropdown() {
  const dropdown = $('#artistDropdown');
  if (dropdown) dropdown.classList.remove('open');
  $('#artistPicker').classList.remove('open');
  setArtistSearchLabel();
}

function selectFirstArtistResult() {
  const first = $('#artistDropdown .artist-option');
  if (first) selectArtist(first.dataset.artistId);
}

function clearUI() {
  state.allItems = [];
  state.itemsOffset = 0;
  state.hasMoreItems = false;
  resetCharacterTagSuggestions();
  resetArtistSuggestions();
  releaseAllImageLoads();
  releaseAllVideoPreviews();
  $('#sidebarList').innerHTML = '';
  $('#sidebarTotal').textContent = '';
  const grid = $('#grid');
  if (grid) grid.innerHTML = '';
  $('#folderTree').innerHTML = '';
  $('#folderTotal').textContent = '';
  renderLibraryEmptyState();
  updateDuplicateFilesButton();
}
