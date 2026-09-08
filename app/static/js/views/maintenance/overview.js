// Maintenance overview (overview): health cards, ML runtime, hash status,
// auto-archive toggle, error-artists dialog, and the full-scan entry.

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy } from '../../store.js';
import { $, escHtml, joinUiMeta, formatSize, formatBytes, formatHealthTime, isAbortError } from '../../utils.js';
import { toast, logUiAction } from '../../logging.js';
import { setMaintenanceView, loadMoveWorkbench } from './index.js';
import { loadArchiveWorkbench } from './organize.js';
import { applyMode } from '../../events.js';
import { loadArtists, selectArtist } from '../../router.js';

const DIMENSION_BACKFILL_POLL_MS = 1500;
let dimensionBackfillPollTimer = null;

const PROVIDER_LABELS = {
  auto: '自动',
  cuda: 'CUDA',
  openvino: 'OpenVINO',
  cpu: 'CPU',
};

const ACTUAL_PROVIDER_LABELS = {
  CUDAExecutionProvider: 'CUDA',
  OpenVINOExecutionProvider: 'OpenVINO',
  CPUExecutionProvider: 'CPU',
  not_initialized: '未初始化',
  none: '无可用会话',
};

function providerLabel(value, map, fallback) {
  return map[value] || value || fallback;
}

export async function loadHealthSummary(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const health = await API.get('/api/health', fetchOptions);
    if (updateState) state.healthSummary = health;
    if (render) renderHealthSummary();
    return health;
  } catch (e) {
    if (isAbortError(e)) throw e;
    const health = {ok: false, error: e.message};
    if (updateState) state.healthSummary = health;
    if (render) renderHealthSummary();
    return health;
  }
}

export async function loadDimensionBackfillStatus(options = {}) {
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const status = await API.get('/api/items/dimensions/backfill/status', fetchOptions);
    state.dimensionBackfill = status;
    renderDimensionBackfillStatus();
    scheduleDimensionBackfillPoll();
    return status;
  } catch (e) {
    if (isAbortError(e)) throw e;
    state.dimensionBackfill = {running: false, complete: false, error: e.message || String(e)};
    renderDimensionBackfillStatus();
    return state.dimensionBackfill;
  }
}

function scheduleDimensionBackfillPoll() {
  if (dimensionBackfillPollTimer) clearTimeout(dimensionBackfillPollTimer);
  dimensionBackfillPollTimer = null;
  if (state.mode !== 'moves' || document.hidden || !state.dimensionBackfill?.running) return;
  dimensionBackfillPollTimer = setTimeout(pollDimensionBackfillStatus, DIMENSION_BACKFILL_POLL_MS);
}

async function pollDimensionBackfillStatus() {
  dimensionBackfillPollTimer = null;
  if (state.mode !== 'moves' || document.hidden) return;
  const previous = state.dimensionBackfill || {};
  try {
    const status = await API.get('/api/items/dimensions/backfill/status');
    state.dimensionBackfill = status;
    renderDimensionBackfillStatus();
    if (previous.running && !status.running) {
      logUiAction('item_dimensions_backfill_result', {
        status: status.error ? 'error' : 'complete',
        updated: Number(status.updated || 0),
        failed: Number(status.failed || 0),
        remaining: Number(status.remaining || 0),
        complete: Boolean(status.complete),
      });
      if (status.error) toast('媒体尺寸补全失败', 'error');
      else if (status.complete) {
        toast(Number(status.updated || 0) ? `已补全 ${Number(status.updated)} 项` : '媒体尺寸已完整', 'success');
        try { await loadItemsPreservingDepth(); } catch (e) {
          logUiAction('item_dimensions_backfill_reload_failed', {error: e.message || String(e)});
        }
      }
    }
  } catch (e) {
    if (!isAbortError(e)) logUiAction('item_dimensions_backfill_status_error', {error: e.message || String(e)});
  } finally {
    scheduleDimensionBackfillPoll();
  }
}

export async function loadHashStatus(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const status = await API.get('/api/hash/status', fetchOptions);
    if (updateState) state.hashStatus = status;
    if (render) renderHashStatus();
    return status;
  } catch (e) {
    if (isAbortError(e)) throw e;
    const status = {database_error: true, error: e.message || String(e)};
    if (updateState) state.hashStatus = status;
    if (render) renderHashStatus();
    return status;
  }
}

export async function loadMlRuntime(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const [status, settings] = await Promise.all([
      API.get('/api/ml-runtime/status', fetchOptions),
      API.get('/api/ml-runtime/settings', fetchOptions),
    ]);
    if (updateState) {
      state.mlRuntimeStatus = status;
      state.mlRuntimeSettings = settings;
    }
    if (render) renderMlRuntime();
    return {status, settings};
  } catch (e) {
    if (isAbortError(e)) throw e;
    const errorStatus = {error: e.message || String(e)};
    if (updateState) state.mlRuntimeStatus = errorStatus;
    if (render) renderMlRuntime();
    return {status: errorStatus, settings: null};
  }
}

function mlArtifactStateLabel(s) {
  const stateLabel = {
    not_started: '未开始',
    downloading: '下载中',
    ready: '就绪',
    error: '失败',
  }[s && s.state] || (s && s.state) || '未知';
  if (s && s.state === 'downloading') {
    const total = Number(s.total_bytes || 0);
    const done = Number(s.downloaded_bytes || 0);
    const progress = total > 0 ? `${formatSize(done)} / ${formatSize(total)}` : `${formatSize(done)}`;
    return `${stateLabel}（${progress}）`;
  }
  return stateLabel;
}

