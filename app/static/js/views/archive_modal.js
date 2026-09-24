// Archive inspection and extraction modal.

import { $, $$, escHtml, formatSize, buttonIcon } from '../utils.js';
import { state } from '../store.js';
import { API } from '../api.js';
import { toast, logUiAction, collectUiLogContext } from '../logging.js';
import { refreshCurrentView } from '../events.js';

let currentItemId = null;
let currentArchiveData = null;
let archiveDialogOpener = null;

export function bindArchiveModal() {
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
      }
    });
  });

  // Extract button
  const extractBtn = $('#archiveExtractBtn');
  if (extractBtn) {
    extractBtn.addEventListener('click', executeArchiveExtraction);
  }
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
    if (summary) {
      summary.innerHTML = `<div class="archive-meta-line"><b>${escHtml(data.archive_name)}</b> (${formatSize(data.archive_size)}) \\u00b7 压缩包已加密</div>`;
    }
    if (tree) {
      tree.innerHTML = '<div class="archive-notice">文件名已加密，请输入密码后点击“解锁预览”。</div>';
    }
    return;
  }

  if (data.is_encrypted && pwdRow) {
    pwdRow.style.display = 'flex';
  } else if (pwdRow) {
    pwdRow.style.display = 'none';
  }

  // Summary header
  if (summary) {
    const s = data.stats || {};
    const imgCount = s.images || 0;
    const vidCount = s.videos || 0;
    const details = [];
    if (imgCount > 0) details.push(`${imgCount} 张图片`);
    if (vidCount > 0) details.push(`${vidCount} 个视频`);
    const detailsStr = details.length ? ` (${details.join('，')})` : '';

    summary.innerHTML = `
      <div class="archive-meta-line">
        <span class="archive-meta-badge">${escHtml(data.archive_name)}</span>
        <span class="archive-meta-sub">${formatSize(data.archive_size)} \\u00b7 共 ${s.total_files || 0} 个文件${detailsStr} \\u00b7 解压后约 ${formatSize(s.total_uncompressed_bytes || 0)}</span>
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
      ? `<div class="archive-more-notice">仅展示前 300 项，其余 ${entries.length - 300} 项在解压后可见</div>`
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

  const url = `/api/archives/entry?item_id=${currentItemId}&entry=${encodeURIComponent(entryPath)}${password ? `&password=${encodeURIComponent(password)}` : ''}`;
  container.style.display = 'block';
  container.innerHTML = `
    <div class="archive-preview-card">
      <div class="archive-preview-header">
        <span>${escHtml(entryPath.split('/').pop() || entryPath)}</span>
        <button class="btn btn-ghost btn-sm" type="button" id="archiveClosePreviewBtn">收起预览</button>
      </div>
      <div class="archive-preview-img-wrap">
        <img src="${url}" alt="" loading="lazy">
      </div>
    </div>
  `;

  $('#archiveClosePreviewBtn')?.addEventListener('click', () => {
    container.style.display = 'none';
    container.innerHTML = '';
  });
}

async function executeArchiveExtraction() {
  if (!currentItemId) return;

  const extractBtn = $('#archiveExtractBtn');
  const statusMsg = $('#archiveStatusMsg');
  const pwdInput = $('#archivePasswordInput');
  const targetModeRadio = $('input[name="archiveTargetMode"]:checked');
  const customFolderInput = $('#archiveCustomFolderName');
  const recycleCheckbox = $('#archiveRecycleSource');

  const password = pwdInput ? pwdInput.value : '';
  const targetMode = targetModeRadio ? targetModeRadio.value : 'current_folder';
  const customFolderName = targetMode === 'new_folder' && customFolderInput ? customFolderInput.value.trim() : null;
  const recycleSource = recycleCheckbox ? recycleCheckbox.checked : true;

  if (extractBtn) {
    extractBtn.disabled = true;
    extractBtn.textContent = '解压中';
  }
  if (statusMsg) {
    statusMsg.textContent = '正在解压整理文件';
    statusMsg.className = 'archive-status-msg';
  }

  try {
    const res = await API.postJson('/api/archives/extract', {
      item_id: currentItemId,
      password: password || undefined,
      target_mode: targetMode,
      custom_folder_name: customFolderName || undefined,
      recycle_source: recycleSource,
    });

    toast(`已解压 ${res.extracted_count || 0} 项文件${res.recycled_source ? '，原压缩包已移入回收站' : ''}`);
    closeArchiveModal();

    // Trigger instant refresh of current view so newly extracted files appear
    await refreshCurrentView();
  } catch (err) {
    const msg = err.message || String(err);
    if (statusMsg) {
      statusMsg.textContent = `解压失败：${msg}`;
      statusMsg.className = 'archive-status-msg error';
    }
  } finally {
    if (extractBtn) {
      extractBtn.disabled = false;
      extractBtn.textContent = '立即解压';
    }
  }
}
