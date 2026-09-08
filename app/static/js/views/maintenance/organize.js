// Maintenance organize (organize): artist folder move and the folder
// archive workbench (template editor, plan list, execute/undo).

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy } from '../../store.js';
import { $, escHtml, formatBytes, formatSize, joinUiMeta, isAbortError } from '../../utils.js';
import { toast } from '../../logging.js';
import { loadArtists, selectArtist } from '../../router.js';

export async function loadArtistFolderMove(options = {}) {
  try {
    const fetchOptions = options.signal ? {signal: options.signal} : {};
    const result = await API.get('/api/media-roots', fetchOptions);
    state.artistFolderMoveRoots = Array.isArray(result?.roots) ? result.roots : [];
    state.artistFolderMoveError = '';
  } catch (error) {
    if (isAbortError(error)) throw error;
    state.artistFolderMoveRoots = [];
    state.artistFolderMoveError = error.message || String(error);
  }
}

function artistFolderDefaultName(artist) {
  const path = String(artist?.path || '').replace(/\\/g, '/').replace(/\/+$/, '');
  return path.split('/').pop() || String(artist?.name || '').trim();
}

// P6: the destination sent to the backend stays `root_index + relative path`,
// composed from the picked parent directory plus the folder name — no new
// path protocol.
function artistFolderMoveRelativeDestination() {
  const parent = String(state.artistFolderMoveParentPath || '').replace(/^\/+|\/+$/g, '');
  const name = String($('#artistFolderMoveDestination')?.value || '').trim();
  return [parent, name].filter(Boolean).join('/');
}

function artistFolderMoveRootIndex() {
  const rootIndex = Number($('#artistFolderMoveRoot')?.value);
  return Number.isInteger(rootIndex) ? rootIndex : null;
}

export function closeArtistFolderMoveDirectoryDialog() {
  const dialog = $('#artistFolderMoveDirectoryDialog');
  if (dialog?.open) dialog.close();
}

function renderArtistFolderMoveDirectoryDialog() {
  const path = $('#artistFolderMoveDirectoryPath');
  const list = $('#artistFolderMoveDirectoryList');
  const up = $('#artistFolderMoveDirectoryUpBtn');
  const select = $('#artistFolderMoveDirectorySelectBtn');
  if (!path || !list || !up || !select) return;
  const currentPath = state.artistFolderMoveDirectoryPath || '';
  const root = state.artistFolderMoveRoots.find(item => Number(item.index) === artistFolderMoveRootIndex());
  path.textContent = [root?.label || root?.path || '媒体目录', currentPath].filter(Boolean).join(' / ');
  up.disabled = state.artistFolderMoveDirectoryLoading || !currentPath;
  select.disabled = state.artistFolderMoveDirectoryLoading;
  select.textContent = state.artistFolderMoveDirectoryLoading ? '读取中' : '确定选择此目录';
  list.innerHTML = state.artistFolderMoveDirectoryLoading
    ? '<div class="move-empty small">读取目录中</div>'
    : (state.artistFolderMoveDirectoryEntries.length
      ? state.artistFolderMoveDirectoryEntries.map(name => `<button class="artist-folder-picker-entry" type="button" data-artist-folder-directory="${escHtml(name)}">${escHtml(name)}</button>`).join('')
      : '<div class="move-empty small">这个目录下没有可进入的文件夹</div>');
}

export async function loadArtistFolderMoveDirectories(path = '') {
  const rootIndex = artistFolderMoveRootIndex();
  if (rootIndex == null || state.artistFolderMoveDirectoryLoading) return;
  state.artistFolderMoveDirectoryLoading = true;
  renderArtistFolderMoveDirectoryDialog();
  try {
    const params = new URLSearchParams({root_index: String(rootIndex)});
    if (path) params.set('path', path);
    const result = await API.get(`/api/media-roots/directories?${params}`);
    state.artistFolderMoveDirectoryPath = String(result?.path || '');
    state.artistFolderMoveDirectoryEntries = Array.isArray(result?.directories) ? result.directories : [];
  } catch (error) {
    state.artistFolderMoveDirectoryEntries = [];
    toast('读取目录失败：' + (error.message || error), 'error');
  } finally {
    state.artistFolderMoveDirectoryLoading = false;
    renderArtistFolderMoveDirectoryDialog();
  }
}

export async function openArtistFolderMoveDirectoryDialog(opener) {
  const dialog = $('#artistFolderMoveDirectoryDialog');
  if (!dialog || typeof dialog.showModal !== 'function' || !state.currentArtist || artistFolderMoveRootIndex() == null) return;
  state.artistFolderMoveDirectoryPath = '';
  state.artistFolderMoveDirectoryEntries = [];
  dialog.showModal();
  await loadArtistFolderMoveDirectories();
  opener?.blur();
}

export function chooseArtistFolderMoveDirectory() {
  if (!$('#artistFolderMoveDestination')) return;
  // P6: the picker now fills the parent directory only; the folder name keeps
  // its own field and the final target is composed from both.
  state.artistFolderMoveParentPath = String(state.artistFolderMoveDirectoryPath || '').replace(/^\/+|\/+$/g, '');
  closeArtistFolderMoveDirectoryDialog();
  invalidateArtistFolderMovePreview();
}

