// Frontend internationalization (i18n): Chinese (zh-CN) & English (en).
// Manages language state, dictionary translation, DOM auto-translation,
// and localStorage persistence with zero dependencies.
//
// Translation contract:
// - App renderers always write Chinese source text; TRANSLATIONS maps it to en.
// - On switching to en we translate the *current* text and remember what we set.
// - On switching back to zh we restore only when the text is still exactly the
//   value we set: dynamic updates written by app code are never clobbered.
// - A MutationObserver keeps dynamically rendered content in sync while en is
//   active (renderers write zh; the observer translates what appears).
// - The shipped dictionary lives in i18n_dict.js and loads lazily on the first
//   English request; Chinese sessions never fetch it.

import { state } from './store.js';
import { $ } from './utils.js';

export const LANG_STORAGE_KEY = 'gallery.lang';

// Runtime translation table: shipped entries are merged in from the lazy
// i18n_dict.js module on first English use (see ensureTranslations), so the
// browse bundle does not carry the dictionary for Chinese sessions. Extras
// registered through registerTranslations land here too and always win over
// shipped entries.
export const TRANSLATIONS = {};

let translationsLoad = null;

// Loads the shipped dictionary once and merges it into TRANSLATIONS. Entries
// already present (registered extras) are never overwritten. Also invalidates
// the derived lookup caches, exactly like registerTranslations does.
export function ensureTranslations() {
  if (!translationsLoad) {
    translationsLoad = import('./i18n_dict.js').then(mod => {
      for (const key of Object.keys(mod.TRANSLATIONS)) {
        if (!Object.prototype.hasOwnProperty.call(TRANSLATIONS, key)) {
          TRANSLATIONS[key] = mod.TRANSLATIONS[key];
        }
      }
      reverseMap = null;
      templateMatchersEn = null;
      templateMatchersZh = null;
      return TRANSLATIONS;
    }).catch(error => {
      // 一次网络抖动不应让整个会话的英文界面永久停在中文：失败的动态导入
      // 不能留在缓存里当成功/失败终态，清掉后下一次切换语言会重新拉取。
      translationsLoad = null;
      throw error;
    });
  }
  return translationsLoad;
}


// Parameterized messages use whole-sentence placeholder keys:
//   t('扫描失败：{detail}', {detail}) -> 'Scan failed: {detail}' filled in.
// Concatenation (t('扫描失败') + '：' + detail) is forbidden: word order and
// punctuation differ per language. tests/test_i18n_coverage.py enforces this.
const PLACEHOLDER_RE = /\{(\w+)\}/g;

// Kill switch: dropping 'en' here makes every string resolve to its Chinese
// source, so a bad English rollout degrades to the zh UI without code changes.
export const ENABLED_LANGS = ['zh', 'en'];

const USER_DATA_PARAMS = new Set([
  'artist', 'cleanName', 'code', 'fileName', 'folder', 'name',
  'originalPath', 'plan', 'preview', 'preview2', 'query', 'source',
  'target', 'template', 'userID',
]);

