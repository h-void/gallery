// Maintenance records (records): recycle bin, operation history, and the
// runtime error log panel.

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy, isCurrentRequestSeq, nextRequestSeq } from '../../store.js';
import { $, escHtml, joinUiMeta, formatHealthTime, formatBytes, buttonIcon, isAbortError } from '../../utils.js';
import { toast } from '../../logging.js';
import { t } from '../../i18n.js';
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

function recycleSystemTrash(entry) {
  return entry?.system_trash === true;
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

export async function loadRecycleSettings() {
  try {
    const res = await API.get('/api/recycle/settings');
    if (res && res.ok) {
      state.recycleStrategy = res.target || 'system';
      state.recycleSystemSupported = Boolean(res.system_supported);
    }
  } catch (_) {}
  renderRecycleStrategy();
}

export async function setRecycleStrategy(strategy) {
  if (strategy !== 'system' && strategy !== 'app') return;
  state.recycleStrategy = strategy;
  renderRecycleStrategy();
  try {
    await API.putJson('/api/recycle/settings', { target: strategy });
    toast(strategy === 'system' ? '已切换为飞牛系统回收站' : '已切换为画廊应用回收站', 'success');
  } catch (err) {
    toast(t('设置保存失败：{error}', {error: err.message || err}), 'error');
  }
}

export function getSavedRecycleViewMode() {
  try {
    const saved = localStorage.getItem('gallery_recycle_view_mode');
    return (saved === 'grid' || saved === 'table') ? saved : 'grid';
  } catch (_) {
    return 'grid';
  }
}

function localizeRecycleError(err) {
  const msg = err?.message || String(err || '');
  if (msg.includes('item is in fnOS system trash; please restore from system trash on desktop')) {
    return t('已移入飞牛系统回收站，请在系统桌面「回收站」中恢复此文件');
  }
  if (msg.includes('item is in fnOS system trash; please empty from system trash on desktop')) {
    return t('文件仍在飞牛系统回收站中，请在系统桌面「回收站」中清空此文件');
  }
  return msg;
}

export function renderRecycleStrategy() {
  const strategy = state.recycleStrategy || 'system';
  const subtitle = $('#recycleStrategySubtitle');
  const systemBtn = $('#recycleStrategySystemBtn');
  const appBtn = $('#recycleStrategyAppBtn');
  const cardTitle = $('#recycleStrategyCardTitle');
  const cardBody = $('#recycleStrategyCardBody');
  // The strategy toggle is a segment group: class carries the visual state and
  // aria-pressed carries it to assistive tech, like every other segment in the app.
  if (systemBtn) {
    const on = strategy === 'system';
    systemBtn.classList.toggle('active', on);
    systemBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
  if (appBtn) {
    const on = strategy === 'app';
    appBtn.classList.toggle('active', on);
    appBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
  if (subtitle) {
    subtitle.textContent = strategy === 'system'
      ? t('当前策略：飞牛系统回收站（移入系统桌面回收站，由飞牛统一管理）')
      : t('当前策略：画廊应用回收站（移入应用私有目录，支持缩略图与就地还原）');
  }
  if (cardTitle) {
    cardTitle.innerHTML = strategy === 'system'
      ? `<span>${t('当前回收策略：')}</span><strong>${t('飞牛系统回收站')}</strong>`
      : `<span>${t('当前回收策略：')}</span><strong>${t('画廊应用回收站')}</strong>`;
  }
  if (cardBody) {
    if (strategy === 'system') {
      cardBody.innerHTML = `
        <div><span class="recycle-strategy-tag-pro">${t('优点：')}</span><span>${t('与飞牛桌面及手机 App 的系统「回收站」完全打通，支持系统自动清理；受 root 隔离保护，防止外部误改。')}</span></div>
        <div><span class="recycle-strategy-tag-con">${t('缺点：')}</span><span>${t('受权限保护画廊内无法直接预览缩略图；如需恢复文件，请在飞牛系统桌面回收站中操作还原。')}</span></div>
      `;
    } else {
      cardBody.innerHTML = `
        <div><span class="recycle-strategy-tag-pro">${t('优点：')}</span><span>${t('画廊维护页内完整支持缩略图网格预览；支持在画廊内一键还原并保留画师与标签元数据；支持批量还原。')}</span></div>
        <div><span class="recycle-strategy-tag-con">${t('缺点：')}</span><span>${t('飞牛系统桌面的回收站中不可见；需在画廊维护页面内手动管理清空；占用画廊应用数据空间。')}</span></div>
      `;
    }
  }
}

export async function loadRecycleBin(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  const append = Boolean(options.append);
  if (!append) {
    loadRecycleSettings().catch(() => {});
    if (!state.recycleViewMode) {
      state.recycleViewMode = getSavedRecycleViewMode();
    }
  }
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
      // Drop selections for entries no longer in the list: a stale id makes
      // "select all" under-count and sends batch requests for rows that are gone.
      const validIds = new Set(entries.map(entry => Number(entry.id)));
      state.recycleSelectedIds = new Set(
        [...state.recycleSelectedIds].filter(id => validIds.has(Number(id)))
      );
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

export function setRecycleViewMode(mode) {
  if (mode !== 'grid' && mode !== 'table') return;
  state.recycleViewMode = mode;
  try { localStorage.setItem('gallery_recycle_view_mode', mode); } catch (_) {}
  renderRecycleBin();
}

// Both batch buttons act on the same rows, so a batch running in either
// direction must block the other entry point and the per-row buttons.
function recycleBatchBusy() {
  return isActionBusy('recycle-batch-restore') || isActionBusy('recycle-batch-purge');
}

// Selection is view state only: update checkboxes and selected classes in place
// instead of re-rendering the list. Re-rendering an 80-thumbnail grid on every
// click rebuilds the DOM, re-decodes images, and jumps the scroll position.
function syncRecycleSelectionUi() {
  const entries = recycleEntries();
  const selectedCount = state.recycleSelectedIds.size;
  const batchBar = $('#recycleBatchBar');
  const selectAllCheckbox = $('#recycleSelectAll');
  const selectedCountEl = $('#recycleSelectedCount');
  if (batchBar) {
    batchBar.hidden = selectedCount === 0;
    if (selectedCountEl) selectedCountEl.textContent = t('已选 {count} 项', {count: selectedCount});
    if (selectAllCheckbox) {
      selectAllCheckbox.checked = entries.length > 0 && selectedCount >= entries.length;
      selectAllCheckbox.indeterminate = selectedCount > 0 && selectedCount < entries.length;
    }
  }
  const list = $('#recycleBinList');
  if (!list) return;
  list.querySelectorAll('[data-recycle-id]').forEach(row => {
    row.classList.toggle('selected', state.recycleSelectedIds.has(Number(row.dataset.recycleId)));
  });
  list.querySelectorAll('[data-recycle-check]').forEach(box => {
    box.checked = state.recycleSelectedIds.has(Number(box.dataset.recycleCheck));
  });
  const tableSelectAll = list.querySelector('[data-recycle-table-select-all]');
  if (tableSelectAll) {
    tableSelectAll.checked = entries.length > 0 && selectedCount >= entries.length;
  }
}

export function toggleRecycleSelection(id, options = {}) {
  const numId = Number(id);
  if (!Number.isFinite(numId)) return;
  const entries = recycleEntries();
  if (options.shift && state.lastRecycleSelectedId != null) {
    const prevIndex = entries.findIndex(e => Number(e.id) === state.lastRecycleSelectedId);
    const currIndex = entries.findIndex(e => Number(e.id) === numId);
    if (prevIndex !== -1 && currIndex !== -1) {
      const start = Math.min(prevIndex, currIndex);
      const end = Math.max(prevIndex, currIndex);
      for (let i = start; i <= end; i++) {
        state.recycleSelectedIds.add(Number(entries[i].id));
      }
      state.lastRecycleSelectedId = numId;
      syncRecycleSelectionUi();
      return;
    }
  }
  if (state.recycleSelectedIds.has(numId)) {
    state.recycleSelectedIds.delete(numId);
  } else {
    state.recycleSelectedIds.add(numId);
  }
  state.lastRecycleSelectedId = numId;
  syncRecycleSelectionUi();
}

export function selectAllRecycleEntries(checked) {
  const entries = recycleEntries();
  if (checked) {
    entries.forEach(e => state.recycleSelectedIds.add(Number(e.id)));
  } else {
    state.recycleSelectedIds.clear();
  }
  syncRecycleSelectionUi();
}

export function clearRecycleSelection() {
  state.recycleSelectedIds.clear();
  syncRecycleSelectionUi();
}

// The list holds one page (80) but a selection can span pages the user loaded
// earlier — or ids that survived a re-read. Batch actions must therefore act on
// every selected entry, not only the rows currently on screen: resolving from
// state.recycleBin silently skipped the rest. Pages beyond the first are fetched
// on demand (the endpoint caps limit at 100) and only when the selection is not
// already covered by what is loaded.
async function resolveRecycleTargets(selectedIds) {
  const wanted = new Set(selectedIds.map(id => Number(id)));
  const found = new Map();
  for (const entry of recycleEntries()) {
    const id = Number(entry.id);
    if (wanted.has(id)) found.set(id, entry);
  }
  if (found.size >= wanted.size) return [...found.values()];
  let offset = 0;
  for (let page = 0; page < 200; page += 1) {
    const payload = await API.get(`/api/recycle?status=recycled&limit=100&offset=${encodeURIComponent(offset)}`);
    const entries = recycleEntries(payload);
    for (const entry of entries) {
      const id = Number(entry.id);
      if (wanted.has(id)) found.set(id, entry);
    }
    if (found.size >= wanted.size) break;
    const next = Number(payload?.next_offset);
    if (!entries.length || !Number.isFinite(next) || next <= offset) break;
    offset = next;
  }
  // Preserve the caller's selection order; a selected id the server no longer
  // returns stays unresolved and is reported through the missing count.
  return [...found.values()];
}

export async function batchRestoreRecycleEntries() {
  if (recycleBatchBusy()) return;
  const selected = [...state.recycleSelectedIds];
  if (!selected.length) return;
  let resolved;
  try {
    resolved = await resolveRecycleTargets(selected);
  } catch (e) {
    toast(t('回收站读取失败：{error}', {error: e.message || String(e)}), 'error');
    return;
  }
  const missing = selected.length - resolved.length;
  const targets = resolved.filter(e => recycleFileExists(e) && !recycleOriginalExists(e) && !recycleSystemTrash(e) && e.status === 'recycled');
  if (!targets.length) {
    toast(t('选中的项目中没有可恢复的文件（可能文件缺失或原路径被占用）'), 'warn');
    return;
  }
  if (!window.confirm(t('确定要将选中的 {count} 项作品恢复至原始路径吗？', {count: targets.length}))) return;
  setActionBusy('recycle-batch-restore', '', true);
  renderRecycleBin();
  const restoredIds = [];
  let failed = 0;
  for (const item of targets) {
    try {
      await API.post(`/api/recycle/${item.id}/restore`);
      restoredIds.push(Number(item.id));
    } catch (_) {
      failed++;
    }
  }
  setActionBusy('recycle-batch-restore', '', false);
  // Keep the rows that did not make it selected, so a refused entry is not
  // silently dropped from the batch the user is still looking at.
  for (const id of restoredIds) state.recycleSelectedIds.delete(id);
  syncRecycleSelectionUi();
  const skipped = missing + (resolved.length - targets.length);
  const summary = failed > 0
    ? t('恢复完成：成功 {restored} 项，失败 {failed} 项', {restored: restoredIds.length, failed})
    : t('已成功恢复 {count} 项作品', {count: restoredIds.length});
  toast(summary, failed ? 'warn' : 'success');
  if (skipped > 0) {
    toast(t('选中的项目中没有可恢复的文件（可能文件缺失或原路径被占用）'), 'warn');
  }
  // Always re-read the bin: the rows that were restored must leave the list, and
  // refreshCurrentView does not touch state.recycleBin in browse mode, so the
  // old success path left every restored entry on screen.
  try {
    await refreshCurrentView({reason: 'recycle_restore'});
  } catch (_) {}
  await Promise.all([
    loadRecycleBin({render: false}),
    loadOperationLog({render: false}),
  ]);
  renderRecycleBin();
  renderOperationLog();
}

export async function batchPurgeRecycleEntries() {
  if (recycleBatchBusy()) return;
  const selected = [...state.recycleSelectedIds];
  if (!selected.length) return;
  if (!window.confirm(t('确定要处理选中的 {count} 项记录吗？普通回收站文件将被彻底删除且无法恢复；飞牛系统回收站中的文件将仅从画廊列表移除。', {count: selected.length}))) return;
  let resolved;
  try {
    resolved = await resolveRecycleTargets(selected);
  } catch (e) {
    toast(t('回收站读取失败：{error}', {error: e.message || String(e)}), 'error');
    return;
  }
  setActionBusy('recycle-batch-purge', '', true);
  renderRecycleBin();
  const byId = new Map(resolved.map(entry => [Number(entry.id), entry]));
  let purged = 0;
  let dropped = 0;
  let failed = 0;
  for (const id of selected) {
    // A file still in the fnOS system trash is managed by the OS daemon and
    // cannot be removed here; drop its record instead so the batch is not
    // entirely failures. An entry the server no longer returns is a plain
    // record removal, which is what the delete already refuses to do for one
    // that still owns bytes.
    const recordOnly = recycleSystemTrash(byId.get(Number(id)));
    try {
      await API.del(`/api/recycle/${id}${recordOnly ? '?record_only=1' : ''}`);
      if (recordOnly) dropped++;
      else purged++;
    } catch (_) {
      failed++;
    }
  }
  setActionBusy('recycle-batch-purge', '', false);
  state.recycleSelectedIds.clear();
  const parts = [];
  if (purged) parts.push(t('彻底删除 {count} 项', {count: purged}));
  if (dropped) parts.push(t('移除记录 {count} 项', {count: dropped}));
  if (failed) parts.push(t('失败 {count} 项', {count: failed}));
  toast(parts.join('，'), failed ? 'warn' : 'success');
  await loadRecycleBin();
}

function recycleRestoreButtonHtml(id, canRestore, restoring, restoreTitle) {
  return `<button class="btn btn-ops" type="button" data-recycle-restore="${id}" ${canRestore ? '' : 'disabled'} title="${escHtml(restoreTitle)}" ${restoring ? 'aria-busy="true"' : ''}>${restoring ? t('恢复中') : t('恢复')}</button>`;
}

// A file in the fnOS system trash cannot be deleted or restored from here, so
// its only exit is dropping the record. Everything else keeps the permanent
// delete button. Both are disabled while a batch owns the rows.
function recyclePurgeButtonHtml(entry, purging, batchBusy) {
  const id = Number(entry.id);
  const disabledAttr = (batchBusy || purging) ? 'disabled aria-busy="true"' : '';
  if (recycleSystemTrash(entry)) {
    return `<button class="btn btn-ghost btn-danger" type="button" data-recycle-record-only="${id}" ${disabledAttr} title="${escHtml(t('文件在飞牛系统回收站中，此操作仅移除画廊记录，不会删除文件'))}">${purging ? t('移除中') : t('移除记录')}</button>`;
  }
  return `<button class="btn btn-ghost btn-danger" type="button" data-recycle-purge="${id}" ${disabledAttr} title="${escHtml(t('从回收站彻底删除'))}">${purging ? t('删除中') : t('彻底删除')}</button>`;
}

export function renderRecycleBin() {
  const summary = $('#recycleBinSummary');
  const list = $('#recycleBinList');
  const more = $('#recycleBinMore');
  const batchBar = $('#recycleBatchBar');
  const gridBtn = $('#recycleViewGridBtn');
  const tableBtn = $('#recycleViewTableBtn');
  const selectAllCheckbox = $('#recycleSelectAll');
  const selectedCountEl = $('#recycleSelectedCount');
  if (!summary || !list || !more) return;

  renderRecycleStrategy();

  const mode = state.recycleViewMode || getSavedRecycleViewMode();
  state.recycleViewMode = mode;
  // Segment state: class for the skin, aria-pressed for assistive tech.
  if (gridBtn) {
    const on = mode === 'grid';
    gridBtn.classList.toggle('active', on);
    gridBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
  if (tableBtn) {
    const on = mode === 'table';
    tableBtn.classList.toggle('active', on);
    tableBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }

  const recycleBin = state.recycleBin;
  if (!recycleBin) {
    summary.textContent = t('回收站读取中');
    list.innerHTML = `<div class="move-empty small">${t('回收站读取中')}</div>`;
    more.innerHTML = '';
    if (batchBar) batchBar.hidden = true;
    return;
  }
  if (recycleBin.error) {
    summary.textContent = t('回收站读取失败');
    list.innerHTML = `<div class="operation-error">${escHtml(recycleBin.error)}</div>`;
    more.innerHTML = '';
    if (batchBar) batchBar.hidden = true;
    return;
  }

  const entries = recycleEntries(recycleBin);
  const batchBusy = recycleBatchBusy();
  const total = Number(recycleBin.total ?? entries.length);
  const unavailable = entries.filter(entry => !recycleFileExists(entry)).length;
  const occupied = entries.filter(recycleOriginalExists).length;
  summary.textContent = joinUiMeta([
    `${t('已显示 {entries}/{total} 项', {entries: entries.length, total: total})}`,
    unavailable ? `${t('{unavailable} 个文件已缺失', {unavailable: unavailable})}` : '',
    occupied ? `${t('{occupied} 个原路径已占用', {occupied: occupied})}` : '',
  ]);

  const selectedCount = state.recycleSelectedIds.size;
  if (batchBar) {
    batchBar.hidden = selectedCount === 0;
    if (selectedCountEl) selectedCountEl.textContent = t('已选 {count} 项', {count: selectedCount});
    if (selectAllCheckbox) {
      selectAllCheckbox.checked = entries.length > 0 && selectedCount >= entries.length;
      selectAllCheckbox.indeterminate = selectedCount > 0 && selectedCount < entries.length;
    }
  }

  if (!entries.length) {
    list.innerHTML = `<div class="move-empty small empty-state">${t('回收站中暂无可恢复的文件')}</div>`;
    list.classList.remove('recycle-mode-grid', 'recycle-mode-table');
  } else if (mode === 'table') {
    list.classList.remove('recycle-mode-grid');
    list.classList.add('recycle-mode-table');
    const allChecked = entries.length > 0 && selectedCount >= entries.length;
    list.innerHTML = `
      <div class="recycle-table-wrap">
        <table class="recycle-table">
          <thead>
            <tr>
              <th class="recycle-th-check"><input type="checkbox" data-recycle-table-select-all ${allChecked ? 'checked' : ''} aria-label="${t('全选')}"></th>
              <th class="recycle-th-name">${t('名称')}</th>
              <th class="recycle-th-path">${t('原路径')}</th>
              <th class="recycle-th-size">${t('大小')}</th>
              <th class="recycle-th-date">${t('删除时间')}</th>
              <th class="recycle-th-status">${t('状态')}</th>
              <th class="recycle-th-actions">${t('操作')}</th>
            </tr>
          </thead>
          <tbody>
            ${entries.map(entry => {
              const id = Number(entry.id);
              const originalPath = String(entry.original_path || entry.restore_path || '');
              const fileExists = recycleFileExists(entry);
              const originalExists = recycleOriginalExists(entry);
              const systemTrash = recycleSystemTrash(entry);
              const status = String(entry.status || 'recycled');
              const displayStatus = !fileExists ? 'missing' : (originalExists ? 'error' : status);
              const displayStatusLabel = !fileExists ? t('文件缺失') : (originalExists ? t('路径占用') : t(recycleStatusLabel(status)));
              const restoring = isActionBusy('recycle-restore', String(id));
              const purging = isActionBusy('recycle-purge', String(id));
              const canRestore = Number.isFinite(id) && status === 'recycled' && fileExists && !originalExists && !systemTrash && !batchBusy && !restoring && !purging;
              const restoreTitle = systemTrash
                ? t('文件在飞牛系统回收站中，请在系统桌面回收站恢复')
                : (originalExists ? t('原路径已被同名文件占用，禁止恢复以防误覆盖现有文件') : (!fileExists ? t('回收站文件已缺失') : t('恢复到原始路径')));
              const isSelected = state.recycleSelectedIds.has(id);
              const isImage = entry.media_type === 'image' || (!entry.media_type && /\.(jpe?g|png|webp|gif|bmp|avif)$/i.test(originalPath));
              const fileName = String(entry.file_name || originalPath.split('/').pop() || '-');
              return `
                <tr class="recycle-bin-row recycle-table-row${canRestore ? '' : ' blocked'}${isSelected ? ' selected' : ''}" data-recycle-id="${id}">
                  <td class="recycle-td-check">
                    <input type="checkbox" class="recycle-check" data-recycle-check="${id}" ${isSelected ? 'checked' : ''} aria-label="${t('选择此项')}">
                  </td>
                  <td class="recycle-td-name">
                    <div class="recycle-table-name">
                      <span class="recycle-table-icon" aria-hidden="true">${buttonIcon(isImage ? 'image' : 'file')}</span>
                      <span class="recycle-bin-row-head"><code data-i18n-skip title="${escHtml(originalPath)}">${escHtml(fileName)}</code></span>
                    </div>
                  </td>
                  <td class="recycle-td-path">
                    <code class="recycle-bin-location" data-i18n-skip title="${escHtml(originalPath)}">${escHtml(originalPath)}</code>
                  </td>
                  <td class="recycle-td-size">${entry.file_size ? escHtml(formatBytes(entry.file_size)) : '-'}</td>
                  <td class="recycle-td-date">${entry.created_at ? escHtml(formatHealthTime(entry.created_at)) : '-'}</td>
                  <td class="recycle-td-status">
                    <span class="recycle-bin-status ${recycleStatusClass(displayStatus)}">${escHtml(displayStatusLabel)}</span>
                  </td>
                  <td class="recycle-td-actions">
                    <div class="recycle-bin-actions">
                      <div class="recycle-bin-btn-group">
                        ${recycleRestoreButtonHtml(id, canRestore, restoring, restoreTitle)}
                        ${recyclePurgeButtonHtml(entry, purging, batchBusy)}
                      </div>
                    </div>
                  </td>
                </tr>`;
            }).join('')}
          </tbody>
        </table>
      </div>`;
  } else {
    list.classList.remove('recycle-mode-table');
    list.classList.add('recycle-mode-grid');
    list.innerHTML = `
      <div class="recycle-grid">
        ${entries.map(entry => {
          const id = Number(entry.id);
          const originalPath = String(entry.original_path || entry.restore_path || '');
          const recycledPath = String(entry.recycled_path || '');
          const fileExists = recycleFileExists(entry);
          const originalExists = recycleOriginalExists(entry);
          const systemTrash = recycleSystemTrash(entry);
          const status = String(entry.status || 'recycled');
          const displayStatus = !fileExists ? 'missing' : (originalExists ? 'error' : status);
          const displayStatusLabel = !fileExists ? t('文件缺失') : (originalExists ? t('路径占用') : t(recycleStatusLabel(status)));
          const restoring = isActionBusy('recycle-restore', String(id));
          const purging = isActionBusy('recycle-purge', String(id));
          const canRestore = Number.isFinite(id) && status === 'recycled' && fileExists && !originalExists && !systemTrash && !batchBusy && !restoring && !purging;
          const fileState = !fileExists ? t('文件已缺失') : (originalExists ? t('原路径已被占用（不可恢复，避免覆盖现有文件）') : t('可以恢复'));
          const restoreTitle = systemTrash
            ? t('文件在飞牛系统回收站中，请在系统桌面回收站恢复')
            : (originalExists ? t('原路径已被同名文件占用，禁止恢复以防误覆盖现有文件') : (!fileExists ? t('回收站文件已缺失') : t('恢复到原始路径')));
          const isSelected = state.recycleSelectedIds.has(id);
          const isImage = entry.media_type === 'image' || (!entry.media_type && /\.(jpe?g|png|webp|gif|bmp|avif)$/i.test(originalPath));
          const fileName = String(entry.file_name || originalPath.split('/').pop() || '-');
          const previewUrl = fileExists && isImage ? `/api/recycle/${id}/preview?max=240` : '';
          return `
            <div class="recycle-bin-row recycle-card${canRestore ? '' : ' blocked'}${isSelected ? ' selected' : ''}" data-recycle-id="${id}">
              <div class="recycle-check-wrap">
                <input type="checkbox" class="recycle-check" data-recycle-check="${id}" ${isSelected ? 'checked' : ''} aria-label="${t('选择此项')}">
              </div>
              <div class="recycle-card-thumb-wrap">
                ${previewUrl
                  ? `<img class="recycle-thumb" src="${escHtml(previewUrl)}" loading="lazy" decoding="async" alt="" onerror="this.style.display='none';this.nextElementSibling.style.display='flex'"><div class="recycle-thumb-placeholder" style="display:none">${buttonIcon('image')}</div>`
                  : `<div class="recycle-thumb-placeholder">${buttonIcon(isImage ? 'image' : 'file')}</div>`
                }
              </div>
              <div class="recycle-card-info">
                <div class="recycle-bin-row-head">
                  <code data-i18n-skip title="${escHtml(originalPath)}">${escHtml(fileName)}</code>
                  <span class="recycle-bin-status ${recycleStatusClass(displayStatus)}">${escHtml(displayStatusLabel)}</span>
                </div>
                <div class="recycle-bin-meta">
                  <span>${entry.file_size ? escHtml(formatBytes(entry.file_size)) : escHtml(fileState)}</span>
                  ${entry.created_at ? `<span>${escHtml(formatHealthTime(entry.created_at))}</span>` : ''}
                </div>
                ${recycledPath ? `<code class="recycle-bin-location" title="${escHtml(recycledPath)}">${escHtml(recycledPath)}</code>` : ''}
                <div class="recycle-bin-actions">
                  <span title="${escHtml(fileState)}">${escHtml(fileState)}</span>
                  <div class="recycle-bin-btn-group">
                    ${recycleRestoreButtonHtml(id, canRestore, restoring, restoreTitle)}
                    ${recyclePurgeButtonHtml(entry, purging, batchBusy)}
                  </div>
                </div>
              </div>
            </div>`;
        }).join('')}
      </div>`;
  }

  const clearBtn = $('#recycleClearBtn');
  if (clearBtn) {
    clearBtn.disabled = entries.length === 0;
  }
  const hasNextPage = entries.length > 0 && typeof recycleBin.next_offset === 'number';
  more.innerHTML = hasNextPage
    ? `<button class="btn btn-ghost" type="button" data-recycle-load-more>${t('加载更多')}</button>`
    : '';
}

export async function restoreRecycleEntry(entryId) {
  const id = Number(entryId);
  const entry = recycleEntries().find(item => Number(item.id) === id);
  if (!entry || !Number.isFinite(id) || isActionBusy('recycle-restore', String(id))) return;
  const originalPath = String(entry.original_path || entry.restore_path || '');
  if (!window.confirm(`${t('确定要将文件恢复至原始路径吗？ {originalPath}', {originalPath: originalPath})}`)) return;
  setActionBusy('recycle-restore', String(id), true);
  renderRecycleBin();
  try {
    const result = await API.post(`/api/recycle/${id}/restore`);
    state.recycleSelectedIds.delete(id);
    syncRecycleSelectionUi();
    try {
      await refreshCurrentView({reason: 'recycle_restore'});
    } catch (_) {}
    await Promise.all([
      loadRecycleBin({render: false}),
      loadOperationLog({render: false}),
    ]);
    renderRecycleBin();
    renderOperationLog();
    toast(t('文件已恢复到原始路径'), 'success');
  } catch (e) {
    if (e.status === 409 || e.status === 404) await loadRecycleBin();
    toast(t('恢复失败：{error}', {error: localizeRecycleError(e)}), 'error');
  } finally {
    setActionBusy('recycle-restore', String(id), false);
    renderRecycleBin();
  }
}

export async function purgeRecycleEntry(entryId, options = {}) {
  const id = Number(entryId);
  const entry = recycleEntries().find(item => Number(item.id) === id);
  if (!Number.isFinite(id) || isActionBusy('recycle-purge', String(id))) return;
  const recordOnly = options.recordOnly === true || recycleSystemTrash(entry);
  const fileName = entry?.file_name || (entry?.original_path ? entry.original_path.split('/').pop() : '该文件');
  const confirmText = recordOnly
    ? t('确定要从画廊列表移除该记录吗？ {fileName} 文件仍会保留在飞牛系统回收站中。', {fileName: fileName})
    : t('确定要彻底删除该文件吗？ {fileName} 删除后将无法恢复。', {fileName: fileName});
  if (!window.confirm(confirmText)) return;
  setActionBusy('recycle-purge', String(id), true);
  renderRecycleBin();
  try {
    const result = await API.del(`/api/recycle/${id}${recordOnly ? '?record_only=1' : ''}`);
    state.recycleSelectedIds.delete(id);
    syncRecycleSelectionUi();
    // A permanent-delete toast is only honest when the backend reports the file
    // gone; `file_removed: false` means the record went and no file was there to
    // remove, which is still a success but not "彻底删除".
    if (recordOnly) toast(t('已移除记录'), 'success');
    else if (result?.file_removed === true) toast(t('已彻底删除'), 'success');
    else toast(t('已移除记录，回收站中已无该文件'), 'success');
    await loadRecycleBin();
  } catch (e) {
    toast(t('删除失败：{error}', {error: localizeRecycleError(e)}), 'error');
  } finally {
    setActionBusy('recycle-purge', String(id), false);
    renderRecycleBin();
  }
}

export async function clearRecycleBin() {
  const recycleBin = state.recycleBin;
  const entries = recycleEntries(recycleBin);
  if (!entries.length) {
    toast(t('回收站为空'), 'info');
    return;
  }
  const systemCount = entries.filter(recycleSystemTrash).length;
  const confirmText = systemCount
    ? t('确定要清空回收站吗？ 共 {entries} 项记录：普通文件将被彻底删除且无法恢复；{system} 项在飞牛系统回收站中，需在系统桌面回收站中清空。', {entries: entries.length, system: systemCount})
    : t('确定要清空回收站吗？ 共 {entries} 项记录，所有文件将被彻底删除且无法恢复。', {entries: entries.length});
  if (!window.confirm(confirmText)) return;
  const clearBtn = $('#recycleClearBtn');
  if (clearBtn) clearBtn.disabled = true;
  try {
    const result = await API.post('/api/recycle/clear', undefined, {timeoutMs: 300000});
    // Report what the backend actually did: fnOS system-trash entries cannot be
    // removed from here, so they stay listed (and are counted separately), and a
    // failed removal keeps its record instead of vanishing from the UI.
    const cleared = Number(result?.cleared_count ?? 0);
    const kept = Number(result?.system_kept ?? 0);
    const failed = Number(result?.failed ?? 0);
    const parts = [t('已清空 {count} 项', {count: cleared})];
    if (kept) parts.push(t('{count} 项在飞牛系统回收站中，请到系统桌面回收站清空', {count: kept}));
    if (failed) parts.push(t('{count} 项清理失败，记录已保留', {count: failed}));
    toast(parts.join('，'), (kept || failed) ? 'warn' : 'success');
    await loadRecycleBin();
  } catch (e) {
    toast(t('清空失败：{error}', {error: localizeRecycleError(e)}), 'error');
  } finally {
    if (clearBtn) clearBtn.disabled = false;
  }
}

function operationLogKindLabel(operation) {
  if (operation.reason === 'tagged_file') return '已加角色文件归位';
  if (operation.kind === 'folder_rename_undo' || operation.reason === 'folder_rename_undo' || operation.reason === 'undo') return '撤销文件夹整理';
  if (operation.kind === 'folder_rename') return '文件夹整理';
  if (operation.kind === 'move') return '路径变更';
  return '操作';
}

function operationLogReasonLabel(reason) {
  const labels = {
    tagged_file: '已加角色文件归位',
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
  return record.reason
    ? t('未删除空文件夹：{reason}', {reason: reasons[record.reason] || record.reason})
    : t('未删除空文件夹');
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
          <span${operation.artist_name ? ' data-i18n-skip' : ''}>${escHtml(operation.artist_name || '未知画师')}</span>
          <span>${t('{operation} 项', {operation: operation.updated_items || 0})}</span>
          <span>${escHtml(operationLogReasonLabel(operation.reason))}</span>
        </div>
        ${operation.message ? `<div class="operation-entry-message">${escHtml(operation.message)}</div>` : ''}
        <div class="operation-path"><span>原</span><code data-i18n-skip title="${escHtml(operation.source || source)}">${escHtml(source || '-')}</code></div>
        <div class="operation-path"><span>新</span><code data-i18n-skip title="${escHtml(operation.target || target)}">${escHtml(target || '-')}</code></div>
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
  if (btnAll) btnAll.textContent = `${t('全部（{total}）', {total: total})}`;
  if (btnFailed) btnFailed.textContent = `${t('仅失败（{failed}）', {failed: failed})}`;
  if (btnSuccess) btnSuccess.textContent = `${t('仅成功（{success}）', {success: success})}`;
}

export const OPERATION_HISTORY_FOLD_THRESHOLD = 5;

export function toggleOperationHistoryFold() {
  state.operationHistoryExpanded = !state.operationHistoryExpanded;
  renderOperationLog();
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
    summary.textContent = t('历史读取中');
    runtimeSummary.textContent = t('日志读取中');
    successList.innerHTML = `<div class="move-empty small">${t('成功记录读取中')}</div>`;
    failureList.innerHTML = `<div class="move-empty small">${t('失败记录读取中')}</div>`;
    errorList.innerHTML = `<div class="move-empty small">${t('运行日志读取中')}</div>`;
    return;
  }
  if (log.error) {
    summary.textContent = t('读取历史失败');
    runtimeSummary.textContent = t('读取日志失败');
    successList.innerHTML = `<div class="operation-error">${escHtml(log.error)}</div>`;
    failureList.innerHTML = `<div class="move-empty small">${t('暂无失败记录')}</div>`;
    errorList.innerHTML = `<div class="move-empty small">${t('暂无运行错误')}</div>`;
    return;
  }
  const history = log.history || [];
  const errors = log.errors || [];
  const failedHistory = history.filter(operation => ['failed', 'error'].includes(String(operation.status || '').toLowerCase()));
  const successfulHistory = history.filter(operation => !['failed', 'error'].includes(String(operation.status || '').toLowerCase()));
  summary.textContent = joinUiMeta([`${t('成功 {successfulHistory} 条', {successfulHistory: successfulHistory.length})}`, `${t('失败 {failedHistory} 条', {failedHistory: failedHistory.length})}`]);
  runtimeSummary.textContent = `${t('{errors} 条最近运行错误', {errors: errors.length})}`;
  const shouldFold = successfulHistory.length > OPERATION_HISTORY_FOLD_THRESHOLD;
  const isExpanded = Boolean(state.operationHistoryExpanded);
  const successfulHistoryVisible = (shouldFold && !isExpanded)
    ? successfulHistory.slice(0, OPERATION_HISTORY_FOLD_THRESHOLD)
    : successfulHistory;

  const foldWrap = $('#operationSuccessFoldWrap');
  const foldBtn = $('#operationSuccessFoldBtn');
  if (foldWrap && foldBtn) {
    if (shouldFold) {
      foldWrap.hidden = false;
      const hiddenCount = successfulHistory.length - OPERATION_HISTORY_FOLD_THRESHOLD;
      foldBtn.textContent = isExpanded ? t('收起') : `${t('展开剩余 {hiddenCount} 条记录', {hiddenCount: hiddenCount})}`;
    } else {
      foldWrap.hidden = true;
    }
  }

  successList.innerHTML = renderOperationEntries(successfulHistoryVisible, t('暂无成功记录'));
  failureList.innerHTML = renderOperationEntries(failedHistory, t('暂无失败记录'));
  errorList.innerHTML = errors.length ? errors.map(row => `
    <div class="operation-error">
      <b>${escHtml(row.source || 'log')}</b>
      <code>${escHtml(row.line || '')}</code>
    </div>
  `).join('') : `<div class="move-empty small">${t('最近没有运行错误')}</div>`;
  syncOperationFilterCounts(history.length, failedHistory.length, successfulHistory.length);
}
