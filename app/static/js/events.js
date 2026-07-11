function applyMode(mode) {
  const fromMode = state.mode;
  const seq = nextRequestSeq('modeSwitchSeq');
  const capturedAnchor = captureGridScrollAnchor();
  const gridScrollAnchor = state.modeSwitchAnchor || capturedAnchor;
  state.modeSwitchAnchor = gridScrollAnchor;
  state.mode = mode;
  state.selectedIds.clear();
  closeFilterDrawer();
  closeMobileHeaderToolsIfMobile();
  updateEditBar();
  const isMoves = mode === 'moves';
  document.body.classList.toggle('mode-moves', isMoves);
  document.body.classList.toggle('mode-edit', mode === 'edit');
  document.body.classList.toggle('mode-browse', mode === 'browse' || !mode);
  $('#movePanel').classList.toggle('visible', isMoves);
  $('#gridContainer').style.display = isMoves ? 'none' : '';
  $('.toolbar').style.display = isMoves ? 'none' : '';
  $('#searchInput').disabled = isMoves;
  $('#searchOptionsBtn').disabled = isMoves;
  if (isMoves) closeSearchOptions();
  updateDuplicateFilesButton();
  renderLibraryEmptyState();
  if (isMoves) {
    setMaintenanceView(state.maintenanceView || 'overview');
    loadMoveWorkbench();
    startMaintenanceAutoRefresh();
  } else {
    stopMaintenanceAutoRefresh();
    renderSidebar();
    loadItems().then(() => {
      const restoreResult = restoreGridScrollAnchor(gridScrollAnchor, {seq});
      if (isCurrentRequestSeq('modeSwitchSeq', seq)) state.modeSwitchAnchor = null;
      logModeChangeLayout({
        from_mode: fromMode,
        to_mode: mode,
        seq,
        restore: restoreResult,
      });
    }).catch(e => {
      if (isCurrentRequestSeq('modeSwitchSeq', seq)) state.modeSwitchAnchor = null;
      logUiAction('mode_change', collectUiLogContext({
        from_mode: fromMode,
        to_mode: mode,
        seq,
        error: e.message || String(e),
      }));
    });
  }
}

