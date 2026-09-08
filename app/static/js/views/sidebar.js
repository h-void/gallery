// Sidebar and browse-page data flow: filters (sort/date), tag list, folder
// tree, duplicate-folder warnings, display preferences, item loading with
// infinite scroll, and the empty-state renderer.

import { API } from '../api.js';
import { state, nextRequestSeq, isCurrentRequestSeq } from '../store.js';
import {
  $, $$, escHtml, mergeTagsByNameCollator,
} from '../utils.js';
import { toast, logUiAction, collectUiLogContext } from '../logging.js';
import {
  BROWSE_KINDS, syncBrowseUrl, getSavedTagSort,
} from '../router.js';
import { renderGrid, appendItemsToGrid, releaseAllImageLoads, releaseAllVideoPreviewLoads } from './grid.js';
import { updateEditBar, resetCharacterTagSuggestions, scheduleCharacterTagSuggestions } from './editbar.js';

// Three-state lifecycle of the duplicate-folder section across hydration and
// runtime updates: 'unknown' before the first completed render, 'empty' after
// a completed render observed zero groups, 'present' after a completed render
// observed one or more groups. Initial hydration must never overwrite the
// restored collapse state; only the runtime empty -> present transition
// auto-reveals the section once.
let duplicateSectionLifecycle = 'unknown';

// Display names for the accessible expand/collapse labels of the independent
// chevron toggles. Both toggle types share one section key.
export const SIDEBAR_SECTION_NAMES = {
  duplicates: '重复文件夹',
  filters: '排序与日期',
  tags: '标签',
  folders: '文件夹',
};

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
const CARD_RATIOS = ['4x3', '3x4'];
const CARD_RATIO_STORAGE_KEY = 'gallery.cardRatio';
const CARD_RATIO_DEFAULT = '4x3';

const ITEM_PAGE_LIMIT = 120;
const INFINITE_SCROLL_THRESHOLD = 700;

// An anchored reload (auto refresh, mode switch, edit apply) used to collapse
// the grid back to the first page; an anchor item beyond that page was gone
// from the DOM and the viewport jumped. Refill pages until the previously
// loaded count is reached so restoreGridScrollAnchor can find its item under
// any sort order. The anchor itself tracks stable item ids, not pixels.
const MAX_DEPTH_REFILL_PAGES = 200;

export function isMobileViewport() {
  return window.matchMedia('(max-width:768px)').matches;
}

export function renderLibraryEmptyState() {
  const panel = $('#libraryEmptyState');
  if (!panel) return;
  const noArtists = !state.currentArtist && state.artists.length === 0;
  const needsArtistPick = !state.currentArtist && state.artists.length > 0;
  const hideForGlobalSearch = isGlobalSearchActive();
  const showEmpty = (noArtists || needsArtistPick) && !hideForGlobalSearch && state.mode !== 'moves';

  panel.style.display = showEmpty ? '' : 'none';
  panel.classList.toggle('needs-artist', needsArtistPick && showEmpty);
  document.body.classList.toggle('library-needs-artist', needsArtistPick && showEmpty);
  document.body.classList.toggle('library-empty-artists', noArtists && showEmpty);
  document.body.classList.toggle('has-artist', Boolean(state.currentArtist));

  // The main empty panel already carries the artist-pick entry and guidance;
  // a second hint in the sidebar repeated the same instructions. Keep the
  // main entry, keep the hint element for recovery, and stop showing it.
  const sidebarHint = $('#sidebarEmptyHint');
  if (sidebarHint) {
    sidebarHint.hidden = true;
  }

  if (!showEmpty) return;

  const artistChoices = $('#libraryEmptyArtists');
  if (artistChoices) {
    const recent = recentArtistList();
    const choices = recent.length ? recent : naturalArtistList().slice(0, 6);
    artistChoices.hidden = choices.length === 0;
    artistChoices.innerHTML = choices.length
      ? `<div class="library-empty-artists-title">${recent.length ? '最近访问' : '可选画师'}</div>
         <div class="library-empty-artists-list">${choices.map(artist => artistOptionButtonHtml(artist, 'btn btn-ghost btn-sm artist-quick-option')).join('')}</div>`
      : '';
    bindArtistChoiceContainer(artistChoices);
  }

  const scanState = state.lastScanState || {};
  const scanButton = $('#emptyScanBtn');
  const selectButton = $('#emptySelectArtistBtn');
  const isScanning = state.scanRunning || scanState.status === 'scanning';

  if (needsArtistPick) {
    panel.classList.remove('scanning');
    if (scanButton) {
      scanButton.style.display = 'none';
      scanButton.disabled = false;
      scanButton.textContent = '扫描全库';
    }
    if (selectButton) {
      selectButton.style.display = '';
      selectButton.disabled = false;
    }
    // The page header already reads 「画廊」; drop the duplicate eyebrow here.
    $('#libraryEmptyKicker').hidden = true;
    $('#libraryEmptyTitle').textContent = '选择一位画师开始浏览';
    $('#libraryEmptyText').textContent = `已有 ${state.artists.length} 位画师。从顶部搜索并选择画师，即可查看其标签、文件夹与媒体作品。`;
    $('#libraryEmptyMeta').textContent = '支持全名、拼音与部分名搜索，也可全局查找标签或文件名。';
    return;
  }

  if (scanButton) {
    scanButton.style.display = '';
    scanButton.disabled = isScanning;
    scanButton.textContent = isScanning ? '扫描中' : '扫描全库';
  }
  if (selectButton) selectButton.style.display = 'none';
  panel.classList.toggle('scanning', isScanning);

  if (isScanning) {
    $('#libraryEmptyKicker').hidden = false;
    $('#libraryEmptyKicker').textContent = '正在扫描';
    $('#libraryEmptyTitle').textContent = '正在整理画廊';
    $('#libraryEmptyText').textContent = scanState.current_path || '正在扫描媒体目录，已发现的画师将陆续呈现。';
    $('#libraryEmptyMeta').textContent = scanState.total_estimate > 0
      ? `${scanState.scanned_count || 0} / ${scanState.total_estimate}`
      : '顶部导航栏会实时显示当前扫描进度。';
    return;
  }

  if (scanState.status === 'idle' && scanState.phase === 'complete') {
    $('#libraryEmptyKicker').hidden = false;
    $('#libraryEmptyKicker').textContent = '扫描完成';
    $('#libraryEmptyTitle').textContent = '没有发现媒体文件';
    $('#libraryEmptyText').textContent = '未在当前媒体目录中检测到受支持的图片、视频或压缩包。';
    $('#libraryEmptyMeta').textContent = '可重新发起扫描，或前往系统设置检查媒体目录授权。';
    return;
  }

  $('#libraryEmptyKicker').hidden = true;
  $('#libraryEmptyTitle').textContent = '画廊还是空的';
  $('#libraryEmptyText').textContent = '点击下方按钮扫描全库，媒体文件整理进画廊后即可在此处浏览。';
  $('#libraryEmptyMeta').textContent = '扫描将在后台自动运行，顶部会实时显示扫描进度。';
}

