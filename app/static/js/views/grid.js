// Grid rendering: keyed card reconcile, lazy image/video queues, marquee
// selection, favorites, and the scroll anchor used by silent reloads.

import { API } from '../api.js';
import { state, isActionBusy, setActionBusy } from '../store.js';
import {
  $, $$, escHtml, buttonIcon, downloadFileName, renderTagNamesHtml,
} from '../utils.js';
import { computeJustifiedRows, justifiedRowHeight } from '../justified.js';
import { toast, logUiAction, collectSelectionLayoutLogContext } from '../logging.js';
import {
  renderTagSearchResults, isGlobalSearchActive, renderSidebar, loadItemsPreservingDepth,
} from './sidebar.js';
import { toggleSelect, selectOnly, applySelectionChange, scheduleCharacterTagSuggestions } from './editbar.js';
import { openLightbox } from './lightbox.js';
import { selectArtist } from '../router.js';

const SELECTION_MARQUEE_THRESHOLD_PX = 4;
const MAX_IMAGE_LOADS = 2;
const IMAGE_OBSERVER_ROOT_MARGIN = '480px';
const IMAGE_LOAD_TIMEOUT_MS = 12000;
const MAX_VIDEO_PREVIEW_LOADS = 1;
const VIDEO_PREVIEW_HOVER_DELAY_MS = 250;
const VIDEO_PREVIEW_LOAD_TIMEOUT_MS = 8000;

let activeVideoPreviewLoads = 0;
const pendingVideoPreviews = [];
let videoPreviewObserver = null;
let activeImageLoads = 0;
const pendingImageLoads = [];
let imageObserver = null;

export function syncSelectedCards(root = $('#grid')) {
  const scope = root || document;
  if (!scope.querySelectorAll) return;
  scope.querySelectorAll('.card[data-id]').forEach(card => {
    const cid = Number(card.dataset.id);
    const selected = state.selectedIds.has(cid);
    card.classList.toggle('selected', selected);
    const chk = card.querySelector('.check');
    if (chk) {
      chk.classList.toggle('visible', selected);
      chk.classList.toggle('checked', selected);
    }
  });
}

// Justified layout (§3.3): the browse grid (gallery/compact views) renders
// equal-height rows that preserve each item's aspect ratio. List view and
// duplicate groups keep the CSS-column layouts. Tag-only search chips live
// in their own container above the grid, so they no longer force flat rows.
function isJustifiedTarget(duplicatesOnly) {
  if (duplicatesOnly || state.view === 'list') return false;
  // Mobile column preference is a real slot count: 2/3 columns render the
  // fixed CSS grid (views.css repeats var(--mobile-grid-columns)) so narrow
  // slots never depend on a row-height hint; only the 1-column tier keeps the
  // justified full-width natural-aspect rows. Desktop stays justified.
  if (justifiedOptions().mobile && state.mobileColumns !== 1) return false;
  return true;
}

function justifiedOptions() {
  return {
    mobile: window.innerWidth <= 768,
    mobileColumns: state.mobileColumns,
  };
}

function cardEntry(item, idx) {
  const plain = buildItemCardHtml(item, idx);
  const key = itemCardRenderKey(plain);
  return {
    item,
    idx,
    key,
    html: plain.replace(
      `data-id="${item.id}"`,
      `data-id="${item.id}" data-rk="${key}"`,
    ),
  };
}

function collectGridCards(grid) {
  const byId = new Map();
  Array.from(grid.children).forEach(node => {
    if (node.classList && node.classList.contains('jrow')) {
      Array.from(node.children).forEach(card => {
        if (card.dataset && card.dataset.id) byId.set(card.dataset.id, card);
      });
    } else if (node.dataset && node.dataset.id) {
      byId.set(node.dataset.id, node);
    }
  });
  return byId;
}

function justifiedTargetRowHeight() {
  return justifiedRowHeight(state.view === 'compact' ? 'compact' : 'grid', justifiedOptions());
}

