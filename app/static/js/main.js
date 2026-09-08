// Application entry: boot sequence, WebSocket scan channel, and progress UI.
// Everything else lives in its own module and is imported explicitly.

import { state, ingestScanState, isCurrentRequestSeq } from './store.js';
import { $ } from './utils.js';
import { toast, logUiAction, collectUiLogContext, installFrontendErrorLogging } from './logging.js';
import { loadArtists, restoreBrowseUrl } from './router.js';
import { bindEvents, refreshCurrentView, scheduleDuplicatesViewRefresh } from './events.js';
import {
  loadSidebarWidth, loadSidebarTagRatio, loadSidebarCollapsed, loadMobileColumns,
  loadCardRatio, syncFilterDrawer, updateScanFolderButton, renderLibraryEmptyState,
} from './views/sidebar.js';
import { initTheme } from './views/theme.js';

let wsRetryDelay = 1000;

function scheduleWsReconnect() {
  setTimeout(connectWS, wsRetryDelay);
  wsRetryDelay = Math.min(wsRetryDelay * 2, 30000);
}

export function connectWS() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  let ws;
  try {
    ws = new WebSocket(`${proto}//${location.host}/ws/scan`);
  } catch (e) {
    scheduleWsReconnect();
    return;
  }
  ws.onopen = () => {
    wsRetryDelay = 1000;
  };
  ws.onmessage = e => {
    let s = {};
    try {
      s = JSON.parse(e.data);
    } catch (err) {
      return;
    }
    if (!s || typeof s !== 'object') return;
    // ingestScanState owns the run-key gate: a terminal snapshot from before
    // page load stays silent, later live runs refresh once per run.
    const gate = ingestScanState(s);
    if (gate.scanning) {
      showProgress(s);
      $('#scanBtn').style.display = 'none';
      updateScanFolderButton();
      $('#stopScanBtn').style.display = '';
      renderLibraryEmptyState();
    } else {
      hideProgress();
      $('#scanBtn').style.display = '';
      updateScanFolderButton();
      $('#stopScanBtn').style.display = 'none';
      renderLibraryEmptyState();
      if (gate.refresh) {
        refreshAfterScan({toast: gate.toast, phase: gate.phase, error: gate.error});
      }
    }
  };
  ws.onerror = () => {
    // onclose always follows onerror; the backoff loop lives there.
  };
  ws.onclose = () => scheduleWsReconnect();
}

function showProgress(s) {
  const panel = $('#progressPanel');
  // Cancel any pending fade-out timer: a new scan started before the previous
  // one finished hiding, so the bar must remain visible.
  if (panel._hideProgressTimer) {
    clearTimeout(panel._hideProgressTimer);
    panel._hideProgressTimer = null;
  }
  panel.classList.remove('hiding');
  panel.classList.add('visible');
  $('#progressTitle').textContent = s.phase === 'discover' ? '发现画师目录' :
    s.phase === 'scan' ? '扫描画师文件' :
    s.phase === 'parse' ? '整理文件记录' : '扫描中';
  const pct = s.total_estimate > 0 ? Math.round(s.scanned_count / s.total_estimate * 100) : 0;
  $('#progressFill').style.width = Math.min(pct, 100) + '%';
  $('#progressCount').textContent = `${s.scanned_count} / ${s.total_estimate}`;
  $('#progressPath').textContent = s.current_path || '';
}

function hideProgress() {
  const panel = $('#progressPanel');
  if (panel._hideProgressTimer) return;
  panel.classList.add('hiding');
  const reducedMotion = typeof window.matchMedia === 'function'
    && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  panel._hideProgressTimer = setTimeout(() => {
    panel._hideProgressTimer = null;
    panel.classList.remove('visible', 'hiding');
  }, reducedMotion ? 0 : 150);
}

async function refreshAfterScan(options = {}) {
  const toastOnFinish = options.toast !== false;
  const isError = options.phase === 'error' || options.phase === 'failed';
  const isPartial = options.phase === 'partial';
  try {
    const seq = await refreshCurrentView({reason: 'scan_complete'});
    if (toastOnFinish && isCurrentRequestSeq('scanRefreshSeq', seq)) {
      if (isError) {
        const detail = options.error ? String(options.error).replace(/^error:\s*/i, '').trim() : '';
        toast(detail ? `扫描失败：${detail}` : '扫描失败', 'error');
        logUiAction('scan_failed', collectUiLogContext({error: detail || 'scan_error'}));
      } else if (isPartial) {
        const detail = options.error ? String(options.error).replace(/^error:\s*/i, '').trim() : '';
        toast(detail ? `扫描部分完成：${detail}` : '扫描部分完成', 'warning');
        logUiAction('scan_partial', collectUiLogContext({error: detail || 'scan_partial'}));
      } else if (options.phase === 'stopped') {
        toast('扫描已停止', 'info');
      } else if (options.phase === 'interrupted') {
        toast('扫描已中断', 'info');
      } else {
        toast('扫描完成', 'success');
      }
    }
  } catch (e) {
    toast('扫描刷新失败', 'error');
    logUiAction('scan_refresh_failed', collectUiLogContext({error: e.message || String(e)}));
  }
}

// store.js owns the sequence helper; the import above keeps the comparison
// identical to the previous single-file implementation.

async function init() {
  loadSidebarWidth();
  loadSidebarTagRatio();
  loadSidebarCollapsed();
  loadMobileColumns();
  initTheme();
  loadCardRatio();
  bindEvents();
  document.body.classList.toggle('mode-moves', state.mode === 'moves');
  document.body.classList.toggle('mode-browse', state.mode !== 'moves');
  syncFilterDrawer();
  connectWS();
  scheduleDuplicatesViewRefresh();
  await loadArtists();
  await restoreBrowseUrl();
}

installFrontendErrorLogging();
init();
