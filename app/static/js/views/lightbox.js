// Lightbox: zoom/pan/pinch/double-tap, media loading with video fallback
// chains, text/source rendering, and the single-item recycle-bin delete.

import { API } from '../api.js';
import { state, isActionBusy, setActionBusy } from '../store.js';
import {
  $, $$, escHtml, formatSize, copyText, downloadFileName, renderTagNamesHtml,
} from '../utils.js';
import { toast, logUiAction, collectUiLogContext, frontendErrorText } from '../logging.js';
import { syncFavoriteButtons, isGifItem, fileVersionParam, renderGrid } from './grid.js';

const LIGHTBOX_ZOOM_MIN = 0.5;
const LIGHTBOX_ZOOM_MAX = 4;
const LIGHTBOX_ZOOM_STEP = 0.15;
const LIGHTBOX_DOUBLE_TAP_ZOOM = 2;
const LIGHTBOX_DOUBLE_TAP_DELAY_MS = 320;
const LIGHTBOX_DOUBLE_TAP_DISTANCE_PX = 36;
const LIGHTBOX_WHEEL_NAV_DELAY = 180;
const LIGHTBOX_VIDEO_FALLBACK_DELAY_MS = 12000;
const VIDEO_TRANSCODE_POLL_INTERVAL_MS = 1000;
const VIDEO_TRANSCODE_WAIT_TIMEOUT_MS = 120000;
const LIGHTBOX_CLOSE_MS = 150;
let lightboxCloseTimer = 0;
let lightboxOpenRaf = 0;

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

export function bindLightboxVideoDiagnostics() {
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

// S5: the lightbox is a real modal. role="dialog" + aria-modal="true" declare
// the boundary; while it is open the background chrome (header + main) is made
// inert so background controls leave both the Tab order and the accessibility
// tree. The lightbox itself and toasts (body children) stay live, and the
// mobile filter drawer keeps managing the sidebar's own inert attribute.
let lightboxBackgroundInert = false;

function setLightboxBackgroundInert(on) {
  const targets = ['#appHeader', 'main']
    .map(selector => document.querySelector(selector))
    .filter(Boolean);
  if (on) {
    targets.forEach(el => {
      if (!el.hasAttribute('inert')) el.setAttribute('inert', '');
    });
    lightboxBackgroundInert = true;
  } else if (lightboxBackgroundInert) {
    targets.forEach(el => el.removeAttribute('inert'));
    lightboxBackgroundInert = false;
  }
}

// The dialog name follows the current media so screen-reader users hear which
// file they are looking at, including while navigating with the lightbox nav.
function syncLightboxAccessibleName(item) {
  const lightbox = $('#lightbox');
  if (!lightbox) return;
  const name = item && (item.file_name || '').trim();
  lightbox.setAttribute('aria-label', name ? `${name} 预览` : '预览');
}

export function openLightbox(idx) {
  const selected = state.allItems[idx];
  const items = lightboxItems();
  if (items.length === 0) return;
  const nextIndex = selected ? items.findIndex(item => item.id === selected.id) : -1;
  if (nextIndex < 0) return;
  state.lastFocusedBeforeLightbox = document.activeElement;
  state.lightboxIndex = nextIndex;
  resetLightboxTransform();
  showLightboxImage(items);
  const lightbox = $('#lightbox');
  if (lightboxCloseTimer) {
    clearTimeout(lightboxCloseTimer);
    lightboxCloseTimer = 0;
  }
  lightbox.style.display = 'flex';
  lightbox.classList.remove('is-closing');
  document.body.classList.add('lightbox-open');
  setLightboxBackgroundInert(true);
  // Trigger the §3.4 open animation on the next frame so the transition runs
  // from the .98 / opacity:0 resting state to .is-open.
  if (lightboxOpenRaf) {
    cancelAnimationFrame(lightboxOpenRaf);
    lightboxOpenRaf = 0;
  }
  lightboxOpenRaf = requestAnimationFrame(() => {
    lightbox.classList.add('is-open');
    lightboxOpenRaf = 0;
  });
  document.addEventListener('keydown', onLightboxKey);
  // Move focus into the dialog for keyboard/screen-reader users.
  const closeBtn = $('#lightbox .close');
  if (closeBtn && typeof closeBtn.focus === 'function') {
    try { closeBtn.focus(); } catch (e) {}
  }
}

export function lightboxItems() {
  return state.allItems.filter(isLightboxItem);
}

export function isLightboxItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  return mediaType === 'image' || mediaType === 'video' || mediaType === 'source' || mediaType === 'text';
}

