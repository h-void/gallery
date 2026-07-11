const SELECTION_MARQUEE_THRESHOLD_PX = 4;

function syncSelectedCards(root = $('#grid')) {
  const scope = root || document;
  if (!scope.querySelectorAll) return;
  scope.querySelectorAll('.card[data-id]').forEach(card => {
    const cid = Number(card.dataset.id);
    const selected = state.selectedIds.has(cid);
    card.classList.toggle('selected', selected);
    const chk = card.querySelector('.check');
    if (chk) chk.classList.toggle('checked', selected);
  });
}

function renderGrid() {
  const grid = $('#grid');
  if (!grid) return;
  const items = state.allItems;
  const tagResultsHtml = renderTagSearchResults();
  const globalSearch = typeof isGlobalSearchActive === 'function' && isGlobalSearchActive();

  if (!state.currentArtist && !globalSearch) {
    releaseAllImageLoads();
    releaseAllVideoPreviews();
    grid.innerHTML = '';
    if (typeof renderLibraryEmptyState === 'function') renderLibraryEmptyState();
    return;
  }

  if (items.length === 0 && !tagResultsHtml) {
    releaseAllImageLoads();
    releaseAllVideoPreviews();
    grid.innerHTML = state.duplicatesOnly
      ? '<div class="empty">当前范围没有重复文件</div>'
      : '<div class="empty">当前范围没有文件</div>';
    return;
  }

  grid.className = 'grid';
  if (state.view === 'compact') grid.classList.add('compact');
  if (state.view === 'list') grid.classList.add('list');

  const html = tagResultsHtml + renderItemCards(items, 0);

  releaseAllImageLoads();
  releaseAllVideoPreviews();
  grid.innerHTML = html;
  syncSelectedCards(grid);
  bindGridEvents();
  observeImages();
  bindGifPreviewEvents();
  bindVideoPreviewEvents();
  observeVideoPreviews();
}

function appendItemsToGrid(items, startIndex) {
  if (!items.length) return;
  const grid = $('#grid');
  grid.insertAdjacentHTML('beforeend', renderItemCards(items, startIndex));
  syncSelectedCards(grid);
  bindGridEvents();
  observeImages();
  bindGifPreviewEvents();
  bindVideoPreviewEvents();
  observeVideoPreviews();
}

function renderRecognizedDatePreview(item) {
  const date = String(item.date || '').trim();
  if (!/^\d{4}-\d{2}(-\d{2})?$/.test(date)) return '';
  return escHtml(date);
}

