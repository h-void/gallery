// Maintenance characters (characters): importable single-tag list, built
// characters, reference images, and the import job poller.

import { API } from '../../api.js';
import { state, nextRequestSeq, isCurrentRequestSeq, isActionBusy, setActionBusy } from '../../store.js';
import {
  $, escHtml, joinUiMeta, formatBytes, formatHealthTime, buttonIcon, isAbortError,
} from '../../utils.js';
import { toast } from '../../logging.js';

const CHARACTER_IMPORT_POLL_MS = 1000;
const CHARACTER_LIBRARY_VIEWS = ['import', 'characters', 'references'];
// Mobile shows one library zone at a time; the list scroll offset is kept so
// returning from the reference view lands where the user left.
let pendingCharacterListScroll = 0;

function characterLibraryIsMobileView() {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
  return window.matchMedia('(max-width:768px)').matches;
}

function characterLibraryScrollContainer() {
  const list = $('#characterList');
  if (!list || typeof window === 'undefined' || !list.parentElement) return null;
  let node = list.parentElement;
  while (node) {
    const style = window.getComputedStyle ? window.getComputedStyle(node) : null;
    const overflowY = style ? style.overflowY : '';
    if ((overflowY === 'auto' || overflowY === 'scroll') && node.scrollHeight > node.clientHeight) return node;
    node = node.parentElement;
  }
  return null;
}

function rememberCharacterListScroll() {
  const container = characterLibraryScrollContainer();
  pendingCharacterListScroll = container ? container.scrollTop : 0;
}

function restoreCharacterListScroll() {
  if (pendingCharacterListScroll <= 0) return;
  const container = characterLibraryScrollContainer();
  if (container) container.scrollTop = pendingCharacterListScroll;
  pendingCharacterListScroll = 0;
}

export function applyCharacterLibraryMobileView() {
  const grid = $('#characterLibraryGrid');
  const views = $('#characterLibraryViews');
  const active = CHARACTER_LIBRARY_VIEWS.includes(state.characterLibraryMobileView)
    ? state.characterLibraryMobileView
    : 'characters';
  const mobile = characterLibraryIsMobileView();
  if (grid) {
    Array.from(grid.querySelectorAll('[data-character-library-panel]')).forEach(panel => {
      panel.hidden = mobile && panel.dataset.characterLibraryPanel !== active;
    });
  }
  if (views) {
    Array.from(views.querySelectorAll('[data-character-library-view]')).forEach(btn => {
      const on = btn.dataset.characterLibraryView === active;
      btn.classList.toggle('active', on);
      btn.setAttribute('aria-selected', on ? 'true' : 'false');
    });
  }
  if (active === 'characters') restoreCharacterListScroll();
}

export function setCharacterLibraryMobileView(view) {
  if (!CHARACTER_LIBRARY_VIEWS.includes(view)) return;
  if (state.characterLibraryMobileView === 'characters' && view !== 'characters') {
    rememberCharacterListScroll();
  }
  state.characterLibraryMobileView = view;
  applyCharacterLibraryMobileView();
}

// Selecting a character on mobile jumps straight to its reference images; the
// desktop layout keeps all three columns visible, so nothing changes there.
export function openCharacterReferences() {
  if (!characterLibraryIsMobileView()) return;
  setCharacterLibraryMobileView('references');
}

export function gotoCharacterLibraryPanel(panel) {
  if (!CHARACTER_LIBRARY_VIEWS.includes(panel)) return;
  if (characterLibraryIsMobileView()) {
    setCharacterLibraryMobileView(panel);
    return;
  }
  const target = document.querySelector(`[data-character-library-panel="${panel}"]`);
  if (target && typeof target.scrollIntoView === 'function') {
    target.scrollIntoView({block: 'nearest'});
  }
}

