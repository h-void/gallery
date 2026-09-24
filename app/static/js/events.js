// Global event wiring and mode switching. All dynamic-list interactions go
// through container-level delegation (grid cards, tag picker, move lists);
// static controls keep direct listeners.

import { API } from './api.js';
import { state, nextRequestSeq, isCurrentRequestSeq } from './store.js';
import { $, $$, debounce } from './utils.js';
import { toast, logUiAction, collectUiLogContext } from './logging.js';
import {
  restoreBrowseUrl, syncBrowseUrl, browseUrlParams, saveItemSort, saveItemDates,
  getSavedTagSort, saveTagSort, loadArtists, closeArtistDropdown, updateBrowseReturnButton,
} from './router.js';
import {
  syncFilterDrawer, openFilterDrawer, closeFilterDrawer, closeFilterDrawerIfMobile,
  closeMobileHeaderTools, closeMobileHeaderToolsIfMobile, toggleMobileHeaderTools,
  syncMobileHeaderTools, syncSearchOptionsControl, openSearchOptions, closeSearchOptions,
  toggleSearchOptions, setSearchScope, setSearchTarget, bindSidebarResize,
  bindSidebarTagResize, bindSidebarSectionToggles, bindMobileColumnToggle, setSidebarWidth,
  setCardRatio, syncItemFilterControls, selectBrowseRole, renderSidebar, renderFolderTree,
  renderToolbar, loadItems, loadItemsPreservingDepth, updateDuplicateFilesButton,
  updateScanFolderButton, isDuplicateFilesScopeActive, scrollToItemsTop,
  onViewportLayoutChange, renderLibraryEmptyState, isMobileViewport, syncClearSearch,
} from './views/sidebar.js';
import { renderGrid, bindGridEvents, captureGridScrollAnchor, restoreGridScrollAnchor, isTaggableItem, scheduleJustifiedRelayout } from './views/grid.js';
import {
  closeLightbox, moveLightbox, onLightboxWheel, startLightboxPan, moveLightboxPan,
  stopLightboxPan, onLightboxDelete, bindLightboxVideoDiagnostics,
} from './views/lightbox.js';
import {
  updateEditBar, applySelectionChange, ensureEditTagContext, selectOrCreateEditTagQuery,
  selectedEditTagIds, selectedEditTagNames, characterSuggestionCoverageWarning,
  applyItemDateBatch, editDateEnteredValue, syncEditDatePrecisionInputs,
  deleteSelectedMediaItems, removeSelectedTagsFromItems, selectAllCharacterSuggestions,
  selectCharacterSuggestionTag, closeEditTagPicker, renderEditTagPicker,
  setEditMode, syncEditModeButton,
} from './views/editbar.js';
import { bindArtistLinks, bindArtistProfileLinks, renderArtistLinks, renderArtistProfileLinks, closeArtistLinksDialog } from './views/links.js';
import { bindArchiveModal, closeArchiveModal } from './views/archive_modal.js';
import {
  loadMoveWorkbench, refreshActiveMaintenanceView, startMaintenanceAutoRefresh,
  stopMaintenanceAutoRefresh, scheduleMaintenanceAutoRefresh, setMaintenanceView, syncMaintenanceTabsEdge, handleMaintenanceJump,
} from './views/maintenance/index.js';
import {
  runFolderRenameAllNow, saveMlDownloadSource, retryMlRuntime,
  loadHealthSummary, setFolderRenameAutoEnabled, startFullScan, backfillItemDimensions, openErrorArtistsDialog, closeErrorArtistsDialog,
  loadErrorArtistsPage, jumpToErrorArtist,
} from './views/maintenance/overview.js';
import { autoResolveMoveCandidates } from './views/maintenance/paths.js';
import {
  saveArchiveSettings, refreshArchivePlans, previewArchivePlans, applyArchiveTemplate, syncArchiveRuleDirtyState,
  scheduleArchiveDraftPreview, switchOrganizeArtist, stepOrganizeArtist,
  toggleAllArchivePlansConfirmation, executeArchivePlans, invalidateArtistFolderMovePreview,
  previewArtistFolderMove, executeArtistFolderMove, openDirectoryPicker,
  closeDirectoryPicker, chooseDirectoryPicker, loadDirectoryPicker, toggleArchivePlansFold,
} from './views/maintenance/organize.js';
import {
  rebuildCharacterIndex, importCharacterLibraryReferences, cancelCharacterImportJob,
  deleteCharacter, deleteCharacterReference, loadCharacterLibrary, renderCharacterLibrary,
  setCharacterLibraryMobileView, gotoCharacterLibraryPanel, openCharacterReferences,
  applyCharacterLibraryMobileView,
  uploadCharacterReference, createCharacter,
} from './views/maintenance/characters.js';
import {
  loadRecycleBin, restoreRecycleEntry, purgeRecycleEntry, clearRecycleBin, setOperationHistoryFilter,
  toggleOperationHistoryFold,
} from './views/maintenance/records.js';
import {
  saveDownloadsSettings, resetDownloadTemplates, addDownloadSubscription, deleteDownloadSubscription,
  toggleDownloadSubscription, setDownloadSubscriptionMode, startDownloadSync,
  downloadPostByHand, insertDownloadTemplateToken, rememberDownloadTemplateInput,
  downloadTemplateInputSelector, toggleDownloadType, openDownloadArtistCombo, closeDownloadArtistCombo,
  pickDownloadArtist, renderDownloadArtistCombo, markDownloadSettingsDirty,
  checkDownloadMissing, refreshDownloadLog, reconcileDownloadLibrary, openDownloadDay,
  checkSubscriptionMissing, reconcileSubscriptionLibrary, renderDownloadArtistFolder,
  loadMoreDownloadDayPosts, decideDownloadPost, loadDownloadCandidates,
  bindDownloadCandidate, verifyDownloadPost, stopDownloadPost, importDownloadPost,
  openDownloadPostFiles, retryDownloadFile, applyDownloadWorksFilter,
  scheduleDownloadWorksSearch, toggleDownloadWorksSelectAll, clearDownloadWorksSelection,
  loadMoreDownloadWorks, downloadWorksSelection, downloadWorksPost,
  toggleDownloadWorkSelection, downloadWorksSelected,
  openDownloadWorksArtistCombo, closeDownloadWorksArtistCombo,
  pickDownloadWorksArtist, renderDownloadWorksArtistCombo,
  jumpToDownloadWorkFolder,
  deliverPostToNetdisk,
  renderDownloadsPanel,
  renderDownloadSubscriptions, setDownloadSubscriptionFilter, toggleDownloadSubscriptionsFold, toggleDownloadLogFold,
} from './views/maintenance/downloads.js';
import {
  saveNetdiskSettings, testNetdiskConnection, checkNetdiskPaths, connectNetdisk, disconnectNetdisk,
  generateNetdiskScript, resetNetdiskPairing, copyNetdiskScript, runNetdiskJobAction, markNetdiskSettingsDirty,
  resolveNetdiskDispatchPost, submitNetdiskDispatch,
  setNetdiskJobFilter, toggleNetdiskJobsFold,
} from './views/maintenance/netdisk.js';
import { toggleTheme, setThemeMode } from './views/theme.js';

const DUPLICATES_VIEW_ACTIVE_REFRESH_MS = 10000;
const DUPLICATES_VIEW_IDLE_REFRESH_MS = 60000;

export function applyMode(mode) {
  const fromMode = state.mode;
  const isMoves = mode === 'moves';
  const maintenanceBtn = $('#maintenanceBtn');
  if (maintenanceBtn) {
    maintenanceBtn.classList.toggle('active', isMoves);
    maintenanceBtn.setAttribute('aria-pressed', String(isMoves));
    maintenanceBtn.textContent = isMoves ? '浏览' : '维护';
    maintenanceBtn.title = isMoves ? '浏览' : '维护';
  }
  state.mode = mode;
  state.selectedIds.clear();
  closeFilterDrawer();
  closeMobileHeaderToolsIfMobile();
  updateEditBar();
  document.body.classList.toggle('mode-moves', isMoves);
  document.body.classList.toggle('mode-browse', mode === 'browse' || !mode);
  $('#movePanel').classList.toggle('visible', isMoves);
  if (isMoves) requestAnimationFrame(syncMaintenanceTabsEdge);
  $('#gridContainer').style.display = isMoves ? 'none' : '';
  $('.toolbar').style.display = isMoves ? 'none' : '';
  $('#searchInput').disabled = isMoves;
  $('#searchOptionsBtn').disabled = isMoves;
  if (isMoves) closeSearchOptions();
  updateDuplicateFilesButton();
  updateBrowseReturnButton();
  renderArtistLinks();
  renderArtistProfileLinks();
  renderLibraryEmptyState();
  if (isMoves) {
    setMaintenanceView(state.maintenanceView || 'overview');
    loadMoveWorkbench().catch(e => {
      if (!isAbortError(e)) toast('维护页面刷新失败：' + (e.message || e), 'error');
    });
    startMaintenanceAutoRefresh();
  } else {
    stopMaintenanceAutoRefresh();
    renderSidebar();
    loadItemsPreservingDepth().then(() => {
      logModeChangeLayout({from_mode: fromMode, to_mode: mode});
    }).catch(e => {
      toast('加载媒体失败', 'error');
      logUiAction('mode_change', collectUiLogContext({
        from_mode: fromMode,
        to_mode: mode,
        error: e.message || String(e),
      }));
    });
  }
}

// One clear path for the typed query: Escape, the clear button, and any
// future caller share the same reset so scope, selection and URL state stay
// consistent. It only clears the search — filters, dates, tags and sort stay.
function clearBrowseSearch(input) {
  if (input) input.value = '';
  state.search = '';
  logUiAction('search_change', {search: '', scope: state.searchScope, target: state.searchTarget});
  state.selectedIds.clear();
  updateEditBar();
  scrollToItemsTop();
  syncBrowseUrl('push');
  loadItems();
}

function isAbortError(error) {
  return Boolean(error && (error.name === 'AbortError' || error.code === 20));
}

