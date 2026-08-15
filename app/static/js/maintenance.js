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

async function loadArtistFolderMove(options = {}) {
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

function artistFolderMoveDirectoryName() {
  return String($('#artistFolderMoveDestination')?.value || '').trim().split('/').filter(Boolean).pop() || artistFolderDefaultName(state.currentArtist);
}

function artistFolderMoveRootIndex() {
  const rootIndex = Number($('#artistFolderMoveRoot')?.value);
  return Number.isInteger(rootIndex) ? rootIndex : null;
}

function closeArtistFolderMoveDirectoryDialog() {
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
  select.textContent = state.artistFolderMoveDirectoryLoading ? '读取中' : '选这个目录';
  list.innerHTML = state.artistFolderMoveDirectoryLoading
    ? '<div class="move-empty small">读取目录中</div>'
    : (state.artistFolderMoveDirectoryEntries.length
      ? state.artistFolderMoveDirectoryEntries.map(name => `<button class="artist-folder-picker-entry" type="button" data-artist-folder-directory="${escHtml(name)}">${escHtml(name)}</button>`).join('')
      : '<div class="move-empty small">这个目录下没有可进入的文件夹</div>');
}

async function loadArtistFolderMoveDirectories(path = '') {
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
    toast('读取目录失败: ' + (error.message || error), 'error');
  } finally {
    state.artistFolderMoveDirectoryLoading = false;
    renderArtistFolderMoveDirectoryDialog();
  }
}

async function openArtistFolderMoveDirectoryDialog(opener) {
  const dialog = $('#artistFolderMoveDirectoryDialog');
  if (!dialog || typeof dialog.showModal !== 'function' || !state.currentArtist || artistFolderMoveRootIndex() == null) return;
  state.artistFolderMoveDirectoryPath = '';
  state.artistFolderMoveDirectoryEntries = [];
  dialog.showModal();
  await loadArtistFolderMoveDirectories();
  opener?.blur();
}

function chooseArtistFolderMoveDirectory() {
  const destination = $('#artistFolderMoveDestination');
  if (!destination) return;
  const parent = state.artistFolderMoveDirectoryPath;
  destination.value = [parent, artistFolderMoveDirectoryName()].filter(Boolean).join('/');
  closeArtistFolderMoveDirectoryDialog();
  invalidateArtistFolderMovePreview();
}

function renderArtistFolderMove() {
  const panel = $('#artistFolderMovePanel');
  if (!panel) return;
  const artist = state.currentArtist;
  const source = $('#artistFolderMoveSource');
  const rootSelect = $('#artistFolderMoveRoot');
  const destination = $('#artistFolderMoveDestination');
  const browseButton = $('#artistFolderMoveBrowseBtn');
  const previewButton = $('#artistFolderMovePreviewBtn');
  const executeButton = $('#artistFolderMoveExecuteBtn');
  const result = $('#artistFolderMoveResult');
  const artistKey = artist ? String(artist.id) : '';
  const changedArtist = panel.dataset.artistId !== artistKey;
  panel.dataset.artistId = artistKey;

  if (source) source.textContent = artist?.path || '选择画师';
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
  if (destination && changedArtist) destination.value = artistFolderDefaultName(artist);

  const busy = state.artistFolderMoveLoading;
  const available = Boolean(artist && state.artistFolderMoveRoots.length && destination?.value.trim());
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
    const conflictCount = Array.isArray(preview.conflicts) ? preview.conflicts.length : 0;
    const status = preview.can_execute
      ? '可以移动'
      : (preview.target_exists ? '目标文件夹已存在' : `发现 ${conflictCount} 个索引冲突`);
    result.innerHTML = `<strong>${escHtml(status)}</strong><span>${Number(preview.item_count || 0)} 个文件 \u00b7 ${formatSize(Number(preview.total_size || 0))}</span><code>${escHtml(preview.target || '')}</code>`;
  }
}

function invalidateArtistFolderMovePreview() {
  state.artistFolderMovePreview = null;
  renderArtistFolderMove();
}