export function syncFilterDrawer() {
  document.body.classList.toggle('filter-drawer-open', state.filterDrawerOpen);
  const backdrop = $('#filterBackdrop');
  if (backdrop) backdrop.hidden = !state.filterDrawerOpen;
  const btn = $('#mobileFilterBtn');
  if (btn) btn.setAttribute('aria-expanded', state.filterDrawerOpen ? 'true' : 'false');
  const sidebar = $('#filterSidebar');
  if (sidebar) {
    if (state.filterDrawerOpen) {
      sidebar.removeAttribute('inert');
      sidebar.setAttribute('aria-hidden', 'false');
    } else if (isMobileViewport()) {
      sidebar.setAttribute('inert', '');
      sidebar.setAttribute('aria-hidden', 'true');
    } else {
      sidebar.removeAttribute('inert');
      sidebar.removeAttribute('aria-hidden');
    }
  }
}

export function openFilterDrawer() {
  state.filterDrawerOpen = true;
  state._filterFocusReturn = document.activeElement;
  syncFilterDrawer();
  const closeBtn = $('#filterDrawerClose');
  if (closeBtn) closeBtn.focus();
}

export function closeFilterDrawer() {
  state.filterDrawerOpen = false;
  syncFilterDrawer();
  const returnEl = state._filterFocusReturn;
  state._filterFocusReturn = null;
  if (returnEl && typeof returnEl.focus === 'function') {
    try { returnEl.focus(); } catch (e) {}
  } else {
    const btn = $('#mobileFilterBtn');
    if (btn) btn.focus();
  }
}

export function closeFilterDrawerIfMobile() {
  if (isMobileViewport()) closeFilterDrawer();
}

// Seeded on first use so the first resize that crosses the breakpoint already
// compares against the viewport the page was rendered in. Lazy (not at module
// load) because non-browser importers evaluate this module without a window.
let lastMobileViewport = null;

export function onViewportLayoutChange() {
  const mobile = isMobileViewport();
  // Leaving mobile must clear inert left by a closed drawer.
  if (!mobile && state.filterDrawerOpen) {
    state.filterDrawerOpen = false;
  }
  syncFilterDrawer();
  syncSidebarCollapse();
  if (!mobile) {
    closeMobileHeaderTools();
  }
  // Crossing the mobile breakpoint swaps the browse layout engine (justified
  // rows vs the fixed mobile CSS grid), which a resize relayout alone cannot
  // do — it only re-partitions an already-justified grid. Re-render once per
  // crossing so the grid never keeps the wrong engine.
  if (lastMobileViewport !== null && lastMobileViewport !== mobile) {
    renderGrid();
  }
  lastMobileViewport = mobile;
}

export function validSearchScope(scope) {
  return ['auto', 'artist', 'folder', 'global'].includes(scope) ? scope : 'auto';
}

export function effectiveSearchScope() {
  const scope = validSearchScope(state.searchScope);
  if (scope !== 'auto') return scope;
  if (state.activeFolder) return 'folder';
  if (state.currentArtist) return 'artist';
  return 'global';
}

function normalizeSearchScope() {
  state.searchScope = validSearchScope(state.searchScope);
  if (state.searchScope === 'artist' && !state.currentArtist) state.searchScope = 'auto';
  if (state.searchScope === 'folder' && !state.activeFolder) state.searchScope = 'auto';
}

function searchOptionsLabel() {
  const labels = {auto: '范围', artist: '画师', folder: '文件夹', global: '全局'};
  const scope = validSearchScope(state.searchScope);
  const tagsOnly = state.searchTarget === 'tags';
  if (scope === 'auto' && tagsOnly) return '仅标签';
  if (scope === 'auto') return '范围';
  return tagsOnly ? `${labels[scope]}/标签` : labels[scope];
}

// S6: the clear affordance exists only while a query is present. Typed input
// calls this directly; every programmatic search reset funnels through
// syncSearchOptionsControl, so one hook here keeps the button in sync.
export function syncClearSearch() {
  const btn = $('#clearSearchBtn');
  if (!btn) return;
  const input = $('#searchInput');
  const hasQuery = Boolean((input && input.value) || state.search);
  btn.hidden = !hasQuery;
}

export function syncSearchOptionsControl() {
  normalizeSearchScope();
  syncClearSearch();
  const input = $('#searchInput');
  if (input) input.placeholder = state.searchTarget === 'tags' ? '搜索标签' : '搜索标签或文件名';

  const btn = $('#searchOptionsBtn');
  if (btn) {
    btn.textContent = searchOptionsLabel();
    btn.classList.toggle('active', state.searchScope !== 'auto' || state.searchTarget === 'tags');
    btn.setAttribute('aria-expanded', state.searchOptionsOpen ? 'true' : 'false');
  }

  const menu = $('#searchOptionsMenu');
  if (menu) menu.hidden = !state.searchOptionsOpen;

  $$('#searchOptionsMenu [data-search-scope]').forEach(scopeBtn => {
    const scope = scopeBtn.dataset.searchScope;
    const unavailable = (scope === 'artist' && !state.currentArtist) || (scope === 'folder' && !state.activeFolder);
    const active = state.searchScope === scope;
    scopeBtn.classList.toggle('active', active);
    scopeBtn.setAttribute('aria-pressed', active ? 'true' : 'false');
    scopeBtn.disabled = unavailable;
  });

  const tagsOnly = $('#tagsOnlyToggle');
  if (tagsOnly) tagsOnly.checked = state.searchTarget === 'tags';
}