// Partition items into justified rows and park one node per item into its row
// cell. Nodes come from `resolveNode(id)`; sizes are applied as inline styles
// so render keys stay size-independent and relayouts reuse loaded thumbnails.
function applyJustifiedRows(grid, items, resolveNode, gridWidth, mode) {
  const width = gridWidth
    || grid.clientWidth
    || ($('#gridContainer') || {}).clientWidth
    || window.innerWidth;
  const rows = computeJustifiedRows(items, width, justifiedTargetRowHeight(), justifiedOptions());
  const fragment = document.createDocumentFragment();
  rows.forEach(row => {
    const rowDiv = document.createElement('div');
    rowDiv.className = 'jrow';
    if (row.trailing || row.single) rowDiv.dataset.trailing = '1';
    row.items.forEach(cell => {
      const node = resolveNode(String(cell.item.id), cell);
      if (!node) return;
      node.style.width = `${Math.round(cell.width)}px`;
      node.style.setProperty('--jh', `${Math.round(cell.height)}px`);
      rowDiv.appendChild(node);
    });
    fragment.appendChild(rowDiv);
  });
  if (mode === 'append') grid.appendChild(fragment);
  else grid.replaceChildren(fragment);
}

// Silent-refresh path for the justified grid: unchanged cards keep their live
// DOM, everything lands in freshly computed rows.
function justifyReplace(grid, entries, previousById) {
  const template = document.createElement('template');
  const nodesById = new Map();
  entries.forEach(entry => {
    const previous = previousById ? previousById.get(String(entry.item.id)) : null;
    if (previous && previous.dataset.rk === entry.key) {
      previous.dataset.idx = String(entry.idx);
      nodesById.set(String(entry.item.id), previous);
      return;
    }
    template.innerHTML = entry.html;
    const node = template.content.firstElementChild;
    if (node) nodesById.set(String(entry.item.id), node);
  });
  applyJustifiedRows(grid, entries.map(entry => entry.item), id => nodesById.get(id), 0, 'replace');
}

// Infinite-scroll append: the previous trailing row merges into the new page's
// layout so rows stay seamless; its cards are reused, thumbs included.
function justifyAppend(grid, entries, reuse) {
  const template = document.createElement('template');
  const entriesById = new Map(entries.map(entry => [String(entry.item.id), entry]));
  applyJustifiedRows(
    grid,
    entries.map(entry => entry.item),
    (id, cell) => {
      const node = reuse.get(id);
      if (node) return node;
      const entry = entriesById.get(id);
      if (!entry) return null;
      template.innerHTML = entry.html;
      return template.content.firstElementChild;
    },
    0,
    'append',
  );
}

// Window resizes re-partition rows once per settle; card nodes are reused as-is.
let justifiedResizeTimer = 0;
export function scheduleJustifiedRelayout() {
  clearTimeout(justifiedResizeTimer);
  justifiedResizeTimer = setTimeout(() => {
    const grid = $('#grid');
    if (!grid || !grid.classList.contains('justified') || !grid.childElementCount) return;
    const nodesById = collectGridCards(grid);
    applyJustifiedRows(grid, state.allItems, id => nodesById.get(id), 0, 'replace');
  }, 150);
}

export function renderGrid() {
  const grid = $('#grid');
  if (!grid) return;
  const items = state.allItems;
  // Sync the tag-results container first: it sits outside #grid and must also
  // hide on the early returns below (e.g. leaving an artist clears the view).
  const tagResultsVisible = renderTagSearchResults();
  const globalSearch = isGlobalSearchActive();

  if (!state.currentArtist && !globalSearch) {
    releaseAllImageLoads();
    releaseAllVideoPreviews();
    grid.innerHTML = '';
    renderLibraryEmptyState();
    return;
  }

  if (items.length === 0 && !tagResultsVisible) {
    releaseAllImageLoads();
    releaseAllVideoPreviews();
   grid.innerHTML = state.duplicatesOnly
      ? '<div class="empty library-empty-compact">当前范围没有重复文件</div>'
      : '<div class="empty library-empty-compact">当前范围没有文件</div>';
    return;
  }

  const justified = isJustifiedTarget(state.duplicatesOnly);
  grid.className = state.duplicatesOnly ? 'duplicate-groups' : 'grid';
  if (!state.duplicatesOnly && state.view === 'compact') grid.classList.add('compact');
  if (!state.duplicatesOnly && state.view === 'list') grid.classList.add('list');
  if (justified) grid.classList.add('justified');

  // Silent refreshes reuse unchanged cards so loaded thumbnails never flash.
  if (!state.duplicatesOnly && grid.childElementCount > 0) {
    reconcileGridCards(grid, items);
    return;
  }

  releaseAllImageLoads();
  releaseAllVideoPreviews();
  if (justified) {
    justifyReplace(grid, items.map((item, idx) => cardEntry(item, idx)), null);
  } else {
    const html = state.duplicatesOnly ? renderDuplicateGroups(items) : renderItemCards(items, 0);
    grid.innerHTML = html;
  }
  syncSelectedCards(grid);
  bindGridEvents();
  observeImages();
  bindGifPreviewEvents();
  bindVideoPreviewEvents();
  observeVideoPreviews();
}