export function renderOrganizeScopeArtist() {
  const scopeArtist = $('#organizeScopeArtist');
  if (scopeArtist) {
    const artist = state.currentArtist;
    scopeArtist.textContent = artist && artist.name ? String(artist.name) : '未选择画师';
  }
  renderOrganizeArtistSelect();
}

export function renderOrganizeArtistSelect() {
  const select = $('#organizeArtistSelect');
  const prevBtn = $('#organizeArtistPrevBtn');
  const nextBtn = $('#organizeArtistNextBtn');
  if (!select) return;
  const artists = Array.isArray(state.artists) ? state.artists : [];
  const currentId = state.currentArtist ? Number(state.currentArtist.id) : null;
  const currentIndex = currentId != null ? artists.findIndex(a => Number(a.id) === currentId) : -1;

  const options = [
    '<option value="">选择画师...</option>',
    ...artists.map(a => `<option value="${Number(a.id)}"${currentId === Number(a.id) ? ' selected' : ''}>${escHtml(a.name || a.path || `画师 #${a.id}`)}</option>`),
  ];
  select.innerHTML = options.join('');
  if (currentId != null) select.value = String(currentId);

  if (prevBtn) {
    prevBtn.disabled = currentIndex <= 0;
  }
  if (nextBtn) {
    nextBtn.disabled = currentIndex < 0 || currentIndex >= artists.length - 1;
  }
}

export async function switchOrganizeArtist(targetArtistId) {
  const id = Number(targetArtistId);
  if (!id) return;
  await selectArtist(id, {history: false, loadItems: false});
  await Promise.all([
    loadArtistFolderMove(),
    loadArchiveWorkbench(),
  ]);
  renderArtistFolderMove();
  renderArchiveWorkbench();
}

export async function stepOrganizeArtist(direction) {
  const artists = Array.isArray(state.artists) ? state.artists : [];
  if (!artists.length) return;
  const currentId = state.currentArtist ? Number(state.currentArtist.id) : null;
  const currentIndex = currentId != null ? artists.findIndex(a => Number(a.id) === currentId) : -1;
  const targetIndex = direction === 'next' ? currentIndex + 1 : currentIndex - 1;
  if (targetIndex >= 0 && targetIndex < artists.length) {
    await switchOrganizeArtist(artists[targetIndex].id);
  }
}

export function renderArtistFolderMove() {
  const panel = $('#artistFolderMovePanel');
  if (!panel) return;
  const artist = state.currentArtist;
  renderOrganizeScopeArtist();
  const source = $('#artistFolderMoveSource');
  const rootSelect = $('#artistFolderMoveRoot');
  const destination = $('#artistFolderMoveDestination');
  const parentOutput = $('#artistFolderMoveParent');
  const targetOutput = $('#artistFolderMoveTarget');
  const browseButton = $('#artistFolderMoveBrowseBtn');
  const previewButton = $('#artistFolderMovePreviewBtn');
  const executeButton = $('#artistFolderMoveExecuteBtn');
  const result = $('#artistFolderMoveResult');
  const artistKey = artist ? String(artist.id) : '';
  const changedArtist = panel.dataset.artistId !== artistKey;
  panel.dataset.artistId = artistKey;

  if (source) source.textContent = artist?.path || '选择画师';
  if (changedArtist) {
    if (destination) destination.value = artist ? artistFolderDefaultName(artist) : '';
    state.artistFolderMoveParentPath = '';
  }
  const selectedRoot = changedArtist
    ? ''
    : String(rootSelect?.value ?? state.artistFolderMovePreview?.target_root_index ?? '');
  if (rootSelect) {
    rootSelect.innerHTML = state.artistFolderMoveRoots.map(root => (
      `<option value="${Number(root.index)}">${escHtml(root.label || root.path || `目录 ${Number(root.index) + 1}`)}</option>`
    )).join('');
    if (selectedRoot && state.artistFolderMoveRoots.some(root => String(root.index) === selectedRoot)) {
      rootSelect.value = selectedRoot;
    }
  }

  if (parentOutput) {
    parentOutput.textContent = state.artistFolderMoveParentPath || '根目录';
  }
  if (targetOutput) {
    const root = state.artistFolderMoveRoots.find(item => Number(item.index) === Number(rootSelect?.value));
    const rootLabel = root ? String(root.label || root.path || '').replace(/\/+$/, '') : '';
    const relative = artistFolderMoveRelativeDestination();
    targetOutput.textContent = rootLabel && relative ? `${rootLabel}/${relative}` : '';
  }

  const busy = state.artistFolderMoveLoading;
  const available = Boolean(artist && state.artistFolderMoveRoots.length && artistFolderMoveRelativeDestination());
  if (rootSelect) rootSelect.disabled = busy || !artist;
  if (destination) destination.disabled = busy || !artist;
  if (browseButton) browseButton.disabled = busy || !artist || !state.artistFolderMoveRoots.length;
  if (previewButton) {
    previewButton.disabled = busy || !available;
    previewButton.textContent = busy ? '处理中' : '预览';
  }
  const preview = state.artistFolderMovePreview;
  const canExecute = Boolean(preview?.can_execute && Number(preview.artist_id) === Number(artist?.id));
  if (executeButton) executeButton.disabled = busy || !canExecute;

  if (!result) return;
  if (!artist) {
    result.textContent = '先选择画师';
  } else if (state.artistFolderMoveError) {
    result.textContent = `读取失败：${state.artistFolderMoveError}`;
  } else if (!preview || Number(preview.artist_id) !== Number(artist.id)) {
    result.textContent = '';
  } else {
    const conflicts = Array.isArray(preview.conflicts) ? preview.conflicts : [];
    const status = preview.can_execute
      ? '可以移动'
      : (preview.target_exists ? '目标文件夹已存在，不会覆盖' : `与现有记录冲突（${conflicts.length} 处）`);
    const conflictPaths = conflicts.map(row => String(row?.path || '')).filter(Boolean).slice(0, 2);
    const conflictNote = conflicts.length
      ? `<div class="move-warning">${escHtml(conflictPaths.join('；'))}${conflicts.length > 2 ? ` 等 ${conflicts.length} 处` : ''}</div>`
      : '';
    // P6: 来源与去向并排展示，容量随 P1 的 formatSize。
    result.innerHTML = `
      <strong>${escHtml(status)}</strong>
      <div class="artist-folder-move-paths">
        <div><span>来源</span><code title="${escHtml(preview.source || '')}">${escHtml(preview.source || '')}</code></div>
        <div><span>去向</span><code title="${escHtml(preview.target || '')}">${escHtml(preview.target || '')}</code></div>
      </div>
      <span>${Number(preview.item_count || 0)} 个文件 \u00b7 ${preview.total_size != null ? formatSize(preview.total_size) : '容量未上报'}</span>
      ${conflictNote}`;
  }
}

export function invalidateArtistFolderMovePreview() {
  state.artistFolderMovePreview = null;
  renderArtistFolderMove();
}

export async function previewArtistFolderMove() {
  const artist = state.currentArtist;
  const rootIndex = artistFolderMoveRootIndex();
  const destination = artistFolderMoveRelativeDestination();
  if (!artist || rootIndex == null || !destination || state.artistFolderMoveLoading) return;
  state.artistFolderMoveLoading = true;
  state.artistFolderMoveError = '';
  renderArtistFolderMove();
  try {
    state.artistFolderMovePreview = await API.postJson(`/api/artists/${artist.id}/folder-move/preview`, {
      root_index: rootIndex,
      destination,
    });
  } catch (error) {
    state.artistFolderMovePreview = null;
    state.artistFolderMoveError = error.message || String(error);
    toast('预览画师文件夹移动失败', 'error');
  } finally {
    state.artistFolderMoveLoading = false;
    renderArtistFolderMove();
  }
}

export async function executeArtistFolderMove() {
  const artist = state.currentArtist;
  const preview = state.artistFolderMovePreview;
  if (!artist || !preview?.can_execute || state.artistFolderMoveLoading) return;
  // P6 确认终稿：路径来自当前有效预览。
  if (!confirm(`将整个画师文件夹从以下位置移动到新位置。\n从：${preview.source || ''}\n到：${preview.target || ''}\n\n执行前会备份数据库。目标已存在时不会覆盖。`)) return;
  state.artistFolderMoveLoading = true;
  state.artistFolderMoveError = '';
  renderArtistFolderMove();
  try {
    const moveResult = await API.postJson(`/api/artists/${artist.id}/folder-move/execute`, {
      root_index: Number(preview.target_root_index),
      destination: preview.destination,
    }, {timeoutMs: 600000});
    state.artistFolderMovePreview = null;
    await loadArtists();
    toast(moveResult?.cleanup_error ? '文件夹已移动，但旧目录临时副本清理失败' : '画师文件夹已移动', moveResult?.cleanup_error ? 'error' : 'success');
  } catch (error) {
    state.artistFolderMoveError = error.message || String(error);
    toast('移动画师文件夹失败', 'error');
  } finally {
    state.artistFolderMoveLoading = false;
    renderArtistFolderMove();
  }
}

function archiveCurrentArtistId() {
  return state.currentArtist ? Number(state.currentArtist.id) : 0;
}

function archiveSettingsPayload(value = state.archiveSettings) {
  return value?.settings && typeof value.settings === 'object' ? value.settings : value;
}

function archiveProfiles(settings = archiveSettingsPayload()) {
  return Array.isArray(settings?.profiles) ? settings.profiles.filter(Boolean) : [];
}

function archiveSelectedProfileId(settings = archiveSettingsPayload()) {
  const profiles = archiveProfiles(settings);
  return String(profiles.find(profile => String(profile.id) === 'default')?.id || profiles[0]?.id || '');
}

function archiveProfileById(profileId, settings = archiveSettingsPayload()) {
  return archiveProfiles(settings).find(profile => String(profile.id) === String(profileId)) || null;
}

function cloneArchiveSettings(settings = archiveSettingsPayload()) {
  return JSON.parse(JSON.stringify(settings || {}));
}

function populateArchiveProfileEditor(profile) {
  const template = $('#archiveTemplateInput');
  const collision = $('#archiveCollisionSelect');
  if (template) template.value = String(profile?.template || '');
  if (collision) collision.value = ['reject', 'merge'].includes(profile?.collision_strategy) ? profile.collision_strategy : 'suffix';
}

function archiveSettingsFromEditor() {
  const settings = cloneArchiveSettings();
  const profileId = archiveSelectedProfileId(settings);
  const profile = archiveProfileById(profileId, settings);
  const template = String($('#archiveTemplateInput')?.value || '').trim();
  if (!profile || !template) throw new Error('目标文件夹命名不能为空');
  profile.name = 'Default';
  profile.template = template;
  const collision = $('#archiveCollisionSelect')?.value;
  profile.collision_strategy = ['reject', 'merge'].includes(collision) ? collision : 'suffix';
  settings.active_profile_id = profileId;
  settings.default_profile_id = profileId;
  settings.artist_profile_ids = {};
  return settings;
}

async function persistArchiveSettings(artistId) {
  const settings = archiveSettingsFromEditor();
  const result = await API.putJson('/api/folder-renames/settings', {settings, artist_id: artistId});
  state.archiveSettings = archiveSettingsPayload(result) || settings;
}

// S1: the editor is only a draft until 保存规则 commits it; this compares the
// editor against the persisted profile so preview can refuse stale rules.
function archiveEditorIsDirty() {
  const settings = archiveSettingsPayload();
  const profile = archiveProfileById(archiveSelectedProfileId(settings), settings);
  if (!profile) return false;
  const template = String($('#archiveTemplateInput')?.value || '').trim();
  const collision = $('#archiveCollisionSelect')?.value;
  const normalizedCollision = ['reject', 'merge'].includes(collision) ? collision : 'suffix';
  return template !== String(profile.template || '').trim()
    || normalizedCollision !== profile.collision_strategy;
}

export function syncArchiveRuleDirtyState() {
  const ruleStatus = $('#archiveRuleStatus');
  if (!ruleStatus) return;
  if (archiveEditorIsDirty()) {
    if (!ruleStatus.dataset.baseText) ruleStatus.dataset.baseText = ruleStatus.textContent;
    ruleStatus.textContent = '规则未保存（草稿已生效，执行时自动保存）';
  } else if (ruleStatus.dataset.baseText) {
    ruleStatus.textContent = ruleStatus.dataset.baseText;
    delete ruleStatus.dataset.baseText;
  }
  const previewBtn = $('#archivePlansPreviewBtn');
  if (previewBtn) previewBtn.disabled = !archiveCurrentArtistId();
}

let archiveDraftPreviewTimer = null;
let archivePreviewAbortController = null;
let archivePreviewReqSeq = 0;

export function scheduleArchiveDraftPreview(delayMs = 350) {
  if (archiveDraftPreviewTimer) clearTimeout(archiveDraftPreviewTimer);
  archiveDraftPreviewTimer = setTimeout(() => {
    archiveDraftPreviewTimer = null;
    if (archiveEditorIsDirty() && archiveCurrentArtistId()) {
      previewArchivePlans({silent: true}).catch(() => {});
    }
  }, delayMs);
}

export async function saveArchiveSettings() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-settings-save')) return;
  setActionBusy('archive-settings-save', '', true);
  try {
    await persistArchiveSettings(artistId);
    state.archivePreview = null;
    await loadArchiveWorkbench({keepRun: true});
    toast('整理方式已保存', 'success');
  } catch (e) {
    toast('保存整理方式失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-settings-save', '', false);
  }
}