export function openSearchOptions() {
  state.searchOptionsOpen = true;
  syncSearchOptionsControl();
}

export function closeSearchOptions() {
  state.searchOptionsOpen = false;
  syncSearchOptionsControl();
}

export function toggleSearchOptions() {
  state.searchOptionsOpen ? closeSearchOptions() : openSearchOptions();
}

export function setSearchScope(scope) {
  state.searchScope = validSearchScope(scope);
  syncSearchOptionsControl();
}

export function setSearchTarget(target) {
  state.searchTarget = target === 'tags' ? 'tags' : 'all';
  syncSearchOptionsControl();
}

export function syncMobileHeaderTools() {
  const header = $('#appHeader');
  if (!header) return;
  header.classList.toggle('mobile-tools-open', state.mobileHeaderToolsOpen);
  header.classList.toggle('mobile-tools-collapsed', !state.mobileHeaderToolsOpen);
  const btn = $('#mobileHeaderToggle');
  if (btn) {
    btn.setAttribute('aria-expanded', state.mobileHeaderToolsOpen ? 'true' : 'false');
    btn.setAttribute('aria-label', state.mobileHeaderToolsOpen ? '收起搜索和扫描' : '展开搜索和扫描');
    btn.setAttribute('title', state.mobileHeaderToolsOpen ? '收起搜索和扫描' : '展开搜索和扫描');
    btn.textContent = state.mobileHeaderToolsOpen ? '收起' : '搜索';
  }
}

export function setMobileHeaderToolsOpen(open) {
  const nextOpen = Boolean(open);
  const preserveGrid = isMobileViewport() && state.mode !== 'moves' && state.mobileHeaderToolsOpen !== nextOpen;
  const gridScrollAnchor = preserveGrid ? captureGridScrollAnchor() : null;
  state.mobileHeaderToolsOpen = nextOpen;
  syncMobileHeaderTools();
  if (!gridScrollAnchor) return;
  restoreGridScrollAnchor(gridScrollAnchor);
  requestAnimationFrame(() => {
    restoreGridScrollAnchor(gridScrollAnchor);
  });
}

export function toggleMobileHeaderTools() {
  setMobileHeaderToolsOpen(!state.mobileHeaderToolsOpen);
}

export function closeMobileHeaderTools() {
  setMobileHeaderToolsOpen(false);
}

export function closeMobileHeaderToolsIfMobile() {
  if (!isMobileViewport()) return;
  closeMobileHeaderTools();
}

function sidebarViewportMaxWidth() {
  return Math.max(SIDEBAR_WIDTH_MIN, Math.min(SIDEBAR_WIDTH_MAX, Math.floor(window.innerWidth * 0.45)));
}

function normalizeSidebarWidth(width) {
  const parsed = Number(width);
  if (!Number.isFinite(parsed)) return SIDEBAR_WIDTH_DEFAULT;
  return Math.max(SIDEBAR_WIDTH_MIN, Math.min(SIDEBAR_WIDTH_MAX, Math.round(parsed)));
}

export function setSidebarWidth(width, persist = false) {
  const desired = normalizeSidebarWidth(width);
  state.sidebarWidth = desired;
  const applied = Math.min(desired, sidebarViewportMaxWidth());
  document.documentElement.style.setProperty('--sidebar-width', `${applied}px`);
  if (persist) {
    try { localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(desired)); } catch (e) {}
  }
}

export function loadSidebarWidth() {
  let saved = null;
  try { saved = localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY); } catch (e) {}
  setSidebarWidth(saved || SIDEBAR_WIDTH_DEFAULT, false);
}

function normalizeSidebarTagRatio(ratio) {
  const parsed = Number(ratio);
  if (!Number.isFinite(parsed)) return SIDEBAR_TAG_RATIO_DEFAULT;
  return Math.max(SIDEBAR_TAG_RATIO_MIN, Math.min(SIDEBAR_TAG_RATIO_MAX, Math.round(parsed)));
}

export function setSidebarTagRatio(ratio, persist = false) {
  const desired = normalizeSidebarTagRatio(ratio);
  state.sidebarTagRatio = desired;
  document.documentElement.style.setProperty('--sidebar-tag-ratio', String(desired));
  const divider = $('#sidebarTagDivider');
  if (divider) {
    divider.setAttribute('aria-valuenow', String(desired));
    divider.setAttribute('aria-valuetext', `标签 ${desired}%`);
  }
  if (persist) {
    try { localStorage.setItem(SIDEBAR_TAG_RATIO_STORAGE_KEY, String(desired)); } catch (e) {}
  }
}

export function loadSidebarTagRatio() {
  let saved = null;
  try { saved = localStorage.getItem(SIDEBAR_TAG_RATIO_STORAGE_KEY); } catch (e) {}
  setSidebarTagRatio(saved || SIDEBAR_TAG_RATIO_DEFAULT, false);
}

function normalizeSidebarCollapsed(value) {
  let parsed = value;
  if (typeof parsed === 'string') {
    try { parsed = JSON.parse(parsed); } catch (e) { parsed = {}; }
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) parsed = {};
  return {
    filters: Boolean(parsed.filters),
    tags: Boolean(parsed.tags),
    folders: Boolean(parsed.folders),
    // The duplicate block used to live outside the shared storage key; its
    // historical UI default is closed, so a missing key must stay collapsed.
    duplicates: parsed.duplicates === undefined ? true : Boolean(parsed.duplicates),
  };
}

export function setSidebarCollapsed(section, collapsed, persist = false) {
  if (!Object.prototype.hasOwnProperty.call(state.sidebarCollapsed, section)) return;
  state.sidebarCollapsed = {...state.sidebarCollapsed, [section]: Boolean(collapsed)};
  syncSidebarCollapse();
  if (persist) {
    try { localStorage.setItem(SIDEBAR_COLLAPSED_STORAGE_KEY, JSON.stringify(state.sidebarCollapsed)); } catch (e) {}
  }
}