export function renderMlRuntime() {
  const panel = $('#mlRuntimePanel');
  const statusEl = $('#mlRuntimeStatus');
  const summaryEl = $('#mlRuntimeSummaryStatus');
  const retryBtn = $('#mlRuntimeRetryBtn');
  const sourceSelect = $('#mlDownloadSourceSelect');
  if (!panel || !statusEl) return;
  const status = state.mlRuntimeStatus || {};
  const settings = state.mlRuntimeSettings || {};
  const model = status.model_status || {};
  const cuda = status.cuda_status || {};
  const busy = Boolean(state.mlRuntimeSaving);

  const inlineStatus = $('#mlRuntimeInlineStatus');
  panel.classList.toggle('is-error', Boolean(status.error));
  if (status.error) {
    summaryEl.textContent = '读取失败';
    statusEl.innerHTML = `<div class="status-muted small">${escHtml(status.error)}</div>`;
    if (inlineStatus) {
      inlineStatus.textContent = String(status.error || '');
      inlineStatus.classList.add('is-error');
    }
    if (retryBtn) retryBtn.disabled = true;
    if (sourceSelect) sourceSelect.disabled = busy;
    return;
  }
  if (inlineStatus) {
    inlineStatus.textContent = '';
    inlineStatus.classList.remove('is-error');
  }

  const gpus = Array.isArray(status.gpus_detected) && status.gpus_detected.length
    ? status.gpus_detected.map(g => ({
        cuda: 'NVIDIA (CUDA)',
        openvino: 'Intel (OpenVINO)',
      }[g] || g)).join('、')
    : '未检测到可用 GPU';
  const modelState = mlArtifactStateLabel(model);
  const cudaState = mlArtifactStateLabel(cuda);
  const ortCore = status.ort_core || '未初始化';
  const plannedProvider = providerLabel(status.planned_provider, PROVIDER_LABELS, '未知');
  const actualProvider = providerLabel(status.actual_provider, ACTUAL_PROVIDER_LABELS, '未初始化');
  const requestedProvider = providerLabel(status.requested_provider, PROVIDER_LABELS, '未知');
  const session = status.character_recognition || {};
  const modelError = model.last_error ? `<div class="ml-runtime-error">${escHtml(String(model.last_error))}</div>` : '';
  const cudaError = cuda.last_error ? `<div class="ml-runtime-error">${escHtml(String(cuda.last_error))}</div>` : '';
  const providerError = status.provider_error || session.fallback_reason;
  const providerErrorHtml = providerError ? `<div class="ml-runtime-error">${escHtml(String(providerError))}</div>` : '';
  const restartNote = status.restart_required
    ? '<div class="ml-runtime-restart">需要重启应用后使用 CUDA（本进程 ONNX Runtime core 已锁定，无法热切换）</div>'
    : '';
  const sessionText = session.session_loaded
    ? `${session.provider || ''} ${session.active_device || ''}`.trim() || '就绪'
    : (session.reason === 'ccip_model_not_found' ? '模型未就绪' : '');
  const fallbackText = status.allow_cpu_fallback ? '允许' : '禁止';

  const gpuActive = ['CUDAExecutionProvider', 'OpenVINOExecutionProvider'].includes(status.actual_provider);
  const gpuPlanned = ['cuda', 'openvino'].includes(status.planned_provider);
  const sessionIdle = ['idle_unloaded', 'preparing'].includes(session.reason);
  const modelReady = model.state === 'ready';
  const modelText = status.download_in_progress ? 'AI 模型准备中' : (modelReady ? 'AI 模型已就绪' : 'AI 模型未就绪');
  const gpuText = gpuActive ? 'GPU 加速已启用'
    : (gpuPlanned && sessionIdle ? 'GPU 加速待加载' : 'GPU 加速未启用');
  summaryEl.textContent = (status.restart_required ? '需重启后生效，' : '') + `${modelText}，${gpuText}`;

  const rows = [
    ['检测到的 GPU', gpus],
    ['请求 Provider', requestedProvider],
    ['计划 Provider', plannedProvider],
    ['实际 Provider', actualProvider],
    ['ORT core', ortCore],
    ['CPU 回退', fallbackText],
    ['CCIP 模型', `${modelState}${status.custom_model_unmanaged ? '（自定义模型，不自动下载）' : ''}`],
    ['CUDA 运行时', cudaState],
  ];
  if (sessionText) rows.push(['推理会话', sessionText]);
  statusEl.innerHTML = rows.map(([label, value]) =>
    `<div class="ml-runtime-row"><span>${escHtml(label)}</span><b>${escHtml(value)}</b></div>`
  ).join('') + modelError + cudaError + providerErrorHtml + restartNote;

  if (sourceSelect) {
    sourceSelect.value = ['official', 'china'].includes(settings.download_source) ? settings.download_source : 'official';
    sourceSelect.disabled = busy;
  }
  if (retryBtn) {
    const canRetry = Boolean(
      !status.download_in_progress
      && (model.state === 'error' || model.state === 'not_started'
        || cuda.state === 'error' || cuda.state === 'not_started')
    );
    retryBtn.disabled = busy || !canRetry;
  }
}

