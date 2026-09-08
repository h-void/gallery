// Maintenance page shared infrastructure: the active-view request owner, the
// per-view loaders, auto-refresh scheduling, and the view switcher.

import { API } from '../../api.js';
import { state, nextRequestSeq, isCurrentRequestSeq } from '../../store.js';
import { $, $$ } from '../../utils.js';
import { toast, logUiAction } from '../../logging.js';
import { loadHealthSummary, loadHashStatus, loadFolderRenameAutoStatus, loadErrorArtistsSummary, loadMlRuntime, loadDimensionBackfillStatus, renderHealthSummary, renderHashStatus, renderFolderRenameAutoStatus, renderMlRuntime, renderOverviewActions, renderDimensionBackfillStatus } from './overview.js';
import { renderMovePathSummary, renderMoveCandidates, renderMoveCandidateGroups, renderMoveHistory } from './paths.js';
import { loadArtistFolderMove, loadArchiveWorkbench, renderArtistFolderMove, renderArchiveWorkbench } from './organize.js';
import { loadCharacterLibrary, characterImportJobBusy, renderCharacterLibrary } from './characters.js';
import { loadOperationLog, loadRecycleBin, renderOperationLog, renderRecycleBin } from './records.js';

export const MAINTENANCE_AUTO_REFRESH_MS = 10000;
export const MAINTENANCE_IDLE_REFRESH_MS = 60000;

let maintenanceAutoRefreshTimer = null;
let maintenanceAutoRefreshInFlight = false;
let activeMaintenanceController = null;
let activeMaintenanceRequest = null;
let maintenanceConsecutiveRefreshFailures = 0;

export function movePanelScrollTop() {
  const panel = $('#movePanel');
  return panel ? panel.scrollTop : 0;
}

export function restoreMovePanelScroll(top) {
  if (top == null) return;
  const panel = $('#movePanel');
  if (!panel) return;
  panel.scrollTop = top;
  requestAnimationFrame(() => { panel.scrollTop = top; });
}

export async function loadMoveWorkbench(options = {}) {
  return refreshActiveMaintenanceView(options);
}

function abortActiveMaintenanceRequest() {
  if (!activeMaintenanceRequest && !activeMaintenanceController) return;
  const controller = activeMaintenanceRequest ? activeMaintenanceRequest.controller : activeMaintenanceController;
  if (controller) controller.abort();
  activeMaintenanceRequest = null;
  activeMaintenanceController = null;
}

const maintenanceLoaders = {
  overview: async loadOptions => {
    await Promise.all([
      loadHealthSummary(loadOptions),
      loadHashStatus(loadOptions),
      loadFolderRenameAutoStatus(loadOptions),
      loadErrorArtistsSummary(loadOptions),
      loadMlRuntime(loadOptions),
      loadDimensionBackfillStatus(loadOptions),
    ]);
    // Lightweight pending count for the overview "接下来做什么" cards.
    try {
      const fetchOptions = loadOptions.signal ? {signal: loadOptions.signal} : {};
      const pending = await API.get(
        '/api/move-candidates?status=pending&hide_grouped=true&limit=1&offset=0',
        fetchOptions
      );
      state.movePendingTotal = pending.total ?? 0;
      state.moveWaitingHashCount = pending.waiting_hash_count || 0;
    } catch (e) {
      if (!isAbortError(e)) {
        // Keep last known counts if the overview card request fails.
      }
    }
  },
  paths: async loadOptions => {
    // P3 修改方向 7: loading/refreshing the 待判断 list is strictly read-only;
    // auto-resolve stays behind its explicit button and the background
    // scan/hash workers keep their own responsibilities.
    const fetchOptions = loadOptions.signal ? {signal: loadOptions.signal} : {};
    const [pending, groups, applied, hashStatus] = await Promise.all([
      API.get('/api/move-candidates?status=pending&hide_grouped=true&limit=500&offset=0', fetchOptions),
      API.get('/api/move-candidates/groups?status=pending', fetchOptions),
      API.get('/api/move-history?status=applied&limit=80&offset=0', fetchOptions),
      API.get('/api/hash/status', fetchOptions),
    ]);
    state.moveCandidates = pending.candidates || [];
    state.movePendingTotal = pending.total ?? state.moveCandidates.length;
    state.moveCandidateGroups = groups.groups || [];
    state.moveWaitingHashCount = pending.waiting_hash_count || 0;
    state.moveHistory = applied.history || [];
    state.moveHistoryTotal = Number(applied.total ?? state.moveHistory.length);
    state.moveHistoryHasMore = applied.has_more === true;
    state.moveHistoryLimit = Number(applied.limit ?? state.moveHistory.length);
    state.hashStatus = hashStatus;
  },
  organize: async loadOptions => {
    await Promise.all([
      loadArtistFolderMove(loadOptions),
      loadArchiveWorkbench(loadOptions),
    ]);
  },
  characters: async loadOptions => {
    await Promise.all([
      loadCharacterLibrary(loadOptions),
    ]);
  },
  records: async loadOptions => {
    await Promise.all([
      loadOperationLog(loadOptions),
      loadRecycleBin(loadOptions),
    ]);
  },
};