export async function previewArchivePlans(options = {}) {
  const artistId = archiveCurrentArtistId();
  if (!artistId) return;
  const isDirty = archiveEditorIsDirty();
  // S1: previewing must not silently save the editor; only 保存规则 persists.
  if (isDirty) {
    syncArchiveRuleDirtyState();
    // 规则未保存：以草稿规则直接生成预览，不提前静默持久化到数据库
  }
  if (archivePreviewAbortController) {
    try { archivePreviewAbortController.abort(); } catch (_) {}
  }
  archivePreviewAbortController = new AbortController();
  const currentSeq = ++archivePreviewReqSeq;
  setActionBusy('archive-preview', '', true);
  try {
    const templateInput = $('#archiveTemplateInput');
    const collisionSelect = $('#archiveCollisionSelect');
    const payload = {
      artist_id: artistId,
      profile_id: archiveSelectedProfileId(),
    };
    if (isDirty && templateInput && templateInput.value.trim()) {
      payload.template = templateInput.value.trim();
    }
    if (isDirty && collisionSelect && collisionSelect.value) {
      payload.collision_strategy = collisionSelect.value;
    }
    const res = await API.postJson('/api/folder-renames/preview', payload, {
      signal: archivePreviewAbortController.signal,
    });
    if (currentSeq !== archivePreviewReqSeq) return;
    state.archivePreview = res;
    renderArchiveWorkbench();
  } catch (e) {
    if (isAbortError(e)) return;
    if (currentSeq !== archivePreviewReqSeq) return;
    state.archivePreview = {error: e.message || String(e)};
    renderArchiveWorkbench();
    if (!options.silent) toast('预览目标失败：' + (e.message || e), 'error');
  } finally {
    if (currentSeq === archivePreviewReqSeq) {
      setActionBusy('archive-preview', '', false);
    }
  }
}

