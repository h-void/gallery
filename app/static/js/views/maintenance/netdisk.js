// Maintenance 网盘下载 panel: the JDownloader local-bridge settings, the pairing
// script, the directory checks and the task list.
//
// Everything here is the local mode described by the netdisk plan §6.9. The
// optional MyJDownloader mode (§6.5) is deliberately absent: the plan says an
// unimplemented mode must not be shown as a clickable empty option, so there is
// no mode selector and no account fields anywhere in this file.
//
// The wording is the plan's approved 终稿 (§6.2), not something invented here.
// Statuses and task states are the exact strings the backend returns, and the
// panel never substitutes a percentage for "下载完成".

import { API } from '../../api.js';
import { state, isActionBusy, setActionBusy } from '../../store.js';
import { $, $$, escHtml, isAbortError, copyText } from '../../utils.js';
import { toast, logUiAction } from '../../logging.js';

// 检查间隔: the plan allows 5–300 seconds; below that the local bridge would
// spend its whole time in its own request.
export const NETDISK_INTERVAL_MIN = 5;
export const NETDISK_INTERVAL_MAX = 300;

// 保留空间: minimum 1 GiB. A smaller reserve cannot absorb a file that is
// already being written.
export const NETDISK_RESERVE_MIN = 1;

export const NETDISK_DEFAULTS = {
  enabled: false,
  auto_start: true,
  auto_import: false,
  cleanup_after_import: false,
  staging_dir: '',
  import_dir: '',
  check_interval_secs: 10,
  reserve_space_gib: 10,
};

// Connection statuses, in the order the backend can produce them. `move_isolated`
// is not one of them: it is a separate fact about whether a move is authorised,
// and mixing it in would let "connected" imply "safe to start".
export const NETDISK_STATES = ['未配置', '连接中', '已连接', '已断开', '设备离线', '凭据失效', '路径不可用', '待核对'];

// Task states §6.2. A task the bridge has not confirmed is 待核对, never a
// success and never a failure.
export const NETDISK_JOB_STATES = [
  '待解析', '解析中', '待开始', '下载中', '已暂停', '需处理', '下载完成', '待入库', '已入库', '失败', '待核对',
];

// Per-task commands §6.2. 停止 and 删除 are intentionally separate entries; the
// plan forbids collapsing them into one button.
export const NETDISK_JOB_ACTIONS = [
  {action: 'start', label: '开始'},
  {action: 'pause', label: '暂停'},
  {action: 'resume', label: '继续'},
  {action: 'retry', label: '重试'},
  {action: 'import', label: '入库'},
  {action: 'remove', label: '移除任务', confirm: '移除任务记录？已下载文件不会删除。'},
];

// Clamp and floor, so a partly typed number never reaches the API as 0 or as a
// value that would peg the bridge at its own timeout.
export function clampNetdiskInterval(value) {
  const n = Math.round(Number(value));
  if (!Number.isFinite(n)) return NETDISK_DEFAULTS.check_interval_secs;
  return Math.min(NETDISK_INTERVAL_MAX, Math.max(NETDISK_INTERVAL_MIN, n));
}

export function clampNetdiskReserve(value) {
  const n = Math.round(Number(value));
  if (!Number.isFinite(n)) return NETDISK_DEFAULTS.reserve_space_gib;
  return Math.max(NETDISK_RESERVE_MIN, n);
}