function activeMaintenanceViewHasActiveWork() {
  const view = state.maintenanceView || 'overview';
  const hashWorker = state.hashStatus && state.hashStatus.worker ? state.hashStatus.worker : {};
  const scan = (state.lastScanState && state.lastScanState.status === 'scanning')
    ? state.lastScanState
    : (state.healthSummary && state.healthSummary.scan ? state.healthSummary.scan : state.lastScanState);
  if (view === 'overview') {
    // Model/CUDA runtime background download counts as active work so the
    // maintenance page keeps its short polling interval while preparing.
    return Boolean(
      (state.mlRuntimeStatus && state.mlRuntimeStatus.download_in_progress)
      || (state.mlRuntimeStatus?.model_status?.state === 'downloading')
      || (state.mlRuntimeStatus?.cuda_status?.state === 'downloading')
    );
  }
  if (view === 'paths') {
    return Boolean((scan && scan.status === 'scanning') || hashWorker.thread_alive);
  }
  if (view === 'characters') {
    return characterImportJobBusy();
  }
  return false;
}

function maintenanceRefreshDelayMs() {
  return activeMaintenanceViewHasActiveWork() ? MAINTENANCE_AUTO_REFRESH_MS : MAINTENANCE_IDLE_REFRESH_MS;
}

function renderActiveMaintenanceView(view) {
  if (view === 'overview') {
    renderHealthSummary();
    renderHashStatus();
    renderFolderRenameAutoStatus();
    renderMlRuntime();
    renderDimensionBackfillStatus();
    renderOverviewActions();
    return;
  }
  if (view === 'paths') {
    renderMovePathSummary();
    renderHashStatus();
    renderMoveCandidateGroups();
    renderMoveCandidates();
    renderMoveHistory();
    return;
  }
  if (view === 'organize') {
    renderArtistFolderMove();
    renderArchiveWorkbench();
    return;
  }
  if (view === 'characters') {
    renderCharacterLibrary();
    return;
  }
  if (view === 'records') {
    renderOperationLog();
    renderRecycleBin();
  }
}

export async function refreshActiveMaintenanceView(options = {}) {
  const preservedScrollTop = options.preserveScroll ? movePanelScrollTop() : null;
  const view = options.view || state.maintenanceView || 'overview';
  if (activeMaintenanceRequest && activeMaintenanceRequest.view === view) {
    return activeMaintenanceRequest.promise;
  }
  const seq = nextRequestSeq('maintenanceLoadSeq');
  abortActiveMaintenanceRequest();
  const controller = new AbortController();
  activeMaintenanceController = controller;
  const signal = controller.signal;
  const request = {view, controller, promise: null};
  const loadOptions = {...options, signal};
  activeMaintenanceRequest = request;
  request.promise = (async () => {
    try {
      const loader = maintenanceLoaders[view] || maintenanceLoaders.overview;
      await loader(loadOptions);
      if (!isCurrentRequestSeq('maintenanceLoadSeq', seq)) return;
      renderActiveMaintenanceView(view);
      maintenanceConsecutiveRefreshFailures = 0;
      restoreMovePanelScroll(preservedScrollTop);
    } catch (e) {
      if (isAbortError(e)) return;
      throw e;
    } finally {
      if (activeMaintenanceRequest === request) {
        activeMaintenanceRequest = null;
      }
      if (activeMaintenanceController === controller) {
        activeMaintenanceController = null;
      }
    }
  })();
  return request.promise;
}