export async function applyArchiveTemplate() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-apply')) return;
  setActionBusy('archive-apply', '', true);
  try {
    await persistArchiveSettings(artistId);
    const result = await API.postJson('/api/folder-renames/apply-template', {
      artist_id: artistId,
      profile_id: archiveSelectedProfileId(),
    });
    state.archivePreview = result.preview || result;
    await loadArchiveWorkbench({keepRun: true});
    const updated = Number(result.updated || result.applied || 0);
    toast(updated ? `已更新 ${updated} 个整理项` : '没有可更新的整理项', updated ? 'success' : 'info');
  } catch (e) {
    toast('更新整理项失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-apply', '', false);
  }
}

function archivePlanRows(value = state.archivePlans) {
  if (Array.isArray(value)) return value.filter(plan => plan && typeof plan === 'object');
  if (value && Array.isArray(value.plans)) return value.plans.filter(plan => plan && typeof plan === 'object');
  return [];
}

function archivePlanHasTarget(plan) {
  return plan?.plan_kind === 'split_by_tag'
    ? Number(plan.split_action_count || 0) > 0
    : Boolean(plan?.target_folder);
}

function archiveStatusLabel(status) {
  const labels = {
    draft: '草稿',
    needs_date: '缺少有效日期',
    date_conflict: '跨月份冲突',
    needs_tags: '待补标签',
    inconsistent_tags: '标签不一致',
    manual_review: '需人工处理',
    aligned: '已符合规则',
    stale: '已过期',
    ready: '待确认',
    confirmed: '已确认',
    executed: '已整理',
    reverted: '已撤销',
    blocked: '已阻止',
    error: '失败',
  };
  return labels[status] || status || '草稿';
}