export async function loadNetdiskPanel(options = {}) {
  const fetchOptions = options.signal ? {signal: options.signal} : {};
  state.netdiskLoading = true;
  try {
    // Settled individually: one unreadable section must not blank the others.
    // The panel re-reads on a timer, and a bridge that was never configured is
    // the normal starting state, not an error worth losing the form over.
    const [settings, connection, jobs] = await Promise.allSettled([
      API.get('/api/netdisk/settings', fetchOptions),
      API.get('/api/netdisk/connection', fetchOptions),
      listNetdiskJobs(fetchOptions),
    ]);
    if (settings.status === 'fulfilled') {
      state.netdiskSettings = {...NETDISK_DEFAULTS, ...(settings.value || {})};
    }
    if (connection.status === 'fulfilled') {
      state.netdiskConnection = connection.value || null;
    } else if (!isAbortError(connection.reason)) {
      logUiAction('netdisk connection read failed', {error: String(connection.reason && connection.reason.message ? connection.reason.message : connection.reason)});
    }
    if (jobs.status === 'fulfilled') {
      state.netdiskJobs = jobs.value;
    }
    if (settings.status === 'rejected' && !isAbortError(settings.reason)) {
      logUiAction('netdisk settings read failed', {error: String(settings.reason && settings.reason.message ? settings.reason.message : settings.reason)});
    }
  } finally {
    state.netdiskLoading = false;
  }
}

async function listNetdiskJobs(fetchOptions) {
  try {
    // The route has no paging: it is the bounded list the bridge tracks, and a
    // half-page would read as "the rest are gone".
    const body = await API.get('/api/netdisk/jobs', fetchOptions);
    return Array.isArray(body?.jobs) ? body.jobs : [];
  } catch (e) {
    if (isAbortError(e)) throw e;
    return [];
  }
}

export function renderNetdiskPanel() {
  const settings = state.netdiskSettings;
  const disabled = !settings;
  if (!state.netdiskSettingsDirty) {
    setToggle('#netdiskEnabledToggle', settings?.enabled);
    setToggle('#netdiskAutoStartToggle', settings?.auto_start);
    setToggle('#netdiskAutoImportToggle', settings?.auto_import);
    setToggle('#netdiskCleanupAfterImportToggle', settings?.cleanup_after_import);
    const staging = $('#netdiskStagingDirInput');
    if (staging && settings) staging.value = String(settings.staging_dir || '');
    const importDir = $('#netdiskImportDirInput');
    if (importDir && settings) importDir.value = String(settings.import_dir || '');
    const interval = $('#netdiskIntervalInput');
    if (interval && settings) interval.value = String(settings.check_interval_secs ?? NETDISK_DEFAULTS.check_interval_secs);
    const reserve = $('#netdiskReserveInput');
    if (reserve && settings) reserve.value = String(settings.reserve_space_gib ?? NETDISK_DEFAULTS.reserve_space_gib);
  }
  for (const selector of ['#netdiskEnabledToggle', '#netdiskAutoStartToggle', '#netdiskAutoImportToggle', '#netdiskCleanupAfterImportToggle',
    '#netdiskStagingDirInput', '#netdiskImportDirInput', '#netdiskIntervalInput', '#netdiskReserveInput']) {
    const el = $(selector);
    if (el) el.disabled = disabled;
  }
  // 下载后入库 and 入库后清理 are only meaningful once the script has passed its capability
  // handshake. The backend still refuses the save; the panel says so up front
  // rather than letting the user flip a switch that cannot take effect.
  const importToggle = $('#netdiskAutoImportToggle');
  if (importToggle) {
    const handshakeOk = Boolean(state.netdiskConnection?.handshaked);
    importToggle.disabled = disabled || !handshakeOk;
    importToggle.title = handshakeOk
      ? '能力检查通过后允许开启；仅导入已验证清单'
      : '需要先完成脚本能力握手才能开启下载后入库';
  }
  const cleanupToggle = $('#netdiskCleanupAfterImportToggle');
  if (cleanupToggle) {
    const handshakeOk = Boolean(state.netdiskConnection?.handshaked);
    cleanupToggle.disabled = disabled || !handshakeOk;
    cleanupToggle.title = handshakeOk
      ? '能力检查通过后允许开启；入库成功后删除暂存文件并从下载器移除已完成任务'
      : '需要先完成脚本能力握手才能开启入库后清理';
  }
  setButtonBusy('#netdiskSaveBtn', disabled, 'netdiskSave');
  setButtonBusy('#netdiskTestBtn', disabled, 'netdiskTest');
  setButtonBusy('#netdiskPathCheckBtn', disabled, 'netdiskPathCheck');
  const reconnect = Boolean(state.netdiskConnection?.disconnected);
  const connectBtn = $('#netdiskConnectBtn');
  if (connectBtn) connectBtn.hidden = !reconnect;
  setButtonBusy('#netdiskConnectBtn', disabled || !state.netdiskSettings?.bridge_configured, 'netdiskConnect');
  const disconnectBtn = $('#netdiskDisconnectBtn');
  if (disconnectBtn) disconnectBtn.hidden = reconnect;
  setButtonBusy('#netdiskDisconnectBtn', disabled || !state.netdiskSettings?.bridge_configured, 'netdiskDisconnect');
  setButtonBusy('#netdiskScriptBtn', disabled || isActionBusy('netdiskResetPairing'), 'netdiskScript');
  setButtonBusy('#netdiskResetPairingBtn', disabled || isActionBusy('netdiskScript'), 'netdiskResetPairing');
  renderNetdiskStatus();
  renderNetdiskJobs();
  renderNetdiskScript();
}

