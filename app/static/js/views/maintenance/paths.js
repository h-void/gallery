// Maintenance paths (paths): move-candidate cards, cross-artist groups,
// history, and the auto-resolve action.

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy } from '../../store.js';
import { $, $$, escHtml } from '../../utils.js';
import { toast } from '../../logging.js';
import { refreshActiveMaintenanceView } from './index.js';
import { renderHashStatus } from './overview.js';
import { loadArtists } from '../../router.js';

const MOVE_HISTORY_PAGE_SIZE = 80;

export function renderMovePathSummary() {
  const pending = state.movePendingTotal ?? state.moveCandidates.length;
  $('#movePendingCount').textContent = pending;
  const pendingCard = $('#movePendingCount')?.closest('.metric-card');
  if (pendingCard) pendingCard.classList.toggle('metric-alert', Number(pending) > 0);
  $('#moveWaitingHashCount').textContent = Number(state.moveWaitingHashCount || 0)
    + Number(state.hashStatus?.scan_candidates?.remaining || 0);
  const historyTotal = Number(state.moveHistoryTotal ?? state.moveHistory.length);
  $('#movePreviewCount').textContent = historyTotal;
}

export function renderMoveCandidateGroups() {
  const list = $('#moveCandidateGroupList');
  if (!list) return;
  const canApplyGroup = group => {
    const applicable = Number(group.applicable_candidate_count ?? group.candidate_count ?? 0);
    return group.can_apply && applicable > 0;
  };
  const groups = (state.moveCandidateGroups || []).filter(canApplyGroup);
  if (groups.length === 0) {
    list.innerHTML = '';
    return;
  }

  list.innerHTML = groups.map(group => {
    const oldArtistPath = group.display_item_artist_path || group.item_artist_path || '';
    const newArtistPath = group.display_candidate_artist_path || group.candidate_artist_path || '';
    const oldArtistId = group.item_artist_id;
    const newArtistId = group.candidate_artist_id;
    const applicableCount = Number(group.applicable_candidate_count ?? group.candidate_count ?? 0);
    const blockedCount = Number(group.blocked_candidate_count || 0);
    const blockedNote = blockedCount > 0
      ? `<div class="move-warning">${blockedCount} 项目标重复会保留为待处理</div>`
      : '';
    const samples = (group.sample_candidates || []).slice(0, 4).map(sample => {
      const oldPath = sample.display_old_path || sample.old_path || '';
      const newPath = sample.display_new_path || sample.new_path || '';
      return `
        <div class="move-group-sample">
          <code title="${escHtml(sample.old_path || oldPath)}">${escHtml(oldPath)}</code>
          <b>&rarr;</b>
          <code title="${escHtml(sample.new_path || newPath)}">${escHtml(newPath)}</code>
        </div>`;
    }).join('');
    return `
      <div class="move-group-card" data-old-artist-id="${oldArtistId}" data-new-artist-id="${newArtistId}">
        <div class="move-group-main">
          <div class="move-card-top">
            <span class="move-reason">${escHtml(moveReasonLabel(group.reason))}</span>
            <span class="move-id">${applicableCount} 可批量 / ${group.candidate_count} 项</span>
          </div>
          <div class="move-artist-paths">
            <div><span>旧画师</span><code title="${escHtml(group.item_artist_path || oldArtistPath)}">${escHtml(oldArtistPath || '-')}</code></div>
            <div><span>新画师</span><code title="${escHtml(group.candidate_artist_path || newArtistPath)}">${escHtml(newArtistPath || '-')}</code></div>
          </div>
          ${blockedNote}
          <div class="move-group-samples">${samples}</div>
        </div>
        <div class="move-actions">
          <button class="btn btn-primary" type="button" data-move-group-action="merge" data-old-artist-id="${oldArtistId}" data-new-artist-id="${newArtistId}">批量确认 ${applicableCount} 项</button>
        </div>
      </div>`;
  }).join('');
  bindMoveGroupActions();
}