export function loadSidebarCollapsed() {
  let saved = null;
  try { saved = localStorage.getItem(SIDEBAR_COLLAPSED_STORAGE_KEY); } catch (e) {}
  state.sidebarCollapsed = normalizeSidebarCollapsed(saved);
  syncSidebarCollapse();
}

export function syncSidebarCollapse() {
  const sidebar = $('#filterSidebar');
  const collapsed = state.sidebarCollapsed || normalizeSidebarCollapsed(null);
  if (sidebar) {
    sidebar.classList.toggle('sidebar-tags-collapsed', collapsed.tags);
    sidebar.classList.toggle('sidebar-folders-collapsed', collapsed.folders);
    $$('[data-sidebar-section]').forEach(section => {
      const key = section.dataset.sidebarSection;
      const isCollapsed = Boolean(collapsed[key]);
      section.classList.toggle('is-collapsed', isCollapsed);
      const body = section.querySelector('.sidebar-section-body');
      if (body) body.hidden = isCollapsed;
      // Synchronize every toggle for the section: the title toggle and the
      // independent chevron toggle share one stored value, one aria-controls
      // target, and one aria-expanded state. The chevron label flips between
      // expand and collapse for assistive technology.
      section.querySelectorAll('[data-sidebar-section-toggle], [data-sidebar-chevron-toggle]').forEach(toggle => {
        toggle.setAttribute('aria-expanded', isCollapsed ? 'false' : 'true');
        if (toggle.dataset.sidebarChevronToggle !== undefined) {
          const name = SIDEBAR_SECTION_NAMES[key] || key;
          toggle.setAttribute('aria-label', `${isCollapsed ? '展开' : '折叠'}${name}`);
        }
      });
    });
  }
  const divider = $('#sidebarTagDivider');
  if (divider) {
    divider.hidden = isMobileViewport() || collapsed.tags || collapsed.folders;
    divider.setAttribute('aria-valuenow', String(state.sidebarTagRatio || SIDEBAR_TAG_RATIO_DEFAULT));
  }
}

export function bindSidebarSectionToggles() {
  $$('[data-sidebar-section-toggle]').forEach(toggle => {
    toggle.addEventListener('click', () => {
      const section = toggle.dataset.sidebarSectionToggle;
      setSidebarCollapsed(section, !state.sidebarCollapsed[section], true);
    });
  });
  $$('[data-sidebar-chevron-toggle]').forEach(toggle => {
    toggle.addEventListener('click', () => {
      const section = toggle.dataset.sidebarChevronToggle;
      setSidebarCollapsed(section, !state.sidebarCollapsed[section], true);
    });
  });
}

export function bindSidebarTagResize() {
  const handle = $('#sidebarTagDivider');
  if (!handle) return;

  const stopResize = e => {
    document.body.classList.remove('sidebar-tag-resizing');
    window.removeEventListener('pointermove', resizeSidebarTagRatio);
    window.removeEventListener('pointerup', stopResize);
    window.removeEventListener('pointercancel', stopResize);
    if (e && e.pointerId != null && handle.releasePointerCapture) {
      try { handle.releasePointerCapture(e.pointerId); } catch (err) {}
    }
    setSidebarTagRatio(state.sidebarTagRatio, true);
  };

  const resizeSidebarTagRatio = e => {
    const tag = $('#tagSection');
    const folder = $('#folderSection');
    if (!tag || !folder) return;
    const tagRect = tag.getBoundingClientRect();
    const folderRect = folder.getBoundingClientRect();
    const dividerHeight = handle.getBoundingClientRect().height;
    const usableHeight = Math.max(1, folderRect.bottom - tagRect.top - dividerHeight);
    const ratio = (e.clientY - tagRect.top - dividerHeight / 2) / usableHeight * 100;
    setSidebarTagRatio(ratio, false);
  };

  handle.addEventListener('pointerdown', e => {
    if (isMobileViewport() || state.sidebarCollapsed.tags || state.sidebarCollapsed.folders) return;
    e.preventDefault();
    document.body.classList.add('sidebar-tag-resizing');
    if (handle.setPointerCapture) handle.setPointerCapture(e.pointerId);
    window.addEventListener('pointermove', resizeSidebarTagRatio);
    window.addEventListener('pointerup', stopResize);
    window.addEventListener('pointercancel', stopResize);
  });

  handle.addEventListener('keydown', e => {
    if (isMobileViewport() || state.sidebarCollapsed.tags || state.sidebarCollapsed.folders) return;
    let next = state.sidebarTagRatio || SIDEBAR_TAG_RATIO_DEFAULT;
    if (e.key === 'ArrowUp') next -= SIDEBAR_TAG_RATIO_STEP;
    else if (e.key === 'ArrowDown') next += SIDEBAR_TAG_RATIO_STEP;
    else if (e.key === 'Home') next = SIDEBAR_TAG_RATIO_MIN;
    else if (e.key === 'End') next = SIDEBAR_TAG_RATIO_MAX;
    else return;
    e.preventDefault();
    setSidebarTagRatio(next, true);
  });
}

export function bindSidebarResize() {
  const handle = $('#sidebarResizer');
  if (!handle) return;
  let startX = 0;
  let startWidth = SIDEBAR_WIDTH_DEFAULT;

  const stopResize = e => {
    document.body.classList.remove('sidebar-resizing');
    window.removeEventListener('pointermove', resizeSidebar);
    window.removeEventListener('pointerup', stopResize);
    window.removeEventListener('pointercancel', stopResize);
    if (e && e.pointerId != null && handle.releasePointerCapture) {
      try { handle.releasePointerCapture(e.pointerId); } catch (err) {}
    }
    setSidebarWidth(state.sidebarWidth, true);
  };

  const resizeSidebar = e => {
    setSidebarWidth(startWidth + e.clientX - startX, false);
  };

  handle.addEventListener('pointerdown', e => {
    if (isMobileViewport()) return;
    e.preventDefault();
    startX = e.clientX;
    startWidth = state.sidebarWidth || SIDEBAR_WIDTH_DEFAULT;
    document.body.classList.add('sidebar-resizing');
    if (handle.setPointerCapture) handle.setPointerCapture(e.pointerId);
    window.addEventListener('pointermove', resizeSidebar);
    window.addEventListener('pointerup', stopResize);
    window.addEventListener('pointercancel', stopResize);
  });

  handle.addEventListener('keydown', e => {
    if (isMobileViewport()) return;
    let next = state.sidebarWidth || SIDEBAR_WIDTH_DEFAULT;
    if (e.key === 'ArrowLeft') next -= 20;
    else if (e.key === 'ArrowRight') next += 20;
    else if (e.key === 'Home') next = SIDEBAR_WIDTH_DEFAULT;
    else if (e.key === 'End') next = SIDEBAR_WIDTH_MAX;
    else return;
    e.preventDefault();
    setSidebarWidth(next, true);
  });
}

