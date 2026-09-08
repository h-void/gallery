// Per-domain stores plus the request-sequence and busy-guard tools. Plan §2
// C3 (G1) replaces the original single mutable state blob with three named
// stores (browse / selection / maintenance). Every field is declared up front
// and set/patch reject undeclared keys. The legacy `state` export is a
// read-only proxy that resolves `state.field` into the matching domain so
// the existing call sites keep working without rewrites.
//
// Store API:
//   store.get(key) / store.set(key, value) / store.patch({...})
//   store.subscribe(listener) -> unsubscribe()
//   store.keys() / store.fields() for inspection and tests.
//
// Request-sequence and busy-guard tools (nextRequestSeq, isCurrentRequestSeq,
// setActionBusy, ...) still operate through the selection store, so they keep
// the same observable behavior as before.

const FIELDS = Symbol('fields');
const LISTENERS = Symbol('listeners');

function createStore(initial) {
  const fields = {...initial};
  const listeners = new Set();

  function emit(key, value) {
    for (const listener of listeners) {
      try {
        listener(key, value, fields);
      } catch (err) {
        // Listener failures must not break the store contract; surface through
        // the global error logger but keep other listeners running.
        if (typeof globalThis !== 'undefined' && globalThis.console) {
          globalThis.console.error('store listener failed:', err);
        }
      }
    }
  }

  function assertField(key) {
    if (!Object.prototype.hasOwnProperty.call(fields, key)) {
      throw new Error(`store: unknown field "${String(key)}"`);
    }
  }

  return {
    [FIELDS]: fields,
    [LISTENERS]: listeners,
    has(key) {
      return Object.prototype.hasOwnProperty.call(fields, key);
    },
    keys() {
      return Object.keys(fields);
    },
    fields() {
      return {...fields};
    },
    get(key) {
      assertField(key);
      return fields[key];
    },
    set(key, value) {
      assertField(key);
      const prev = fields[key];
      fields[key] = value;
      if (!Object.is(prev, value)) emit(key, value);
      return value;
    },
    patch(updates) {
      if (!updates || typeof updates !== 'object') {
        throw new Error('store.patch requires a plain object of updates');
      }
      const changed = [];
      for (const key of Object.keys(updates)) {
        assertField(key);
        const prev = fields[key];
        const next = updates[key];
        if (!Object.is(prev, next)) {
          fields[key] = next;
          changed.push(key);
        }
      }
      for (const key of changed) emit(key, fields[key]);
      return changed;
    },
    subscribe(listener) {
      if (typeof listener !== 'function') {
        throw new Error('store.subscribe requires a function');
      }
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

// ---- Domain declarations ----
//
// Every field is declared up front. `selection` owns the cross-cutting
// request-sequence counters and busy-guard set so existing tools continue to
// find them where callers expect.

const browse = createStore({
  artists: [],
  artistsLoaded: false,
  currentArtist: null,
  stats: null,
  mode: 'browse',
  view: 'grid',
  cardRatio: '4x3',
  themeMode: 'light',
  activeRole: null,
  activeFolder: null,
  search: '',
  searchScope: 'auto',
  searchTarget: 'all',
  itemSort: 'date_desc',
  itemDateFrom: '',
  itemDateTo: '',
  itemSortExplicit: false,
  searchOptionsOpen: false,
  duplicatesOnly: false,
  duplicatesRefreshInFlight: false,
  duplicatesRefreshFingerprint: null,
  duplicatesRefreshFailures: 0,
  allItems: [],
  itemsOffset: 0,
  itemsCursor: null,
  hasMoreItems: false,
  loadingItems: false,
  loadingMoreItems: false,
  itemLoadSeq: 0,
  artistLoadSeq: 0,
  urlRestoreSeq: 0,
  scanRefreshSeq: 0,
  browseUrlRestored: false,
  tags: [],
  tagSearchResults: [],
  folders: null,
  duplicateFolders: [],
  filterDrawerOpen: false,
  mobileHeaderToolsOpen: false,
  mobileColumns: 2,
  sidebarWidth: 260,
  sidebarTagRatio: 46,
  sidebarCollapsed: {filters: false, tags: false, folders: false, duplicates: true},
});

const selection = createStore({
  selectedIds: new Set(),
  editMode: false,
  selectedEditTagIds: new Set(),
  selectedEditTagNames: new Set(),
  editTagQuery: '',
  editContextArtistId: null,
  editContextKey: '',
  editTagContextLoading: false,
  editGlobalTagResults: [],
  editGlobalTagSearchLoading: false,
  characterTagSuggestions: [],
  characterSuggestionSelectedNames: new Set(),
  characterSuggestionCache: new Map(),
  characterSuggestionSeq: 0,
  characterSuggestionPageKey: '',
  characterSuggestionScheduleSeq: 0,
  characterSuggestionScheduleTimer: null,
  characterSuggestionScheduleFrame: null,
  characterSuggestionLoading: false,
  characterSuggestionStatus: 'idle',
  characterSuggestionMessage: '',
  characterSuggestionSampleTotal: 0,
  characterSuggestionSampleLimit: 0,
  selectionMarquee: null,
  selectionModifierDown: false,
  suppressNextGridClick: false,
  selectionRestoreSeq: 0,
  maintenanceLoadSeq: 0,
  recycleLoadSeq: 0,
  characterLibraryLoadSeq: 0,
  errorArtistsRequestSeq: 0,
  actionBusy: new Set(),
  lightboxIndex: -1,
  lastFocusedBeforeLightbox: null,
  lightboxZoom: 1,
  lightboxPanX: 0,
  lightboxPanY: 0,
  lightboxPanActive: false,
  lightboxPanPointerX: 0,
  lightboxPanPointerY: 0,
  lightboxPanStartX: 0,
  lightboxPanStartY: 0,
  lightboxPointers: new Map(),
  lightboxPinchActive: false,
  lightboxPinchStartDistance: 0,
  lightboxPinchStartZoom: 1,
  lightboxTapPointerId: null,
  lightboxTapStartX: 0,
  lightboxTapStartY: 0,
  lightboxTapMoved: false,
  lightboxLastTapAt: 0,
  lightboxLastTapX: 0,
  lightboxLastTapY: 0,
  lightboxWheelLastAt: 0,
  lightboxLoadToken: 0,
});

const maintenance = createStore({
  maintenanceView: 'overview',
  scanRunning: false,
  lastScanState: null,
  lastSeenScanRun: null,
  moveCandidates: [],
  movePendingTotal: 0,
  moveCandidateGroups: [],
  moveWaitingHashCount: 0,
  moveHistory: [],
  moveHistoryTotal: 0,
  moveHistoryHasMore: false,
  moveHistoryLimit: 0,
  moveHistoryLoading: false,
  folderRenameAuto: null,
  recycleBin: null,
  archivePlans: [],
  archiveSettings: null,
  archivePreview: null,
  archiveRun: null,
  archiveWorkbenchLoading: false,
  artistFolderMoveRoots: [],
  artistFolderMovePreview: null,
  artistFolderMoveLoading: false,
  artistFolderMoveError: '',
  artistFolderMoveParentPath: '',
  artistFolderMoveDirectoryPath: '',
  artistFolderMoveDirectoryEntries: [],
  artistFolderMoveDirectoryLoading: false,
  artistLinks: null,
  artistLinksLoading: false,
  artistLinksCategory: 'all',
  artistLinksProvider: 'all',
  artistLinksAvailability: 'all',
  artistLinksQuery: '',
  artistProfileLinks: null,
  artistProfileLinksLoading: false,
  characterLibrary: null,
  characterLibraryLoading: false,
  characterLibrarySelectedCharacterId: null,
  characterLibrarySearchQuery: '',
  characterLibraryMobileView: 'characters',
  characterImportJob: null,
  characterImportJobTimer: null,
  characterImportFinishedJobId: null,
  characterImportPollFailures: 0,
  hashStatus: null,
  dimensionBackfill: null,
  healthSummary: null,
  mlRuntimeStatus: null,
  mlRuntimeSettings: null,
  mlRuntimeSaving: false,
  readOnlyMode: false,
  errorArtists: [],
  errorArtistsTotal: null,
  errorArtistsQuery: '',
  errorArtistsSort: 'recent',
  errorArtistsOffset: 0,
  errorArtistsHasMore: false,
  errorArtistsLoading: false,
  errorArtistsScrollTop: 0,
  operationLog: null,
  _filterFocusReturn: null,
});

const DOMAINS = {browse, selection, maintenance};

function lookup(key) {
  for (const store of [browse, selection, maintenance]) {
    if (store.has(key)) return store;
  }
  return null;
}

// Legacy compatibility: `state.field` reads/writes the matching domain. Reads
// are cheap; writes route through the store so subscribers fire. Unknown keys
// throw on set (mirroring the per-store contract) and return undefined on
// get (to keep `state.someMissingField ?? defaultValue` patterns working).
export const state = new Proxy({}, {
  get(_target, key) {
    if (typeof key !== 'string') return undefined;
    const store = lookup(key);
    if (!store) return undefined;
    return store.get(key);
  },
  set(_target, key, value) {
    if (typeof key !== 'string') return true;
    const store = lookup(key);
    if (!store) {
      throw new Error(`store: unknown field "${key}"`);
    }
    store.set(key, value);
    return true;
  },
  has(_target, key) {
    if (typeof key !== 'string') return false;
    return Boolean(lookup(key));
  },
  ownKeys() {
    return [...browse.keys(), ...selection.keys(), ...maintenance.keys()];
  },
  getOwnPropertyDescriptor(_target, key) {
    if (typeof key !== 'string') return undefined;
    const store = lookup(key);
    if (!store) return undefined;
    return {enumerable: true, configurable: true};
  },
});

export {browse, selection, maintenance, DOMAINS};

// ---- Request-sequence and busy-guard tools ----
//
// These tools historically lived on the monolithic state. They continue to
// read/write through the per-domain stores so observable behavior matches.

export function nextRequestSeq(name) {
  const store = lookup(name);
  if (!store) {
    throw new Error(`nextRequestSeq: unknown counter "${name}"`);
  }
  const next = Number(store.get(name) || 0) + 1;
  store.set(name, next);
  return next;
}

export function isCurrentRequestSeq(name, seq) {
  const store = lookup(name);
  if (!store) return false;
  return Number(store.get(name) || 0) === Number(seq);
}

export function isTerminalScanState(s) {
  return Boolean(
    s && (
      (s.status === 'idle' && ['complete', 'stopped', 'interrupted', 'error', 'partial', 'failed'].includes(s.phase))
      || s.status === 'error'
    )
  );
}

export function scanRunKeyOf(s) {
  if (!s || typeof s !== 'object') return null;
  if (s.scan_id) return `scan_id:${s.scan_id}`;
  if (s.started_at != null) return `started_at:${s.started_at}`;
  if (s.updated_at != null) return `updated_at:${s.updated_at}`;
  return null;
}

export function shouldRefreshScanRun(s, lastSeenRun) {
  const key = scanRunKeyOf(s);
  if (key == null) return {refresh: false, key: null};
  if (isTerminalScanState(s) && key !== lastSeenRun) return {refresh: true, key};
  return {refresh: false, key};
}

export function ingestScanState(s) {
  const isInitialScanSnapshot = !maintenance.get('lastScanState');
  const wasScanning = Boolean(maintenance.get('lastScanState') && maintenance.get('lastScanState').status === 'scanning');
  maintenance.patch({lastScanState: s, scanRunning: s.status === 'scanning'});
  const gate = shouldRefreshScanRun(s, maintenance.get('lastSeenScanRun'));
  const isError = Boolean(s && (s.phase === 'error' || s.phase === 'failed' || s.status === 'error'));
  const isPartial = Boolean(s && s.phase === 'partial');
  const scanJustFinished = Boolean(wasScanning && isTerminalScanState(s));
  let refresh = false;
  if (s.status !== 'scanning' && gate.refresh) {
    maintenance.set('lastSeenScanRun', gate.key);
    if (!(isInitialScanSnapshot && isTerminalScanState(s))) {
      refresh = true;
    }
  }
  return {
    scanning: s.status === 'scanning',
    refresh,
    toast: scanJustFinished,
    phase: s ? s.phase : '',
    error: isError ? (s.current_path || s.error || '扫描出错') : (isPartial ? (s.current_path || '部分未完成') : null),
  };
}

export function actionBusyKey(name, id = '') {
  return id ? `${name}:${id}` : name;
}

export function isActionBusy(name, id = '') {
  return selection.get('actionBusy').has(actionBusyKey(name, id));
}

export function setActionBusy(name, id = '', busy = true) {
  const set = selection.get('actionBusy');
  const key = actionBusyKey(name, id);
  if (busy) set.add(key);
  else set.delete(key);
  // The Set reference rarely changes; rebroadcast for any subscribers that
  // capture it directly rather than reading through get().
  selection.set('actionBusy', set);
}