export function renderMoveCandidates() {
  const list = $('#moveCandidateList');
  const groupedCandidateIds = new Set();
  (state.moveCandidateGroups || []).forEach(group => {
    if (!group.can_apply) return;
    const blocked = new Set((group.blocked_move_ids || []).map(Number));
    (group.move_ids || []).map(Number).forEach(id => {
      if (!blocked.has(id)) groupedCandidateIds.add(id);
    });
  });
  const groupedCount = (state.moveCandidateGroups || [])
    .filter(group => group.can_apply && Number(group.applicable_candidate_count ?? group.candidate_count ?? 0) > 0)
    .reduce((total, group) => total + Number(group.applicable_candidate_count ?? group.candidate_count ?? 0), 0);
  const candidates = (state.moveCandidates || []).filter(candidate => !groupedCandidateIds.has(Number(candidate.id)));
  if (candidates.length === 0) {
    if (groupedCount) {
      list.innerHTML = '<div class="move-empty">已在上方按画师路径分组 ' + groupedCount + ' 项</div>';
      return;
    }
    list.innerHTML = '<div class="move-empty">没有待确认的路径</div>';
    return;
  }

  // 同一新路径挂了多少条待判断旧记录：>1 即多对一歧义，只能呈现冲突，
  // 不能替用户挑选对应关系（P3 修改方向 4）。
  const sameTargetCounts = new Map();
  candidates.forEach(row => {
    const key = row.new_path || '';
    sameTargetCounts.set(key, (sameTargetCounts.get(key) || 0) + 1);
  });
  list.innerHTML = candidates.map(c => {
    const label = moveReasonLabel(c.reason);
    const isManual = c.reason === 'manual_needed';
    const oldPath = c.display_old_path || c.old_path || '无旧路径';
    const newPath = c.display_new_path || c.new_path || '';
    const oldArtistPath = c.display_item_artist_path || c.item_artist_path || '';
    const newArtistPath = c.display_candidate_artist_path || c.candidate_artist_path || '';
    const isCrossArtist = Boolean(c.is_cross_artist);
    const multiOldRecords = (sameTargetCounts.get(c.new_path || '') || 1) > 1;
    const cannotConfirm = c.can_confirm === false || isCrossArtist;
    // 内容相同必须双方哈希一致才算数；只有单方或不一致都是待核对内容，
    // 原始哈希保留在 title 详情里。
    const hashLabel = c.hash_match === true ? '内容相同' : '待核对内容';
    const hashDetail = [
      c.content_hash ? `新路径 ${c.content_hash}` : '',
      c.item_hash ? `旧记录 ${c.item_hash}` : '',
    ].filter(Boolean).join(' / ') || '哈希未就绪';
    const warning = isCrossArtist
      ? '<div class="move-warning">内容相同，但画师不同，需要核对归属。确认同一文件会沿用旧记录和标签；确定不是同一文件时选择「保留为独立文件」。</div>'
      : (multiOldRecords
        ? '<div class="move-warning">内容相同，但有多个旧记录，尚不能确定对应关系。</div>'
        : (isManual ? '<div class="move-warning">请人工核对旧/新路径；能确定同一文件时再确认。</div>' : ''));
    const artistPaths = isCrossArtist ? `
        <div class="move-artist-paths">
          <div><span>旧画师</span><code title="${escHtml(c.item_artist_path || oldArtistPath)}">${escHtml(oldArtistPath || '-')}</code></div>
          <div><span>新画师</span><code title="${escHtml(c.candidate_artist_path || newArtistPath)}">${escHtml(newArtistPath || '-')}</code></div>
        </div>` : '';
    const confirmButton = cannotConfirm ? '' : `<button class="btn btn-primary" type="button" data-move-action="confirm" data-id="${c.id}">确认同一文件</button>`;
    return `<div class="move-card" data-id="${c.id}">
      <div class="move-card-main">
        <div class="move-card-top">
          <span class="move-reason">${escHtml(label)}</span>
          <span class="move-id">#${c.id}</span>
        </div>
        <div class="move-path old"><span>旧</span><code title="${escHtml(c.old_path || oldPath)}">${escHtml(oldPath)}</code></div>
        <div class="move-path new"><span>新</span><code title="${escHtml(c.new_path || newPath)}">${escHtml(newPath)}</code></div>
        ${warning}
        ${artistPaths}
        <div class="move-meta">
          <span>记录 ${c.item_id || '-'}</span>
          <span>检查码 ${shortHash(c.content_hash)}</span>
          <span title="${escHtml(hashDetail)}">${hashLabel}</span>
        </div>
      </div>
      <div class="move-actions">
        ${confirmButton}
        <button class="btn btn-ghost" type="button" data-move-action="new" data-id="${c.id}">保留为独立文件</button>
        <button class="btn btn-danger" type="button" data-move-action="ignore" data-id="${c.id}">忽略此匹配</button>
      </div>
    </div>`;
  }).join('');
  bindMoveActions();
}

