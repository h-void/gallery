// User-interface logging and toast feedback. Everything here must stay
// diagnosable per the UI logging rules: bounded payloads, no credentials, and
// every user-visible toast goes through logUiAction.

import { $, $$ } from './utils.js';
import { state } from './store.js';

const FRONTEND_ERROR_LOG_LIMIT = 20;
const FRONTEND_ERROR_DEDUPE_MS = 30000;
let frontendErrorLogCount = 0;
const frontendErrorLastSeen = new Map();

export function logUiAction(event, data = {}) {
  const payload = JSON.stringify({event, data});
  try {
    if (navigator.sendBeacon) {
      const blob = new Blob([payload], {type:'application/json'});
      if (navigator.sendBeacon('/api/ui-log', blob)) return;
    }
  } catch (e) {}
  fetch('/api/ui-log', {
    method:'POST',
    headers:{'Content-Type':'application/json'},
    body: payload,
    keepalive:true,
  }).catch(() => {});
}

export function collectUiLogContext(extra = {}) {
  const grid = $('#grid');
  const container = $('#gridContainer');
  return Object.assign({
    mode: state.mode,
    artist_id: state.currentArtist ? state.currentArtist.id : null,
    artist_name: state.currentArtist ? state.currentArtist.name : '',
    folder: state.activeFolder || '',
    search_scope: state.searchScope,
    search_target: state.searchTarget,
    loaded_count: state.allItems.length,
    card_count: grid ? grid.querySelectorAll('.card').length : 0,
    has_more: state.hasMoreItems,
    mobile_columns: state.mobileColumns,
    viewport: `${window.innerWidth}x${window.innerHeight}`,
    scroll_top: container ? Math.round(container.scrollTop) : Math.round(window.scrollY || 0),
    user_agent: navigator.userAgent,
  }, extra);
}

export function collectSelectionLayoutLogContext(extra = {}) {
  const editBar = $('#editBar');
  const container = $('#gridContainer');
  const containerRect = container ? container.getBoundingClientRect() : null;
  const cards = [...$$('#grid .card[data-id]')];
  const firstVisible = containerRect ? cards.find(card => {
    const rect = card.getBoundingClientRect();
    return rect.bottom > containerRect.top && rect.top < containerRect.bottom;
  }) : null;
  return collectUiLogContext(Object.assign({
    edit_bar_height: editBar ? Math.round(editBar.getBoundingClientRect().height) : 0,
    grid_scroll_top: container ? Math.round(container.scrollTop) : Math.round(window.scrollY || 0),
    grid_client_height: container ? Math.round(container.clientHeight) : Math.round(window.innerHeight || 0),
    first_visible_id: firstVisible ? Number(firstVisible.dataset.id) : null,
    selected_item_ids: [...state.selectedIds],
  }, extra));
}

export function frontendErrorText(value) {
  if (!value) return '';
  if (value.message) return String(value.message);
  try {
    return typeof value === 'string' ? value : JSON.stringify(value);
  } catch (e) {
    return String(value);
  }
}

export function frontendErrorStack(value) {
  if (!value || !value.stack) return '';
  return String(value.stack);
}

export function logFrontendError(event, data) {
  if (frontendErrorLogCount >= FRONTEND_ERROR_LOG_LIMIT) return;
  const key = `${event}:${data.message || data.reason || ''}:${data.source || ''}:${data.line || ''}`;
  const now = Date.now();
  const lastSeen = frontendErrorLastSeen.get(key) || 0;
  if (now - lastSeen < FRONTEND_ERROR_DEDUPE_MS) return;
  frontendErrorLastSeen.set(key, now);
  frontendErrorLogCount += 1;
  logUiAction(event, collectUiLogContext(data));
}

export function installFrontendErrorLogging() {
  window.addEventListener('error', event => {
    logFrontendError('frontend_error', {
      message: frontendErrorText(event.error) || String(event.message || ''),
      source: event.filename || '',
      line: event.lineno || 0,
      column: event.colno || 0,
      stack: frontendErrorStack(event.error),
    });
  });
  window.addEventListener('unhandledrejection', event => {
    const reason = event.reason;
    logFrontendError('frontend_rejection', {
      reason: frontendErrorText(reason),
      stack: frontendErrorStack(reason),
    });
  });
}

export function toast(msg, type) {
  logUiAction('toast', {message: String(msg ?? ''), type: String(type || '')});
  const el = document.createElement('div');
  el.className = `toast ${type}`;
  el.textContent = msg;
  // 错误提示立即播报（alert），其余礼貌播报（status），保证辅助技术能收到。
  el.setAttribute('role', type === 'error' ? 'alert' : 'status');
  document.body.appendChild(el);
  // Three-second visible window plus a 150ms fade-out before DOM removal.
  setTimeout(() => {
    el.classList.add('toast-leaving');
    const reducedMotion = typeof window.matchMedia === 'function'
      && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    setTimeout(() => el.remove(), reducedMotion ? 0 : 150);
  }, 3000);
}