function archiveStatusExplanation(plan) {
  const status = plan.status;
  if (status === 'needs_date') return '文件夹内媒体缺少有效识别或手动日期，请在媒体卡片或批量修改中设置日期';
  if (status === 'date_conflict') return '文件夹内媒体的有效日期跨越了多个月份，请统一月份后再整理';
  if (status === 'inconsistent_tags') return '文件会按各自的有效月份和标签分开整理，未打标签的文件保留原处';
  if (status === 'aligned') return '目录已符合 Default 规则，无需整理';
  if (status === 'manual_review') return '此项需要核对，请查看执行记录';
  if (status === 'blocked') return '当前路径不能安全整理，未执行移动';
  if (plan.plan_kind === 'split_by_tag' && status !== 'executed') return '将按每个文件的有效月份和标签分开整理；未打标签的文件保留原处';
  return '';
}

export async function refreshArchivePlans() {
  const artistId = archiveCurrentArtistId();
  if (!artistId) {
    await loadArchiveWorkbench();
    return;
  }
  let refreshFailed = false;
  try {
    await API.post(`/api/folder-renames/refresh?artist_id=${encodeURIComponent(artistId)}`);
  } catch (e) {
    // S8: a failed refresh must be visible; the list below stays as-is.
    if (!isAbortError(e)) refreshFailed = true;
  }
  await loadArchiveWorkbench();
  if (refreshFailed) toast('刷新整理项失败，当前列表可能不是最新', 'error');
}

export async function loadArchiveWorkbench(options = {}) {
  // P6: no implicit artist selection any more — entering the organize view
  // with no artist picked shows 先选择画师 and keeps the move actions disabled
  // until the user chooses one from the artist picker.
  const artistId = archiveCurrentArtistId();
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  if (!artistId) {
    if (updateState) {
      state.archiveSettings = null;
      state.archivePlans = [];
      state.archivePreview = null;
      state.archiveRun = null;
    }
    if (render) renderArchiveWorkbench();
    return {plans: []};
  }
  try {
    const [settingsResult, plansResult] = await Promise.all([
      API.get(`/api/folder-renames/settings?artist_id=${encodeURIComponent(artistId)}`, fetchOptions),
      API.get(`/api/folder-renames?artist_id=${encodeURIComponent(artistId)}`, fetchOptions),
    ]);
    const settings = archiveSettingsPayload(settingsResult);
    const plans = archivePlanRows(plansResult);
    if (updateState) {
      state.archiveSettings = settings;
      state.archivePlans = plans;
      if (!options.keepPreview) state.archivePreview = null;
      if (!options.keepRun) {
        state.archiveRun = null;
      }
      state.archiveWorkbenchLoading = false;
    }
    if (render) renderArchiveWorkbench();
    return {settings, plans};
  } catch (e) {
    if (isAbortError(e)) throw e;
    if (updateState) {
      state.archiveSettings = {error: e.message || String(e), profiles: []};
      state.archivePlans = [];
      state.archivePreview = null;
      state.archiveRun = null;
      state.archiveWorkbenchLoading = false;
    }
    if (render) renderArchiveWorkbench();
    return {plans: []};
  }
}

