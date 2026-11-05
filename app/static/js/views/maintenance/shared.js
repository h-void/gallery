// Eagerly-loaded micro-helpers for the maintenance views.
//
// The maintenance modules themselves load lazily (see lazy.js), but two kinds
// of helper must be available without downloading that bundle:
// - `downloadTemplateInputSelector` runs while the browse UI wires its events,
//   before any maintenance panel has been opened;
// - `movePanelScrollTop` / `restoreMovePanelScroll` read and restore the move
//   panel scroll position synchronously around an artist switch.
// They stay tiny so the browse bundle keeps them nearly for free; downloads.js
// and maintenance/index.js re-export them under their original names.

import { $ } from '../../utils.js';
import { state } from '../../store.js';

export const TEMPLATE_INPUTS = {
  folder: '#downloadFolderTemplateInput',
  image: '#downloadImageTemplateInput',
  attachment: '#downloadAttachmentTemplateInput',
};

export function downloadTemplateInputSelector(inputId) {
  return TEMPLATE_INPUTS[inputId] || '';
}

export function movePanelScrollTop() {
  const panel = $('#movePanel');
  return panel ? panel.scrollTop : 0;
}

export function restoreMovePanelScroll(top) {
  if (top == null) return;
  const panel = $('#movePanel');
  if (!panel) return;
  panel.scrollTop = top;
  requestAnimationFrame(() => { panel.scrollTop = top; });
}

export const DOWNLOAD_CALENDAR_OPEN_STORAGE_KEY = 'gallery.downloadCalendarOpen';

export function readDownloadCalendarOpen() {
  if (typeof localStorage === 'undefined') return {};
  try {
    const raw = localStorage.getItem(DOWNLOAD_CALENDAR_OPEN_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return parsed;
    }
  } catch (e) {}
  return {};
}

export function saveDownloadCalendarOpen(subId, isOpen) {
  const id = Number(subId);
  if (!Number.isFinite(id)) return;
  if (!state.downloadCalendarOpen) state.downloadCalendarOpen = {};
  state.downloadCalendarOpen[id] = Boolean(isOpen);
  if (typeof localStorage !== 'undefined') {
    try {
      localStorage.setItem(
        DOWNLOAD_CALENDAR_OPEN_STORAGE_KEY,
        JSON.stringify(state.downloadCalendarOpen)
      );
    } catch (e) {}
  }
}