function normalizeMobileColumns(value) {
  const parsed = parseInt(value, 10);
  return [1, 2, 3].includes(parsed) ? parsed : MOBILE_COLUMNS_DEFAULT;
}

export function setMobileColumns(columns, persist = false) {
  state.mobileColumns = normalizeMobileColumns(columns);
  document.documentElement.style.setProperty('--mobile-grid-columns', String(state.mobileColumns));
  $$('#mobileColumnToggle [data-mobile-columns]').forEach(btn => {
    const active = Number(btn.dataset.mobileColumns) === state.mobileColumns;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });
  if (persist) {
    try { localStorage.setItem(MOBILE_COLUMNS_STORAGE_KEY, String(state.mobileColumns)); } catch (e) {}
  }
}

export function loadMobileColumns() {
  let saved = null;
  try { saved = localStorage.getItem(MOBILE_COLUMNS_STORAGE_KEY); } catch (e) {}
  setMobileColumns(saved || MOBILE_COLUMNS_DEFAULT, false);
}

export function normalizeCardRatio(value) {
  return CARD_RATIOS.includes(value) ? value : CARD_RATIO_DEFAULT;
}

export function setCardRatio(ratio, persist = false) {
  state.cardRatio = normalizeCardRatio(ratio);
  document.body.classList.toggle('card-ratio-3x4', state.cardRatio === '3x4');
  const select = $('#cardRatioSelect');
  if (select) select.value = state.cardRatio;
  if (persist) {
    try { localStorage.setItem(CARD_RATIO_STORAGE_KEY, String(state.cardRatio)); } catch (e) {}
  }
}

export function loadCardRatio() {
  let saved = null;
  try { saved = localStorage.getItem(CARD_RATIO_STORAGE_KEY); } catch (e) {}
  setCardRatio(saved || CARD_RATIO_DEFAULT, false);
}

export function bindMobileColumnToggle() {
  $$('#mobileColumnToggle [data-mobile-columns]').forEach(btn => {
    btn.addEventListener('click', () => {
      setMobileColumns(btn.dataset.mobileColumns, true);
      // The toggle must take effect immediately: renderGrid re-partitions the
      // browse grid through the normal reconcile path, reusing loaded
      // thumbnails and never re-requesting the item list.
      renderGrid();
      logUiAction('mobile_column_change', collectUiLogContext({
        columns: state.mobileColumns,
      }));
    });
  });
}

export function renderDuplicateFolders() {
  const section = $('#duplicateSection');
  const groups = state.duplicateFolders;
  if (!groups.length) {
    section.style.display = 'none';
    $('#duplicateList').innerHTML = '';
    $('#duplicateCount').textContent = '0';
    duplicateSectionLifecycle = 'empty';
    return;
  }

  // A fresh page load restores the stored collapse preference: hydration with
  // existing groups shows the section but leaves `sidebarCollapsed`,
  // `aria-expanded`, `hidden`, and local storage exactly as restored. Only a
  // runtime transition from zero groups to the first groups reveals the
  // section once, and that is the only render that may write persisted state.
  const appeared = duplicateSectionLifecycle === 'empty' && section.style.display === 'none';
  duplicateSectionLifecycle = 'present';
  section.style.display = '';
  if (appeared) setSidebarCollapsed('duplicates', false, true);
  $('#duplicateCount').textContent = `${groups.length}组`;
  $('#duplicateList').innerHTML = groups.map(group => `
    <div class="duplicate-group">
      <div class="duplicate-name">${escHtml(group.name)} <span>${group.count}</span></div>
      ${group.paths.map(path => `
        <button class="duplicate-path" type="button" data-artist-id="${path.id}" title="${escHtml(path.path)}">
          <span>${escHtml(path.display_path || path.path)}</span>
          <strong>${path.item_count || 0}</strong>
        </button>
      `).join('')}
    </div>
  `).join('');

  bindDuplicateListActions($('#duplicateList'));
}

// Duplicate paths jump to the artist and close the mobile drawer; one
// delegated listener on the list covers every render.
function bindDuplicateListActions(container) {
  if (!container || container.dataset.duplicateBound === '1') return;
  container.dataset.duplicateBound = '1';
  container.addEventListener('click', e => {
    const btn = e.target instanceof Element ? e.target.closest('.duplicate-path') : null;
    if (!btn || !container.contains(btn)) return;
    selectArtist(btn.dataset.artistId);
    closeFilterDrawerIfMobile();
  });
}

export function renderSidebar() {
  $('#tagFilterReset').disabled = !state.activeRole || String(state.activeRole).startsWith('__');
  const s = state.stats;
  if (!s) {
    $('#sidebarList').innerHTML = '';
    return;
  }
  const tags = Array.isArray(s.tags) ? s.tags : [];
  let html = '';
  sortSidebarTags(tags).forEach(r => {
    const active = state.activeRole === String(r.id) ? ' active' : '';
    html += `<div class="sidebar-item${active}" data-role="${r.id}" role="button" tabindex="0">
      <span>${escHtml(r.name)}</span><span class="count">${r.count}</span></div>`;
  });
  $('#sidebarList').innerHTML = html || '<div class="sidebar-list-empty sidebar-empty-state">没有标签</div>';

  bindSidebarEvents();
}