function bindEvents() {
  $('#mobileHeaderToggle').addEventListener('click', toggleMobileHeaderTools);
  syncMobileHeaderTools();
  syncSearchOptionsControl();
  $('#mobileFilterBtn').addEventListener('click', openFilterDrawer);
  $('#filterBackdrop').addEventListener('click', closeFilterDrawer);
  $('#filterDrawerClose').addEventListener('click', closeFilterDrawer);
  bindSidebarResize();
  bindMobileColumnToggle();
  bindLightboxVideoDiagnostics();
  window.addEventListener('resize', () => {
    setSidebarWidth(state.sidebarWidth, false);
    if (typeof onViewportLayoutChange === 'function') {
      onViewportLayoutChange();
    } else if (!isMobileViewport()) {
      closeFilterDrawer();
      closeMobileHeaderTools();
    }
  });
  $('#artistSearch').addEventListener('focus', e => {
    e.target.select();
    renderArtistDropdown('');
  });
  $('#artistSearch').addEventListener('input', e => {
    renderArtistDropdown(e.target.value);
  });
  $('#artistSearch').addEventListener('keydown', e => {
    if (e.key === 'Enter') {
      e.preventDefault();
      selectFirstArtistResult();
    } else if (e.key === 'Escape') {
      closeArtistDropdown();
    }
  });
  $('#duplicateToggle').addEventListener('click', () => {
    state.duplicateFoldersOpen = !state.duplicateFoldersOpen;
    renderDuplicateFolders();
  });
  $('#searchInput').addEventListener('input', debounce(e => {
    state.search = e.target.value;
    logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
    state.selectedIds.clear();
    updateEditBar();
    scrollToItemsTop();
    loadItems();
  }, 300));
  $('#searchOptionsBtn').addEventListener('click', e => {
    e.stopPropagation();
    toggleSearchOptions();
  });
  $$('#searchOptionsMenu [data-search-scope]').forEach(btn => {
    btn.addEventListener('click', e => {
      e.stopPropagation();
      setSearchScope(btn.dataset.searchScope);
      logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
      state.selectedIds.clear();
      updateEditBar();
      scrollToItemsTop();
      loadItems();
    });
  });
  $('#tagsOnlyToggle').addEventListener('change', e => {
    setSearchTarget(e.target.checked ? 'tags' : 'all');
    logUiAction('search_change', {search: state.search, scope: state.searchScope, target: state.searchTarget});
    state.selectedIds.clear();
    updateEditBar();
    scrollToItemsTop();
    loadItems();
  });
  $('#duplicateFilesBtn').addEventListener('click', () => {
    state.duplicatesOnly = !state.duplicatesOnly;
    updateDuplicateFilesButton();
    state.selectedIds.clear();
    updateEditBar();
    scrollToItemsTop();
    loadItems();
  });
  const gridContainer = $('#gridContainer');
  if (gridContainer) gridContainer.addEventListener('scroll', maybeLoadMoreOnScroll, {passive: true});
  window.addEventListener('scroll', maybeLoadMoreOnScroll, {passive: true});
  $('#scanFolderBtn').addEventListener('click', async () => {
    if (!state.currentArtist || !isCurrentScanScopeActive() || isActionBusy('scan-context')) return;
    setActionBusy('scan-context', '', true);
    const isFolderScan = Boolean(state.activeFolder);
    const params = new URLSearchParams();
    params.set('artist_id', state.currentArtist.id);
    if (state.activeFolder) params.set('folder', state.activeFolder);
    try {
      const r = await API.post('/api/scan/folder?' + params.toString());
      if (r.ok) toast(isFolderScan ? '文件夹扫描已启动' : '画师扫描已启动', 'success');
      else toast(r.message || '扫描已在运行', 'error');
    } catch (e) {
      toast(isFolderScan ? '启动文件夹扫描失败' : '启动画师扫描失败', 'error');
    } finally {
      setActionBusy('scan-context', '', false);
    }
  });

  $$('.mode-tabs button').forEach(btn => {
    btn.addEventListener('click', () => {
      $$('.mode-tabs button').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      $$('.mode-tabs button').forEach(b => b.setAttribute('aria-pressed', b === btn ? 'true' : 'false'));
      applyMode(btn.dataset.mode);
    });
  });

  $$('#desktopViewToggle button').forEach(btn => {
    btn.addEventListener('click', () => {
      $$('#desktopViewToggle button').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      $$('#desktopViewToggle button').forEach(b => b.setAttribute('aria-pressed', b === btn ? 'true' : 'false'));
      state.view = btn.dataset.view;
      renderGrid();
    });
  });

  $('#scanBtn').addEventListener('click', startFullScan);
  $('#emptyScanBtn').addEventListener('click', startFullScan);
  const emptySelectArtistBtn = $('#emptySelectArtistBtn');
  if (emptySelectArtistBtn) {
    emptySelectArtistBtn.addEventListener('click', e => {
      // Keep the open dropdown from the same click; document click would
      // otherwise see a target outside #artistPicker and close it immediately.
      e.preventDefault();
      e.stopPropagation();
      focusArtistPicker();
    });
  }

  $('#stopScanBtn').addEventListener('click', async () => {
    if (isActionBusy('scan-stop')) return;
    setActionBusy('scan-stop', '', true);
    try {
      const r = await API.post('/api/scan/stop');
      if (r.ok) toast('正在停止扫描', 'success');
      else toast(r.message || '停止失败', 'error');
    } catch (e) {
      toast('停止失败', 'error');
    } finally {
      setActionBusy('scan-stop', '', false);
    }
  });

  $('#moveRefreshBtn').addEventListener('click', () => loadMoveWorkbench({preserveScroll: true}));
  $('#moveAutoResolveBtn').addEventListener('click', autoResolveMoveCandidates);
  const folderRenameAutoToggle = $('#folderRenameAutoExecuteToggle');
  if (folderRenameAutoToggle) {
    folderRenameAutoToggle.addEventListener('change', e => {
      setFolderRenameAutoEnabled(e.target.checked);
    });
  }
  $('#characterImportCurrentArtistBtn').addEventListener('click', () => {
    if (!state.currentArtist) {
      toast('先选择画师', 'error');
      return;
    }
    importCharacterLibraryReferences({
      scope: 'artist',
      body: {artist_id: state.currentArtist.id, limit_per_tag: 3},
    });
  });
  $('#characterImportAllBtn').addEventListener('click', () => {
    importCharacterLibraryReferences({
      scope: 'all',
      body: {limit_per_tag: 3},
    });
  });
  $('#characterRebuildIndexBtn').addEventListener('click', rebuildCharacterIndex);
  const characterTagImportList = $('#characterTagImportList');
  if (characterTagImportList) {
    characterTagImportList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const cancelBtn = target ? target.closest('[data-character-import-cancel]') : null;
      if (cancelBtn && characterTagImportList.contains(cancelBtn)) {
        cancelCharacterImportJob(cancelBtn.dataset.characterImportCancel);
        return;
      }
    });
  }
  const characterList = $('#characterList');
  if (characterList) {
    characterList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const deleteBtn = target ? target.closest('[data-character-delete]') : null;
      if (deleteBtn && characterList.contains(deleteBtn)) {
        deleteCharacter(Number(deleteBtn.dataset.characterDelete));
        return;
      }
      const btn = target ? target.closest('[data-character-select]') : null;
      if (!btn || !characterList.contains(btn)) return;
      const characterId = Number(btn.dataset.characterSelect);
      if (!characterId) return;
      state.characterLibrarySelectedCharacterId = characterId;
      loadCharacterLibrary({characterId});
    });
  }
  const characterReferenceList = $('#characterReferenceList');
  if (characterReferenceList) {
    characterReferenceList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-character-reference-delete]') : null;
      if (!btn || !characterReferenceList.contains(btn)) return;
      deleteCharacterReference(Number(btn.dataset.characterId), Number(btn.dataset.characterReferenceDelete));
    });
  }
  const maintenanceTabs = $('.maintenance-view-tabs');
  if (maintenanceTabs) {
    maintenanceTabs.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-maintenance-view]') : null;
      if (!btn || !maintenanceTabs.contains(btn)) return;
      setMaintenanceView(btn.dataset.maintenanceView);
      if (state.mode === 'moves') {
        refreshActiveMaintenanceView({preserveScroll: true, reason: 'tab'});
      }
    });
  }
  const overviewActions = $('#overviewActionCards');
  if (overviewActions) {
    overviewActions.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const card = target ? target.closest('[data-maintenance-jump]') : null;
      if (!card || !overviewActions.contains(card)) return;
      handleMaintenanceJump(card.dataset.maintenanceJump);
    });
  }

  document.addEventListener('visibilitychange', () => {
    if (state.mode === 'moves' && !document.hidden) {
      refreshActiveMaintenanceView({preserveScroll: true, reason: 'visible'}).catch(e => {
        if (!isAbortError(e)) toast('维护页面刷新失败: ' + (e.message || e), 'error');
      });
      scheduleMaintenanceAutoRefresh();
    }
  });

  $('#manualBackupBtn').addEventListener('click', async () => {
    if (isActionBusy('backup-manual')) return;
    setActionBusy('backup-manual', '', true);
    const btn = $('#manualBackupBtn');
    const result = $('#backupResult');
    btn.disabled = true;
    btn.textContent = '备份中';
    result.textContent = '';
    result.style.color = '';
    try {
      const r = await API.post('/api/backup');
      result.textContent = r.ok ? '备份完成' : '备份失败';
      result.style.color = r.ok ? 'var(--status-ok)' : 'var(--status-danger)';
      await loadHealthSummary();
    } catch (e) {
      result.textContent = '备份失败: ' + (e.message || e);
      result.style.color = 'var(--status-danger)';
    } finally {
      btn.disabled = false;
      btn.textContent = '立即备份数据库';
      setActionBusy('backup-manual', '', false);
    }
  });

  $('#editApplyBtn').addEventListener('click', async () => {
    await ensureEditTagContext();
    await selectOrCreateEditTagQuery();
    const tagIds = selectedEditTagIds();
    const tagNames = selectedEditTagNames(tagIds);
    logUiAction('edit_apply_click', {
      selected_count: state.selectedIds.size,
      item_ids: [...state.selectedIds],
      folder: state.activeFolder || '',
      artist_id: currentEditArtistId(),
      mode: 'add',
      tag_ids: tagIds,
      tag_names: tagNames,
    });
    if (tagIds.length === 0 && tagNames.length === 0) { toast('请选择标签', 'error'); return; }
    if (state.selectedIds.size > 0) {
      classifyItems([...state.selectedIds], tagIds, 'add');
      return;
    }
    if (state.activeFolder) {
      classifyFolder(state.activeFolder, tagIds, 'add');
      return;
    }
    toast('请选择图片或文件夹', 'error');
  });

  $('#editSelectAllBtn').addEventListener('click', () => {
    applySelectionChange((state.allItems || []).filter(isTaggableItem).map(item => item.id), {reason: 'select_all'});
  });

  $('#editDeleteSelectedBtn').addEventListener('click', () => deleteSelectedMediaItems());

  $('#editTagSearch').addEventListener('focus', e => {
    state.editTagQuery = e.target.value;
    openEditTagPicker();
  });
  $('#editTagSearch').addEventListener('input', e => {
    state.editTagQuery = e.target.value;
    openEditTagPicker();
  });
  $('#editTagSearch').addEventListener('keydown', e => {
    if (e.key === 'Enter') {
      e.preventDefault();
      selectFirstEditTagResult();
    } else if (e.key === 'Escape') {
      closeEditTagPicker();
    }
  });

  $('#editDeleteRoleBtn').addEventListener('click', removeSelectedTagsFromItems);
  const characterSuggestionsList = $('#characterSuggestionsList');
  if (characterSuggestionsList) {
    characterSuggestionsList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-character-suggestion-tag]') : null;
      if (!btn || !characterSuggestionsList.contains(btn)) return;
      selectCharacterSuggestionTag((btn.dataset.characterSuggestionTag || '').trim());
    });
  }
  const artistSuggestionsList = $('#artistSuggestionsList');
  if (artistSuggestionsList) {
    artistSuggestionsList.addEventListener('click', e => {
      const target = e.target instanceof Element ? e.target : null;
      const btn = target ? target.closest('[data-artist-suggestion-id]') : null;
      if (!btn || !artistSuggestionsList.contains(btn)) return;
      confirmArtistSuggestion(btn.dataset.artistSuggestionItem, btn.dataset.artistSuggestionId);
    });
  }

  $('#editCancelBtn').addEventListener('click', () => {
    applySelectionChange([], {reason: 'cancel_selection'});
  });

  document.addEventListener('click', e => {
    if (!$('#editTagPicker').contains(e.target)) {
      closeEditTagPicker();
    }
    const artistPicker = $('#artistPicker');
    const emptySelectArtist = $('#emptySelectArtistBtn');
    const clickedArtistChrome = Boolean(
      (artistPicker && artistPicker.contains(e.target))
      || (emptySelectArtist && emptySelectArtist.contains(e.target))
    );
    if (!clickedArtistChrome) {
      closeArtistDropdown();
    }
    if (!$('#searchControl').contains(e.target)) {
      closeSearchOptions();
    }
  });
  document.addEventListener('keydown', e => {
    if (e.key === 'Control' || e.key === 'Meta') state.selectionModifierDown = true;
    if (e.key === 'Escape' && closeTopmostOverlay()) {
      e.preventDefault();
    }
    // Focus trap for open filter drawer / lightbox dialogs.
    if (e.key === 'Tab') {
      const trapRoot = state.filterDrawerOpen
        ? $('#filterSidebar')
        : ($('#lightbox')?.style.display === 'flex' ? $('#lightbox') : null);
      if (trapRoot) {
        const focusables = [...trapRoot.querySelectorAll(
          'a[href],button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])'
        )].filter(el => !el.hasAttribute('inert') && el.offsetParent !== null);
        if (focusables.length) {
          const first = focusables[0];
          const last = focusables[focusables.length - 1];
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault();
            last.focus();
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault();
            first.focus();
          } else if (!trapRoot.contains(document.activeElement)) {
            e.preventDefault();
            first.focus();
          }
        }
      }
    }
  });
  document.addEventListener('keyup', e => {
    if (e.key === 'Control' || e.key === 'Meta') state.selectionModifierDown = e.ctrlKey || e.metaKey;
  });
  window.addEventListener('blur', () => {
    state.selectionModifierDown = false;
  });

  $('#lightbox').addEventListener('click', e => {
    const eventTarget = e.target;
    const closeButton = eventTarget instanceof Element ? eventTarget.closest('.close') : null;
    if (eventTarget === $('#lightbox') || eventTarget === $('#lightboxStage') || (closeButton && $('#lightbox').contains(closeButton))) closeLightbox();
  });
  $('#lightbox').addEventListener('wheel', onLightboxWheel, {passive: false});
  const lightboxImg = $('#lightboxImg');
  lightboxImg.addEventListener('pointerdown', startLightboxPan);
  lightboxImg.addEventListener('pointermove', moveLightboxPan);
  lightboxImg.addEventListener('pointerup', stopLightboxPan);
  lightboxImg.addEventListener('pointercancel', stopLightboxPan);
  $('#lightboxDownloadBtn').addEventListener('click', e => {
    e.stopPropagation();
  });
  const deleteBtn = $('#lightboxDeleteBtn');
  if (deleteBtn) {
    deleteBtn.addEventListener('click', e => {
      e.stopPropagation();
      onLightboxDelete(deleteBtn);
    });
  }
  $('#lightbox .prev').addEventListener('click', e => {
    e.stopPropagation();
    moveLightbox(-1);
  });
  $('#lightbox .next').addEventListener('click', e => {
    e.stopPropagation();
    moveLightbox(1);
  });
}