export async function saveMlDownloadSource() {
  const select = $('#mlDownloadSourceSelect');
  if (!select || state.mlRuntimeSaving) return;
  const source = select.value;
  state.mlRuntimeSaving = true;
  if (select) select.disabled = true;
  const retryBtn = $('#mlRuntimeRetryBtn');
  if (retryBtn) retryBtn.disabled = true;
  try {
    const saved = await API.putJson('/api/ml-runtime/settings', {download_source: source});
    if (state.mlRuntimeSettings) {
      state.mlRuntimeSettings = {...state.mlRuntimeSettings, ...saved};
    }
    toast('下载源已保存', 'success');
    await loadMlRuntime();
  } catch (error) {
    toast(`保存下载源失败：${error.message || String(error)}`, 'error');
    renderMlRuntime();
  } finally {
    state.mlRuntimeSaving = false;
    if (select) select.disabled = false;
    renderMlRuntime();
  }
}

export async function retryMlRuntime() {
  const retryBtn = $('#mlRuntimeRetryBtn');
  if (!retryBtn || retryBtn.disabled || state.mlRuntimeSaving) return;
  retryBtn.disabled = true;
  try {
    const result = await API.post('/api/ml-runtime/retry');
    if (result && result.busy) {
      toast('已有准备任务在运行');
    } else {
      toast('已开始重新准备', 'success');
    }
    await loadMlRuntime();
  } catch (error) {
    toast(`重试失败：${error.message || String(error)}`, 'error');
    renderMlRuntime();
  }
}

export function renderFolderRenameAutoStatus() {
  const toggle = $('#folderRenameAutoExecuteToggle');
  if (!toggle) return;
  const status = state.folderRenameAuto;
  const hasError = Boolean(status && status.error);
  const saving = Boolean(status && status.saving);
  const runningAll = isActionBusy('archive-run-all');
  toggle.checked = Boolean(status && status.enabled);
  toggle.disabled = !status || hasError || saving || runningAll;
  const runAllButton = $('#folderRenameRunAllBtn');
  if (runAllButton) {
    runAllButton.disabled = runningAll;
    runAllButton.textContent = runningAll ? '整理中' : '立即整理全部';
  }
}

export async function loadErrorArtistsSummary(options = {}) {
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const result = await API.get('/api/folder-renames/error-artists?limit=1&offset=0', fetchOptions);
    state.errorArtistsTotal = Number.isFinite(Number(result?.total)) ? Number(result.total) : null;
  } catch (error) {
    if (isAbortError(error)) throw error;
    state.errorArtistsTotal = null;
  }
  return state.errorArtistsTotal;
}

function errorArtistsDialog() { return $('#errorArtistsDialog'); }

export function renderErrorArtistsDialog() {
  const list = $('#errorArtistsList');
  const more = $('#errorArtistsMore');
  const empty = $('#errorArtistsEmpty');
  if (!list || !more || !empty) return;
  list.innerHTML = state.errorArtists.map(row => `<button class="error-artist-row" type="button" data-error-artist-id="${Number(row.artist_id)}" data-error-latest-plan-id="${Number(row.latest_plan_id)}" title="${escHtml(row.reason || row.message || '需要人工处理')}">
    <span class="error-artist-name">${escHtml(row.artist_name || row.artist_id)}</span><strong>${Number(row.error_count || 0)}</strong><span class="error-artist-reason">${escHtml(row.message || row.reason || '需要人工处理')}</span>
  </button>`).join('');
  empty.hidden = state.errorArtistsLoading || state.errorArtists.length > 0;
  more.hidden = !state.errorArtistsHasMore || state.errorArtistsLoading;
  more.textContent = state.errorArtistsLoading ? '读取中' : '加载更多';
}

function resetErrorArtistsList() {
  state.errorArtists = [];
  state.errorArtistsOffset = 0;
  state.errorArtistsHasMore = false;
  state.errorArtistsScrollTop = 0;
  const scrollHost = $('#errorArtistsDialog .artist-links-dialog-body');
  if (scrollHost) scrollHost.scrollTop = 0;
  renderErrorArtistsDialog();
}

export async function loadErrorArtistsPage({reset = false} = {}) {
  if (state.errorArtistsLoading && !reset) return;
  if (reset) resetErrorArtistsList();
  if (!reset && !state.errorArtistsHasMore && state.errorArtists.length) return;
  const seq = ++state.errorArtistsRequestSeq;
  const scrollHost = $('#errorArtistsDialog .artist-links-dialog-body');
  const beforeScrollTop = scrollHost?.scrollTop || 0;
  const beforeScrollHeight = scrollHost?.scrollHeight || 0;
  state.errorArtistsLoading = true;
  renderErrorArtistsDialog();
  try {
    const params = new URLSearchParams({q: state.errorArtistsQuery, sort: state.errorArtistsSort, offset: String(state.errorArtistsOffset), limit: '50'});
    const result = await API.get('/api/folder-renames/error-artists?' + params);
    if (seq !== state.errorArtistsRequestSeq) return;
    const rows = Array.isArray(result?.artists) ? result.artists : [];
    const existing = new Set(state.errorArtists.map(row => Number(row.artist_id)));
    const next = rows.filter(row => !existing.has(Number(row.artist_id)));
    const removeCount = Math.max(0, state.errorArtists.length + next.length - 500);
    const removedHeight = removeCount
      ? Array.from($('#errorArtistsList')?.children || []).slice(0, removeCount).reduce((sum, row) => sum + row.getBoundingClientRect().height + 8, 0)
      : 0;
    state.errorArtists = [...state.errorArtists, ...next];
    if (state.errorArtists.length > 500) state.errorArtists = state.errorArtists.slice(-500);
    state.errorArtistsOffset = Number(result?.offset || 0) + rows.length;
    state.errorArtistsTotal = Number(result?.total ?? state.errorArtistsTotal);
    state.errorArtistsHasMore = Boolean(result?.has_more);
    if (scrollHost && beforeScrollHeight && removedHeight) {
      requestAnimationFrame(() => { scrollHost.scrollTop = Math.max(0, beforeScrollTop - removedHeight); });
    }
  } catch (error) {
    if (!isAbortError(error)) toast('读取出错画师失败', 'error');
  } finally {
    if (seq === state.errorArtistsRequestSeq) {
      state.errorArtistsLoading = false;
      renderErrorArtistsDialog();
    }
  }
}