function setToggle(selector, value) {
  const el = $(selector);
  if (el) el.checked = Boolean(value);
}

function setButtonBusy(selector, disabled, key) {
  const el = $(selector);
  if (el) el.disabled = disabled || isActionBusy(key);
}

export function renderNetdiskStatus() {
  const el = $('#netdiskStatusText');
  if (!el) return;
  const connection = state.netdiskConnection;
  const raw = String(connection?.state || (state.netdiskSettings?.bridge_configured ? '连接中' : '未配置'));
  const label = NETDISK_STATES.includes(raw) ? raw : '待核对';
  const parts = [label];
  // The §6.6 pre-check is reported next to the badge, not hidden in a tooltip:
  // when it is false every 开始 is refused, and the user has to be able to see
  // why without clicking anything.
  if (connection?.connected && connection.handshaked && connection.move_isolated === false) {
    parts.push(connection.auto_start_enabled === true
      ? 'JDownloader 全局自动开始未关闭，已停止移入'
      : '无法读取 JDownloader 自动开始设置，已停止移入');
  }
  el.textContent = parts.join(' \u00b7 ');
  el.dataset.netdiskState = label;
}

// Which commands make sense for a stored state. The backend refuses the rest
// with a reason anyway, but a button that can only fail is not an option the
// panel should offer.
const NETDISK_ACTION_STATES = {
  start: ['issued'],
  pause: ['submitted'],
  resume: ['submitted'],
  retry: ['issued', 'submitted', 'confirmed'],
  import: ['confirmed'],
  remove: ['issued', 'submitted', 'confirmed'],
};

export const NETDISK_JOBS_FOLD_THRESHOLD = 5;

export function isNetdiskJobCompleted(job) {
  if (!job) return false;
  const linked = Number(job.linked ?? 0);
  const total = Number(job.total ?? 0);
  if (total > 0 && linked >= total) return true;
  return String(job.state_label || '') === '已入库';
}

export function setNetdiskJobFilter(filter) {
  state.netdiskJobFilter = ['pending', 'completed'].includes(filter) ? filter : 'all';
  state.netdiskJobsExpanded = false;
  renderNetdiskJobs();
}

export function toggleNetdiskJobsFold() {
  state.netdiskJobsExpanded = !state.netdiskJobsExpanded;
  renderNetdiskJobs();
}