export async function loadCharacterLibrary(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  const requestedCharacterId = options.characterId != null
    ? Number(options.characterId)
    : state.characterLibrarySelectedCharacterId;
  // Direct-click path has no shared AbortController: capture a sequence token
  // so an older response can never overwrite a newer selection.
  const seq = nextRequestSeq('characterLibraryLoadSeq');
  const isCurrent = () => !updateState || isCurrentRequestSeq('characterLibraryLoadSeq', seq);
  const previousLibrary = state.characterLibrary;
  let summary = null;
  try {
    try {
      summary = await API.get('/api/characters/summary', fetchOptions);
    } catch (summaryError) {
      if (isAbortError(summaryError)) throw summaryError;
      const characterLibrary = {
        ...(previousLibrary || {}),
        summary: previousLibrary?.summary || {tags: [], characters: [], totals: {}},
        selected_character_id: requestedCharacterId,
        references: previousLibrary?.references || [],
        import_job: state.characterImportJob,
        summary_error: summaryError.message || String(summaryError),
      };
      if (!isCurrent()) return characterLibrary;
      if (updateState) {
        state.characterLibrary = characterLibrary;
        state.characterLibrarySelectedCharacterId = requestedCharacterId;
        state.characterLibraryLoading = false;
      }
      if (render) renderCharacterLibrary();
      toast('刷新角色库失败：' + (summaryError.message || summaryError), 'error');
      return characterLibrary;
    }
    if (!isCurrent()) return {summary};
    const characters = summary.characters || [];
    const selectedCharacterId = characters.length
      ? (
        requestedCharacterId && characters.some(character => Number(character.id) === Number(requestedCharacterId))
          ? Number(requestedCharacterId)
          : Number(characters[0].id)
      )
      : null;
    let references = {references: []};
    let referenceErrorMessage = '';
    if (selectedCharacterId) {
      try {
        references = await API.get(`/api/characters/${selectedCharacterId}/references?limit=200`, fetchOptions);
      } catch (referenceError) {
        if (isAbortError(referenceError)) throw referenceError;
        references = {references: previousLibrary?.references || []};
        referenceErrorMessage = referenceError.message || String(referenceError);
        toast('读取角色引用失败：' + referenceErrorMessage, 'error');
      }
    }
    let importJob = null;
    try {
      importJob = await API.get('/api/characters/import-from-tags/jobs/current', fetchOptions);
    } catch (e) {
      if (isAbortError(e)) throw e;
      importJob = state.characterImportJob;
    }
    const characterLibrary = {
      summary,
      selected_character_id: selectedCharacterId,
      references: references.references || [],
      import_job: importJob,
      reference_error: referenceErrorMessage,
    };
    if (!isCurrent()) return characterLibrary;
    if (updateState) {
      state.characterLibrary = characterLibrary;
      state.characterLibrarySelectedCharacterId = selectedCharacterId;
      state.characterLibraryLoading = false;
      state.characterImportJob = importJob;
      if (characterImportJobBusy()) startCharacterImportPolling();
    }
    if (render) renderCharacterLibrary();
    return characterLibrary;
  } catch (e) {
    if (isAbortError(e)) throw e;
    const characterLibrary = {
      error: e.message,
      summary: {tags: [], characters: [], totals: {}},
      selected_character_id: null,
      references: [],
    };
    if (!isCurrent()) return characterLibrary;
    if (updateState) {
      state.characterLibrary = characterLibrary;
      state.characterLibrarySelectedCharacterId = null;
      state.characterLibraryLoading = false;
    }
    if (render) renderCharacterLibrary();
    return characterLibrary;
  }
}

function characterLibrarySummaryText(library) {
  const summary = library && library.summary ? library.summary : {};
  const totals = summary.totals || {};
  const status = summary.status || {};
  const scope = '全部画师';
  const parts = [
    `范围 ${scope}`,
    `${totals.tags || (summary.tags || []).length || 0} 个单角色标签`,
    `${totals.characters || (summary.characters || []).length || 0} 个已建角色`,
    `${totals.references || 0} 张参考图`,
  ];
  if (status.available === false) {
    parts.push(status.reason || '角色识别未启用');
  } else if (status.backend) {
    parts.push(status.backend);
  }
  return joinUiMeta(parts);
}

function characterLibrarySelectedCharacter(library) {
  const summary = library && library.summary ? library.summary : {};
  const characters = summary.characters || [];
  const selectedId = library ? library.selected_character_id : null;
  if (!characters.length || selectedId == null) return null;
  return characters.find(character => Number(character.id) === Number(selectedId)) || null;
}

function characterIdFromImportResult(result) {
  const importedIds = result && Array.isArray(result.imported_character_ids) ? result.imported_character_ids : [];
  const importedId = importedIds.map(value => Number(value)).find(Boolean);
  if (importedId) return importedId;
  const references = result && Array.isArray(result.references) ? result.references : [];
  const reference = references.find(item => Number(item.character_id));
  if (reference) return Number(reference.character_id);
  const characters = result && result.characters && typeof result.characters === 'object' ? result.characters : {};
  const ids = Object.values(characters).map(value => Number(value)).filter(Boolean);
  return ids.length ? ids[0] : null;
}