export function openErrorArtistsDialog() {
  const dialog = errorArtistsDialog();
  if (!dialog || typeof dialog.showModal !== 'function') return;
  dialog.showModal();
  const scrollHost = $('#errorArtistsDialog .artist-links-dialog-body');
  if (scrollHost) requestAnimationFrame(() => { scrollHost.scrollTop = state.errorArtistsScrollTop || 0; });
  const search = $('#errorArtistsSearch');
  if (search) { search.value = state.errorArtistsQuery; search.focus(); }
  if (!state.errorArtists.length) loadErrorArtistsPage({reset: true}); else renderErrorArtistsDialog();
}

export function closeErrorArtistsDialog() {
  const dialog = errorArtistsDialog();
  const scrollHost = $('#errorArtistsDialog .artist-links-dialog-body');
  if (scrollHost) state.errorArtistsScrollTop = scrollHost.scrollTop;
  if (dialog?.open) dialog.close();
}

export async function jumpToErrorArtist(row) {
  const artistId = Number(row?.artist_id);
  const planId = Number(row?.latest_plan_id);
  logUiAction('error_artist_jump', {artist_id: artistId, plan_id: planId, at: Date.now() / 1000});
  closeErrorArtistsDialog();
  if (!state.artists.length) await loadArtists();
  const artist = state.artists.find(item => Number(item.id) === artistId);
  if (!artist) { toast('目标画师已不存在', 'error'); return; }
  try {
    await selectArtist(artistId, {history: false, loadItems: false});
    applyMode('moves');
    setMaintenanceView('organize');
    await loadArchiveWorkbench({view: 'organize'});
    const target = $(`[data-archive-plan-id="${String(planId)}"]`);
    if (target) {
      target.classList.add('error-plan-highlight');
      target.scrollIntoView({block: 'center', behavior: 'smooth'});
      setTimeout(() => target.classList.remove('error-plan-highlight'), 3500);
    } else toast('整理计划不在当前列表中', 'error');
  } catch (error) { toast('打开文件整理失败', 'error'); }
}

export async function loadFolderRenameAutoStatus(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const status = await API.get('/api/folder-renames/auto', fetchOptions);
    if (updateState) state.folderRenameAuto = status;
    return status;
  } catch (e) {
    if (isAbortError(e)) throw e;
    const status = {enabled: false, error: e.message || String(e)};
    if (updateState) state.folderRenameAuto = status;
    return status;
  } finally {
    if (render) renderFolderRenameAutoStatus();
  }
}

export async function setFolderRenameAutoEnabled(enabled) {
  const previous = state.folderRenameAuto;
  state.folderRenameAuto = {...(previous || {}), enabled, saving: true};
  renderFolderRenameAutoStatus();
  try {
    const status = await API.putJson('/api/folder-renames/auto', {enabled});
    state.folderRenameAuto = status;
    renderFolderRenameAutoStatus();
    toast(enabled ? '自动整理已开启' : '自动整理已关闭', 'success');
  } catch (e) {
    state.folderRenameAuto = previous || {enabled: !enabled};
    renderFolderRenameAutoStatus();
    toast('自动整理设置失败：' + (e.message || e), 'error');
  }
}

export async function runFolderRenameAllNow() {
  if (isActionBusy('archive-run-all')) return;
  const confirmed = window.confirm('确定立即整理所有画师吗？\n系统会先备份数据库，并重新检查所有待整理项及之前的失败项。');
  logUiAction('archive_run_all_confirm', {confirmed});
  if (!confirmed) return;
  setActionBusy('archive-run-all', '', true);
  renderFolderRenameAutoStatus();
  // Bounded wait: a hung request previously kept the button stuck at
  // 整理中 forever. Ten minutes covers large libraries with backups.
  const controller = new AbortController();
  const abortTimer = setTimeout(() => controller.abort(), 10 * 60 * 1000);
  try {
    const response = await fetch('/api/folder-renames/execute-all', {
      method: 'POST',
      signal: controller.signal,
    });
    const result = await API.parseResponse(response);
    await loadMoveWorkbench({preserveScroll: true});
    const executed = Number(result.executed_count || 0);
    const failed = Number(result.failed_count || 0);
    logUiAction('archive_run_all_result', {
      status: String(result.status || ''),
      executed_count: executed,
      failed_count: failed,
      skipped_count: Number(result.skipped_count || 0),
      retried_count: Number(result.retried_count || 0),
    });
    if (failed) {
      toast(`全库整理完成：成功 ${executed} 项，失败 ${failed} 项`, 'error');
    } else {
      toast(executed ? `全库整理完成：${executed} 项` : '当前没有可执行的整理项', executed ? 'success' : 'info');
    }
  } catch (e) {
    logUiAction('archive_run_all_result', {status: 'error', error: e.message || String(e)});
    toast('全库整理失败：' + (e.message || e), 'error');
  } finally {
    clearTimeout(abortTimer);
    setActionBusy('archive-run-all', '', false);
    renderFolderRenameAutoStatus();
  }
}