const NUMERIC_PLACEHOLDERS = new Set([
  'DOWNLOAD_CALENDAR_WEEKS', 'Number', 'Number2', 'addedAsNew', 'allJobs',
  'amount', 'amount2', 'amount3', 'applicableCount', 'applied', 'artistCount',
  'backupRetainedCount', 'blocked', 'blockedCount', 'calendar', 'claimable',
  'completedJobs', 'confirmed', 'confirmedPlans', 'conflicts', 'count',
  'count2', 'covered', 'covered_subscriptions', 'created', 'current_version',
  'database', 'days', 'deferred', 'deleted', 'delivered', 'direct',
  'directPosts', 'discovered', 'discovered_posts', 'discovery_failures',
  'downloadDayMissing', 'downloaded_files', 'drives', 'entries', 'errors',
  'executed', 'external', 'externalPosts', 'failed', 'failedHistory',
  'fetched', 'final_missing_days', 'gaps', 'generation', 'group',
  'hiddenCount', 'hit', 'id', 'idle', 'importRemaining', 'index',
  'integrity_failed', 'intervalHours', 'limit', 'line', 'linked', 'links',
  'matched', 'mismatched', 'missing', 'moved', 'needsAttention', 'observed',
  'occupied', 'operation', 'outstanding_posts', 'page_posts', 'pages_saved',
  'pending', 'pendingJobs', 'plans', 'posts', 'processed', 'purged',
  'queued', 'read', 'ready', 'recycled', 'recovered', 'referenceTotal',
  'remaining', 'resolveNetdiskBatchDeliveredCount', 'resolved', 'restored',
  'result', 'result2', 'reverted', 'reviewed_version', 'skipped',
  'skippedCount', 'social', 'stale', 'sub', 'subscriptions', 'success',
  'successful', 'successfulHistory', 'synced_subs', 'system', 'tags',
  'total', 'totals', 'unavailable', 'unconfirmedReady', 'unmoved',
  'unverified', 'updated', 'verified', 'visibleSubs', 'waiting', 'year',
  'PAWCHIVE_OBSERVE_MAX_POSTS', 'PAWCHIVE_ROUND_MAX_POSTS_PER_SUBSCRIPTION',
]);

export function registerTranslations(map) {
  if (!map || typeof map !== 'object') return;
  Object.assign(TRANSLATIONS, map);
  reverseMap = null;
  templateMatchersEn = null;
  templateMatchersZh = null;
}

export function normalizeLanguage(value) {
  if (value === 'en' && ENABLED_LANGS.indexOf('en') !== -1) return 'en';
  return 'zh';
}

// Module-level cache so render paths can call t() per string without the
// Proxy/localStorage probe each time. Validated against state.lang on every
// read: tests (and the storage sync) reset that field behind our back.
let currentLangCache = null;

export function getLanguage() {
  if (currentLangCache && state && state.lang === currentLangCache) return currentLangCache;
  let lang = 'zh';
  if (state && state.lang) {
    lang = normalizeLanguage(state.lang);
  } else if (typeof localStorage !== 'undefined') {
    try {
      const saved = localStorage.getItem(LANG_STORAGE_KEY);
      if (saved) lang = normalizeLanguage(saved);
    } catch (e) {}
  }
  if (state && state.lang) currentLangCache = lang;
  return lang;
}

function resolveEntry(key, context) {
  if (context) {
    const scoped = `${key}#${context}`;
    if (Object.prototype.hasOwnProperty.call(TRANSLATIONS, scoped)) return TRANSLATIONS[scoped];
  }
  return Object.prototype.hasOwnProperty.call(TRANSLATIONS, key) ? TRANSLATIONS[key] : null;
}

function interpolate(template, params, isEn = false) {
  if (!params || typeof params !== 'object') return template;
  return template.replace(PLACEHOLDER_RE, (match, name) => {
    if (!Object.prototype.hasOwnProperty.call(params, name)) return match;
    const raw = String(params[name]);
    if (isEn && !USER_DATA_PARAMS.has(name) && /[\u4e00-\u9fff]/.test(raw)) {
      const tr = lookupEn(raw, 1);
      return tr !== null ? tr : raw;
    }
    return raw;
  });
}

// t(key, params?, context?) — params is a {name: value} object for placeholder
// filling; a plain string second argument is the legacy fallback form
// t(key, 'fallback'). Unmatched input (including already-translated text) is
// returned unchanged: double translation is a no-op, never undefined.
export function t(text, params, context) {
  if (!text) return '';
  const legacyFallback = typeof params === 'string' ? params : undefined;
  const dictParams = typeof params === 'object' && params !== null ? params : undefined;
  const scoped = typeof context === 'string' ? context : undefined;
  if (ENABLED_LANGS.indexOf('en') === -1 || getLanguage() === 'zh') {
    return interpolate(text, dictParams, false);
  }
  const en = resolveEntry(text, scoped) || (scoped ? null : lookupEn(text, 0));
  if (en !== null) return interpolate(en, dictParams, true);
  return legacyFallback !== undefined ? legacyFallback : interpolate(text, dictParams, true);
}