function characterImportFailureText(result) {
  const firstReason = result && result.first_failure_reason ? String(result.first_failure_reason) : '';
  const failures = result && Array.isArray(result.failures) ? result.failures : [];
  const failureReasons = failures
    .map(failure => failure && (failure.reason || failure.error || failure.message))
    .filter(Boolean);
  const reason = firstReason || failureReasons[0] || '未返回失败原因';
  return `导入失败：${reason}`;
}

export function characterImportJobBusy() {
  const job = state.characterImportJob;
  return Boolean(job && ['pending', 'running'].includes(job.status));
}

function characterImportJobSummary(job) {
  if (!job || job.status === 'idle') return '';
  const labels = {
    pending: '等待导入',
    running: '正在导入',
    completed: '导入完成',
    failed: '导入失败',
    cancelled: '已取消',
  };
  return joinUiMeta([
    labels[job.status] || job.status || '导入任务',
    `${job.processed || 0} / ${job.total || 0}`,
    `新增 ${job.added || job.added_references || 0}`,
    `跳过 ${job.skipped_existing || job.skipped_existing_references || 0}`,
    `失败 ${job.failed || 0}`,
    job.current_tag ? `当前 ${job.current_tag}` : '',
  ]);
}

function characterImportJobMarkup(job) {
  if (!job || job.status === 'idle') return '';
  const total = Number(job.total || 0);
  const processed = Number(job.processed || 0);
  const pct = total > 0 ? Math.min(100, Math.round(processed / total * 100)) : 0;
  const busy = ['pending', 'running'].includes(job.status);
  const failures = Array.isArray(job.failures) ? job.failures : [];
  const failureText = job.first_failure_reason || (failures[0] && (failures[0].reason || failures[0].message)) || '';
  return `
    <div class="character-import-job">
      <div class="character-import-job-head">
        <span>${escHtml(characterImportJobSummary(job))}</span>
        ${busy ? `<button class="btn btn-danger" type="button" data-character-import-cancel="${escHtml(job.job_id || '')}">${buttonIcon('close')}取消</button>` : ''}
      </div>
      <div class="character-import-progress" aria-label="角色库导入进度">
        <span style="width:${pct}%"></span>
      </div>
      ${failureText ? `<div class="character-import-failures">失败：${escHtml(failureText)}</div>` : ''}
    </div>
  `;
}

function characterReferencePreviewVersion(reference) {
  return reference.file_mtime || reference.updated_at || reference.created_at || reference.file_size || '';
}