export function renderDimensionBackfillStatus() {
  const btn = $('#dimensionBackfillBtn');
  const result = $('#dimensionBackfillResult');
  if (!btn || !result) return;
  const status = state.dimensionBackfill || {};
  const busy = isActionBusy('item-dimensions-backfill');
  const running = Boolean(status.running);
  btn.disabled = busy || running;
  btn.textContent = running || busy ? '补全中' : '补全媒体尺寸';
  if (status.error) {
    result.textContent = `补全失败：${status.error}`;
    result.style.color = 'var(--status-danger)';
  } else if (running) {
    const processed = Number(status.processed || 0);
    const updated = Number(status.updated || 0);
    const remaining = Number(status.remaining || 0);
    result.textContent = `已处理 ${processed} 项，已补全 ${updated} 项${remaining ? `，剩余 ${remaining} 项` : ''}`;
    result.style.color = '';
  } else if (status.complete) {
    result.textContent = Number(status.updated || 0)
      ? `已补全 ${Number(status.updated)} 项`
      : '媒体尺寸已完整';
    result.style.color = 'var(--status-ok)';
  } else if (status.cursor_done && Number(status.remaining || 0) > 0) {
    result.textContent = `已补全 ${Number(status.updated || 0)} 项，${Number(status.remaining)} 项无法读取`;
    result.style.color = 'var(--status-danger)';
  }
}

export async function backfillItemDimensions() {
  if (isActionBusy('item-dimensions-backfill') || state.dimensionBackfill?.running) return;
  setActionBusy('item-dimensions-backfill', '', true);
  logUiAction('item_dimensions_backfill_start', {});
  try {
    const status = await API.post('/api/items/dimensions/backfill');
    state.dimensionBackfill = status;
    renderDimensionBackfillStatus();
    scheduleDimensionBackfillPoll();
    const incomplete = status.cursor_done && !status.complete && Number(status.remaining || 0) > 0;
    toast(
      status.error ? '媒体尺寸补全失败'
        : status.running ? '已开始后台补全'
          : status.complete ? '媒体尺寸已完整'
            : incomplete ? '媒体尺寸补全未完成' : '补全已结束',
      status.error || incomplete ? 'error' : 'success',
    );
  } catch (e) {
    state.dimensionBackfill = {running: false, complete: false, error: e.message || String(e)};
    renderDimensionBackfillStatus();
    logUiAction('item_dimensions_backfill_result', {status: 'error', error: e.message || String(e)});
    toast('媒体尺寸补全失败', 'error');
  } finally {
    setActionBusy('item-dimensions-backfill', '', false);
    renderDimensionBackfillStatus();
  }
}

export function renderOverviewActions() {
  const pathsHint = $('#overviewPathsHint');
  if (!pathsHint) return;
  const pending = Number(state.movePendingTotal || 0);
  const waiting = Number(state.moveWaitingHashCount || 0)
    + Number(state.hashStatus?.scan_candidates?.remaining || 0);
  const hints = [];
  if (pending > 0) hints.push(`${pending} 项待确认`);
  if (waiting > 0) hints.push(`${waiting} 项正在自动入库`);
  if (hints.length > 0) {
    pathsHint.textContent = hints.join('，');
    pathsHint.classList.toggle('is-attention', pending > 0);
  } else {
    pathsHint.textContent = '当前没有待确认的路径';
    pathsHint.classList.remove('is-attention');
  }
}

function isHealthObject(value) {
  return Boolean(value && typeof value === 'object' && !Array.isArray(value));
}

function healthNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function formatHealthScanStatus(scan) {
  if (!isHealthObject(scan) || (!scan.status && !scan.phase)) return '扫描状态就绪';
  const isError = scan.status === 'error' || scan.phase === 'error' || scan.phase === 'failed';
  if (isError) {
    const errorMsg = scan.current_path ? String(scan.current_path).replace(/^error:\s*/i, '').trim() : '';
    return errorMsg ? `扫描失败 \u00b7 ${errorMsg}` : '扫描失败';
  }
  const isPartial = scan.phase === 'partial';
  if (isPartial) {
    const errorMsg = scan.current_path ? String(scan.current_path).replace(/^error:\s*/i, '').trim() : '';
    return errorMsg ? `扫描部分完成 \u00b7 ${errorMsg}` : '扫描部分完成';
  }
  const statusLabels = {
    idle: '空闲',
    scanning: '正在扫描',
    error: '出错',
    stopping: '正在停止',
  };
  const phaseLabels = {
    complete: '已完成',
    discover: '发现目录',
    scan: '扫描文件',
    parse: '整理记录',
    stopped: '已停止',
    interrupted: '已中断',
    partial: '部分完成',
    failed: '失败',
  };
  const status = statusLabels[scan.status] || '未知状态';
  const phase = phaseLabels[scan.phase] || '';
  const scanned = healthNumber(scan.scanned_count);
  const total = healthNumber(scan.total_estimate);
  const showCount = total != null && total > 0 && scanned != null
    && (scan.status === 'scanning' || scan.phase === 'complete');
  const count = showCount ? `${scanned}/${total}` : '';
  const missing = scan.phase === 'complete' && showCount && scanned < total ? `未扫描 ${total - scanned}` : '';
  return joinUiMeta([status, phase, count, missing]);
}