export function renderMoveHistory() {
  const list = $('#moveHistoryList');
  const more = $('#moveHistoryMore');
  if (!list) return;
  const history = state.moveHistory || [];
  if (history.length === 0) {
    list.innerHTML = '<div class="move-empty small">暂无自动确认记录</div>';
    if (more) more.innerHTML = '';
    return;
  }
  list.innerHTML = history.map(h => {
    const oldPath = h.display_old_path || h.old_path || '';
    const newPath = h.display_new_path || h.new_path || '';
    return `
      <div class="move-history-row">
        <span>${escHtml(moveReasonLabel(h.reason))}</span>
        <code title="${escHtml(h.old_path || oldPath)}">${escHtml(oldPath)}</code>
        <b>&rarr;</b>
        <code title="${escHtml(h.new_path || newPath)}">${escHtml(newPath)}</code>
      </div>
    `;
  }).join('');
  const total = Number(state.moveHistoryTotal ?? history.length);
  const hasMore = history.length < total || state.moveHistoryHasMore === true;
  if (more) {
    more.innerHTML = hasMore
      ? `<button class="btn btn-ghost" type="button" data-move-history-load-more ${state.moveHistoryLoading ? 'aria-busy="true"' : ''}>${state.moveHistoryLoading ? '读取中' : '加载更多'}</button>`
      : '';
  }
  bindMoveHistoryMore();
}

export async function loadMoreMoveHistory(options = {}) {
  if (state.moveHistoryLoading) return null;
  state.moveHistoryLoading = true;
  renderMoveHistory();
  const limit = Number(state.moveHistoryLimit || MOVE_HISTORY_PAGE_SIZE);
  const offset = (state.moveHistory || []).length;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  try {
    const page = await API.get(
      `/api/move-history?status=applied&limit=${limit}&offset=${offset}`,
      fetchOptions
    );
    const nextRows = page.history || [];
    const merged = [...(state.moveHistory || []), ...nextRows];
    state.moveHistory = merged;
    state.moveHistoryTotal = Number(page.total ?? merged.length);
    state.moveHistoryHasMore = page.has_more === true;
    renderMoveHistory();
    return page;
  } catch (e) {
    toast('加载更多历史记录失败：' + (e.message || String(e)), 'error');
    renderMoveHistory();
    return null;
  } finally {
    state.moveHistoryLoading = false;
    renderMoveHistory();
  }
}

// One delegated listener per list; renderMove* replaces innerHTML freely.
export function bindMoveActions() {
  const list = $('#moveCandidateList');
  if (!list || list.dataset.moveActionsBound === '1') return;
  list.dataset.moveActionsBound = '1';
  list.addEventListener('click', e => {
    const btn = e.target instanceof Element ? e.target.closest('[data-move-action]') : null;
    if (btn && list.contains(btn)) runMoveAction(btn.dataset.id, btn.dataset.moveAction);
  });
}

export function bindMoveGroupActions() {
  const list = $('#moveCandidateGroupList');
  if (!list || list.dataset.moveGroupBound === '1') return;
  list.dataset.moveGroupBound = '1';
  list.addEventListener('click', e => {
    const btn = e.target instanceof Element ? e.target.closest('[data-move-group-action]') : null;
    if (btn && list.contains(btn)) applyMoveCandidateGroup(btn.dataset.oldArtistId, btn.dataset.newArtistId);
  });
}

export function bindMoveHistoryMore() {
  const more = $('#moveHistoryMore');
  if (!more || more.dataset.moveHistoryMoreBound === '1') return;
  more.dataset.moveHistoryMoreBound = '1';
  more.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    const btn = target ? target.closest('[data-move-history-load-more]') : null;
    if (!btn || !more.contains(btn)) return;
    if (state.moveHistoryLoading) return;
    loadMoreMoveHistory();
  });
}

export async function applyMoveCandidateGroup(oldArtistId, newArtistId) {
  if (!oldArtistId || !newArtistId) return;
  const busyKey = `${oldArtistId}:${newArtistId}`;
  if (isActionBusy('move-group-action', busyKey)) return;
  setActionBusy('move-group-action', busyKey, true);
  try {
    const result = await API.post(`/api/move-candidates/groups/${oldArtistId}/${newArtistId}/merge`);
    const resolved = Number(result.resolved_existing || 0);
    const applied = Number(result.applied || 0);
    const stale = Number(result.stale || 0);
    const skipped = Number(result.skipped || 0);
    toast(`批量确认 ${applied} 项，已在库中 ${resolved} 项，清理陈旧 ${stale} 项，跳过 ${skipped} 项`, (applied || resolved || stale) ? 'success' : 'error');
    await refreshActiveMaintenanceView({preserveScroll: true, view: 'paths'});
    await loadArtists();
  } catch (e) {
    toast('批量路径确认失败：' + e.message, 'error');
  } finally {
    setActionBusy('move-group-action', busyKey, false);
  }
}