export function renderNetdiskJobs() {
  const list = $('#netdiskJobList');
  const count = $('#netdiskJobCount');
  const allJobs = Array.isArray(state.netdiskJobs) ? state.netdiskJobs : [];

  const completedJobs = allJobs.filter(isNetdiskJobCompleted);
  const pendingJobs = allJobs.filter(j => !isNetdiskJobCompleted(j));
  const counts = {
    all: allJobs.length,
    pending: pendingJobs.length,
    completed: completedJobs.length,
  };

  const countAll = $('#netdiskJobCountAll');
  if (countAll) countAll.textContent = counts.all ? `（${counts.all}）` : '';
  const countPending = $('#netdiskJobCountPending');
  if (countPending) countPending.textContent = counts.pending ? `（${counts.pending}）` : '';
  const countCompleted = $('#netdiskJobCountCompleted');
  if (countCompleted) countCompleted.textContent = counts.completed ? `（${counts.completed}）` : '';

  const currentFilter = state.netdiskJobFilter || 'all';
  if (typeof $$ === 'function') {
    const tagButtons = $$('#netdiskJobTags [data-netdisk-filter]');
    if (tagButtons) {
      tagButtons.forEach(btn => {
        btn.classList.toggle('active', btn.dataset.netdiskFilter === currentFilter);
      });
    }
  }

  let filteredJobs = allJobs;
  if (currentFilter === 'pending') filteredJobs = pendingJobs;
  else if (currentFilter === 'completed') filteredJobs = completedJobs;

  if (count) {
    if (!allJobs.length) {
      count.textContent = '';
    } else if (currentFilter === 'all') {
      count.textContent = `共 ${allJobs.length} 个任务`;
    } else if (currentFilter === 'pending') {
      count.textContent = `未完成 ${pendingJobs.length} 个`;
    } else if (currentFilter === 'completed') {
      count.textContent = `已完成 ${completedJobs.length} 个`;
    }
  }

  const foldWrap = $('#netdiskJobsFoldWrap');
  const foldBtn = $('#netdiskJobsFoldBtn');

  if (!list) return;

  if (!allJobs.length) {
    list.innerHTML = '<div class="move-empty small">还没有网盘任务</div>';
    if (foldWrap) foldWrap.hidden = true;
    return;
  }

  if (!filteredJobs.length) {
    const emptyText = currentFilter === 'pending' ? '没有未完成的任务' : '没有已完成的任务';
    list.innerHTML = `<div class="move-empty small">${emptyText}</div>`;
    if (foldWrap) foldWrap.hidden = true;
    return;
  }

  const shouldFold = filteredJobs.length > NETDISK_JOBS_FOLD_THRESHOLD;
  const isExpanded = Boolean(state.netdiskJobsExpanded);
  const visibleJobs = (shouldFold && !isExpanded)
    ? filteredJobs.slice(0, NETDISK_JOBS_FOLD_THRESHOLD)
    : filteredJobs;

  if (foldWrap && foldBtn) {
    if (shouldFold) {
      foldWrap.hidden = false;
      const hiddenCount = filteredJobs.length - NETDISK_JOBS_FOLD_THRESHOLD;
      foldBtn.textContent = isExpanded ? '收起' : `展开剩余 ${hiddenCount} 项`;
    } else {
      foldWrap.hidden = true;
    }
  }

  list.innerHTML = visibleJobs.map(job => {
    const id = escHtml(String(job.task_id ?? ''));
    const name = escHtml(String(job.package_name || job.task_id || '未命名任务'));
    // Only the approved wording reaches the screen. A label the backend did not
    // produce becomes 待核对 rather than being printed as-is.
    const rawLabel = String(job.state_label || '');
    const label = escHtml(NETDISK_JOB_STATES.includes(rawLabel) ? rawLabel : '待核对');
    const links = Number(job.link_count ?? 0);
    const total = Number(job.total ?? 0);
    const linked = Number(job.linked ?? 0);
    const meta = [`状态 ${label}`];
    if (links > 0) meta.push(`${links} 条链接`);
    // Reported as "linked / total", never as a percentage: the plan forbids a
    // bar that reaches 100% while files are still unbound.
    if (total > 0) meta.push(`已入库 ${linked}/${total}`);
    if (job.attention) meta.push(escHtml(String(job.attention)));
    const actions = NETDISK_JOB_ACTIONS.map(item => {
      const allowed = NETDISK_ACTION_STATES[item.action] || [];
      const enabled = allowed.includes(String(job.state || ''))
        && !isActionBusy(`netdiskJob:${job.task_id}:${item.action}`);
      return `<button class="btn" type="button" data-netdisk-job-action="${item.action}" data-netdisk-job="${id}"${enabled ? '' : ' disabled'}>${escHtml(item.label)}</button>`;
    }).join('');
    return `
      <div class="netdisk-job-item" data-netdisk-job-row="${id}">
        <div class="netdisk-job-main">
          <div class="netdisk-job-title">${name}</div>
          <div class="netdisk-job-meta">${meta.join(' &#183; ')}</div>
        </div>
        <div class="netdisk-job-actions">${actions}</div>
      </div>`;
  }).join('');
}