function formatHealthHashStatus(hash) {
  if (!isHealthObject(hash) || !isHealthObject(hash.items) || !isHealthObject(hash.scan_candidates)) {
    return '查重状态就绪';
  }
  const itemRemaining = healthNumber(hash.items.remaining);
  const candidateRemaining = healthNumber(hash.scan_candidates.remaining);
  const itemErrors = healthNumber(hash.items.error);
  const candidateErrors = healthNumber(hash.scan_candidates.error);
  if ([itemRemaining, candidateRemaining, itemErrors, candidateErrors].some(value => value == null)) {
    return '查重就绪';
  }
  const remaining = itemRemaining + candidateRemaining;
  const errors = itemErrors + candidateErrors;
  if (remaining === 0 && errors === 0) return joinUiMeta(['已完成查重', '无重复错误']);
  const parts = [remaining ? `剩余 ${remaining} 项待比对` : '查重完成'];
  parts.push(errors ? `${errors} 项异常` : '无错误');
  return joinUiMeta(parts);
}

function formatHealthSchedule(schedule, nextKey) {
  if (!isHealthObject(schedule)) return '等待排期';
  if (schedule.error || schedule.last_error) return '排期读取失败';
  if (schedule.enabled === false) return '自动排期未开启';
  if (schedule.enabled !== true) return '等待排期';
  const nextAt = schedule[nextKey];
  const interval = healthNumber(schedule.interval);
  const intervalHours = interval ? Math.round(interval / 3600 * 10) / 10 : 0;
  if (!nextAt) return intervalHours ? joinUiMeta([`每 ${intervalHours} 小时`, '等待排期']) : '等待排期';
  const prefix = schedule.overdue ? '已到执行时间' : `下次预计 ${formatHealthTime(nextAt)}`;
  const suffix = schedule.deferred_by_manual ? '手动后顺延' : '';
  return intervalHours ? joinUiMeta([prefix, `每 ${intervalHours} 小时`, suffix]) : joinUiMeta([prefix, suffix]);
}