export function appendItemsToGrid(items, startIndex) {
  if (!items.length) return;
  // ponytail: duplicate pages are small and rerendering keeps cross-page groups correct.
  if (state.duplicatesOnly) {
    renderGrid();
    return;
  }
  const grid = $('#grid');
  if (grid.classList.contains('justified')) {
    const reuse = new Map();
    const trailingRows = grid.querySelectorAll('.jrow[data-trailing]');
    const lastTrailing = trailingRows[trailingRows.length - 1];
    let tail = items;
    let tailStart = startIndex;
    if (lastTrailing) {
      Array.from(lastTrailing.children).forEach(card => reuse.set(card.dataset.id, card));
      const carriedIds = new Set(reuse.keys());
      const carried = state.allItems.slice(0, startIndex).filter(row => carriedIds.has(String(row.id)));
      lastTrailing.remove();
      tail = [...carried, ...tail];
      tailStart = startIndex - carried.length;
    }
    justifyAppend(grid, tail.map((item, offset) => cardEntry(item, tailStart + offset)), reuse);
    syncSelectedCards(grid);
    bindGridEvents();
    observeImages();
    bindGifPreviewEvents();
    bindVideoPreviewEvents();
    observeVideoPreviews();
    return;
  }
  grid.insertAdjacentHTML('beforeend', renderItemCards(items, startIndex));
  syncSelectedCards(grid);
  bindGridEvents();
  observeImages();
  bindGifPreviewEvents();
  bindVideoPreviewEvents();
  observeVideoPreviews();
}

// Leaving the justified engine: card nodes reused from a justified partition
// still carry the inline width/--jh of their old row cell. Clear them so the
// list/duplicate/fixed-grid CSS slots size themselves again.
function clearJustifiedInlineSizes(node) {
  if (!node || !node.style) return;
  node.style.width = '';
  node.style.removeProperty('--jh');
}

// Keyed card reuse for silent refreshes: a card whose render key is unchanged
// keeps its live DOM (loaded images included); only new or changed cards are
// rebuilt. Position stability is handled by the scroll anchor in this module.
function reconcileGridCards(grid, items) {
  const previousById = collectGridCards(grid);
  if (grid.classList.contains('justified')) {
    justifyReplace(grid, items.map((item, idx) => cardEntry(item, idx)), previousById);
    syncSelectedCards(grid);
    bindGridEvents();
    observeImages();
    bindGifPreviewEvents();
    bindVideoPreviewEvents();
    observeVideoPreviews();
    return;
  }
  const template = document.createElement('template');
  const nextNodes = [];
  items.forEach((item, idx) => {
    const plain = buildItemCardHtml(item, idx);
    const key = itemCardRenderKey(plain);
    const previous = previousById.get(String(item.id));
    if (previous && previous.dataset.rk === key) {
      previous.dataset.idx = String(idx);
      clearJustifiedInlineSizes(previous);
      nextNodes.push(previous);
      return;
    }
    template.innerHTML = plain.replace(
      `data-id="${item.id}"`,
      `data-id="${item.id}" data-rk="${key}"`,
    );
    const node = template.content.firstElementChild;
    if (node) nextNodes.push(node);
  });
  grid.replaceChildren(...nextNodes);
  syncSelectedCards(grid);
  bindGridEvents();
  observeImages();
  bindGifPreviewEvents();
  bindVideoPreviewEvents();
  observeVideoPreviews();
}

