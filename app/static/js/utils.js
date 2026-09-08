// Pure helpers shared by every view. No DOM at import time, no state access
// beyond what callers pass in — this module must stay importable from tests.

export const UI_FIELD_SEPARATOR = ' \u00b7 ';

export const $ = (sel) => document.querySelector(sel);
export const $$ = (sel) => document.querySelectorAll(sel);

export const BUTTON_ICONS = {
  close: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12"></path><path d="M18 6 6 18"></path></svg>',
  trash: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"></path><path d="M8 6V4h8v2"></path><path d="M19 6l-1 14H6L5 6"></path><path d="M10 11v5"></path><path d="M14 11v5"></path></svg>',
  download: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v10"></path><path d="M8 9l4 4 4-4"></path><path d="M5 21h14"></path></svg>',
  file: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h8l4 4v14H6z"></path><path d="M14 3v5h5"></path></svg>',
  play: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 9 6-9 6z"></path></svg>',
  image: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"></rect><circle cx="8.5" cy="9" r="1.5"></circle><path d="m21 15-4.5-4.5L8 19"></path></svg>',
  refresh: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 1-15.5 6.2"></path><path d="M3 12A9 9 0 0 1 18.5 5.8"></path><path d="M18 2v4h4"></path><path d="M6 22v-4H2"></path></svg>',
  chevronDown: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"></path></svg>',
  star: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"><path d="M12 3.8l2.5 5.1 5.7.8-4.1 4 1 5.6-5.1-2.7-5.1 2.7 1-5.6-4.1-4 5.7-.8z"/></svg>',
  starFilled: '<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linejoin="round"><path d="M12 3.8l2.5 5.1 5.7.8-4.1 4 1 5.6-5.1-2.7-5.1 2.7 1-5.6-4.1-4 5.7-.8z"/></svg>',
  search: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"></circle><path d="m21 21-4.35-4.35"></path></svg>',
};

export function buttonIcon(name) {
  const icon = BUTTON_ICONS[name];
  return icon ? `<span class="btn-glyph" aria-hidden="true">${icon}</span>` : '';
}

export function escHtml(s) {
  if (s === null || s === undefined || s === '') return '';
  const text = typeof s === 'string' ? s : String(s);
  return text.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

export const mergeTagsByNameCollator = new Intl.Collator(undefined, {numeric: true, sensitivity: 'base'});

export function compareNameParts(a = '', b = '') {
  return mergeTagsByNameCollator.compare(a || '', b || '');
}

export function compareCharacterNames(a = '', b = '') {
  return compareNameParts(a, b);
}

export function searchableTextMatches(query, ...values) {
  const needle = String(query || '').trim().toLowerCase();
  if (!needle) return true;
  const compactNeedle = needle.replace(/\s+/g, '');
  return values.some(value => {
    const text = String(value || '').toLowerCase();
    const compactText = text.replace(/\s+/g, '');
    return text.includes(needle) || (compactNeedle && compactText.includes(compactNeedle));
  });
}

export function artistIdNumber(value) {
  const id = Number(value);
  return Number.isSafeInteger(id) && id > 0 ? id : null;
}

export function validBrowseDate(value) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value || '')) return '';
  const date = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(date.getTime()) && date.toISOString().slice(0, 10) === value ? value : '';
}

export function folderTreeHasPath(node, path) {
  if (!node || typeof node !== 'object' || Array.isArray(node)) return false;
  if (node.path === path) return true;
  const children = Array.isArray(node.children) ? node.children : [];
  return children.some(child => folderTreeHasPath(child, path));
}

export function joinUiMeta(parts) {
  return parts.map(part => String(part || '').trim()).filter(Boolean).join(UI_FIELD_SEPARATOR);
}

export function formatSize(bytes) {
  // A reported number is formatted; anything missing or unusable stays 未知 so
  // a missing size is never presented as a real zero-byte value.
  if (bytes === null || bytes === undefined || bytes === '') return '未知';
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '未知';
  if (value === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let scaled = value;
  let i = 0;
  while (scaled >= 1024 && i < units.length - 1) {
    scaled /= 1024;
    i += 1;
  }
  const text = i === 0 ? String(Math.round(scaled)) : scaled.toFixed(1).replace(/\.0$/, '');
  return `${text} ${units[i]}`;
}

// Compatibility alias kept for legacy callers. Output matches formatSize exactly.
export function formatBytes(bytes) { return formatSize(bytes); }

export function formatHealthTime(timestamp) {
  if (!timestamp) return '无记录';
  const date = new Date(Number(timestamp) * 1000);
  if (Number.isNaN(date.getTime())) return '无记录';
  return date.toLocaleString();
}

export function downloadFileName(item) {
  return (item.file_name || 'image').replace(/[\\/:*?"<>|]/g, '_');
}

// Card and lightbox metadata: one node per tag name so fields and names are
// separated by CSS spacing instead of the global middle-dot separator.
export function renderTagNamesHtml(tags) {
  if (!tags || tags.length === 0) return '<span class="meta-tag">未加标签</span>';
  return tags.map(t => `<span class="meta-tag">${escHtml(t.name)}</span>`).join('');
}

export async function copyText(text) {
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

export function debounce(fn, ms) {
  let timer;
  return (...args) => { clearTimeout(timer); timer = setTimeout(() => fn(...args), ms); };
}

export function isAbortError(error) {
  return Boolean(error && (error.name === 'AbortError' || error.code === 20));
}