export function renderHealthSummary() {
  const grid = $('#healthGrid');
  if (!grid) return;
  const health = state.healthSummary;
  if (!health) {
    grid.innerHTML = '<div class="maintenance-card status-card status-muted move-empty small">系统健康状态读取中</div>';
    return;
  }
  if (health.error) {
    grid.innerHTML = `<div class="maintenance-card status-card status-danger"><b>状态检查失败</b><span>${escHtml(health.error)}</span></div>`;
    return;
  }
  // Status summary plus secondary detail: the first lines stay visible, the
  // rest folds into a native <details> so touch and keyboard users can open
  // the full text without relying on a hover-only title.
  const healthSummaryDetail = (lines, primaryCount) => {
    const visible = (lines || []).filter(Boolean);
    const primary = visible.slice(0, primaryCount).join(' \u00b7 ');
    const rest = visible.slice(primaryCount);
    const title = ` title="${escHtml(visible.join(' \u00b7 '))}"`;
    if (!rest.length) return `<span${title}>${escHtml(primary)}</span>`;
    return `<span${title}>${escHtml(primary)}</span>`
      + `<details class="health-detail"><summary>详情</summary>`
      + rest.map(line => `<span>${escHtml(line)}</span>`).join('')
      + `</details>`;
  };
  const database = health.database;
  const backups = health.backups;
  const latestBackup = isHealthObject(backups) && isHealthObject(backups.latest) ? backups.latest : null;
  const logs = health.logs;
  const galleryLog = isHealthObject(logs) ? logs.gallery_log : null;
  const uiLog = isHealthObject(logs) ? logs.ui_actions_log : null;
  const scan = health.scan;
  const scanSchedule = health.scan_schedule;
  const backupSchedule = health.backup_schedule;
  const folderArchive = health.folder_archive;
  const hash = health.hash;
  const errors = health.recent_errors;
  const degradedReasons = Array.isArray(health.degraded_reasons) ? health.degraded_reasons : [];
  const databaseKnown = isHealthObject(database);
  const backupsKnown = isHealthObject(backups);
  const folderArchiveKnown = isHealthObject(folderArchive);
  const errorArtistsCount = state.errorArtistsTotal;
  const logsKnown = isHealthObject(logs) && isHealthObject(galleryLog) && isHealthObject(uiLog);
  const scanKnown = isHealthObject(scan) && Boolean(scan.status || scan.phase);
  const hashKnown = isHealthObject(hash) && isHealthObject(hash.items) && isHealthObject(hash.scan_candidates);
  const hashItemRemaining = hashKnown ? healthNumber(hash.items.remaining) : null;
  const hashCandidateRemaining = hashKnown ? healthNumber(hash.scan_candidates.remaining) : null;
  const hashItemErrors = hashKnown ? healthNumber(hash.items.error) : null;
  const hashCandidateErrors = hashKnown ? healthNumber(hash.scan_candidates.error) : null;
  const hashRemaining = hashItemRemaining != null && hashCandidateRemaining != null ? hashItemRemaining + hashCandidateRemaining : null;
  const hashErrors = hashItemErrors != null && hashCandidateErrors != null ? hashItemErrors + hashCandidateErrors : null;
  const hashCountsKnown = hashRemaining != null && hashErrors != null;
  const databaseStatus = !databaseKnown
    ? 'status-muted'
    : (health.database_error || health.schema_error ? 'status-danger' : (database.exists !== true ? 'status-danger' : 'status-ok'));
  const backupCount = backupsKnown ? healthNumber(backups.count) : null;
  const backupRetainedCount = backupsKnown ? healthNumber(backups.retained_count ?? backups.count) : null;
  const backupTotalBytes = backupsKnown ? healthNumber(backups.total_size_bytes) : null;
  const backupStatus = !backupsKnown
    ? 'status-muted'
    : (backups.error || (backupSchedule && backupSchedule.last_error) ? 'status-danger' : (backupCount == null ? 'status-muted' : (backupCount > 0 && latestBackup ? 'status-ok' : 'status-warn')));
  const scanIncomplete = scanKnown && scan.phase === 'complete'
    && healthNumber(scan.scanned_count) != null
    && healthNumber(scan.total_estimate) != null
    && scan.scanned_count < scan.total_estimate;
  const scanIsError = scanKnown && (scan.status === 'error' || scan.phase === 'error' || scan.phase === 'failed');
  const scanIsWarn = scanIncomplete || scan.phase === 'interrupted' || scan.phase === 'partial';
  const scanStatus = !scanKnown
    ? 'status-muted'
    : (scanIsError ? 'status-danger' : (scanIsWarn ? 'status-warn' : (scan.status === 'scanning' ? 'status-info' : 'status-ok')));
  const hashStatus = !hashCountsKnown || hash.blake3_available == null
    ? 'status-muted'
    : (health.database_error || hash.blake3_available === false ? 'status-danger' : (hashRemaining > 0 ? 'status-info' : (hashErrors > 0 ? 'status-warn' : 'status-ok')));
  const logStatus = !logsKnown
    ? 'status-muted'
    : (galleryLog.error || uiLog.error ? 'status-danger' : (galleryLog.exists === false && uiLog.exists === false ? 'status-warn' : 'status-ok'));
  const errorsKnown = Array.isArray(errors);
  const errorStatus = !errorsKnown ? 'status-muted' : (errors.length ? 'status-warn' : 'status-ok');
  const archiveFailedPlans = folderArchiveKnown ? healthNumber(folderArchive.failed_plans) : null;
  const archiveStatus = !folderArchiveKnown || archiveFailedPlans == null ? 'status-muted' : (archiveFailedPlans > 0 ? 'status-danger' : 'status-ok');
  const overallStatus = health.degraded === true ? 'status-danger' : 'status-ok';
  const overallText = health.degraded === true ? `需要处理${degradedReasons.length ? ` \u00b7 ${degradedReasons.join('、')}` : ''}` : '系统运行正常';
  const databaseText = !databaseKnown
    ? ['数据库状态未上报']
    : [
        health.database_error ? '数据库读取失败' : (health.schema_error ? '数据库结构检查失败' : (database.exists === true ? '数据库正常' : '数据库文件不存在')),
        database.size_bytes != null ? formatBytes(database.size_bytes) : '',
      ];
  const databaseStorageText = databaseKnown ? [
    database.page_size_bytes != null ? `页大小 ${formatBytes(database.page_size_bytes)}` : '页大小未上报',
    database.page_count != null ? `共 ${database.page_count} 页` : '页数未上报',
    database.free_pages != null ? `空闲页 ${database.free_pages}` : '空闲页未上报',
    database.reclaimable_bytes != null ? `数据库内空闲 ${formatBytes(database.reclaimable_bytes)}` : '空闲容量未上报',
    database.reclaimable_bytes != null ? '后续写入会复用，当前不会自动缩小数据库文件。' : '',
    database.wal_size_bytes != null ? `WAL ${formatBytes(database.wal_size_bytes)}` : 'WAL 大小未上报',
    database.storage_error ? '存储诊断不完整' : '',
  ] : [];
  databaseText.push(...databaseStorageText);
  const backupText = !backupsKnown
    ? ['备份状态未上报']
    : backups.error
      ? ['备份读取失败']
    : backupCount == null
      ? ['备份数量未上报']
      : backupCount === 0
        ? ['暂无备份', `当前保留 ${backupRetainedCount ?? 0} 份`]
        : latestBackup
          ? [
              latestBackup.name || '最近备份',
              latestBackup.updated_at ? `备份于 ${formatHealthTime(latestBackup.updated_at)}` : '',
              `自动保留 ${backupRetainedCount ?? backupCount} 份`,
              backupTotalBytes != null ? `占用 ${formatBytes(backupTotalBytes)}` : '备份占用未上报',
            ].filter(Boolean)
          : ['最近备份未上报', `当前保留 ${backupRetainedCount ?? backupCount} 份`];
  const logText = !logsKnown
    ? ['日志状态未上报']
    : galleryLog.error || uiLog.error
      ? ['日志读取失败']
    : galleryLog.exists === false && uiLog.exists === false
      ? ['日志文件未创建']
      : [
          galleryLog.size_bytes != null && uiLog.size_bytes != null ? `共 ${formatBytes(galleryLog.size_bytes + uiLog.size_bytes)}` : '日志大小未上报',
          galleryLog.exists === true ? `应用 ${formatBytes(galleryLog.size_bytes)}` : '应用日志文件未创建',
          uiLog.exists === true ? `浏览器 ${formatBytes(uiLog.size_bytes)}` : '浏览器日志文件未创建',
        ];
  const errorHtml = !errorsKnown
    ? '<span>错误状态未上报</span>'
    : errors.length
      ? `<details class="error-lines"><summary>最近 ${errors.length} 条错误</summary>${errors.map(row => `<code>${escHtml(row.source || '')}: ${escHtml(row.line || '')}</code>`).join('')}</details>`
      : '<span>最近没有错误记录</span>';
  grid.innerHTML = `
    <div class="maintenance-card status-card ${overallStatus}"><b>整体状态</b><span title="${escHtml(overallText)}">${escHtml(overallText)}</span></div>
    <div class="maintenance-card status-card ${databaseStatus}"><b>数据库</b>${healthSummaryDetail(databaseText, 2)}</div>
    <div class="maintenance-card status-card ${backupStatus}"><b>最近备份</b>${healthSummaryDetail(backupText, 1)}<span class="health-schedule">${escHtml(formatHealthSchedule(backupSchedule, 'next_run_at'))}</span></div>
    <div class="maintenance-card status-card ${scanStatus}"><b>扫描</b><span title="${escHtml(formatHealthScanStatus(scan))}">${escHtml(formatHealthScanStatus(scan))}</span><span class="health-schedule">${escHtml(formatHealthSchedule(scanSchedule, 'next_auto_scan_at'))}</span></div>
    <div class="maintenance-card status-card ${hashStatus}"><b>相同文件检查</b><span title="${escHtml(formatHealthHashStatus(hash))}">${escHtml(formatHealthHashStatus(hash))}</span></div>
    <div class="maintenance-card status-card ${archiveStatus}"><b>文件夹整理</b><span title="${escHtml(archiveFailedPlans == null ? '状态未上报' : (archiveFailedPlans ? `执行失败 ${archiveFailedPlans}` : '无执行失败'))}">${escHtml(archiveFailedPlans == null ? '状态未上报' : (archiveFailedPlans ? `执行失败 ${archiveFailedPlans}` : '无执行失败'))}</span></div>
    <button class="maintenance-card status-card ${errorArtistsCount ? 'status-danger' : 'status-ok'} error-artists-card" type="button" data-error-artists-open aria-haspopup="dialog" title="点击查看出错画师列表及处理指引"><b>出错画师（按画师）</b><span title="${escHtml(errorArtistsCount == null ? '状态未上报' : (errorArtistsCount ? `${errorArtistsCount} 位画师` : '暂无出错画师'))}">${escHtml(errorArtistsCount == null ? '状态未上报' : (errorArtistsCount ? `${errorArtistsCount} 位画师` : '暂无出错画师'))}</span></button>
    <div class="maintenance-card status-card ${logStatus}"><b>日志</b>${healthSummaryDetail(logText, 1)}</div>
    <div class="maintenance-card status-card ${errorStatus}"><b>最近错误</b><span>${errorHtml}</span></div>
  `;
  renderHashStatus();
  renderDimensionBackfillStatus();
  renderOverviewActions();
}