function closeTopmostOverlay() {
  if ($('#lightbox').style.display === 'flex') {
    closeLightbox();
    return true;
  }
  if (state.filterDrawerOpen) {
    closeFilterDrawer();
    return true;
  }
  if ($('#editTagPicker').classList.contains('open')) {
    closeEditTagPicker();
    return true;
  }
  if ($('#artistDropdown').classList.contains('open')) {
    closeArtistDropdown();
    return true;
  }
  if (state.searchOptionsOpen) {
    closeSearchOptions();
    return true;
  }
  if (state.mobileHeaderToolsOpen) {
    closeMobileHeaderTools();
    return true;
  }
  return false;
}

function isTerminalScanState(s) {
  return s && s.status === 'idle' && ['complete', 'stopped', 'interrupted'].includes(s.phase);
}

function connectWS() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/scan`);
  ws.onmessage = e => {
    let s = {};
    try {
      s = JSON.parse(e.data);
    } catch (err) {
      return;
    }
    if (!s || typeof s !== 'object') return;
    const wasScanning = state.lastScanState && state.lastScanState.status === 'scanning';
    state.lastScanState = s;
    state.scanRunning = s.status === 'scanning';
    const scanTerminal = isTerminalScanState(s);
    const scanJustFinished = wasScanning && scanTerminal;
    if (s.status === 'scanning') {
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
      if (scanJustFinished) {
        refreshAfterScan();
      }
    }
  };
  ws.onclose = () => setTimeout(connectWS, 3000);
}

function showProgress(s) {
  const panel = $('#progressPanel');
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
  $('#progressPanel').classList.remove('visible');
}

async function refreshAfterScan() {
  const seq = await refreshCurrentView({reason: 'scan_complete'});
  if (isCurrentRequestSeq('scanRefreshSeq', seq)) toast('扫描完成', 'success');
}

function captureGridScrollAnchor() {
  const container = $('#gridContainer');
  if (!container) return null;
  const containerRect = container.getBoundingClientRect();
  const cards = [...$$('#grid .card[data-id]')];
  const fullyVisible = cards.find(card => {
    const rect = card.getBoundingClientRect();
    return rect.top >= containerRect.top && rect.bottom <= containerRect.bottom;
  });
  const partiallyVisible = cards.find(card => {
    const rect = card.getBoundingClientRect();
    return rect.bottom > containerRect.top && rect.top < containerRect.bottom;
  });
  const firstVisible = fullyVisible || partiallyVisible || cards[0] || null;
  const documentScroller = document.scrollingElement || document.documentElement;
  const containerScrollable = container.scrollHeight > container.clientHeight + 1;
  const actualScrollSource = containerScrollable ? 'grid' : 'document';
  const scrollTarget = actualScrollSource === 'grid' ? container : documentScroller;
  const editBar = $('#editBar');
  if (!firstVisible) {
    return {
      id: null,
      nextIds: [],
      orderedIds: [],
      visibleIndex: -1,
      viewportTop: null,
      offset: 0,
      fallbackScrollTop: scrollTarget ? scrollTarget.scrollTop : 0,
      gridScrollTop: container.scrollTop,
      edit_bar_height: editBar ? Math.round(editBar.getBoundingClientRect().height) : 0,
      actualScrollSource,
    };
  }
  const visibleIndex = cards.indexOf(firstVisible);
  const firstVisibleRect = firstVisible.getBoundingClientRect();
  return {
    id: firstVisible.dataset.id,
    orderedIds: cards.map(card => card.dataset.id).filter(Boolean),
    visibleIndex,
    viewportTop: firstVisibleRect.top,
    offset: firstVisibleRect.top - containerRect.top,
    fallbackScrollTop: scrollTarget ? scrollTarget.scrollTop : container.scrollTop,
    gridScrollTop: container.scrollTop,
    edit_bar_height: editBar ? Math.round(editBar.getBoundingClientRect().height) : 0,
    actualScrollSource,
  };
}

function restoreGridScrollAnchor(anchor, options = {}) {
  const container = $('#gridContainer');
  const seq = options.seq;
  if (seq != null && !isCurrentRequestSeq('modeSwitchSeq', seq)) return {restored: false, stale: true};
  if (!anchor || !container) return {restored: false, missing_anchor: true};
  const cards = [...$$('#grid .card[data-id]')];
  const cardsById = new Map(cards.map(card => [String(card.dataset.id), card]));
  let target = anchor.id ? cards.find(card => String(card.dataset.id) === String(anchor.id)) : null;
  if (!target) {
    const oldIds = (anchor.orderedIds || []).map(id => String(id));
    const originalIndex = Math.max(0, Number.isFinite(anchor.visibleIndex) ? anchor.visibleIndex : oldIds.indexOf(String(anchor.id)));
    const fallbackId = oldIds.slice(originalIndex + 1).find(id => cardsById.has(id));
    target = fallbackId ? cardsById.get(fallbackId) : null;
  }
  const documentScroller = document.scrollingElement || document.documentElement;
  const scrollTarget = anchor.actualScrollSource === 'document' ? documentScroller : container;
  const maxScrollTop = Math.max(0, scrollTarget.scrollHeight - scrollTarget.clientHeight);
  if (target) {
    const containerRect = container.getBoundingClientRect();
    const beforeTop = Number.isFinite(anchor.viewportTop) ? anchor.viewportTop : null;
    const targetRect = target.getBoundingClientRect();
    const topDelta = Number.isFinite(beforeTop)
      ? targetRect.top - anchor.viewportTop
      : targetRect.top - containerRect.top - anchor.offset;
    const beforeScrollTop = scrollTarget.scrollTop;
    scrollTarget.scrollTop = Math.max(0, Math.min(scrollTarget.scrollTop + topDelta, maxScrollTop));
    const afterRect = target.getBoundingClientRect();
    return {
      restored: true,
      id: target.dataset.id ? Number(target.dataset.id) : null,
      first_visible_id: target.dataset.id ? Number(target.dataset.id) : null,
      before_top: beforeTop,
      after_top: afterRect.top,
      top_delta: beforeTop == null ? null : afterRect.top - beforeTop,
      requested_delta: topDelta,
      applied_scroll_delta: scrollTarget.scrollTop - beforeScrollTop,
      scroll_source: scrollTarget === document.scrollingElement ? 'document' : 'grid',
      grid_scroll_top: Math.round(container.scrollTop),
      edit_bar_height: $('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0,
    };
  }
  scrollTarget.scrollTop = Math.max(0, Math.min(anchor.fallbackScrollTop || 0, maxScrollTop));
  return {
    restored: false,
    fallback: true,
    first_visible_id: null,
    before_top: Number.isFinite(anchor.viewportTop) ? anchor.viewportTop : null,
    after_top: null,
    top_delta: null,
    scroll_source: scrollTarget === document.scrollingElement ? 'document' : 'grid',
    grid_scroll_top: Math.round(container.scrollTop),
    edit_bar_height: $('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0,
  };
}

function logModeChangeLayout(data = {}) {
  const restore = data.restore || {};
  logUiAction('mode_change', collectUiLogContext({
    from_mode: data.from_mode || '',
    to_mode: data.to_mode || state.mode,
    seq: data.seq ?? null,
    first_visible_id: restore.first_visible_id ?? null,
    before_top: restore.before_top == null ? null : Math.round(restore.before_top),
    after_top: restore.after_top == null ? null : Math.round(restore.after_top),
    top_delta: restore.top_delta == null ? null : Math.round(restore.top_delta),
    grid_scroll_top: restore.grid_scroll_top ?? ($('#gridContainer') ? Math.round($('#gridContainer').scrollTop) : 0),
    edit_bar_height: restore.edit_bar_height ?? ($('#editBar') ? Math.round($('#editBar').getBoundingClientRect().height) : 0),
    restored: Boolean(restore.restored),
    stale: Boolean(restore.stale),
    scroll_source: restore.scroll_source || '',
  }));
}

async function refreshCurrentView({reason = 'manual'} = {}) {
  const currentArtistId = state.currentArtist ? state.currentArtist.id : null;
  const hadNoArtistsBeforeRefresh = state.artists.length === 0;
  const activeFolder = state.activeFolder;
  const currentMode = state.mode;
  const maintenanceView = state.maintenanceView;
  const gridScrollAnchor = captureGridScrollAnchor();
  const seq = nextRequestSeq('scanRefreshSeq');
  await loadArtists();
  if (!isCurrentRequestSeq('scanRefreshSeq', seq)) return seq;
  if (currentMode === 'moves' || state.mode === 'moves') {
    setMaintenanceView(maintenanceView || 'overview');
    await loadMoveWorkbench({preserveScroll: true});
    logUiAction('refresh_current_view', {reason, mode: 'moves'});
    return seq;
  }
  const shouldAutoSelectFirstScannedArtist =
    reason === 'scan_complete' &&
    !currentArtistId &&
    hadNoArtistsBeforeRefresh &&
    state.artists.length > 0;
  if (shouldAutoSelectFirstScannedArtist) {
    await selectArtist(state.artists[0].id);
    logUiAction('refresh_current_view', {reason, auto_selected_artist: true});
    return seq;
  }
  if (currentArtistId) {
    state.currentArtist = state.artists.find(a => a.id === currentArtistId) || null;
    if (!state.currentArtist) {
      clearUI();
      logUiAction('refresh_current_view', {reason, artist_missing: true});
      return seq;
    }
    state.activeFolder = activeFolder;
    const [stats, tags, folders] = await Promise.all([
      API.get(`/api/artists/${currentArtistId}/stats`),
      API.get(`/api/tags?artist_id=${currentArtistId}`),
      API.get(`/api/folders?artist_id=${currentArtistId}`),
    ]);
    if (!isCurrentRequestSeq('scanRefreshSeq', seq)) return seq;
    state.stats = stats;
    state.tags = tags;
    state.folders = folders;
    renderSidebar();
    renderFolderTree();
    renderEditTagPicker();
    renderToolbar();
    await loadItems();
    restoreGridScrollAnchor(gridScrollAnchor);
  }
  logUiAction('refresh_current_view', {reason});
  return seq;
}

function toast(msg, type) {
  const el = document.createElement('div');
  el.className = `toast ${type}`;
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(() => el.remove(), 2500);
}

function escHtml(s) {
  if (!s) return '';
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

function formatSize(bytes) {
  if (!bytes) return '0 B';
  const units = ['B','KB','MB','GB'];
  let i = 0;
  let s = bytes;
  while (s >= 1024 && i < units.length - 1) { s /= 1024; i++; }
  return s.toFixed(1) + ' ' + units[i];
}

function renderTagNames(tags) {
  if (!tags || tags.length === 0) return '未加标签';
  return joinUiMeta(tags.map(t => escHtml(t.name)));
}

function downloadFileName(item) {
  return (item.file_name || 'image').replace(/[\\/:*?"<>|]/g, '_');
}

async function copyText(text) {
  if (!text) return false;
  if (navigator.clipboard && navigator.clipboard.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (e) {}
  }
  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.setAttribute('readonly', '');
  textarea.style.position = 'fixed';
  textarea.style.left = '-9999px';
  document.body.appendChild(textarea);
  textarea.select();
  try {
    return document.execCommand('copy');
  } catch (e) {
    return false;
  } finally {
    textarea.remove();
  }
}

function debounce(fn, ms) {
  let timer;
  return (...args) => { clearTimeout(timer); timer = setTimeout(() => fn(...args), ms); };
}

installFrontendErrorLogging();
init();
