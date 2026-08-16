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
  },
  artistLinksUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/links`; },
  artistLinksReindexUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/links/reindex`; },
  artistProfileLinksUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/profile-links`; },
  artistProfileLinkUrl(artistId, linkId) { return `/api/artists/${encodeURIComponent(artistId)}/profile-links/${encodeURIComponent(linkId)}`; }
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
  artistLinks: null,
  artistLinksLoading: false,
  artistLinksCategory: 'all',
  artistLinksProvider: 'all',
  artistLinksAvailability: 'all',
  artistLinksQuery: '',
  artistProfileLinks: null,
  artistProfileLinksLoading: false,
  stats: null,
  mode: 'browse',
  view: 'grid',
  maintenanceView: 'overview',
  activeRole: null,
  activeFolder: null,
  search: '',
  searchScope: 'auto',
  searchTarget: 'all',
  itemSort: 'date_desc',
  itemDateFrom: '',
  itemDateTo: '',
  searchOptionsOpen: false,
  duplicatesOnly: false,
  scanRunning: false,
  lastScanState: null,
  lastSeenScanRun: null,
  itemSortExplicit: false,
  selectedIds: new Set(),
  selectionMarquee: null,
  selectionModifierDown: false,
  suppressNextGridClick: false,
  allItems: [],
  itemsOffset: 0,
  itemsCursor: null,
  hasMoreItems: false,
  loadingItems: false,
  loadingMoreItems: false,
  itemLoadSeq: 0,
  artistLoadSeq: 0,
  urlRestoreSeq: 0,
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
  characterSuggestionSelectedNames: new Set(),
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
  recycleBin: null,
  recycleLoadSeq: 0,
  archiveSettings: null,
  archivePlans: [],
  archivePreview: null,
  archiveWorkbenchLoading: false,
  artistFolderMoveRoots: [],
  artistFolderMovePreview: null,
  artistFolderMoveLoading: false,
  artistFolderMoveError: '',
  artistFolderMoveDirectoryPath: '',
  artistFolderMoveDirectoryEntries: [],
  artistFolderMoveDirectoryLoading: false,
  characterLibrary: null,
  characterLibraryLoading: false,
  characterLibrarySelectedCharacterId: null,
  characterLibrarySearchQuery: '',
  characterImportJob: null,
  characterImportJobTimer: null,
  characterImportFinishedJobId: null,
  hashStatus: null,
  healthSummary: null,
  operationLog: null,
  duplicateFolders: [],
  filterDrawerOpen: false,
  mobileHeaderToolsOpen: false,
  mobileColumns: 2,
  sidebarWidth: 260,
  sidebarTagRatio: 46,
  sidebarCollapsed: {filters: false, tags: false, folders: false, duplicates: true},
};

const BROWSE_KINDS = {
  untagged: '__untagged__',
  archives: '__archives__',
  videos: '__videos__',
  sources: '__sources__',
  favorites: '__favorites__',
};
const BROWSE_SORTS = ['date_desc', 'date_asc', 'name', 'size', 'scanned_desc'];
const BROWSE_VIEWS = ['grid', 'compact', 'list'];
const ARTIST_ROUTE_RESERVED_SEGMENTS = new Set(['.', '..', 'api', 'static', 'ws', 'favicon.ico']);

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
const SIDEBAR_TAG_RATIO_STORAGE_KEY = 'gallery.sidebarTagRatio';
const SIDEBAR_COLLAPSED_STORAGE_KEY = 'gallery.sidebarCollapsed';
const SIDEBAR_TAG_RATIO_DEFAULT = 46;
const SIDEBAR_TAG_RATIO_MIN = 20;
const SIDEBAR_TAG_RATIO_MAX = 80;
const SIDEBAR_TAG_RATIO_STEP = 5;
const MOBILE_COLUMNS_STORAGE_KEY = 'gallery.mobileColumns';
const MOBILE_COLUMNS_DEFAULT = 2;
const ITEM_SORT_STORAGE_KEY = 'gallery.itemSort';
const ITEM_DATE_FROM_STORAGE_KEY = 'gallery.itemDateFrom';
const ITEM_DATE_TO_STORAGE_KEY = 'gallery.itemDateTo';
const TAG_SORT_STORAGE_KEY = 'gallery.tagSort';

function getSavedItemSort() {
  try {
    const saved = localStorage.getItem(ITEM_SORT_STORAGE_KEY);
    if (saved && BROWSE_SORTS.includes(saved)) return saved;
  } catch (e) {}
  return null;
}

function saveItemSort(sort) {
  try {
    if (sort && BROWSE_SORTS.includes(sort) && sort !== 'date_desc') {
      localStorage.setItem(ITEM_SORT_STORAGE_KEY, String(sort));
    } else {
      localStorage.removeItem(ITEM_SORT_STORAGE_KEY);
    }
  } catch (e) {}
}

function getSavedItemDates() {
  try {
    const from = localStorage.getItem(ITEM_DATE_FROM_STORAGE_KEY) || '';
    const to = localStorage.getItem(ITEM_DATE_TO_STORAGE_KEY) || '';
    const dateFrom = validBrowseDate(from);
    const dateTo = validBrowseDate(to);
    if (!dateFrom || !dateTo || dateFrom <= dateTo) {
      return {from: dateFrom || '', to: dateTo || ''};
    }
  } catch (e) {}
  return {from: '', to: ''};
}

function saveItemDates(from, to) {
  try {
    if (from) localStorage.setItem(ITEM_DATE_FROM_STORAGE_KEY, String(from));
    else localStorage.removeItem(ITEM_DATE_FROM_STORAGE_KEY);
    if (to) localStorage.setItem(ITEM_DATE_TO_STORAGE_KEY, String(to));
    else localStorage.removeItem(ITEM_DATE_TO_STORAGE_KEY);
  } catch (e) {}
}

function getSavedTagSort() {
  try {
    const saved = localStorage.getItem(TAG_SORT_STORAGE_KEY);
    if (['default', 'name', 'count'].includes(saved)) return saved;
  } catch (e) {}
  return 'default';
}

function saveTagSort(sort) {
  try {
    if (sort && sort !== 'default') {
      localStorage.setItem(TAG_SORT_STORAGE_KEY, String(sort));
    } else {
      localStorage.removeItem(TAG_SORT_STORAGE_KEY);
    }
  } catch (e) {}
}
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

function isTerminalScanState(s) {
  return s && s.status === 'idle' && ['complete', 'stopped', 'interrupted'].includes(s.phase);
}

function scanRunKeyOf(s) {
  if (!s || typeof s !== 'object') return null;
  // The backend publishes one immutable scan_id per run; started_at/updated_at
  // are fallbacks for legacy or fallback scan-state payloads.
  if (s.scan_id) return `scan_id:${s.scan_id}`;
  if (s.started_at != null) return `started_at:${s.started_at}`;
  if (s.updated_at != null) return `updated_at:${s.updated_at}`;
  return null;
}

function shouldRefreshScanRun(s, lastSeenRun) {
  const key = scanRunKeyOf(s);
  if (key == null) return {refresh: false, key: null};
  if (isTerminalScanState(s) && key !== lastSeenRun) return {refresh: true, key};
  return {refresh: false, key};
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

function validBrowseDate(value) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value || '')) return '';
  const date = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(date.getTime()) && date.toISOString().slice(0, 10) === value ? value : '';
}

function folderTreeHasPath(node, path) {
  if (!node || typeof node !== 'object' || Array.isArray(node)) return false;
  if (node.path === path) return true;
  return asArray(node.children).some(child => folderTreeHasPath(child, path));
}

function artistRouteName(artist) {
  const name = String(artist?.name || '').trim();
  return name || `artist-${artist?.id || 'unknown'}`;
}

function artistRouteKey(segment) {
  try {
    return encodeURIComponent(decodeURIComponent(String(segment || ''))).toLowerCase();
  } catch (_) {
    return '';
  }
}

function artistRouteEntries(artists = state.artists) {
  const rows = asArray(artists).filter(artist => artist && artist.id != null);
  const nameCounts = new Map();
  const bases = new Map();
  rows.forEach(artist => {
    const id = String(artist.id);
    const nameKey = artistRouteName(artist).toLowerCase();
    nameCounts.set(nameKey, (nameCounts.get(nameKey) || 0) + 1);
    bases.set(id, encodeURIComponent(artistRouteName(artist)));
  });

  const suffixes = new Map();
  rows.forEach(artist => {
    const id = String(artist.id);
    const nameKey = artistRouteName(artist).toLowerCase();
    if ((nameCounts.get(nameKey) || 0) > 1 || ARTIST_ROUTE_RESERVED_SEGMENTS.has(artistRouteKey(bases.get(id)))) {
      suffixes.set(id, `--${id}`);
    }
  });

  let entries = [];
  for (let attempt = 0; attempt <= rows.length; attempt += 1) {
    const bySegment = new Map();
    entries = rows.map(artist => {
      const id = String(artist.id);
      const segment = `${bases.get(id)}${suffixes.get(id) || ''}`;
      const entry = {artist, id, segment};
      const key = artistRouteKey(segment);
      if (!bySegment.has(key)) bySegment.set(key, []);
      bySegment.get(key).push(entry);
      return entry;
    });
    const collisions = [...bySegment.values()].filter(group => group.length > 1);
    if (!collisions.length) break;
    collisions.flat().forEach(entry => {
      suffixes.set(entry.id, `${suffixes.get(entry.id) || ''}--${entry.id}`);
    });
  }
  return entries;
}

function artistRoutePath(artist) {
  const id = String(artist?.id || '');
  const entry = artistRouteEntries().find(candidate => candidate.id === id);
  return entry ? `/${entry.segment}` : '/';
}

function artistFromBrowsePath() {
  const raw = location.pathname.replace(/^\/+|\/+$/g, '');
  if (!raw || raw.includes('/')) return null;
  const key = artistRouteKey(raw);
  if (!key) return null;
  return artistRouteEntries().find(entry => artistRouteKey(entry.segment) === key)?.artist || null;
}

function browseUrlParams() {
  const params = new URLSearchParams();
  const activeRole = String(state.activeRole || '');
  if (activeRole && !activeRole.startsWith('__')) {
    params.set('tag', activeRole);
  } else {
    const kind = Object.keys(BROWSE_KINDS).find(key => BROWSE_KINDS[key] === activeRole);
    if (kind) params.set('kind', kind);
  }
  if (state.currentArtist && state.activeFolder) params.set('folder', state.activeFolder);
  if (state.search) params.set('q', state.search);
  if (state.searchScope !== 'auto') params.set('scope', state.searchScope);
  if (state.searchTarget !== 'all') params.set('target', state.searchTarget);
  if (state.itemSort !== 'date_desc' || state.itemSortExplicit) params.set('sort', state.itemSort);
  if (state.itemDateFrom) params.set('from', state.itemDateFrom);
  if (state.itemDateTo) params.set('to', state.itemDateTo);
  if (state.view !== 'grid') params.set('view', state.view);
  if (state.duplicatesOnly && isDuplicateFilesScopeActive()) params.set('duplicates', '1');
  return params;
}

function syncBrowseUrl(method = 'replace') {
  const query = browseUrlParams().toString();
  const path = state.currentArtist ? artistRoutePath(state.currentArtist) : '/';
  const url = path + (query ? `?${query}` : '') + location.hash;
  if (method === 'push') {
    if (url !== location.pathname + location.search + location.hash) history.pushState(null, '', url);
  } else {
    history.replaceState(null, '', url);
  }
}

async function restoreBrowseUrl() {
  const seq = nextRequestSeq('urlRestoreSeq');
  const params = new URLSearchParams(location.search);
  const legacyArtist = asArray(state.artists).find(row => String(row.id) === (params.get('artist') || '')) || null;
  const artist = artistFromBrowsePath() || legacyArtist;
  const dateFrom = validBrowseDate(params.get('from'));
  const dateTo = validBrowseDate(params.get('to'));
  const validRange = !dateFrom || !dateTo || dateFrom <= dateTo;

  state.search = params.get('q') || '';
  state.searchScope = validSearchScope(params.get('scope'));
  state.searchTarget = params.get('target') === 'tags' ? 'tags' : 'all';
  const restoredSort = params.get('sort');
  const hasRestoredSort = BROWSE_SORTS.includes(restoredSort);
  const savedSort = getSavedItemSort();
  if (hasRestoredSort) {
    state.itemSort = restoredSort;
    state.itemSortExplicit = true;
  } else if (savedSort) {
    state.itemSort = savedSort;
    state.itemSortExplicit = true;
  } else {
    state.itemSort = 'date_desc';
    state.itemSortExplicit = false;
  }
  const savedDates = getSavedItemDates();
  const hasParamFrom = params.has('from');
  const hasParamTo = params.has('to');
  if (hasParamFrom || hasParamTo) {
    state.itemDateFrom = validRange ? dateFrom : '';
    state.itemDateTo = validRange ? dateTo : '';
  } else {
    state.itemDateFrom = savedDates.from || '';
    state.itemDateTo = savedDates.to || '';
  }
  state.view = BROWSE_VIEWS.includes(params.get('view')) ? params.get('view') : 'grid';
  state.activeRole = null;
  state.activeFolder = null;
  state.duplicatesOnly = false;

  if (artist) {
    await selectArtist(artist.id, {preserveBrowseState: true, loadItems: false, history: false});
  } else {
    state.currentArtist = null;
    state.stats = null;
    state.tags = [];
    state.folders = null;
    clearUI();
  }
  if (!isCurrentRequestSeq('urlRestoreSeq', seq)) return;

  const tag = params.get('tag') || '';
  const tagId = state.currentArtist && state.tags.some(row => String(row.id) === tag) ? tag : '';
  const kind = params.get('kind') || '';
  state.activeRole = tagId || BROWSE_KINDS[kind] || null;
  const folder = params.get('folder') || '';
  state.activeFolder = state.currentArtist && folderTreeHasPath(state.folders, folder) ? folder : null;
  state.duplicatesOnly = params.get('duplicates') === '1';

  $('#searchInput').value = state.search;
  syncSearchOptionsControl();
  syncItemFilterControls();
  $$('#desktopViewToggle [data-view]').forEach(btn => {
    const active = btn.dataset.view === state.view;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', String(active));
  });
  renderSidebar();
  renderFolderTree();
  renderToolbar();
  syncBrowseUrl('replace');
  if (state.currentArtist || isGlobalSearchActive()) await loadItems();
  else clearUI();
}

async function init() {
  loadSidebarWidth();
  loadSidebarTagRatio();
  loadSidebarCollapsed();
  loadMobileColumns();
  bindEvents();
  document.body.classList.toggle('mode-moves', state.mode === 'moves');
  document.body.classList.toggle('mode-edit', state.mode === 'edit');
  document.body.classList.toggle('mode-browse', state.mode !== 'moves' && state.mode !== 'edit');
  if (typeof syncFilterDrawer === 'function') syncFilterDrawer();
  connectWS();
  await loadArtists();
  await restoreBrowseUrl();
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
  if (typeof resetArtistLinks === 'function') resetArtistLinks();
  if (typeof resetArtistProfileLinks === 'function') resetArtistProfileLinks();
  state.currentArtist = id ? asArray(state.artists).find(a => a.id === parseInt(id)) : null;
  setArtistSearchLabel();
  closeArtistDropdown();
  if (!options.preserveBrowseState) {
    state.activeRole = options.tagId ? String(options.tagId) : null;
    state.activeFolder = null;
    state.duplicatesOnly = false;
    state.tagSearchResults = [];
    state.search = '';
    $('#searchInput').value = '';
  }
  if (!options.preserveBrowseState) syncSearchOptionsControl();
  state.selectedIds.clear();
  state.editContextArtistId = state.currentArtist ? state.currentArtist.id : null;
  state.editContextKey = state.currentArtist ? String(state.currentArtist.id) : '';
  state.selectedEditTagIds.clear();
  state.selectedEditTagNames.clear();
  state.editTagQuery = '';
  resetCharacterTagSuggestions();
  resetArtistSuggestions();
  const editTagSearch = $('#editTagSearch');
  if (editTagSearch) editTagSearch.value = '';
  if (!state.currentArtist) {
    clearUI();
    if (options.history !== false) syncBrowseUrl(options.history || 'push');
    return;
  }
  renderLibraryEmptyState();
  const artistId = state.currentArtist.id;
  if (typeof loadArtistLinks === 'function') loadArtistLinks(artistId, seq);
  if (typeof loadArtistProfileLinks === 'function') loadArtistProfileLinks(artistId, seq);

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
  if (options.loadItems === false) return;
  if (options.history !== false) syncBrowseUrl(options.history || 'push');
  scrollToItemsTop();
  await loadItems();
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
  state.itemsCursor = null;
  state.hasMoreItems = false;
  state.stats = null;
  state.tags = [];
  state.folders = null;
  resetCharacterTagSuggestions();
  resetArtistSuggestions();
  releaseAllImageLoads();
  releaseAllVideoPreviews();
  $('#sidebarList').innerHTML = '';
  const grid = $('#grid');
  if (grid) grid.innerHTML = '';
  $('#folderTree').innerHTML = '';
  renderLibraryEmptyState();
  renderToolbar();
}
