function normalizeSelectionIds(ids) {
  const validIds = new Set((state.allItems || []).filter(isTaggableItem).map(item => Number(item.id)));
  return new Set([...ids].map(Number).filter(id => validIds.has(id)));
}

function selectionSetsEqual(a, b) {
  if (a.size !== b.size) return false;
  for (const id of a) {
    if (!b.has(id)) return false;
  }
  return true;
}

function applySelectionChange(ids, options = {}) {
  const gridScrollAnchor = captureGridScrollAnchor();
  const restoreSeq = nextRequestSeq('selectionRestoreSeq');
  const nextIds = normalizeSelectionIds(ids instanceof Set ? ids : new Set(ids || []));
  const changed = !selectionSetsEqual(state.selectedIds, nextIds);
  if (changed) {
    state.selectedIds = nextIds;
    resetCharacterTagSuggestions();
    resetArtistSuggestions();
    resetEditDeleteSelectedButton();
  }
  updateEditBar();
  syncSelectedCards();
  restoreGridScrollAnchor(gridScrollAnchor);
  requestAnimationFrame(() => {
    if (!isCurrentRequestSeq('selectionRestoreSeq', restoreSeq)) return;
    restoreGridScrollAnchor(gridScrollAnchor);
  });
  if (options.log !== false && (changed || options.forceLog)) {
    logUiAction(options.event || 'selection_change', collectSelectionLayoutLogContext({
      reason: options.reason || 'selection',
      selected_count: state.selectedIds.size,
      boxed_count: options.boxed_count ?? null,
      modifier: Boolean(options.modifier),
    }));
  }
  if (changed && options.schedule !== false) {
    scheduleCharacterTagSuggestions({reason: 'selection'});
    scheduleArtistSuggestions({reason: 'selection'});
  }
  return changed;
}

function toggleSelect(id, options = {}) {
  const nextIds = new Set(state.selectedIds);
  if (state.selectedIds.has(id)) {
    nextIds.delete(id);
  } else {
    nextIds.add(id);
  }
  const changed = applySelectionChange(nextIds, {
    reason: options.reason || 'toggle',
    event: 'item_select',
    log: false,
  });
  logUiAction('item_select', collectSelectionLayoutLogContext({
    reason: options.reason || 'toggle',
    id,
    selected: state.selectedIds.has(id),
    selected_count: state.selectedIds.size,
  }));
  if (!changed && options.schedule) {
    scheduleCharacterTagSuggestions({reason: 'selection'});
    scheduleArtistSuggestions({reason: 'selection'});
  }
}

function selectOnly(id, options = {}) {
  applySelectionChange([id], {
    reason: options.reason || 'click',
    event: 'item_select',
    log: false,
  });
  logUiAction('item_select', collectSelectionLayoutLogContext({
    reason: options.reason || 'click',
    id,
    selected: state.selectedIds.has(id),
    selected_count: state.selectedIds.size,
  }));
}

function selectedMediaItemsForDelete() {
  const ids = new Set([...state.selectedIds].map(Number));
  return (state.allItems || []).filter(item => ids.has(Number(item.id)) && item.file_path);
}

function resetEditDeleteSelectedButton(btn = $('#editDeleteSelectedBtn')) {
  if (!btn) return;
  delete btn.dataset.confirmStep;
  btn.disabled = false;
  btn.classList.remove('confirm1', 'deleting');
  btn.textContent = '删除所选';
}

async function deleteSelectedMediaItems() {
  const btn = $('#editDeleteSelectedBtn');
  if (isActionBusy('edit-delete-selected')) return;
  const ids = [...state.selectedIds];
  const items = selectedMediaItemsForDelete();
  if (items.length === 0) {
    toast('请先选择要删除的媒体', 'error');
    return;
  }
  if (btn && btn.dataset.confirmStep !== '1') {
    btn.dataset.confirmStep = '1';
    btn.classList.add('confirm1');
    btn.textContent = `确认删除 ${items.length} 项`;
    return;
  }

  if (!confirm(`确认删除 ${items.length} 个文件？文件将移到飞牛回收站，可在回收站恢复。`)) {
    resetEditDeleteSelectedButton(btn);
    return;
  }

  setActionBusy('edit-delete-selected', '', true);
  if (btn) {
    btn.disabled = true;
    btn.classList.add('deleting');
    btn.textContent = '删除中';
  }
  const deletedItemIds = [];
  const failedItemIds = [];
  try {
    for (const item of items) {
      const itemId = Number(item.id);
      const ok = await deleteMediaItem({
        filePath: item.file_path,
        itemId,
        fileName: item.file_name || '',
        skipConfirm: true,
        renderAfter: false,
        toastOnSuccess: false,
      });
      if (ok) {
        deletedItemIds.push(itemId);
      } else {
        failedItemIds.push(itemId);
      }
    }
    if (deletedItemIds.length) renderGrid();
    applySelectionChange([], {reason: 'delete_selected', log: false});
    logUiAction('edit_delete_selected_result', {
      item_ids: ids,
      deleted_item_ids: deletedItemIds,
      failed_item_ids: failedItemIds,
      deleted_count: deletedItemIds.length,
      failed_count: failedItemIds.length,
    });
    if (failedItemIds.length) {
      toast(`已移到回收站 ${deletedItemIds.length} 项，${failedItemIds.length} 项失败`, 'error');
    } else {
      toast(`已移到回收站 ${deletedItemIds.length} 项`, 'success');
    }
  } finally {
    resetEditDeleteSelectedButton(btn);
    setActionBusy('edit-delete-selected', '', false);
  }
}

function updateEditBar() {
  const bar = $('#editBar');
  if (!bar) return;
  if (state.mode === 'edit') {
    bar.classList.add('visible');
    bar.classList.toggle('is-empty-selection', state.selectedIds.size === 0);
    if (state.selectedIds.size > 0) {
      $('#selectedCount').textContent = `已选 ${state.selectedIds.size} 项`;
    } else if (state.activeFolder) {
      $('#selectedCount').textContent = `当前文件夹：${state.activeFolder}`;
    } else {
      $('#selectedCount').textContent = '已选 0 项';
    }
  } else {
    bar.classList.remove('visible');
    bar.classList.remove('is-empty-selection');
  }
  if (state.mode !== 'edit' || state.selectedIds.size === 0) resetEditDeleteSelectedButton();
  renderEditTagPicker();
  renderCharacterTagSuggestions();
  renderArtistSuggestions();
  ensureEditTagContext();
}

function tagMatchesEditQuery(tag, query) {
  return searchableTextMatches(query, tag.name, tag.search_text);
}

function tagNameKey(name) {
  return (name || '').trim().toLowerCase();
}