export function sortSidebarTags(tags) {
  const sorted = [...tags];
  const tagSortEl = $('#tagSort');
  const mode = tagSortEl ? tagSortEl.value : getSavedTagSort();
  if (mode === 'name') {
    sorted.sort((a, b) => mergeTagsByNameCollator.compare(a.name || '', b.name || ''));
  } else if (mode === 'count') {
    sorted.sort((a, b) => (b.count || 0) - (a.count || 0) || mergeTagsByNameCollator.compare(a.name || '', b.name || ''));
  }
  return sorted;
}

export function renderMediaFilter() {
  const select = $('#mediaFilter');
  const s = state.currentArtist ? state.stats : null;
  if (!s) {
    select.innerHTML = '<option value="">全部媒体</option>';
    select.disabled = true;
    return;
  }
  const active = Object.values(BROWSE_KINDS).includes(state.activeRole) ? state.activeRole : '';
  const filters = [
    ['', '全部媒体', s.total],
    [BROWSE_KINDS.untagged, '未加标签', s.untagged || 0],
    [BROWSE_KINDS.favorites, '收藏', s.favorites || 0],
    [BROWSE_KINDS.archives, '压缩包', s.archives || 0],
    [BROWSE_KINDS.videos, '视频', s.videos || 0],
    [BROWSE_KINDS.sources, '源文件', s.sources || 0],
  ].filter(([value, , count]) => !value || count > 0 || value === active || value === BROWSE_KINDS.favorites || value === BROWSE_KINDS.untagged);
  select.innerHTML = filters.map(([value, label, count]) =>
    `<option value="${value}">${label} ${count}</option>`
  ).join('');
  select.value = active;
  select.disabled = false;
}

export function renderFolderTree() {
  const tree = state.folders;
  if (!tree || typeof tree !== 'object' || Array.isArray(tree)) {
    $('#folderTree').innerHTML = '';
    return;
  }

  $('#folderTree').innerHTML = renderFolderNode(tree, 0);
  bindFolderEvents();
}

function renderFolderNode(node, level) {
  if (!node || typeof node !== 'object') return '';
  if (Array.isArray(node)) return '';
  const path = node.path || '';
  const name = path ? node.name : '全部';
  const active = state.activeFolder === path || (!state.activeFolder && !path);
  let html = `<div class="folder-item${path ? '' : ' folder-all'}${active ? ' active' : ''}" data-folder="${escHtml(path)}" role="button" tabindex="0" title="${escHtml(path || name)}" style="--level:${level}">
    <span class="folder-name">${escHtml(name)}</span><span class="count">${node.item_count || 0}</span>
  </div>`;
  const children = Array.isArray(node.children) ? node.children : [];
  children.forEach(child => {
    html += renderFolderNode(child, level + 1);
  });
  return html;
}

export function selectFolder(folder) {
  state.activeFolder = folder || null;
  state.search = '';
  $('#searchInput').value = '';
  state.tagSearchResults = [];
  state.selectedIds.clear();
  syncSearchOptionsControl();
  updateEditBar();
  renderFolderTree();
  updateDuplicateFilesButton();
  scrollToItemsTop();
  syncBrowseUrl('push');
  loadItems();
  closeFilterDrawerIfMobile();
}

function bindFolderEvents() {
  const tree = $('#folderTree');
  if (!tree || tree.dataset.folderBound === '1') return;
  tree.dataset.folderBound = '1';
  tree.addEventListener('click', e => {
    const el = e.target instanceof Element ? e.target.closest('.folder-item') : null;
    if (el && tree.contains(el)) selectFolder(el.dataset.folder || '');
  });
  tree.addEventListener('keydown', e => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const el = e.target instanceof Element ? e.target.closest('.folder-item') : null;
    if (!el || !tree.contains(el)) return;
    e.preventDefault();
    selectFolder(el.dataset.folder || '');
  });
}

function bindSidebarEvents() {
  const list = $('#sidebarList');
  if (!list || list.dataset.sidebarBound === '1') return;
  list.dataset.sidebarBound = '1';
  list.addEventListener('click', e => {
    const el = e.target instanceof Element ? e.target.closest('.sidebar-item') : null;
    if (el && list.contains(el)) selectBrowseRole(el.dataset.role);
  });
  list.addEventListener('keydown', e => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const el = e.target instanceof Element ? e.target.closest('.sidebar-item') : null;
    if (!el || !list.contains(el)) return;
    e.preventDefault();
    selectBrowseRole(el.dataset.role);
  });
  list.addEventListener('dragover', e => {
    if (state.selectedIds.size === 0) return;
    const el = e.target instanceof Element ? e.target.closest('.sidebar-item') : null;
    if (!el) return;
    e.preventDefault();
    el.classList.add('drag-over');
  });
  list.addEventListener('dragleave', e => {
    const el = e.target instanceof Element ? e.target.closest('.sidebar-item') : null;
    if (el) el.classList.remove('drag-over');
  });
  list.addEventListener('drop', e => {
    if (state.selectedIds.size === 0) return;
    const el = e.target instanceof Element ? e.target.closest('.sidebar-item') : null;
    if (!el) return;
    e.preventDefault();
    el.classList.remove('drag-over');
    const role = el.dataset.role || null;
    if (state.selectedIds.size > 0 && role && !role.startsWith('__')) {
      classifyItems([...state.selectedIds], [parseInt(role)], 'add');
    }
  });
}

export function selectBrowseRole(role) {
  state.activeRole = role || null;
  state.selectedIds.clear();
  updateEditBar();
  renderSidebar();
  renderToolbar();
  scrollToItemsTop();
  syncBrowseUrl('push');
  loadItems();
  closeFilterDrawerIfMobile();
}

export function renderToolbar() {
  renderMediaFilter();
  updateDuplicateFilesButton();
}

