// Browse routing: artist path <-> selection, URL sync/restore, the artist
// picker dropdown, and the localStorage-backed browse preferences.

import { API } from './api.js';
import { state, nextRequestSeq, isCurrentRequestSeq } from './store.js';
import {
  $, $$, escHtml, artistIdNumber, compareNameParts, searchableTextMatches,
  folderTreeHasPath, validBrowseDate,
} from './utils.js';
import { toast } from './logging.js';
import {
  renderSidebar, renderFolderTree, renderToolbar, clearUI, loadItems, scrollToItemsTop,
  syncSearchOptionsControl, syncItemFilterControls, validSearchScope, isGlobalSearchActive,
  renderLibraryEmptyState, renderDuplicateFolders, isDuplicateFilesScopeActive,
} from './views/sidebar.js';
import { updateEditBar, resetCharacterTagSuggestions, renderEditTagPicker } from './views/editbar.js';
import { resetArtistLinks, resetArtistProfileLinks, loadArtistLinks, loadArtistProfileLinks } from './views/links.js';
import { loadMoveWorkbench, movePanelScrollTop, restoreMovePanelScroll } from './views/maintenance/index.js';

const BROWSE_KINDS = {
  untagged: '__untagged__',
  archives: '__archives__',
  videos: '__videos__',
  sources: '__sources__',
  favorites: '__favorites__',
};
const BROWSE_SORTS = ['date_desc', 'date_asc', 'name', 'name_desc', 'size', 'size_asc', 'scanned_desc'];
const BROWSE_VIEWS = ['grid', 'compact', 'list'];
const ARTIST_ROUTE_RESERVED_SEGMENTS = new Set(['.', '..', 'api', 'static', 'ws', 'favicon.ico']);

const ITEM_SORT_STORAGE_KEY = 'gallery.itemSort';
const ITEM_DATE_FROM_STORAGE_KEY = 'gallery.itemDateFrom';
const ITEM_DATE_TO_STORAGE_KEY = 'gallery.itemDateTo';
const TAG_SORT_STORAGE_KEY = 'gallery.tagSort';
const ARTIST_HISTORY_STORAGE_KEY = 'gallery.artist-history.v1';
const MAX_ARTIST_HISTORY = 6;
const MAX_ARTIST_DROPDOWN_RESULTS = 1000;

export { BROWSE_KINDS, BROWSE_SORTS, BROWSE_VIEWS };

function getSavedItemSort() {
  try {
    const saved = localStorage.getItem(ITEM_SORT_STORAGE_KEY);
    if (saved && BROWSE_SORTS.includes(saved)) return saved;
  } catch (e) {}
  return null;
}

export function saveItemSort(sort) {
  try {
    if (sort && BROWSE_SORTS.includes(sort) && sort !== 'date_desc') {
      localStorage.setItem(ITEM_SORT_STORAGE_KEY, String(sort));
    } else {
      localStorage.removeItem(ITEM_SORT_STORAGE_KEY);
    }
  } catch (e) {}
}