export function renderNetdiskScript() {
  const box = $('#netdiskScriptBox');
  const pre = $('#netdiskScriptText');
  if (!box || !pre) return;
  const script = state.netdiskScript;
  if (!script) {
    box.hidden = true;
    pre.textContent = '';
    return;
  }
  box.hidden = false;
  pre.textContent = String(script.script || '');
  const hint = $('#netdiskScriptHint');
  if (hint) {
    hint.textContent = '在 JDownloader 的 Event Scripter 中替换原脚本，设为每 10 秒运行并批准权限。查看和复制不会更换密钥；重置配对后须替换 JD 脚本。脚本包含配对密钥，请勿分享。';
  }
}

export function markNetdiskSettingsDirty() {
  state.netdiskSettingsDirty = true;
  const badge = $('#netdiskDirtyBadge');
  if (badge) badge.hidden = false;
}

function readNetdiskForm() {
  const settings = state.netdiskSettings || NETDISK_DEFAULTS;
  const value = selector => {
    const el = $(selector);
    return el ? String(el.value ?? '') : '';
  };
  const checked = selector => {
    const el = $(selector);
    return el ? Boolean(el.checked) : false;
  };
  return {
    enabled: checked('#netdiskEnabledToggle'),
    auto_start: checked('#netdiskAutoStartToggle'),
    auto_import: checked('#netdiskAutoImportToggle'),
    cleanup_after_import: checked('#netdiskCleanupAfterImportToggle'),
    staging_dir: value('#netdiskStagingDirInput').trim(),
    import_dir: value('#netdiskImportDirInput').trim(),
    check_interval_secs: clampNetdiskInterval(value('#netdiskIntervalInput')),
    reserve_space_gib: clampNetdiskReserve(value('#netdiskReserveInput')),
    bridge_configured: Boolean(settings.bridge_configured),
  };
}

export async function saveNetdiskSettings() {
  if (isActionBusy('netdiskSave')) return;
  const payload = readNetdiskForm();
  if (!payload.staging_dir) {
    toast('请先填写暂存目录', 'warn');
    return;
  }
  setActionBusy('netdiskSave', '', true);
  setButtonBusy('#netdiskSaveBtn', false, 'netdiskSave');
  try {
    await API.putJson('/api/netdisk/settings', payload);
    state.netdiskSettings = payload;
    state.netdiskSettingsDirty = false;
    const badge = $('#netdiskDirtyBadge');
    if (badge) badge.hidden = true;
    toast('网盘设置已保存');
    await loadNetdiskPanel();
    renderNetdiskPanel();
  } catch (e) {
    // A 409 here is the backend refusing 下载后入库 before the capability
    // handshake; its message is the approved wording, so it is shown as-is.
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskSave', '', false);
    renderNetdiskPanel();
  }
}

// The inbound bridge is online only while its authenticated heartbeat is fresh.
export async function testNetdiskConnection() {
  if (isActionBusy('netdiskTest')) return;
  setActionBusy('netdiskTest', '', true);
  setButtonBusy('#netdiskTestBtn', false, 'netdiskTest');
  try {
    const body = await API.post('/api/netdisk/test');
    const parts = [];
    if (body?.disconnected || body?.state === '已断开') {
      parts.push('已断开连接，请重新连接');
    } else if (body?.connected && body?.handshaked) {
      parts.push('已连接');
    } else if (body?.state === '设备离线') {
      parts.push('设备离线，请检查 JDownloader 和连接脚本');
    } else if (body?.bridge_configured) {
      parts.push('配对密钥已生成 \u00b7 等待下载器连接');
    } else {
      parts.push('尚未生成配对密钥');
    }
    if (body?.enabled === false) parts.push('网盘下载已关闭');
    toast(parts.join(' \u00b7 ') || '待核对', body?.connected ? 'success' : 'warn');
    // The badge comes from the real read, not from this probe: the probe is a
    // different question and must not overwrite the connection state.
    await refreshNetdiskConnection();
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskTest', '', false);
    renderNetdiskPanel();
  }
}