export async function loadItems(options = {}) {
  const append = Boolean(options.append);
  if (append && !state.hasMoreItems) return;
  if (append && (state.loadingItems || state.loadingMoreItems)) return;
  const seq = append ? Number(state.itemLoadSeq || 0) : nextRequestSeq('itemLoadSeq');
  const artistIdAtStart = state.currentArtist ? Number(state.currentArtist.id) : null;
  const searchScope = effectiveSearchScope();
  const globalSearch = isGlobalSearchActive();
  const folderScoped = state.activeFolder && (!state.search || searchScope === 'folder');
  const duplicateScopeActive = isDuplicateFilesScopeActive();
  if (state.duplicatesOnly && !duplicateScopeActive) {
    state.duplicatesOnly = false;
  }
  updateDuplicateFilesButton();
  if (!state.currentArtist && !globalSearch) {
    state.allItems = [];
    state.itemsOffset = 0;
    state.hasMoreItems = false;
    if (!append) {
      // This call claimed the load sequence number; an in-flight older load is
      // now stale and would skip its own flag reset, so release the flags here.
      state.loadingItems = false;
      state.loadingMoreItems = false;
      releaseAllImageLoads();
      releaseAllVideoPreviewLoads();
      const grid = $('#grid');
      if (grid) grid.innerHTML = '';
      renderLibraryEmptyState();
    }
    return;
  }
  state.loadingItems = true;
  state.loadingMoreItems = append;
  if (!append) {
    state.itemsCursor = null;
    releaseAllImageLoads();
    releaseAllVideoPreviewLoads();
    resetCharacterTagSuggestions();
  }

  const cursorSearch = globalSearch;
  const offset = append ? state.itemsOffset : 0;
  const previousCount = append ? state.allItems.length : 0;
  const params = new URLSearchParams({limit: ITEM_PAGE_LIMIT, sort: state.itemSort});
  if (!cursorSearch) params.set('offset', offset);
  if (cursorSearch && append && state.itemsCursor) params.set('cursor', state.itemsCursor);

  if (!globalSearch) {
    const aid = state.currentArtist.id;
    params.set('artist_id', aid);

    if (state.activeRole === '__archives__') {
      params.set('archive_only', 'true');
    } else if (state.activeRole === '__videos__') {
      params.set('media_type', 'video');
    } else if (state.activeRole === '__sources__') {
      params.set('media_type', 'source');
    } else if (state.activeRole === '__untagged__') {
      params.set('untagged', 'true');
    } else if (state.activeRole === '__favorites__') {
      params.set('favorite_only', 'true');
    } else if (state.activeRole) {
      params.set('tag_id', state.activeRole);
    }
  }

  if (state.search) params.set('search', state.search);
  if (state.itemDateFrom) params.set('date_from', state.itemDateFrom);
  if (state.itemDateTo) params.set('date_to', state.itemDateTo);
  if (state.search && state.searchTarget === 'tags') params.set('search_tags_only', 'true');
  if (!globalSearch && folderScoped) params.set('folder', state.activeFolder);
  if (!globalSearch && state.duplicatesOnly) params.set('duplicates_only', 'true');

  let tagSearchPromise = Promise.resolve({tags: []});
  if (state.search && state.searchTarget === 'tags') {
    const tagParams = new URLSearchParams({search: state.search, limit: 100});
    if (!globalSearch && state.currentArtist) tagParams.set('artist_id', state.currentArtist.id);
    tagSearchPromise = API.get('/api/tags/search?' + tagParams.toString());
  }

  let loadSucceeded = false;
  try {
    const [data, tagData] = await Promise.all([
      API.get('/api/items?' + params.toString()),
      tagSearchPromise,
    ]);
    if (!isCurrentRequestSeq('itemLoadSeq', seq)
      || (state.currentArtist ? Number(state.currentArtist.id) : null) !== artistIdAtStart) return;
    const nextItems = Array.isArray(data.items)
      ? data.items.filter(row => row && typeof row === 'object')
      : [];
    state.allItems = append ? state.allItems.concat(nextItems) : nextItems;
    state.itemsOffset = state.allItems.length;
    state.itemsCursor = cursorSearch ? (data.next_cursor || null) : null;
    state.hasMoreItems = cursorSearch
      ? (data.has_more != null ? Boolean(data.has_more) : nextItems.length === ITEM_PAGE_LIMIT)
      : (data.total != null ? state.itemsOffset < Number(data.total) : nextItems.length === ITEM_PAGE_LIMIT);
    if (!append) {
      state.tagSearchResults = Array.isArray(tagData.tags)
        ? tagData.tags.filter(row => row && typeof row === 'object')
        : [];
    }
    if (append) {
      appendItemsToGrid(nextItems, previousCount);
    } else {
      renderGrid();
    }
    if (state.selectedIds.size > 0) {
      scheduleCharacterTagSuggestions({reason: 'items', append});
    }
    updateDuplicateFilesButton();
    if (isDuplicateFilesScopeActive()) {
      // Keep the previous hash baseline. A scan refresh can finish while its
      // candidates are still hashing; clearing it here would make the first
      // poll observe only the completed state and miss the new items.
      scheduleDuplicatesViewRefresh();
    }
    logUiAction('items_loaded', collectUiLogContext({
      append,
      returned_count: nextItems.length,
      offset: state.itemsOffset,
      has_more: state.hasMoreItems,
      mobile: isMobileViewport(),
    }));
    loadSucceeded = true;
  } catch (e) {
    if (isCurrentRequestSeq('itemLoadSeq', seq) && append) {
      toast('加载更多媒体失败', 'error');
    } else if (isCurrentRequestSeq('itemLoadSeq', seq)) {
      state.allItems = [];
      state.itemsOffset = 0;
      state.hasMoreItems = false;
      state.tagSearchResults = [];
      renderGrid();
      toast('加载媒体失败', 'error');
    }
  } finally {
    if (isCurrentRequestSeq('itemLoadSeq', seq)) {
      state.loadingItems = false;
      state.loadingMoreItems = false;
    }
  }
  if (!isCurrentRequestSeq('itemLoadSeq', seq)) return;
  if (!loadSucceeded) return;
  requestAnimationFrame(maybeLoadMoreOnScroll);
}

export async function loadItemsPreservingDepth() {
  const expectedCount = state.allItems.length;
  await loadItems();
  if (expectedCount <= 0 || !state.hasMoreItems) return;
  let pages = 0;
  while (
    state.hasMoreItems
    && !state.loadingItems && !state.loadingMoreItems
    && state.allItems.length < expectedCount
    && pages < MAX_DEPTH_REFILL_PAGES
  ) {
    const seqBefore = Number(state.itemLoadSeq || 0);
    await loadItems({append: true});
    pages += 1;
    // A user-initiated reload owns newer state now; stop refilling.
    if (Number(state.itemLoadSeq || 0) !== seqBefore) return;
  }
}