function applyItemFilterControls() {
  const dateFrom = $('#itemDateFrom');
  const dateTo = $('#itemDateTo');
  const invalid = dateFrom && dateTo && dateFrom.value && dateTo.value && dateFrom.value > dateTo.value;
  if (dateFrom) dateFrom.setCustomValidity(invalid ? '开始日期不能晚于结束日期' : '');
  if (dateTo) dateTo.setCustomValidity(invalid ? '结束日期不能早于开始日期' : '');
  if (invalid) {
    if (dateFrom) dateFrom.reportValidity();
    return;
  }
  const sortEl = $('#itemSort');
  if (sortEl) state.itemSort = sortEl.value;
  state.itemDateFrom = dateFrom ? dateFrom.value : '';
  state.itemDateTo = dateTo ? dateTo.value : '';
  saveItemSort(state.itemSort);
  saveItemDates(state.itemDateFrom, state.itemDateTo);
  syncItemFilterControls();
  renderFolderTree();
  state.selectedIds.clear();
  updateEditBar();
  scrollToItemsTop();
  syncBrowseUrl('push');
  loadItems();
}

function logModeChangeLayout(data = {}) {
  const restore = data.restore || {};
  logUiAction('mode_change', collectUiLogContext({
    from_mode: data.from_mode || '',
    to_mode: data.to_mode || state.mode,
    seq: data.seq ?? null,
    first_visible_id: restore.first_visible_id ?? null,
    before_top: restore.before_top == null ? null : Math.round(restore.before_top),
    after_top: restore.after_top == null ? null : Math.round(restore.after_top),
    top_delta: restore.top_delta == null ? null : Math.round(restore.top_delta),
    grid_scroll_top: restore.grid_scroll_top ?? ($('#gridContainer') ? Math.round($('#gridContainer').scrollTop) : 0),
    edit_bar_height: restore.edit_bar_height ?? ($('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0),
    restored: Boolean(restore.restored),
    stale: Boolean(restore.stale),
    scroll_source: restore.scroll_source || '',
  }));
}

export async function refreshCurrentView({reason = 'manual'} = {}) {
  const currentArtistId = state.currentArtist ? state.currentArtist.id : null;
  // selectArtist bumps artistLoadSeq but not scanRefreshSeq: watch both so a
  // user switching artists mid-refresh can never be overwritten back to the
  // previous artist's state or URL.
  const artistLoadSeqAtStart = Number(state.artistLoadSeq || 0);
  const artistChanged = () => Number(state.artistLoadSeq || 0) !== artistLoadSeqAtStart;
  const hadNoArtistsBeforeRefresh = state.artists.length === 0;
  const activeFolder = state.activeFolder;
  const currentMode = state.mode;
  const maintenanceView = state.maintenanceView;
  const gridScrollAnchor = captureGridScrollAnchor();
  const seq = nextRequestSeq('scanRefreshSeq');
  await loadArtists();
  if (!isCurrentRequestSeq('scanRefreshSeq', seq) || artistChanged()) return seq;
  if (currentMode === 'moves' || state.mode === 'moves') {
    setMaintenanceView(maintenanceView || 'overview');
    await loadMoveWorkbench({preserveScroll: true});
    logUiAction('refresh_current_view', {reason, mode: 'moves'});
    return seq;
  }
  const shouldAutoSelectFirstScannedArtist =
    reason === 'scan_complete' &&
    state.browseUrlRestored &&
    !currentArtistId &&
    hadNoArtistsBeforeRefresh &&
    state.artists.length > 0;
  if (shouldAutoSelectFirstScannedArtist) {
    await selectArtist(state.artists[0].id, {history: 'replace'});
    logUiAction('refresh_current_view', {reason, auto_selected_artist: true});
    return seq;
  }
  if (currentArtistId) {
    state.currentArtist = state.artists.find(a => a.id === currentArtistId) || null;
    if (!state.currentArtist) {
      clearUI();
      syncBrowseUrl('replace');
      logUiAction('refresh_current_view', {reason, artist_missing: true});
      return seq;
    }
    state.activeFolder = activeFolder;
    const [stats, tags, folders] = await Promise.all([
      API.get(`/api/artists/${currentArtistId}/stats`),
      API.get(`/api/tags?artist_id=${currentArtistId}`),
      API.get(`/api/folders?artist_id=${currentArtistId}`),
    ]);
    if (!isCurrentRequestSeq('scanRefreshSeq', seq) || artistChanged()) return seq;
    state.stats = stats;
    state.tags = tags;
    state.folders = folders;
    renderSidebar();
    renderFolderTree();
    renderEditTagPicker();
    renderToolbar();
    // After a finished scan, make newly scanned files visible on the first
    // implicit browse page: without an explicit sort or date range, switch the
    // implicit date_desc order to scanned_desc and mark it explicit so the
    // 全部 badge reflects the actual order. Explicit sorts and any scoped
    // browse state (folder, tag/role, search, duplicates) remain untouched.
    if (reason === 'scan_complete' && !state.itemSortExplicit && state.itemSort === 'date_desc' &&
        !state.itemDateFrom && !state.itemDateTo &&
        !state.activeFolder && !state.activeRole && !state.search && !state.duplicatesOnly) {
      state.itemSort = 'scanned_desc';
      state.itemSortExplicit = true;
      syncItemFilterControls();
    }
    await loadItemsPreservingDepth();
    if (artistChanged()) return seq;
    restoreGridScrollAnchor(gridScrollAnchor);
  }
  if (artistChanged()) return seq;
  syncBrowseUrl('replace');
  logUiAction('refresh_current_view', {reason});
  return seq;
}

// Late imports closing the events <-> router cycle.
import { selectArtist } from './router.js';
import { clearUI } from './views/sidebar.js';

// The all/duplicates views change as scan candidates finish hashing. Poll
// /api/hash/status while the user is looking at either view and silently
// reload it when progress moves; refreshCurrentView keeps scroll anchored.
let duplicatesViewRefreshTimer = null;

export function duplicatesViewRefreshEligible() {
  if (state.mode !== 'browse') return false;
  if (document.hidden) return false;
  const lightbox = $('#lightbox');
  if (lightbox && lightbox.style && lightbox.style.display && lightbox.style.display !== 'none') return false;
  return isDuplicateFilesScopeActive();
}

export function hashProgressFingerprint(status) {
  const items = status.items || {};
  const candidates = status.scan_candidates || {};
  return [
    items.pending, items.processing, items.done, items.error,
    candidates.pending, candidates.processing, candidates.done, candidates.error,
  ].join('/');
}

function hashCheckHasWork(status) {
  const items = status.items || {};
  const candidates = status.scan_candidates || {};
  return Boolean(
    Number(items.remaining || 0) + Number(candidates.remaining || 0) > 0
    || (status.worker && status.worker.thread_alive)
  );
}

export function scheduleDuplicatesViewRefresh(delay) {
  if (duplicatesViewRefreshTimer) clearTimeout(duplicatesViewRefreshTimer);
  const fallback = duplicatesViewRefreshEligible()
    ? DUPLICATES_VIEW_ACTIVE_REFRESH_MS
    : DUPLICATES_VIEW_IDLE_REFRESH_MS;
  duplicatesViewRefreshTimer = setTimeout(refreshDuplicatesViewAutomatically, delay != null ? delay : fallback);
}

export async function refreshDuplicatesViewAutomatically() {
  if (!duplicatesViewRefreshEligible() || state.duplicatesRefreshInFlight
      || state.loadingItems || state.loadingMoreItems) {
    scheduleDuplicatesViewRefresh();
    return;
  }
  state.duplicatesRefreshInFlight = true;
  let delay = DUPLICATES_VIEW_IDLE_REFRESH_MS;
  try {
    const status = await API.get('/api/hash/status');
    if (!duplicatesViewRefreshEligible()) return;
    delay = hashCheckHasWork(status) ? DUPLICATES_VIEW_ACTIVE_REFRESH_MS : DUPLICATES_VIEW_IDLE_REFRESH_MS;
    const fingerprint = hashProgressFingerprint(status);
    if (state.duplicatesRefreshFingerprint !== null && fingerprint !== state.duplicatesRefreshFingerprint) {
      await refreshCurrentView({reason: 'duplicates_hash_progress'});
    }
    state.duplicatesRefreshFingerprint = fingerprint;
    state.duplicatesRefreshFailures = 0;
  } catch (e) {
    state.duplicatesRefreshFailures += 1;
    if (state.duplicatesRefreshFailures === 3) {
      toast('重复文件自动刷新失败，稍后会继续尝试', 'error');
    }
    logUiAction('duplicates_refresh_failed', collectUiLogContext({error: e.message || String(e)}));
  } finally {
    state.duplicatesRefreshInFlight = false;
    scheduleDuplicatesViewRefresh(delay);
  }
}

// The 网盘下载 section under 下载. Everything here is one command the plan
// names; there is deliberately no global speed limit, stop-downloader or
// account management button, because those belong to the downloader itself.
function bindNetdiskPanel() {
  const saveBtn = $('#netdiskSaveBtn');
  if (saveBtn) saveBtn.addEventListener('click', saveNetdiskSettings);
  const testBtn = $('#netdiskTestBtn');
  if (testBtn) testBtn.addEventListener('click', testNetdiskConnection);
  const pathBtn = $('#netdiskPathCheckBtn');
  if (pathBtn) pathBtn.addEventListener('click', checkNetdiskPaths);
  const scriptBtn = $('#netdiskScriptBtn');
  if (scriptBtn) scriptBtn.addEventListener('click', generateNetdiskScript);
  const resetPairingBtn = $('#netdiskResetPairingBtn');
  if (resetPairingBtn) resetPairingBtn.addEventListener('click', resetNetdiskPairing);
  const scriptCopyBtn = $('#netdiskScriptCopyBtn');
  if (scriptCopyBtn) scriptCopyBtn.addEventListener('click', copyNetdiskScript);
  const disconnectBtn = $('#netdiskDisconnectBtn');
  const connectBtn = $('#netdiskConnectBtn');
  if (connectBtn) connectBtn.addEventListener('click', connectNetdisk);
  if (disconnectBtn) disconnectBtn.addEventListener('click', disconnectNetdisk);

  const resolveBtn = $('#netdiskDispatchResolveBtn');
  if (resolveBtn) resolveBtn.addEventListener('click', resolveNetdiskDispatchPost);
  const submitBtn = $('#netdiskDispatchSubmitBtn');
  if (submitBtn) submitBtn.addEventListener('click', submitNetdiskDispatch);
  const dispatchInput = $('#netdiskDispatchInput');
  if (dispatchInput) {
    dispatchInput.addEventListener('keydown', e => {
      if (e.key === 'Enter') {
        e.preventDefault();
        resolveNetdiskDispatchPost();
      }
    });
  }

  const panel = $('#netdiskPanel');
  if (panel) {
    // Same reason as the subscription settings: the page re-reads on a timer and
    // must not overwrite edits the user has not saved.
    for (const eventName of ['change', 'input']) {
      panel.addEventListener(eventName, e => {
        if (e?.target?.closest('#netdiskDispatchSection')) return;
        markNetdiskSettingsDirty();
      });
    }
    panel.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const tagBtn = target.closest('[data-netdisk-filter]');
      if (tagBtn && panel.contains(tagBtn)) {
        setNetdiskJobFilter(tagBtn.dataset.netdiskFilter || 'all');
        return;
      }
      const foldBtn = target.closest('#netdiskJobsFoldBtn');
      if (foldBtn && panel.contains(foldBtn)) {
        toggleNetdiskJobsFold();
        return;
      }
      const button = target.closest('[data-netdisk-job-action]');
      if (!button || !panel.contains(button)) return;
      runNetdiskJobAction(button.dataset.netdiskJob || '', button.dataset.netdiskJobAction);
    });
  }
}

