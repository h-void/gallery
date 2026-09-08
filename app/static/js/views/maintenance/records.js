// Maintenance records (records): recycle bin, operation history, and the
// runtime error log panel.

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy, isCurrentRequestSeq, nextRequestSeq } from '../../store.js';
import { $, escHtml, joinUiMeta, formatHealthTime, isAbortError } from '../../utils.js';
import { toast } from '../../logging.js';
import { refreshCurrentView } from '../../events.js';

export async function loadOperationLog(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const log = await API.get('/api/operation-log?limit=80&error_limit=40', fetchOptions);
    if (updateState) state.operationLog = log;
    if (render) renderOperationLog();
    return log;
  } catch (e) {
    if (isAbortError(e)) throw e;
    const log = {history: [], errors: [], error: e.message};
    if (updateState) state.operationLog = log;
    if (render) renderOperationLog();
    return log;
  }
}

function recycleEntries(payload = state.recycleBin) {
  if (Array.isArray(payload)) return payload;
  return Array.isArray(payload?.entries) ? payload.entries : [];
}

function recycleFileExists(entry) {
  return entry?.recycled_file_exists !== false;
}

function recycleOriginalExists(entry) {
  return entry?.original_file_exists === true;
}

function recycleStatusLabel(status) {
  const labels = {
    recycled: '可恢复',
    restored: '已恢复',
    missing: '文件缺失',
    error: '需要处理',
  };
  return labels[String(status || 'recycled')] || String(status || '可恢复');
}

function recycleStatusClass(status) {
  return ['recycled', 'restored', 'missing', 'error'].includes(status) ? status : 'unknown';
}

export async function loadRecycleBin(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  const append = Boolean(options.append);
  const previous = state.recycleBin && typeof state.recycleBin === 'object' ? state.recycleBin : null;
  const offset = append ? Number(previous?.next_offset) : 0;
  if (append && (!Number.isFinite(offset) || offset < 0)) return previous;
  const seq = nextRequestSeq('recycleLoadSeq');
  try {
    const recycleBin = await API.get(
      `/api/recycle?status=recycled&limit=80&offset=${encodeURIComponent(offset)}`,
      fetchOptions
    );
    if (!isCurrentRequestSeq('recycleLoadSeq', seq)) return state.recycleBin;
    if (updateState) {
      const entries = append ? [...recycleEntries(previous), ...recycleEntries(recycleBin)] : recycleEntries(recycleBin);
      state.recycleBin = {...recycleBin, entries};
    }
    if (render && isCurrentRequestSeq('recycleLoadSeq', seq)) renderRecycleBin();
    return recycleBin;
  } catch (e) {
    if (isAbortError(e)) throw e;
    if (!isCurrentRequestSeq('recycleLoadSeq', seq)) return state.recycleBin;
    const recycleBin = {entries: [], error: e.message || String(e)};
    if (updateState) state.recycleBin = recycleBin;
    if (render) renderRecycleBin();
    return recycleBin;
  }
}

export function renderRecycleBin() {
  const summary = $('#recycleBinSummary');
  const list = $('#recycleBinList');
  const more = $('#recycleBinMore');
  if (!summary || !list || !more) return;
  const recycleBin = state.recycleBin;
  if (!recycleBin) {
    summary.textContent = '回收站读取中';
    list.innerHTML = '<div class="move-empty small">回收站读取中</div>';
    more.innerHTML = '';
    return;
  }
  if (recycleBin.error) {
    summary.textContent = '回收站读取失败';
    list.innerHTML = `<div class="operation-error">${escHtml(recycleBin.error)}</div>`;
    more.innerHTML = '';
    return;
  }
  const entries = recycleEntries(recycleBin);
  const total = Number(recycleBin.total ?? entries.length);
  const unavailable = entries.filter(entry => !recycleFileExists(entry)).length;
  const occupied = entries.filter(recycleOriginalExists).length;
  summary.textContent = joinUiMeta([
    `已显示 ${entries.length}/${total} 项`,
    unavailable ? `${unavailable} 个文件已缺失` : '',
    occupied ? `${occupied} 个原路径已占用` : '',
  ]);
  list.innerHTML = entries.length ? entries.map(entry => {
    const id = Number(entry.id);
    const originalPath = String(entry.original_path || entry.restore_path || '');
    const recycledPath = String(entry.recycled_path || '');
    const fileExists = recycleFileExists(entry);
    const originalExists = recycleOriginalExists(entry);
    const status = String(entry.status || 'recycled');
    const displayStatus = !fileExists ? 'missing' : (originalExists ? 'error' : status);
    const displayStatusLabel = !fileExists
      ? '文件缺失'
      : (originalExists ? '路径占用' : recycleStatusLabel(status));
    const restoring = isActionBusy('recycle-restore', String(id));
    const canRestore = Number.isFinite(id) && status === 'recycled' && fileExists && !originalExists && !restoring;
    const fileState = !fileExists
      ? '文件已缺失'
      : (originalExists ? '原路径已被占用（不可恢复，避免覆盖现有文件）' : '可以恢复');
    const restoreTitle = originalExists
      ? '原路径已被同名文件占用，禁止恢复以防误覆盖现有文件'
      : (!fileExists ? '回收站文件已缺失' : '恢复到原始路径');
    return `
      <div class="recycle-bin-row${canRestore ? '' : ' blocked'}">
        <div class="recycle-bin-row-head">
          <code title="${escHtml(originalPath)}">${escHtml(originalPath || '-')}</code>
          <span class="recycle-bin-status ${recycleStatusClass(displayStatus)}">${escHtml(displayStatusLabel)}</span>
        </div>
        <div class="recycle-bin-meta">
          <span>${escHtml(fileState)}</span>
          ${entry.created_at ? `<span>${escHtml(formatHealthTime(entry.created_at))}</span>` : ''}
        </div>
        ${recycledPath ? `<code class="recycle-bin-location" title="${escHtml(recycledPath)}">${escHtml(recycledPath)}</code>` : ''}
        <div class="recycle-bin-actions">
          <span>${entry.file_name ? escHtml(String(entry.file_name)) : ''}</span>
          <button class="btn btn-ops" type="button" data-recycle-restore="${id}" ${canRestore ? '' : 'disabled'} title="${escHtml(restoreTitle)}" ${restoring ? 'aria-busy="true"' : ''}>${restoring ? '恢复中' : '恢复'}</button>
        </div>
      </div>`;
  }).join('') : '<div class="move-empty small empty-state">回收站中暂无可恢复的文件</div>';
  // Backend returns numeric next_offset only when another page exists; the
  // empty bin sends null, which must not surface a load-more button.
  const hasNextPage = entries.length > 0 && typeof recycleBin.next_offset === 'number';
  more.innerHTML = hasNextPage
    ? '<button class="btn btn-ghost" type="button" data-recycle-load-more>加载更多</button>'
    : '';
}