export function getSavedItemDates() {
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

export function saveItemDates(from, to) {
  try {
    if (from) localStorage.setItem(ITEM_DATE_FROM_STORAGE_KEY, String(from));
    else localStorage.removeItem(ITEM_DATE_FROM_STORAGE_KEY);
    if (to) localStorage.setItem(ITEM_DATE_TO_STORAGE_KEY, String(to));
    else localStorage.removeItem(ITEM_DATE_TO_STORAGE_KEY);
  } catch (e) {}
}

export function getSavedTagSort() {
  try {
    const saved = localStorage.getItem(TAG_SORT_STORAGE_KEY);
    if (['default', 'name', 'count'].includes(saved)) return saved;
  } catch (e) {}
  return 'default';
}

export function saveTagSort(sort) {
  try {
    if (sort && sort !== 'default') {
      localStorage.setItem(TAG_SORT_STORAGE_KEY, String(sort));
    } else {
      localStorage.removeItem(TAG_SORT_STORAGE_KEY);
    }
  } catch (e) {}
}

function saveArtistHistory(history) {
  try {
    localStorage.setItem(ARTIST_HISTORY_STORAGE_KEY, JSON.stringify(history));
  } catch (e) {}
}

export function getArtistHistory(knownArtists = null) {
  let rawText = null;
  try {
    rawText = localStorage.getItem(ARTIST_HISTORY_STORAGE_KEY);
  } catch (e) {
    return [];
  }
  if (!rawText) return [];

  let parsed;
  try {
    parsed = JSON.parse(rawText);
  } catch (e) {
    saveArtistHistory([]);
    return [];
  }
  const source = Array.isArray(parsed) ? parsed : [];
  const artists = knownArtists || (state.artistsLoaded ? state.artists : null);
  const knownIds = artists
    ? new Set(artists.map(artist => artistIdNumber(artist.id)).filter(Boolean))
    : null;
  const seen = new Set();
  const cleaned = source.map(row => {
    if (!row || typeof row !== 'object') return null;
    const id = artistIdNumber(row.id);
    const lastVisitedAt = Number(row.lastVisitedAt);
    const visitCount = Number(row.visitCount);
    if (!id || !Number.isFinite(lastVisitedAt) || lastVisitedAt <= 0
      || !Number.isSafeInteger(visitCount) || visitCount <= 0
      || (knownIds && !knownIds.has(id)) || seen.has(id)) return null;
    seen.add(id);
    return {id, lastVisitedAt, visitCount};
  }).filter(Boolean)
    .sort((a, b) => b.lastVisitedAt - a.lastVisitedAt || b.visitCount - a.visitCount || a.id - b.id)
    .slice(0, MAX_ARTIST_HISTORY);
  if (JSON.stringify(cleaned) !== JSON.stringify(source)) saveArtistHistory(cleaned);
  return cleaned;
}

export function recentArtistList() {
  const artists = new Map(state.artists
    .map(artist => [artistIdNumber(artist.id), artist])
    .filter(([id]) => id));
  return getArtistHistory()
    .map(row => artists.get(row.id))
    .filter(Boolean);
}

export function naturalArtistList(artists = state.artists) {
  return artists.slice().sort((a, b) =>
    compareNameParts(a.name, b.name) || (artistIdNumber(a.id) || 0) - (artistIdNumber(b.id) || 0)
  );
}

export function artistOptionButtonHtml(artist, className = 'artist-option', extraAttrs = '') {
  return `<button class="${className}" type="button"${extraAttrs} data-artist-id="${artist.id}" title="${escHtml(artist.path || artist.name)}">
    <span>${escHtml(artist.name)}</span>
    <strong>${artist.item_count || 0}</strong>
  </button>`;
}

// Container-level click delegation for artist choice buttons; the container
// element outlives its innerHTML so one listener covers every render.
export function bindArtistChoiceContainer(container) {
  if (!container || container.dataset.artistChoiceBound === '1') return;
  container.dataset.artistChoiceBound = '1';
  container.addEventListener('click', e => {
    const btn = e.target instanceof Element ? e.target.closest('[data-artist-id]') : null;
    if (!btn || !container.contains(btn)) return;
    selectArtist(btn.dataset.artistId);
  });
}

export function recordArtistVisit(id) {
  const artistId = artistIdNumber(id);
  if (!artistId || !state.artists.some(artist => artistIdNumber(artist.id) === artistId)) return;
  const history = getArtistHistory();
  const previous = history.find(row => row.id === artistId);
  const next = [{
    id: artistId,
    lastVisitedAt: Date.now(),
    visitCount: (previous?.visitCount || 0) + 1,
  }, ...history.filter(row => row.id !== artistId)].slice(0, MAX_ARTIST_HISTORY);
  saveArtistHistory(next);
  renderLibraryEmptyState();
}

export function artistRouteName(artist) {
  const name = String(artist?.name || '').trim();
  return name || `artist-${artist?.id || 'unknown'}`;
}

export function artistRouteKey(segment) {
  try {
    return encodeURIComponent(decodeURIComponent(String(segment || ''))).toLowerCase();
  } catch (_) {
    return '';
  }
}

export function artistRouteEntries(artists = state.artists) {
  const rows = artists.filter(artist => artist && artist.id != null);
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

export function artistRoutePath(artist) {
  const id = String(artist?.id || '');
  const entry = artistRouteEntries().find(candidate => candidate.id === id);
  return entry ? `/${entry.segment}` : '/';
}

export function artistFromBrowsePath() {
  const raw = location.pathname.replace(/^\/+|\/+$/g, '');
  if (!raw || raw.includes('/')) return null;
  const key = artistRouteKey(raw);
  if (!key) return null;
  return artistRouteEntries().find(entry => artistRouteKey(entry.segment) === key)?.artist || null;
}

export function browseUrlParams() {
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

export function syncBrowseUrl(method = 'replace') {
  const query = browseUrlParams().toString();
  const path = state.currentArtist ? artistRoutePath(state.currentArtist) : '/';
  const url = path + (query ? `?${query}` : '') + location.hash;
  if (method === 'push') {
    if (url !== location.pathname + location.search + location.hash) history.pushState(null, '', url);
  } else {
    history.replaceState(null, '', url);
  }
}

export async function restoreBrowseUrl() {
  const seq = nextRequestSeq('urlRestoreSeq');
  const params = new URLSearchParams(location.search);
  const legacyArtist = state.artists.find(row => String(row.id) === (params.get('artist') || '')) || null;
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
  state.browseUrlRestored = true;
  if (state.currentArtist || isGlobalSearchActive()) await loadItems();
  else clearUI();
}

export async function loadArtists() {
  try {
    const artists = await API.get('/api/artists');
    state.artists = Array.isArray(artists)
      ? artists.filter(row => row && typeof row === 'object')
      : [];
    // Duplicate-folder warnings are cosmetic; their failure must not wipe the
    // successfully loaded artist list.
    try {
      await loadDuplicateFolders();
    } catch (e) {
      state.duplicateFolders = [];
      renderDuplicateFolders();
    }
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
  state.artistsLoaded = true;
  setArtistSearchLabel();
  renderLibraryEmptyState();
}

async function loadDuplicateFolders() {
  const duplicates = await API.get('/api/artists/duplicates');
  state.duplicateFolders = Array.isArray(duplicates.groups)
    ? duplicates.groups.filter(row => row && typeof row === 'object')
    : [];
  renderDuplicateFolders();
}

export async function selectArtist(id, options = {}) {
  const seq = nextRequestSeq('artistLoadSeq');
  const preservedArtistChangeScrollTop = state.mode === 'moves' ? movePanelScrollTop() : null;
  resetArtistLinks();
  resetArtistProfileLinks();
  state.currentArtist = id ? state.artists.find(a => a.id === parseInt(id)) : null;
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
  const editTagSearch = $('#editTagSearch');
  if (editTagSearch) editTagSearch.value = '';
  if (!state.currentArtist) {
    clearUI();
    if (options.history !== false) syncBrowseUrl(options.history || 'push');
    return;
  }
  renderLibraryEmptyState();
  const artistId = state.currentArtist.id;
  loadArtistLinks(artistId, seq);
  loadArtistProfileLinks(artistId, seq);

  try {
    const [stats, tags, folders] = await Promise.all([
      API.get(`/api/artists/${artistId}/stats`),
      API.get(`/api/tags?artist_id=${artistId}`),
      API.get(`/api/folders?artist_id=${artistId}`),
    ]);
    if (!isCurrentRequestSeq('artistLoadSeq', seq)) return;
    state.stats = (stats && typeof stats === 'object' && !Array.isArray(stats)) ? stats : null;
    state.tags = Array.isArray(tags)
      ? tags.filter(row => row && typeof row === 'object')
      : [];
    state.editContextKey = String(artistId);
    state.folders = (folders && typeof folders === 'object' && !Array.isArray(folders)) ? folders : null;
    renderSidebar();
    renderFolderTree();
    renderEditTagPicker();
    renderToolbar();
    if (state.mode === 'moves') {
      await loadMoveWorkbench({preserveScroll: true});
      restoreMovePanelScroll(preservedArtistChangeScrollTop);
      recordArtistVisit(artistId);
      return;
    }
  } catch (e) {
    if (!isCurrentRequestSeq('artistLoadSeq', seq)) return;
    clearUI();
    toast('加载画师数据失败', 'error');
    return;
  }
  recordArtistVisit(artistId);
  if (options.loadItems === false) return;
  if (options.history !== false) syncBrowseUrl(options.history || 'push');
  scrollToItemsTop();
  await loadItems();
}

export function focusArtistPicker() {
  const input = $('#artistSearch');
  if (!input) return;
  // Open with an empty query so the full list appears even if the field still
  // shows a previous label; focus/select run after paint so the dropdown stays open.
  renderArtistDropdown('');
  requestAnimationFrame(() => {
    try {
      input.focus({preventScroll: true});
    } catch (e) {
      input.focus();
    }
    try { input.select(); } catch (e) {}
  });
}

export function artistOptionLabel(artist) {
  return `${artist.name} (${artist.item_count})`;
}

export function setArtistSearchLabel(value = null) {
  const input = $('#artistSearch');
  if (!input) return;
  input.value = value !== null ? value : (state.currentArtist ? artistOptionLabel(state.currentArtist) : '');
}

export function artistMatchesQuery(artist, query) {
  if (!artist || typeof artist !== 'object') return false;
  return searchableTextMatches(query, artist.name, artist.search_text);
}

export function renderArtistDropdown(query = '') {
  const dropdown = $('#artistDropdown');
  if (!dropdown) return;
  const trimmedQuery = query.trim();
  const results = state.artists.filter(a => artistMatchesQuery(a, trimmedQuery)).slice(0, MAX_ARTIST_DROPDOWN_RESULTS);
  $('#artistPicker').classList.add('open');
  dropdown.classList.add('open');
  const input = $('#artistSearch');
  if (input) input.setAttribute('aria-expanded', 'true');
  let optionIndex = 0;
  const optionHtml = artist => artistOptionButtonHtml(
    artist,
    'artist-option',
    ` role="option" aria-selected="false" id="artist-option-${optionIndex++}"`,
  );
  if (results.length === 0) {
    dropdown.innerHTML = '<div class="artist-empty">没有匹配的画师</div>';
    artistDropdownActiveIndex = -1;
    syncArtistDropdownActive();
    return;
  }
  if (trimmedQuery) {
    dropdown.innerHTML = results.map(optionHtml).join('');
  } else {
    const recent = recentArtistList().slice(0, MAX_ARTIST_DROPDOWN_RESULTS);
    const recentHtml = recent.length
      ? `<div class="artist-section-label">最近访问</div>${recent.map(optionHtml).join('')}`
      : '';
    dropdown.innerHTML = `${recentHtml}<div class="artist-section-label">全部画师</div>${results.map(optionHtml).join('')}`;
  }
  artistDropdownActiveIndex = -1;
  syncArtistDropdownActive();
  bindArtistChoiceContainer(dropdown);
}

export function closeArtistDropdown() {
  const dropdown = $('#artistDropdown');
  if (dropdown) dropdown.classList.remove('open');
  $('#artistPicker').classList.remove('open');
  artistDropdownActiveIndex = -1;
  syncArtistDropdownActive();
  const input = $('#artistSearch');
  if (input) input.setAttribute('aria-expanded', 'false');
  setArtistSearchLabel();
}

let artistDropdownActiveIndex = -1;

export function artistDropdownOptions() {
  const dropdown = $('#artistDropdown');
  return dropdown ? Array.from(dropdown.querySelectorAll('.artist-option')) : [];
}

export function syncArtistDropdownActive() {
  const options = artistDropdownOptions();
  options.forEach((option, index) => {
    option.classList.toggle('active', index === artistDropdownActiveIndex);
    option.setAttribute('aria-selected', index === artistDropdownActiveIndex ? 'true' : 'false');
  });
  const input = $('#artistSearch');
  if (!input) return;
  const active = artistDropdownActiveIndex >= 0 ? options[artistDropdownActiveIndex] : null;
  if (active && active.id) {
    input.setAttribute('aria-activedescendant', active.id);
  } else if (typeof input.removeAttribute === 'function') {
    input.removeAttribute('aria-activedescendant');
  }
}

export function moveArtistDropdownActive(step) {
  const dropdown = $('#artistDropdown');
  if (!dropdown || !dropdown.classList.contains('open')) {
    const input = $('#artistSearch');
    renderArtistDropdown(input ? input.value : '');
  }
  const options = artistDropdownOptions();
  if (!options.length) return;
  const count = options.length;
  artistDropdownActiveIndex = artistDropdownActiveIndex < 0
    ? (step > 0 ? 0 : count - 1)
    : (artistDropdownActiveIndex + step + count) % count;
  options[artistDropdownActiveIndex].scrollIntoView({block: 'nearest'});
  syncArtistDropdownActive();
}

export function selectFirstArtistResult() {
  const options = artistDropdownOptions();
  if (!options.length) return;
  const option = options[artistDropdownActiveIndex >= 0 ? artistDropdownActiveIndex : 0];
  if (option) selectArtist(option.dataset.artistId);
}