export function renderCharacterLibrary() {
  const summaryEl = $('#characterLibrarySummary');
  const tagList = $('#characterTagImportList');
  const characterList = $('#characterList');
  const referenceList = $('#characterReferenceList');
  if (!summaryEl || !tagList || !characterList || !referenceList) return;

  const library = state.characterLibrary;
  if (!library) {
    summaryEl.textContent = state.characterLibraryLoading ? '角色库读取中' : '角色特征库未加载';
    tagList.innerHTML = '<div class="character-library-empty">暂无可导入的单角色标签</div>';
    characterList.innerHTML = '<div class="character-library-empty">暂无已建角色</div>';
    referenceList.innerHTML = '<div class="character-library-empty">请先在「已建角色」列表中选择角色</div>';
    applyCharacterLibraryMobileView();
    return;
  }
  if (library.error) {
    summaryEl.textContent = `角色库读取失败：${library.error}`;
    tagList.innerHTML = `<div class="character-library-empty">${escHtml(library.error)}</div>`;
    characterList.innerHTML = '<div class="character-library-empty">角色库不可用</div>';
    referenceList.innerHTML = '<div class="character-library-empty">角色库不可用</div>';
    applyCharacterLibraryMobileView();
    return;
  }

  const summary = library.summary || {};
  const query = String(state.characterLibrarySearchQuery || '').trim().toLowerCase();
  const searchInput = $('#characterLibrarySearchInput');
  const searchClearBtn = $('#characterLibrarySearchClearBtn');
  if (searchInput && document.activeElement !== searchInput && searchInput.value !== (state.characterLibrarySearchQuery || '')) {
    searchInput.value = state.characterLibrarySearchQuery || '';
  }
  if (searchClearBtn) {
    searchClearBtn.hidden = !query;
  }

  const rawTags = summary.tags || [];
  const rawCharacters = summary.characters || [];
  const tags = query
    ? rawTags.filter(t => String(t.name || '').toLowerCase().includes(query) || String(t.character_id) === query)
    : rawTags;
  const characters = query
    ? rawCharacters.filter(c => String(c.name || '').toLowerCase().includes(query) || String(c.id) === query)
    : rawCharacters;

  // Auto-select the first character so the reference column is not blank on open.
  if (
    characters.length
    && (library.selected_character_id == null || library.selected_character_id === '')
    && (state.characterLibrarySelectedCharacterId == null || state.characterLibrarySelectedCharacterId === '')
  ) {
    const firstId = Number(characters[0].id);
    library.selected_character_id = firstId;
    state.characterLibrarySelectedCharacterId = firstId;
    // Load references for the auto-selected character without blocking first paint.
    loadCharacterLibrary({
      characterId: firstId,
    }).catch(() => {});
  }
  const references = library.references || [];
  const selectedCharacter = characterLibrarySelectedCharacter(library);
  const selectedCharacterId = selectedCharacter ? Number(selectedCharacter.id) : null;
  const currentArtistId = state.currentArtist ? Number(state.currentArtist.id) : null;
  const importBusy = characterImportJobBusy();
  const importCurrentDisabled = !currentArtistId || importBusy || isActionBusy('character-library-import', 'artist');
  const importAllDisabled = importBusy || isActionBusy('character-library-import', 'all');
  const rebuildDisabled = isActionBusy('character-library-rebuild');

  summaryEl.textContent = characterLibrarySummaryText(library);
  const jobMarkup = characterImportJobMarkup(state.characterImportJob);

  const tagButtons = tags.length ? tags.map(tag => {
    const referenceCount = Number(tag.reference_count || 0);
    const singleTagCount = Number(tag.single_tag_image_count || 0);
    const imported = referenceCount > 0;
    const stateBadge = tag.character_id
      ? `<span class="character-library-badge${imported ? ' character-library-imported' : ''}">${imported ? `${referenceCount} 张参考图` : '未导入'}</span>`
      : '<span class="character-library-badge">未导入</span>';
    const artistCount = Number(tag.artist_count || 0);
    const sourceLabel = artistCount > 1 ? `来自 ${artistCount} 个画师` : '';
    const tagName = String(tag.name || '');
    return `
      <div class="character-tag-row">
        <div class="character-tag-main">
          <b>${escHtml(tagName)}</b>
          <span>${escHtml(joinUiMeta([`${singleTagCount} 张单标签图`, sourceLabel]))}</span>
        </div>
        <div class="character-tag-actions">
          ${stateBadge}
        </div>
      </div>
    `;
  }).join('') : `<div class="character-library-empty">${query ? `未找到匹配的标签 "${escHtml(query)}"` : '暂无可导入标签'}</div>`;

  const characterButtons = characters.length ? characters.map(character => {
    const active = selectedCharacterId && Number(character.id) === Number(selectedCharacterId);
    const referenceCount = Number(character.reference_count || 0);
    return `
      <div class="character-card-shell${active ? ' active' : ''}">
        <button type="button" class="btn character-card-select" data-character-select="${character.id}">
          <div class="character-card-head">
            <b>${escHtml(character.name)}</b>
          </div>
          <div class="character-card-meta">
            <div class="character-card-meta-row">
              <span>${escHtml(`${referenceCount} 张参考图`)}</span>
            </div>
          </div>
        </button>
        <div class="character-card-actions">
          <button type="button" class="btn btn-ghost btn-icon character-card-search" data-character-search="${escHtml(character.name)}" title="在画廊中检索「${escHtml(character.name)}」" aria-label="在画廊中检索「${escHtml(character.name)}」">${buttonIcon('search')}</button>
          <button type="button" class="btn btn-danger btn-icon character-card-delete" data-character-delete="${character.id}" title="删除角色" aria-label="删除角色">${buttonIcon('trash')}</button>
        </div>
      </div>
    `;
  }).join('') : `<div class="character-library-empty">${query ? `未找到匹配的角色 "${escHtml(query)}"` : '暂无角色<button class="btn character-library-empty-action" type="button" data-character-library-goto="import">导入标签</button>'}</div>`;
  const referenceCards = references.length ? references.map(reference => {
    const pathText = reference.display_file_path || reference.file_path || reference.file_name || '未绑定文件';
    const previewUrl = reference.file_path ? API.previewUrl(reference.file_path, characterReferencePreviewVersion(reference), 256) : '';
    const SOURCE_LABELS = {tag_single: '来自标签', manual: '手动添加'};
    const MEDIA_LABELS = {image: '图片', video: '视频', text: '文本'};
    const sourceLabel = SOURCE_LABELS[reference.source_type] || reference.source_type || '未知来源';
    const mediaLabel = MEDIA_LABELS[reference.media_type] || reference.media_type || '';
    const rawDetail = `source_type=${reference.source_type || ''} media_type=${reference.media_type || ''}`;
    const detail = joinUiMeta([
      sourceLabel,
      mediaLabel,
      reference.file_size != null ? formatBytes(reference.file_size) : '',
    ]);
    return `
      <div class="character-reference-card">
        <div class="character-reference-thumb">
          ${previewUrl ? `<img src="${escHtml(previewUrl)}" alt="" loading="lazy" onerror="this.closest('.character-reference-thumb').classList.add('failed')">` : '<span>无预览</span>'}
        </div>
        <div class="character-reference-detail">
          <div class="character-reference-head">
            <div title="${escHtml(rawDetail)}">
              <b>${escHtml(reference.character_name || '')}</b>
              <span>${escHtml(detail)}</span>
            </div>
            <button class="btn btn-danger character-reference-delete" type="button" data-character-reference-delete="${reference.id}" data-character-id="${reference.character_id}">${buttonIcon('trash')}删除</button>
          </div>
          <div class="character-reference-path">
            <code title="${escHtml(reference.real_file_path || reference.file_path || '')}">${escHtml(pathText)}</code>
          </div>
        </div>
      </div>
    `;
  }).join('') : '<div class="character-library-empty">请选择角色后查看参考图</div>';

  tagList.innerHTML = jobMarkup + tagButtons;
  characterList.innerHTML = characterButtons;
  referenceList.innerHTML = referenceCards;

  // P4: id and creation time live with the selected character's details, not
  // repeated on every card in the list.
  const selectedMeta = $('#characterLibrarySelectedMeta');
  if (selectedMeta) {
    selectedMeta.textContent = selectedCharacter
      ? joinUiMeta([`#${selectedCharacter.id}`, selectedCharacter.created_at ? `创建于 ${formatHealthTime(selectedCharacter.created_at)}` : ''])
      : '';
  }

  const importScopeSelect = $('#characterImportScopeSelect');
  if (importScopeSelect) {
    importScopeSelect.disabled = importCurrentDisabled && importAllDisabled;
    importScopeSelect.title = currentArtistId ? `当前画师：${state.currentArtist.name}` : '先选择画师';
  }
  const importBtn = $('#characterImportBtn');
  if (importBtn) {
    const scope = importScopeSelect && importScopeSelect.value === 'all' ? 'all' : 'artist';
    importBtn.disabled = scope === 'artist' ? importCurrentDisabled : importAllDisabled;
    importBtn.title = scope === 'artist'
      ? (currentArtistId ? `导入当前画师 ${state.currentArtist.name}` : '先选择画师')
      : '导入全库中符合条件的标签';
  }
  const rebuildBtn = $('#characterRebuildIndexBtn');
  if (rebuildBtn) {
    rebuildBtn.disabled = rebuildDisabled;
    rebuildBtn.title = '重建角色识别用的参考索引';
  }
  applyCharacterLibraryMobileView();
}