export async function restoreRecycleEntry(entryId) {
  const id = Number(entryId);
  const entry = recycleEntries().find(item => Number(item.id) === id);
  if (!entry || !Number.isFinite(id) || isActionBusy('recycle-restore', String(id))) return;
  const originalPath = String(entry.original_path || entry.restore_path || '');
  if (!window.confirm(`确定要将文件恢复至原始路径吗？\n${originalPath}`)) return;
  setActionBusy('recycle-restore', String(id), true);
  renderRecycleBin();
  try {
    const result = await API.post(`/api/recycle/${id}/restore`);
    try {
      await refreshCurrentView({reason: 'recycle_restore'});
    } catch (_) {
      await Promise.all([
        loadRecycleBin({render: false}),
        loadOperationLog({render: false}),
      ]);
      renderRecycleBin();
      renderOperationLog();
    }
    toast(result.message || '文件已恢复到原始路径', 'success');
  } catch (e) {
    if (e.status === 409 || e.status === 404) await loadRecycleBin();
    toast('恢复失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('recycle-restore', String(id), false);
    renderRecycleBin();
  }
}

function operationLogKindLabel(operation) {
  if (operation.reason === 'tagged_file') return '已标签文件归位';
  if (operation.kind === 'folder_rename_undo' || operation.reason === 'folder_rename_undo' || operation.reason === 'undo') return '撤销文件夹整理';
  if (operation.kind === 'folder_rename') return '文件夹整理';
  if (operation.kind === 'move') return '路径变更';
  return '操作';
}

function operationLogReasonLabel(reason) {
  const labels = {
    tagged_file: '已标签文件归位',
    folder_rename: '文件夹整理',
    folder_rename_undo: '已撤销文件夹整理',
    undo: '已撤销文件夹整理',
    backup_failed: '数据库备份失败',
    source_missing: '来源文件夹不存在',
    target_exists: '目标已存在',
    bad_folder_path: '文件夹路径无效',
    db_update_failed: '数据库路径更新失败',
    outside_artist: '路径不在画师目录内',
    permission_denied: '文件夹没有改名权限',
    execution_failed: '执行失败',
    blocked: '当前不安全，已跳过',
  };
  return labels[reason] || reason || '';
}

function operationLogStatusLabel(status) {
  const labels = {
    applied: '已确认',
    executed: '已执行',
    reverted: '已撤销',
    preview: '自动确认',
    new: '新文件',
    ignored: '已忽略',
    failed: '失败',
    error: '失败',
  };
  return labels[status] || status || '已记录';
}

function emptyFolderCleanupStatusLabel(record) {
  if (!record) return '未处理';
  if (record.status === 'deleted') return '已删除空文件夹';
  const reasons = {
    missing: '目录不存在',
    not_empty: '目录非空',
    outside_cleanup_root: '超出清理范围',
  };
  return `未删除空文件夹${record.reason ? `：${reasons[record.reason] || record.reason}` : ''}`;
}

function renderEmptyFolderCleanup(records) {
  const cleanup = (records || []).filter(record => record && record.path);
  if (!cleanup.length) return '';
  return `<div class="empty-folder-cleanup">${
    cleanup.map(record => `
      <div class="empty-folder-cleanup-row ${escHtml(record.status || 'skipped')}">
        <span>${escHtml(emptyFolderCleanupStatusLabel(record))}</span>
        <code title="${escHtml(record.path)}">${escHtml(record.path)}</code>
      </div>
    `).join('')
  }</div>`;
}

function renderOperationEntries(operations, emptyText) {
  if (!operations.length) return `<div class="move-empty small">${escHtml(emptyText)}</div>`;
  return operations.map(operation => {
    const source = operation.display_source || operation.source || '';
    const target = operation.display_target || operation.target || '';
    const isFailed = ['failed', 'error'].includes(String(operation.status || '').toLowerCase());
    const kindClass = isFailed
      ? 'failed'
      : (operation.kind === 'folder_rename_undo' || operation.reason === 'folder_rename_undo' || operation.reason === 'undo'
        ? 'undo'
        : (operation.kind === 'folder_rename' ? 'rename' : (operation.kind === 'move' ? 'move' : 'other')));
    return `
      <div class="operation-entry ${kindClass}">
        <div class="operation-entry-head">
          <b>${escHtml(operationLogKindLabel(operation))}</b>
          <span class="${isFailed ? 'is-failed' : ''}">${escHtml(operationLogStatusLabel(operation.status))}</span>
          <em>${escHtml(formatHealthTime(operation.at))}</em>
        </div>
        <div class="operation-entry-meta">
          <span>${escHtml(operation.artist_name || '未知画师')}</span>
          <span>${operation.updated_items || 0} 项</span>
          <span>${escHtml(operationLogReasonLabel(operation.reason))}</span>
        </div>
        ${operation.message ? `<div class="operation-entry-message">${escHtml(operation.message)}</div>` : ''}
        <div class="operation-path"><span>原</span><code title="${escHtml(operation.source || source)}">${escHtml(source || '-')}</code></div>
        <div class="operation-path"><span>新</span><code title="${escHtml(operation.target || target)}">${escHtml(target || '-')}</code></div>
        ${renderEmptyFolderCleanup(operation.empty_folders || [])}
      </div>
    `;
  }).join('');
}

export function setOperationHistoryFilter(filter = 'all') {
  const grid = $('#operationLogGrid');
  if (grid) grid.dataset.filter = filter;
  const buttons = document.querySelectorAll('#operationHistoryPanel [data-operation-filter]');
  buttons.forEach(btn => {
    const active = btn.dataset.operationFilter === filter;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-selected', active ? 'true' : 'false');
  });
}

function syncOperationFilterCounts(total, failed, success) {
  const btnAll = $('#operationFilterAll');
  const btnFailed = $('#operationFilterFailed');
  const btnSuccess = $('#operationFilterSuccess');
  if (btnAll) btnAll.textContent = `全部（${total}）`;
  if (btnFailed) btnFailed.textContent = `仅失败（${failed}）`;
  if (btnSuccess) btnSuccess.textContent = `仅成功（${success}）`;
}

export function renderOperationLog() {
  const summary = $('#operationLogSummary');
  const runtimeSummary = $('#operationRuntimeLogSummary');
  const successList = $('#operationSuccessList');
  const failureList = $('#operationFailureList');
  const errorList = $('#operationErrorList');
  if (!summary || !runtimeSummary || !successList || !failureList || !errorList) return;
  const log = state.operationLog;
  if (!log) {
    summary.textContent = '历史读取中';
    runtimeSummary.textContent = '日志读取中';
    successList.innerHTML = '<div class="move-empty small">成功记录读取中</div>';
    failureList.innerHTML = '<div class="move-empty small">失败记录读取中</div>';
    errorList.innerHTML = '<div class="move-empty small">运行日志读取中</div>';
    return;
  }
  if (log.error) {
    summary.textContent = '读取历史失败';
    runtimeSummary.textContent = '读取日志失败';
    successList.innerHTML = `<div class="operation-error">${escHtml(log.error)}</div>`;
    failureList.innerHTML = '<div class="move-empty small">暂无失败记录</div>';
    errorList.innerHTML = '<div class="move-empty small">暂无运行错误</div>';
    return;
  }
  const history = log.history || [];
  const errors = log.errors || [];
  const failedHistory = history.filter(operation => ['failed', 'error'].includes(String(operation.status || '').toLowerCase()));
  const successfulHistory = history.filter(operation => !['failed', 'error'].includes(String(operation.status || '').toLowerCase()));
  summary.textContent = joinUiMeta([`成功 ${successfulHistory.length} 条`, `失败 ${failedHistory.length} 条`]);
  runtimeSummary.textContent = `${errors.length} 条最近运行错误`;
  successList.innerHTML = renderOperationEntries(successfulHistory, '暂无成功记录');
  failureList.innerHTML = renderOperationEntries(failedHistory, '暂无失败记录');
  errorList.innerHTML = errors.length ? errors.map(row => `
    <div class="operation-error">
      <b>${escHtml(row.source || 'log')}</b>
      <code>${escHtml(row.line || '')}</code>
    </div>
  `).join('') : '<div class="move-empty small">最近没有运行错误</div>';
  syncOperationFilterCounts(history.length, failedHistory.length, successfulHistory.length);
}