async function previewArtistFolderMove() {
  const artist = state.currentArtist;
  const rootIndex = Number($('#artistFolderMoveRoot')?.value);
  const destination = $('#artistFolderMoveDestination')?.value.trim() || '';
  if (!artist || !Number.isInteger(rootIndex) || !destination || state.artistFolderMoveLoading) return;
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

async function executeArtistFolderMove() {
  const artist = state.currentArtist;
  const preview = state.artistFolderMovePreview;
  if (!artist || !preview?.can_execute || state.artistFolderMoveLoading) return;
  if (!confirm(`确定要将画师文件夹迁移至以下路径吗？\n${preview.target}\n\n系统将在执行前自动创建数据库备份。`)) return;
  state.artistFolderMoveLoading = true;
  state.artistFolderMoveError = '';
  renderArtistFolderMove();
  try {
    const moveResult = await API.postJson(`/api/artists/${artist.id}/folder-move/execute`, {
      root_index: Number(preview.target_root_index),
      destination: preview.destination,
    });
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
  if (selected === 'paths') {
    const target = $('.maintenance-workbench');
    if (target && options.scrollToWorkbench) {
      requestAnimationFrame(() => {
        target.scrollIntoView({block: 'start', behavior: 'smooth'});
      });
    }
  }
}

function renderOverviewActions() {
  const pathsHint = $('#overviewPathsHint');
  if (!pathsHint) return;
  const pending = Number(state.movePendingTotal || 0);
  const waiting = Number(state.moveWaitingHashCount || 0)
    + Number(state.hashStatus?.scan_candidates?.remaining || 0);
  const hints = [];
  if (pending > 0) hints.push(`${pending} 项需要你确认`);
  if (waiting > 0) hints.push(`${waiting} 项正在自动入库`);
  if (hints.length > 0) {
    pathsHint.textContent = hints.join('，');
    pathsHint.classList.toggle('is-attention', pending > 0);
  } else {
    pathsHint.textContent = '当前没有需要你确认的路径';
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
    setMaintenanceView('records');
    loadMoveWorkbench({view: 'records'}).catch(() => {});
    return;
  }
  if (jump === 'characters') {
    setMaintenanceView('characters');
    loadMoveWorkbench({view: 'characters'}).catch(() => {});
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

async function loadRecycleBin(options = {}) {
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

function renderRecycleBin() {
  const summary = $('#recycleBinSummary');
  const list = $('#recycleBinList');
  const more = $('#recycleBinMore');
  if (!summary || !list || !more) return;
  const recycleBin = state.recycleBin;
  if (!recycleBin) {
    summary.textContent = '回收站读取中...';
    list.innerHTML = '<div class="move-empty small">回收站读取中...</div>';
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
    `${entries.length}/${total} 项可恢复`,
    unavailable ? `${unavailable} 个回收文件缺失` : '',
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
      ? '回收文件缺失'
      : (originalExists ? '原路径已被占用' : '可以恢复');
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
          <button class="btn btn-ops" type="button" data-recycle-restore="${id}" ${canRestore ? '' : 'disabled'} ${restoring ? 'aria-busy="true"' : ''}>${restoring ? '恢复中...' : '恢复'}</button>
        </div>
      </div>`;
  }).join('') : '<div class="move-empty small">回收站中暂无可恢复的文件</div>';
  const nextOffset = Number(recycleBin.next_offset);
  more.innerHTML = Number.isFinite(nextOffset) && nextOffset >= entries.length
    ? '<button class="btn btn-ghost" type="button" data-recycle-load-more>加载更多</button>'
    : '';
}

async function restoreRecycleEntry(entryId) {
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
    toast('恢复失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('recycle-restore', String(id), false);
    renderRecycleBin();
  }
}

function renderMovePathSummary() {
  $('#movePendingCount').textContent = state.movePendingTotal ?? state.moveCandidates.length;
  $('#moveWaitingHashCount').textContent = Number(state.moveWaitingHashCount || 0)
    + Number(state.hashStatus?.scan_candidates?.remaining || 0);
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
    `${totals.tags || (summary.tags || []).length || 0} 个单角色标签`,
    `${totals.characters || (summary.characters || []).length || 0} 个已建角色`,
    `${totals.references || 0} 条参考特征图`,
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
    summaryEl.textContent = state.characterLibraryLoading ? '角色库读取中...' : '角色特征库未加载';
    tagList.innerHTML = '<div class="character-library-empty">暂无可导入的单角色标签</div>';
    characterList.innerHTML = '<div class="character-library-empty">暂无已建特征库角色</div>';
    referenceList.innerHTML = '<div class="character-library-empty">请在中间列表选择角色以查看其参考特征图</div>';
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
  if (!confirm('确定要移除此参考图片吗？')) return;
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
  if (!confirm('确定要删除该角色及其所有参考图吗？后续若扫描到符合规则的单标签作品仍可能重新生成。')) return;
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
    toast(enabled ? '自动整理已开启' : '自动整理已关闭', 'success');
  } catch (e) {
    state.folderRenameAuto = previous || {enabled: !enabled};
    renderFolderRenameAutoStatus();
    toast('自动整理设置失败: ' + (e.message || e), 'error');
  }
}

function archiveSettingsPayload(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value.settings && typeof value.settings === 'object' ? value.settings : value;
}

function archiveProfiles(settings = state.archiveSettings) {
  const profiles = settings && Array.isArray(settings.profiles) ? settings.profiles : [];
  return profiles.filter(profile => profile && typeof profile === 'object');
}

function archiveArtistOverrides(settings = state.archiveSettings) {
  const candidates = [
    settings?.artist_profile_ids,
    settings?.artist_overrides,
    settings?.artist_profiles,
  ];
  return candidates.find(value => value && typeof value === 'object' && !Array.isArray(value)) || {};
}

function archiveCurrentArtistId() {
  return state.currentArtist ? Number(state.currentArtist.id) : 0;
}

function archiveSelectedProfileId(settings = state.archiveSettings) {
  const select = $('#archiveProfileSelect');
  const selected = select ? String(select.value || '') : '';
  if (selected) return selected;
  const artistId = archiveCurrentArtistId();
  const overrides = archiveArtistOverrides(settings);
  return String(
    overrides[String(artistId)] || settings?.default_profile_id || settings?.active_profile_id || ''
  );
}

function archiveProfileById(profileId, settings = state.archiveSettings) {
  return archiveProfiles(settings).find(profile => String(profile.id) === String(profileId)) || null;
}

function archiveProfileCollision(profile) {
  const collision = profile?.collision_strategy || profile?.collision || 'suffix';
  return collision === 'reject' || collision === 'error' ? 'reject' : 'suffix';
}

function archiveProfileTemplate(profile) {
  return String(profile?.template || profile?.path_template || '');
}

function cloneArchiveSettings(settings = state.archiveSettings) {
  const source = archiveSettingsPayload(settings) || {};
  return JSON.parse(JSON.stringify(source));
}

function archivePlanRows(value = state.archivePlans) {
  if (Array.isArray(value)) return value.filter(plan => plan && typeof plan === 'object');
  if (value && Array.isArray(value.plans)) return value.plans.filter(plan => plan && typeof plan === 'object');
  return [];
}

function archivePreviewRows(preview = state.archivePreview) {
  if (!preview || typeof preview !== 'object') return [];
  for (const key of ['plans', 'preview', 'results', 'items']) {
    if (Array.isArray(preview[key])) return preview[key];
  }
  return [];
}

function archivePreviewByPlanId() {
  return new Map(archivePreviewRows().map(row => [Number(row.plan_id ?? row.id), row]));
}

function archiveStatusLabel(status) {
  const labels = {
    draft: '草稿',
    needs_tags: '待补标签',
    manual_review: '待检查',
    ready: '待确认',
    confirmed: '已确认',
    executed: '已执行',
    reverted: '已撤销',
    blocked: '已阻止',
    error: '失败',
  };
  return labels[status] || status || '草稿';
}

function archivePreviewReason(row) {
  if (!row || typeof row !== 'object') return '';
  if (Array.isArray(row.conflicts) && row.conflicts.length) {
    const conflict = row.conflicts[0];
    return archivePreviewReason({
      reason: typeof conflict === 'object' ? (conflict.code || conflict.reason || conflict.message) : conflict,
    });
  }
  const reason = row.reason || row.error || row.message || '';
  const labels = {
    target_exists: '目标已存在',
    duplicate_target: '多个整理项使用同一目标',
    invalid_template: '整理后的文件夹名无效',
    missing_template_value: '整理后的文件夹名缺少可用内容',
    same_as_source: '目标与来源相同',
    target_is_source: '目标与另一整理项来源冲突',
    bad_folder_path: '目标路径无效',
  };
  return labels[reason] || String(reason || '');
}

function archiveProfileIdFromName(name, profiles) {
  const base = String(name || 'custom')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 36) || 'custom';
  const used = new Set(profiles.map(profile => String(profile.id)));
  if (!used.has(base)) return base;
  for (let index = 2; index < 1000; index += 1) {
    const candidate = `${base}-${index}`;
    if (!used.has(candidate)) return candidate;
  }
  return `custom-${Date.now()}`;
}

function archivePlanPayload(plan, target, status) {
  const tagIdsKey = 'selected_' + 'tag_ids';
  const tagIds = typeof plan[tagIdsKey] === 'string'
    ? (() => {
      try { return JSON.parse(plan[tagIdsKey]); } catch (e) { return []; }
    })()
    : (plan[tagIdsKey] || []);
  return {
    source_folder: plan.source_folder || '',
    target_folder: target,
    status,
    [tagIdsKey]: tagIds,
    original_title: plan.original_title || '',
    original_folder_name: plan.original_folder_name || '',
    parsed_date: plan.parsed_date || '',
  };
}

async function loadArchiveWorkbench(options = {}) {
  const artistId = archiveCurrentArtistId();
  const render = options.render !== false;
  const updateState = options.updateState !== false;
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  if (!artistId) {
    if (updateState) {
      state.archiveSettings = null;
      state.archivePlans = [];
      state.archivePreview = null;
    }
    if (render) renderArchiveWorkbench();
    return {settings: null, plans: []};
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
      state.archiveWorkbenchLoading = false;
    }
    if (render) renderArchiveWorkbench();
    return {settings: state.archiveSettings, plans: []};
  }
}

function populateArchiveProfileEditor(profile) {
  const name = $('#archiveProfileNameInput');
  const template = $('#archiveTemplateInput');
  const collision = $('#archiveCollisionSelect');
  if (name) name.value = profile ? String(profile.name || '') : '';
  if (template) template.value = archiveProfileTemplate(profile);
  if (collision) collision.value = archiveProfileCollision(profile);
}

function renderArchiveFolderOptions() {
  const options = $('#archiveFolderOptions');
  if (!options) return;
  const folders = [];
  const visit = node => {
    if (!node || typeof node !== 'object') return;
    const path = String(node.path || '');
    if (path) folders.push(path);
    for (const child of node.children || []) visit(child);
  };
  visit(state.folders);
  options.innerHTML = [...new Set(folders)].sort((left, right) => left.localeCompare(right, undefined, {numeric: true}))
    .map(folder => `<option value="${escHtml(folder)}"></option>`)
    .join('');
}

function renderArchiveWorkbench() {
  const profileSelect = $('#archiveProfileSelect');
  const planList = $('#archivePlanList');
  const ruleStatus = $('#archiveRuleStatus');
  const planSummary = $('#archivePlanSummary');
  const previewSummary = $('#archivePreviewSummary');
  if (!profileSelect || !planList || !ruleStatus || !planSummary || !previewSummary) return;

  const artistId = archiveCurrentArtistId();
  const settings = archiveSettingsPayload(state.archiveSettings);
  const profiles = archiveProfiles(settings);
  const selectedId = archiveSelectedProfileId(settings);
  const selectedProfile = archiveProfileById(selectedId, settings) || profiles[0] || null;
  const overrides = archiveArtistOverrides(settings);
  const overrideEnabled = Boolean(artistId && overrides[String(artistId)]);
  const profileName = $('#archiveProfileNameInput');
  const templateInput = $('#archiveTemplateInput');
  const collisionSelect = $('#archiveCollisionSelect');
  const overrideToggle = $('#archiveArtistOverrideToggle');
  const deleteButton = $('#archiveProfileDeleteBtn');
  const controls = [
    $('#archiveProfileNewBtn'),
    deleteButton,
    $('#archiveProfileSaveBtn'),
    $('#archivePlansRefreshBtn'),
    $('#archivePlansPreviewBtn'),
    $('#archivePlansApplyBtn'),
    $('#archivePlansDryRunBtn'),
    $('#archivePlansExecuteBtn'),
    $('#archivePlanSourceInput'),
    $('#archivePlanCreateForm button'),
  ].filter(Boolean);

  profileSelect.innerHTML = profiles.length
    ? profiles.map(profile => `<option value="${escHtml(String(profile.id))}">${escHtml(String(profile.name || profile.id))}</option>`).join('')
    : '<option value="">没有可用整理方式</option>';
  profileSelect.value = selectedProfile ? String(selectedProfile.id) : '';
  profileSelect.disabled = !artistId || profiles.length === 0;
  for (const control of controls) control.disabled = !artistId;
  if (profileName) profileName.disabled = !artistId || !selectedProfile;
  if (templateInput) templateInput.disabled = !artistId || !selectedProfile;
  if (collisionSelect) collisionSelect.disabled = !artistId || !selectedProfile;
  if (overrideToggle) {
    overrideToggle.disabled = !artistId || !selectedProfile;
    overrideToggle.checked = overrideEnabled;
  }
  if (deleteButton) deleteButton.disabled = !artistId || profiles.length <= 1;
  populateArchiveProfileEditor(selectedProfile);
  renderArchiveFolderOptions();

  if (!artistId) {
    ruleStatus.textContent = '先选择画师';
    planSummary.textContent = '选择画师后读取整理项';
    previewSummary.textContent = '';
    planList.innerHTML = '<div class="move-empty small">选择画师后管理待整理的文件夹</div>';
    return;
  }
  if (settings?.error) {
    ruleStatus.textContent = `读取整理方式失败: ${settings.error}`;
  } else if (!selectedProfile) {
    ruleStatus.textContent = '没有可用整理方式';
  } else {
    const scope = overrideEnabled ? '只用于当前画师' : '所有画师默认使用';
    ruleStatus.textContent = `${scope} \u00b7 ${archiveProfileCollision(selectedProfile) === 'suffix' ? '冲突自动编号' : '冲突保留'}`;
  }

  const plans = archivePlanRows();
  const confirmed = plans.filter(plan => plan.status === 'confirmed').length;
  const ready = plans.filter(plan => plan.status === 'ready').length;
  const executed = plans.filter(plan => plan.status === 'executed').length;
  const reverted = plans.filter(plan => plan.status === 'reverted').length;
  planSummary.textContent = joinUiMeta([
    `${plans.length} 个整理项`,
    `${ready} 待确认`,
    `${confirmed} 已确认`,
    executed ? `${executed} 已执行` : '',
    reverted ? `${reverted} 已撤销` : '',
  ]);

  const previewRows = archivePreviewRows();
  const previewMap = archivePreviewByPlanId();
  if (state.archivePreview?.error) {
    previewSummary.textContent = `预览改名结果失败: ${state.archivePreview.error}`;
  } else if (previewRows.length) {
    const blocked = previewRows.filter(row => row.status === 'blocked' || row.ok === false || row.error || row.reason || (row.conflicts || []).length).length;
    previewSummary.textContent = joinUiMeta([`已检查 ${previewRows.length} 个整理项`, blocked ? `${blocked} 个需处理` : '都可以更新']);
  } else {
    previewSummary.textContent = '';
  }

  planList.innerHTML = plans.length ? plans.map(plan => {
    const planId = Number(plan.id);
    const locked = plan.status === 'executed';
    const undoing = isActionBusy('archive-plan-undo', String(planId));
    const canUndo = plan.status === 'executed' && Number.isFinite(planId) && !undoing;
    const preview = previewMap.get(planId);
    const previewTarget = preview ? String(preview.target_folder || preview.target || '') : '';
    const previewReason = archivePreviewReason(preview);
    const target = previewTarget || String(plan.target_folder || '');
    const previewClass = previewReason ? ' blocked' : (preview ? ' ready' : '');
    const confirmAction = plan.status === 'confirmed' ? 'unconfirm' : 'confirm';
    const confirmLabel = plan.status === 'confirmed' ? '取消确认' : '确认';
    return `
      <div class="archive-plan-row${plan.status === 'executed' ? ' executed' : ''}${plan.status === 'reverted' ? ' reverted' : ''}" data-archive-plan-id="${planId}">
        <div class="archive-plan-row-head">
          <code title="${escHtml(String(plan.source_folder || ''))}">${escHtml(String(plan.source_folder || '-'))}</code>
          <span class="archive-plan-status ${escHtml(String(plan.status || 'draft'))}">${escHtml(archiveStatusLabel(plan.status))}</span>
        </div>
        <div class="archive-plan-target">
          <span>目标</span>
          <input type="text" data-archive-plan-target="${planId}" value="${escHtml(target)}" maxlength="500" ${locked ? 'disabled' : ''}>
          <button class="btn btn-ghost" type="button" data-archive-plan-save="${planId}" ${locked ? 'disabled' : ''}>保存</button>
        </div>
        ${preview ? `<div class="archive-plan-preview${previewClass}">${previewReason ? escHtml(previewReason) : '可以执行'}</div>` : ''}
        <div class="archive-plan-row-actions">
          <span>${plan.file_count ? `${plan.file_count} 项` : ''}</span>
          <div class="archive-plan-row-controls">
            <button class="btn btn-ghost" type="button" data-archive-plan-confirm="${planId}" ${locked || !target ? 'disabled' : ''}>${confirmLabel}</button>
            ${plan.status === 'executed' ? `<button class="btn btn-ops" type="button" data-archive-plan-undo="${planId}" ${canUndo ? '' : 'disabled'} ${undoing ? 'aria-busy="true"' : ''}>${undoing ? '撤销中' : '撤销整理'}</button>` : ''}
          </div>
        </div>
      </div>`;
  }).join('') : '<div class="move-empty small">暂无待整理的文件夹</div>';
}

function archiveSettingsFromEditor() {
  const settings = cloneArchiveSettings();
  settings.profiles = archiveProfiles(settings);
  const selectedId = archiveSelectedProfileId(settings);
  const profile = settings.profiles.find(item => String(item.id) === selectedId);
  if (!profile) throw new Error('没有可保存的整理方式');
  const name = String($('#archiveProfileNameInput')?.value || '').trim();
  const template = String($('#archiveTemplateInput')?.value || '').trim();
  if (!name || !template) throw new Error('名称和整理后的文件夹名不能为空');
  profile.name = name;
  profile.template = template;
  profile.collision_strategy = $('#archiveCollisionSelect')?.value === 'reject' ? 'reject' : 'suffix';
  delete profile.collision;
  const artistId = archiveCurrentArtistId();
  const artistOverrides = archiveArtistOverrides(settings);
  settings.artist_profile_ids = {...artistOverrides};
  if ($('#archiveArtistOverrideToggle')?.checked && artistId) {
    settings.artist_profile_ids[String(artistId)] = selectedId;
  } else if (artistId) {
    delete settings.artist_profile_ids[String(artistId)];
    settings.default_profile_id = selectedId;
  }
  delete settings.active_profile_id;
  delete settings.artist_overrides;
  delete settings.artist_profiles;
  return settings;
}

async function saveArchiveSettings() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-settings-save')) return;
  setActionBusy('archive-settings-save', '', true);
  try {
    const settings = archiveSettingsFromEditor();
    const result = await API.putJson('/api/folder-renames/settings', {settings});
    state.archiveSettings = archiveSettingsPayload(result) || settings;
    state.archivePreview = null;
    renderArchiveWorkbench();
    toast('整理方式已保存', 'success');
  } catch (e) {
    toast('保存整理方式失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-settings-save', '', false);
  }
}

function createArchiveProfile() {
  const settings = cloneArchiveSettings();
  settings.profiles = archiveProfiles(settings);
  const id = archiveProfileIdFromName('custom', settings.profiles);
  settings.profiles.push({
    id,
    name: '自定义整理方式',
    template: '{year}/{date} {tags}',
    collision_strategy: 'suffix',
  });
  settings.default_profile_id = id;
  state.archiveSettings = settings;
  state.archivePreview = null;
  const select = $('#archiveProfileSelect');
  if (select) select.selectedIndex = -1;
  renderArchiveWorkbench();
}

function deleteArchiveProfile() {
  const settings = cloneArchiveSettings();
  const selectedId = archiveSelectedProfileId(settings);
  const profiles = archiveProfiles(settings);
  if (profiles.length <= 1) {
    toast('至少保留一种整理方式', 'error');
    return;
  }
  settings.profiles = profiles.filter(profile => String(profile.id) !== selectedId);
  settings.default_profile_id = String(settings.profiles[0].id);
  const overrides = archiveArtistOverrides(settings);
  settings.artist_profile_ids = {...overrides};
  for (const [artistId, profileId] of Object.entries(settings.artist_profile_ids)) {
    if (String(profileId) === selectedId) delete settings.artist_profile_ids[artistId];
  }
  state.archiveSettings = settings;
  state.archivePreview = null;
  renderArchiveWorkbench();
}

async function previewArchivePlans() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-preview')) return;
  setActionBusy('archive-preview', '', true);
  try {
    const result = await API.postJson('/api/folder-renames/preview', {
      artist_id: artistId,
      profile_id: archiveSelectedProfileId(),
    });
    state.archivePreview = result;
    renderArchiveWorkbench();
  } catch (e) {
    state.archivePreview = {error: e.message || String(e)};
    renderArchiveWorkbench();
    toast('预览改名结果失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-preview', '', false);
  }
}

async function applyArchiveTemplate() {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-apply')) return;
  setActionBusy('archive-apply', '', true);
  try {
    const result = await API.postJson('/api/folder-renames/apply-template', {
      artist_id: artistId,
      profile_id: archiveSelectedProfileId(),
    });
    state.archivePreview = result;
    await loadArchiveWorkbench({keepPreview: true});
    const applied = Number(result.applied || result.updated || 0);
    toast(applied ? `已更新 ${applied} 个整理项` : '没有可更新的整理项', applied ? 'success' : 'info');
  } catch (e) {
    toast('更新整理项失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-apply', '', false);
  }
}

async function createArchivePlan(sourceFolder) {
  const artistId = archiveCurrentArtistId();
  const source = String(sourceFolder || '').trim();
  if (!artistId || !source || isActionBusy('archive-plan-create')) return;
  if (archivePlanRows().some(plan => String(plan.source_folder) === source)) {
    toast('该来源文件夹已在整理清单中', 'error');
    return;
  }
  setActionBusy('archive-plan-create', '', true);
  try {
    await API.putJson('/api/folder-renames', {
      artist_id: artistId,
      plans: [{source_folder: source, target_folder: '', status: 'draft'}],
    });
    const input = $('#archivePlanSourceInput');
    if (input) input.value = '';
    await loadArchiveWorkbench();
    toast('整理项已新增', 'success');
  } catch (e) {
    toast('新增整理项失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-create', '', false);
  }
}

async function saveArchivePlanTarget(planId) {
  const artistId = archiveCurrentArtistId();
  const plan = archivePlanRows().find(item => Number(item.id) === Number(planId));
  const input = $(`[data-archive-plan-target="${Number(planId)}"]`);
  if (!artistId || !plan || !input || isActionBusy('archive-plan-save', String(planId))) return;
  const target = String(input.value || '').trim();
  setActionBusy('archive-plan-save', String(planId), true);
  try {
    await API.putJson('/api/folder-renames', {
      artist_id: artistId,
      plans: [archivePlanPayload(plan, target, target ? 'ready' : 'draft')],
    });
    await loadArchiveWorkbench();
    toast('整理项已保存', 'success');
  } catch (e) {
    toast('保存整理项失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-save', String(planId), false);
  }
}

async function toggleArchivePlanConfirmation(planId) {
  const plan = archivePlanRows().find(item => Number(item.id) === Number(planId));
  if (!plan || isActionBusy('archive-plan-confirm', String(planId))) return;
  const endpoint = plan.status === 'confirmed' ? 'unconfirm' : 'reconfirm';
  setActionBusy('archive-plan-confirm', String(planId), true);
  try {
    await API.post(`/api/folder-renames/plans/${Number(planId)}/${endpoint}`);
    await loadArchiveWorkbench();
    toast(plan.status === 'confirmed' ? '已取消确认' : '整理项已确认', 'success');
  } catch (e) {
    toast('更新整理项失败: ' + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-confirm', String(planId), false);
  }
}

async function undoArchivePlan(planId) {
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
    toast('撤销整理失败: ' + archiveUndoFailureLabel(reason), 'error');
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

async function executeArchivePlans(dryRun) {
  const artistId = archiveCurrentArtistId();
  if (!artistId || isActionBusy('archive-plan-execute')) return;
  if (!dryRun && !window.confirm('确定要执行所有已确认的整理操作吗？\n系统将在执行前自动创建数据库备份，确保数据安全。')) return;
  setActionBusy('archive-plan-execute', '', true);
  try {
    const result = await API.postJson('/api/folder-renames/execute', {artist_id: artistId, dry_run: Boolean(dryRun)});
    state.archivePreview = {results: result.results || [], execution: result};
    await loadArchiveWorkbench({keepPreview: true});
    const rows = result.results || [];
    const successful = rows.filter(row => row.status === (dryRun ? 'dry_run' : 'executed')).length;
    toast(dryRun ? `检查完成: ${successful} 个整理项可执行` : `整理完成: ${successful} 个整理项`, successful ? 'success' : 'info');
  } catch (e) {
    toast((dryRun ? '执行检查失败: ' : '整理操作失败: ') + (e.message || e), 'error');
  } finally {
    setActionBusy('archive-plan-execute', '', false);
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
    const kindClass = operation.kind === 'folder_rename_undo' || operation.reason === 'folder_rename_undo' || operation.reason === 'undo'
      ? 'undo'
      : (operation.kind === 'folder_rename' ? 'rename' : (operation.kind === 'move' ? 'move' : 'other'));
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
  if (!isHealthObject(scan) || (!scan.status && !scan.phase)) return '扫描状态就绪';
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

function renderHealthSummary() {
  const grid = $('#healthGrid');
  if (!grid) return;
  const health = state.healthSummary;
  if (!health) {
    grid.innerHTML = '<div class="maintenance-card status-card status-muted move-empty small">系统健康状态读取中...</div>';
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
  const folderArchive = health.folder_archive;
  const hash = health.hash;
  const errors = health.recent_errors;
  const degradedReasons = Array.isArray(health.degraded_reasons) ? health.degraded_reasons : [];
  const databaseKnown = isHealthObject(database);
  const backupsKnown = isHealthObject(backups);
  const folderArchiveKnown = isHealthObject(folderArchive);
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
    database.reclaimable_bytes != null ? `可回收空间 ${formatBytes(database.reclaimable_bytes)}` : '可回收空间未上报',
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
          ? [latestBackup.name || '最近备份', `自动保留 ${backupRetainedCount ?? backupCount} 份`, backupTotalBytes != null ? `占用 ${formatBytes(backupTotalBytes)}` : '备份占用未上报']
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
      ? errors.slice(0, 3).map(row => `<code>${escHtml(row.source || '')}: ${escHtml(row.line || '')}</code>`).join('')
      : '<span>最近没有错误记录</span>';
  grid.innerHTML = `
    <div class="maintenance-card status-card ${overallStatus}"><b>整体状态</b><span>${escHtml(overallText)}</span></div>
    <div class="maintenance-card status-card ${databaseStatus}"><b>数据库</b><span>${escHtml(joinUiMeta(databaseText))}</span></div>
    <div class="maintenance-card status-card ${backupStatus}"><b>最近备份</b><span>${escHtml(joinUiMeta(backupText))}</span><span class="health-schedule">${escHtml(formatHealthSchedule(backupSchedule, 'next_run_at'))}</span></div>
    <div class="maintenance-card status-card ${scanStatus}"><b>扫描</b><span>${escHtml(formatHealthScanStatus(scan))}</span><span class="health-schedule">${escHtml(formatHealthSchedule(scanSchedule, 'next_auto_scan_at'))}</span></div>
    <div class="maintenance-card status-card ${hashStatus}"><b>相同文件检查</b><span>${escHtml(formatHealthHashStatus(hash))}</span></div>
    <div class="maintenance-card status-card ${archiveStatus}"><b>文件夹整理</b><span>${escHtml(archiveFailedPlans == null ? '状态未上报' : (archiveFailedPlans ? `整理失败 ${archiveFailedPlans}` : '无失败计划'))}</span></div>
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
    const addedAsNew = Number(result.added_as_new || 0);
    const stale = Number(result.stale || 0);
    const skipped = Number(result.skipped || 0);
    const remaining = Number(result.remaining || 0);
    if (!silent) {
      toast(`安全处理 ${applied + resolved + addedAsNew + stale} 项，新增独立文件 ${addedAsNew} 项，已在库中 ${resolved} 项，清理陈旧 ${stale} 项，跳过 ${skipped} 项，剩余 ${remaining} 项`, (applied || resolved || addedAsNew || stale) ? 'success' : 'info');
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
