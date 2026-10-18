// Lazy facade for the maintenance view modules.
//
// The browse UI must not pay for the maintenance workbench up front: each
// export here forwards to its owning module through one cached `import()`
// per module, so the maintenance bundle is fetched on first use and call
// sites keep their original shape. Repeated calls queue onto the same
// module promise, so sequential calls keep their order. Tiny synchronous
// helpers live in shared.js instead (see that file for why).

import { downloadTemplateInputSelector, movePanelScrollTop, restoreMovePanelScroll } from './shared.js';

export { downloadTemplateInputSelector, movePanelScrollTop, restoreMovePanelScroll };

const moduleLoads = new Map();
function load(spec) {
  if (!moduleLoads.has(spec)) {
    const pending = import(spec);
    moduleLoads.set(spec, pending);
    // 一次网络抖动不能让该模块在本次会话里永久不可用：失败的动态导入
    // 不留在缓存里，下一次调用会重新拉取（调用方照常把错误 toast 出来）。
    pending.catch(() => {
      if (moduleLoads.get(spec) === pending) moduleLoads.delete(spec);
    });
  }
  return moduleLoads.get(spec);
}

function fwd(spec, name) {
  return (...args) => load(spec).then(mod => mod[name](...args));
}

export const addDownloadSubscription = fwd('./downloads.js', 'addDownloadSubscription');
export const annotateArchivePlan = fwd('./organize.js', 'annotateArchivePlan');
export const applyArchiveTemplate = fwd('./organize.js', 'applyArchiveTemplate');
export const applyCharacterLibraryMobileView = fwd('./characters.js', 'applyCharacterLibraryMobileView');
export const applyDownloadWorksFilter = fwd('./downloads.js', 'applyDownloadWorksFilter');
export const autoResolveMoveCandidates = fwd('./paths.js', 'autoResolveMoveCandidates');
export const backfillItemDimensions = fwd('./overview.js', 'backfillItemDimensions');
export const batchPurgeRecycleEntries = fwd('./records.js', 'batchPurgeRecycleEntries');
export const batchRestoreRecycleEntries = fwd('./records.js', 'batchRestoreRecycleEntries');
export const bindDownloadCandidate = fwd('./downloads.js', 'bindDownloadCandidate');
export const cancelCharacterImportJob = fwd('./characters.js', 'cancelCharacterImportJob');
export const checkDownloadMissing = fwd('./downloads.js', 'checkDownloadMissing');
export const checkNetdiskPaths = fwd('./netdisk.js', 'checkNetdiskPaths');
export const checkSubscriptionMissing = fwd('./downloads.js', 'checkSubscriptionMissing');
export const chooseDirectoryPicker = fwd('./organize.js', 'chooseDirectoryPicker');
export const clearDownloadWorksSelection = fwd('./downloads.js', 'clearDownloadWorksSelection');
export const clearRecycleBin = fwd('./records.js', 'clearRecycleBin');
export const clearRecycleSelection = fwd('./records.js', 'clearRecycleSelection');
export const closeDirectoryPicker = fwd('./organize.js', 'closeDirectoryPicker');
export const closeDownloadArtistCombo = fwd('./downloads.js', 'closeDownloadArtistCombo');
export const closeDownloadWorksArtistCombo = fwd('./downloads.js', 'closeDownloadWorksArtistCombo');
export const closeErrorArtistsDialog = fwd('./overview.js', 'closeErrorArtistsDialog');
export const connectNetdisk = fwd('./netdisk.js', 'connectNetdisk');
export const copyNetdiskScript = fwd('./netdisk.js', 'copyNetdiskScript');
export const createCharacter = fwd('./characters.js', 'createCharacter');
export const decideDownloadPost = fwd('./downloads.js', 'decideDownloadPost');
export const deleteCharacter = fwd('./characters.js', 'deleteCharacter');
export const deleteCharacterReference = fwd('./characters.js', 'deleteCharacterReference');
export const deleteDownloadSubscription = fwd('./downloads.js', 'deleteDownloadSubscription');
export const deliverPostToNetdisk = fwd('./downloads.js', 'deliverPostToNetdisk');
export const disconnectNetdisk = fwd('./netdisk.js', 'disconnectNetdisk');
export const downloadPostByHand = fwd('./downloads.js', 'downloadPostByHand');
export const downloadWorksNetdiskSelection = fwd('./downloads.js', 'downloadWorksNetdiskSelection');
export const downloadWorksPost = fwd('./downloads.js', 'downloadWorksPost');
export const downloadWorksSelected = fwd('./downloads.js', 'downloadWorksSelected');
export const downloadWorksSelection = fwd('./downloads.js', 'downloadWorksSelection');
export const ensureNetdiskDispatchReady = fwd('./netdisk.js', 'ensureNetdiskDispatchReady');
export const executeArchivePlans = fwd('./organize.js', 'executeArchivePlans');
export const executeArtistFolderMove = fwd('./organize.js', 'executeArtistFolderMove');
export const generateNetdiskScript = fwd('./netdisk.js', 'generateNetdiskScript');
export const gotoCharacterLibraryPanel = fwd('./characters.js', 'gotoCharacterLibraryPanel');
export const handleMaintenanceJump = fwd('./index.js', 'handleMaintenanceJump');
export const importCharacterLibraryReferences = fwd('./characters.js', 'importCharacterLibraryReferences');
export const importDownloadPost = fwd('./downloads.js', 'importDownloadPost');
export const insertDownloadTemplateToken = fwd('./downloads.js', 'insertDownloadTemplateToken');
export const invalidateArtistFolderMovePreview = fwd('./organize.js', 'invalidateArtistFolderMovePreview');
export const jumpToArchivePlanFolder = fwd('./organize.js', 'jumpToArchivePlanFolder');
export const jumpToDownloadWorkFolder = fwd('./downloads.js', 'jumpToDownloadWorkFolder');
export const jumpToErrorArtist = fwd('./overview.js', 'jumpToErrorArtist');
export const loadArchiveWorkbench = fwd('./organize.js', 'loadArchiveWorkbench');
export const loadCharacterLibrary = fwd('./characters.js', 'loadCharacterLibrary');
export const loadDirectoryPicker = fwd('./organize.js', 'loadDirectoryPicker');
export const loadDownloadCandidates = fwd('./downloads.js', 'loadDownloadCandidates');
export const loadErrorArtistsPage = fwd('./overview.js', 'loadErrorArtistsPage');
export const loadHealthSummary = fwd('./overview.js', 'loadHealthSummary');
export const loadMoreDownloadDayPosts = fwd('./downloads.js', 'loadMoreDownloadDayPosts');
export const loadMoreDownloadWorks = fwd('./downloads.js', 'loadMoreDownloadWorks');
export const loadMoveWorkbench = fwd('./index.js', 'loadMoveWorkbench');
export const loadRecycleBin = fwd('./records.js', 'loadRecycleBin');
export const markDownloadSettingsDirty = fwd('./downloads.js', 'markDownloadSettingsDirty');
export const markNetdiskSettingsDirty = fwd('./netdisk.js', 'markNetdiskSettingsDirty');
export const moveDownloadArtistComboActive = fwd('./downloads.js', 'moveDownloadArtistComboActive');
export const moveDownloadWorksArtistComboActive = fwd('./downloads.js', 'moveDownloadWorksArtistComboActive');
export const openCharacterReferences = fwd('./characters.js', 'openCharacterReferences');
export const openDirectoryPicker = fwd('./organize.js', 'openDirectoryPicker');
export const openDownloadArtistCombo = fwd('./downloads.js', 'openDownloadArtistCombo');
export const openDownloadDay = fwd('./downloads.js', 'openDownloadDay');
export const openDownloadPostFiles = fwd('./downloads.js', 'openDownloadPostFiles');
export const openDownloadWorksArtistCombo = fwd('./downloads.js', 'openDownloadWorksArtistCombo');
export const openErrorArtistsDialog = fwd('./overview.js', 'openErrorArtistsDialog');
export const openNamingMigrationPreview = fwd('./downloads.js', 'openNamingMigrationPreview');
export const pickDownloadArtist = fwd('./downloads.js', 'pickDownloadArtist');
export const pickDownloadWorksArtist = fwd('./downloads.js', 'pickDownloadWorksArtist');
export const previewArchivePlans = fwd('./organize.js', 'previewArchivePlans');
export const previewArtistFolderMove = fwd('./organize.js', 'previewArtistFolderMove');
export const purgeRecycleEntry = fwd('./records.js', 'purgeRecycleEntry');
export const rebuildCharacterIndex = fwd('./characters.js', 'rebuildCharacterIndex');
export const reconcileDownloadLibrary = fwd('./downloads.js', 'reconcileDownloadLibrary');
export const reconcileSubscriptionLibrary = fwd('./downloads.js', 'reconcileSubscriptionLibrary');
export const refreshActiveMaintenanceView = fwd('./index.js', 'refreshActiveMaintenanceView');
export const refreshArchivePlans = fwd('./organize.js', 'refreshArchivePlans');
export const refreshDownloadLog = fwd('./downloads.js', 'refreshDownloadLog');
export const rememberDownloadTemplateInput = fwd('./downloads.js', 'rememberDownloadTemplateInput');
export const renderCharacterLibrary = fwd('./characters.js', 'renderCharacterLibrary');
export const renderDirectoryPicker = fwd('./organize.js', 'renderDirectoryPicker');
export const renderDownloadArtistCombo = fwd('./downloads.js', 'renderDownloadArtistCombo');
export const renderDownloadArtistFolder = fwd('./downloads.js', 'renderDownloadArtistFolder');
export const renderDownloadSubscriptions = fwd('./downloads.js', 'renderDownloadSubscriptions');
export const renderDownloadWorksArtistCombo = fwd('./downloads.js', 'renderDownloadWorksArtistCombo');
export const renderDownloadsPanel = fwd('./downloads.js', 'renderDownloadsPanel');
export const resetDownloadTemplates = fwd('./downloads.js', 'resetDownloadTemplates');
export const resetNetdiskPairing = fwd('./netdisk.js', 'resetNetdiskPairing');
export const resolveNetdiskDispatchPost = fwd('./netdisk.js', 'resolveNetdiskDispatchPost');
export const restoreRecycleEntry = fwd('./records.js', 'restoreRecycleEntry');
export const retryDownloadFile = fwd('./downloads.js', 'retryDownloadFile');
export const retryMlRuntime = fwd('./overview.js', 'retryMlRuntime');
export const runFolderRenameAllNow = fwd('./overview.js', 'runFolderRenameAllNow');
export const runNetdiskJobAction = fwd('./netdisk.js', 'runNetdiskJobAction');
export const saveArchiveSettings = fwd('./organize.js', 'saveArchiveSettings');
export const saveDownloadsSettings = fwd('./downloads.js', 'saveDownloadsSettings');
export const saveMlDownloadSource = fwd('./overview.js', 'saveMlDownloadSource');
export const saveNetdiskSettings = fwd('./netdisk.js', 'saveNetdiskSettings');
export const scheduleArchiveDraftPreview = fwd('./organize.js', 'scheduleArchiveDraftPreview');
export const scheduleDownloadWorksSearch = fwd('./downloads.js', 'scheduleDownloadWorksSearch');
export const scheduleMaintenanceAutoRefresh = fwd('./index.js', 'scheduleMaintenanceAutoRefresh');
export const selectActiveDownloadArtistCombo = fwd('./downloads.js', 'selectActiveDownloadArtistCombo');
export const selectActiveDownloadWorksArtistCombo = fwd('./downloads.js', 'selectActiveDownloadWorksArtistCombo');
export const selectAllRecycleEntries = fwd('./records.js', 'selectAllRecycleEntries');
export const setCharacterLibraryMobileView = fwd('./characters.js', 'setCharacterLibraryMobileView');
export const setDownloadSubscriptionMode = fwd('./downloads.js', 'setDownloadSubscriptionMode');
export const setFolderRenameAutoEnabled = fwd('./overview.js', 'setFolderRenameAutoEnabled');
export const setMaintenanceView = fwd('./index.js', 'setMaintenanceView');
export const setNetdiskJobFilter = fwd('./netdisk.js', 'setNetdiskJobFilter');
export const setOperationHistoryFilter = fwd('./records.js', 'setOperationHistoryFilter');
export const setRecycleStrategy = fwd('./records.js', 'setRecycleStrategy');
export const setRecycleViewMode = fwd('./records.js', 'setRecycleViewMode');
export const startDownloadSync = fwd('./downloads.js', 'startDownloadSync');
export const startFullScan = fwd('./overview.js', 'startFullScan');
export const startMaintenanceAutoRefresh = fwd('./index.js', 'startMaintenanceAutoRefresh');
export const stepOrganizeArtist = fwd('./organize.js', 'stepOrganizeArtist');
export const stopDownloadPost = fwd('./downloads.js', 'stopDownloadPost');
export const stopMaintenanceAutoRefresh = fwd('./index.js', 'stopMaintenanceAutoRefresh');
export const submitNetdiskDispatch = fwd('./netdisk.js', 'submitNetdiskDispatch');
export const switchOrganizeArtist = fwd('./organize.js', 'switchOrganizeArtist');
export const syncArchiveRuleDirtyState = fwd('./organize.js', 'syncArchiveRuleDirtyState');
export const syncMaintenanceTabsEdge = fwd('./index.js', 'syncMaintenanceTabsEdge');
export const testNetdiskConnection = fwd('./netdisk.js', 'testNetdiskConnection');
export const toggleAllArchivePlansConfirmation = fwd('./organize.js', 'toggleAllArchivePlansConfirmation');
export const toggleArchivePlanConfirmation = fwd('./organize.js', 'toggleArchivePlanConfirmation');
export const toggleArchivePlansFold = fwd('./organize.js', 'toggleArchivePlansFold');
export const toggleDownloadLogFold = fwd('./downloads.js', 'toggleDownloadLogFold');
export const toggleDownloadSubscription = fwd('./downloads.js', 'toggleDownloadSubscription');
export const toggleDownloadType = fwd('./downloads.js', 'toggleDownloadType');
export const toggleDownloadWorkSelection = fwd('./downloads.js', 'toggleDownloadWorkSelection');
export const toggleDownloadWorksSelectAll = fwd('./downloads.js', 'toggleDownloadWorksSelectAll');
export const toggleNetdiskJobsFold = fwd('./netdisk.js', 'toggleNetdiskJobsFold');
export const toggleOperationHistoryFold = fwd('./records.js', 'toggleOperationHistoryFold');
export const toggleRecycleSelection = fwd('./records.js', 'toggleRecycleSelection');
export const undoArchivePlan = fwd('./organize.js', 'undoArchivePlan');
export const uploadCharacterReference = fwd('./characters.js', 'uploadCharacterReference');
export const verifyDownloadPost = fwd('./downloads.js', 'verifyDownloadPost');