function numericTagId(value) {
  if (value == null || value === '') return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function tagRecordsForTag(tag) {
  const sourceRecords = tag && Array.isArray(tag.tag_records) && tag.tag_records.length ? tag.tag_records : [tag];
  const records = [];
  sourceRecords.forEach(record => {
    const tagId = numericTagId(record?.tag_id ?? record?.id);
    const artistId = numericTagId(record?.artist_id);
    const name = (record?.name || tag?.name || '').trim();
    if (tagId == null || artistId == null || !name) return;
    records.push({tag_id: tagId, artist_id: artistId, name});
  });
  return records;
}

function mergeTagRecords(existingRecords, nextRecords) {
  const byKey = new Map();
  [...(existingRecords || []), ...(nextRecords || [])].forEach(record => {
    const tagId = numericTagId(record?.tag_id);
    const artistId = numericTagId(record?.artist_id);
    const name = (record?.name || '').trim();
    if (tagId == null || artistId == null || !name) return;
    byKey.set(`${artistId}:${tagId}`, {tag_id: tagId, artist_id: artistId, name});
  });
  return [...byKey.values()];
}

function selectedEditArtistIds() {
  const artistIds = new Set();
  (state.allItems || []).forEach(item => {
    if (state.selectedIds.has(item.id) && isTaggableItem(item)) {
      artistIds.add(Number(item.artist_id));
    }
  });
  if (artistIds.size === 0 && state.currentArtist) {
    artistIds.add(Number(state.currentArtist.id));
  }
  return [...artistIds].filter(Boolean).sort((a, b) => a - b);
}

function currentEditArtistId() {
  const ids = selectedEditArtistIds();
  if (ids.length === 1) return ids[0];
  return state.editContextArtistId || (state.currentArtist ? state.currentArtist.id : null);
}

function mergeTagsByName(tagGroups) {
  const byName = new Map();
  tagGroups.flat().forEach(tag => {
    const key = tagNameKey(tag.name);
    if (!key) return;
    const countKey = tag.id != null ? `tag:${tag.id}` : `name:${key}`;
    const countedTagIds = tag.countedTagIds instanceof Set ? tag.countedTagIds : new Set([countKey]);
    const tagRecords = tagRecordsForTag(tag);
    const artistIds = Array.isArray(tag.artist_ids)
      ? tag.artist_ids
      : [tag.artist_id, ...tagRecords.map(record => record.artist_id)];
    const existing = byName.get(key);
    if (existing) {
      if (!existing.countedTagIds.has(countKey)) {
        const alreadyCounted = [...countedTagIds].some(id => existing.countedTagIds.has(id));
        if (!alreadyCounted) existing.item_count += tag.item_count || 0;
        existing.countedTagIds.add(countKey);
        countedTagIds.forEach(id => existing.countedTagIds.add(id));
      }
      artistIds.filter(id => id != null).forEach(id => {
        const artistId = Number(id);
        if (!existing.artist_ids.includes(artistId)) existing.artist_ids.push(artistId);
      });
      existing.tag_records = mergeTagRecords(existing.tag_records, tagRecords);
      existing.global = Boolean(existing.global || tag.global);
    } else {
      byName.set(key, {
        ...tag,
        name: tag.name,
        item_count: tag.item_count || 0,
        artist_ids: artistIds.filter(id => id != null).map(id => Number(id)),
        countedTagIds: new Set(countedTagIds),
        tag_records: tagRecords,
      });
    }
  });
  return [...byName.values()].sort((a, b) => compareNameParts(a.name || '', b.name || ''));
}

function editAvailableTags() {
  return mergeTagsByName([
    state.tags || [],
    state.editGlobalTagResults || [],
  ]);
}

async function loadGlobalEditTagResults(query = '') {
  const clean = (query || '').trim();
  const token = ++editGlobalTagSearchToken;

  state.editGlobalTagSearchLoading = true;
  logUiAction('edit_tag_search', {
    search: clean,
    scope: state.searchScope,
    selected_count: state.selectedIds.size,
    artist_ids: selectedEditArtistIds(),
  });
  renderEditTagPicker();
  try {
    const params = new URLSearchParams({limit: 100});
    if (clean) params.set('search', clean);
    const data = await API.get('/api/tags/search?' + params.toString());
    if (token !== editGlobalTagSearchToken) return;
    state.editGlobalTagResults = (data.tags || []).map(tag => ({...tag, global: true}));
  } catch (e) {
    if (token === editGlobalTagSearchToken) state.editGlobalTagResults = [];
  } finally {
    if (token === editGlobalTagSearchToken) {
      state.editGlobalTagSearchLoading = false;
      renderEditTagPicker();
    }
  }
}

function editTagOptions(query = '') {
  return editAvailableTags().filter(tag => tagMatchesEditQuery(tag, query));
}

function isCharacterSuggestionCandidate(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  return Boolean((mediaType === 'image' || mediaType === 'video') && !item.missing && !item.is_archive && item.file_path);
}

function characterSuggestionItemKey(item) {
  const version = [item.file_size || 0, Math.round(Number(item.file_mtime || item.mtime || 0))].join(':');
  return `${item.id}:${version}`;
}

function characterSuggestionCandidateSelection() {
  const loadedItems = state.allItems || [];
  if (state.selectedIds.size > 0) {
    const selectedCandidates = loadedItems
      .filter(item => state.selectedIds.has(item.id))
      .filter(isCharacterSuggestionCandidate);
    return {
      items: selectedCandidates.slice(0, CHARACTER_SUGGESTION_SELECTED_LIMIT),
      total: selectedCandidates.length,
      limit: CHARACTER_SUGGESTION_SELECTED_LIMIT,
    };
  }
  const currentCandidates = loadedItems.filter(isCharacterSuggestionCandidate);
  return {
    items: currentCandidates.slice(0, CHARACTER_SUGGESTION_UNSELECTED_LIMIT),
    total: currentCandidates.length,
    limit: CHARACTER_SUGGESTION_UNSELECTED_LIMIT,
  };
}

function characterSuggestionCandidates() {
  return characterSuggestionCandidateSelection().items;
}

function characterSuggestionPageKey() {
  const selection = characterSuggestionCandidateSelection();
  return [
    selection.total,
    selection.limit,
    ...selection.items.map(characterSuggestionItemKey),
  ].join('|');
}

function resetCharacterTagSuggestions({clearCache = false} = {}) {
  if (state.characterSuggestionScheduleFrame != null) {
    cancelAnimationFrame(state.characterSuggestionScheduleFrame);
    state.characterSuggestionScheduleFrame = null;
  }
  if (state.characterSuggestionScheduleTimer != null) {
    clearTimeout(state.characterSuggestionScheduleTimer);
    state.characterSuggestionScheduleTimer = null;
  }
  state.characterTagSuggestions = [];
  state.characterSuggestionLoading = false;
  state.characterSuggestionStatus = 'idle';
  state.characterSuggestionMessage = '';
  state.characterSuggestionSampleTotal = 0;
  state.characterSuggestionSampleLimit = 0;
  state.characterSuggestionPageKey = '';
  nextRequestSeq('characterSuggestionSeq');
  nextRequestSeq('characterSuggestionScheduleSeq');
  if (clearCache) state.characterSuggestionCache.clear();
  renderCharacterTagSuggestions();
}

function characterSuggestionSampleText() {
  const total = Number(state.characterSuggestionSampleTotal || 0);
  const limit = Number(state.characterSuggestionSampleLimit || 0);
  if (total > limit && limit > 0) return `已抽样 ${limit}/${total} 张`;
  return '';
}

function characterSuggestionStatusText() {
  const sampleText = characterSuggestionSampleText();
  if (state.mode !== 'edit') return '未识别';
  if (state.characterSuggestionLoading) return sampleText ? `识别中${UI_FIELD_SEPARATOR}${sampleText}` : '识别中';
  if (state.characterSuggestionStatus === 'unavailable') return '识别不可用';
  if (state.characterSuggestionStatus === 'empty') return '无可识别媒体';
  if (state.characterSuggestionStatus === 'none') return sampleText ? `无建议${UI_FIELD_SEPARATOR}${sampleText}` : '无建议';
  if (state.characterSuggestionMessage) return state.characterSuggestionMessage;
  if (state.characterTagSuggestions.length) {
    const base = `建议 ${state.characterTagSuggestions.length} 个`;
    return sampleText ? `${base}${UI_FIELD_SEPARATOR}${sampleText}` : base;
  }
  return '未识别';
}

function renderCharacterTagSuggestions() {
  const panel = $('#characterSuggestions');
  if (!panel) return;
  const status = $('#characterSuggestionsStatus');
  const list = $('#characterSuggestionsList');
  if (status) status.textContent = characterSuggestionStatusText();
  if (!list) return;
  const suggestions = state.characterTagSuggestions || [];
  if (!suggestions.length) {
    list.innerHTML = '';
    return;
  }
  const selectedNameKeys = selectedEditTagNameKeys();
  list.innerHTML = suggestions.map(suggestion => {
    const name = suggestion.name || '';
    const weak = suggestion.status === 'needs_review' ? ' weak' : '';
    const selected = selectedNameKeys.has(tagNameKey(name));
    const meta = suggestion.count > 1 ? `${suggestion.count} 张` : `${Math.round((suggestion.confidence || 0) * 100)}%`;
    return `
      <button class="btn character-suggestion-chip${weak}${selected ? ' selected' : ''}" type="button" data-character-suggestion-tag="${escHtml(name)}" aria-pressed="${selected ? 'true' : 'false'}">
        <span>${escHtml(name)}</span>
        <em>${escHtml(meta)}</em>
      </button>
    `;
  }).join('');
}

function selectCharacterSuggestionTag(tagName) {
  const selected = addVirtualEditTag(tagName);
  if (!selected) return;
  logUiAction('edit_tag_suggestion_select', collectSelectionLayoutLogContext({
    name: tagName,
    selected_count: state.selectedIds.size,
    artist_ids: selectedEditArtistIds(),
  }));
  renderCharacterTagSuggestions();
}

function characterSuggestionName(result) {
  const direct = result?.character?.name || result?.character_name || '';
  if (direct) return direct;
  const first = Array.isArray(result?.predictions) ? result.predictions[0] : null;
  return first?.character_name || first?.name || '';
}

function characterSuggestionConfidence(result) {
  const prediction = Array.isArray(result?.predictions) ? result.predictions[0] : null;
  const value = result?.confidence ?? result?.score ?? prediction?.score ?? 0;
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}

function aggregateCharacterTagSuggestions(results) {
  const byName = new Map();
  results.forEach(result => {
    const status = result?.status || '';
    if (status !== 'accepted' && status !== 'needs_review') return;
    const name = characterSuggestionName(result).trim();
    const key = tagNameKey(name);
    if (!key) return;
    const confidence = characterSuggestionConfidence(result);
    const existing = byName.get(key);
    if (existing) {
      existing.count += 1;
      existing.confidence = Math.max(existing.confidence, confidence);
      if (existing.status !== 'accepted' && status === 'accepted') existing.status = status;
      return;
    }
    byName.set(key, {name, status, confidence, count: 1});
  });
  return [...byName.values()].sort((a, b) => {
    if (a.status !== b.status) return a.status === 'accepted' ? -1 : 1;
    if (b.count !== a.count) return b.count - a.count;
    if (b.confidence !== a.confidence) return b.confidence - a.confidence;
    return compareCharacterNames(a.name || '', b.name || '');
  });
}

function characterSuggestionRuntimeSummary(results) {
  const runtime = (results || []).find(result => result && result.runtime)?.runtime || {};
  return {
    provider: runtime.provider || '',
    requested_provider: runtime.requested_provider || '',
    active_device: runtime.active_device || '',
    fallback_reason: runtime.fallback_reason || '',
    indexed_references: runtime.indexed_references || 0,
    duration_ms: runtime.duration_ms || 0,
  };
}

function characterSuggestionErrorSummary(error) {
  const detail = error && error.detail && typeof error.detail === 'object' ? error.detail : {};
  return {
    provider: detail.provider || '',
    requested_provider: detail.requested_provider || '',
    active_device: detail.active_device || '',
    fallback_reason: detail.fallback_reason || '',
    available_providers: detail.available_providers || [],
    render_device: detail.render_device || '',
    model_variant: detail.model_variant || '',
    reason: detail.reason || '',
    message: detail.message || '',
    indexed_references: detail.indexed_references || 0,
  };
}

function isCurrentCharacterSuggestionRequest(seq, pageKey) {
  return isCurrentRequestSeq('characterSuggestionSeq', seq)
    && state.characterSuggestionPageKey === pageKey
    && characterSuggestionPageKey() === pageKey;
}

async function recognizeCharacterSuggestionItem(item) {
  const key = characterSuggestionItemKey(item);
  if (state.characterSuggestionCache.has(key)) {
    return state.characterSuggestionCache.get(key);
  }
  const pending = API.postJson(`/api/items/${item.id}/character-recognition?top_k=3`, {});
  state.characterSuggestionCache.set(key, pending);
  try {
    const result = await pending;
    state.characterSuggestionCache.set(key, result);
    return result;
  } catch (e) {
    state.characterSuggestionCache.delete(key);
    throw e;
  }
}

async function loadCharacterTagSuggestions() {
  const seq = nextRequestSeq('characterSuggestionSeq');
  const selection = characterSuggestionCandidateSelection();
  const candidates = selection.items;
  const pageKey = characterSuggestionPageKey();
  state.characterSuggestionPageKey = pageKey;
  state.characterTagSuggestions = [];
  state.characterSuggestionMessage = '';
  state.characterSuggestionSampleTotal = selection.total;
  state.characterSuggestionSampleLimit = selection.limit;
  if (!candidates.length) {
    state.characterSuggestionStatus = 'empty';
    state.characterSuggestionLoading = false;
    renderCharacterTagSuggestions();
    return;
  }

  state.characterSuggestionStatus = 'loading';
  state.characterSuggestionLoading = true;
  logUiAction('edit_character_suggestions_start', {
    candidate_count: candidates.length,
    sample_total: selection.total,
    sample_limit: selection.limit,
    selected_count: state.selectedIds.size,
    artist_id: state.currentArtist ? state.currentArtist.id : null,
  });
  renderCharacterTagSuggestions();
  const results = [];
  let failedCount = 0;
  try {
    for (const item of candidates) {
      if (!isCurrentCharacterSuggestionRequest(seq, pageKey)) return;
      try {
        const result = await recognizeCharacterSuggestionItem(item);
        if (!isCurrentCharacterSuggestionRequest(seq, pageKey)) return;
        results.push(result);
      } catch (e) {
        if (!isCurrentCharacterSuggestionRequest(seq, pageKey)) return;
        failedCount += 1;
        logUiAction('edit_character_suggestions_failed', {
          candidate_count: candidates.length,
          sample_total: selection.total,
          sample_limit: selection.limit,
          success_count: results.length,
          failed_count: failedCount,
          suggestion_count: 0,
          item_id: item.id,
          file_name: item.file_name || '',
          artist_id: item.artist_id || null,
          error: e.message || String(e),
          ...(results.length ? characterSuggestionRuntimeSummary(results) : characterSuggestionErrorSummary(e)),
        });
        if (!results.length) {
          state.characterSuggestionStatus = 'unavailable';
          state.characterSuggestionMessage = '识别不可用';
          return;
        }
        break;
      }
    }
    if (!isCurrentCharacterSuggestionRequest(seq, pageKey)) return;
    state.characterTagSuggestions = aggregateCharacterTagSuggestions(results);
    state.characterSuggestionStatus = state.characterTagSuggestions.length ? 'ready' : 'none';
    state.characterSuggestionMessage = '';
    logUiAction('edit_character_suggestions_done', {
      candidate_count: candidates.length,
      sample_total: selection.total,
      sample_limit: selection.limit,
      success_count: results.length,
      failed_count: failedCount,
      suggestion_count: state.characterTagSuggestions.length,
      ...characterSuggestionRuntimeSummary(results),
    });
  } finally {
    if (isCurrentCharacterSuggestionRequest(seq, pageKey)) {
      state.characterSuggestionLoading = false;
      renderCharacterTagSuggestions();
    }
  }
}

function ensureCharacterTagSuggestions(options = {}) {
  if (state.mode !== 'edit') return;
  const pageKey = characterSuggestionPageKey();
  if (pageKey === state.characterSuggestionPageKey && (state.characterSuggestionLoading || state.characterSuggestionStatus !== 'idle')) {
    renderCharacterTagSuggestions();
    return;
  }
  loadCharacterTagSuggestions();
}

function scheduleCharacterTagSuggestions(options = {}) {
  if (state.mode !== 'edit') return;
  const pageKey = characterSuggestionPageKey();
  const scheduleSeq = nextRequestSeq('characterSuggestionScheduleSeq');
  if (state.characterSuggestionScheduleFrame != null) {
    cancelAnimationFrame(state.characterSuggestionScheduleFrame);
    state.characterSuggestionScheduleFrame = null;
  }
  if (state.characterSuggestionScheduleTimer != null) {
    clearTimeout(state.characterSuggestionScheduleTimer);
    state.characterSuggestionScheduleTimer = null;
  }
  state.characterSuggestionScheduleFrame = requestAnimationFrame(() => {
    state.characterSuggestionScheduleFrame = null;
    state.characterSuggestionScheduleTimer = setTimeout(() => {
      state.characterSuggestionScheduleTimer = null;
      if (!isCurrentRequestSeq('characterSuggestionScheduleSeq', scheduleSeq)) return;
      if (state.mode !== 'edit') return;
      if (pageKey !== characterSuggestionPageKey()) return;
      ensureCharacterTagSuggestions();
    }, CHARACTER_SUGGESTION_DELAY_MS);
  });
}

function artistSuggestionTargetItem() {
  if (state.selectedIds.size !== 1) return null;
  const itemId = Number([...state.selectedIds][0]);
  return (state.allItems || []).find(item => Number(item.id) === itemId && !item.missing) || null;
}

function artistSuggestionItemKey(item) {
  const version = [item.file_size || 0, Math.round(Number(item.file_mtime || item.mtime || 0))].join(':');
  return item ? `${item.id}:${version}` : '';
}

function artistSuggestionPageKey() {
  const item = artistSuggestionTargetItem();
  return item ? artistSuggestionItemKey(item) : `selection:${state.selectedIds.size}`;
}

function resetArtistSuggestions() {
  if (state.artistSuggestionScheduleFrame != null) {
    cancelAnimationFrame(state.artistSuggestionScheduleFrame);
    state.artistSuggestionScheduleFrame = null;
  }
  if (state.artistSuggestionScheduleTimer != null) {
    clearTimeout(state.artistSuggestionScheduleTimer);
    state.artistSuggestionScheduleTimer = null;
  }
  state.artistSuggestions = [];
  state.artistSuggestionLoading = false;
  state.artistSuggestionStatus = 'idle';
  state.artistSuggestionMessage = '';
  state.artistSuggestionPageKey = '';
  nextRequestSeq('artistSuggestionSeq');
  nextRequestSeq('artistSuggestionScheduleSeq');
  renderArtistSuggestions();
}

function artistSuggestionStatusText() {
  if (state.mode !== 'edit') return '未检查';
  if (state.artistSuggestionLoading) return '检查中';
  if (state.selectedIds.size !== 1) return '选择单项';
  if (state.artistSuggestionStatus === 'unavailable') return '建议不可用';
  if (state.artistSuggestionStatus === 'none') return '无建议';
  if (state.artistSuggestionMessage) return state.artistSuggestionMessage;
  if (state.artistSuggestions.length) return `建议 ${state.artistSuggestions.length} 个`;
  return '未检查';
}

function renderArtistSuggestions() {
  const panel = $('#artistSuggestions');
  if (!panel) return;
  panel.hidden = !ARTIST_SUGGESTIONS_VISIBLE;
  panel.setAttribute('aria-hidden', String(!ARTIST_SUGGESTIONS_VISIBLE));
  const status = $('#artistSuggestionsStatus');
  const list = $('#artistSuggestionsList');
  if (!ARTIST_SUGGESTIONS_VISIBLE) {
    if (status) status.textContent = '';
    if (list) list.innerHTML = '';
    return;
  }
  if (status) status.textContent = artistSuggestionStatusText();
  if (!list) return;
  const suggestions = state.artistSuggestions || [];
  if (!suggestions.length) {
    list.innerHTML = '';
    return;
  }
  list.innerHTML = suggestions.map(suggestion => {
    const artistId = Number(suggestion.artist_id);
    const itemId = Number(suggestion.item_id);
    const name = suggestion.artist_name || '';
    const confirmed = suggestion.status === 'confirmed';
    const meta = confirmed ? '已确认' : artistSuggestionMetaLabel(suggestion);
    const bandClass = artistSuggestionBandClass(suggestion);
    return `
      <button class="btn artist-suggestion-chip${bandClass ? ` ${bandClass}` : ''}${confirmed ? ' confirmed' : ''}" type="button" data-artist-suggestion-id="${artistId}" data-artist-suggestion-item="${itemId}" aria-pressed="${confirmed ? 'true' : 'false'}">
        <span>${escHtml(name)}</span>
        <em>${escHtml(meta)}</em>
      </button>
    `;
  }).join('');
}

function normalizeArtistSuggestions(result, itemId) {
  const rows = Array.isArray(result?.suggestions) ? result.suggestions : (result?.metadata_candidates || []);
  return rows.map(row => ({
    item_id: result?.item_id || itemId,
    artist_id: row.artist_id,
    artist_name: row.artist_name || '',
    reason: row.reason || '',
    status: row.status || 'suggested',
    calibrated_band: row.calibrated_band || '',
    calibrated_score: row.calibrated_score,
    fused_score: row.fused_score,
    matched_ref_item_id: row.matched_ref_item_id,
  })).filter(row => row.artist_id != null && row.artist_name);
}

async function loadArtistSuggestions() {
  if (!ARTIST_SUGGESTIONS_VISIBLE) return;
  const seq = nextRequestSeq('artistSuggestionSeq');
  const item = artistSuggestionTargetItem();
  const pageKey = artistSuggestionPageKey();
  state.artistSuggestionPageKey = pageKey;
  state.artistSuggestions = [];
  state.artistSuggestionMessage = '';
  if (!item) {
    state.artistSuggestionStatus = 'idle';
    state.artistSuggestionLoading = false;
    renderArtistSuggestions();
    return;
  }

  state.artistSuggestionStatus = 'loading';
  state.artistSuggestionLoading = true;
  logUiAction('edit_artist_suggestions_start', {
    item_id: item.id,
    selected_count: state.selectedIds.size,
  });
  renderArtistSuggestions();
  try {
    const result = await API.post(`/api/items/${item.id}/artist-suggestions?limit=3`);
    if (!isCurrentRequestSeq('artistSuggestionSeq', seq) || state.artistSuggestionPageKey !== pageKey) return;
    state.artistSuggestions = normalizeArtistSuggestions(result, item.id);
    state.artistSuggestionStatus = state.artistSuggestions.length ? 'ready' : 'none';
    logUiAction('edit_artist_suggestions_done', {
      item_id: item.id,
      suggestion_count: state.artistSuggestions.length,
    });
  } catch (e) {
    if (!isCurrentRequestSeq('artistSuggestionSeq', seq) || state.artistSuggestionPageKey !== pageKey) return;
    state.artistSuggestionStatus = 'unavailable';
    state.artistSuggestionMessage = '建议不可用';
    logUiAction('edit_artist_suggestions_failed', {
      item_id: item.id,
      error: e.message || String(e),
    });
  } finally {
    if (isCurrentRequestSeq('artistSuggestionSeq', seq) && state.artistSuggestionPageKey === pageKey) {
      state.artistSuggestionLoading = false;
      renderArtistSuggestions();
    }
  }
}

function ensureArtistSuggestions() {
  if (!ARTIST_SUGGESTIONS_VISIBLE) return;
  if (state.mode !== 'edit') return;
  const pageKey = artistSuggestionPageKey();
  if (pageKey === state.artistSuggestionPageKey && (state.artistSuggestionLoading || state.artistSuggestionStatus !== 'idle')) {
    renderArtistSuggestions();
    return;
  }
  loadArtistSuggestions();
}

function scheduleArtistSuggestions(options = {}) {
  if (!ARTIST_SUGGESTIONS_VISIBLE) return;
  if (state.mode !== 'edit') return;
  const pageKey = artistSuggestionPageKey();
  const scheduleSeq = nextRequestSeq('artistSuggestionScheduleSeq');
  if (state.artistSuggestionScheduleFrame != null) {
    cancelAnimationFrame(state.artistSuggestionScheduleFrame);
    state.artistSuggestionScheduleFrame = null;
  }
  if (state.artistSuggestionScheduleTimer != null) {
    clearTimeout(state.artistSuggestionScheduleTimer);
    state.artistSuggestionScheduleTimer = null;
  }
  state.artistSuggestionScheduleFrame = requestAnimationFrame(() => {
    state.artistSuggestionScheduleFrame = null;
    state.artistSuggestionScheduleTimer = setTimeout(() => {
      state.artistSuggestionScheduleTimer = null;
      if (!isCurrentRequestSeq('artistSuggestionScheduleSeq', scheduleSeq)) return;
      if (state.mode !== 'edit') return;
      if (pageKey !== artistSuggestionPageKey()) return;
      ensureArtistSuggestions();
    }, ARTIST_SUGGESTION_DELAY_MS);
  });
}

async function confirmArtistSuggestion(itemId, artistId) {
  itemId = Number(itemId);
  artistId = Number(artistId);
  if (!itemId || !artistId) return;
  const busyId = `${itemId}:${artistId}`;
  if (isActionBusy('artist-suggestion-confirm', busyId)) return;
  setActionBusy('artist-suggestion-confirm', busyId, true);
  try {
    const result = await API.post(`/api/items/${itemId}/artist-suggestions/${artistId}/confirm`);
    state.artistSuggestions = (state.artistSuggestions || []).map(suggestion => (
      Number(suggestion.item_id) === itemId && Number(suggestion.artist_id) === artistId
        ? {...suggestion, status: 'confirmed', artist_name: result.artist_name || suggestion.artist_name}
        : suggestion
    ));
    logUiAction('artist_suggestion_confirmed', {
      item_id: itemId,
      artist_id: artistId,
      artist_name: result.artist_name || '',
    });
    renderArtistSuggestions();
    toast('画师建议已确认', 'success');
  } catch (e) {
    toast('确认画师建议失败', 'error');
  } finally {
    setActionBusy('artist-suggestion-confirm', busyId, false);
  }
}

function artistSuggestionReasonLabel(reason) {
  return {
    filename: '文件名',
    folder: '文件夹',
    tag: '标签',
    model: '模型',
  }[reason] || '建议';
}

function artistSuggestionBandLabel(band) {
  return {high: '高置信', medium: '中置信', low: '低置信'}[band] || '';
}

function artistSuggestionBandClass(suggestion) {
  return suggestion && suggestion.calibrated_band ? `band-${suggestion.calibrated_band}` : '';
}

function artistSuggestionMetaLabel(suggestion) {
  const base = artistSuggestionReasonLabel(suggestion.reason);
  const band = artistSuggestionBandLabel(suggestion.calibrated_band);
  const score = Number.isFinite(Number(suggestion.calibrated_score))
    ? Number(suggestion.calibrated_score)
    : Number(suggestion.fused_score);
  if (!band || !Number.isFinite(score)) return base;
  return `${base}${UI_FIELD_SEPARATOR}${band} ${Math.round(score * 100)}%`;
}

async function ensureEditTagContext() {
  const artistIds = selectedEditArtistIds();
  const contextKey = artistIds.join(',');
  state.editContextArtistId = artistIds.length === 1 ? artistIds[0] : null;
  if (!artistIds.length) {
    editTagContextLoadToken++;
    state.editTagContextLoading = false;
    state.editContextKey = '';
    renderEditTagPicker();
    return;
  }
  if (
    state.currentArtist &&
    artistIds.length === 1 &&
    artistIds[0] === Number(state.currentArtist.id) &&
    state.editContextKey === contextKey &&
    (state.tags || []).length
  ) {
    state.editTagContextLoading = false;
    pruneSelectedEditTags();
    renderEditTagPicker();
    return;
  }

  const token = ++editTagContextLoadToken;
  state.editTagContextLoading = true;
  renderEditTagPicker();
  try {
    const groups = await Promise.all(
      artistIds.map(id => API.get(`/api/tags?artist_id=${id}`).catch(() => []))
    );
    if (token !== editTagContextLoadToken) return;
    state.tags = mergeTagsByName(groups);
    state.editContextKey = contextKey;
    pruneSelectedEditTags();
    logUiAction('edit_tag_context_loaded', {
      artist_ids: artistIds,
      tag_count: state.tags.length,
      selected_count: state.selectedIds.size,
    });
  } finally {
    if (token === editTagContextLoadToken) {
      state.editTagContextLoading = false;
      renderEditTagPicker();
    }
  }
}

function pruneSelectedEditTags() {
  const existing = new Set(editAvailableTags().map(tag => Number(tag.id)));
  state.selectedEditTagIds = new Set([...state.selectedEditTagIds].filter(id => existing.has(Number(id))));
  state.selectedEditTagNames = new Set(
    [...state.selectedEditTagNames].filter(name => tagNameKey(name))
  );
}

function exactEditTagMatch(query) {
  const q = (query || '').trim().toLowerCase();
  if (!q) return null;
  return editAvailableTags().find(tag => (tag.name || '').trim().toLowerCase() === q) || null;
}

function selectedEditTagNameKeys() {
  const names = selectedEditTagNames();
  return new Set(names.map(tagNameKey));
}

function selectedEditTagRecords() {
  const selectedTagIds = new Set([...state.selectedEditTagIds].map(numericTagId).filter(id => id != null));
  const records = [];
  const seen = new Set();
  [state.tags || [], state.editGlobalTagResults || []].flat().forEach(tag => {
    if (!tag) return;
    if (tag.tag_records && !Array.isArray(tag.tag_records)) return;
    tagRecordsForTag(tag).forEach(record => {
      if (!selectedTagIds.has(record.tag_id)) return;
      const key = `${record.artist_id}:${record.tag_id}`;
      if (seen.has(key)) return;
      seen.add(key);
      records.push(record);
    });
  });
  return records;
}

function renderEditTagPicker() {
  const panel = $('#editTagPickerPanel');
  if (!panel) return;
  if (state.editTagContextLoading) {
    panel.innerHTML = '<div class="tag-picker-empty">正在读取标签</div>';
    updateEditTagPickerSummary();
    return;
  }
  pruneSelectedEditTags();
  const query = (state.editTagQuery || '').trim();
  const tags = editTagOptions(query);
  const selectedNameKeys = selectedEditTagNameKeys();
  const rows = tags.map(tag => {
    const selected = state.selectedEditTagIds.has(Number(tag.id)) || selectedNameKeys.has(tagNameKey(tag.name));
    const globalAttr = tag.global ? ` data-global-tag-id="${tag.id}"` : '';
    return `
      <button class="btn tag-picker-option${selected ? ' selected' : ''}" type="button" data-tag-id="${tag.id}"${globalAttr} data-tag-name="${escHtml(tag.name)}" aria-pressed="${selected ? 'true' : 'false'}">
        <span>${escHtml(tag.name)}</span>
        <em>${tag.item_count || 0}</em>
      </button>
    `;
  });
  if (state.editGlobalTagSearchLoading) {
    rows.push('<div class="tag-picker-empty">正在搜索全局标签</div>');
  }
  if (query && !state.editGlobalTagSearchLoading && !exactEditTagMatch(query)) {
    rows.push(`
      <button class="btn tag-picker-option tag-picker-create" type="button" data-create-tag="${escHtml(query)}">
        <span>创建标签：${escHtml(query)}</span>
      </button>
    `);
  }
  const emptyText = query
    ? '没有匹配的全局标签，按回车创建'
    : '输入标签名搜索全局标签';
  panel.innerHTML = rows.join('') || `<div class="tag-picker-empty">${emptyText}</div>`;
  $$('#editTagPickerPanel [data-tag-id]').forEach(btn => {
    btn.addEventListener('click', () => toggleEditTagSelection(Number(btn.dataset.tagId), btn.dataset.tagName || ''));
  });
  $$('#editTagPickerPanel [data-create-tag]').forEach(btn => {
    btn.addEventListener('click', () => createOrSelectEditTag(btn.dataset.createTag || ''));
  });
  updateEditTagPickerSummary();
}

function updateEditTagPickerSummary() {
  const count = selectedEditTagNames().length;
  const summary = $('#editTagPickerSummary');
  if (summary) summary.textContent = count ? `已选 ${count} 个标签` : '未选标签';
  const input = $('#editTagSearch');
  if (input) input.placeholder = count ? '继续搜索标签' : '选择或搜索标签';
}

function clearEditTagQuery() {
  state.editTagQuery = '';
  const input = $('#editTagSearch');
  if (input) input.value = '';
}

function openEditTagPicker() {
  $('#editTagPicker').classList.add('open');
  renderEditTagPicker();
  ensureEditTagContext();
  loadGlobalEditTagResults(state.editTagQuery);
}

function closeEditTagPicker() {
  $('#editTagPicker').classList.remove('open');
  clearEditTagQuery();
  renderEditTagPicker();
}

function setSelectedEditTagName(name, selected) {
  const key = tagNameKey(name);
  if (!key) return;
  const retained = [...state.selectedEditTagNames].filter(existing => tagNameKey(existing) !== key);
  if (selected) retained.push(name.trim());
  state.selectedEditTagNames = new Set(retained);
}

function removeSelectedEditTag(tagId, tagName = '') {
  const id = Number(tagId);
  const tag = editAvailableTags().find(t => Number(t.id) === id);
  const name = tagName || tag?.name || '';
  state.selectedEditTagIds.delete(id);
  setSelectedEditTagName(name, false);
}

function selectEditTag(tagId, tagName = '') {
  const tag = editAvailableTags().find(t => Number(t.id) === Number(tagId));
  const name = tagName || tag?.name || '';
  state.selectedEditTagIds.add(Number(tagId));
  setSelectedEditTagName(name, true);
  logUiAction('edit_tag_select', {
    name,
    selected: true,
    global: Boolean(tag?.global),
    selected_count: state.selectedIds.size,
    artist_ids: selectedEditArtistIds(),
  });
  clearEditTagQuery();
  openEditTagPicker();
}

function toggleEditTagSelection(tagId, tagName = '') {
  const id = Number(tagId);
  const tag = editAvailableTags().find(t => Number(t.id) === id);
  const name = tagName || tag?.name || '';
  const selected = state.selectedEditTagIds.has(id) || selectedEditTagNameKeys().has(tagNameKey(name));
  if (selected) {
    removeSelectedEditTag(id, name);
  } else {
    state.selectedEditTagIds.add(id);
    setSelectedEditTagName(name, true);
  }
  logUiAction('edit_tag_select', {
    name,
    selected: !selected,
    global: Boolean(tag?.global),
    selected_count: state.selectedIds.size,
    artist_ids: selectedEditArtistIds(),
  });
  renderEditTagPicker();
  ensureEditTagContext();
}

function selectFirstEditTagResult() {
  const query = (state.editTagQuery || '').trim();
  const exact = exactEditTagMatch(query);
  if (exact) {
    selectEditTag(exact.id, exact.name);
    return;
  }
  const first = $('#editTagPickerPanel [data-tag-id]');
  if (first) {
    selectEditTag(Number(first.dataset.tagId), first.dataset.tagName || '');
    return;
  }
  if (query) createOrSelectEditTag(query);
}

function addVirtualEditTag(tagName) {
  const key = tagNameKey(tagName);
  if (!key) return null;
  const existing = exactEditTagMatch(tagName);
  if (existing) {
    selectEditTag(existing.id, existing.name);
    return existing;
  }
  const virtual = {id: `name:${key}`, name: tagName.trim(), item_count: 0, virtual: true};
  state.tags = [...(state.tags || []).filter(tag => tagNameKey(tag.name) !== key), virtual];
  setSelectedEditTagName(tagName, true);
  clearEditTagQuery();
  renderEditTagPicker();
  openEditTagPicker();
  return virtual;
}

async function createOrSelectEditTag(name = '') {
  const tagName = (name || state.editTagQuery || $('#editTagSearch')?.value || '').trim();
  if (!tagName) {
    toast('请输入标签名', 'error');
    return;
  }
  const existing = exactEditTagMatch(tagName);
  if (existing) {
    selectEditTag(existing.id, existing.name);
    return existing;
  }
  const artistId = currentEditArtistId();
  if (!artistId) return addVirtualEditTag(tagName);
  const busyId = tagName.toLowerCase();
  if (isActionBusy('edit-create-tag', busyId)) return null;
  setActionBusy('edit-create-tag', busyId, true);
  try {
    const created = await API.post(`/api/tags?artist_id=${artistId}&name=${encodeURIComponent(tagName)}`);
    state.tags = await API.get(`/api/tags?artist_id=${artistId}`);
    if (created && created.id) selectEditTag(Number(created.id), created.name || tagName);
    setSelectedEditTagName(tagName, true);
    clearEditTagQuery();
    renderEditTagPicker();
    openEditTagPicker();
    toast('标签已创建', 'success');
    return created;
  } catch (e) {
    toast('创建标签失败', 'error');
    return null;
  } finally {
    setActionBusy('edit-create-tag', busyId, false);
  }
}

async function selectOrCreateEditTagQuery() {
  const tagName = (state.editTagQuery || $('#editTagSearch')?.value || '').trim();
  if (!tagName) return null;
  return createOrSelectEditTag(tagName);
}

function selectedEditTagIds() {
  const selected = new Set([...state.selectedEditTagIds].map(numericTagId).filter(id => id != null));
  const selectedNames = selectedEditTagNameKeys();
  return editAvailableTags()
    .filter(tag => selected.has(numericTagId(tag.id)) || selectedNames.has(tagNameKey(tag.name)))
    .map(tag => numericTagId(tag.id))
    .filter(id => id != null);
}

function selectedEditTagNames(extraTagIds = []) {
  const names = [...state.selectedEditTagNames];
  const ids = new Set([...state.selectedEditTagIds, ...(extraTagIds || [])].map(id => Number(id)));
  editAvailableTags().forEach(tag => {
    if (ids.has(Number(tag.id)) && tag.name) names.push(tag.name);
  });
  const byKey = new Map();
  names.forEach(name => {
    const key = tagNameKey(name);
    if (key && !byKey.has(key)) byKey.set(key, name.trim());
  });
  return [...byKey.values()];
}

function clearSelectedEditTags() {
  state.selectedEditTagIds.clear();
  state.selectedEditTagNames.clear();
  renderEditTagPicker();
  updateEditTagPickerSummary();
}

async function classifyItems(ids, tagIds, mode='add') {
  if (!ids.length) return;
  if (isActionBusy('edit-classify-items')) return;
  setActionBusy('edit-classify-items', '', true);
  const artistId = currentEditArtistId();
  const tagNames = selectedEditTagNames(tagIds);
  if (!tagNames.length && !tagIds.length) {
    setActionBusy('edit-classify-items', '', false);
    return;
  }
  const gridScrollAnchor = captureGridScrollAnchor();
  try {
    let result = null;
    if (tagNames.length) {
      result = await API.putJson('/api/items/tags-by-name', {item_ids: ids, tag_names: tagNames, mode});
    } else if (artistId) {
      result = await API.put(`/api/items/tags?artist_id=${artistId}&item_ids=${ids.join(',')}&tag_ids=${tagIds.join(',')}&mode=${mode}`);
    }
    logUiAction('edit_apply_result', {
      target: 'items',
      mode,
      requested_count: ids.length,
      item_ids: ids,
      updated: result?.updated || 0,
      artists: result?.artists || 0,
      tags: result?.tags || tagNames.length || tagIds.length,
      propagated: result?.propagated || 0,
      tag_ids: tagIds,
      tag_names: tagNames,
      changed_count: result?.changed_count || 0,
      changed_item_ids: result?.changed_item_ids || [],
    });
    clearSelectedEditTags();
    state.selectedIds.clear();
    resetCharacterTagSuggestions();
    resetArtistSuggestions();
    updateEditBar();
    toast('标签已更新', 'success');

    if (state.currentArtist) {
      state.stats = await API.get(`/api/artists/${state.currentArtist.id}/stats`);
      state.tags = await API.get(`/api/tags?artist_id=${state.currentArtist.id}`);
      renderSidebar();
    } else {
      await ensureEditTagContext();
    }
    renderEditTagPicker();
    await loadItems();
    const restoreResult = restoreGridScrollAnchor(gridScrollAnchor);
    logUiAction('edit_apply_layout', collectUiLogContext({
      target: 'items',
      mode,
      first_visible_id: restoreResult?.first_visible_id ?? null,
      before_top: restoreResult?.before_top == null ? null : Math.round(restoreResult.before_top),
      after_top: restoreResult?.after_top == null ? null : Math.round(restoreResult.after_top),
      top_delta: restoreResult?.top_delta == null ? null : Math.round(restoreResult.top_delta),
      grid_scroll_top: restoreResult?.grid_scroll_top ?? ($('#gridContainer') ? Math.round($('#gridContainer').scrollTop) : 0),
      edit_bar_height: restoreResult?.edit_bar_height ?? ($('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0),
      restored: Boolean(restoreResult?.restored),
    }));
    return result;
  } catch (e) {
    const message = e.message || String(e);
    logUiAction('edit_apply_result', {
      target: 'items',
      mode,
      requested_count: ids.length,
      item_ids: ids,
      tag_ids: tagIds,
      tag_names: tagNames,
      error: message,
    });
    toast((mode === 'remove' ? '移除标签失败: ' : '添加标签失败: ') + message, 'error');
    return {failed: true, error: message};
  } finally {
    setActionBusy('edit-classify-items', '', false);
  }
}

async function classifyFolder(folder, tagIds, mode='add') {
  if (!state.currentArtist || !folder) return;
  if (isActionBusy('edit-classify-folder', folder)) return;
  setActionBusy('edit-classify-folder', folder, true);
  const tagNames = selectedEditTagNames(tagIds);
  if (!tagNames.length && !tagIds.length) {
    setActionBusy('edit-classify-folder', folder, false);
    return;
  }
  const gridScrollAnchor = captureGridScrollAnchor();
  try {
    let result = null;
    if (tagNames.length) {
      result = await API.putJson('/api/folders/tags-by-name', {
        artist_id: state.currentArtist.id,
        folder,
        tag_names: tagNames,
        mode,
      });
    } else {
      result = await API.put(`/api/folders/tags?artist_id=${state.currentArtist.id}&folder=${encodeURIComponent(folder)}&tag_ids=${tagIds.join(',')}&mode=${mode}`);
    }
    logUiAction('edit_apply_result', {
      target: 'folder',
      mode,
      folder,
      updated: result?.updated || 0,
      tag_ids: tagIds,
      tag_names: result?.tag_names || tagNames,
    });
    clearSelectedEditTags();
    updateEditBar();
    toast(`文件夹标签已更新：${result.updated} 张`, 'success');

    state.stats = await API.get(`/api/artists/${state.currentArtist.id}/stats`);
    state.tags = await API.get(`/api/tags?artist_id=${state.currentArtist.id}`);
    state.folders = await API.get(`/api/folders?artist_id=${state.currentArtist.id}`);
    renderEditTagPicker();
    renderSidebar();
    renderFolderTree();
    await loadItems();
    const restoreResult = restoreGridScrollAnchor(gridScrollAnchor);
    logUiAction('edit_apply_layout', collectUiLogContext({
      target: 'folder',
      mode,
      folder,
      first_visible_id: restoreResult?.first_visible_id ?? null,
      before_top: restoreResult?.before_top == null ? null : Math.round(restoreResult.before_top),
      after_top: restoreResult?.after_top == null ? null : Math.round(restoreResult.after_top),
      top_delta: restoreResult?.top_delta == null ? null : Math.round(restoreResult.top_delta),
      grid_scroll_top: restoreResult?.grid_scroll_top ?? ($('#gridContainer') ? Math.round($('#gridContainer').scrollTop) : 0),
      edit_bar_height: restoreResult?.edit_bar_height ?? ($('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0),
      restored: Boolean(restoreResult?.restored),
    }));
  } catch (e) {
    logUiAction('edit_apply_result', {
      target: 'folder',
      mode,
      folder,
      error: e.message,
    });
    toast('更新文件夹标签失败: ' + e.message, 'error');
  } finally {
    setActionBusy('edit-classify-folder', folder, false);
  }
}

async function removeSelectedTagsFromItems() {
  if (isActionBusy('edit-remove-tags')) return;
  const ids = [...state.selectedIds];
  const tagIds = selectedEditTagIds();
  const tagNames = selectedEditTagNames(tagIds);
  if (state.selectedIds.size === 0) {
    toast('请先选择要移除标签的媒体', 'error');
    return;
  }
  if (tagIds.length === 0 && tagNames.length === 0) {
    toast('请选择要移除的标签', 'error');
    return;
  }
  setActionBusy('edit-remove-tags', '', true);
  try {
    const result = await classifyItems(ids, tagIds, 'remove');
    if (result?.failed) {
      logUiAction('edit_remove_tags_result', {
        mode: 'remove',
        item_ids: ids,
        tag_ids: tagIds,
        tag_names: tagNames,
        error: result.error || 'failed',
      });
      return result;
    }
    tagIds.forEach(tagId => {
      const tag = editAvailableTags().find(candidate => Number(candidate.id) === Number(tagId));
      removeSelectedEditTag(tagId, tag?.name || '');
    });
    logUiAction('edit_remove_tags_result', {
      mode: 'remove',
      item_ids: ids,
      tag_ids: tagIds,
      tag_names: tagNames,
      updated: result?.updated || 0,
      changed_count: result?.changed_count || 0,
      changed_item_ids: result?.changed_item_ids || [],
    });
    return result;
  } catch (e) {
    logUiAction('edit_remove_tags_result', {
      mode: 'remove',
      item_ids: ids,
      tag_ids: tagIds,
      tag_names: tagNames,
      error: e.message,
    });
    toast('移除标签失败: ' + e.message, 'error');
  } finally {
    setActionBusy('edit-remove-tags', '', false);
  }
}