export function bindEvents() {
  $('#mobileHeaderToggle').addEventListener('click', toggleMobileHeaderTools);
  syncMobileHeaderTools();
  syncSearchOptionsControl();
  $('#editModeBtn').addEventListener('click', () => {
    if (state.editMode || state.selectedIds.size > 0) {
      setEditMode(false);
    } else {
      setEditMode(true);
    }
  });
  syncEditModeButton();
  $('#mobileFilterBtn').addEventListener('click', openFilterDrawer);
  $('#filterBackdrop').addEventListener('click', closeFilterDrawer);
  $('#filterDrawerClose').addEventListener('click', closeFilterDrawer);
  bindSidebarResize();
  bindSidebarTagResize();
  bindSidebarSectionToggles();
  bindMobileColumnToggle();
  const cardRatioEl = $('#cardRatioSelect');
  if (cardRatioEl) {
    cardRatioEl.addEventListener('change', () => {
      setCardRatio(cardRatioEl.value, true);
    });
  }
  const themeToggleEl = $('#themeToggleBtn');
  if (themeToggleEl) {
    themeToggleEl.addEventListener('click', () => {
      toggleTheme();
    });
  }
  const themeModeEl = $('#themeModeSelect');
  if (themeModeEl) {
    themeModeEl.addEventListener('change', () => {
      setThemeMode(themeModeEl.value, true);
    });
  }
  syncItemFilterControls();
  $('#mediaFilter').addEventListener('change', e => selectBrowseRole(e.target.value));
  const tagSortEl = $('#tagSort');
  if (tagSortEl) {
    tagSortEl.value = getSavedTagSort();
    tagSortEl.addEventListener('change', () => {
      saveTagSort(tagSortEl.value);
      renderSidebar();
    });
  }
  $('#tagFilterReset').addEventListener('click', () => selectBrowseRole(null));
  const itemSortEl = $('#itemSort');
  if (itemSortEl) {
    itemSortEl.addEventListener('change', () => {
      state.itemSortExplicit = true;
      applyItemFilterControls();
    });
  }
  const itemDateFromEl = $('#itemDateFrom');
  if (itemDateFromEl) itemDateFromEl.addEventListener('change', applyItemFilterControls);
  const itemDateToEl = $('#itemDateTo');
  if (itemDateToEl) itemDateToEl.addEventListener('change', applyItemFilterControls);
  const itemDateResetEl = $('#itemDateReset');
  if (itemDateResetEl) {
    itemDateResetEl.addEventListener('click', () => {
      state.itemDateFrom = '';
      state.itemDateTo = '';
      saveItemDates('', '');
      syncItemFilterControls();
      state.selectedIds.clear();
      updateEditBar();
      scrollToItemsTop();
      syncBrowseUrl('push');
      loadItems();
    });
  }
  window.addEventListener('popstate', restoreBrowseUrl);
  bindLightboxVideoDiagnostics();
  // Seed the viewport layout tracker once at boot: a resize that crosses the
  // 768px breakpoint must re-render the browse grid into the other engine,
  // which requires knowing the viewport the page was rendered in.
  onViewportLayoutChange();
  window.addEventListener('resize', () => {
    setSidebarWidth(state.sidebarWidth, false);
    scheduleJustifiedRelayout();
    onViewportLayoutChange();
  });
  $('#artistSearch').addEventListener('focus', e => {
    e.target.select();
    renderArtistDropdown('');
  });
  $('#artistSearch').addEventListener('input', e => {
    renderArtistDropdown(e.target.value);
  });
  $('#artistSearch').addEventListener('keydown', e => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      moveArtistDropdownActive(e.key === 'ArrowDown' ? 1 : -1);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      selectFirstArtistResult();
    } else if (e.key === 'Escape') {
      closeArtistDropdown();
    }
  });
  $('#searchInput').addEventListener('input', syncClearSearch);
  $('#searchInput').addEventListener('input', debounce(e => {
    state.search = e.target.value;
    logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
    state.selectedIds.clear();
    updateEditBar();
    scrollToItemsTop();
    syncBrowseUrl('push');
    loadItems();
  }, 300));
  $('#searchInput').addEventListener('keydown', e => {
    if (e.key === 'Escape') {
      if (e.target.value) {
        e.preventDefault();
        clearBrowseSearch(e.target);
      } else {
        e.target.blur();
      }
    }
  });
  const clearSearchBtn = $('#clearSearchBtn');
  if (clearSearchBtn) {
    clearSearchBtn.addEventListener('click', () => {
      const input = $('#searchInput');
      clearBrowseSearch(input);
      if (input && typeof input.focus === 'function') input.focus();
    });
  }
  $('#searchOptionsBtn').addEventListener('click', e => {
    e.stopPropagation();
    toggleSearchOptions();
  });
  const searchOptionsMenu = $('#searchOptionsMenu');
  if (searchOptionsMenu) {
    searchOptionsMenu.addEventListener('click', e => {
      const btn = e.target instanceof Element ? e.target.closest('[data-search-scope]') : null;
      if (!btn || !searchOptionsMenu.contains(btn)) return;
      e.stopPropagation();
      setSearchScope(btn.dataset.searchScope);
      logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
      state.selectedIds.clear();
      updateEditBar();
      scrollToItemsTop();
      syncBrowseUrl('push');
      loadItems();
    });
  }
  $('#tagsOnlyToggle').addEventListener('change', e => {
    setSearchTarget(e.target.checked ? 'tags' : 'all');
    logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
    state.selectedIds.clear();
    updateEditBar();
    scrollToItemsTop();
    syncBrowseUrl('push');
    loadItems();
  });
  const duplicateFilesToggle = $('#duplicateFilesToggle');
  if (duplicateFilesToggle) {
    duplicateFilesToggle.addEventListener('click', e => {
      const btn = e.target instanceof Element ? e.target.closest('[data-duplicates]') : null;
      if (!btn || !duplicateFilesToggle.contains(btn)) return;
      const duplicatesOnly = btn.dataset.duplicates === 'duplicates';
      if (state.duplicatesOnly === duplicatesOnly) return;
      state.duplicatesOnly = duplicatesOnly;
      updateDuplicateFilesButton();
      state.selectedIds.clear();
      updateEditBar();
      scrollToItemsTop();
      syncBrowseUrl('push');
      loadItems();
    });
  }
  const gridContainer = $('#gridContainer');
  if (gridContainer) gridContainer.addEventListener('scroll', maybeLoadMoreOnScroll, {passive: true});
  window.addEventListener('scroll', maybeLoadMoreOnScroll, {passive: true});
  $('#scanFolderBtn').addEventListener('click', async () => {
    if (!state.currentArtist || !isCurrentScanScopeActive() || isActionBusy('scan-context')) return;
    setActionBusy('scan-context', '', true);
    const isFolderScan = Boolean(state.activeFolder);
    const params = new URLSearchParams();
    params.set('artist_id', state.currentArtist.id);
    if (state.activeFolder) params.set('folder', state.activeFolder);
    try {
      const r = await API.post('/api/scan/folder?' + params.toString());
      if (r.ok) toast(isFolderScan ? '文件夹扫描已启动' : '画师扫描已启动', 'success');
      else toast(r.message || '扫描已在运行', 'error');
    } catch (e) {
      // 409: another scan or file operation owns the slot.
      if (e && e.status === 409) {
        toast('扫描已在运行', 'error');
      } else {
        toast(isFolderScan ? '启动文件夹扫描失败' : '启动画师扫描失败', 'error');
      }
    } finally {
      setActionBusy('scan-context', '', false);
    }
  });

  const maintenanceBtn = $('#maintenanceBtn');
  if (maintenanceBtn) {
    maintenanceBtn.addEventListener('click', () => {
      const nextMode = state.mode === 'moves' ? 'browse' : 'moves';
      // Edit mode belongs to the browse grid; the maintenance panel has
      // nothing to select, so leaving for it ends the session instead of
      // stranding the check marks behind it.
      if (nextMode === 'moves') setEditMode(false);
      applyMode(nextMode);
      syncBrowseUrl('push');
    });
  }

  const browseReturnBtn = $('#browseReturnBtn');
  if (browseReturnBtn) {
    browseReturnBtn.addEventListener('click', () => {
      const returnTarget = state.returnToView || 'downloads';
      state.returnToView = null;
      updateBrowseReturnButton();
      applyMode('moves');
      setMaintenanceView(returnTarget);
      syncBrowseUrl('push');
    });
  }

  const desktopViewToggle = $('#desktopViewToggle');
  if (desktopViewToggle) {
    desktopViewToggle.addEventListener('click', e => {
      const btn = e.target instanceof Element ? e.target.closest('button[data-view]') : null;
      if (!btn || !desktopViewToggle.contains(btn)) return;
      $$('#desktopViewToggle button').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      $$('#desktopViewToggle button').forEach(b => b.setAttribute('aria-pressed', b === btn ? 'true' : 'false'));
      state.view = btn.dataset.view;
      syncBrowseUrl('push');
      renderGrid();
    });
  }

  const desktopRatioToggle = $('#desktopRatioToggle');
  if (desktopRatioToggle) {
    desktopRatioToggle.addEventListener('click', e => {
      const btn = e.target instanceof Element ? e.target.closest('button[data-card-ratio]') : null;
      if (!btn || !desktopRatioToggle.contains(btn)) return;
      setCardRatio(btn.dataset.cardRatio, true);
    });
  }

  $('#scanBtn').addEventListener('click', startFullScan);
  $('#emptyScanBtn').addEventListener('click', startFullScan);
  const emptySelectArtistBtn = $('#emptySelectArtistBtn');
  if (emptySelectArtistBtn) {
    emptySelectArtistBtn.addEventListener('click', e => {
      // Keep the open dropdown from the same click; document click would
      // otherwise see a target outside #artistPicker and close it immediately.
      e.preventDefault();
      e.stopPropagation();
      focusArtistPicker();
    });
  }

  $('#stopScanBtn').addEventListener('click', async () => {
    if (isActionBusy('scan-stop')) return;
    setActionBusy('scan-stop', '', true);
    try {
      const r = await API.post('/api/scan/stop');
      if (r.ok) toast('正在停止扫描', 'success');
      else toast(r.message || '停止失败', 'error');
    } catch (e) {
      toast('停止失败', 'error');
    } finally {
      setActionBusy('scan-stop', '', false);
    }
  });

  $('#moveRefreshBtn').addEventListener('click', () => {
    loadMoveWorkbench({preserveScroll: true}).catch(e => {
      if (!isAbortError(e)) toast('维护页面刷新失败：' + (e.message || e), 'error');
    });
  });
  $('#moveAutoResolveBtn').addEventListener('click', autoResolveMoveCandidates);
  const folderRenameRunAllBtn = $('#folderRenameRunAllBtn');
  if (folderRenameRunAllBtn) folderRenameRunAllBtn.addEventListener('click', runFolderRenameAllNow);
  const folderRenameAutoToggle = $('#folderRenameAutoExecuteToggle');
  if (folderRenameAutoToggle) {
    folderRenameAutoToggle.addEventListener('change', e => {
      setFolderRenameAutoEnabled(e.target.checked);
    });
  }
  const organizeArtistSelect = $('#organizeArtistSelect');
  if (organizeArtistSelect) organizeArtistSelect.addEventListener('change', e => switchOrganizeArtist(e.target.value));
  const organizeArtistPrevBtn = $('#organizeArtistPrevBtn');
  if (organizeArtistPrevBtn) organizeArtistPrevBtn.addEventListener('click', () => stepOrganizeArtist('prev'));
  const organizeArtistNextBtn = $('#organizeArtistNextBtn');
  if (organizeArtistNextBtn) organizeArtistNextBtn.addEventListener('click', () => stepOrganizeArtist('next'));
  const archiveProfileSaveBtn = $('#archiveProfileSaveBtn');
  if (archiveProfileSaveBtn) archiveProfileSaveBtn.addEventListener('click', saveArchiveSettings);
  const archiveTemplateInput = $('#archiveTemplateInput');
  if (archiveTemplateInput) {
    archiveTemplateInput.addEventListener('input', () => {
      syncArchiveRuleDirtyState();
      scheduleArchiveDraftPreview();
    });
  }
  const archiveCollisionSelect = $('#archiveCollisionSelect');
  if (archiveCollisionSelect) {
    archiveCollisionSelect.addEventListener('change', () => {
      syncArchiveRuleDirtyState();
      scheduleArchiveDraftPreview();
    });
  }
  const archivePlansRefreshBtn = $('#archivePlansRefreshBtn');
  if (archivePlansRefreshBtn) archivePlansRefreshBtn.addEventListener('click', () => refreshArchivePlans());
  const archivePlansPreviewBtn = $('#archivePlansPreviewBtn');
  if (archivePlansPreviewBtn) archivePlansPreviewBtn.addEventListener('click', previewArchivePlans);
  const archivePlansApplyBtn = $('#archivePlansApplyBtn');
  if (archivePlansApplyBtn) archivePlansApplyBtn.addEventListener('click', applyArchiveTemplate);
  const archivePlansConfirmAllBtn = $('#archivePlansConfirmAllBtn');
  if (archivePlansConfirmAllBtn) archivePlansConfirmAllBtn.addEventListener('click', toggleAllArchivePlansConfirmation);
  $$('.archive-token-strip [data-insert-token]').forEach(token => {
    token.addEventListener('click', () => {
      const input = $('#archiveTemplateInput');
      if (!input || input.disabled) return;
      const value = token.dataset.insertToken || token.textContent || '';
      const start = input.selectionStart ?? input.value.length;
      const end = input.selectionEnd ?? input.value.length;
      input.value = input.value.slice(0, start) + value + input.value.slice(end);
      input.focus();
      input.setSelectionRange(start + value.length, start + value.length);
      syncArchiveRuleDirtyState();
      scheduleArchiveDraftPreview();
    });
  });
  const archivePlansDryRunBtn = $('#archivePlansDryRunBtn');
  if (archivePlansDryRunBtn) archivePlansDryRunBtn.addEventListener('click', () => executeArchivePlans(true));
  const archivePlansExecuteBtn = $('#archivePlansExecuteBtn');
  if (archivePlansExecuteBtn) archivePlansExecuteBtn.addEventListener('click', () => executeArchivePlans(false));
  const archivePlanList = $('#archivePlanList');
  if (archivePlanList) archivePlanList.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    if (!target) return;
    const undo = target.closest('[data-archive-plan-undo]');
    if (undo && archivePlanList.contains(undo)) return void undoArchivePlan(undo.dataset.archivePlanUndo);
    const confirm = target.closest('[data-archive-plan-confirm]');
    if (confirm && archivePlanList.contains(confirm)) return void toggleArchivePlanConfirmation(confirm.dataset.archivePlanConfirm);
    const jump = target.closest('[data-archive-plan-jump]');
    if (jump && archivePlanList.contains(jump)) return void jumpToArchivePlanFolder(jump.dataset.archivePlanJump);
  });
  const archivePlansFoldBtn = $('#archivePlansFoldBtn');
  if (archivePlansFoldBtn) archivePlansFoldBtn.addEventListener('click', toggleArchivePlansFold);
  bindNetdiskPanel();
  const downloadSettingsSaveBtn = $('#downloadSettingsSaveBtn');
  if (downloadSettingsSaveBtn) downloadSettingsSaveBtn.addEventListener('click', saveDownloadsSettings);
  const downloadCheckBtn = $('#downloadCheckBtn');
  if (downloadCheckBtn) downloadCheckBtn.addEventListener('click', checkDownloadMissing);
  const downloadReconcileBtn = $('#downloadReconcileBtn');
  if (downloadReconcileBtn) downloadReconcileBtn.addEventListener('click', reconcileDownloadLibrary);
  const downloadLogRefreshBtn = $('#downloadLogRefreshBtn');
  if (downloadLogRefreshBtn) downloadLogRefreshBtn.addEventListener('click', refreshDownloadLog);
  const downloadLogFoldBtn = $('#downloadLogFoldBtn');
  if (downloadLogFoldBtn) downloadLogFoldBtn.addEventListener('click', toggleDownloadLogFold);
  const downloadSubSearch = $('#downloadSubscriptionSearch');
  if (downloadSubSearch) {
    downloadSubSearch.addEventListener('input', e => {
      state.downloadSubscriptionSearch = e.target.value;
      renderDownloadSubscriptions();
    });
  }
  const downloadSettingsPanel = $('#downloadSettingsPanel');
  if (downloadSettingsPanel) {
    downloadSettingsPanel.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const segment = target?.closest('[data-download-type]');
      if (segment && downloadSettingsPanel.contains(segment)) {
        toggleDownloadType(segment.dataset.downloadType);
      }
    });
    // Any hand edit to the switches, interval or template fields marks the form
    // dirty, so the page auto-refresh stops repopulating it from the server.
    for (const eventName of ['change', 'input']) {
      downloadSettingsPanel.addEventListener(eventName, () => markDownloadSettingsDirty());
    }
  }
  const downloadTemplateResetBtn = $('#downloadTemplateResetBtn');
  if (downloadTemplateResetBtn) downloadTemplateResetBtn.addEventListener('click', resetDownloadTemplates);
  const downloadSubscriptionForm = $('#downloadSubscriptionForm');
  if (downloadSubscriptionForm) {
    downloadSubscriptionForm.addEventListener('submit', e => {
      e.preventDefault();
      addDownloadSubscription();
    });
  }
  const downloadSyncBtn = $('#downloadSyncBtn');
  if (downloadSyncBtn) downloadSyncBtn.addEventListener('click', startDownloadSync);
  const downloadArtistInput = $('#downloadSubscriptionArtistInput');
  if (downloadArtistInput) {
    downloadArtistInput.addEventListener('focus', openDownloadArtistCombo);
    downloadArtistInput.addEventListener('input', () => {
      // Typing clears a previous pick: the box is a search again, not a name.
      const hidden = $('#downloadSubscriptionArtistSelect');
      if (hidden) hidden.value = '';
      openDownloadArtistCombo();
    });
    downloadArtistInput.addEventListener('keydown', e => {
      if (e.key === 'Escape') closeDownloadArtistCombo();
    });
  }
  const downloadArtistListbox = $('#downloadSubscriptionArtistListbox');
  if (downloadArtistListbox) {
    downloadArtistListbox.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const option = target?.closest('[data-download-artist-value]');
      if (option && downloadArtistListbox.contains(option)) {
        pickDownloadArtist(option.dataset.downloadArtistValue);
        renderDownloadArtistCombo();
      }
    });
  }
  const downloadSubscriptionRoot = $('#downloadSubscriptionRoot');
  if (downloadSubscriptionRoot) {
    // A different root invalidates the picked parent: a relative path means a
    // different folder under a different root.
    downloadSubscriptionRoot.addEventListener('change', () => {
      state.downloadArtistRootIndex = Number(downloadSubscriptionRoot.value) || 0;
      state.downloadArtistParentPath = '';
      renderDownloadArtistFolder();
    });
  }
  const downloadSubscriptionFolderBrowseBtn = $('#downloadSubscriptionFolderBrowseBtn');
  if (downloadSubscriptionFolderBrowseBtn) {
    downloadSubscriptionFolderBrowseBtn.addEventListener('click', () => openDirectoryPicker('downloadArtist', downloadSubscriptionFolderBrowseBtn));
  }
  document.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    if (!target?.closest('#downloadWorksArtistCombo')) closeDownloadWorksArtistCombo();
    if (!target?.closest('.download-subscription-artist .download-artist-combo')) closeDownloadArtistCombo();
  });
  // Remember which template input the variable chips should insert into: the
  // chips sit below all three fields, so the caret target is ambiguous.
  for (const inputId of ['folder', 'image', 'attachment']) {
    const selector = downloadTemplateInputSelector(inputId);
    const input = selector ? $(selector) : null;
    if (input) input.addEventListener('focus', () => rememberDownloadTemplateInput(inputId));
  }
  $$('.download-token-strip [data-download-token]').forEach(token => {
    token.addEventListener('click', () => {
      insertDownloadTemplateToken(token.dataset.downloadToken || token.textContent || '');
    });
  });
  const downloadSubscriptionList = $('#downloadSubscriptionList');
  if (downloadSubscriptionList) {
    // A day cell is a span with `role="button"`, so it needs the key handling a
    // real button would have given it for free.
    const activateDay = target => {
      const day = target.closest('[data-download-day]');
      if (!day || !downloadSubscriptionList.contains(day)) return false;
      const item = day.closest('[data-download-subscription]');
      if (!item) return false;
      openDownloadDay(item.dataset.downloadSubscription, day.dataset.downloadDay);
      return true;
    };
    downloadSubscriptionList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const remove = target.closest('[data-download-subscription-delete]');
      if (remove && downloadSubscriptionList.contains(remove)) {
        deleteDownloadSubscription(remove.dataset.downloadSubscriptionDelete);
        return;
      }
      // Per-artist halves of 立即操作: same rounds, one subscription.
      const subCheck = target.closest('[data-download-subscription-check]');
      if (subCheck && downloadSubscriptionList.contains(subCheck)) {
        checkSubscriptionMissing(subCheck.dataset.downloadSubscriptionCheck);
        return;
      }
      const subReconcile = target.closest('[data-download-subscription-reconcile]');
      if (subReconcile && downloadSubscriptionList.contains(subReconcile)) {
        reconcileSubscriptionLibrary(subReconcile.dataset.downloadSubscriptionReconcile);
        return;
      }
      const decision = target.closest('[data-download-post-decision]');
      if (decision && downloadSubscriptionList.contains(decision)) {
        decideDownloadPost(decision.dataset.downloadPostId, decision.dataset.downloadPostDecision);
        return;
      }
      const bind = target.closest('[data-download-post-candidate-bind]');
      if (bind && downloadSubscriptionList.contains(bind)) {
        bindDownloadCandidate(bind.dataset.downloadPostCandidateBind, bind.dataset.downloadCandidatePath);
        return;
      }
      // 停止 / 单文件重试 / 手动导入. Each is one work's own instruction, so
      // they sit next to the work rather than in the panel header.
      const stopPost = target.closest('[data-download-post-stop]');
      if (stopPost && downloadSubscriptionList.contains(stopPost)) {
        stopDownloadPost(stopPost.dataset.downloadPostStop);
        return;
      }
      const importPost = target.closest('[data-download-post-import]');
      if (importPost && downloadSubscriptionList.contains(importPost)) {
        importDownloadPost(importPost.dataset.downloadPostImport);
        return;
      }
      const postFiles = target.closest('[data-download-post-files]');
      if (postFiles && downloadSubscriptionList.contains(postFiles)) {
        openDownloadPostFiles(postFiles.dataset.downloadPostFiles);
        return;
      }
      const fileRetry = target.closest('[data-download-file-retry]');
      if (fileRetry && downloadSubscriptionList.contains(fileRetry)) {
        retryDownloadFile(fileRetry.dataset.downloadFilePost, fileRetry.dataset.downloadFileRetry);
        return;
      }
      const verify = target.closest('[data-download-post-verify]');
      if (verify && downloadSubscriptionList.contains(verify)) {
        verifyDownloadPost(verify.dataset.downloadPostVerify);
        return;
      }
      const candidates = target.closest('[data-download-post-candidates]');
      if (candidates && downloadSubscriptionList.contains(candidates)) {
        loadDownloadCandidates(candidates.dataset.downloadPostCandidates);
        return;
      }
      if (target.closest('[data-download-day-more]')) {
        loadMoreDownloadDayPosts();
        return;
      }
      activateDay(target);
    });
    downloadSubscriptionList.addEventListener('keydown', e => {
      if (e.key !== 'Enter' && e.key !== ' ') return;
      const target = e.target instanceof Element ? e.target : null;
      if (!target || !activateDay(target)) return;
      e.preventDefault();
    });
    downloadSubscriptionList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const fetch = target.closest('[data-download-post-fetch]');
      if (fetch && downloadSubscriptionList.contains(fetch)) {
        downloadPostByHand(fetch.dataset.downloadPostFetch);
      }
    });
    downloadSubscriptionList.addEventListener('change', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const mode = target.closest('[data-download-subscription-mode]');
      if (mode && downloadSubscriptionList.contains(mode)) {
        setDownloadSubscriptionMode(mode.dataset.downloadSubscriptionMode, mode.value);
        return;
      }
      const toggle = target.closest('[data-download-subscription-toggle]');
      if (toggle && downloadSubscriptionList.contains(toggle)) {
        toggleDownloadSubscription(toggle.dataset.downloadSubscriptionToggle, toggle.checked);
      }
    });
  }
  const downloadWorksPanel = $('#downloadWorksPanel');
  if (downloadWorksPanel) {
    downloadWorksPanel.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const pick = target.closest('[data-download-works-pick]');
      if (pick && downloadWorksPanel.contains(pick)) {
        toggleDownloadWorkSelection(pick.dataset.downloadWorksPick, pick.checked);
        return;
      }
      const dayJump = target.closest('[data-download-works-jump-day]');
      if (dayJump && downloadWorksPanel.contains(dayJump)) {
        jumpToDownloadWorkFolder(
          dayJump.dataset.downloadWorksJumpArtist,
          dayJump.dataset.downloadWorksJumpDay,
          dayJump.dataset.downloadWorksJumpTitle || ''
        );
        return;
      }
      const fetch = target.closest('[data-download-works-fetch]');
      if (fetch && downloadWorksPanel.contains(fetch)) {
        downloadWorksPost(fetch.dataset.downloadWorksFetch);
        return;
      }
      const netdisk = target.closest('[data-download-works-netdisk]');
      if (netdisk && downloadWorksPanel.contains(netdisk)) {
        deliverPostToNetdisk(netdisk.dataset.downloadWorksNetdisk);
        return;
      }
    });
    const downloadWorksList = $('#downloadWorksList');
    if (downloadWorksList) {
      downloadWorksList.addEventListener('scroll', () => {
        if (downloadWorksList.scrollTop + downloadWorksList.clientHeight >= downloadWorksList.scrollHeight - 150) {
          if (!state.downloadAllWorks?.loading && state.downloadAllWorks?.next_cursor) {
            loadMoreDownloadWorks();
          }
        }
      });
    }
    const downloadWorksArtistInput = $('#downloadWorksArtistInput');
    if (downloadWorksArtistInput) {
      downloadWorksArtistInput.addEventListener('focus', openDownloadWorksArtistCombo);
      downloadWorksArtistInput.addEventListener('input', () => {
        const hidden = $('#downloadWorksArtist');
        if (hidden && !downloadWorksArtistInput.value.trim()) {
          hidden.value = '';
          applyDownloadWorksFilter({artistId: null});
        }
        openDownloadWorksArtistCombo();
      });
      downloadWorksArtistInput.addEventListener('keydown', e => {
        if (e.key === 'Escape') closeDownloadWorksArtistCombo();
      });
    }
    const downloadWorksArtistListbox = $('#downloadWorksArtistListbox');
    if (downloadWorksArtistListbox) {
      downloadWorksArtistListbox.addEventListener('click', e => {
        const target = e.target instanceof Element ? e.target : null;
        const option = target?.closest('[data-download-works-artist-value]');
        if (option && downloadWorksArtistListbox.contains(option)) {
          pickDownloadWorksArtist(option.dataset.downloadWorksArtistValue, option.textContent);
        }
      });
    }
    downloadWorksPanel.addEventListener('change', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const pick = target.closest('[data-download-works-pick]');
      if (pick && downloadWorksPanel.contains(pick)) {
        toggleDownloadWorkSelection(pick.dataset.downloadWorksPick, pick.checked);
        return;
      }
      const selectAll = target.closest('#downloadWorksSelectAll');
      if (selectAll && downloadWorksPanel.contains(selectAll)) {
        toggleDownloadWorksSelectAll(selectAll.checked);
        return;
      }
      const artist = target.closest('#downloadWorksArtist');
      if (artist && downloadWorksPanel.contains(artist)) {
        applyDownloadWorksFilter({artistId: artist.value ? Number(artist.value) : null});
        return;
      }
      const day = target.closest('#downloadWorksDay');
      if (day && downloadWorksPanel.contains(day)) {
        applyDownloadWorksFilter({day: day.value || ''});
        return;
      }
      const stateFilter = target.closest('#downloadWorksState');
      if (stateFilter && downloadWorksPanel.contains(stateFilter)) {
        applyDownloadWorksFilter({state: stateFilter.value || ''});
      }
    });
    downloadWorksPanel.addEventListener('input', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      // The search box re-reads after the typing stops; the other controls are
      // discrete and commit on `change`.
      if (target.closest('#downloadWorksSearch')) scheduleDownloadWorksSearch(target.value);
    });
  }
  const downloadWorksClearBtn = $('#downloadWorksClearBtn');
  if (downloadWorksClearBtn) {
    downloadWorksClearBtn.addEventListener('click', () => clearDownloadWorksSelection());
  }
  const downloadWorksDownloadBtn = $('#downloadWorksDownloadBtn');
  if (downloadWorksDownloadBtn) {
    downloadWorksDownloadBtn.addEventListener('click', () => downloadWorksSelection(downloadWorksSelected()));
  }
  const recycleRefreshBtn = $('#recycleRefreshBtn');
  if (recycleRefreshBtn) recycleRefreshBtn.addEventListener('click', () => loadRecycleBin());
  const recycleClearBtn = $('#recycleClearBtn');
  if (recycleClearBtn) recycleClearBtn.addEventListener('click', () => clearRecycleBin());
  const operationHistoryPanel = $('#operationHistoryPanel');
  if (operationHistoryPanel) {
    operationHistoryPanel.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const btn = target.closest('[data-operation-filter]');
      if (btn && operationHistoryPanel.contains(btn)) {
        setOperationHistoryFilter(btn.dataset.operationFilter || 'all');
        return;
      }
      const foldBtn = target.closest('#operationSuccessFoldBtn');
      if (foldBtn && operationHistoryPanel.contains(foldBtn)) {
        toggleOperationHistoryFold();
      }
    });
  }
  const artistFolderMoveForm = $('#artistFolderMoveForm');
  if (artistFolderMoveForm) artistFolderMoveForm.addEventListener('submit', e => { e.preventDefault(); previewArtistFolderMove(); });
  const artistFolderMovePreviewBtn = $('#artistFolderMovePreviewBtn');
  if (artistFolderMovePreviewBtn) artistFolderMovePreviewBtn.addEventListener('click', previewArtistFolderMove);
  const artistFolderMoveRoot = $('#artistFolderMoveRoot');
  if (artistFolderMoveRoot) artistFolderMoveRoot.addEventListener('change', () => {
    // 父目录相对于所选根目录：换根目录即清空旧父目录与预览（P6）。
    state.artistFolderMoveParentPath = '';
    invalidateArtistFolderMovePreview();
  });
  const artistFolderMoveDestination = $('#artistFolderMoveDestination');
  if (artistFolderMoveDestination) artistFolderMoveDestination.addEventListener('input', invalidateArtistFolderMovePreview);
  const artistFolderMoveBrowseBtn = $('#artistFolderMoveBrowseBtn');
  if (artistFolderMoveBrowseBtn) artistFolderMoveBrowseBtn.addEventListener('click', () => openDirectoryPicker('artistMove', artistFolderMoveBrowseBtn));
  const artistFolderMoveExecuteBtn = $('#artistFolderMoveExecuteBtn');
  if (artistFolderMoveExecuteBtn) artistFolderMoveExecuteBtn.addEventListener('click', executeArtistFolderMove);
  const artistFolderMoveDirectoryDialog = $('#artistFolderMoveDirectoryDialog');
  if (artistFolderMoveDirectoryDialog) {
    artistFolderMoveDirectoryDialog.addEventListener('cancel', event => { event.preventDefault(); closeDirectoryPicker(); });
    artistFolderMoveDirectoryDialog.addEventListener('click', event => { if (event.target === artistFolderMoveDirectoryDialog) closeDirectoryPicker(); });
  }
  const artistFolderMoveDirectoryCloseBtn = $('#artistFolderMoveDirectoryCloseBtn');
  if (artistFolderMoveDirectoryCloseBtn) artistFolderMoveDirectoryCloseBtn.addEventListener('click', closeDirectoryPicker);
  const artistFolderMoveDirectoryUpBtn = $('#artistFolderMoveDirectoryUpBtn');
  if (artistFolderMoveDirectoryUpBtn) artistFolderMoveDirectoryUpBtn.addEventListener('click', () => loadDirectoryPicker(state.directoryPickerPath.split('/').slice(0, -1).join('/')));
  const artistFolderMoveDirectorySelectBtn = $('#artistFolderMoveDirectorySelectBtn');
  if (artistFolderMoveDirectorySelectBtn) artistFolderMoveDirectorySelectBtn.addEventListener('click', chooseDirectoryPicker);
  const artistFolderMoveDirectoryList = $('#artistFolderMoveDirectoryList');
  if (artistFolderMoveDirectoryList) artistFolderMoveDirectoryList.addEventListener('click', event => {
    const target = event.target instanceof Element ? event.target.closest('[data-artist-folder-directory]') : null;
    if (target && artistFolderMoveDirectoryList.contains(target)) loadDirectoryPicker([state.directoryPickerPath, target.dataset.artistFolderDirectory].filter(Boolean).join('/'));
  });
  const recycleBinList = $('#recycleBinList');
  if (recycleBinList) recycleBinList.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    const restore = target ? target.closest('[data-recycle-restore]') : null;
    if (restore && recycleBinList.contains(restore)) return void restoreRecycleEntry(restore.dataset.recycleRestore);
    const purge = target ? target.closest('[data-recycle-purge]') : null;
    if (purge && recycleBinList.contains(purge)) return void purgeRecycleEntry(purge.dataset.recyclePurge);
  });
  const recycleBinMore = $('#recycleBinMore');
  if (recycleBinMore) recycleBinMore.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    const more = target ? target.closest('[data-recycle-load-more]') : null;
    if (more && recycleBinMore.contains(more)) loadRecycleBin({append: true});
  });
  const characterImportBtn = $('#characterImportBtn');
  if (characterImportBtn) characterImportBtn.addEventListener('click', () => {
    const scope = $('#characterImportScopeSelect')?.value === 'all' ? 'all' : 'artist';
    if (scope === 'artist' && !state.currentArtist) {
      toast('请先在上方选择画师', 'error');
      return;
    }
    importCharacterLibraryReferences({
      scope,
      body: scope === 'artist'
        ? {artist_id: state.currentArtist.id, limit_per_tag: 3}
        : {limit_per_tag: 3},
    });
  });
  $('#characterRebuildIndexBtn').addEventListener('click', rebuildCharacterIndex);

  const characterCreateBtn = $('#characterCreateBtn');
  const characterCreateBox = $('#characterCreateBox');
  const characterCreateInput = $('#characterCreateInput');
  const characterCreateConfirmBtn = $('#characterCreateConfirmBtn');
  const characterCreateCancelBtn = $('#characterCreateCancelBtn');

  function openCharacterCreate() {
    if (!characterCreateBox || !characterCreateInput) return;
    characterCreateBox.hidden = false;
    characterCreateInput.value = '';
    characterCreateInput.focus();
  }

  function closeCharacterCreate() {
    if (!characterCreateBox || !characterCreateInput) return;
    characterCreateBox.hidden = true;
    characterCreateInput.value = '';
  }

  async function handleCharacterCreate() {
    if (!characterCreateInput) return;
    const name = characterCreateInput.value.trim();
    if (!name) {
      toast('请输入角色名称', 'warning');
      characterCreateInput.focus();
      return;
    }
    const result = await createCharacter(name);
    if (result) {
      closeCharacterCreate();
    }
  }

  if (characterCreateBtn) {
    characterCreateBtn.addEventListener('click', () => {
      if (characterCreateBox && !characterCreateBox.hidden) {
        closeCharacterCreate();
      } else {
        openCharacterCreate();
      }
    });
  }
  if (characterCreateConfirmBtn) {
    characterCreateConfirmBtn.addEventListener('click', handleCharacterCreate);
  }
  if (characterCreateCancelBtn) {
    characterCreateCancelBtn.addEventListener('click', closeCharacterCreate);
  }
  if (characterCreateInput) {
    characterCreateInput.addEventListener('keydown', e => {
      if (e.key === 'Enter') {
        e.preventDefault();
        handleCharacterCreate();
      } else if (e.key === 'Escape') {
        e.preventDefault();
        closeCharacterCreate();
      }
    });
  }
  const characterImportScopeSelect = $('#characterImportScopeSelect');
  if (characterImportScopeSelect) {
    characterImportScopeSelect.addEventListener('change', () => renderCharacterLibrary());
  }
  const characterLibrarySearchInput = $('#characterLibrarySearchInput');
  if (characterLibrarySearchInput) {
    characterLibrarySearchInput.addEventListener('input', e => {
      state.characterLibrarySearchQuery = e.target.value;
      renderCharacterLibrary();
    });
    characterLibrarySearchInput.addEventListener('keydown', e => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        e.target.value = '';
        state.characterLibrarySearchQuery = '';
        renderCharacterLibrary();
      }
    });
  }
  const characterLibrarySearchClearBtn = $('#characterLibrarySearchClearBtn');
  if (characterLibrarySearchClearBtn) {
    characterLibrarySearchClearBtn.addEventListener('click', () => {
      if (characterLibrarySearchInput) characterLibrarySearchInput.value = '';
      state.characterLibrarySearchQuery = '';
      renderCharacterLibrary();
      if (characterLibrarySearchInput) characterLibrarySearchInput.focus();
    });
  }
  const characterTagImportList = $('#characterTagImportList');
  if (characterTagImportList) {
    characterTagImportList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const cancelBtn = target ? target.closest('[data-character-import-cancel]') : null;
      if (cancelBtn && characterTagImportList.contains(cancelBtn)) {
        cancelCharacterImportJob(cancelBtn.dataset.characterImportCancel);
        return;
      }
    });
  }
  const characterImportJobContainer = $('#characterImportJobContainer');
  if (characterImportJobContainer) {
    characterImportJobContainer.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const cancelBtn = target ? target.closest('[data-character-import-cancel]') : null;
      if (cancelBtn && characterImportJobContainer.contains(cancelBtn)) {
        cancelCharacterImportJob(cancelBtn.dataset.characterImportCancel);
        return;
      }
    });
  }
  const characterList = $('#characterList');
  if (characterList) {
    characterList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const deleteBtn = target ? target.closest('[data-character-delete]') : null;
      if (deleteBtn && characterList.contains(deleteBtn)) {
        deleteCharacter(Number(deleteBtn.dataset.characterDelete));
        return;
      }
      const searchBtn = target ? target.closest('[data-character-search]') : null;
      if (searchBtn && characterList.contains(searchBtn)) {
        const query = searchBtn.dataset.characterSearch;
        if (query) {
          applyMode('browse');
          const searchInput = $('#searchInput');
          if (searchInput) searchInput.value = query;
          state.searchQuery = query;
          syncClearSearch();
          loadItems({reset: true});
        }
        return;
      }
      const shell = target ? target.closest('.character-card-shell') : null;
      const btn = target ? (target.closest('[data-character-select]') || shell?.querySelector('[data-character-select]')) : null;
      if (!btn || !characterList.contains(btn)) return;
      const characterId = Number(btn.dataset.characterSelect);
      if (!characterId) return;
      state.characterLibrarySelectedCharacterId = characterId;
      openCharacterReferences();
      loadCharacterLibrary({characterId});
    });
  }
  // Empty-state shortcut: no character yet, so offer the import zone directly
  // instead of leaving the user in a dead list.
  if (characterList) {
    characterList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const gotoBtn = target ? target.closest('[data-character-library-goto]') : null;
      if (gotoBtn && characterList.contains(gotoBtn)) {
        gotoCharacterLibraryPanel(gotoBtn.dataset.characterLibraryGoto);
        return;
      }
      const createTrigger = target ? target.closest('[data-character-create-trigger]') : null;
      if (createTrigger && characterList.contains(createTrigger)) {
        openCharacterCreate();
        return;
      }
    });
  }
  const characterLibraryViews = $('#characterLibraryViews');
  if (characterLibraryViews) {
    characterLibraryViews.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-character-library-view]') : null;
      if (!btn || !characterLibraryViews.contains(btn)) return;
      setCharacterLibraryMobileView(btn.dataset.characterLibraryView);
    });
  }
  const characterLibraryBackBtn = $('#characterLibraryBackBtn');
  if (characterLibraryBackBtn) {
    characterLibraryBackBtn.addEventListener('click', () => {
      setCharacterLibraryMobileView('characters');
    });
  }
  const characterReferenceUploadBtn = $('#characterReferenceUploadBtn');
  const characterReferenceUploadInput = $('#characterReferenceUploadInput');
  if (characterReferenceUploadBtn && characterReferenceUploadInput) {
    characterReferenceUploadBtn.addEventListener('click', () => {
      if (!state.characterLibrarySelectedCharacterId) {
        toast('请先选择角色', 'error');
        return;
      }
      characterReferenceUploadInput.click();
    });
    characterReferenceUploadInput.addEventListener('change', () => {
      const file = characterReferenceUploadInput.files && characterReferenceUploadInput.files[0];
      // Clear before uploading, otherwise re-picking the same file never fires
      // `change` again.
      characterReferenceUploadInput.value = '';
      if (file) {
        uploadCharacterReference(state.characterLibrarySelectedCharacterId, file);
      }
    });
  }
  const characterReferenceList = $('#characterReferenceList');
  if (characterReferenceList) {
    characterReferenceList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-character-reference-delete]') : null;
      if (!btn || !characterReferenceList.contains(btn)) return;
      deleteCharacterReference(Number(btn.dataset.characterId), Number(btn.dataset.characterReferenceDelete));
    });
  }
  const maintenanceTabs = $('.maintenance-view-tabs');
  if (maintenanceTabs) {
    // Mirror horizontal scroll state so CSS can show/hide the edge fade that
    // hints at off-screen views on mobile.
    maintenanceTabs.addEventListener('scroll', syncMaintenanceTabsEdge, {passive: true});
    window.addEventListener('resize', syncMaintenanceTabsEdge);
    // Crossing the mobile breakpoint must repaint the character-library view
    // switch: desktop shows all three zones again.
    window.addEventListener('resize', applyCharacterLibraryMobileView);
    syncMaintenanceTabsEdge();
    maintenanceTabs.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-maintenance-view]') : null;
      if (!btn || !maintenanceTabs.contains(btn)) return;
      setMaintenanceView(btn.dataset.maintenanceView);
      if (state.mode === 'moves') {
        refreshActiveMaintenanceView({preserveScroll: true, reason: 'tab'});
      }
    });
  }
  const overviewActions = $('#overviewActionCards');
  if (overviewActions) {
    overviewActions.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const card = target ? target.closest('[data-maintenance-jump]') : null;
      if (!card || !overviewActions.contains(card)) return;
      handleMaintenanceJump(card.dataset.maintenanceJump);
    });
  }
  const healthGrid = $('#healthGrid');
  if (healthGrid) healthGrid.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target.closest('[data-error-artists-open]') : null;
    if (target && healthGrid.contains(target)) openErrorArtistsDialog();
  });
  const mlDownloadSourceSelect = $('#mlDownloadSourceSelect');
  if (mlDownloadSourceSelect) {
    mlDownloadSourceSelect.addEventListener('change', () => saveMlDownloadSource());
  }
  const mlRuntimeRetryBtn = $('#mlRuntimeRetryBtn');
  if (mlRuntimeRetryBtn) {
    mlRuntimeRetryBtn.addEventListener('click', () => retryMlRuntime());
  }
  const errorArtistsDialogEl = $('#errorArtistsDialog');
  if (errorArtistsDialogEl) {
    $('#errorArtistsCloseBtn')?.addEventListener('click', closeErrorArtistsDialog);
    errorArtistsDialogEl.addEventListener('cancel', e => { e.preventDefault(); closeErrorArtistsDialog(); });
    errorArtistsDialogEl.addEventListener('click', e => { if (e.target === errorArtistsDialogEl) closeErrorArtistsDialog(); });
  }
  const errorArtistsScroll = $('#errorArtistsDialog .artist-links-dialog-body');
  if (errorArtistsScroll) errorArtistsScroll.addEventListener('scroll', () => {
    if (errorArtistsScroll.scrollTop + errorArtistsScroll.clientHeight >= errorArtistsScroll.scrollHeight - 80) {
      loadErrorArtistsPage();
    }
  });
  const errorArtistsSearch = $('#errorArtistsSearch');
  if (errorArtistsSearch) {
    errorArtistsSearch.addEventListener('input', debounce(e => {
      state.errorArtistsQuery = e.target.value.trim();
      state.errorArtistsRequestSeq += 1;
      loadErrorArtistsPage({reset: true});
    }, 300));
  }
  const errorArtistsSort = $('#errorArtistsSort');
  if (errorArtistsSort) errorArtistsSort.addEventListener('change', e => {
    state.errorArtistsSort = e.target.value === 'count' ? 'count' : 'recent';
    state.errorArtistsRequestSeq += 1;
    loadErrorArtistsPage({reset: true});
  });
  $('#errorArtistsMore')?.addEventListener('click', () => loadErrorArtistsPage());
  $('#errorArtistsList')?.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target.closest('[data-error-artist-id]') : null;
    if (!target) return;
    jumpToErrorArtist({artist_id: target.dataset.errorArtistId, latest_plan_id: target.dataset.errorLatestPlanId});
  });

  const maintenanceGuideDialogEl = $('#maintenanceGuideDialog');
  if (maintenanceGuideDialogEl) {
    $('#maintenanceGuideOpenBtn')?.addEventListener('click', () => {
      if (typeof maintenanceGuideDialogEl.showModal === 'function') maintenanceGuideDialogEl.showModal();
    });
    $('#maintenanceGuideCloseBtn')?.addEventListener('click', () => {
      if (maintenanceGuideDialogEl.open) maintenanceGuideDialogEl.close();
    });
    maintenanceGuideDialogEl.addEventListener('cancel', e => {
      e.preventDefault();
      if (maintenanceGuideDialogEl.open) maintenanceGuideDialogEl.close();
    });
    maintenanceGuideDialogEl.addEventListener('click', e => {
      if (e.target === maintenanceGuideDialogEl && maintenanceGuideDialogEl.open) maintenanceGuideDialogEl.close();
    });
  }

  document.addEventListener('visibilitychange', () => {
    if (state.mode === 'moves' && !document.hidden) {
      refreshActiveMaintenanceView({preserveScroll: true, reason: 'visible'}).catch(e => {
        if (!isAbortError(e)) toast('维护页面刷新失败：' + (e.message || e), 'error');
      });
      scheduleMaintenanceAutoRefresh();
    }
  });

  $('#manualBackupBtn').addEventListener('click', async () => {
    if (isActionBusy('backup-manual')) return;
    setActionBusy('backup-manual', '', true);
    const btn = $('#manualBackupBtn');
    const result = $('#backupResult');
    btn.disabled = true;
    btn.textContent = '备份中';
    result.textContent = '';
    result.style.color = '';
    try {
      const r = await API.post('/api/backup');
      result.textContent = r.ok ? '备份完成' : '备份失败';
      result.style.color = r.ok ? 'var(--status-ok)' : 'var(--status-danger)';
      await loadHealthSummary();
    } catch (e) {
      result.textContent = '备份失败：' + (e.message || e);
      result.style.color = 'var(--status-danger)';
    } finally {
      btn.disabled = false;
      btn.textContent = '立即备份数据库';
      setActionBusy('backup-manual', '', false);
    }
  });

  $('#dimensionBackfillBtn').addEventListener('click', backfillItemDimensions);

  $('#editApplyBtn').addEventListener('click', async () => {
    await ensureEditTagContext();
    await selectOrCreateEditTagQuery();
    const tagIds = selectedEditTagIds();
    const tagNames = selectedEditTagNames(tagIds);
    logUiAction('edit_apply_click', {
      selected_count: state.selectedIds.size,
      item_ids: [...state.selectedIds],
      folder: state.activeFolder || '',
      artist_id: currentEditArtistId(),
      mode: 'add',
      tag_ids: tagIds,
      tag_names: tagNames,
    });
    if (tagIds.length === 0 && tagNames.length === 0) { toast('请选择要操作的目标标签', 'error'); return; }
    if (state.selectedIds.size > 0) {
      const suggestionWarning = characterSuggestionCoverageWarning([...state.selectedIds], tagNames);
      if (suggestionWarning && !window.confirm(suggestionWarning)) return;
      classifyItems([...state.selectedIds], tagIds, 'add');
      return;
    }
    if (isCurrentFolderScopeActive()) {
      classifyFolder(state.activeFolder, tagIds, 'add');
      return;
    }
    toast('请选择作品或文件夹', 'error');
  });

  $('#editSelectAllBtn').addEventListener('click', () => {
    const taggable = (state.allItems || []).filter(isTaggableItem);
    const isAllSelected = taggable.length > 0 && state.selectedIds.size >= taggable.length;
    if (isAllSelected) {
      applySelectionChange([], {reason: 'deselect_all'});
    } else {
      applySelectionChange(taggable.map(item => item.id), {reason: 'select_all'});
    }
  });

  $('#editDeleteSelectedBtn').addEventListener('click', () => deleteSelectedMediaItems());

  $('#editTagSearch').addEventListener('focus', e => {
    state.editTagQuery = e.target.value;
    openEditTagPicker();
  });
  $('#editTagSearch').addEventListener('input', e => {
    state.editTagQuery = e.target.value;
    openEditTagPicker();
  });
  $('#editTagSearch').addEventListener('keydown', e => {
    if (e.key === 'Enter') {
      e.preventDefault();
      selectFirstEditTagResult();
    } else if (e.key === 'Escape') {
      closeEditTagPicker();
    }
  });

  const editDatePrecision = $('#editDatePrecision');
  if (editDatePrecision) {
    editDatePrecision.addEventListener('change', syncEditDatePrecisionInputs);
  }
  const editDateMonth = $('#editDateMonth');
  if (editDateMonth) {
    editDateMonth.addEventListener('input', () => {
      const dayInput = $('#editDateDay');
      if (dayInput && !dayInput.hidden && editDateMonth.value && !dayInput.value) {
        dayInput.value = editDateMonth.value + '-01';
      }
    });
  }
  $('#editDateApplyBtn').addEventListener('click', () => {
    const value = editDateEnteredValue();
    if (!value) {
      toast('请先选择要设置的日期', 'error');
      return;
    }
    applyItemDateBatch(value);
  });
  $('#editDateResetBtn').addEventListener('click', () => applyItemDateBatch(null));
  API.get('/api/capabilities')
    .then(caps => { state.readOnlyMode = Boolean(caps && caps.read_only); renderEditDateControl(); })
    .catch(() => {});

  $('#editDeleteRoleBtn').addEventListener('click', removeSelectedTagsFromItems);
  const characterSuggestionsList = $('#characterSuggestionsList');
  if (characterSuggestionsList) {
    characterSuggestionsList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target) return;
      const acceptAll = target.closest('[data-character-suggestion-accept-all]');
      if (acceptAll && characterSuggestionsList.contains(acceptAll)) {
        return void selectAllCharacterSuggestions();
      }
      const btn = target.closest('[data-character-suggestion-tag]');
      if (!btn || !characterSuggestionsList.contains(btn)) return;
      selectCharacterSuggestionTag((btn.dataset.characterSuggestionTag || '').trim());
    });
  }

  $('#editCancelBtn').addEventListener('click', () => {
    applySelectionChange([], {reason: 'cancel_selection'});
  });

  // The tag picker's panel handler rewrites its innerHTML while handling a
  // selection, which detaches the clicked option before this bubble-phase
  // check runs — contains(detachedNode) is false, so every selection used to
  // read as an outside click and slammed the picker shut. Capture the
  // containment verdict first: capture runs before the panel handler.
  let clickInsideEditTagPicker = false;
  document.addEventListener('click', e => {
    clickInsideEditTagPicker = $('#editTagPicker').contains(e.target);
  }, true);
  document.addEventListener('click', e => {
    if (!clickInsideEditTagPicker && !$('#editTagPicker').contains(e.target)) {
      closeEditTagPicker();
    }
    const artistPicker = $('#artistPicker');
    const emptySelectArtist = $('#emptySelectArtistBtn');
    const clickedArtistChrome = Boolean(
      (artistPicker && artistPicker.contains(e.target))
      || (emptySelectArtist && emptySelectArtist.contains(e.target))
    );
    if (!clickedArtistChrome) {
      closeArtistDropdown();
    }
    if (!$('#searchControl').contains(e.target)) {
      closeSearchOptions();
    }
  });
  document.addEventListener('keydown', e => {
    if (e.key === 'Control' || e.key === 'Meta') state.selectionModifierDown = true;
    if (e.key === 'Escape' && closeTopmostOverlay()) {
      e.preventDefault();
      if (typeof e.stopImmediatePropagation === 'function') e.stopImmediatePropagation();
    }
    if ((e.key === 't' || e.key === 'T') && !e.ctrlKey && !e.metaKey && !e.altKey) {
      const activeEl = document.activeElement;
      const tag = activeEl ? activeEl.tagName : '';
      if (tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT' && !activeEl?.isContentEditable) {
        e.preventDefault();
        toggleTheme();
      }
    }
    // Focus trap for open filter drawer / lightbox dialogs.
    if (e.key === 'Tab') {
      const trapRoot = state.filterDrawerOpen
        ? $('#filterSidebar')
        : ($('#lightbox')?.style.display === 'flex' ? $('#lightbox') : null);
      if (trapRoot) {
        const focusables = [...trapRoot.querySelectorAll(
          'a[href],button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])'
        )].filter(el => !el.hasAttribute('inert') && el.offsetParent !== null);
        if (focusables.length) {
          const first = focusables[0];
          const last = focusables[focusables.length - 1];
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault();
            last.focus();
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault();
            first.focus();
          } else if (!trapRoot.contains(document.activeElement)) {
            e.preventDefault();
            first.focus();
          }
        }
      }
    }
  });
  document.addEventListener('keyup', e => {
    if (e.key === 'Control' || e.key === 'Meta') state.selectionModifierDown = e.ctrlKey || e.metaKey;
  });
  window.addEventListener('blur', () => {
    state.selectionModifierDown = false;
  });

  $('#lightbox').addEventListener('click', e => {
    const eventTarget = e.target;
    const closeButton = eventTarget instanceof Element ? eventTarget.closest('.close') : null;
    if (eventTarget === $('#lightbox') || eventTarget === $('#lightboxStage') || (closeButton && $('#lightbox').contains(closeButton))) closeLightbox();
  });
  $('#lightbox').addEventListener('wheel', onLightboxWheel, {passive: false});
  const lightboxImg = $('#lightboxImg');
  lightboxImg.addEventListener('pointerdown', startLightboxPan);
  lightboxImg.addEventListener('pointermove', moveLightboxPan);
  lightboxImg.addEventListener('pointerup', stopLightboxPan);
  lightboxImg.addEventListener('pointercancel', stopLightboxPan);
  $('#lightboxDownloadBtn').addEventListener('click', e => {
    e.stopPropagation();
  });
  $('#lightboxFavoriteBtn').addEventListener('click', e => {
    e.stopPropagation();
    const item = state.allItems.find(row => row.id === parseInt(e.currentTarget.dataset.favorite));
    toggleItemFavorite(item);
  });
  const deleteBtn = $('#lightboxDeleteBtn');
  if (deleteBtn) {
    deleteBtn.addEventListener('click', e => {
      e.stopPropagation();
      onLightboxDelete(deleteBtn);
    });
  }
  $('#lightbox .prev').addEventListener('click', e => {
    e.stopPropagation();
    moveLightbox(-1);
  });
  $('#lightbox .next').addEventListener('click', e => {
    e.stopPropagation();
    moveLightbox(1);
  });
  bindArtistLinks();
  bindArtistProfileLinks();
  bindArchiveModal();
}