async function refreshMoveWorkbenchAutomatically() {
  if (state.mode !== 'moves' || document.hidden || maintenanceAutoRefreshInFlight) {
    scheduleMaintenanceAutoRefresh();
    return;
  }
  maintenanceAutoRefreshInFlight = true;
  try {
    await refreshActiveMaintenanceView({preserveScroll: true, reason: 'auto'});
  } catch (e) {
    if (!isAbortError(e)) {
      maintenanceConsecutiveRefreshFailures += 1;
      if (maintenanceConsecutiveRefreshFailures === 3) {
        toast('维护页面自动刷新失败，稍后会继续尝试', 'error');
      }
    }
  } finally {
    maintenanceAutoRefreshInFlight = false;
    scheduleMaintenanceAutoRefresh();
  }
}

export function scheduleMaintenanceAutoRefresh() {
  if (maintenanceAutoRefreshTimer) clearTimeout(maintenanceAutoRefreshTimer);
  maintenanceAutoRefreshTimer = null;
  if (state.mode !== 'moves' || document.hidden) return;
  maintenanceAutoRefreshTimer = setTimeout(refreshMoveWorkbenchAutomatically, maintenanceRefreshDelayMs());
}

export function startMaintenanceAutoRefresh() {
  if (maintenanceAutoRefreshTimer) {
    clearTimeout(maintenanceAutoRefreshTimer);
    maintenanceAutoRefreshTimer = null;
  }
  scheduleMaintenanceAutoRefresh();
}

export function stopMaintenanceAutoRefresh() {
  abortActiveMaintenanceRequest();
  if (!maintenanceAutoRefreshTimer) return;
  clearTimeout(maintenanceAutoRefreshTimer);
  maintenanceAutoRefreshTimer = null;
}

function maintenanceAvailableViews() {
  return [...$$('.maintenance-view-panel[data-maintenance-view-panel]')]
    .map(panel => panel.dataset.maintenanceViewPanel)
    .filter(Boolean);
}

export function setMaintenanceView(view, options = {}) {
  const maintenanceViews = maintenanceAvailableViews();
  const selected = maintenanceViews.includes(view) ? view : 'overview';
  state.maintenanceView = selected;
  $$('.maintenance-view-tabs [data-maintenance-view]').forEach(btn => {
    const active = btn.dataset.maintenanceView === selected;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-selected', active ? 'true' : 'false');
    if (active) {
      const activeTab = btn;
      activeTab.scrollIntoView({block: 'nearest', inline: 'center'});
    }
  });
  $$('.maintenance-view-panel[data-maintenance-view-panel]').forEach(panel => {
    const active = panel.dataset.maintenanceViewPanel === selected;
    panel.hidden = !active;
    panel.classList.toggle('active', active);
  });
  if (selected === 'paths') {
    const target = $('.maintenance-workbench');
    if (target && options.scrollToWorkbench) {
      requestAnimationFrame(() => {
        target.scrollIntoView({block: 'start', behavior: 'smooth'});
      });
    }
  }
}

export function handleMaintenanceJump(jump) {
  if (jump === 'paths') {
    setMaintenanceView('paths', {scrollToWorkbench: true});
    loadMoveWorkbench({view: 'paths'}).catch(() => {});
    return;
  }
  if (jump === 'organize') {
    setMaintenanceView('organize');
    loadMoveWorkbench({view: 'organize'}).catch(() => {});
    return;
  }
  if (jump === 'history') {
    setMaintenanceView('records');
    loadMoveWorkbench({view: 'records'}).catch(() => {});
    return;
  }
  if (jump === 'characters') {
    setMaintenanceView('characters');
    loadMoveWorkbench({view: 'characters'}).catch(() => {});
  }
}

function isAbortError(error) {
  return Boolean(error && (error.name === 'AbortError' || error.code === 20));
}

// The maintenance tab edge fade lives with the tab strip's scroll handling.
export function syncMaintenanceTabsEdge() {
  const maintenanceTabs = $('.maintenance-view-tabs');
  if (!maintenanceTabs || maintenanceTabs.clientWidth === 0) return;
  const atEnd = maintenanceTabs.scrollLeft + maintenanceTabs.clientWidth
    >= maintenanceTabs.scrollWidth - 2;
  if (atEnd) maintenanceTabs.dataset.scrolledEnd = '';
  else delete maintenanceTabs.dataset.scrolledEnd;
}