function rememberCharacterImportJob(job) {
  state.characterImportJob = job && job.status ? job : null;
  renderCharacterLibrary();
}

function stopCharacterImportPolling() {
  if (!state.characterImportJobTimer) return;
  clearInterval(state.characterImportJobTimer);
  state.characterImportJobTimer = null;
}

function startCharacterImportPolling() {
  stopCharacterImportPolling();
  state.characterImportJobTimer = setInterval(pollCharacterImportJob, CHARACTER_IMPORT_POLL_MS);
}

async function pollCharacterImportJob() {
  try {
    const job = await API.get('/api/characters/import-from-tags/jobs/current');
    state.characterImportPollFailures = 0;
    rememberCharacterImportJob(job);
    if (!job || !['pending', 'running'].includes(job.status)) {
      stopCharacterImportPolling();
      await finishCharacterImportJob(job);
    }
  } catch (e) {
    if (isAbortError(e)) return;
    // One transient poll failure must not freeze the progress bar while the
    // import keeps running server-side; give up only after several in a row.
    state.characterImportPollFailures = (state.characterImportPollFailures || 0) + 1;
    if (state.characterImportPollFailures >= 5) {
      stopCharacterImportPolling();
      toast('读取角色库导入进度失败：' + (e.message || e), 'error');
    }
  }
}