export function renderArchiveWorkbench() {
  const planList = $('#archivePlanList');
  const planSummary = $('#archivePlanSummary');
  const previewSummary = $('#archivePreviewSummary');
  const ruleStatus = $('#archiveRuleStatus');
  const templateInput = $('#archiveTemplateInput');
  const collisionSelect = $('#archiveCollisionSelect');
  if (!planList || !planSummary || !previewSummary || !ruleStatus) return;

  const artistId = archiveCurrentArtistId();
  const controls = [
    $('#archiveProfileSaveBtn'),
    $('#archivePlansRefreshBtn'),
    $('#archivePlansPreviewBtn'),
    $('#archivePlansApplyBtn'),
    $('#archivePlansConfirmAllBtn'),
    $('#archivePlansDryRunBtn'),
    $('#archivePlansExecuteBtn'),
  ].filter(Boolean);

  for (const control of controls) control.disabled = !artistId;
  const settings = archiveSettingsPayload();
  const profile = archiveProfileById(archiveSelectedProfileId(settings), settings);
  if (templateInput) templateInput.disabled = !artistId || !profile;
  if (collisionSelect) collisionSelect.disabled = !artistId || !profile;
  populateArchiveProfileEditor(profile);

  if (!artistId) {
    ruleStatus.textContent = '选择画师后编辑 Default 命名规则';
    planSummary.textContent = '选择画师后读取整理项';
    previewSummary.textContent = '';
    planList.innerHTML = '<div class="move-empty small">选择画师后管理待整理的文件夹</div>';
    return;
  }

  if (settings?.error) {
    ruleStatus.textContent = `读取 Default 规则失败：${settings.error}`;
  } else if (profile) {
    const collisionLabel = profile.collision_strategy === 'reject'
      ? '冲突跳过'
      : (profile.collision_strategy === 'merge' ? '合并同名文件夹' : '冲突自动编号');
    ruleStatus.textContent = `Default \u00b7 ${collisionLabel}`;
  } else {
    ruleStatus.textContent = 'Default 规则不可用';
  }
  // S1: unsaved editor edits surface as draft state; previewing remains enabled.
  if (archiveEditorIsDirty()) {
    ruleStatus.dataset.baseText = ruleStatus.textContent;
    ruleStatus.textContent = '规则未保存（草稿已生效，执行时自动保存）';
  } else {
    delete ruleStatus.dataset.baseText;
  }
  const previewBtn = $('#archivePlansPreviewBtn');
  if (previewBtn) previewBtn.disabled = !artistId;

  const plans = archivePlanRows();
  const confirmed = plans.filter(plan => plan.status === 'confirmed').length;
  const ready = plans.filter(plan => plan.status === 'ready').length;
  const executed = plans.filter(plan => plan.status === 'executed').length;
  const reverted = plans.filter(plan => plan.status === 'reverted').length;
  const needsAttention = plans.filter(plan => ['needs_date', 'date_conflict', 'inconsistent_tags', 'manual_review', 'blocked'].includes(plan.status)).length;
  planSummary.textContent = joinUiMeta([
    `${plans.length} 个文件夹`,
    ready ? `${ready} 待确认` : '',
    confirmed ? `${confirmed} 已确认（将执行）` : '',
    needsAttention ? `${needsAttention} 需处理` : '',
    executed ? `${executed} 已整理` : '',
    reverted ? `${reverted} 已撤销` : '',
  ]);

  if (state.archiveRun?.results?.length) {
    const blocked = state.archiveRun.results.filter(row => row.status === 'error' || row.reason || row.error).length;
    previewSummary.textContent = joinUiMeta([
      `${state.archiveRun.dry_run ? '检查' : '执行'}完成：${state.archiveRun.results.length} 个整理项`,
      blocked ? `${blocked} 个需处理` : state.archiveRun.dry_run ? '全部可以执行' : '全部成功',
    ]);
  } else {
    const previewPlans = state.archivePreview?.plans || [];
    if (state.archivePreview?.error) {
      previewSummary.textContent = `预览失败：${state.archivePreview.error}`;
    } else if (previewPlans.length) {
      const conflicts = state.archivePreview?.conflicts?.length || 0;
      previewSummary.textContent = `${previewPlans.length} 个目标预览${conflicts ? `，${conflicts} 个冲突` : '，均可应用'}`;
    } else {
      previewSummary.textContent = '';
    }
  }

  const isAutoOrganize = Boolean(state.folderRenameAuto?.enabled);
  const confirmAllBtn = $('#archivePlansConfirmAllBtn');
  if (confirmAllBtn) {
    let autoBadge = $('#archiveAutoBadge');
    if (isAutoOrganize) {
      // Auto mode makes manual confirmation meaningless; render the state as a
      // plain badge instead of a disabled button that looks broken.
      confirmAllBtn.hidden = true;
      if (!autoBadge) {
        autoBadge = document.createElement('span');
        autoBadge.id = 'archiveAutoBadge';
        autoBadge.className = 'archive-auto-badge';
        confirmAllBtn.after(autoBadge);
      }
      autoBadge.textContent = '自动整理已开启（无需确认）';
      autoBadge.title = '自动整理开启时，符合规则的文件夹将在全库扫描时自动处理，无需手动确认';
    } else {
      confirmAllBtn.hidden = false;
      if (autoBadge) autoBadge.remove();
      const unconfirmedReady = plans.filter(plan => plan.status === 'ready' && archivePlanHasTarget(plan));
      const confirmedPlans = plans.filter(plan => plan.status === 'confirmed');
      if (unconfirmedReady.length > 0) {
        confirmAllBtn.textContent = `全部确认（${unconfirmedReady.length}）`;
        confirmAllBtn.disabled = !artistId;
      } else if (confirmedPlans.length > 0) {
        confirmAllBtn.textContent = `全部取消确认（${confirmedPlans.length}）`;
        confirmAllBtn.disabled = !artistId;
      } else {
        confirmAllBtn.textContent = '全部确认';
        confirmAllBtn.disabled = true;
      }
    }
  }

  planList.innerHTML = plans.length ? plans.map(plan => {
    const planId = Number(plan.id);
    const locked = plan.status === 'executed';
    const isSplit = plan.plan_kind === 'split_by_tag';
    const splitTargets = (Array.isArray(plan.target_folders) ? plan.target_folders : [])
      .map(value => String(value || '')).filter(Boolean);
    const hasTarget = archivePlanHasTarget(plan);
    const canConfirm = (plan.status === 'ready' || plan.status === 'confirmed') && hasTarget;
    const undoing = isActionBusy('archive-plan-undo', String(planId));
    const canUndo = !isSplit && plan.status === 'executed' && Number.isFinite(planId) && !undoing;
    const target = isSplit ? splitTargets.join('；') : String(plan.target_folder || '');
    const targetLabel = isSplit ? `拆分到 ${splitTargets.length || Number(plan.split_action_count || 0)} 个位置` : '整理后名称';
    const explanation = archiveStatusExplanation(plan);
    const confirmLabel = plan.status === 'confirmed' ? '取消确认' : '确认';
    const confirmBtnMarkup = isAutoOrganize
      ? `<span class="archive-auto-hint" title="自动整理开启时，全库扫描完成后自动处理">自动就绪</span>`
      : `<button class="btn btn-ghost" type="button" data-archive-plan-confirm="${planId}" ${locked || !canConfirm ? 'disabled' : ''}>${confirmLabel}</button>`;
    return `
      <div class="archive-plan-row${plan.status === 'executed' ? ' executed' : ''}${plan.status === 'reverted' ? ' reverted' : ''}" data-archive-plan-id="${planId}">
        <div class="archive-plan-row-head">
          <code title="${escHtml(String(plan.source_folder || ''))}">${escHtml(String(plan.source_folder || '-'))}</code>
          <span class="archive-plan-status ${escHtml(String(plan.status || 'draft'))}">${escHtml(archiveStatusLabel(plan.status))}</span>
        </div>
        <div class="archive-plan-target">
          <span>${escHtml(targetLabel)}</span>
          <code class="archive-target-preview" title="${escHtml(target)}">${escHtml(target || '（未生成目标路径）')}</code>
        </div>
        ${explanation ? `<div class="archive-plan-preview${plan.status === 'aligned' ? '' : ' blocked'}">${escHtml(explanation)}</div>` : ''}
        <div class="archive-plan-row-actions">
          <span>${plan.file_count ? `${plan.file_count} 项` : ''}</span>
          <div class="archive-plan-row-controls">
            ${confirmBtnMarkup}
            ${plan.status === 'executed' && !isSplit ? `<button class="btn btn-ops" type="button" data-archive-plan-undo="${planId}" ${canUndo ? '' : 'disabled'} ${undoing ? 'aria-busy="true"' : ''}>${undoing ? '撤销中' : '撤销整理'}</button>` : ''}
          </div>
        </div>
      </div>`;
  }).join('') : '<div class="move-empty small">暂无待整理的文件夹</div>';
}