function escapeRegExp(str) {
  return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

let templateMatchersEn = null;
let templateMatchersZh = null;

function ensureTemplateMatchers() {
  if (templateMatchersEn && templateMatchersZh) return;
  templateMatchersEn = [];
  templateMatchersZh = [];

  const enCounts = new Map();
  for (const key of Object.keys(TRANSLATIONS)) {
    if (key.indexOf('#') !== -1) continue;
    const en = TRANSLATIONS[key];
    if (typeof en === 'string') {
      enCounts.set(en, (enCounts.get(en) || 0) + 1);
    }
  }

  for (const zhKey of Object.keys(TRANSLATIONS)) {
    if (zhKey.indexOf('#') !== -1) continue;
    const enVal = TRANSLATIONS[zhKey];
    if (typeof enVal !== 'string' || !/\{\w+\}/.test(zhKey)) continue;

    // 1. Build zh -> en matcher
    const zhParts = zhKey.split(/\{(\w+)\}/g);
    const zhLiterals = [];
    const zhNames = [];
    for (let i = 0; i < zhParts.length; i++) {
      if (i % 2 === 0) zhLiterals.push(zhParts[i]);
      else zhNames.push(zhParts[i]);
    }
    const zhStatic = zhLiterals.join('');
    const cjkCount = (zhStatic.match(/[\u4e00-\u9fff]/g) || []).length;
    if (cjkCount >= 1 && (zhStatic.trim().length > 1 || zhNames.every(n => NUMERIC_PLACEHOLDERS.has(n)))) {
      let longestLit = '';
      for (const lit of zhLiterals) {
        if (lit.length > longestLit.length) longestLit = lit;
      }
      const multiline = zhKey.indexOf('\n') !== -1;
      let pattern = '^';
      for (let i = 0; i < zhLiterals.length; i++) {
        pattern += escapeRegExp(zhLiterals[i]);
        if (i < zhNames.length) {
          const name = zhNames[i];
          if (cjkCount <= 1 && NUMERIC_PLACEHOLDERS.has(name)) {
            pattern += '(\\d+(?:\\.\\d+)?)';
          } else {
            pattern += multiline ? '([\\s\\S]+?)' : '(.+?)';
          }
        }
      }
      pattern += '$';
      templateMatchersEn.push({
        regex: new RegExp(pattern),
        names: zhNames,
        enTemplate: enVal,
        longestLiteral: longestLit,
        cjkCount,
        staticLen: zhStatic.length,
      });
    }

    // 2. Build en -> zh reverse matcher for unambiguous templates
    if (enCounts.get(enVal) === 1 && /\{\w+\}/.test(enVal)) {
      const enParts = enVal.split(/\{(\w+)\}/g);
      const enLiterals = [];
      const enNames = [];
      for (let i = 0; i < enParts.length; i++) {
        if (i % 2 === 0) enLiterals.push(enParts[i]);
        else enNames.push(enParts[i]);
      }
      const enNameSet = new Set(enNames);
      if (zhNames.every(n => enNameSet.has(n))) {
        const enStatic = enLiterals.join('');
        const letterCount = (enStatic.match(/[A-Za-z]/g) || []).length;
        if (letterCount >= 3) {
          let longestLit = '';
          for (const lit of enLiterals) {
            if (lit.length > longestLit.length) longestLit = lit;
          }
          const multiline = enVal.indexOf('\n') !== -1;
          let pattern = '^';
          for (let i = 0; i < enLiterals.length; i++) {
            pattern += escapeRegExp(enLiterals[i]);
            if (i < enNames.length) {
              const name = enNames[i];
              if (letterCount <= 6 && NUMERIC_PLACEHOLDERS.has(name)) {
                pattern += '(\\d+(?:\\.\\d+)?)';
              } else {
                pattern += multiline ? '([\\s\\S]+?)' : '(.+?)';
              }
            }
          }
          pattern += '$';
          templateMatchersZh.push({
            regex: new RegExp(pattern),
            names: enNames,
            zhTemplate: zhKey,
            longestLiteral: longestLit,
            letterCount,
            staticLen: enStatic.length,
          });
        }
      }
    }
  }

  templateMatchersEn.sort((a, b) => (b.cjkCount - a.cjkCount) || (b.staticLen - a.staticLen));
  templateMatchersZh.sort((a, b) => (b.letterCount - a.letterCount) || (b.staticLen - a.staticLen));
}

function matchTemplateEn(zhText, depth) {
  ensureTemplateMatchers();
  for (let i = 0; i < templateMatchersEn.length; i++) {
    const m = templateMatchersEn[i];
    if (m.longestLiteral && zhText.indexOf(m.longestLiteral) === -1) continue;
    const match = m.regex.exec(zhText);
    if (!match) continue;
    const params = {};
    for (let j = 0; j < m.names.length; j++) {
      const name = m.names[j];
      const val = match[j + 1];
      if (!USER_DATA_PARAMS.has(name) && /[\u4e00-\u9fff]/.test(val)) {
        params[name] = lookupEn(val, depth + 1) ?? val;
      } else {
        params[name] = val;
      }
    }
    return interpolate(m.enTemplate, params, false);
  }
  return null;
}

function matchTemplateZh(enText, depth) {
  ensureTemplateMatchers();
  for (let i = 0; i < templateMatchersZh.length; i++) {
    const m = templateMatchersZh[i];
    if (m.longestLiteral && enText.indexOf(m.longestLiteral) === -1) continue;
    const match = m.regex.exec(enText);
    if (!match) continue;
    const params = {};
    for (let j = 0; j < m.names.length; j++) {
      const name = m.names[j];
      const val = match[j + 1];
      if (!USER_DATA_PARAMS.has(name) && /[A-Za-z]/.test(val)) {
        params[name] = lookupZh(val, depth + 1) ?? val;
      } else {
        params[name] = val;
      }
    }
    return interpolate(m.zhTemplate, params, false);
  }
  return null;
}

function lookupEn(zhText, depth = 0) {
  if (!zhText || typeof zhText !== 'string') return null;
  const direct = resolveEntry(zhText, undefined);
  if (direct !== null) return direct;
  if (depth > 3 || !/[\u4e00-\u9fa5]/.test(zhText)) return null;
  const matchFailed = zhText.match(/^执行失败\s+(\d+)$/);
  if (matchFailed) return `Execution failed: ${matchFailed[1]}`;

  // Colon-prefix keys: e.g. '备份失败：' + detail
  const colonIdx = zhText.indexOf('：');
  if (colonIdx > 0 && colonIdx < zhText.length - 1 && zhText.indexOf('\n') === -1) {
    const prefix = zhText.slice(0, colonIdx + 1);
    const rest = zhText.slice(colonIdx + 1).trim();
    const enPrefix = resolveEntry(prefix, undefined);
    if (enPrefix !== null && !/\{\w+\}/.test(enPrefix)) {
      const enRest = lookupEn(rest, depth + 1) ?? rest;
      return `${enPrefix.trimEnd()} ${enRest}`;
    }
  }

  const tplMatch = matchTemplateEn(zhText, depth);
  if (tplMatch !== null) return tplMatch;

  for (const [sep, enSep] of [[' \u00b7 ', ' \u00b7 '], ['，', ', '], ['、', ', ']]) {
    if (zhText.includes(sep)) {
      const parts = zhText.split(sep);
      const translatedParts = [];
      let anyTranslated = false;
      let allValid = true;
      for (const part of parts) {
        const trimmed = part.trim();
        if (!trimmed) {
          translatedParts.push('');
          continue;
        }
        if (!/[\u4e00-\u9fa5]/.test(trimmed)) {
          translatedParts.push(trimmed);
          continue;
        }
        const tr = lookupEn(trimmed, depth + 1);
        if (tr !== null) {
          translatedParts.push(tr);
          anyTranslated = true;
        } else {
          allValid = false;
          break;
        }
      }
      if (allValid && anyTranslated) {
        return translatedParts.join(enSep);
      }
    }
  }
  return null;
}

// Reverse lookup for text that was rendered through t() while en was active:
// by the time the user switches back to zh the DOM only holds the English
// value, so the walker inverts it. Only unambiguous base entries participate —
// scoped `key#context` entries and values shared by several source strings
// (e.g. 角色/角色库) would restore the wrong Chinese text, so those are skipped
// and the element keeps its current value until a re-render.
let reverseMap = null;
function ensureReverseMap() {
  if (reverseMap) return;
  reverseMap = new Map();
  const ambiguous = new Set();
  for (const key of Object.keys(TRANSLATIONS)) {
    if (key.indexOf('#') !== -1) continue;
    const en = TRANSLATIONS[key];
    if (typeof en !== 'string' || /\{\w+\}/.test(en)) continue;
    if (ambiguous.has(en)) continue;
    if (reverseMap.has(en)) {
      reverseMap.delete(en);
      ambiguous.add(en);
    } else {
      reverseMap.set(en, key);
    }
  }
}

function lookupZh(enText, depth = 0) {
  if (!enText || typeof enText !== 'string') return null;
  ensureReverseMap();
  if (reverseMap.has(enText)) return reverseMap.get(enText);
  if (depth > 3 || !/[a-zA-Z]/.test(enText)) return null;
  const matchFailedEn = enText.match(/^Execution failed:\s*(\d+)$/);
  if (matchFailedEn) return `执行失败 ${matchFailedEn[1]}`;

  const colonIdx = enText.indexOf(': ');
  if (colonIdx > 0 && enText.indexOf('\n') === -1) {
    const enPrefix = enText.slice(0, colonIdx + 1);
    const rest = enText.slice(colonIdx + 2).trim();
    const zhPrefix = reverseMap.get(enPrefix);
    if (zhPrefix && zhPrefix.endsWith('：')) {
      const zhRest = lookupZh(rest, depth + 1) ?? rest;
      return `${zhPrefix}${zhRest}`;
    }
  }

  const tplMatch = matchTemplateZh(enText, depth);
  if (tplMatch !== null) return tplMatch;

  if (enText.includes(' \u00b7 ')) {
    const sep = ' \u00b7 ';
    const parts = enText.split(sep);
    const zhParts = [];
    let anyRestored = false;
    for (const part of parts) {
      const trimmed = part.trim();
      if (!trimmed) {
        zhParts.push('');
        continue;
      }
      const zh = lookupZh(trimmed, depth + 1);
      if (zh !== null) {
        zhParts.push(zh);
        anyRestored = true;
      } else if (!/[a-zA-Z]/.test(trimmed)) {
        zhParts.push(trimmed);
      } else {
        return null;
      }
    }
    if (anyRestored) return zhParts.join(sep);
  }
  return null;
}

function hasElementChildren(el) {
  return !!(el.children && el.children.length > 0);
}

// Swap text under the ownership contract: translate on the way to en and
// remember both sides; restore on the way to zh only while the element still
// shows exactly the value we set. Any text rewritten by app code in between is
// left untouched (that is the stale-restore bug this replaces).
function swapText(el, sourceText, enText, isEn) {
  if (isEn) {
    if (el._i18nAppliedText !== undefined && el.textContent === el._i18nAppliedText) return;
    el._i18nOrigText = sourceText !== undefined ? sourceText : el.textContent;
    el._i18nAppliedText = enText;
    el.textContent = enText;
    return;
  }
  if (el._i18nOrigText !== undefined && el._i18nAppliedText !== undefined && el.textContent === el._i18nAppliedText) {
    el.textContent = el._i18nOrigText;
  }
  el._i18nAppliedText = undefined;
}

function swapAttr(el, attr, sourceValue, enValue, isEn) {
  const appliedKey = `_i18nApplied_${attr}`;
  const origKey = `_i18nOrig_${attr}`;
  const current = el.getAttribute(attr) || '';
  if (isEn) {
    if (el[appliedKey] !== undefined && current === el[appliedKey]) return;
    el[origKey] = sourceValue !== undefined ? sourceValue : current;
    el[appliedKey] = enValue;
    el.setAttribute(attr, enValue);
    return;
  }
  if (el[origKey] !== undefined && el[appliedKey] !== undefined && current === el[appliedKey]) {
    el.setAttribute(attr, el[origKey]);
  }
  el[appliedKey] = undefined;
}

// data-i18n* key-based translation: the key is the dictionary lookup source.
// Unknown keys keep the element's own text/attribute (never "undefined"), and
// elements with child elements keep their markup untouched.
function applyKeyed(el, key, isEn, attr) {
  const en = lookupEn(key);
  if (attr) {
    const current = el.getAttribute(attr) || '';
    const source = current || key;
    const target = en !== null ? en : source;
    swapAttr(el, attr, source, target, isEn);
    return;
  }
  if (hasElementChildren(el)) return;
  const current = el.textContent || '';
  const source = current || key;
  const target = en !== null ? en : source;
  swapText(el, source, target, isEn);
}

function applyAutoTextNode(node, isEn) {
  const current = node.nodeValue !== undefined ? (node.nodeValue || '') : (node.textContent || '');
  const trimmed = current.trim();
  if (!trimmed) return;
  const startIdx = current.indexOf(trimmed);
  const lead = current.slice(0, startIdx);
  const trail = current.slice(startIdx + trimmed.length);
  if (isEn) {
    if (node._i18nAppliedText !== undefined && current === node._i18nAppliedText) return;
    const en = lookupEn(trimmed);
    if (en === null) return;
    const needLeadSpace = !lead && node.previousSibling && node.previousSibling.nodeType === 1 && /^[A-Za-z0-9(]/.test(en);
    const needTrailSpace = !trail && node.nextSibling && node.nextSibling.nodeType === 1 && /[A-Za-z0-9):]$/.test(en);
    const applied = `${lead || (needLeadSpace ? ' ' : '')}${en}${trail || (needTrailSpace ? ' ' : '')}`;
    node._i18nOrigText = current;
    node._i18nAppliedText = applied;
    if (node.nodeValue !== undefined) node.nodeValue = applied;
    else node.textContent = applied;
    return;
  }
  if (node._i18nOrigText !== undefined && node._i18nAppliedText !== undefined && current === node._i18nAppliedText) {
    if (node.nodeValue !== undefined) node.nodeValue = node._i18nOrigText;
    else node.textContent = node._i18nOrigText;
    node._i18nAppliedText = undefined;
    return;
  }
  node._i18nAppliedText = undefined;
  const zh = lookupZh(trimmed);
  if (zh !== null) {
    const restored = `${lead}${zh}${trail}`;
    if (current !== restored) {
      if (node.nodeValue !== undefined) node.nodeValue = restored;
      else node.textContent = restored;
    }
  }
}

// Auto translation: look the *current* value up as a zh source string.
function applyAutoText(el, isEn) {
  const tagName = (el.tagName || '').toUpperCase();
  if (tagName === 'SCRIPT' || tagName === 'STYLE') return;
  if (hasElementChildren(el)) {
    if (el.childNodes && el.childNodes.length > 0) {
      for (let i = 0; i < el.childNodes.length; i++) {
        const child = el.childNodes[i];
        if (child && child.nodeType === 3) {
          applyAutoTextNode(child, isEn);
        }
      }
    }
    return;
  }
  const current = el.textContent || '';
  const trimmed = current.trim();
  if (!trimmed) return;
  if (isEn) {
    const en = lookupEn(trimmed);
    if (en !== null) swapText(el, current, en, true);
    return;
  }
  swapText(el, undefined, undefined, false);
  // Text rendered through t() while en was active carries no bookkeeping;
  // invert unambiguous English values back to their Chinese source.
  const now = (el.textContent || '').trim();
  const zh = lookupZh(now);
  if (zh !== null && el.textContent !== zh) {
    el.textContent = zh;
    el._i18nAppliedText = undefined;
  }
}

function applyAutoAttr(el, attr, isEn) {
  const current = el.getAttribute(attr) || '';
  if (isEn) {
    const en = lookupEn(current.trim());
    if (en !== null) swapAttr(el, attr, current, en, true);
    return;
  }
  swapAttr(el, attr, undefined, undefined, false);
  const now = (el.getAttribute(attr) || '').trim();
  const zh = lookupZh(now);
  if (zh !== null && (el.getAttribute(attr) || '') !== zh) {
    el.setAttribute(attr, zh);
    el[`_i18nApplied_${attr}`] = undefined;
  }
}

// Elements whose text and attributes are user data — a folder, artist, tag,
// character or file name — carry `data-i18n-skip`. The auto pass looks any leaf
// text up as a UI source string, so without this marker a folder named 全部
// would render as "All" and an artist named "All" as 全部: the app would show a
// name that is not the name on disk. Explicit `data-i18n*` keys still apply,
// because those are the app's own copy, and children are still walked.
function isUserDataElement(el) {
  return typeof el.hasAttribute === 'function' && el.hasAttribute('data-i18n-skip');
}

function walkElement(el, isEn) {
  if (!el) return;
  if (typeof el.getAttribute === 'function') {
    const userData = isUserDataElement(el);
    const key = el.getAttribute('data-i18n');
    if (key) {
      applyKeyed(el, key, isEn, null);
    } else if (!userData) {
      applyAutoText(el, isEn);
    }

    const titleKey = el.getAttribute('data-i18n-title');
    if (titleKey) applyKeyed(el, titleKey, isEn, 'title');
    else if (!userData) applyAutoAttr(el, 'title', isEn);

    const phKey = el.getAttribute('data-i18n-placeholder');
    if (phKey) applyKeyed(el, phKey, isEn, 'placeholder');
    else if (!userData) applyAutoAttr(el, 'placeholder', isEn);

    const ariaKey = el.getAttribute('data-i18n-aria-label');
    if (ariaKey) applyKeyed(el, ariaKey, isEn, 'aria-label');
    else if (!userData) applyAutoAttr(el, 'aria-label', isEn);
  }

  if (el.children && el.children.length > 0) {
    for (let i = 0; i < el.children.length; i++) {
      walkElement(el.children[i], isEn);
    }
  }
}

export function translateDom(root) {
  if (typeof document === 'undefined') return;
  const lang = getLanguage();
  const isEn = lang === 'en';

  if (typeof document.title === 'string') {
    if (!document._i18nOrigTitle && document.title.includes('画廊')) {
      document._i18nOrigTitle = document.title;
    }
    if (document._i18nOrigTitle) {
      document.title = isEn ? document._i18nOrigTitle.replace('画廊', 'Gallery') : document._i18nOrigTitle;
    }
  }

  const target = (root && root !== document) ? root : (document.body || document.documentElement);
  if (target) {
    walkElement(target, isEn);
  }
}

// Keeps dynamically rendered content in sync while en is active. Renderers
// write Chinese source strings; anything that appears or changes gets the same
// translate-if-known treatment as the full walk. Our own translation writes
// re-trigger the observer, but then the current value is the English text,
// which is not a dictionary key, so the callback converges immediately.
let domObserver = null;
let observedRoot = null;

function translateMutationTarget(node, isEn) {
  if (!node) return;
  if (node.nodeType === 3) {
    const parent = node.parentElement || (node.parentNode && node.parentNode.nodeType === 1 ? node.parentNode : null);
    // A renderer that rewrote a user-data element's text must not have it
    // translated on the way in either.
    if (parent && !isUserDataElement(parent)) applyAutoText(parent, isEn);
    return;
  }
  walkElement(node, isEn);
}

function ensureDomObserver() {
  if (typeof MutationObserver === 'undefined' || typeof document === 'undefined') return;
  const root = document.body || document.documentElement;
  if (!root) return;
  if (domObserver && observedRoot === root) return;
  if (domObserver && typeof domObserver.disconnect === 'function') domObserver.disconnect();
  domObserver = new MutationObserver(records => {
    if (getLanguage() !== 'en') return;
    for (const record of records) {
      if (record.type === 'characterData') {
        translateMutationTarget(record.target, true);
      } else if (record.type === 'childList') {
        const nodes = record.addedNodes || [];
        for (let i = 0; i < nodes.length; i++) translateMutationTarget(nodes[i], true);
      } else if (record.type === 'attributes') {
        walkElement(record.target, true);
      }
    }
  });
  domObserver.observe(root, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['title', 'placeholder', 'aria-label'],
  });
  observedRoot = root;
}

