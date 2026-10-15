// Archive inspection and extraction modal.

import { $, $$, escHtml, formatSize, buttonIcon } from '../utils.js';
import { state } from '../store.js';
import { API, scheduleFolderRenamesRefresh } from '../api.js';
import { toast, logUiAction, collectUiLogContext } from '../logging.js';
import { t } from '../i18n.js';
import { refreshCurrentView, applyMode } from '../events.js';

let currentItemId = null;
let currentArchiveData = null;
let archiveDialogOpener = null;

export function bindArchiveModal() {
  bindBatchArchiveModal();
  const dialog = $('#archiveDialog');
  if (!dialog || dialog.dataset.bound) return;
  dialog.dataset.bound = '1';

  const closeBtn = $('#archiveDialogCloseBtn');
  if (closeBtn) {
    closeBtn.addEventListener('click', closeArchiveModal);
  }
  const cancelBtn = $('#archiveCancelBtn');
  if (cancelBtn) {
    cancelBtn.addEventListener('click', closeArchiveModal);
  }

  dialog.addEventListener('cancel', e => {
    e.preventDefault();
    closeArchiveModal();
  });

  dialog.addEventListener('click', e => {
    if (e.target === dialog) closeArchiveModal();
  });

  // Password retry button
  const retryBtn = $('#archivePasswordRetryBtn');
  if (retryBtn) {
    retryBtn.addEventListener('click', () => {
      const pwd = $('#archivePasswordInput')?.value || '';
      if (currentItemId) loadArchiveInspection(currentItemId, pwd);
    });
  }

  const pwdInput = $('#archivePasswordInput');
  if (pwdInput) {
    pwdInput.addEventListener('keydown', e => {
      if (e.key === 'Enter') {
        e.preventDefault();
        const pwd = pwdInput.value || '';
        if (currentItemId) loadArchiveInspection(currentItemId, pwd);
      }
    });
  }

  // Radio button toggle for custom folder name
  $$('input[name="archiveTargetMode"]').forEach(radio => {
    radio.addEventListener('change', () => {
      const customInput = $('#archiveCustomFolderName');
      if (customInput) {
        customInput.style.display = radio.value === 'new_folder' ? 'block' : 'none';
        if (radio.value === 'new_folder') {
          customInput.focus?.();
          customInput.select?.();
        }
      }
    });
  });

  // Extract button
  const extractBtn = $('#archiveExtractBtn');
  if (extractBtn) {
    extractBtn.addEventListener('click', executeArchiveExtraction);
  }

  dialog.addEventListener('keydown', e => {
    const inlinePreview = $('#archiveInlinePreview');
    if (!inlinePreview || inlinePreview.style.display === 'none') return;
    if (e.target && (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA')) return;
    if (e.key === 'ArrowLeft') {
      const prevBtn = $('#archivePrevPreviewBtn');
      if (prevBtn && !prevBtn.disabled) {
        e.preventDefault();
        prevBtn.click();
      }
    } else if (e.key === 'ArrowRight') {
      const nextBtn = $('#archiveNextPreviewBtn');
      if (nextBtn && !nextBtn.disabled) {
        e.preventDefault();
        nextBtn.click();
      }
    }
  });
}

export function openArchiveModal(itemId, opener = null) {
  const dialog = $('#archiveDialog');
  if (!dialog) return;

  archiveDialogOpener = opener;
  currentItemId = itemId;
  currentArchiveData = null;

  // Reset inputs
  const pwdInput = $('#archivePasswordInput');
  if (pwdInput) pwdInput.value = '';
  const pwdRow = $('#archivePasswordRow');
  if (pwdRow) pwdRow.style.display = 'none';

  const customFolderInput = $('#archiveCustomFolderName');
  if (customFolderInput) {
    customFolderInput.style.display = 'none';
    customFolderInput.value = '';
  }

  const currentFolderRadio = $('input[name="archiveTargetMode"][value="current_folder"]');
  if (currentFolderRadio) currentFolderRadio.checked = true;

  const recycleCheckbox = $('#archiveRecycleSource');
  if (recycleCheckbox) recycleCheckbox.checked = true;

  // One-shot option: it applies to the next extraction only, so a fresh dialog
  // always starts with it unchecked.
  const autoArchiveInput = $('#archiveAutoArchive');
  if (autoArchiveInput) autoArchiveInput.checked = false;

  const statusMsg = $('#archiveStatusMsg');
  if (statusMsg) {
    statusMsg.textContent = '';
    statusMsg.className = 'archive-status-msg';
  }

  const tree = $('#archiveEntriesTree');
  if (tree) tree.innerHTML = '<div class="archive-loading">读取中</div>';

  const summary = $('#archiveInfoSummary');
  if (summary) summary.innerHTML = '';

  dialog.showModal();
  loadArchiveInspection(itemId, '');
}

export function closeArchiveModal() {
  const dialog = $('#archiveDialog');
  if (dialog?.open) {
    dialog.close();
  }
  if (archiveDialogOpener && document.contains(archiveDialogOpener)) {
    archiveDialogOpener.focus();
  }
  archiveDialogOpener = null;
  currentItemId = null;
  currentArchiveData = null;
}

async function loadArchiveInspection(itemId, password = '') {
  const summary = $('#archiveInfoSummary');
  const tree = $('#archiveEntriesTree');
  const pwdRow = $('#archivePasswordRow');
  const statusMsg = $('#archiveStatusMsg');

  if (tree) tree.innerHTML = '<div class="archive-loading">读取中</div>';
  if (statusMsg) statusMsg.textContent = '';

  try {
    const data = await API.postJson('/api/archives/inspect', {
      item_id: itemId,
      password: password || undefined,
    });

    currentArchiveData = data;
    renderArchiveInspection(data, password);
  } catch (err) {
    const msg = err.message || String(err);
    if (tree) tree.innerHTML = '';
    if (statusMsg) {
      statusMsg.textContent = msg;
      statusMsg.className = 'archive-status-msg error';
    }
  }
}

function renderArchiveInspection(data, currentPassword) {
  const summary = $('#archiveInfoSummary');
  const tree = $('#archiveEntriesTree');
  const pwdRow = $('#archivePasswordRow');
  const customFolderInput = $('#archiveCustomFolderName');

  const titleEl = $('#archiveDialogTitle');
  if (titleEl) {
    titleEl.textContent = data.archive_name || '压缩包预览与解压';
  }

  // Pre-fill folder name if empty
  if (customFolderInput && !customFolderInput.value) {
    const stem = (data.archive_name || '').replace(/\.[^/.]+$/, '');
    customFolderInput.value = stem;
  }

  // Handle encryption
  if (data.header_encrypted) {
    if (pwdRow) pwdRow.style.display = 'flex';
    $('#archivePasswordInput')?.focus?.();
    if (summary) {
      summary.innerHTML = `<div class="archive-meta-line"><b>${escHtml(data.archive_name)}</b> (${t(formatSize(data.archive_size))}) \u00b7 <span>${t('压缩包已加密')}</span></div>`;
    }
    if (tree) {
      tree.innerHTML = '<div class="archive-notice">文件名已加密，请输入密码后点击“解锁预览”。</div>';
    }
    return;
  }

  if (data.is_encrypted && pwdRow) {
    pwdRow.style.display = 'flex';
    if (!currentPassword) $('#archivePasswordInput')?.focus?.();
  } else if (pwdRow) {
    pwdRow.style.display = 'none';
  }

  // Summary header
  if (summary) {
    const s = data.stats || {};
    const imgCount = s.images || 0;
    const vidCount = s.videos || 0;
    const details = [];
    if (imgCount > 0) details.push(t('{count} 张图片', {count: imgCount}));
    if (vidCount > 0) details.push(t('{count} 个视频', {count: vidCount}));
    const detailsStr = details.length === 2 ? ` (${t('{a}，{b}', {a: details[0], b: details[1]})})` : (details.length === 1 ? ` (${details[0]})` : '');

    summary.innerHTML = `
      <div class="archive-meta-line">
        <span class="archive-meta-badge">${escHtml(data.archive_name)}</span>
        <span class="archive-meta-sub">${formatSize(data.archive_size)} \u00b7 ${t('共 {total} 个文件', {total: s.total_files || 0})}${detailsStr} \u00b7 ${t('解压后约 {size}', {size: formatSize(s.total_uncompressed_bytes || 0)})}</span>
      </div>
    `;
  }

  // File list
  if (tree) {
    const entries = data.entries || [];
    if (entries.length === 0) {
      tree.innerHTML = '<div class="archive-empty">压缩包内无可见文件</div>';
      return;
    }

    const rows = entries.slice(0, 300).map(entry => {
      const isImg = entry.media_type === 'image';
      const icon = entry.is_dir
        ? '📁'
        : (isImg ? '🖼️' : (entry.media_type === 'video' ? '🎬' : (entry.media_type === 'source' ? '🎨' : (entry.media_type === 'text' ? '📄' : '📦'))));

      const sizeStr = entry.is_dir ? '' : formatSize(entry.size);
      const entryClass = isImg ? 'archive-entry-item clickable' : 'archive-entry-item';
      const previewAttr = isImg ? `data-preview-entry="${escHtml(entry.path)}"` : '';

      return `
        <div class="${entryClass}" ${previewAttr} title="${escHtml(entry.path)}">
          <span class="archive-entry-icon">${icon}</span>
          <span class="archive-entry-path">${escHtml(entry.path)}</span>
          <span class="archive-entry-size">${sizeStr}</span>
        </div>
      `;
    }).join('');

    const moreNotice = entries.length > 300
      ? `<div class="archive-more-notice">${t('仅展示前 300 项，其余 {count} 项在解压后可见', {count: entries.length - 300})}</div>`
      : '';

    tree.innerHTML = `<div class="archive-list-container">${rows}</div>${moreNotice}<div id="archiveInlinePreview" class="archive-inline-preview" style="display:none"></div>`;

    // Click image entry to preview inline
    tree.querySelectorAll('[data-preview-entry]').forEach(el => {
      el.addEventListener('click', () => {
        const entryPath = el.dataset.previewEntry;
        showInlineEntryPreview(entryPath, currentPassword);
      });
    });
  }
}

function showInlineEntryPreview(entryPath, password) {
  const container = $('#archiveInlinePreview');
  if (!container || !currentItemId) return;

  const tree = $('#archiveEntriesTree');
  const previewableEls = tree ? Array.from(tree.querySelectorAll('[data-preview-entry]')) : [];
  previewableEls.forEach(el => el.classList.toggle('active', el.dataset.previewEntry === entryPath));
  const currentIdx = previewableEls.findIndex(el => el.dataset.previewEntry === entryPath);
  const hasPrev = currentIdx > 0;
  const hasNext = currentIdx >= 0 && currentIdx < previewableEls.length - 1;
  const navBtns = previewableEls.length > 1
    ? `<button class="btn btn-ghost btn-sm" type="button" id="archivePrevPreviewBtn" ${hasPrev ? '' : 'disabled'}>${t('上一张')}</button>
       <button class="btn btn-ghost btn-sm" type="button" id="archiveNextPreviewBtn" ${hasNext ? '' : 'disabled'}>${t('下一张')}</button>`
    : '';

  const url = `/api/archives/entry?item_id=${currentItemId}&entry=${encodeURIComponent(entryPath)}${password ? `&password=${encodeURIComponent(password)}` : ''}`;
  container.style.display = 'block';
  container.innerHTML = `
    <div class="archive-preview-card">
      <div class="archive-preview-header">
        <span data-i18n-skip>${escHtml(entryPath.split('/').pop() || entryPath)}${previewableEls.length > 1 ? ` (${currentIdx + 1} / ${previewableEls.length})` : ''}</span>
        <div class="archive-preview-actions">
          ${navBtns}
          <button class="btn btn-ghost btn-sm" type="button" id="archiveClosePreviewBtn">收起预览</button>
        </div>
      </div>
      <div class="archive-preview-img-wrap">
        <img id="archiveInlinePreviewImg" src="${url}" alt="" loading="lazy">
      </div>
    </div>
  `;

  const previewImg = $('#archiveInlinePreviewImg');
  if (previewImg) {
    previewImg.addEventListener('error', () => {
      const wrap = previewImg.closest('.archive-preview-img-wrap');
      if (wrap) wrap.innerHTML = `<div class="archive-notice">${t('预览加载失败（可能受密码保护或超出单文件预览大小限制）')}</div>`;
    });
  }
  $('#archivePrevPreviewBtn')?.addEventListener('click', () => {
    if (hasPrev) showInlineEntryPreview(previewableEls[currentIdx - 1].dataset.previewEntry, password);
  });
  $('#archiveNextPreviewBtn')?.addEventListener('click', () => {
    if (hasNext) showInlineEntryPreview(previewableEls[currentIdx + 1].dataset.previewEntry, password);
  });
  $('#archiveClosePreviewBtn')?.addEventListener('click', () => {
    previewableEls.forEach(el => el.classList.remove('active'));
    container.style.display = 'none';
    container.innerHTML = '';
  });
  container.scrollIntoView?.({behavior: 'smooth', block: 'nearest'});
}


export function showExtractionProgress(info = {}) {
  const panel = $('#progressPanel');
  if (!panel) return;
  if (panel._hideProgressTimer) {
    clearTimeout(panel._hideProgressTimer);
    panel._hideProgressTimer = null;
  }
  panel.classList.remove('hiding');
  panel.classList.add('visible');
  const current = Number(info.current || 0);
  const total = Number(info.total || 0);
  const titleEl = $('#progressTitle');
  if (titleEl) titleEl.textContent = t('正在解压');
  const pct = total > 0 ? Math.round((current / total) * 100) : (info.indeterminate ? 100 : 0);
  const fillEl = $('#progressFill');
  if (fillEl) fillEl.style.width = Math.min(Math.max(pct, 0), 100) + '%';
  const countEl = $('#progressCount');
  if (countEl) countEl.textContent = total > 1 ? `${current} / ${total}` : (total === 1 ? '1 / 1' : '');
  const pathEl = $('#progressPath');
  if (pathEl) {
    pathEl.textContent = info.currentFile || '';
    pathEl.title = info.currentFile || '';
  }
}

export function hideExtractionProgress() {
  const panel = $('#progressPanel');
  if (!panel) return;
  if (panel._hideProgressTimer) return;
  panel.classList.add('hiding');
  const reducedMotion = typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  panel._hideProgressTimer = setTimeout(() => {
    panel._hideProgressTimer = null;
    panel.classList.remove('visible', 'hiding');
  }, reducedMotion ? 0 : 150);
}

async function executeArchiveExtraction() {
  if (!currentItemId) return;

  const extractBtn = $('#archiveExtractBtn');
  const statusMsg = $('#archiveStatusMsg');
  const pwdInput = $('#archivePasswordInput');
  const targetModeRadio = $('input[name="archiveTargetMode"]:checked');
  const customFolderInput = $('#archiveCustomFolderName');
  const recycleCheckbox = $('#archiveRecycleSource');
  const autoArchiveInput = $('#archiveAutoArchive');

  const password = pwdInput ? pwdInput.value : '';
  const targetMode = targetModeRadio ? targetModeRadio.value : 'current_folder';
  const customFolderName = targetMode === 'new_folder' && customFolderInput ? customFolderInput.value.trim() : null;
  const recycleSource = recycleCheckbox ? recycleCheckbox.checked : true;
  const autoArchive = Boolean(autoArchiveInput && autoArchiveInput.checked);
  // Capture the owning artist now: the view refresh below can swap or clear
  // state.currentArtist, and the plan refresh must still hit the right artist.
  const archiveArtistId = state.currentArtist?.id || null;

  if (extractBtn) {
    extractBtn.disabled = true;
    extractBtn.textContent = '解压中';
  }
  if (statusMsg) {
    statusMsg.textContent = '正在解压整理文件';
    statusMsg.className = 'archive-status-msg';
  }
  showExtractionProgress({
    current: 1,
    total: 1,
    currentFile: currentArchiveData?.archive_name || '',
    indeterminate: true,
  });

  try {
    const res = await API.postJson('/api/archives/extract', {
      item_id: currentItemId,
      password: password || undefined,
      target_mode: targetMode,
      custom_folder_name: customFolderName || undefined,
      recycle_source: recycleSource,
    });

    toast(res.recycled_source
      ? t('已解压 {count} 项文件，原压缩包已移入回收站', {count: res.extracted_count || 0})
      : t('已解压 {count} 项文件', {count: res.extracted_count || 0}));
    closeArchiveModal();

    // Schedule before the view refresh: the scheduler debounces it anyway and
    // a failing refresh must not drop the plan update.
    if (autoArchive && archiveArtistId) {
      scheduleFolderRenamesRefresh(archiveArtistId);
    }

    // Trigger instant refresh of current view so newly extracted files appear
    await refreshCurrentView();
  } catch (err) {
    const msg = err.message || String(err);
    if (statusMsg) {
      statusMsg.textContent = t('解压失败：{msg}', {msg});
      statusMsg.className = 'archive-status-msg error';
    }
  } finally {
    hideExtractionProgress();
    // The plan-generation checkbox is a one-shot option for this extraction,
    // success or failure alike.
    if (autoArchiveInput) autoArchiveInput.checked = false;
    if (extractBtn) {
      extractBtn.disabled = false;
      extractBtn.textContent = '立即解压';
    }
  }
}


let batchArchiveOptions = null;
let isBatchExtracting = false;
let batchExtractCancelled = false;
let lastBatchArchivePassword = '';

export function isBatchArchiveExtracting() {
  return isBatchExtracting;
}

export function stopBatchArchiveExtraction() {
  if (!isBatchExtracting) return false;
  batchExtractCancelled = true;
  return true;
}

function updateBatchArchiveSummaryCount() {
  const dialog = $('#batchArchiveDialog');
  const summary = $('#batchArchiveInfoSummary');
  if (!dialog || !summary) return;
  const selectedArchives = Array.isArray(batchArchiveOptions?.selectedArchives) ? batchArchiveOptions.selectedArchives : [];
  const totalArtistArchives = Number(state.stats?.archives || 0);
  const scopeRadio = dialog.querySelector('input[name="batchArchiveScope"]:checked');
  const scope = scopeRadio ? scopeRadio.value : 'selected';
  const count = scope === 'all_artist' ? totalArtistArchives : selectedArchives.length;
  summary.innerHTML = `<div class="archive-meta-line"><b>${t('批量解压压缩包')}</b> \u00b7 <span>${t('已准备解压 {count} 个压缩包', {count})}</span></div>`;
}

export function bindBatchArchiveModal() {
  const dialog = $('#batchArchiveDialog');
  if (!dialog || dialog.dataset.bound) return;
  dialog.dataset.bound = '1';

  const closeBtn = $('#batchArchiveDialogCloseBtn');
  if (closeBtn) closeBtn.addEventListener('click', closeBatchArchiveModal);
  const cancelBtn = $('#batchArchiveCancelBtn');
  if (cancelBtn) cancelBtn.addEventListener('click', closeBatchArchiveModal);

  dialog.addEventListener('cancel', e => {
    e.preventDefault();
    closeBatchArchiveModal();
  });
  dialog.addEventListener('click', e => {
    if (e.target === dialog) closeBatchArchiveModal();
  });

  dialog.querySelectorAll('input[name="batchArchiveScope"]').forEach(radio => {
    radio.addEventListener('change', updateBatchArchiveSummaryCount);
  });

  const extractBtn = $('#batchArchiveExtractBtn');
  if (extractBtn) extractBtn.addEventListener('click', executeBatchArchiveExtraction);
}

export function openBatchArchiveModal(options = {}) {
  const dialog = $('#batchArchiveDialog');
  if (!dialog) return;

  batchArchiveOptions = options;
  const selectedArchives = Array.isArray(options.selectedArchives) ? options.selectedArchives : [];
  const artistId = options.artistId || (state.currentArtist ? state.currentArtist.id : null);
  const totalArtistArchives = Number(state.stats?.archives || 0);

  const scopeWrap = $('#batchArchiveScopeToggleWrap');
  const selectedRadio = scopeWrap ? scopeWrap.querySelector('input[value="selected"]') : null;
  const artistRadio = scopeWrap ? scopeWrap.querySelector('input[value="all_artist"]') : null;
  const selectedLabel = $('#batchArchiveScopeSelectedLabel');
  const artistLabel = $('#batchArchiveScopeArtistLabel');

  if (selectedRadio) selectedRadio.disabled = selectedArchives.length === 0;

  if (selectedArchives.length > 0 && artistId && totalArtistArchives > selectedArchives.length) {
    if (scopeWrap) scopeWrap.style.display = 'flex';
    if (selectedRadio) selectedRadio.checked = true;
    if (selectedLabel) selectedLabel.textContent = t('解压选中的压缩包 ({count})', {count: selectedArchives.length});
    if (artistLabel) artistLabel.textContent = t('解压当前画师全部压缩包 ({count})', {count: totalArtistArchives});
  } else if (selectedArchives.length === 0 && artistId && totalArtistArchives > 0) {
    if (scopeWrap) scopeWrap.style.display = 'flex';
    if (artistRadio) artistRadio.checked = true;
    if (selectedLabel) selectedLabel.textContent = t('解压选中的压缩包 (0)');
    if (artistLabel) artistLabel.textContent = t('解压当前画师全部压缩包 ({count})', {count: totalArtistArchives});
  } else {
    if (scopeWrap) scopeWrap.style.display = 'none';
    if (selectedRadio) selectedRadio.checked = true;
  }

  updateBatchArchiveSummaryCount();

  const pwdInput = $('#batchArchivePasswordInput');
  if (pwdInput) pwdInput.value = lastBatchArchivePassword || '';

  const targetRadio = dialog.querySelector('input[name="batchArchiveTargetMode"][value="new_folder"]');
  if (targetRadio) targetRadio.checked = true;

  const recycleCheckbox = $('#batchArchiveRecycleSource');
  if (recycleCheckbox) recycleCheckbox.checked = true;

  const autoArchiveCheckbox = $('#batchArchiveAutoArchive');
  if (autoArchiveCheckbox) autoArchiveCheckbox.checked = false;

  const statusMsg = $('#batchArchiveStatusMsg');
  if (statusMsg) {
    statusMsg.textContent = '';
    statusMsg.className = 'archive-status-msg';
  }

  dialog.showModal();
}

export function closeBatchArchiveModal() {
  const dialog = $('#batchArchiveDialog');
  if (dialog?.open) dialog.close();
}

export async function executeBatchArchiveExtraction() {
  if (isBatchExtracting) return;
  const dialog = $('#batchArchiveDialog');
  const statusMsg = $('#batchArchiveStatusMsg');

  const pwdInput = $('#batchArchivePasswordInput');
  const targetModeRadio = dialog ? dialog.querySelector('input[name="batchArchiveTargetMode"]:checked') : null;
  const scopeRadio = dialog ? dialog.querySelector('input[name="batchArchiveScope"]:checked') : null;
  const recycleCheckbox = $('#batchArchiveRecycleSource');
  const autoArchiveInput = $('#batchArchiveAutoArchive');

  const password = pwdInput ? pwdInput.value.trim() : '';
  lastBatchArchivePassword = password;
  const targetMode = targetModeRadio ? targetModeRadio.value : 'new_folder';
  const scope = scopeRadio ? scopeRadio.value : 'selected';
  const recycleSource = recycleCheckbox ? recycleCheckbox.checked : true;
  const autoArchive = Boolean(autoArchiveInput && autoArchiveInput.checked);

  let targetArchives = [];
  const artistId = batchArchiveOptions?.artistId || (state.currentArtist ? state.currentArtist.id : null);

  if (scope === 'all_artist' && artistId) {
    if (statusMsg) {
      statusMsg.textContent = t('正在准备批量解压');
      statusMsg.className = 'archive-status-msg';
    }
    try {
      const res = await API.get(`/api/items?artist_id=${artistId}&archive_only=true&limit=500`);
      targetArchives = Array.isArray(res?.items)
        ? res.items.filter(item => item && (item.media_type === 'archive' || item.is_archive))
        : [];
    } catch (e) {
      toast('获取画师压缩包列表失败：' + (e.message || e), 'error');
      return;
    }
  } else {
    targetArchives = Array.isArray(batchArchiveOptions?.selectedArchives)
      ? batchArchiveOptions.selectedArchives
      : [];
  }

  if (targetArchives.length === 0) {
    toast(t('没有需要解压的压缩包'), 'warn');
    closeBatchArchiveModal();
    return;
  }

  closeBatchArchiveModal();
  isBatchExtracting = true;
  batchExtractCancelled = false;
  const total = targetArchives.length;
  let succeeded = 0;
  let skippedPwd = 0;
  let failed = 0;

  const stopBtn = $('#stopScanBtn');
  if (total > 1 && stopBtn) stopBtn.style.display = '';

  showExtractionProgress({
    current: 0,
    total,
    currentFile: targetArchives[0]?.file_name || targetArchives[0]?.file_path || '',
  });

  try {
    for (let i = 0; i < total; i++) {
      if (batchExtractCancelled) break;
      const archive = targetArchives[i];
      const fileName = archive.file_name || archive.file_path || `archive-${archive.id}`;
      showExtractionProgress({
        current: i + 1,
        total,
        currentFile: fileName,
      });

      try {
        await API.postJson('/api/archives/extract', {
          item_id: archive.id,
          password: password || undefined,
          target_mode: targetMode,
          recycle_source: recycleSource,
        });
        succeeded += 1;
      } catch (err) {
        const msg = String(err.message || err);
        if (msg.includes('password') || msg.includes('密码') || msg.includes('encrypted') || msg.includes('加密')) {
          skippedPwd += 1;
        } else {
          failed += 1;
        }
      }
    }
  } finally {
    isBatchExtracting = false;
    if (stopBtn && !state.scanning) stopBtn.style.display = 'none';
    hideExtractionProgress();
  }

  if (autoArchive && artistId) {
    scheduleFolderRenamesRefresh(artistId);
  }

  // Clear selections and refresh views
  const { applySelectionChange } = await import('./editbar.js');
  applySelectionChange([], {reason: 'batch_extract', schedule: false});
  await refreshCurrentView();

  if (batchExtractCancelled) {
    toast(t('批量解压已停止：已完成 {succeeded} / {total} 个', {succeeded, total}), 'info');
  } else if (skippedPwd > 0) {
    toast(recycleSource
      ? t('批量解压完成：成功 {succeeded} 个，跳过 {skipped} 个需密码，原文件已移入回收站', {succeeded, skipped: skippedPwd})
      : t('批量解压完成：成功 {succeeded} 个，跳过 {skipped} 个需密码', {succeeded, skipped: skippedPwd}), 'warn');
  } else {
    toast(recycleSource
      ? t('已解压 {count} 个压缩包，原文件已移入回收站', {count: succeeded})
      : t('已解压 {count} 个压缩包', {count: succeeded}), 'success');
  }
}
