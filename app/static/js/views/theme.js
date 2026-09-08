// Theme management: Studio Folio Light & Studio Folio Dark ("乌木夜色").
// Supports light, dark, and auto (system preference) with zero FOUC and
// dynamic mobile theme-color sync.

import { state } from '../store.js';
import { $ } from '../utils.js';

export const THEME_STORAGE_KEY = 'gallery.theme';
export const THEME_LIGHT_CANVAS = '#F4ECDD';
export const THEME_DARK_CANVAS = '#1A1612';

let mediaQueryList = null;

export function normalizeThemeMode(value) {
  if (value === 'dark' || value === 'auto') return value;
  return 'light';
}

export function getSystemPrefersDark() {
  if (typeof window === 'undefined' || !window.matchMedia) return false;
  return window.matchMedia('(prefers-color-scheme: dark)').matches;
}

export function getResolvedTheme(mode = state.themeMode) {
  const norm = normalizeThemeMode(mode);
  if (norm === 'auto') {
    return getSystemPrefersDark() ? 'dark' : 'light';
  }
  return norm;
}

export function applyTheme(resolvedTheme) {
  const isDark = resolvedTheme === 'dark';
  if (typeof document !== 'undefined' && document.documentElement) {
    if (isDark) {
      if (typeof document.documentElement.setAttribute === 'function') {
        document.documentElement.setAttribute('data-theme', 'dark');
      }
    } else {
      if (typeof document.documentElement.removeAttribute === 'function') {
        document.documentElement.removeAttribute('data-theme');
      }
    }

    const themeColorMeta = document.querySelector('meta[name="theme-color"]');
    if (themeColorMeta) {
      themeColorMeta.setAttribute('content', isDark ? THEME_DARK_CANVAS : THEME_LIGHT_CANVAS);
    }

    const toggleBtn = $('#themeToggleBtn');
    if (toggleBtn) {
      const mode = state.themeMode || 'light';
      const label = mode === 'auto'
        ? (isDark ? '切换主题（当前：跟随系统 - 暗）' : '切换主题（当前：跟随系统 - 亮）')
        : (isDark ? '切换到浅色模式' : '切换到深色模式');
      toggleBtn.setAttribute('title', label);
      toggleBtn.setAttribute('aria-label', label);
      toggleBtn.setAttribute('aria-pressed', isDark ? 'true' : 'false');
    }

    const select = $('#themeModeSelect');
    if (select) {
      select.value = state.themeMode || 'light';
    }
  }
}

export function setThemeMode(mode, persist = false) {
  const normalized = normalizeThemeMode(mode);
  state.themeMode = normalized;
  const resolved = getResolvedTheme(normalized);
  applyTheme(resolved);

  if (persist && typeof localStorage !== 'undefined') {
    try {
      localStorage.setItem(THEME_STORAGE_KEY, normalized);
    } catch (e) {}
  }
  return normalized;
}

export function toggleTheme() {
  const currentResolved = getResolvedTheme(state.themeMode);
  const nextMode = currentResolved === 'dark' ? 'light' : 'dark';
  return setThemeMode(nextMode, true);
}

export function initTheme() {
  let saved = null;
  if (typeof localStorage !== 'undefined') {
    try {
      saved = localStorage.getItem(THEME_STORAGE_KEY);
    } catch (e) {}
  }

  const mode = normalizeThemeMode(saved);
  setThemeMode(mode, false);

  if (typeof window !== 'undefined' && window.matchMedia) {
    if (!mediaQueryList) {
      mediaQueryList = window.matchMedia('(prefers-color-scheme: dark)');
      const listener = () => {
        if (state.themeMode === 'auto') {
          applyTheme(getResolvedTheme('auto'));
        }
      };
      if (mediaQueryList.addEventListener) {
        mediaQueryList.addEventListener('change', listener);
      } else if (mediaQueryList.addListener) {
        mediaQueryList.addListener(listener);
      }
    }
  }
}