export function scrollToItemsTop() {
  const container = $('#gridContainer');
  if (container) container.scrollTo({top: 0, behavior: 'auto'});
  window.scrollTo({top: 0, behavior: 'auto'});
}

export function remainingScrollDistance() {
  const container = $('#gridContainer');
  if (container && container.clientHeight) {
    return container.scrollHeight - container.scrollTop - container.clientHeight;
  }
  return document.documentElement.scrollHeight - (window.scrollY + window.innerHeight);
}

export function maybeLoadMoreOnScroll() {
  if (state.mode === 'moves') return;
  if (!state.hasMoreItems || state.loadingItems || state.loadingMoreItems) return;
  if (remainingScrollDistance() <= INFINITE_SCROLL_THRESHOLD) {
    loadItems({append: true});
  }
}

export function isCurrentFolderScopeActive() {
  return Boolean(state.currentArtist && state.activeFolder && state.mode !== 'moves' && !isGlobalSearchActive() && (!state.search || effectiveSearchScope() === 'folder'));
}

export function isDuplicateFilesScopeActive() {
  return Boolean(state.currentArtist && state.mode !== 'moves' && !isGlobalSearchActive());
}

export function isCurrentArtistScanScopeActive() {
  return Boolean(state.currentArtist && !state.activeFolder && state.mode !== 'moves' && !isGlobalSearchActive() && (!state.search || effectiveSearchScope() === 'artist'));
}

export function isCurrentScanScopeActive() {
  return isCurrentFolderScopeActive() || isCurrentArtistScanScopeActive();
}

export function updateDuplicateFilesButton() {
  const toggle = $('#duplicateFilesToggle');
  if (!toggle) return;
  const visible = isDuplicateFilesScopeActive();
  if (!visible) state.duplicatesOnly = false;
  toggle.style.display = visible ? '' : 'none';
  $$('#duplicateFilesToggle [data-duplicates]').forEach(btn => {
    const active = (btn.dataset.duplicates === 'duplicates') === state.duplicatesOnly;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });
  updateScanFolderButton();
}

export function updateScanFolderButton() {
  const btn = $('#scanFolderBtn');
  if (!btn) return;
  const label = state.activeFolder ? '扫描文件夹' : '扫描画师';
  btn.textContent = label;
  btn.title = label;
  btn.style.display = !state.scanRunning && isCurrentScanScopeActive() ? '' : 'none';
}

export function isGlobalSearchActive() {
  return Boolean(state.search && effectiveSearchScope() === 'global');
}

// Tag-only search results render into their own #tagResults container above
// #grid, not into the grid HTML. The rebuild is signature-gated: unrelated
// grid re-renders (loads, refreshes) reuse the same chip nodes, so the chip
// under the pointer stays put and stays clickable mid-interaction.
let tagResultsSignature = null;

export function hasTagSearchResults() {
  return Boolean(
    state.search
    && state.searchTarget === 'tags'
    && state.tagSearchResults.length
    && (state.currentArtist || isGlobalSearchActive()),
  );
}

export function renderTagSearchResults() {
  const container = $('#tagResults');
  const visible = hasTagSearchResults();
  const signature = visible
    ? `${state.search}|${state.tagSearchResults.map(tag => `${tag.id}:${tag.item_count || 0}`).join(',')}`
    : '';
  if (container) {
    if (signature !== tagResultsSignature || (visible && !container.firstElementChild)) {
      tagResultsSignature = signature;
      container.innerHTML = visible
        ? `<div class="tag-result-title">标签结果</div>
        <div class="tag-result-list">
          ${state.tagSearchResults.map(tag => `
            <button class="tag-result-card" type="button" data-tag-jump="${tag.id}" data-artist-id="${tag.artist_id}" title="转到 ${escHtml(tag.artist_name || '')}">
              <span>${escHtml(tag.name)}</span>
              <em>${escHtml(tag.artist_name || '未知画师')}</em>
              <strong>${tag.item_count || 0} 项</strong>
            </button>
          `).join('')}
        </div>`
        : '';
    }
    container.hidden = !visible;
  }
  return visible;
}

export function clearUI() {
  state.allItems = [];
  state.itemsOffset = 0;
  state.itemsCursor = null;
  state.hasMoreItems = false;
  state.stats = null;
  state.tags = [];
  state.folders = null;
  resetCharacterTagSuggestions();
  releaseAllImageLoads();
  releaseAllVideoPreviews();
  $('#sidebarList').innerHTML = '';
  const grid = $('#grid');
  if (grid) grid.innerHTML = '';
  $('#folderTree').innerHTML = '';
  const tagResults = $('#tagResults');
  if (tagResults) {
    tagResults.innerHTML = '';
    tagResults.hidden = true;
  }
  tagResultsSignature = null;
  renderLibraryEmptyState();
  renderToolbar();
}

export function syncItemFilterControls() {
  const sortEl = $('#itemSort');
  if (sortEl) sortEl.value = state.itemSort;
  const dateFrom = $('#itemDateFrom');
  if (dateFrom) {
    dateFrom.value = state.itemDateFrom;
    dateFrom.max = state.itemDateTo;
  }
  const dateTo = $('#itemDateTo');
  if (dateTo) {
    dateTo.value = state.itemDateTo;
    dateTo.min = state.itemDateFrom;
  }
  const dateReset = $('#itemDateReset');
  if (dateReset) {
    dateReset.disabled = !state.itemDateFrom && !state.itemDateTo;
  }
}

// Cross-module imports that close the sidebar <-> router/grid cycles. All of
// these are only ever invoked from function bodies, never at module scope.
import { recentArtistList, naturalArtistList, artistOptionButtonHtml, bindArtistChoiceContainer, selectArtist } from '../router.js';
import { captureGridScrollAnchor, restoreGridScrollAnchor } from './grid.js';
import { releaseAllVideoPreviews } from './grid.js';
import { classifyItems } from './editbar.js';
import { scheduleDuplicatesViewRefresh } from '../events.js';