async function refreshNetdiskConnection() {
  try {
    state.netdiskConnection = await API.get('/api/netdisk/connection');
  } catch (e) {
    if (!isAbortError(e)) logUiAction('netdisk connection read failed', {error: String(e && e.message ? e.message : e)});
  }
}

export async function checkNetdiskPaths() {
  if (isActionBusy('netdiskPathCheck')) return;
  const staging = $('#netdiskStagingDirInput');
  const importDir = $('#netdiskImportDirInput');
  if (!staging || !staging.value.trim()) {
    toast('请先填写暂存目录', 'warn');
    return;
  }
  setActionBusy('netdiskPathCheck', '', true);
  setButtonBusy('#netdiskPathCheckBtn', false, 'netdiskPathCheck');
  try {
    const body = await API.postJson('/api/netdisk/path-check', {
      staging_dir: staging.value.trim(),
      import_dir: importDir ? importDir.value.trim() : '',
    });
    // The route answers one fact per directory. Collapsing them into a single
    // boolean would hide which of the two is unusable.
    const reasons = [];
    if (body?.staging && body.staging.ok === false) reasons.push(`暂存目录：${body.staging.reason || '不可用'}`);
    if (body?.import && body.import.ok === false) reasons.push(`入库目录：${body.import.reason || '不可用'}`);
    const ok = body?.ok !== false && reasons.length === 0;
    toast(ok ? '目录检查通过' : reasons.join(' \u00b7 '), ok ? 'info' : 'error');
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskPathCheck', '', false);
    setButtonBusy('#netdiskPathCheckBtn', !state.netdiskSettings, 'netdiskPathCheck');
  }
}

export async function connectNetdisk() {
  if (isActionBusy('netdiskConnect')) return;
  setActionBusy('netdiskConnect', '', true);
  setButtonBusy('#netdiskConnectBtn', false, 'netdiskConnect');
  try {
    state.netdiskConnection = await API.post('/api/netdisk/connect');
    toast(state.netdiskConnection?.connected ? '已连接' : '等待下载器连接');
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskConnect', '', false);
    renderNetdiskPanel();
  }
}

export async function disconnectNetdisk() {
  if (isActionBusy('netdiskDisconnect')) return;
  setActionBusy('netdiskDisconnect', '', true);
  setButtonBusy('#netdiskDisconnectBtn', false, 'netdiskDisconnect');
  try {
    await API.post('/api/netdisk/disconnect');
    // After an explicit disconnect the panel must not keep implying it tracks
    // progress: the status badge goes straight to 已断开.
    if (state.netdiskConnection) {
      state.netdiskConnection = {...state.netdiskConnection, connected: false, handshaked: false, disconnected: true, state: '已断开'};
    }
    toast('已断开连接。已投递的任务会继续下载。');
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskDisconnect', '', false);
    renderNetdiskPanel();
  }
}

export async function generateNetdiskScript() {
  if (isActionBusy('netdiskScript') || isActionBusy('netdiskResetPairing')) return;
  setActionBusy('netdiskScript', '', true);
  setButtonBusy('#netdiskScriptBtn', false, 'netdiskScript');
  try {
    // Retrieve the existing pairing; only the first setup creates a key.
    const body = await API.postJson('/api/netdisk/script', {});
    state.netdiskScript = body || null;
    await loadNetdiskPanel();
    renderNetdiskPanel();
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy('netdiskScript', '', false);
    renderNetdiskPanel();
  }
}