function closeTopmostOverlay() {
  const artistLinksDialog = $('.artist-links-dialog[open]');
  if (artistLinksDialog) {
    if (artistLinksDialog.id === 'archiveDialog') {
      closeArchiveModal();
    } else {
      closeArtistLinksDialog(artistLinksDialog);
    }
    return true;
  }
  if ($('#lightbox').style.display === 'flex') {
    closeLightbox();
    return true;
  }
  if (state.filterDrawerOpen) {
    closeFilterDrawer();
    return true;
  }
  if ($('#editTagPicker').classList.contains('open')) {
    closeEditTagPicker();
    return true;
  }
  if ($('#artistDropdown').classList.contains('open')) {
    closeArtistDropdown();
    return true;
  }
  if (state.searchOptionsOpen) {
    closeSearchOptions();
    return true;
  }
  if (state.mobileHeaderToolsOpen) {
    closeMobileHeaderTools();
    return true;
  }
  if (state.editMode || state.selectedIds.size > 0) {
    setEditMode(false);
    return true;
  }
  return false;
}

// Late imports closing remaining cycles; every use is inside a function body.
import {
  maybeLoadMoreOnScroll, isCurrentScanScopeActive, isCurrentFolderScopeActive,
} from './views/sidebar.js';
import { toggleItemFavorite } from './views/grid.js';
import { focusArtistPicker, renderArtistDropdown, moveArtistDropdownActive, selectFirstArtistResult } from './router.js';
import {
  openEditTagPicker, selectFirstEditTagResult, classifyItems, classifyFolder, currentEditArtistId,
} from './views/editbar.js';
import {
  toggleArchivePlanConfirmation, undoArchivePlan, jumpToArchivePlanFolder,
} from './views/maintenance/organize.js';
import { isActionBusy, setActionBusy } from './store.js';
