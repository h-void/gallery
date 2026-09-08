// API client and URL builders (the former core.js API object).

const API_TIMEOUT_MS = 15000;

function requestOptionsWithTimeout(options = {}) {
  if (options.signal || typeof AbortSignal === 'undefined' || !AbortSignal.timeout) return options;
  const {timeoutMs, ...rest} = options;
  const timeout = (Number.isFinite(timeoutMs) && timeoutMs > 0) ? timeoutMs : API_TIMEOUT_MS;
  return {...rest, signal: AbortSignal.timeout(timeout)};
}

function fetchWithTimeout(path, options = {}) {
  return fetch(path, requestOptionsWithTimeout(options));
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
  async put(path) {
    const r = await fetchWithTimeout(path, {method:'PUT'});
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
    const r = await fetchWithTimeout(path, {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify(data || {}),
      keepalive:true,
      ...options
    });
    return this.parseResponse(r);
  },
  async del(path) {
    const r = await fetchWithTimeout(path, {method:'DELETE'});
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