export async function resetNetdiskPairing() {
  if (isActionBusy('netdiskResetPairing') || isActionBusy('netdiskScript')) return;
  if (!window.confirm('重置配对？旧脚本将失效，需要在 JD 中替换连接脚本。')) return;
  setActionBusy('netdiskResetPairing', '', true);
  renderNetdiskPanel();
  try {
    await API.post('/api/netdisk/token/rotate');
    state.netdiskScript = null;
    state.netdiskScript = await API.postJson('/api/netdisk/script', {});
    toast('配对已重置，请替换 JD 连接脚本');
  } catch (e) {
    toast(String(e?.message || e), 'error');
  } finally {
    await loadNetdiskPanel();
    setActionBusy('netdiskResetPairing', '', false);
    renderNetdiskPanel();
  }
}

export async function copyNetdiskScript() {
  const script = String(state.netdiskScript?.script || $('#netdiskScriptText')?.textContent || '').trim();
  if (!script) return;
  const ok = await copyText(script);
  toast(ok ? '脚本已复制' : '复制失败', ok ? 'info' : 'error');
}

// One task command. Every action goes through the same route so a state the
// backend refuses comes back as its own message rather than as a generic error.
export async function runNetdiskJobAction(taskId, action) {
  if (isActionBusy(`netdiskJob:${taskId}:${action}`)) return;
  const item = NETDISK_JOB_ACTIONS.find(entry => entry.action === action);
  if (item?.confirm && typeof window !== 'undefined' && typeof window.confirm === 'function') {
    if (!window.confirm(item.confirm)) return;
  }
  setActionBusy(`netdiskJob:${taskId}:${action}`, '', true);
  renderNetdiskJobs();
  try {
    // Every control action queues a command rather than acting on the engine
    // directly, so the route takes no link selection from the panel: the task
    // already froze the links it was registered with.
    await API.postJson(`/api/netdisk/jobs/${encodeURIComponent(taskId)}/${action}`, {link_ids: []});
    await loadNetdiskPanel();
    renderNetdiskPanel();
  } catch (e) {
    // 409 from the start pre-check carries the reason the move is refused.
    toast(String(e?.message || e), 'error');
  } finally {
    setActionBusy(`netdiskJob:${taskId}:${action}`, '', false);
    renderNetdiskJobs();
  }
}

// The dispatch entries outside this panel (download works rows, artist link
// dialogs) run before the netdisk panel was ever opened, so the connection
// state may not be loaded yet. Read it once on demand; the panel's own timer
// refresh keeps it current afterwards.
async function netdiskDispatchConnection() {
  if (!state.netdiskConnection) {
    try {
      state.netdiskConnection = await API.get('/api/netdisk/connection');
    } catch (e) {
      if (!isAbortError(e)) state.netdiskConnection = null;
      return null;
    }
  }
  return state.netdiskConnection;
}

// Shared pre-check for every netdisk dispatch entry: the backend refuses an
// unpaired bridge or a disabled netdisk download anyway, but the user should
// hear it before submitting rather than through a rejected request.
// Returns true when dispatch may proceed.
export async function ensureNetdiskDispatchReady() {
  const connection = await netdiskDispatchConnection();
  if (!connection?.connected) {
    toast('下载器未连接，请先在网盘面板完成配对', 'warn');
    return false;
  }
  return true;
}