export function setLanguage(lang, persist = false) {
  const normalized = normalizeLanguage(lang);
  // The shipped dictionary loads lazily: start the fetch on the first English
  // request and retranslate once it lands. When it is already loaded (or the
  // session stays Chinese) nothing is pending and the sync pass below is final.
  const dictionaryPending = normalized === 'en' && !translationsLoad;
  if (normalized === 'en') {
    // 字典分片加载失败只意味着这次保持中文原文；调用方不处理这个 promise，
    // 在这里收口避免未处理的 rejection。
    ensureTranslations().catch(() => {});
  }
  if (state && state.lang !== undefined) {
    state.lang = normalized;
  }

  if (typeof document !== 'undefined' && document.documentElement) {
    if (typeof document.documentElement.setAttribute === 'function') {
      document.documentElement.setAttribute('lang', normalized === 'en' ? 'en' : 'zh-CN');
      // CSS hook kept in sync with the head bootstrap script.
      document.documentElement.setAttribute('data-lang', normalized);
    }
    document.documentElement.lang = normalized === 'en' ? 'en' : 'zh-CN';
    const langBtn = $('#langToggleBtn');
    if (langBtn) {
      langBtn.textContent = normalized === 'en' ? '中' : 'EN';
      // Tooltip names the action in the language currently displayed.
      const title = normalized === 'en' ? 'Switch to Chinese' : '切换为中文';
      langBtn.setAttribute('title', title);
      langBtn.setAttribute('aria-label', title);
    }
    translateDom(document);
    ensureDomObserver();
  }

  if (persist && typeof localStorage !== 'undefined') {
    try {
      localStorage.setItem(LANG_STORAGE_KEY, normalized);
    } catch (e) {}
  }

  if (typeof window !== 'undefined' && window.dispatchEvent) {
    window.dispatchEvent(new CustomEvent('gallery:lang', { detail: { lang: normalized } }));
  }

  // A first English switch may have translated against a still-empty runtime
  // table; when the lazy dictionary lands, run one more document pass so the
  // visible copy converges without another user action.
  if (dictionaryPending) {
    ensureTranslations().then(() => {
      if (typeof document !== 'undefined' && document.documentElement && getLanguage() === 'en') {
        translateDom(document);
      }
    }).catch(() => {});
  }

  return normalized;
}