async function finishCharacterImportJob(result) {
  if (!result || result.status === 'idle') return;
  if (result.job_id && state.characterImportFinishedJobId === result.job_id) return;
  if (result.job_id) state.characterImportFinishedJobId = result.job_id;
  if (Number(result.added || result.added_references || 0) === 0 && Number(result.failed || 0) > 0) {
    toast(characterImportFailureText(result), 'error');
  } else if (result.status === 'cancelled') {
    toast('角色库导入已取消', 'error');
  } else {
    toast(`已导入 ${result.added || result.added_references || 0} 张参考图`, result.status === 'failed' ? 'error' : 'success');
  }
  const importedCharacterId = characterIdFromImportResult(result);
  await loadCharacterLibrary({characterId: importedCharacterId || state.characterLibrarySelectedCharacterId});
}

export async function importCharacterLibraryReferences(payload) {
  const busyScope = payload.scope || '';
  if (isActionBusy('character-library-import', busyScope)) return;
  if (characterImportJobBusy()) return;
  setActionBusy('character-library-import', busyScope, true);
  try {
    const result = await API.postJson('/api/characters/import-from-tags/jobs', payload.body || {});
    rememberCharacterImportJob(result);
    if (result.busy) {
      toast('已有角色库导入任务在运行', 'error');
    } else {
      toast('角色库导入已开始', 'success');
    }
    startCharacterImportPolling();
  } catch (e) {
    toast('导入角色库失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-import', busyScope, false);
  }
}

export async function cancelCharacterImportJob(jobId) {
  if (!jobId || isActionBusy('character-library-import-cancel', jobId)) return;
  setActionBusy('character-library-import-cancel', jobId, true);
  try {
    const result = await API.post(`/api/characters/import-from-tags/jobs/${jobId}/cancel`);
    rememberCharacterImportJob(result);
  } catch (e) {
    toast('取消角色库导入失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-import-cancel', jobId, false);
  }
}

export async function deleteCharacterReference(characterId, referenceId) {
  if (!characterId || !referenceId) return;
  if (isActionBusy('character-library-delete', `${characterId}:${referenceId}`)) return;
  if (!confirm('确定要移除此参考图片吗？')) return;
  setActionBusy('character-library-delete', `${characterId}:${referenceId}`, true);
  try {
    await API.del(`/api/characters/${characterId}/references/${referenceId}`);
    toast('参考图已删除', 'success');
    await loadCharacterLibrary({characterId});
  } catch (e) {
    toast('删除参考图失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-delete', `${characterId}:${referenceId}`, false);
  }
}

export async function deleteCharacter(characterId) {
  if (!characterId) return;
  if (isActionBusy('character-library-character-delete', characterId)) return;
  if (!confirm('确定删除该角色及其全部参考图记录？不会删除原图片。再次导入，或启用后台导入后，可能重新建立。')) return;
  setActionBusy('character-library-character-delete', characterId, true);
  try {
    await API.del(`/api/characters/${characterId}`);
    toast('角色已删除', 'success');
    const nextCharacterId = Number(state.characterLibrarySelectedCharacterId) === Number(characterId) ? null : state.characterLibrarySelectedCharacterId;
    await loadCharacterLibrary({characterId: nextCharacterId});
    // The deleted character left the reference view: go back to the list so the
    // mobile view never keeps an empty zone with no selection.
    if (nextCharacterId == null && characterLibraryIsMobileView()) {
      setCharacterLibraryMobileView('characters');
    }
  } catch (e) {
    toast('删除角色失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-character-delete', characterId, false);
  }
}

export async function rebuildCharacterIndex() {
  if (isActionBusy('character-library-rebuild')) return;
  setActionBusy('character-library-rebuild', '', true);
  try {
    const result = await API.post('/api/admin/rebuild-character-index', undefined, {timeoutMs: 600000});
    const text = result.ok ? `角色参考已刷新：${result.vector_count || 0} 条` : (result.reason || '刷新失败');
    toast(text, result.ok ? 'success' : 'error');
    await loadCharacterLibrary({characterId: state.characterLibrarySelectedCharacterId});
  } catch (e) {
    toast('刷新角色参考失败：' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-rebuild', '', false);
  }
}
