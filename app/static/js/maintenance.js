function movePanelScrollTop() {
  const panel = $('#movePanel');
  return panel ? panel.scrollTop : 0;
}

function restoreMovePanelScroll(top) {
  if (top == null) return;
  const panel = $('#movePanel');
  if (!panel) return;
  panel.scrollTop = top;
  requestAnimationFrame(() => { panel.scrollTop = top; });
}

async function loadMoveWorkbench(options = {}) {
  return refreshActiveMaintenanceView(options);
}

function isAbortError(error) {
  return Boolean(error && (error.name === 'AbortError' || error.code === 20));
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
    await maybeAutoResolveMoveCandidatesBeforePathLoad(loadOptions);
    const fetchOptions = loadOptions.signal ? {signal: loadOptions.signal} : {};
    const [pending, groups, applied, hashStatus] = await Promise.all([
      API.get('/api/move-candidates?status=pending&hide_grouped=true&limit=80&offset=0', fetchOptions),
      API.get('/api/move-candidates/groups?status=pending', fetchOptions),
      API.get('/api/move-history?status=applied&limit=40&offset=0', fetchOptions),
      API.get('/api/hash/status', fetchOptions),
    ]);
    state.moveCandidates = pending.candidates || [];
    state.movePendingTotal = pending.total ?? state.moveCandidates.length;
    state.moveCandidateGroups = groups.groups || [];
    state.moveWaitingHashCount = pending.waiting_hash_count || 0;
    state.moveHistory = applied.history || [];
    state.hashStatus = hashStatus;
  },
  advanced: async loadOptions => {
    await loadOperationLog(loadOptions);
    await loadCharacterLibrary(loadOptions);
  },
};

async function maybeAutoResolveMoveCandidatesBeforePathLoad(loadOptions = {}) {
  if (loadOptions.skipAutoResolve) return null;
  if (loadOptions.signal && loadOptions.signal.aborted) return null;
  if (activeMaintenanceViewHasActiveWork()) return null;
  return autoResolveMoveCandidates({silent: true, refresh: false, updateArtists: false});
}