export async function resolveNetdiskDispatchPost() {
  const input = $('#netdiskDispatchInput');
  const preview = $('#netdiskDispatchPreview');
  const submitBtn = $('#netdiskDispatchSubmitBtn');
  if (!input || !preview) return;
  const raw = input.value.trim();
  if (!raw) {
    toast('请输入作品 ID 或链接', 'warn');
    return;
  }
  const idMatch = raw.match(/\/post\/(\d+)/) || raw.match(/^(\d+)$/);
  const postId = idMatch ? Number(idMatch[1]) : null;
  const isLink = /^https?:\/\//i.test(raw);
  // A scheme-prefixed non-http input (file://, ftp://, ...) is neither a post
  // id nor a deliverable link; naming the protocol beats the generic
  // "未在画库中找到该作品" answer the fall-through would give.
  const schemeMatch = raw.match(/^([a-z][a-z0-9+.-]*):\/\//i);
  if (schemeMatch && !isLink) {
    preview.hidden = true;
    toast(`仅支持 http 或 https 链接，不支持 ${schemeMatch[1]}://`, 'warn');
    return;
  }

  state.netdiskResolvedPost = null;
  state.netdiskResolvedLink = null;
  if (submitBtn) submitBtn.disabled = true;

  if (postId) {
    // The list search matches title, source-site post_id, and the internal
    // numeric id (pawchive.rs matches id exactly for a numeric query), so one
    // pass covers every id shape a user can paste.
    try {
      const res = await API.get(`/api/pawchive/posts?search=${encodeURIComponent(String(postId))}&limit=20`);
      const posts = Array.isArray(res?.posts) ? res.posts : [];
      const matched = posts.find(p => Number(p.id) === postId || String(p.post_id) === String(postId));
      if (matched) {
        state.netdiskResolvedPost = matched;
        preview.hidden = false;
        const extCount = matched.external_link_count || (matched.has_external_links ? 1 : 0);
        preview.innerHTML = `<strong>${escHtml(matched.title || matched.post_id || '作品')}</strong>
          <div>画师：${escHtml(matched.artist_name || '未知')} \u00b7 日期：${escHtml(matched.day || '无日期')} \u00b7 外部网盘链接：${extCount} 个</div>`;
        if (submitBtn) submitBtn.disabled = false;
        toast('已解析作品', 'info');
        return;
      }
    } catch (e) {
      // Fall through
    }
  }

  if (/^https?:\/\//i.test(raw)) {
    state.netdiskResolvedLink = raw;
    preview.hidden = false;
    preview.innerHTML = `<strong>直接投递链接</strong><div>${escHtml(raw)}</div>`;
    if (submitBtn) submitBtn.disabled = false;
    toast('链接已就绪', 'info');
    return;
  }

  preview.hidden = true;
  toast('未在画库中找到该作品，且不是有效链接', 'warn');
}

export async function submitNetdiskDispatch() {
  const input = $('#netdiskDispatchInput');
  const preview = $('#netdiskDispatchPreview');
  const submitBtn = $('#netdiskDispatchSubmitBtn');
  const post = state.netdiskResolvedPost;
  const link = state.netdiskResolvedLink || (input?.value.trim().match(/^https?:\/\//i) ? input.value.trim() : null);

  if (!post && !link) {
    toast('请先输入并解析作品或链接', 'warn');
    return;
  }

  if (!(await ensureNetdiskDispatchReady())) return;

  const payload = post ? { post_id: Number(post.id) } : { link };
  if (isActionBusy('netdiskDispatchSubmit')) return;
  setActionBusy('netdiskDispatchSubmit', '', true);
  if (submitBtn) submitBtn.disabled = true;

  async function doSubmit(extra = {}) {
    try {
      const res = await API.postJson('/api/netdisk/jobs', { ...payload, ...extra });
      if (res.auto_start) {
        toast('已投递到 JDownloader，任务将自动开始', 'info');
      } else {
        toast('已投递到 JDownloader 收集器，请在网盘面板或链接收集器中确认开始', 'info');
      }
      state.netdiskResolvedPost = null;
      state.netdiskResolvedLink = null;
      if (input) input.value = '';
      if (preview) {
        preview.hidden = true;
        preview.innerHTML = '';
      }
      await loadNetdiskPanel();
      renderNetdiskPanel();
      return res;
    } catch (err) {
      if (err.status === 409 || err.message?.includes('409') || err.message?.includes('已有活跃投递任务') || err.message?.includes('进行中')) {
        const retry = window.confirm('该作品已有投递任务正在进行中，是否重新投递？');
        if (retry) {
          return doSubmit({ force: true });
        }
        return null;
      }
      toast('投递失败：' + (err.message || err), 'error');
      return null;
    } finally {
      setActionBusy('netdiskDispatchSubmit', '', false);
      if (submitBtn) submitBtn.disabled = false;
    }
  }

  return doSubmit();
}