function renderDuplicateGroups(items) {
  const groups = [];
  items.forEach(item => {
    const hash = String(item.content_hash || '');
    const current = groups[groups.length - 1];
    if (!current || current.hash !== hash) groups.push({hash, items: []});
    groups[groups.length - 1].items.push(item);
  });
  const viewClass = state.view === 'compact' ? ' compact' : (state.view === 'list' ? ' list' : '');
  let startIndex = 0;
  return groups.map((group, index) => {
    const cards = renderItemCards(group.items, startIndex);
    startIndex += group.items.length;
    const count = state.hasMoreItems && index === groups.length - 1
      ? `已加载 ${group.items.length} 个相同文件`
      : `${group.items.length} 个相同文件`;
    return `<section class="duplicate-file-group">
      <div class="duplicate-file-group-title">${count}</div>
      <div class="duplicate-file-group-items grid${viewClass}">${cards}</div>
    </section>`;
  }).join('');
}

function renderRecognizedDatePreview(item) {
  const date = String(item.display_date || item.date || '').trim();
  if (!/^\d{4}-\d{2}(-\d{2})?$/.test(date)) return '';
  const cls = String(item.manual_date || '').trim() ? ' recognized-date manual' : ' recognized-date';
  return `<span class="${cls}" title="${String(item.manual_date || '').trim() ? '手动设置的日期' : '扫描检测到的日期'}">${escHtml(date)}</span>`;
}

export function itemCardRenderKey(html) {
  html = html.replace(/data-idx="\d+"/, 'data-idx=""');
  let hash = 5381;
  for (let i = 0; i < html.length; i += 1) {
    hash = ((hash << 5) + hash + html.charCodeAt(i)) | 0;
  }
  return `${(hash >>> 0).toString(36)}:${html.length.toString(36)}`;
}

function renderItemCardHtml(item, idx) {
  const html = buildItemCardHtml(item, idx);
  return html.replace(
    `data-id="${item.id}"`,
    `data-id="${item.id}" data-rk="${itemCardRenderKey(html)}"`,
  );
}

function renderItemCards(items, startIndex = 0) {
  let html = '';
  items.forEach((item, offset) => {
    html += renderItemCardHtml(item, startIndex + offset);
  });
  return html;
}

function mediaFilePlaceholder(label, icon = 'file') {
  return `<div class="media-file-icon">${buttonIcon(icon)}<span class="media-file-label">${label}</span></div>`;
}

