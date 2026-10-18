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