export async function autoResolveMoveCandidates(options = {}) {
  const silent = options.silent === true;
  const refresh = options.refresh !== false;
  const updateArtists = options.updateArtists !== false;
  if (isActionBusy('move-auto-resolve')) return null;
  setActionBusy('move-auto-resolve', '', true);
  const btn = $('#moveAutoResolveBtn');
  const oldText = btn ? btn.textContent : '';
  if (btn && !silent) {
    btn.disabled = true;
    btn.textContent = '处理中';
  }
  try {
    const result = await API.post('/api/move-candidates/auto-resolve');
    const resolved = Number(result.resolved_existing || 0);
    const applied = Number(result.applied || 0);
    const addedAsNew = Number(result.added_as_new || 0);
    const stale = Number(result.stale || 0);
    const skipped = Number(result.skipped || 0);
    const remaining = Number(result.remaining || 0);
    if (!silent) {
      toast(`安全处理 ${applied + resolved + addedAsNew + stale} 项，新增独立文件 ${addedAsNew} 项，已在库中 ${resolved} 项，清理陈旧 ${stale} 项，跳过 ${skipped} 项，剩余 ${remaining} 项`, (applied || resolved || addedAsNew || stale) ? 'success' : 'info');
    }
    if (refresh) {
      await refreshActiveMaintenanceView({preserveScroll: true, view: 'paths'});
    }
    if (updateArtists) {
      await loadArtists();
    }
    return result;
  } catch (e) {
    if (!silent) toast('自动确认处理失败：' + e.message, 'error');
    return null;
  } finally {
    if (btn && !silent) {
      btn.disabled = false;
      btn.textContent = oldText || '处理可自动确认的项';
    }
    setActionBusy('move-auto-resolve', '', false);
  }
}

export async function runMoveAction(id, action) {
  const paths = {
    confirm: `/api/move-candidates/${id}/confirm`,
    new: `/api/move-candidates/${id}/new`,
    ignore: `/api/move-candidates/${id}/ignore`,
  };
  const path = paths[action];
  if (!path) return;
  // 计划 P3 文案终稿：独立入库与忽略各带一句不冒充"稍后处理"的确认。
  const confirms = {
    new: '将此路径单独入库，不继承旧记录的标签。不会复制或删除原文件。',
    ignore: '忽略这组新旧路径的匹配，不会把新路径作为独立文件入库。',
  };
  if (confirms[action] && !window.confirm(confirms[action])) return;
  if (isActionBusy('move-action', `${action}:${id}`)) return;
  setActionBusy('move-action', `${action}:${id}`, true);
  try {
    const result = await API.post(path);
    if (result.action === 'blocked') {
      const blockedMessages = {
        cross_artist_manual_needed: '跨画师路径变化暂不能直接确认',
        duplicate_target_candidates: '多个旧记录指向同一个新文件，请手动选择',
      };
      toast(blockedMessages[result.reason] || result.reason || '路径候选暂不能确认', 'error');
    } else if (result.action === 'moved') {
      toast('此项已处理，列表已更新', 'success');
    } else if (result.action === 'new') {
      toast('已保留为独立文件', 'success');
    } else if (result.action === 'existing') {
      toast('此路径已在库中，列表已更新', 'success');
    } else if (result.action === 'ignored') {
      toast('已忽略此匹配', 'success');
    } else if (result.reason === 'candidate_stale') {
      toast('文件状态已变化，请刷新后核对', 'error');
    } else {
      // S5/P3: unknown refusals surface a readable message; the raw reason
      // code and candidate id stay visible via moveReasonLabel/console.
      console.warn('[move-candidate] unconfirmed', {id, action, result});
      toast('未能确认，请查看详情', 'error');
    }
    await refreshActiveMaintenanceView({preserveScroll: true, view: 'paths'});
    await loadArtists();
  } catch (e) {
    toast('路径候选操作失败：' + e.message, 'error');
  } finally {
    setActionBusy('move-action', `${action}:${id}`, false);
  }
}

export function moveReasonLabel(reason) {
  const labels = {
    inode_untrusted: '同一文件待确认',
    hash_duplicate_active: '重复文件',
    hash_multiple_missing: '多个旧路径',
    manual_needed: '手动确认',
    inode: '同一文件',
    hash_unique: '只找到一个旧文件',
    category_rename: '目录改名',
    target_occupied: '目标已存在',
    missing_hash_not_ready: '等待哈希结果',
    candidate_stale: '文件状态已变化，请刷新后核对',
    no_match: '未能确认，请查看详情',
  };
  return labels[reason] || reason || '待确认';
}

function shortHash(value) {
  if (!value) return '-';
  return value.length > 12 ? value.slice(0, 12) : value;
}