export function toggleLanguage() {
  const current = getLanguage();
  const next = current === 'en' ? 'zh' : 'en';
  return setLanguage(next, true);
}

// Cross-tab sync: a language switch in another tab writes localStorage and
// fires `storage` here. Mirror it so both tabs converge on one language and
// the module cache can never serve a stale value after the mirror.
function installLanguageStorageSync() {
  if (typeof window === 'undefined' || typeof window.addEventListener !== 'function') return;
  if (window._galleryLangStorageSyncBound) return;
  window._galleryLangStorageSyncBound = true;
  window.addEventListener('storage', (event) => {
    if (!event || (event.key !== LANG_STORAGE_KEY && event.key !== null)) return;
    currentLangCache = null;
    let saved = null;
    if (typeof localStorage !== 'undefined') {
      try {
        saved = localStorage.getItem(LANG_STORAGE_KEY);
      } catch (e) {}
    }
    setLanguage(normalizeLanguage(saved), false);
  });
}

export function initI18n() {
  let saved = null;
  if (typeof localStorage !== 'undefined') {
    try {
      saved = localStorage.getItem(LANG_STORAGE_KEY);
    } catch (e) {}
  }
  const lang = normalizeLanguage(saved);
  setLanguage(lang, false);
  installLanguageStorageSync();

  const langBtn = $('#langToggleBtn');
  if (langBtn && !langBtn._langBound) {
    langBtn._langBound = true;
    langBtn.addEventListener('click', () => {
      toggleLanguage();
    });
  }
}