export function showLightboxImage(items) {
  const item = items[state.lightboxIndex];
  if (!item) return;
  syncLightboxAccessibleName(item);
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
    textEl.innerHTML = `<div class="lightbox-text-loading">读取中</div>`;
    textEl.style.display = 'flex';
    API.get(API.textUrl(item.file_path))
      .then(data => {
        if (loadToken !== state.lightboxLoadToken) return;
        const truncNote = data.truncated ? `<div class="lightbox-text-truncated">仅显示部分内容，文件共 ${formatSize(data.size)}</div>` : '';
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
  const favoriteBtn = $('#lightboxFavoriteBtn');
  favoriteBtn.dataset.favorite = String(item.id);
  syncFavoriteButtons(item);
  const deleteBtn = $('#lightboxDeleteBtn');
  if (deleteBtn) {
    deleteBtn.dataset.filePath = item.file_path;
    deleteBtn.dataset.itemId = String(item.id);
    deleteBtn.dataset.fileName = item.file_name || '';
    resetLightboxDeleteBtn(deleteBtn);
  }
  const copyPath = item.real_file_path || item.file_path;
  const lightboxMeta = [
    `<span class="lightbox-meta-tags">${renderTagNamesHtml(item.tags)}</span>`,
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

export function applyLightboxZoom() {
  const img = $('#lightboxImg');
  if (!img) return;
  Object.assign(img.style, {
    transform: `translate(${state.lightboxPanX}px, ${state.lightboxPanY}px) scale(${state.lightboxZoom})`,
    cursor: state.lightboxZoom > 1 ? 'grab' : 'default',
  });
}

export function clampLightboxZoom(value) {
  const parsed = Number(value);
  if (!Number.isFinite(parsed)) return 1;
  return Math.max(LIGHTBOX_ZOOM_MIN, Math.min(LIGHTBOX_ZOOM_MAX, Number(parsed.toFixed(2))));
}

export function setLightboxZoom(value) {
  state.lightboxZoom = clampLightboxZoom(value);
  if (state.lightboxZoom <= 1) {
    state.lightboxPanX = 0;
    state.lightboxPanY = 0;
    state.lightboxPanActive = false;
  }
  applyLightboxZoom();
}

export function resetLightboxTransform() {
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

export function startLightboxPan(e) {
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

export function moveLightboxPan(e) {
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

export function stopLightboxPan(e) {
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

export function moveLightbox(delta) {
  const items = lightboxItems();
  if (!items.length) return;
  const nextIndex = Math.max(0, Math.min(items.length - 1, state.lightboxIndex + delta));
  if (nextIndex === state.lightboxIndex) return;
  state.lightboxIndex = nextIndex;
  resetLightboxTransform();
  showLightboxImage(items);
}

export function onLightboxKey(e) {
  if (['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target?.tagName) || e.target?.isContentEditable) {
    if (e.key === 'Escape') {
      e.target.blur();
    }
    return;
  }
  if (e.key === 'Escape') {
    if (state.lightboxZoom > 1) {
      setLightboxZoom(1);
    } else {
      closeLightbox();
    }
    return;
  }
  if (e.key === 'ArrowLeft') moveLightbox(-1);
  if (e.key === 'ArrowRight') moveLightbox(1);
}

export function onLightboxWheel(e) {
  if (Math.abs(e.deltaY) < 1) return;
  e.preventDefault();
  e.stopPropagation();

  if (e.ctrlKey || state.lightboxZoom > 1) {
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

export function resetLightboxDeleteBtn(btn) {
  if (!btn) return;
  btn.classList.remove('deleting');
  btn.title = '删除文件';
  btn.setAttribute('aria-label', '删除文件');
}

export async function onLightboxDelete(btn) {
  if (!btn) return;
  if (btn.classList.contains('deleting')) return;
  await deleteMediaItem({
    filePath: btn.dataset.filePath,
    itemId: parseInt(btn.dataset.itemId),
    fileName: btn.dataset.fileName || '',
    button: btn,
    closeLightboxAfter: true,
    onError: () => resetLightboxDeleteBtn(btn),
  });
  resetLightboxDeleteBtn(btn);
}

export async function deleteMediaItem({filePath, itemId, fileName = '', button = null, closeLightboxAfter = false, skipConfirm = false, onError = null, renderAfter = true, toastOnSuccess = true}) {
  if (!filePath || !itemId) return false;
  const busyId = String(itemId);
  if (isActionBusy('media-delete', busyId)) return false;
  if (!skipConfirm && !window.confirm(`确定要将文件「${fileName || '此文件'}」移入回收站吗？`)) return false;
  setActionBusy('media-delete', busyId, true);
  if (button) button.classList.add('deleting');
  try {
    await API.del(API.deleteFileUrl(filePath));
    state.allItems = state.allItems.filter(i => i.id !== itemId);
    state.itemsOffset = state.allItems.length;
    state.selectedIds.delete(itemId);
    if (closeLightboxAfter) closeLightbox();
    if (renderAfter) renderGrid();
    if (toastOnSuccess) toast('文件已移入回收站', 'success');
    return true;
  } catch (e) {
    if (onError) onError(e);
    toast('移入回收站失败', 'error');
    return false;
  } finally {
    if (button) button.classList.remove('deleting');
    setActionBusy('media-delete', busyId, false);
  }
}

export function closeLightbox() {
  const lightbox = $('#lightbox');
  if (lightboxOpenRaf) {
    cancelAnimationFrame(lightboxOpenRaf);
    lightboxOpenRaf = 0;
  }
  document.body.classList.remove('lightbox-open');
  setLightboxBackgroundInert(false);
  lightbox.classList.remove('is-open');
  lightbox.classList.add('is-closing');
  if (lightboxCloseTimer) clearTimeout(lightboxCloseTimer);
  const reducedMotion = typeof window.matchMedia === 'function'
    && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  lightboxCloseTimer = setTimeout(() => {
    lightbox.style.display = 'none';
    lightbox.classList.remove('is-closing');
    lightboxCloseTimer = 0;
  }, reducedMotion ? 0 : LIGHTBOX_CLOSE_MS);
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
