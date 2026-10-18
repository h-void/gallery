// API client and URL builders (the former core.js API object).

const API_TIMEOUT_MS = 15000;

function requestOptionsWithTimeout(options = {}) {
  if (options.signal || typeof AbortSignal === 'undefined' || !AbortSignal.timeout) return options;
  const {timeoutMs, ...rest} = options;
  const timeout = (Number.isFinite(timeoutMs) && timeoutMs > 0) ? timeoutMs : API_TIMEOUT_MS;
  return {...rest, signal: AbortSignal.timeout(timeout)};
}

// fetch 拒绝时抛的是浏览器英文 DOMException（AbortSignal.timeout 的
// "signal timed out"、取消的 "The operation was aborted."、断网的
// "NetworkError when attempting to fetch resource."），toast 会把
// error.message 原样拼接展示。在此统一转成中文终稿；取消保留
// AbortError 身份，isAbortError 的调用方仍按静默取消处理。
function normalizedFetchError(error) {
  if (!error || typeof error !== 'object') return error;
  const name = String(error.name || '');
  const message = String(error.message || error);
  const canceled = name === 'AbortError' || Number(error.code) === 20;
  const timedOut = name === 'TimeoutError'
    || (!canceled && /timed?\s*out|timeout/i.test(message));
  const offline = error instanceof TypeError
    || /networkerror|failed to fetch|load failed|network request failed/i.test(message);
  let normalized = null;
  if (timedOut) {
    normalized = new Error('请求超时');
    normalized.name = 'TimeoutError';
  } else if (canceled) {
    normalized = new Error('请求已中断');
    normalized.name = 'AbortError';
    normalized.code = 20;
  } else if (offline) {
    normalized = new Error('网络连接失败');
    normalized.name = 'NetworkError';
  }
  if (!normalized) return error;
  normalized.cause = error;
  return normalized;
}

function fetchWithTimeout(path, options = {}) {
  return fetch(path, requestOptionsWithTimeout(options)).catch(error => {
    throw normalizedFetchError(error);
  });
}

export const API = {
  async parseResponse(r) {
    let body = {};
    try {
      body = await r.json();
    } catch (e) {
      body = {};
    }
    if (!r.ok) {
      const detail = body.detail || body.message || body.error;
      const message = (detail && typeof detail === 'object')
        ? (detail.error || detail.message || r.statusText || `HTTP ${r.status}`)
        : (detail || r.statusText || `HTTP ${r.status}`);
      const error = new Error(message);
      error.status = r.status;
      error.detail = detail;
      error.body = body;
      throw error;
    }
    return body;
  },
  async get(path, options = {}) {
    const r = await fetchWithTimeout(path, options);
    return this.parseResponse(r);
  },
  async post(path, data, options = {}) {
    if (data !== undefined && data !== null) return this.postJson(path, data, options);
    const r = await fetchWithTimeout(path, {method:'POST', keepalive:true, ...options});
    return this.parseResponse(r);
  },
  async put(path, options = {}) {
    const r = await fetchWithTimeout(path, {method:'PUT', ...options});
    return this.parseResponse(r);
  },
  async putJson(path, data, options = {}) {
    const r = await fetchWithTimeout(path, {
      method:'PUT',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(data || {}),
      ...options
    });
    return this.parseResponse(r);
  },
  async postJson(path, data, options = {}) {
    const body = JSON.stringify(data || {});
    const r = await fetchWithTimeout(path, {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body,
      keepalive: body.length < 32768,
      ...options
    });
    return this.parseResponse(r);
  },
  // Raw-body upload: the File is the request body, so there is no JSON encoding
  // and the server, not a multipart boundary, decides the type from the bytes.
  // No `keepalive` either: it is capped at 64 KiB and throws on a real photo.
  async postFile(path, file, options = {}) {
    const {timeoutMs = 300000, ...rest} = options;
    const r = await fetchWithTimeout(path, {
      method:'POST',
      headers:{'Content-Type': (file && file.type) || 'application/octet-stream'},
      body: file,
      timeoutMs,
      ...rest
    });
    return this.parseResponse(r);
  },
  async del(path, options = {}) {
    const r = await fetchWithTimeout(path, {method:'DELETE', ...options});
    return this.parseResponse(r);
  },
  fileUrl(filePath, version) {
    const params = new URLSearchParams({path: filePath});
    if (version) params.set('v', version);
    return '/api/file?' + params.toString();
  },
  previewUrl(filePath, version, maxEdge) {
    const params = new URLSearchParams({path: filePath});
    if (version) params.set('v', version);
    if (maxEdge) params.set('max', String(maxEdge));
    return '/api/file/preview?' + params.toString();
  },
  streamUrl(filePath) {
    return '/api/file/stream?path=' + encodeURIComponent(filePath);
  },
  videoFrameUrl(filePath, version) {
    const params = new URLSearchParams({path: filePath, t: '0.1'});
    if (version) params.set('v', version);
    return '/api/file/video-frame?' + params.toString();
  },
  videoCompatibleUrl(filePath) {
    return '/api/file/video-compatible?path=' + encodeURIComponent(filePath);
  },
  videoHlsUrl(filePath) {
    return '/api/file/video-hls?path=' + encodeURIComponent(filePath);
  },
  videoTranscodeUrl(filePath) {
    return '/api/file/video-transcode?path=' + encodeURIComponent(filePath);
  },
  videoTranscodeStatusUrl(filePath) {
    return '/api/file/video-transcode-status?path=' + encodeURIComponent(filePath);
  },
  videoTranscodedUrl(filePath) {
    return '/api/file/video-transcoded?path=' + encodeURIComponent(filePath);
  },
  textUrl(filePath) {
    return '/api/file/text?path=' + encodeURIComponent(filePath);
  },
  deleteFileUrl(filePath) {
    return '/api/file/delete?path=' + encodeURIComponent(filePath);
  },
  artistLinksUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/links`; },
  artistLinksReindexUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/links/reindex`; },
  artistProfileLinksUrl(artistId) { return `/api/artists/${encodeURIComponent(artistId)}/profile-links`; },
  artistProfileLinkUrl(artistId, linkId) { return `/api/artists/${encodeURIComponent(artistId)}/profile-links/${encodeURIComponent(linkId)}`; }
};

// Folder-rename plan refresh is expensive (auto-discover + target recompute for
// a whole artist), and tag/date edits fire it from several places. Collapse a
// burst of triggers into one refresh per artist: debounce the request and skip
// firing while an earlier refresh for the same artist is still in flight (the
// deferred run then happens after it settles, so no update is dropped).
const FOLDER_RENAMES_REFRESH_DELAY_MS = 1200;
const folderRenamesRefreshTimers = new Map();
const folderRenamesRefreshInFlight = new Set();

export function scheduleFolderRenamesRefresh(artistId, delayMs = FOLDER_RENAMES_REFRESH_DELAY_MS) {
  const key = String(artistId || '');
  if (!key) return;
  const previous = folderRenamesRefreshTimers.get(key);
  if (previous) clearTimeout(previous);
  const timer = setTimeout(() => {
    folderRenamesRefreshTimers.delete(key);
    if (folderRenamesRefreshInFlight.has(key)) {
      scheduleFolderRenamesRefresh(artistId, delayMs);
      return;
    }
    folderRenamesRefreshInFlight.add(key);
    API.post(`/api/folder-renames/refresh?artist_id=${encodeURIComponent(key)}`, undefined, {timeoutMs: 120000})
      .catch(() => {})
      .finally(() => folderRenamesRefreshInFlight.delete(key));
  }, delayMs);
  folderRenamesRefreshTimers.set(key, timer);
}