export async function toggleArchivePlanConfirmation(planId) {
  const plan = archivePlanRows().find(item => Number(item.id) === Number(planId));
  if (!plan || isActionBusy('archive-plan-confirm', String(planId))) return;
  const endpoint = plan.status === 'confirmed' ? 'unconfirm' : 'reconfirm';
  setActionBusy('archive-plan-confirm', String(planId), true);
  try {
    await API.post(`/api/folder-renames/plans/${Number(planId)}/${endpoint}`);
    await loadArchiveWorkbench();
    toast(plan.status === 'confirmed' ? '已取消确认' : '整理项已确认', 'success');
  } catch (e) {
    toast('更新整理项失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-confirm', String(planId), false);
  }
}

export async function toggleAllArchivePlansConfirmation() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-confirm-all')) return;
  const plans = archivePlanRows();
  const unconfirmedReady = plans.filter(plan => plan.status === 'ready' && archivePlanHasTarget(plan));
  setActionBusy('archive-confirm-all', '', true);
  try {
    if (unconfirmedReady.length > 0) {
      const result = await API.postJson('/api/folder-renames/confirm-all', {artist_id: artistId});
      const count = Number(result?.confirmed || 0);
      toast(count ? `已全部确认 ${count} 个整理项` : '没有可确认的整理项', count ? 'success' : 'info');
    } else {
      const result = await API.postJson('/api/folder-renames/unconfirm-all', {artist_id: artistId});
      const count = Number(result?.unconfirmed || 0);
      toast(`已全部取消确认（${count} 个整理项）`, 'info');
    }
    await loadArchiveWorkbench();
  } catch (e) {
    toast('批量确认失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-confirm-all', '', false);
  }
}