function activeMaintenanceViewHasActiveWork() {
  const view = state.maintenanceView || 'overview';
  const hashWorker = state.hashStatus && state.hashStatus.worker ? state.hashStatus.worker : {};
  const scan = (state.lastScanState && state.lastScanState.status === 'scanning')
    ? state.lastScanState
    : (state.healthSummary && state.healthSummary.scan ? state.healthSummary.scan : state.lastScanState);
  if (view === 'paths') {
    return Boolean((scan && scan.status === 'scanning') || hashWorker.thread_alive);
  }
  if (view === 'advanced') {
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
  if (view === 'advanced') {
    renderOperationLog();
    renderCharacterLibrary();
  }
}

async function refreshActiveMaintenanceView(options = {}) {
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

function scheduleMaintenanceAutoRefresh() {
  if (maintenanceAutoRefreshTimer) clearTimeout(maintenanceAutoRefreshTimer);
  maintenanceAutoRefreshTimer = null;
  if (state.mode !== 'moves' || document.hidden) return;
  maintenanceAutoRefreshTimer = setTimeout(refreshMoveWorkbenchAutomatically, maintenanceRefreshDelayMs());
}

function startMaintenanceAutoRefresh() {
  if (maintenanceAutoRefreshTimer) {
    clearTimeout(maintenanceAutoRefreshTimer);
    maintenanceAutoRefreshTimer = null;
  }
  scheduleMaintenanceAutoRefresh();
}

function stopMaintenanceAutoRefresh() {
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

function setMaintenanceView(view, options = {}) {
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
  if (selected === 'advanced' && options.focus === 'characters') {
    const panel = $('#characterLibraryPanel');
    if (panel) {
      panel.open = true;
      requestAnimationFrame(() => {
        panel.scrollIntoView({block: 'start', behavior: 'smooth'});
      });
    }
  }
  if (selected === 'advanced' && options.focus === 'history') {
    const panel = $('#operationLogPanel');
    if (panel) {
      panel.open = true;
      requestAnimationFrame(() => {
        panel.scrollIntoView({block: 'start', behavior: 'smooth'});
      });
    }
  }
  if (selected === 'paths') {
    const workbench = $('.maintenance-workbench');
    if (workbench && options.scrollToWorkbench) {
      requestAnimationFrame(() => {
        workbench.scrollIntoView({block: 'start', behavior: 'smooth'});
      });
    }
  }
}

function renderOverviewActions() {
  const pathsHint = $('#overviewPathsHint');
  if (!pathsHint) return;
  const pending = Number(state.movePendingTotal || 0);
  const waiting = Number(state.moveWaitingHashCount || 0);
  if (pending > 0) {
    pathsHint.textContent = `${pending} 项需要你确认` + (waiting > 0 ? `，另有 ${waiting} 项系统处理中` : '');
    pathsHint.classList.add('is-attention');
  } else if (waiting > 0) {
    pathsHint.textContent = `${waiting} 项相同文件检查进行中，暂无需要确认的路径`;
    pathsHint.classList.remove('is-attention');
  } else {
    pathsHint.textContent = '当前没有需要你确认的路径变更';
    pathsHint.classList.remove('is-attention');
  }
}

function handleMaintenanceJump(jump) {
  if (jump === 'paths') {
    setMaintenanceView('paths', {scrollToWorkbench: true});
    loadMoveWorkbench({view: 'paths'}).catch(() => {});
    return;
  }
  if (jump === 'history') {
    setMaintenanceView('advanced', {focus: 'history'});
    loadMoveWorkbench({view: 'advanced'}).catch(() => {});
    return;
  }
  if (jump === 'characters') {
    setMaintenanceView('advanced', {focus: 'characters'});
    loadMoveWorkbench({view: 'advanced'}).catch(() => {});
  }
}

async function loadHealthSummary(options = {}) {
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

async function loadHashStatus(options = {}) {
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

async function loadOperationLog(options = {}) {
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

function renderMovePathSummary() {
  $('#movePendingCount').textContent = state.movePendingTotal ?? state.moveCandidates.length;
  $('#moveWaitingHashCount').textContent = state.moveWaitingHashCount || 0;
  $('#movePreviewCount').textContent = state.moveHistory.length;
}

async function loadCharacterLibrary(options = {}) {
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  const requestedCharacterId = options.characterId != null
    ? Number(options.characterId)
    : state.characterLibrarySelectedCharacterId;
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
      if (updateState) {
        state.characterLibrary = characterLibrary;
        state.characterLibrarySelectedCharacterId = requestedCharacterId;
        state.characterLibraryLoading = false;
      }
      if (render) renderCharacterLibrary();
      toast('刷新角色库失败: ' + (summaryError.message || summaryError), 'error');
      return characterLibrary;
    }
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
        toast('读取角色引用失败: ' + referenceErrorMessage, 'error');
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
    `${totals.tags || (summary.tags || []).length || 0} 个标签候选`,
    `${totals.characters || (summary.characters || []).length || 0} 个角色`,
    `${totals.references || 0} 条参考图`,
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

function characterImportJobBusy() {
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

function renderCharacterLibrary() {
  const summaryEl = $('#characterLibrarySummary');
  const tagList = $('#characterTagImportList');
  const characterList = $('#characterList');
  const referenceList = $('#characterReferenceList');
  if (!summaryEl || !tagList || !characterList || !referenceList) return;

  const library = state.characterLibrary;
  if (!library) {
    summaryEl.textContent = state.characterLibraryLoading ? '角色库读取中' : '角色库未加载';
    tagList.innerHTML = '<div class="character-library-empty">暂无角色标签</div>';
    characterList.innerHTML = '<div class="character-library-empty">暂无角色</div>';
    referenceList.innerHTML = '<div class="character-library-empty">请选择角色后查看参考图</div>';
    return;
  }
  if (library.error) {
    summaryEl.textContent = `角色库读取失败：${library.error}`;
    tagList.innerHTML = `<div class="character-library-empty">${escHtml(library.error)}</div>`;
    characterList.innerHTML = '<div class="character-library-empty">角色库不可用</div>';
    referenceList.innerHTML = '<div class="character-library-empty">角色库不可用</div>';
    return;
  }

  const summary = library.summary || {};
  const tags = summary.tags || [];
  const characters = summary.characters || [];
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
    const pendingCount = Math.max(0, singleTagCount - referenceCount);
    const imported = referenceCount > 0;
    const linked = tag.character_id
      ? `<span class="character-library-badge${imported ? ' character-library-imported' : ''}">${referenceCountLabel(tag.reference_count)}</span><span class="character-library-badge">角色 #${tag.character_id}</span>`
      : '';
    const artistCount = Number(tag.artist_count || 0);
    const sourceLabel = artistCount > 1 ? `来自 ${artistCount} 个画师` : '';
    const tagName = String(tag.name || '');
    const pendingLabel = imported ? '' : `<span class="character-library-badge">${pendingCount} 张待自动导入</span>`;
    return `
      <div class="character-tag-row">
        <div class="character-tag-main">
          <b>${escHtml(tagName)}</b>
          <span>${escHtml(joinUiMeta([`${singleTagCount} 张单标签图`, `${referenceCount} 条参考图`, sourceLabel]))}</span>
        </div>
        <div class="character-tag-actions">
          ${linked}
          ${pendingLabel}
        </div>
      </div>
    `;
  }).join('') : '<div class="character-library-empty">暂无可导入标签</div>';

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
              <span>${escHtml(joinUiMeta([`${referenceCount} 条参考图`, `#${character.id}`]))}</span>
            </div>
            <div class="character-card-created">${escHtml(`创建于 ${character.created_at || '未知'}`)}</div>
          </div>
        </button>
        <button type="button" class="btn btn-danger btn-icon character-card-delete" data-character-delete="${character.id}" title="删除角色" aria-label="删除角色">${buttonIcon('trash')}</button>
      </div>
    `;
  }).join('') : '<div class="character-library-empty">暂无角色</div>';
  const referenceCards = references.length ? references.map(reference => {
    const pathText = reference.display_file_path || reference.file_path || reference.file_name || '未绑定文件';
    const previewUrl = reference.file_path ? API.previewUrl(reference.file_path, characterReferencePreviewVersion(reference), 256) : '';
    const detail = joinUiMeta([
      reference.source_type || 'gallery_item',
      reference.media_type || '',
      reference.embedding_dim ? `${reference.embedding_dim} 维` : '',
      reference.file_size ? formatBytes(reference.file_size) : '',
    ]);
    return `
      <div class="character-reference-card">
        <div class="character-reference-thumb">
          ${previewUrl ? `<img src="${escHtml(previewUrl)}" alt="" loading="lazy" onerror="this.closest('.character-reference-thumb').classList.add('failed')">` : '<span>无预览</span>'}
        </div>
        <div class="character-reference-detail">
          <div class="character-reference-head">
            <div>
              <b>${escHtml(reference.character_name || '')}</b>
              <span>${escHtml(detail)}</span>
            </div>
            <button class="btn btn-danger" type="button" data-character-reference-delete="${reference.id}" data-character-id="${reference.character_id}">${buttonIcon('trash')}删除</button>
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

  const currentArtistBtn = $('#characterImportCurrentArtistBtn');
  if (currentArtistBtn) {
    currentArtistBtn.disabled = importCurrentDisabled;
    currentArtistBtn.title = currentArtistId ? `导入当前画师 ${state.currentArtist.name}` : '先选择画师';
  }
  const allBtn = $('#characterImportAllBtn');
  if (allBtn) {
    allBtn.disabled = importAllDisabled;
    allBtn.title = '导入全库中符合条件的标签';
  }
  const rebuildBtn = $('#characterRebuildIndexBtn');
  if (rebuildBtn) {
    rebuildBtn.disabled = rebuildDisabled;
    rebuildBtn.title = '重建角色识别用的参考索引';
  }
}

function referenceCountLabel(referenceCount) {
  return Number(referenceCount || 0) > 0 ? '已导入' : '未导入';
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
    rememberCharacterImportJob(job);
    if (!job || !['pending', 'running'].includes(job.status)) {
      stopCharacterImportPolling();
      await finishCharacterImportJob(job);
    }
  } catch (e) {
    stopCharacterImportPolling();
    toast('读取角色库导入进度失败: ' + (e.message || e), 'error');
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
    toast(`已导入 ${result.added || result.added_references || 0} 条参考图`, result.status === 'failed' ? 'error' : 'success');
  }
  const importedCharacterId = characterIdFromImportResult(result);
  await loadCharacterLibrary({characterId: importedCharacterId || state.characterLibrarySelectedCharacterId});
}

async function importCharacterLibraryReferences(payload) {
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
    toast('导入角色库失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-import', busyScope, false);
  }
}

async function cancelCharacterImportJob(jobId) {
  if (!jobId || isActionBusy('character-library-import-cancel', jobId)) return;
  setActionBusy('character-library-import-cancel', jobId, true);
  try {
    const result = await API.post(`/api/characters/import-from-tags/jobs/${jobId}/cancel`);
    rememberCharacterImportJob(result);
  } catch (e) {
    toast('取消角色库导入失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-import-cancel', jobId, false);
  }
}

async function deleteCharacterReference(characterId, referenceId) {
  if (!characterId || !referenceId) return;
  if (isActionBusy('character-library-delete', `${characterId}:${referenceId}`)) return;
  if (!confirm('删除这条参考图？')) return;
  setActionBusy('character-library-delete', `${characterId}:${referenceId}`, true);
  try {
    await API.del(`/api/characters/${characterId}/references/${referenceId}`);
    toast('参考图已删除', 'success');
    await loadCharacterLibrary({characterId});
  } catch (e) {
    toast('删除参考图失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-delete', `${characterId}:${referenceId}`, false);
  }
}

async function deleteCharacter(characterId) {
  if (!characterId) return;
  if (isActionBusy('character-library-character-delete', characterId)) return;
  if (!confirm('删除这个角色和它的参考图？之后如果还有符合规则的单标签图，可能会再次自动导入。')) return;
  setActionBusy('character-library-character-delete', characterId, true);
  try {
    await API.del(`/api/characters/${characterId}`);
    toast('角色已删除', 'success');
    const nextCharacterId = Number(state.characterLibrarySelectedCharacterId) === Number(characterId) ? null : state.characterLibrarySelectedCharacterId;
    await loadCharacterLibrary({characterId: nextCharacterId});
  } catch (e) {
    toast('删除角色失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-character-delete', characterId, false);
  }
}

async function rebuildCharacterIndex() {
  if (isActionBusy('character-library-rebuild')) return;
  setActionBusy('character-library-rebuild', '', true);
  try {
    const result = await API.post('/api/admin/rebuild-character-index');
    const text = result.ok ? `角色参考已刷新：${result.vector_count || 0} 条` : (result.reason || '刷新失败');
    toast(text, result.ok ? 'success' : 'error');
    await loadCharacterLibrary({characterId: state.characterLibrarySelectedCharacterId});
  } catch (e) {
    toast('刷新角色参考失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('character-library-rebuild', '', false);
  }
}

function renderFolderRenameAutoStatus() {
  const toggle = $('#folderRenameAutoExecuteToggle');
  if (!toggle) return;
  const status = state.folderRenameAuto;
  const hasError = Boolean(status && status.error);
  const saving = Boolean(status && status.saving);
  toggle.checked = Boolean(status && status.enabled);
  toggle.disabled = !status || hasError || saving;
}

async function loadFolderRenameAutoStatus(options = {}) {
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

async function setFolderRenameAutoEnabled(enabled) {
  const previous = state.folderRenameAuto;
  state.folderRenameAuto = {...(previous || {}), enabled, saving: true};
  renderFolderRenameAutoStatus();
  try {
    const status = await API.putJson('/api/folder-renames/auto', {enabled});
    state.folderRenameAuto = status;
    renderFolderRenameAutoStatus();
    toast(enabled ? '自动归档已开启' : '自动归档已关闭', 'success');
  } catch (e) {
    state.folderRenameAuto = previous || {enabled: !enabled};
    renderFolderRenameAutoStatus();
    toast('自动归档设置失败: ' + (e.message || e), 'error');
  }
}

function formatBytes(bytes) {
  const value = Number(bytes || 0);
  if (value >= 1024 * 1024 * 1024) return `${(value / 1024 / 1024 / 1024).toFixed(1)} GB`;
  if (value >= 1024 * 1024) return `${(value / 1024 / 1024).toFixed(1)} MB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${value} B`;
}

function formatHealthTime(timestamp) {
  if (!timestamp) return '无记录';
  const date = new Date(Number(timestamp) * 1000);
  if (Number.isNaN(date.getTime())) return '无记录';
  return date.toLocaleString();
}

function operationLogKindLabel(operation) {
  if (operation.reason === 'tagged_file') return '已标签文件归位';
  if (operation.kind === 'folder_rename') return '文件夹归档';
  if (operation.kind === 'move') return '路径变更';
  return '操作';
}

function operationLogReasonLabel(reason) {
  const labels = {
    tagged_file: '已标签文件归位',
    folder_rename: '文件夹归档',
    backup_failed: '数据库备份失败',
    source_missing: '来源文件夹不存在',
    target_exists: '目标已存在',
    bad_folder_path: '文件夹路径无效',
    db_update_failed: '数据库路径更新失败',
    outside_artist: '路径不在画师目录内',
    execution_failed: '执行失败',
    blocked: '当前不安全，已跳过',
  };
  return labels[reason] || reason || '';
}

function operationLogStatusLabel(status) {
  const labels = {
    applied: '已确认',
    executed: '已执行',
    preview: '自动确认',
    new: '新文件',
    ignored: '已忽略',
    failed: '失败',
    error: '失败',
  };
  return labels[status] || status || '已记录';
}

function renderOperationLog() {
  const summary = $('#operationLogSummary');
  const historyList = $('#operationHistoryList');
  const errorList = $('#operationErrorList');
  if (!summary || !historyList || !errorList) return;
  const log = state.operationLog;
  if (!log) {
    summary.textContent = '历史读取中';
    historyList.innerHTML = '<div class="move-empty small">历史读取中</div>';
    errorList.innerHTML = '<div class="move-empty small">错误读取中</div>';
    return;
  }
  if (log.error) {
    summary.textContent = '读取历史失败';
    historyList.innerHTML = `<div class="operation-error">${escHtml(log.error)}</div>`;
    errorList.innerHTML = '<div class="move-empty small">暂无错误记录</div>';
    return;
  }
  const history = log.history || [];
  const errors = log.errors || [];
  summary.textContent = joinUiMeta([`${history.length} 条移动/重命名历史`, `${errors.length} 条最近错误`]);
  historyList.innerHTML = history.length ? history.map(operation => {
    const source = operation.display_source || operation.source || '';
    const target = operation.display_target || operation.target || '';
    const kindClass = operation.kind === 'folder_rename'
      ? 'rename'
      : (operation.kind === 'move' ? 'move' : 'other');
    return `
      <div class="operation-entry ${kindClass}">
        <div class="operation-entry-head">
          <b>${escHtml(operationLogKindLabel(operation))}</b>
          <span>${escHtml(operationLogStatusLabel(operation.status))}</span>
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
  }).join('') : '<div class="move-empty small">暂无移动或重命名历史</div>';
  errorList.innerHTML = errors.length ? errors.map(row => `
    <div class="operation-error">
      <b>${escHtml(row.source || 'log')}</b>
      <code>${escHtml(row.line || '')}</code>
    </div>
  `).join('') : '<div class="move-empty small">最近没有错误记录</div>';
}

function isHealthObject(value) {
  return Boolean(value && typeof value === 'object' && !Array.isArray(value));
}

function healthNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function formatHealthScanStatus(scan) {
  if (!isHealthObject(scan) || (!scan.status && !scan.phase)) return '扫描状态未上报';
  const statusLabels = {
    idle: '空闲',
    scanning: '扫描中',
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
    return '哈希状态未上报';
  }
  const itemRemaining = healthNumber(hash.items.remaining);
  const candidateRemaining = healthNumber(hash.scan_candidates.remaining);
  const itemErrors = healthNumber(hash.items.error);
  const candidateErrors = healthNumber(hash.scan_candidates.error);
  if ([itemRemaining, candidateRemaining, itemErrors, candidateErrors].some(value => value == null)) {
    return '哈希计数未上报';
  }
  const remaining = itemRemaining + candidateRemaining;
  const errors = itemErrors + candidateErrors;
  if (remaining === 0 && errors === 0) return joinUiMeta(['已完成', '无错误']);
  const parts = [remaining ? `剩余 ${remaining}` : '已完成'];
  parts.push(errors ? `错误 ${errors}` : '无错误');
  return joinUiMeta(parts);
}

function formatHealthSchedule(schedule, nextKey) {
  if (!isHealthObject(schedule)) return '排期未上报';
  if (schedule.error || schedule.last_error) return '排期读取失败';
  if (schedule.enabled === false) return '未启用';
  if (schedule.enabled !== true) return '排期未上报';
  const nextAt = schedule[nextKey];
  const interval = healthNumber(schedule.interval);
  const intervalHours = interval ? Math.round(interval / 3600 * 10) / 10 : 0;
  if (!nextAt) return intervalHours ? joinUiMeta([`每 ${intervalHours} 小时`, '等待排期']) : '等待排期';
  const prefix = schedule.overdue ? '已到时间' : `下次 ${formatHealthTime(nextAt)}`;
  const suffix = schedule.deferred_by_manual ? '手动后顺延' : '';
  return intervalHours ? joinUiMeta([prefix, `每 ${intervalHours} 小时`, suffix]) : joinUiMeta([prefix, suffix]);
}

function renderHealthSummary() {
  const grid = $('#healthGrid');
  if (!grid) return;
  const health = state.healthSummary;
  if (!health) {
    grid.innerHTML = '<div class="maintenance-card status-card status-muted move-empty small">健康状态读取中</div>';
    return;
  }
  if (health.error) {
    grid.innerHTML = `<div class="maintenance-card status-card status-danger"><b>状态检查失败</b><span>${escHtml(health.error)}</span></div>`;
    return;
  }
  const database = health.database;
  const backups = health.backups;
  const latestBackup = isHealthObject(backups) && isHealthObject(backups.latest) ? backups.latest : null;
  const logs = health.logs;
  const galleryLog = isHealthObject(logs) ? logs.gallery_log : null;
  const uiLog = isHealthObject(logs) ? logs.ui_actions_log : null;
  const scan = health.scan;
  const scanSchedule = health.scan_schedule;
  const backupSchedule = health.backup_schedule;
  const hash = health.hash;
  const errors = health.recent_errors;
  const databaseKnown = isHealthObject(database);
  const backupsKnown = isHealthObject(backups);
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
    : (health.database_error || health.schema_error ? 'status-danger' : (database.exists !== true ? 'status-danger' : (health.ok === true ? 'status-ok' : 'status-warn')));
  const backupCount = backupsKnown ? healthNumber(backups.count) : null;
  const backupStatus = !backupsKnown
    ? 'status-muted'
    : (backups.error || (backupSchedule && backupSchedule.last_error) ? 'status-danger' : (backupCount == null ? 'status-muted' : (backupCount > 0 && latestBackup ? 'status-ok' : 'status-warn')));
  const scanIncomplete = scanKnown && scan.phase === 'complete'
    && healthNumber(scan.scanned_count) != null
    && healthNumber(scan.total_estimate) != null
    && scan.scanned_count < scan.total_estimate;
  const scanStatus = !scanKnown
    ? 'status-muted'
    : (scan.status === 'error' ? 'status-danger' : (scanIncomplete || scan.phase === 'interrupted' ? 'status-warn' : (scan.status === 'scanning' ? 'status-info' : 'status-ok')));
  const hashStatus = !hashCountsKnown || hash.blake3_available == null
    ? 'status-muted'
    : (health.database_error || hash.blake3_available === false ? 'status-danger' : (hashRemaining > 0 ? 'status-info' : (hashErrors > 0 ? 'status-warn' : 'status-ok')));
  const logStatus = !logsKnown
    ? 'status-muted'
    : (galleryLog.error || uiLog.error ? 'status-danger' : (galleryLog.exists === false && uiLog.exists === false ? 'status-warn' : 'status-ok'));
  const errorsKnown = Array.isArray(errors);
  const errorStatus = !errorsKnown ? 'status-muted' : (errors.length ? 'status-warn' : 'status-ok');
  const databaseText = !databaseKnown
    ? ['数据库状态未上报']
    : [
        health.database_error ? '数据库读取失败' : (health.schema_error ? '数据库结构检查失败' : (database.exists === true ? (health.ok === true ? '文件可读' : '状态异常') : '文件不存在')),
        database.size_bytes != null ? formatBytes(database.size_bytes) : '',
      ];
  const backupText = !backupsKnown
    ? ['备份状态未上报']
    : backups.error
      ? ['备份读取失败']
    : backupCount == null
      ? ['备份数量未上报']
      : backupCount === 0
        ? ['暂无备份', '共 0 个']
        : latestBackup
          ? [latestBackup.name || '最近备份', `共 ${backupCount} 个`, latestBackup.size_bytes != null ? formatBytes(latestBackup.size_bytes) : '备份大小未上报']
          : ['最近备份未上报', `共 ${backupCount} 个`];
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
      ? errors.slice(0, 3).map(row => `<code>${escHtml(row.source || '')}: ${escHtml(row.line || '')}</code>`).join('')
      : '<span>最近没有错误记录</span>';
  grid.innerHTML = `
    <div class="maintenance-card status-card ${databaseStatus}"><b>数据库</b><span>${escHtml(joinUiMeta(databaseText))}</span></div>
    <div class="maintenance-card status-card ${backupStatus}"><b>最近备份</b><span>${escHtml(joinUiMeta(backupText))}</span><span class="health-schedule">${escHtml(formatHealthSchedule(backupSchedule, 'next_run_at'))}</span></div>
    <div class="maintenance-card status-card ${scanStatus}"><b>扫描</b><span>${escHtml(formatHealthScanStatus(scan))}</span><span class="health-schedule">${escHtml(formatHealthSchedule(scanSchedule, 'next_auto_scan_at'))}</span></div>
    <div class="maintenance-card status-card ${hashStatus}"><b>相同文件检查</b><span>${escHtml(formatHealthHashStatus(hash))}</span></div>
    <div class="maintenance-card status-card ${logStatus}"><b>日志</b><span>${escHtml(joinUiMeta(logText))}</span></div>
    <div class="maintenance-card status-card ${errorStatus}"><b>最近错误</b><span>${errorHtml}</span></div>
  `;
  renderHashStatus();
  renderOverviewActions();
}

function renderHashStatus() {
  const status = state.hashStatus;
  if (!status) {
    $('#hashStatusText').textContent = '等待检查';
    return;
  }
  if (status.database_error) {
    $('#hashStatusText').textContent = '数据出错';
    return;
  }
  if (!status.blake3_available) {
    $('#hashStatusText').textContent = '暂时不能检查';
    return;
  }
  const items = status.items || {};
  const candidates = status.scan_candidates || {};
  const remaining = (items.remaining || 0) + (candidates.remaining || 0);
  $('#hashStatusText').textContent = formatHashWorkerStatus(status.worker || {}, remaining);
}

function formatHashWorkerStatus(worker, remaining) {
  if (worker.last_error) return '数据出错';
  if (remaining <= 0) return '检查完成';
  if (worker.thread_alive) return joinUiMeta(['正在检查', `还剩 ${remaining}`]);
  return joinUiMeta(['等待检查', `还剩 ${remaining}`]);
}

async function startFullScan(event) {
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
    toast('启动扫描失败', 'error');
  } finally {
    setActionBusy('scan-full', '', false);
  }
}

function renderMoveCandidateGroups() {
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

function renderMoveCandidates() {
  const list = $('#moveCandidateList');
  const groupedCandidateIds = new Set();
  const groupedCount = (state.moveCandidateGroups || [])
    .filter(group => group.can_apply && Number(group.applicable_candidate_count ?? group.candidate_count ?? 0) > 0)
    .reduce((total, group) => total + Number(group.applicable_candidate_count ?? group.candidate_count ?? 0), 0);
  const candidates = (state.moveCandidates || []).filter(candidate => !groupedCandidateIds.has(Number(candidate.id)));
  if (candidates.length === 0) {
    if (groupedCount) {
      list.innerHTML = '<div class="move-empty">已在上方按画师路径分组 ' + groupedCount + ' 项</div>';
      return;
    }
    list.innerHTML = '<div class="move-empty">没有需要你确认的路径</div>';
    return;
  }

  list.innerHTML = candidates.map(c => {
    const label = moveReasonLabel(c.reason);
    const isManual = c.reason === 'manual_needed';
    const oldPath = c.display_old_path || c.old_path || '无旧路径';
    const newPath = c.display_new_path || c.new_path || '';
    const oldArtistPath = c.display_item_artist_path || c.item_artist_path || '';
    const newArtistPath = c.display_candidate_artist_path || c.candidate_artist_path || '';
    const isCrossArtist = Boolean(c.is_cross_artist);
    const cannotConfirm = c.can_confirm === false || isCrossArtist;
    const warning = isCrossArtist
      ? '<div class="move-warning">跨画师路径需要你确认。确定不是同一文件时，再选「作为独立新文件」；暂时不想处理可以忽略。</div>'
      : (isManual ? '<div class="move-warning">请人工核对旧/新路径；能确定同一文件时再确认。</div>' : '');
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
          <span>${c.content_hash ? '哈希已匹配' : '哈希未完成'}</span>
          <span>文件标记 ${c.st_dev || '-'}:${c.st_ino || '-'}</span>
        </div>
      </div>
      <div class="move-actions">
        ${confirmButton}
        <button class="btn btn-ghost" type="button" data-move-action="new" data-id="${c.id}">作为独立新文件</button>
        <button class="btn btn-danger" type="button" data-move-action="ignore" data-id="${c.id}">忽略此候选</button>
      </div>
    </div>`;
  }).join('');
  bindMoveActions();
}

function renderMoveHistory() {
  const list = $('#moveHistoryList');
  const history = state.moveHistory || [];
  if (history.length === 0) {
    list.innerHTML = '<div class="move-empty small">暂无自动确认记录</div>';
    return;
  }
  list.innerHTML = history.slice(0, 40).map(h => {
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
}

function bindMoveActions() {
  $$('#moveCandidateList [data-move-action]').forEach(btn => {
    btn.addEventListener('click', async () => {
      await runMoveAction(btn.dataset.id, btn.dataset.moveAction);
    });
  });
}

function bindMoveGroupActions() {
  $$('#moveCandidateGroupList [data-move-group-action]').forEach(btn => {
    btn.addEventListener('click', async () => {
      await applyMoveCandidateGroup(btn.dataset.oldArtistId, btn.dataset.newArtistId);
    });
  });
}

async function applyMoveCandidateGroup(oldArtistId, newArtistId) {
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
    toast('批量路径确认失败: ' + e.message, 'error');
  } finally {
    setActionBusy('move-group-action', busyKey, false);
  }
}

async function autoResolveMoveCandidates(options = {}) {
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
    const stale = Number(result.stale || 0);
    const skipped = Number(result.skipped || 0);
    const remaining = Number(result.remaining || 0);
    if (!silent) {
      toast(`安全处理 ${applied + resolved + stale} 项，已在库中 ${resolved} 项，清理陈旧 ${stale} 项，跳过 ${skipped} 项，剩余 ${remaining} 项`, (applied || resolved || stale) ? 'success' : 'info');
    }
    if (refresh) {
      await refreshActiveMaintenanceView({preserveScroll: true, view: 'paths', skipAutoResolve: true});
    }
    if (updateArtists) {
      await loadArtists();
    }
    return result;
  } catch (e) {
    if (!silent) toast('自动确认处理失败: ' + e.message, 'error');
    return null;
  } finally {
    if (btn && !silent) {
      btn.disabled = false;
      btn.textContent = oldText || '处理可自动确认的项';
    }
    setActionBusy('move-auto-resolve', '', false);
  }
}

async function runMoveAction(id, action) {
  const paths = {
    confirm: `/api/move-candidates/${id}/confirm`,
    new: `/api/move-candidates/${id}/new`,
    ignore: `/api/move-candidates/${id}/ignore`,
  };
  const path = paths[action];
  if (!path) return;
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
      toast('路径已确认', 'success');
    } else if (result.action === 'new') {
      toast('已作为独立新文件', 'success');
    } else if (result.action === 'existing') {
      toast('路径已在库中', 'success');
    } else if (result.action === 'ignored') {
      toast('已忽略候选', 'success');
    } else {
      toast(result.reason || result.action || '路径候选未变更', 'error');
    }
    await refreshActiveMaintenanceView({preserveScroll: true, view: 'paths'});
    await loadArtists();
  } catch (e) {
    toast('路径候选操作失败: ' + e.message, 'error');
  } finally {
    setActionBusy('move-action', `${action}:${id}`, false);
  }
}

function moveReasonLabel(reason) {
  const labels = {
    inode_untrusted: '同一文件待确认',
    hash_duplicate_active: '重复文件',
    hash_multiple_missing: '多个旧路径',
    manual_needed: '手动确认',
    inode: '同一文件',
    hash_unique: '只找到一个旧文件',
    category_rename: '目录改名',
  };
  return labels[reason] || reason || '待确认';
}

function shortHash(value) {
  if (!value) return '-';
  return value.length > 12 ? value.slice(0, 12) : value;
}