function buildItemCardHtml(item, idx) {
    const selected = state.selectedIds.has(item.id);
    const sel = selected ? ' selected' : '';
    const chk = selected ? ' checked' : '';
    const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
    const isArchive = mediaType === 'archive' || item.is_archive;
    const fileUrl = API.fileUrl(item.file_path, fileVersionParam(item));
    const previewFileUrl = API.previewUrl(item.file_path, fileVersionParam(item), IMAGE_PREVIEW_MAX_EDGE);
    const checkVisible = selected ? ' visible' : '';
    const recognizedDateHtml = renderRecognizedDatePreview(item);
    const cardMetaFields = [renderTagNamesHtml(item.tags), recognizedDateHtml].filter(part => part);
    const cardMetaRow = cardMetaFields.length
      ? `<div class="date card-meta">${cardMetaFields.join('')}</div>`
      : '';
    const favoriteLabel = item.favorite ? '取消收藏' : '收藏';
    const favorite = `<button class="btn btn-ghost btn-icon card-favorite${item.favorite ? ' active' : ''}" type="button" data-favorite="${item.id}" title="${favoriteLabel}" aria-label="${favoriteLabel} ${escHtml(item.file_name)}" aria-pressed="${item.favorite ? 'true' : 'false'}"><span data-favorite-glyph aria-hidden="true">${buttonIcon(item.favorite ? 'starFilled' : 'star')}</span></button>`;
    const download = `<a class="btn btn-ghost btn-icon card-download" data-download href="${fileUrl}" download="${escHtml(downloadFileName(item))}" title="下载文件" aria-label="下载 ${escHtml(item.file_name)}">${buttonIcon('download')}</a>`;
    // Full file name stays reachable on every card (hover/AT tooltip) even
    // when the one-line title truncates.
    const titleRow = `<div class="card-title-row"><div class="role" title="${escHtml(item.file_name)}">${escHtml(item.file_name)}</div></div>`;
    const cardActions = `<div class="card-actions">${favorite}${download}</div>`;
    const artistJump = isGlobalSearchActive() && item.artist_id
      ? `<button class="btn btn-ghost artist-jump" type="button" data-artist-jump="${item.artist_id}" title="转到 ${escHtml(item.artist_name || '画师')}">转到画师</button>`
      : '';
    const previewUrl = escHtml(videoPreviewUrl(item));

    if (isArchive) {
      return `<div class="card archive-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        ${mediaFilePlaceholder('ZIP')}
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
    }
    if (mediaType === 'video') {
      return `<div class="card video-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        <div class="video-preview">
          <img class="video-thumb loading" data-preview-src="${previewUrl}" alt="" decoding="async" draggable="false">
          ${mediaFilePlaceholder('播放', 'play')}
        </div>
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
    }
    if (mediaType === 'source') {
      return `<div class="card source-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        ${mediaFilePlaceholder('SRC')}
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
    }
    if (mediaType === 'text') {
      return `<div class="card text-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        ${mediaFilePlaceholder('TXT')}
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
    }
    if (isGifItem(item)) {
      return `<div class="card gif-card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        <div class="gif-preview">
          <img class="thumb gif-thumb" data-gif-src="${fileUrl}" loading="lazy" decoding="async" alt="" draggable="false">
          <div class="gif-placeholder">${buttonIcon('image')}<span>GIF</span></div>
        </div>
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
    }
    return `<div class="card${sel}" data-id="${item.id}" data-idx="${idx}" role="button" tabindex="0">
        <div class="check${checkVisible}${chk}" data-check="${item.id}"></div>
        ${cardActions}
        <img class="thumb loading" data-src="${previewFileUrl}" decoding="async" fetchpriority="low" draggable="false">
        <div class="info">
          ${titleRow}
          ${cardMetaRow}
          ${artistJump}
        </div>
      </div>`;
}

const IMAGE_PREVIEW_MAX_EDGE = 512;

// Grid interactions use one delegated listener on the container: reconcile
// swaps card nodes freely and no per-card binding ever needs a rebind.
export function bindGridEvents() {
  const grid = $('#grid');
  if (!grid || grid.dataset.delegated === '1') return;
  grid.dataset.delegated = '1';
  // Tag-result chips live in #tagResults above the grid, so they need their
  // own delegation; bound once, it survives chip rebuilds.
  const tagResults = $('#tagResults');
  if (tagResults && tagResults.dataset.delegated !== '1') {
    tagResults.dataset.delegated = '1';
    tagResults.addEventListener('click', e => {
      const tagJump = e.target instanceof Element ? e.target.closest('[data-tag-jump]') : null;
      if (!tagJump || !tagResults.contains(tagJump)) return;
      jumpToTag(parseInt(tagJump.dataset.artistId), parseInt(tagJump.dataset.tagJump));
    });
  }
  grid.addEventListener('click', e => {
    const target = e.target instanceof Element ? e.target : null;
    if (!target) return;
    const download = target.closest('[data-download]');
    if (download && grid.contains(download)) {
      e.stopPropagation();
      return;
    }
    const favoriteBtn = target.closest('[data-favorite]');
    if (favoriteBtn && grid.contains(favoriteBtn)) {
      e.stopPropagation();
      const item = state.allItems.find(row => row.id === parseInt(favoriteBtn.dataset.favorite));
      toggleItemFavorite(item);
      return;
    }
    const artistJump = target.closest('[data-artist-jump]');
    if (artistJump && grid.contains(artistJump)) {
      e.stopPropagation();
      jumpToArtist(parseInt(artistJump.dataset.artistJump));
      return;
    }
    const chk = target.closest('.check');
    if (chk && grid.contains(chk)) {
      e.stopPropagation();
      if (state.suppressNextGridClick) {
        state.suppressNextGridClick = false;
        e.preventDefault();
        return;
      }
      toggleSelect(parseInt(chk.dataset.check), {reason: selectionModifierActive(e) ? 'ctrl_check' : 'check'});
      return;
    }
    const card = target.closest('.card');
    if (!card || !grid.contains(card)) return;
    activateCard(card, e);
  });
  grid.addEventListener('keydown', e => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const target = e.target instanceof Element ? e.target : null;
    if (!target) return;
    // Only proxy when the card itself is focused. A focused favorite button or
    // download link keeps its native Enter/Space behavior (one favorite toggle,
    // a real download) and must never open the lightbox or toggle selection.
    if (cardInnerControlTarget(target)) return;
    const card = target.closest('.card');
    if (!card || !grid.contains(card)) return;
    e.preventDefault();
    activateCard(card, e);
  });
  bindSelectionMarqueeEvents();
}

function activateCard(card, e) {
  // Edit mode (§4.1) is explicit: while it is on, a plain click toggles
  // selection and never opens the viewer. With it off, selection stays the
  // implicit state it has always been — any selection or a ctrl/meta click
  // keeps selecting, a plain click otherwise opens the viewer.
  if (state.editMode) {
    if (state.suppressNextGridClick) {
      state.suppressNextGridClick = false;
      e.preventDefault();
      return;
    }
    toggleSelect(parseInt(card.dataset.id), {reason: selectionModifierActive(e) ? 'ctrl_click' : 'edit_mode_click'});
    return;
  }
  if (state.selectedIds.size > 0 || selectionModifierActive(e)) {
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
}

export function selectionModifierActive(e) {
  return Boolean(state.selectionModifierDown || (e && (e.ctrlKey || e.metaKey)));
}

// Interactive controls that live inside a card and handle themselves: clicks,
// Enter and Space on them must never fall through to the card's own activate
// proxy (marquee, keyboard, and click delegation share this boundary).
function cardInnerControlTarget(target) {
  if (!(target instanceof Element)) return null;
  return target.closest('.check, [data-download], [data-artist-jump], [data-tag-jump], button, a, input, select, textarea, label');
}

function selectionMarqueeBlockedTarget(target) {
  return Boolean(cardInnerControlTarget(target));
}

function bindSelectionMarqueeEvents() {
  const container = $('#gridContainer');
  if (!container || container.dataset.selectionMarqueeBound === '1') return;
  container.dataset.selectionMarqueeBound = '1';
  container.addEventListener('pointerdown', startSelectionMarquee);
}

function startSelectionMarquee(e) {
  const container = $('#gridContainer');
  if (!container || state.selectedIds.size === 0) return;
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

export function isTaggableItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  return mediaType === 'image' || mediaType === 'video' || mediaType === 'source' || mediaType === 'archive' || mediaType === 'text' || item.is_archive;
}

export function syncFavoriteButtons(item) {
  if (!item) return;
  $$(`[data-favorite="${item.id}"]`).forEach(btn => {
    const favorite = Boolean(item.favorite);
    const label = favorite ? '取消收藏' : '收藏';
    btn.classList.toggle('active', favorite);
    btn.setAttribute('aria-pressed', String(favorite));
    btn.setAttribute('aria-label', `${label} ${item.file_name || ''}`.trim());
    btn.title = label;
    btn.disabled = isActionBusy('item-favorite', item.id);
    const glyph = btn.querySelector('[data-favorite-glyph]');
    if (glyph) glyph.innerHTML = buttonIcon(favorite ? 'starFilled' : 'star');
  });
}

export async function toggleItemFavorite(item) {
  if (!item || isActionBusy('item-favorite', item.id)) return;
  const previous = Boolean(item.favorite);
  const anchor = state.activeRole === '__favorites__' ? captureGridScrollAnchor() : null;
  setActionBusy('item-favorite', item.id, true);
  syncFavoriteButtons(item);
  try {
    const result = await API.putJson(`/api/items/${item.id}/favorite`, {favorite: !previous});
    item.favorite = Boolean(result.favorite);
    const loaded = state.allItems.find(row => row.id === item.id);
    if (loaded) loaded.favorite = item.favorite;
    if (state.stats) {
      state.stats.favorites = Math.max(0, Number(state.stats.favorites || 0) + (item.favorite ? 1 : -1));
      renderSidebar();
    }
    if (state.activeRole === '__favorites__' && !item.favorite) {
      await loadItemsPreservingDepth();
      restoreGridScrollAnchor(anchor);
    } else {
      syncFavoriteButtons(item);
    }
    toast(item.favorite ? '已收藏' : '已取消收藏', 'success');
  } catch (e) {
    toast('更新收藏失败', 'error');
  } finally {
    setActionBusy('item-favorite', item.id, false);
    syncFavoriteButtons(item);
  }
}

export async function jumpToArtist(artistId) {
  if (!artistId) return;
  state.searchScope = 'auto';
  syncSearchOptionsControl();
  await selectArtist(artistId);
}

export async function jumpToTag(artistId, tagId) {
  if (!artistId || !tagId) return;
  state.search = '';
  $('#searchInput').value = '';
  state.searchScope = 'auto';
  syncSearchOptionsControl();
  await selectArtist(artistId, {tagId});
}

export function fileVersionParam(item) {
  const size = Number(item.file_size || 0);
  const mtime = Math.round(Number(item.file_mtime || item.mtime || 0));
  return `${size}-${mtime}`;
}

export function isGifItem(item) {
  const mediaType = item.media_type || (item.is_archive ? 'archive' : 'image');
  if (mediaType !== 'image') return false;
  const name = (item.file_name || item.file_path || '').toLowerCase();
  return name.endsWith('.gif');
}

export function observeImages() {
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

  // Cached previews can already be decoded when the load handler is rebound.
  // Apply their intrinsic ratio so a mode switch cannot leave fallback rows.
  $$('#grid .thumb, #grid .video-thumb').forEach(img => {
    if (img.complete && img.naturalWidth > 0) updateItemAspectFromMedia(img);
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
    // Keep the source recoverable when a mode switch cancels this request.
    img.dataset.imageSource = src;
    img.src = src;
    img.removeAttribute('data-src');
  }
}

function clearImageLoadTimer(img) {
  if (!img.dataset.imageLoadTimer) return;
  clearTimeout(Number(img.dataset.imageLoadTimer));
  delete img.dataset.imageLoadTimer;
}

// A backfill can take a while on a large library. Use the decoded preview's
// intrinsic ratio for cards whose database dimensions are still missing, then
// let the normal justified relayout apply the measured width.
export function updateItemAspectFromMedia(media) {
  const width = Number(media && media.naturalWidth);
  const height = Number(media && media.naturalHeight);
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) return false;
  const card = media.closest ? media.closest('.card[data-id]') : null;
  const id = Number(card && card.dataset && card.dataset.id);
  const item = (state.allItems || []).find(row => Number(row.id) === id);
  if (!item || (Number(item.width) > 0 && Number(item.height) > 0)) return false;
  item.width = width;
  item.height = height;
  scheduleJustifiedRelayout();
  return true;
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
    updateItemAspectFromMedia(img);
  } else {
    delete img.dataset.imageLoaded;
    img.classList.add('failed');
  }
  if (clearSource) {
    img.removeAttribute('src');
  }
  pumpImageLoadQueue();
}

export function releaseAllImageLoads() {
  if (imageObserver) {
    imageObserver.disconnect();
    imageObserver = null;
  }
  const images = new Set([
    ...pendingImageLoads,
    ...$$('#grid .thumb[data-image-loading="1"], #grid .thumb[data-image-queued="1"], #grid .thumb[data-image-observed="1"]'),
  ]);
  pendingImageLoads.splice(0);
  activeImageLoads = 0;
  images.forEach(img => {
    if (!img) return;
    if (!img.dataset.src && img.dataset.imageSource) img.dataset.src = img.dataset.imageSource;
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
  img.onload = () => updateItemAspectFromMedia(img);
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
    updateItemAspectFromMedia(video);
  }
  if (clearSource) {
    video.removeAttribute('src');
  }
  pumpVideoPreviewQueue();
}

export function releaseAllVideoPreviewLoads() {
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

export function releaseAllVideoPreviews() {
  releaseAllVideoPreviewLoads();
}

// An anchored reload needs the first visible card id plus its pixel offset so
// the refreshed DOM can be scrolled back to the same artwork, not the same
// scrollTop. Cards carry stable data-id values, so the anchor survives reorders.
export function captureGridScrollAnchor() {
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

export function restoreGridScrollAnchor(anchor) {
  const container = $('#gridContainer');
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

// Late module-binding imports that close the sidebar/lightbox cycles; every
// call site here runs after all modules have evaluated.
import { renderLibraryEmptyState, syncSearchOptionsControl } from './sidebar.js';
import { isCurrentRequestSeq } from '../store.js';