function renderItemCards(items, startIndex = 0) {
  let html = '';
  asArray(items).forEach((item, offset) => {
    const idx = startIndex + offset;
    const sel = state.selectedIds.has(item.id) ? ' selected' : '';
    const chk = state.selectedIds.has(item.id) ? ' checked' : '';

    const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
    const isArchive = mediaType === 'archive' || item.is_archive;
    const fileUrl = API.fileUrl(item.file_path, fileVersionParam(item));
    const previewFileUrl = API.previewUrl(item.file_path, fileVersionParam(item), IMAGE_PREVIEW_MAX_EDGE);
    const checkVisible = state.mode === 'edit' ? ' visible' : '';
    const recognizedDateHtml = renderRecognizedDatePreview(item);
    const cardMetaHtml = joinUiMeta([renderTagNames(item.tags), recognizedDateHtml]);
    const download = `<a class="btn btn-ghost btn-icon card-download" data-download href="${fileUrl}" download="${escHtml(downloadFileName(item))}" title="下载文件" aria-label="下载 ${escHtml(item.file_name)}">${buttonIcon('download')}</a>`;
    const titleRow = `<div class="card-title-row"><div class="role">${escHtml(item.file_name)}</div>${download}</div>`;
    const artistJump = isGlobalSearchActive() && item.artist_id
      ? `<button class="btn btn-ghost artist-jump" type="button" data-artist-jump="${item.artist_id}" title="转到 ${escHtml(item.artist_name || '画师')}">转到画师</button>`
      : '';
    const previewUrl = escHtml(videoPreviewUrl(item));

    if (isArchive) {
      html += `<div class="card archive-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <div class="media-file-icon">ZIP</div>
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    } else if (mediaType === 'video') {
      html += `<div class="card video-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <div class="video-preview">
          <img class="video-thumb loading" data-preview-src="${previewUrl}" alt="" decoding="async" draggable="false">
          <div class="media-file-icon">▶</div>
        </div>
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    } else if (mediaType === 'source') {
      html += `<div class="card source-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <div class="media-file-icon">SRC</div>
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    } else if (mediaType === 'text') {
      html += `<div class="card text-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <div class="media-file-icon">TXT</div>
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    } else if (isGifItem(item)) {
      html += `<div class="card gif-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <div class="gif-preview">
          <img class="thumb gif-thumb" data-gif-src="${fileUrl}" loading="lazy" decoding="async" alt="" draggable="false">
          <div class="gif-placeholder">GIF</div>
        </div>
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    } else {
      html += `<div class="card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        <img class="thumb loading" data-src="${previewFileUrl}" decoding="async" fetchpriority="low" draggable="false">
        <div class="info">
          ${titleRow}
          <div class="date">${cardMetaHtml}</div>
          ${artistJump}
        </div>
      </div>`;
    }
  });
  return html;
}

function bindGridEvents() {
  $$('#grid .card').forEach(card => {
    if (card.dataset.gridBound === '1') return;
    card.dataset.gridBound = '1';
    const activateCard = e => {
      if (state.mode === 'edit' && e.target.classList.contains('check')) return;
      if (state.mode === 'edit') {
        if (state.suppressNextGridClick) {
          state.suppressNextGridClick = false;
          e.preventDefault();
          return;
        }
        if (selectionModifierActive(e)) {
          toggleSelect(parseInt(card.dataset.id), {reason: 'ctrl_click'});
        } else {
          selectOnly(parseInt(card.dataset.id), {reason: 'click'});
        }
      } else {
        if (card.classList.contains('archive-card')) return;
        const idx = parseInt(card.dataset.idx);
        openLightbox(idx);
      }
    };
    card.addEventListener('click', activateCard);
    card.addEventListener('keydown', e => {
      if (e.key !== 'Enter' && e.key !== ' ') return;
      e.preventDefault();
      activateCard(e);
    });
  });

  $$('#grid .check').forEach(chk => {
    if (chk.dataset.checkBound === '1') return;
    chk.dataset.checkBound = '1';
    chk.addEventListener('click', e => {
      e.stopPropagation();
      if (state.suppressNextGridClick) {
        state.suppressNextGridClick = false;
        e.preventDefault();
        return;
      }
      toggleSelect(parseInt(chk.dataset.check), {reason: selectionModifierActive(e) ? 'ctrl_check' : 'check'});
    });
  });

  $$('#grid [data-download]').forEach(link => {
    if (link.dataset.downloadBound === '1') return;
    link.dataset.downloadBound = '1';
    link.addEventListener('click', e => {
      e.stopPropagation();
    });
  });

  $$('#grid [data-artist-jump]').forEach(btn => {
    if (btn.dataset.artistJumpBound === '1') return;
    btn.dataset.artistJumpBound = '1';
    btn.addEventListener('click', e => {
      e.stopPropagation();
      jumpToArtist(parseInt(btn.dataset.artistJump));
    });
  });

  $$('#grid [data-tag-jump]').forEach(btn => {
    if (btn.dataset.tagJumpBound === '1') return;
    btn.dataset.tagJumpBound = '1';
    btn.addEventListener('click', e => {
      e.stopPropagation();
      jumpToTag(parseInt(btn.dataset.artistId), parseInt(btn.dataset.tagJump));
    });
  });

  bindSelectionMarqueeEvents();
}

function selectionModifierActive(e) {
  return Boolean(state.selectionModifierDown || (e && (e.ctrlKey || e.metaKey)));
}

function selectionMarqueeBlockedTarget(target) {
  if (!(target instanceof Element)) return false;
  return Boolean(target.closest('.check, [data-download], [data-artist-jump], [data-tag-jump], button, a, input, select, textarea, label'));
}

function bindSelectionMarqueeEvents() {
  const container = $('#gridContainer');
  if (!container || container.dataset.selectionMarqueeBound === '1') return;
  container.dataset.selectionMarqueeBound = '1';
  container.addEventListener('pointerdown', startSelectionMarquee);
}

function startSelectionMarquee(e) {
  const container = $('#gridContainer');
  if (!container || state.mode !== 'edit') return;
  if (e.pointerType && e.pointerType !== 'mouse') return;
  if (e.pointerType === 'mouse' && e.button !== 0) return;
  if (selectionMarqueeBlockedTarget(e.target)) return;
  if (!container.contains(e.target instanceof Node ? e.target : null)) return;
  state.selectionMarquee = {
    pointerId: e.pointerId,
    startX: e.clientX,
    startY: e.clientY,
    currentX: e.clientX,
    currentY: e.clientY,
    active: false,
    moved: false,
    modifier: selectionModifierActive(e),
    baseSelectedIds: new Set(state.selectedIds),
    overlay: null,
  };
  window.addEventListener('pointermove', moveSelectionMarquee);
  window.addEventListener('pointerup', finishSelectionMarquee);
  window.addEventListener('pointercancel', cancelSelectionMarquee);
}

function moveSelectionMarquee(e) {
  if (!state.selectionMarquee || e.pointerId !== state.selectionMarquee.pointerId) return;
  state.selectionMarquee.currentX = e.clientX;
  state.selectionMarquee.currentY = e.clientY;
  state.selectionMarquee.modifier = selectionModifierActive(e);
  const movedX = Math.abs(e.clientX - state.selectionMarquee.startX);
  const movedY = Math.abs(e.clientY - state.selectionMarquee.startY);
  if (!state.selectionMarquee.active && Math.max(movedX, movedY) < SELECTION_MARQUEE_THRESHOLD_PX) return;
  if (!state.selectionMarquee.active) {
    state.selectionMarquee.active = true;
    state.selectionMarquee.moved = true;
    const overlay = document.createElement('div');
    overlay.className = 'selection-marquee';
    $('#gridContainer').appendChild(overlay);
    state.selectionMarquee.overlay = overlay;
    $('#gridContainer').classList.add('selecting');
    const container = $('#gridContainer');
    if (container && container.setPointerCapture && e.pointerId != null) {
      try { container.setPointerCapture(e.pointerId); } catch (err) {}
    }
    logUiAction('selection_box_start', collectSelectionLayoutLogContext({
      modifier: state.selectionMarquee.modifier,
      selected_count: state.selectedIds.size,
    }));
  }
  e.preventDefault();
  updateSelectionMarquee();
}

function selectionIdsForMarquee(boxedIds, baseSelectedIds, modifier) {
  if (!modifier) return new Set(boxedIds);
  const nextIds = new Set(baseSelectedIds);
  boxedIds.forEach(id => {
    if (nextIds.has(id)) {
      nextIds.delete(id);
    } else {
      nextIds.add(id);
    }
  });
  return nextIds;
}

function marqueeRect(selection, containerRect) {
  const left = Math.min(selection.startX, selection.currentX);
  const top = Math.min(selection.startY, selection.currentY);
  const right = Math.max(selection.startX, selection.currentX);
  const bottom = Math.max(selection.startY, selection.currentY);
  return {
    left,
    top,
    right,
    bottom,
    width: right - left,
    height: bottom - top,
    localLeft: left - containerRect.left,
    localTop: top - containerRect.top,
  };
}

function cardRectIntersectsMarquee(cardRect, box) {
  return cardRect.right >= box.left
    && cardRect.left <= box.right
    && cardRect.bottom >= box.top
    && cardRect.top <= box.bottom;
}

function updateSelectionMarquee() {
  if (!state.selectionMarquee || !state.selectionMarquee.active) return;
  const container = $('#gridContainer');
  const containerRect = container.getBoundingClientRect();
  const box = marqueeRect(state.selectionMarquee, containerRect);
  const overlay = state.selectionMarquee.overlay;
  if (overlay) {
    overlay.style.left = `${box.localLeft + container.scrollLeft}px`;
    overlay.style.top = `${box.localTop + container.scrollTop}px`;
    overlay.style.width = `${box.width}px`;
    overlay.style.height = `${box.height}px`;
  }
  const boxedIds = [];
  $$('#grid .card[data-id]').forEach(card => {
    const id = Number(card.dataset.id);
    const item = (state.allItems || []).find(candidate => Number(candidate.id) === id);
    if (!item || !isTaggableItem(item)) return;
    if (cardRectIntersectsMarquee(card.getBoundingClientRect(), box)) boxedIds.push(id);
  });
  const nextIds = selectionIdsForMarquee(boxedIds, state.selectionMarquee.baseSelectedIds, state.selectionMarquee.modifier);
  state.selectionMarquee.boxedCount = boxedIds.length;
  state.selectionMarquee.boxedIds = boxedIds;
  applySelectionChange(nextIds, {reason: 'selection_box', boxed_count: boxedIds.length, modifier: state.selectionMarquee.modifier, schedule: false, log: false});
}

function finishSelectionMarquee(e) {
  if (!state.selectionMarquee || e.pointerId !== state.selectionMarquee.pointerId) return;
  const selection = state.selectionMarquee;
  if (selection.active) {
    updateSelectionMarquee();
    state.suppressNextGridClick = true;
    setTimeout(() => { state.suppressNextGridClick = false; }, 250);
    logUiAction('selection_box_apply', collectSelectionLayoutLogContext({
      modifier: selection.modifier,
      boxed_count: selection.boxedCount || 0,
      selected_count: state.selectedIds.size,
      boxed_item_ids: selection.boxedIds || [],
      selected_item_ids: [...state.selectedIds],
    }));
    scheduleCharacterTagSuggestions({reason: 'selection'});
    scheduleArtistSuggestions({reason: 'selection'});
  }
  cleanupSelectionMarquee(e);
}

function cancelSelectionMarquee(e) {
  if (!state.selectionMarquee || e.pointerId !== state.selectionMarquee.pointerId) return;
  cleanupSelectionMarquee(e);
}

function cleanupSelectionMarquee(e) {
  const container = $('#gridContainer');
  if (state.selectionMarquee && state.selectionMarquee.overlay) {
    state.selectionMarquee.overlay.remove();
  }
  if (container) {
    container.classList.remove('selecting');
    if (container.releasePointerCapture && e && e.pointerId != null) {
      try { container.releasePointerCapture(e.pointerId); } catch (err) {}
    }
  }
  window.removeEventListener('pointermove', moveSelectionMarquee);
  window.removeEventListener('pointerup', finishSelectionMarquee);
  window.removeEventListener('pointercancel', cancelSelectionMarquee);
  state.selectionMarquee = null;
}

async function jumpToArtist(artistId) {
  if (!artistId) return;
  state.searchScope = 'auto';
  syncSearchOptionsControl();
  await selectArtist(artistId);
}

async function jumpToTag(artistId, tagId) {
  if (!artistId || !tagId) return;
  state.search = '';
  $('#searchInput').value = '';
  state.searchScope = 'auto';
  syncSearchOptionsControl();
  await selectArtist(artistId, {tagId});
}

function fileVersionParam(item) {
  const size = Number(item.file_size || 0);
  const mtime = Math.round(Number(item.file_mtime || item.mtime || 0));
  return `${size}-${mtime}`;
}

function isGifItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  if (mediaType !== 'image') return false;
  const name = (item.file_name || item.file_path || '').toLowerCase();
  return name.endsWith('.gif');
}

function observeImages() {
  if (!imageObserver) {
    const container = $('#gridContainer');
    imageObserver = new IntersectionObserver((entries) => {
      entries.forEach(entry => {
        if (entry.isIntersecting) {
          const img = entry.target;
          if (img.dataset.src) {
            if (imageObserver) imageObserver.unobserve(img);
            delete img.dataset.imageObserved;
            queueImageLoad(img);
          }
        }
      });
    }, { root: container || null, rootMargin: IMAGE_OBSERVER_ROOT_MARGIN });
  }

  $$('#grid .thumb[data-src]').forEach(img => {
    if (img.dataset.imageObserved === '1' || img.dataset.imageQueued === '1' || img.dataset.imageLoading === '1' || img.dataset.imageLoaded === '1') return;
    img.dataset.imageObserved = '1';
    imageObserver.observe(img);
  });
}

function isImageNearLoadWindow(img) {
  const container = $('#gridContainer');
  if (!container) return true;
  const margin = parseInt(IMAGE_OBSERVER_ROOT_MARGIN, 10) || 0;
  const viewport = container.getBoundingClientRect();
  const rect = img.getBoundingClientRect();
  return rect.bottom >= viewport.top - margin
    && rect.top <= viewport.bottom + margin
    && rect.right >= viewport.left
    && rect.left <= viewport.right;
}

function reobserveImage(img) {
  delete img.dataset.imageQueued;
  if (!img.isConnected || !img.dataset.src || !imageObserver) return;
  delete img.dataset.imageObserved;
  img.dataset.imageObserved = '1';
  imageObserver.observe(img);
}

function queueImageLoad(img) {
  if (!img.dataset.src || img.dataset.imageQueued === '1' || img.dataset.imageLoading === '1' || img.dataset.imageLoaded === '1') return;
  if (!isImageNearLoadWindow(img)) {
    reobserveImage(img);
    return;
  }
  img.dataset.imageQueued = '1';
  if (!pendingImageLoads.includes(img)) pendingImageLoads.push(img);
  pumpImageLoadQueue();
}

function pumpImageLoadQueue() {
  while (activeImageLoads < MAX_IMAGE_LOADS && pendingImageLoads.length) {
    const img = pendingImageLoads.shift();
    delete img.dataset.imageQueued;
    if (!img.isConnected || !img.dataset.src || img.dataset.imageLoading === '1' || img.dataset.imageLoaded === '1') continue;
    if (!isImageNearLoadWindow(img)) {
      reobserveImage(img);
      continue;
    }
    activeImageLoads += 1;
    img.dataset.imageLoading = '1';
    img.classList.remove('failed');
    img.onload = () => finishImageLoad(img, true);
    img.onerror = () => finishImageLoad(img, false, true);
    img.dataset.imageLoadTimer = String(setTimeout(() => {
      finishImageLoad(img, false, true);
    }, IMAGE_LOAD_TIMEOUT_MS));
    const src = img.dataset.src;
    img.src = src;
    img.removeAttribute('data-src');
  }
}

function clearImageLoadTimer(img) {
  if (!img.dataset.imageLoadTimer) return;
  clearTimeout(Number(img.dataset.imageLoadTimer));
  delete img.dataset.imageLoadTimer;
}

function finishImageLoad(img, loaded, clearSource = false) {
  if (img.dataset.imageLoading === '1') {
    activeImageLoads = Math.max(0, activeImageLoads - 1);
  }
  clearImageLoadTimer(img);
  delete img.dataset.imageLoading;
  delete img.dataset.imageQueued;
  img.onload = null;
  img.onerror = null;
  img.classList.remove('loading');
  if (loaded) {
    img.dataset.imageLoaded = '1';
    img.classList.remove('failed');
  } else {
    img.classList.add('failed');
  }
  if (clearSource) {
    img.removeAttribute('src');
  }
  pumpImageLoadQueue();
}

function releaseAllImageLoads() {
  if (imageObserver) {
    imageObserver.disconnect();
    imageObserver = null;
  }
  const images = new Set([
    ...pendingImageLoads,
    ...$$('#grid .thumb[data-image-loading="1"], #grid .thumb[data-image-queued="1"]'),
  ]);
  pendingImageLoads.splice(0);
  activeImageLoads = 0;
  images.forEach(img => {
    if (!img) return;
    clearImageLoadTimer(img);
    img.onload = null;
    img.onerror = null;
    delete img.dataset.imageLoading;
    delete img.dataset.imageObserved;
    delete img.dataset.imageQueued;
    img.removeAttribute('src');
  });
}

function videoPreviewUrl(item) {
  return API.videoFrameUrl(item.file_path, fileVersionParam(item));
}

function bindGifPreviewEvents() {
  $$('#grid .gif-card').forEach(card => {
    if (card.dataset.gifBound === '1') return;
    card.dataset.gifBound = '1';
    const img = card.querySelector('.gif-thumb[data-gif-src]');
    if (!img) return;
    card.addEventListener('pointerenter', () => playGifPreview(img));
    card.addEventListener('pointerleave', () => stopGifPreview(img));
  });
}

function playGifPreview(img) {
  if (!img.dataset.gifSrc) return;
  img.src = img.dataset.gifSrc;
  img.classList.add('playing');
}

function stopGifPreview(img) {
  img.onload = null;
  img.onerror = null;
  img.classList.remove('playing');
  img.removeAttribute('src');
}

function bindVideoPreviewEvents() {
  $$('#grid .video-card').forEach(card => {
    if (card.dataset.videoBound === '1') return;
    card.dataset.videoBound = '1';
    const video = card.querySelector('.video-thumb[data-preview-src]');
    if (!video) return;
    card.addEventListener('pointerenter', () => scheduleVideoPreview(video));
    card.addEventListener('pointerleave', () => clearVideoPreviewTimer(video));
  });
}

function isVideoPreviewNearLoadWindow(video) {
  const container = $('#gridContainer');
  if (!container) return true;
  const margin = 200;
  const viewport = container.getBoundingClientRect();
  const rect = video.getBoundingClientRect();
  return rect.bottom >= viewport.top - margin
    && rect.top <= viewport.bottom + margin
    && rect.right >= viewport.left
    && rect.left <= viewport.right;
}

function reobserveVideoPreview(video) {
  if (!video.isConnected || !video.dataset.previewSrc || !videoPreviewObserver) return;
  videoPreviewObserver.observe(video);
}

function observeVideoPreviews() {
  if (!videoPreviewObserver) {
    const container = $('#gridContainer');
    videoPreviewObserver = new IntersectionObserver((entries) => {
      entries.forEach(entry => {
        if (entry.isIntersecting) {
          queueVideoPreview(entry.target);
          if (videoPreviewObserver) videoPreviewObserver.unobserve(entry.target);
        }
      });
    }, { root: container || null, rootMargin: '200px' });
  }

  $$('#grid .video-thumb[data-preview-src]').forEach(video => {
    if (video.dataset.previewObserved === '1') return;
    video.dataset.previewObserved = '1';
    videoPreviewObserver.observe(video);
  });
}

function scheduleVideoPreview(video) {
  if (!video.dataset.previewSrc || video.dataset.loading === '1' || video.dataset.loaded === '1') return;
  clearVideoPreviewTimer(video);
  video.dataset.previewTimer = String(setTimeout(() => {
    delete video.dataset.previewTimer;
    queueVideoPreview(video);
  }, VIDEO_PREVIEW_HOVER_DELAY_MS));
}

function clearVideoPreviewTimer(video) {
  if (!video.dataset.previewTimer) return;
  clearTimeout(Number(video.dataset.previewTimer));
  delete video.dataset.previewTimer;
}

function clearVideoPreviewLoadTimer(video) {
  if (!video.dataset.previewLoadTimer) return;
  clearTimeout(Number(video.dataset.previewLoadTimer));
  delete video.dataset.previewLoadTimer;
}

function queueVideoPreview(video) {
  if (!video.dataset.previewSrc || video.dataset.loading === '1' || video.dataset.loaded === '1') return;
  if (!pendingVideoPreviews.includes(video)) pendingVideoPreviews.push(video);
  pumpVideoPreviewQueue();
}

function pumpVideoPreviewQueue() {
  while (activeVideoPreviewLoads < MAX_VIDEO_PREVIEW_LOADS && pendingVideoPreviews.length) {
    const video = pendingVideoPreviews.shift();
    if (!video.isConnected || !video.dataset.previewSrc || video.dataset.loading === '1' || video.dataset.loaded === '1') continue;
    if (!isVideoPreviewNearLoadWindow(video)) {
      reobserveVideoPreview(video);
      continue;
    }
    activeVideoPreviewLoads += 1;
    video.dataset.loading = '1';
    video.onload = () => finishVideoPreview(video, true);
    video.onerror = () => finishVideoPreview(video, false, true);
    video.dataset.previewLoadTimer = String(setTimeout(() => {
      finishVideoPreview(video, false, true);
    }, VIDEO_PREVIEW_LOAD_TIMEOUT_MS));
    video.src = video.dataset.previewSrc;
  }
}

function finishVideoPreview(video, loaded, clearSource = false) {
  if (video.dataset.loading === '1') {
    activeVideoPreviewLoads = Math.max(0, activeVideoPreviewLoads - 1);
  }
  clearVideoPreviewLoadTimer(video);
  delete video.dataset.loading;
  video.onload = null;
  video.onerror = null;
  video.classList.remove('loading');
  if (loaded) {
    video.dataset.loaded = '1';
    video.classList.add('ready');
  }
  if (clearSource) {
    video.removeAttribute('src');
  }
  pumpVideoPreviewQueue();
}

function releaseVideoPreview(video) {
  clearVideoPreviewTimer(video);
  clearVideoPreviewLoadTimer(video);
  const idx = pendingVideoPreviews.indexOf(video);
  if (idx >= 0) pendingVideoPreviews.splice(idx, 1);
  if (video.dataset.loading === '1') {
    activeVideoPreviewLoads = Math.max(0, activeVideoPreviewLoads - 1);
  }
  video.onload = null;
  video.onerror = null;
  delete video.dataset.loading;
  delete video.dataset.loaded;
  video.classList.remove('ready');
  video.classList.add('loading');
  video.removeAttribute('src');
  pumpVideoPreviewQueue();
}

function releaseAllVideoPreviewLoads() {
  if (videoPreviewObserver) {
    videoPreviewObserver.disconnect();
    videoPreviewObserver = null;
  }
  const videos = new Set([
    ...pendingVideoPreviews,
    ...$$('#grid .video-thumb'),
  ]);
  pendingVideoPreviews.splice(0);
  activeVideoPreviewLoads = 0;
  videos.forEach(video => {
    if (!video) return;
    clearVideoPreviewTimer(video);
    clearVideoPreviewLoadTimer(video);
    video.onload = null;
    video.onerror = null;
    delete video.dataset.loading;
    delete video.dataset.previewObserved;
    delete video.dataset.loaded;
    video.classList.remove('ready');
    video.classList.add('loading');
    video.removeAttribute('src');
  });
}

function releaseAllVideoPreviews() {
  releaseAllVideoPreviewLoads();
}

function lightboxPreviewPlaceholderUrl(item) {
  if (!item || item.id === undefined || item.id === null) return '';
  let placeholderUrl = '';
  $$('#grid .card').forEach(card => {
    if (placeholderUrl || card.dataset.id !== String(item.id)) return;
    const thumb = card.querySelector('img.thumb');
    if (!thumb || thumb.classList.contains('failed')) return;
    if (thumb.dataset.imageLoaded !== '1' && !(thumb.complete && thumb.naturalWidth > 0)) return;
    placeholderUrl = thumb.currentSrc || thumb.src || '';
  });
  return placeholderUrl;
}

function clearLightboxVideoFallbackTimer(video) {
  if (!video || !video.dataset.videoFallbackTimer) return;
  clearTimeout(Number(video.dataset.videoFallbackTimer));
  delete video.dataset.videoFallbackTimer;
}

function lightboxVideoLogData(video, extra = {}) {
  return collectUiLogContext(Object.assign({
    item_id: video.dataset.itemId || '',
    file_name: video.dataset.fileName || '',
    file_path: video.dataset.filePath || '',
    current_src: video.currentSrc || video.src || '',
    hls_tried: video.dataset.hlsTried === '1',
    compatible_tried: video.dataset.compatibleTried === '1',
    network_state: video.networkState,
    ready_state: video.readyState,
    error_code: video.error ? video.error.code : 0,
    error_message: video.error ? video.error.message : '',
  }, extra));
}

function logLightboxVideoEvent(event, video, extra = {}) {
  if (!video || !video.dataset.filePath) return;
  logUiAction(event, lightboxVideoLogData(video, extra));
}

function switchLightboxVideoToTranscode(video, reason) {
  if (!video || !video.dataset.filePath) return false;
  if (!video.dataset.transcodeSrc || video.dataset.transcodeTried === '1') return false;
  clearLightboxVideoFallbackTimer(video);
  video.dataset.transcodeTried = '1';
  logLightboxVideoEvent('video_transcode_start', video, {reason});
  startLightboxVideoTranscode(video);
  return true;
}

function switchLightboxVideoToCompatible(video, reason) {
  if (!video || !video.dataset.filePath) return false;
  const compatibleSrc = video.dataset.compatibleSrc || '';
  if (!compatibleSrc || video.dataset.compatibleTried === '1') return false;
  const wasPlaying = !video.paused && !video.ended;
  clearLightboxVideoFallbackTimer(video);
  video.dataset.compatibleTried = '1';
  logLightboxVideoEvent('video_fallback_start', video, {reason});
  video.src = compatibleSrc;
  video.load();
  if (wasPlaying && video.play) {
    video.play().catch(error => {
      logLightboxVideoEvent('video_fallback_play_rejected', video, {message: frontendErrorText(error)});
    });
  }
  return true;
}

function shouldPreferCompatibleVideoStream() {
  const ua = navigator.userAgent || '';
  const vendor = navigator.vendor || '';
  const isIOS = /iPad|iPhone|iPod/.test(ua) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const isSafari = /Safari/.test(ua) && /Apple/.test(vendor) && !/Chrome|CriOS|FxiOS|Edg|OPR|Android/.test(ua);
  return isIOS || isSafari;
}

function shouldUseVideoTranscode() {
  return shouldPreferCompatibleVideoStream();
}

function shouldUseVideoHls() {
  return shouldPreferCompatibleVideoStream();
}

function setLightboxVideoStatus(text) {
  const info = $('#lightboxInfo');
  if (!info) return;
  const status = info.querySelector('[data-video-status]');
  if (status) status.textContent = text || '';
}

async function waitForVideoTranscode(video, token) {
  const deadline = performance.now() + VIDEO_TRANSCODE_WAIT_TIMEOUT_MS;
  while (performance.now() < deadline) {
    const status = await API.get(video.dataset.transcodeStatusSrc);
    if (token !== video.dataset.loadToken) return null;
    if (status.status === 'ready') return status;
    if (status.status === 'error') {
      throw new Error(status.message || status.error || 'video transcode failed');
    }
    await new Promise(resolve => setTimeout(resolve, VIDEO_TRANSCODE_POLL_INTERVAL_MS));
  }
  throw new Error('video transcode timed out');
}

async function startLightboxVideoTranscode(video) {
  if (!video || !video.dataset.filePath) return;
  const token = video.dataset.loadToken || '';
  const transcodeStartedAt = performance.now();
  try {
    setLightboxVideoStatus('正在为 Safari 准备视频');
    let status = await API.get(video.dataset.transcodeStatusSrc);
    if (token !== video.dataset.loadToken) return;
    if (status.status !== 'ready') {
      await API.post(video.dataset.transcodeSrc);
      if (token !== video.dataset.loadToken) return;
      status = await waitForVideoTranscode(video, token);
      if (!status) return;
    }
    if (status.status !== 'ready') {
      throw new Error(status.error || 'video transcode did not finish');
    }
    video.src = video.dataset.transcodedSrc;
    video.load();
    setLightboxVideoStatus('Safari 视频准备完成');
    logLightboxVideoEvent('video_transcode_ready', video, {
      key: status.key || '',
      elapsed_ms: Math.round(performance.now() - transcodeStartedAt)
    });
  } catch (error) {
    if (token !== video.dataset.loadToken) return;
    setLightboxVideoStatus('Safari 视频准备失败，正在尝试兼容流');
    logLightboxVideoEvent('video_transcode_error', video, {
      message: frontendErrorText(error),
      elapsed_ms: Math.round(performance.now() - transcodeStartedAt)
    });
    if (!switchLightboxVideoToCompatible(video, 'transcode_error')) {
      showLightboxVideoFailure(video, 'transcode_error');
    }
  }
}

function showLightboxVideoFailure(video, reason) {
  if (!video || !video.dataset.filePath) return;
  setLightboxVideoStatus('视频加载失败：文件不可访问或格式不受支持');
  logLightboxVideoEvent('video_terminal_error', video, {reason});
}

function handleLightboxVideoReadinessFailure(video, reason) {
  if (video && video.dataset.hlsTried === '1' && video.dataset.transcodeTried !== '1') {
    return switchLightboxVideoToTranscode(video, reason);
  }
  if (video && reason !== 'media_error') {
    logLightboxVideoEvent('video_stream_waiting', video, {reason});
    return false;
  }
  if (switchLightboxVideoToCompatible(video, reason)) return true;
  showLightboxVideoFailure(video, reason);
  return false;
}

function scheduleLightboxVideoFallback(video) {
  clearLightboxVideoFallbackTimer(video);
  if (!video || (!video.dataset.compatibleSrc && !video.dataset.transcodeSrc)) return;
  if (video.dataset.compatibleTried === '1' || video.dataset.transcodeTried === '1') return;
  video.dataset.videoFallbackTimer = String(setTimeout(() => {
    delete video.dataset.videoFallbackTimer;
    if (!video.isConnected || video.style.display === 'none' || !video.dataset.filePath) return;
    if (video.readyState < 3) {
      handleLightboxVideoReadinessFailure(video, 'canplay_timeout');
    }
  }, LIGHTBOX_VIDEO_FALLBACK_DELAY_MS));
}

function bindLightboxVideoDiagnostics() {
  const video = $('#lightboxVideo');
  if (!video || video.dataset.diagnosticsBound === '1') return;
  video.dataset.diagnosticsBound = '1';
  video.addEventListener('loadstart', () => {
    logLightboxVideoEvent('video_loadstart', video);
  });
  video.addEventListener('loadedmetadata', () => {
    logLightboxVideoEvent('video_loadedmetadata', video, {
      duration: Number.isFinite(video.duration) ? Number(video.duration.toFixed(3)) : null,
      video_width: video.videoWidth || 0,
      video_height: video.videoHeight || 0,
    });
  });
  video.addEventListener('canplay', () => {
    clearLightboxVideoFallbackTimer(video);
    logLightboxVideoEvent('video_canplay', video, {
      video_width: video.videoWidth || 0,
      video_height: video.videoHeight || 0,
    });
  });
  video.addEventListener('stalled', () => {
    logLightboxVideoEvent('video_stalled', video);
    if (video.readyState < 1) handleLightboxVideoReadinessFailure(video, 'stalled_before_metadata');
  });
  video.addEventListener('error', () => {
    clearLightboxVideoFallbackTimer(video);
    logLightboxVideoEvent('video_error', video);
    if (video.dataset.compatibleTried === '1') {
      showLightboxVideoFailure(video, 'media_error');
      return;
    }
    handleLightboxVideoReadinessFailure(video, 'media_error');
  });
}

function openLightbox(idx) {
  const selected = state.allItems[idx];
  const items = lightboxItems();
  if (items.length === 0) return;
  const nextIndex = selected ? items.findIndex(item => item.id === selected.id) : -1;
  if (nextIndex < 0) return;
  state.lastFocusedBeforeLightbox = document.activeElement;
  state.lightboxIndex = nextIndex;
  resetLightboxTransform();
  showLightboxImage(items);
  $('#lightbox').style.display = 'flex';
  document.addEventListener('keydown', onLightboxKey);
  // Move focus into the dialog for keyboard/screen-reader users.
  const closeBtn = $('#lightbox .close') || $('#lightboxClose') || $('#lightboxDownloadBtn');
  if (closeBtn && typeof closeBtn.focus === 'function') {
    try { closeBtn.focus(); } catch (e) {}
  }
}

function lightboxItems() {
  return state.allItems.filter(isLightboxItem);
}

function isTaggableItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  return mediaType === 'image' || mediaType === 'video' || mediaType === 'source' || mediaType === 'archive' || mediaType === 'text' || item.is_archive;
}

function isLightboxItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  return mediaType === 'image' || mediaType === 'video' || mediaType === 'source' || mediaType === 'text';
}

function showLightboxImage(items) {
  const item = items[state.lightboxIndex];
  if (!item) return;
  const loadToken = ++state.lightboxLoadToken;
  const mediaType = item.media_type || 'image';
  const lightbox = $('#lightbox');
  lightbox.classList.toggle('text-mode', mediaType === 'text');
  const fileUrl = API.fileUrl(item.file_path, fileVersionParam(item));
  const displayFileUrl = fileUrl;
  const placeholderUrl = mediaType === 'image' && !isGifItem(item) ? lightboxPreviewPlaceholderUrl(item) : '';
  const img = $('#lightboxImg');
  const video = $('#lightboxVideo');
  const file = $('#lightboxFile');
  img.onload = null;
  img.onerror = null;
  img.style.display = 'none';
  img.classList.remove('ready', 'failed', 'placeholder');
  img.classList.add('loading');
  img.removeAttribute('src');
  img.alt = item.file_name || '';
  delete img.dataset.fallbackSrc;
  video.style.display = 'none';
  file.style.display = 'none';
  const textEl = $('#lightboxText');
  if (textEl) { textEl.style.display = 'none'; textEl.innerHTML = ''; }
  clearLightboxVideoFallbackTimer(video);
  video.pause();
  video.removeAttribute('src');
  delete video.dataset.itemId;
  delete video.dataset.fileName;
  delete video.dataset.filePath;
  delete video.dataset.originalSrc;
  delete video.dataset.hlsSrc;
  delete video.dataset.hlsTried;
  delete video.dataset.compatibleSrc;
  delete video.dataset.compatibleTried;
  delete video.dataset.transcodeSrc;
  delete video.dataset.transcodeStatusSrc;
  delete video.dataset.transcodedSrc;
  delete video.dataset.transcodeTried;
  delete video.dataset.loadToken;
  video.load();

  if (mediaType === 'video') {
    img.classList.remove('loading');
    video.dataset.itemId = String(item.id);
    video.dataset.fileName = item.file_name || '';
    video.dataset.filePath = item.file_path || '';
    video.dataset.loadToken = String(loadToken);
    video.dataset.originalSrc = API.streamUrl(item.file_path);
    video.dataset.hlsSrc = API.videoHlsUrl(item.file_path);
    video.dataset.compatibleSrc = API.videoCompatibleUrl(item.file_path);
    video.dataset.transcodeSrc = API.videoTranscodeUrl(item.file_path);
    video.dataset.transcodeStatusSrc = API.videoTranscodeStatusUrl(item.file_path);
    video.dataset.transcodedSrc = API.videoTranscodedUrl(item.file_path);
    const useHlsStream = shouldUseVideoHls();
    video.dataset.hlsTried = '0';
    video.dataset.compatibleTried = '0';
    video.dataset.transcodeTried = '0';
    if (useHlsStream) {
      video.dataset.hlsTried = '1';
      video.src = video.dataset.hlsSrc;
      video.load();
      scheduleLightboxVideoFallback(video);
      logLightboxVideoEvent('video_hls_start', video, {reason: 'apple_webkit'});
    } else {
      video.src = video.dataset.originalSrc;
      video.load();
      scheduleLightboxVideoFallback(video);
    }
    video.style.display = '';
  } else if (mediaType === 'source') {
    img.classList.remove('loading');
    file.innerHTML = `<div class="source-mark">SRC</div><div>${escHtml(item.file_name)}</div><small>${formatSize(item.file_size)}</small>`;
    file.style.display = 'flex';
  } else if (mediaType === 'text') {
    img.classList.remove('loading');
    const textEl = $('#lightboxText');
    textEl.innerHTML = `<div class="lightbox-text-loading">读取中…</div>`;
    textEl.style.display = 'flex';
    fetch(API.textUrl(item.file_path))
      .then(r => r.ok ? r.json() : Promise.reject(r.status))
      .then(data => {
        if (loadToken !== state.lightboxLoadToken) return;
        const truncNote = data.truncated ? `<div class="lightbox-text-truncated">（仅显示前 ${formatSize(data.size)} 中的部分内容）</div>` : '';
        textEl.innerHTML = `<pre class="lightbox-text-pre">${escHtml(data.content)}</pre>${truncNote}`;
      })
      .catch(() => {
        if (loadToken !== state.lightboxLoadToken) return;
        textEl.innerHTML = `<div class="lightbox-text-loading">读取失败</div>`;
      });
  } else {
    if (displayFileUrl !== fileUrl) img.dataset.fallbackSrc = fileUrl;
    const revealLoadedImage = (src) => {
      if (loadToken !== state.lightboxLoadToken) return;
      img.src = src;
      const reveal = () => {
        if (loadToken !== state.lightboxLoadToken) return;
        img.classList.remove('loading', 'failed', 'placeholder');
        img.classList.add('ready');
        img.style.display = '';
      };
      if (img.decode) {
        img.decode().then(reveal).catch(reveal);
      } else {
        reveal();
      }
    };
    const loadFallbackImage = (fallbackSrc) => {
      if (loadToken !== state.lightboxLoadToken) return;
      const fallbackLoader = new Image();
      fallbackLoader.decoding = 'async';
      fallbackLoader.onload = () => revealLoadedImage(fallbackSrc);
      fallbackLoader.onerror = () => {
        if (loadToken !== state.lightboxLoadToken) return;
        img.classList.remove('loading', 'ready', 'placeholder');
        img.classList.add('failed');
        if (!placeholderUrl) img.style.display = '';
      };
      fallbackLoader.src = fallbackSrc;
    };
    const loader = new Image();
    loader.decoding = 'async';
    loader.onload = () => revealLoadedImage(displayFileUrl);
    loader.onerror = () => {
      if (loadToken !== state.lightboxLoadToken) return;
      if (img.dataset.fallbackSrc) {
        const fallbackSrc = img.dataset.fallbackSrc;
        delete img.dataset.fallbackSrc;
        loadFallbackImage(fallbackSrc);
        return;
      }
      img.classList.remove('loading', 'ready', 'placeholder');
      img.classList.add('failed');
      if (!placeholderUrl) img.style.display = '';
    };
    if (placeholderUrl) {
      img.src = placeholderUrl;
      img.classList.remove('loading', 'failed');
      img.classList.add('ready', 'placeholder');
      img.style.display = '';
    }
    loader.src = displayFileUrl;
  }
  applyLightboxZoom();
  const download = $('#lightboxDownloadBtn');
  download.href = fileUrl;
  download.download = downloadFileName(item);
  download.setAttribute('aria-label', `下载 ${item.file_name}`);
  const deleteBtn = $('#lightboxDeleteBtn');
  if (deleteBtn) {
    deleteBtn.dataset.filePath = item.file_path;
    deleteBtn.dataset.itemId = String(item.id);
    deleteBtn.dataset.fileName = item.file_name || '';
    resetLightboxDeleteBtn(deleteBtn);
  }
  const copyPath = item.real_file_path || item.file_path;
  const lightboxMeta = [
    `<span class="lightbox-meta-tags">${renderTagNames(item.tags) || escHtml('未加标签')}</span>`,
    item.date ? `<span class="lightbox-meta-date">${escHtml(item.date)}</span>` : '',
    item.file_name ? `<span class="lightbox-meta-name">${escHtml(item.file_name)}</span>` : '',
  ];
  $('#lightboxPath').innerHTML = `
    <button type="button" class="btn lightbox-path-panel" data-copy-path="${escHtml(copyPath)}" title="${escHtml(copyPath)}">${escHtml(item.display_file_path || item.real_file_path || item.file_path)}</button>
  `;
  $('#lightboxInfo').innerHTML = `
    ${lightboxMeta.filter(part => part).join('')}
    ${mediaType === 'video' ? '<span class="lightbox-meta-status" data-video-status></span>' : ''}
  `;
  const pathButton = $('#lightboxPath .lightbox-path-panel');
  if (pathButton) {
    pathButton.addEventListener('click', async e => {
      e.stopPropagation();
      const ok = await copyText(pathButton.dataset.copyPath || '');
      toast(ok ? '真实路径已复制' : '复制路径失败', ok ? 'success' : 'error');
    });
  }
}

function applyLightboxZoom() {
  const img = $('#lightboxImg');
  if (!img) return;
  Object.assign(img.style, {
    transform: `translate(${state.lightboxPanX}px, ${state.lightboxPanY}px) scale(${state.lightboxZoom})`,
    cursor: state.lightboxZoom > 1 ? 'grab' : 'default',
  });
}

function clampLightboxZoom(value) {
  const parsed = Number(value);
  if (!Number.isFinite(parsed)) return 1;
  return Math.max(LIGHTBOX_ZOOM_MIN, Math.min(LIGHTBOX_ZOOM_MAX, Number(parsed.toFixed(2))));
}

function setLightboxZoom(value) {
  state.lightboxZoom = clampLightboxZoom(value);
  if (state.lightboxZoom <= 1) {
    state.lightboxPanX = 0;
    state.lightboxPanY = 0;
    state.lightboxPanActive = false;
  }
  applyLightboxZoom();
}

function resetLightboxTransform() {
  state.lightboxZoom = 1;
  state.lightboxPanX = 0;
  state.lightboxPanY = 0;
  state.lightboxPanActive = false;
  state.lightboxPointers.clear();
  state.lightboxPinchActive = false;
  state.lightboxPinchStartDistance = 0;
  state.lightboxPinchStartZoom = 1;
  state.lightboxTapPointerId = null;
  state.lightboxTapMoved = false;
  state.lightboxLastTapAt = 0;
  applyLightboxZoom();
}

function isTouchLightboxPointer(e) {
  return e && (e.pointerType === 'touch' || e.pointerType === 'pen');
}

function lightboxPointerDistance(a, b) {
  if (!a || !b) return 0;
  return Math.hypot(a.x - b.x, a.y - b.y);
}

function lightboxPointerPoints() {
  return [...state.lightboxPointers.values()];
}

function startLightboxPinch() {
  const points = lightboxPointerPoints();
  if (points.length < 2) return;
  const distance = lightboxPointerDistance(points[0], points[1]);
  if (distance <= 0) return;
  state.lightboxPinchActive = true;
  state.lightboxPinchStartDistance = distance;
  state.lightboxPinchStartZoom = state.lightboxZoom;
  state.lightboxPanActive = false;
  state.lightboxTapPointerId = null;
  state.lightboxTapMoved = true;
}

function updateLightboxPinchZoom(e) {
  if (!state.lightboxPinchActive || state.lightboxPointers.size < 2) return;
  e.preventDefault();
  e.stopPropagation();
  const points = lightboxPointerPoints();
  const distance = lightboxPointerDistance(points[0], points[1]);
  if (distance <= 0 || state.lightboxPinchStartDistance <= 0) return;
  setLightboxZoom(state.lightboxPinchStartZoom * (distance / state.lightboxPinchStartDistance));
}

function handleLightboxDoubleTap(e) {
  if (!isTouchLightboxPointer(e) || state.lightboxTapMoved) return false;
  const now = Date.now();
  const distance = Math.hypot(e.clientX - state.lightboxLastTapX, e.clientY - state.lightboxLastTapY);
  const isDoubleTap = (
    state.lightboxLastTapAt > 0
    && now - state.lightboxLastTapAt <= LIGHTBOX_DOUBLE_TAP_DELAY_MS
    && distance <= LIGHTBOX_DOUBLE_TAP_DISTANCE_PX
  );
  state.lightboxLastTapAt = now;
  state.lightboxLastTapX = e.clientX;
  state.lightboxLastTapY = e.clientY;
  if (!isDoubleTap) return false;
  e.preventDefault();
  e.stopPropagation();
  state.lightboxLastTapAt = 0;
  setLightboxZoom(state.lightboxZoom > 1 ? 1 : LIGHTBOX_DOUBLE_TAP_ZOOM);
  return true;
}

function startLightboxPan(e) {
  if (e.pointerType === 'mouse' && e.button !== 0) return;
  if (e.pointerId != null) {
    state.lightboxPointers.set(e.pointerId, {x: e.clientX, y: e.clientY});
  }
  const img = $('#lightboxImg');
  if (img && img.setPointerCapture && e.pointerId != null) {
    try { img.setPointerCapture(e.pointerId); } catch (err) {}
  }
  if (state.lightboxPointers.size === 2) {
    e.preventDefault();
    e.stopPropagation();
    startLightboxPinch();
    return;
  }
  if (isTouchLightboxPointer(e)) {
    state.lightboxTapPointerId = e.pointerId;
    state.lightboxTapStartX = e.clientX;
    state.lightboxTapStartY = e.clientY;
    state.lightboxTapMoved = false;
  }
  if (state.lightboxZoom <= 1) return;
  e.preventDefault();
  e.stopPropagation();
  state.lightboxPanActive = true;
  state.lightboxPanPointerX = e.clientX;
  state.lightboxPanPointerY = e.clientY;
  state.lightboxPanStartX = state.lightboxPanX;
  state.lightboxPanStartY = state.lightboxPanY;
}

function moveLightboxPan(e) {
  if (e.pointerId != null && state.lightboxPointers.has(e.pointerId)) {
    state.lightboxPointers.set(e.pointerId, {x: e.clientX, y: e.clientY});
  }
  if (state.lightboxTapPointerId === e.pointerId) {
    const tapDistance = Math.hypot(e.clientX - state.lightboxTapStartX, e.clientY - state.lightboxTapStartY);
    if (tapDistance > LIGHTBOX_DOUBLE_TAP_DISTANCE_PX) state.lightboxTapMoved = true;
  }
  if (state.lightboxPointers.size === 2) {
    if (!state.lightboxPinchActive) startLightboxPinch();
    updateLightboxPinchZoom(e);
    return;
  }
  if (!state.lightboxPanActive || state.lightboxZoom <= 1) return;
  e.preventDefault();
  e.stopPropagation();
  state.lightboxPanX = state.lightboxPanStartX + (e.clientX - state.lightboxPanPointerX);
  state.lightboxPanY = state.lightboxPanStartY + (e.clientY - state.lightboxPanPointerY);
  applyLightboxZoom();
}

function stopLightboxPan(e) {
  const wasPinching = state.lightboxPinchActive || state.lightboxPointers.size > 1;
  const img = $('#lightboxImg');
  if (img && img.releasePointerCapture && e && e.pointerId != null) {
    try { img.releasePointerCapture(e.pointerId); } catch (err) {}
  }
  if (e && e.pointerId != null) {
    state.lightboxPointers.delete(e.pointerId);
  } else {
    state.lightboxPointers.clear();
  }
  if (state.lightboxPanActive) state.lightboxPanActive = false;
  if (state.lightboxPointers.size < 2) {
    state.lightboxPinchActive = false;
    state.lightboxPinchStartDistance = 0;
    state.lightboxPinchStartZoom = state.lightboxZoom;
  }
  if (!wasPinching && e && state.lightboxTapPointerId === e.pointerId) {
    handleLightboxDoubleTap(e);
  }
  if (state.lightboxPointers.size === 0) {
    state.lightboxTapPointerId = null;
    state.lightboxTapMoved = false;
  }
}

function moveLightbox(delta) {
  const items = lightboxItems();
  if (!items.length) return;
  const nextIndex = Math.max(0, Math.min(items.length - 1, state.lightboxIndex + delta));
  if (nextIndex === state.lightboxIndex) return;
  state.lightboxIndex = nextIndex;
  resetLightboxTransform();
  showLightboxImage(items);
}

function onLightboxKey(e) {
  if (e.key === 'Escape') closeLightbox();
  if (e.key === 'ArrowLeft') moveLightbox(-1);
  if (e.key === 'ArrowRight') moveLightbox(1);
}

function onLightboxWheel(e) {
  if (Math.abs(e.deltaY) < 1) return;
  e.preventDefault();
  e.stopPropagation();

  if (e.ctrlKey) {
    const direction = e.deltaY < 0 ? 1 : -1;
    const zoom = state.lightboxZoom + direction * LIGHTBOX_ZOOM_STEP;
    setLightboxZoom(zoom);
    return;
  }

  const now = Date.now();
  if (now - state.lightboxWheelLastAt < LIGHTBOX_WHEEL_NAV_DELAY) return;
  state.lightboxWheelLastAt = now;
  moveLightbox(e.deltaY > 0 ? 1 : -1);
}

function resetLightboxDeleteBtn(btn) {
  if (!btn) return;
  btn.dataset.confirmStep = '0';
  const label = btn.querySelector('.lightbox-delete-label');
  if (label) label.textContent = '删除';
  btn.classList.remove('deleting', 'confirm1', 'confirm2');
  btn.title = '删除文件';
}

async function onLightboxDelete(btn) {
  const step = parseInt(btn.dataset.confirmStep || '0');
  const label = btn.querySelector('.lightbox-delete-label');
  if (step === 0) {
    btn.dataset.confirmStep = '1';
    if (label) label.textContent = '确认删除？';
    btn.classList.add('confirm1');
  } else if (step === 1) {
    btn.dataset.confirmStep = '2';
    if (label) label.textContent = '再次确认？';
    btn.classList.remove('confirm1');
    btn.classList.add('confirm2');
  } else {
    if (label) label.textContent = '删除中…';
    await deleteMediaItem({
      filePath: btn.dataset.filePath,
      itemId: parseInt(btn.dataset.itemId),
      fileName: btn.dataset.fileName || '',
      button: btn,
      closeLightboxAfter: true,
      onError: () => resetLightboxDeleteBtn(btn),
    });
  }
}

async function deleteMediaItem({filePath, itemId, fileName = '', button = null, closeLightboxAfter = false, skipConfirm = false, onError = null, renderAfter = true, toastOnSuccess = true}) {
  if (!filePath || !itemId) return false;
  const busyId = String(itemId);
  if (isActionBusy('media-delete', busyId)) return false;
  if (!skipConfirm && !confirm(`删除文件 ${fileName || itemId}？文件将移到飞牛回收站，可在回收站恢复。`)) return false;
  setActionBusy('media-delete', busyId, true);
  if (button) button.classList.add('deleting');
  try {
    await API.del(API.deleteFileUrl(filePath));
    state.allItems = state.allItems.filter(i => i.id !== itemId);
    state.selectedIds.delete(itemId);
    if (closeLightboxAfter) closeLightbox();
    if (renderAfter) renderGrid();
    if (toastOnSuccess) toast('文件已移到回收站', 'success');
    return true;
  } catch (e) {
    if (onError) onError(e);
    toast('移到回收站失败', 'error');
    return false;
  } finally {
    if (button) button.classList.remove('deleting');
    setActionBusy('media-delete', busyId, false);
  }
}

function closeLightbox() {
  const lightbox = $('#lightbox');
  lightbox.style.display = 'none';
  lightbox.classList.remove('text-mode');
  state.lightboxLoadToken += 1;
  resetLightboxTransform();
  const img = $('#lightboxImg');
  img.onload = null;
  img.onerror = null;
  img.removeAttribute('src');
  img.style.display = 'none';
  img.classList.remove('loading', 'ready', 'failed', 'placeholder');
  delete img.dataset.fallbackSrc;
  const video = $('#lightboxVideo');
  clearLightboxVideoFallbackTimer(video);
  video.pause();
  video.removeAttribute('src');
  delete video.dataset.itemId;
  delete video.dataset.fileName;
  delete video.dataset.filePath;
  delete video.dataset.originalSrc;
  delete video.dataset.hlsSrc;
  delete video.dataset.hlsTried;
  delete video.dataset.compatibleSrc;
  delete video.dataset.compatibleTried;
  delete video.dataset.transcodeSrc;
  delete video.dataset.transcodeStatusSrc;
  delete video.dataset.transcodedSrc;
  delete video.dataset.loadToken;
  video.load();
  document.removeEventListener('keydown', onLightboxKey);
  const previousFocus = state.lastFocusedBeforeLightbox;
  state.lastFocusedBeforeLightbox = null;
  if (previousFocus && previousFocus.isConnected && previousFocus.focus) {
    previousFocus.focus();
  }
}