export async function undoArchivePlan(planId) {
  const id = Number(planId);
  const plan = archivePlanRows().find(item => Number(item.id) === id);
  if (!plan || !Number.isFinite(id) || plan.status !== 'executed' || isActionBusy('archive-plan-undo', String(id))) return;
  const source = String(plan.source_folder || '原始位置');
  const target = String(plan.target_folder || '整理后的位置');
  if (!window.confirm(`确定要撤销本次整理吗？\n将把「${target}」恢复至原始路径「${source}」。`)) return;
  setActionBusy('archive-plan-undo', String(id), true);
  renderArchiveWorkbench();
  try {
    const result = await API.post(`/api/folder-renames/plans/${id}/undo`);
    if (result?.ok === false) throw new Error(archiveUndoFailureLabel(result.message || result.reason));
    await Promise.all([
      loadArchiveWorkbench({render: false}),
      loadOperationLog({render: false}),
    ]);
    renderArchiveWorkbench();
    renderOperationLog();
    toast(result?.message || '整理已撤销', 'success');
  } catch (e) {
    const reason = e?.body?.message || e?.body?.reason || e?.body?.error
      || e?.detail?.message || e?.detail?.reason || e?.message || e;
    toast('撤销整理失败：' + archiveUndoFailureLabel(reason), 'error');
  } finally {
    setActionBusy('archive-plan-undo', String(id), false);
    renderArchiveWorkbench();
  }
}

function archiveUndoFailureLabel(reason) {
  const labels = {
    plan_not_found: '整理项不存在',
    plan_not_executed: '该整理项当前不能撤销',
    not_executed: '该整理项当前不能撤销',
    artist_missing: '画师目录不存在',
    target_missing: '整理后的文件夹不存在',
    target_not_directory: '整理后的位置不是文件夹',
    source_exists: '原始位置已被占用',
    stale_state: '整理项状态已变更，请刷新后重试',
    outside_artist: '路径不在画师目录内',
  };
  return labels[reason] || String(reason || '当前无法撤销整理');
}

export async function executeArchivePlans(dryRun) {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-plan-execute')) return;
  const isDirty = archiveEditorIsDirty();
  const template = String($('#archiveTemplateInput')?.value || '').trim();
  const plans = archivePlanRows();
  const unconfirmedReady = plans.filter(p => p.status === 'ready' && archivePlanHasTarget(p));
  const confirmMsg = isDirty
    ? `确定要执行整理操作吗？\n将同时保存当前修改的规则模板「${template}」并执行整理。系统将在执行前自动创建数据库备份，确保数据安全。`
    : '确定要执行整理操作吗？\n系统将在执行前自动创建数据库备份，确保数据安全。';
  if (!dryRun && !window.confirm(confirmMsg)) return;
  setActionBusy('archive-plan-execute', '', true);
  try {
    if (!dryRun && isDirty) {
      try {
        await persistArchiveSettings(artistId);
        syncArchiveRuleDirtyState();
      } catch (saveErr) {
        toast('保存整理规则失败，已中止整理：' + (saveErr.message || saveErr), 'error');
        return;
      }
    }
    const isAuto = Boolean(state.folderRenameAuto?.enabled);
    // S2: 检查（dry run）never confirms; and an auto-confirm failure must be
    // visible instead of silently falling through to execution.
    if (!dryRun && isAuto && unconfirmedReady.length > 0) {
      try {
        await API.postJson('/api/folder-renames/confirm-all', {artist_id: artistId});
      } catch (e) {
        toast('自动确认待整理项失败：' + (e.message || e), 'error');
        return;
      }
    }
    const result = await API.postJson('/api/folder-renames/execute', {artist_id: artistId, dry_run: Boolean(dryRun)}, {timeoutMs: 600000});
    state.archiveRun = {dry_run: Boolean(dryRun), results: result.results || [], execution: result};
    await loadArchiveWorkbench({keepRun: true});
    const rows = result.results || [];
    const successful = rows.filter(row => row.status === (dryRun ? 'dry_run' : 'executed')).length;
    const failed = rows.filter(row => row.status === 'error').length;
    const skipped = Math.max(0, rows.length - successful - failed);
    if (dryRun && successful === 0) {
      const confirmed = plans.filter(p => p.status === 'confirmed' && archivePlanHasTarget(p));
      if (confirmed.length > 0) {
        toast(`检查完成：0 个可执行（${confirmed.length} 个已确认但未通过检查）`, 'info');
      } else if (unconfirmedReady.length > 0) {
        toast(`检查完成：0 个可执行（${unconfirmedReady.length} 个待确认）`, 'info');
      } else {
        toast('检查完成：0 个可执行', 'info');
      }
    } else if (!dryRun && failed > 0) {
      // S9: a mixed run must not read as a full success.
      toast(`部分整理失败：${successful} 个成功${skipped ? `、${skipped} 个跳过` : ''}、${failed} 个失败`, 'error');
    } else {
      toast(dryRun ? `检查完成：${successful} 个整理项可执行` : `整理完成：${successful} 个整理项`, successful ? 'success' : 'info');
    }
  } catch (e) {
    toast((dryRun ? '执行检查失败：' : '整理操作失败：') + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-execute', '', false);
  }
}

// Late imports closing the organize <-> records cycle; undoArchivePlan refreshes
// the operation log after a successful undo.
import { loadOperationLog, renderOperationLog } from './records.js';