export function renderHashStatus() {
  const status = state.hashStatus;
  if (!status) {
    $('#hashStatusText').textContent = '等待检查';
    return;
  }
  if (status.database_error) {
    $('#hashStatusText').textContent = '数据库异常';
    return;
  }
  if (!status.blake3_available) {
    $('#hashStatusText').textContent = '暂时不能检查';
    return;
  }
  const items = status.items || {};
  const candidates = status.scan_candidates || {};
  const itemRemaining = Number(items.remaining || 0);
  const importRemaining = Number(candidates.remaining || 0);
  const remaining = itemRemaining + importRemaining;
  $('#hashStatusText').textContent = importRemaining > 0
    ? joinUiMeta(['正在自动入库', `还剩 ${importRemaining}`])
    : formatHashWorkerStatus(status.worker || {}, remaining);
}

function formatHashWorkerStatus(worker, remaining) {
  if (worker.last_error) return '检查异常';
  if (remaining <= 0) return '检查完成';
  if (worker.thread_alive) return joinUiMeta(['正在检查', `还剩 ${remaining}`]);
  return joinUiMeta(['等待检查', `还剩 ${remaining}`]);
}

export async function startFullScan(event) {
  const source = event?.currentTarget?.id === 'emptyScanBtn' ? 'empty' : 'header';
  if (isActionBusy('scan-full')) return;
  setActionBusy('scan-full', '', true);
  logUiAction('scan_start_click', {source});
  try {
    const r = await API.post('/api/scan');
    if (r.ok) {
      state.scanRunning = true;
      state.lastScanState = {status:'scanning', phase:'discover', scanned_count:0, total_estimate:0, current_path:''};
      renderLibraryEmptyState();
      toast('扫描已启动', 'success');
    } else {
      toast(r.message || '扫描已在运行', 'error');
    }
  } catch (e) {
    // 409: another scan or file operation owns the slot.
    if (e && e.status === 409) {
      toast('扫描已在运行', 'error');
    } else {
      toast('启动扫描失败', 'error');
    }
  } finally {
    setActionBusy('scan-full', '', false);
  }
}

// Late import closing the overview -> sidebar cycle; startFullScan only
// touches the empty state inside its success branch.
import { loadItemsPreservingDepth, renderLibraryEmptyState } from '../sidebar.js';
