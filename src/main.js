import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { createIcons, icons } from 'lucide';
import './styles.css';

const MAX_REFS = 9;
const AUTO_REFRESH_MS = 3000;
const TIMING_STORAGE_KEY = 'image-lab.runtime-timings-v1';
const state = {
  app: null,
  prompt: '半身人物设定图，电影灯光，真实服装纹理，背景干净，适合短视频角色一致性参考。',
  size: '1024x1024',
  refs: [],
  selectedHistoryId: null,
  generating: false,
  settingsOpen: false,
  statusText: '准备就绪',
  statusIcon: 'circle-dot',
  statusSpin: false,
  refreshTimer: null,
  refreshInFlight: false,
  refreshQueued: false,
  runtimeTimings: loadRuntimeTimings(),
};

const sizes = ['1024x1024', '1536x1024', '1024x1536', '1792x1024'];
const adapters = [
  ['async_generations', 'async generations'],
  ['openai_edits', 'openai edits'],
  ['sync_generations', 'sync generations'],
];

function fileUrl(path) {
  if (!path) return '';
  try { return convertFileSrc(path); } catch { return path; }
}

function esc(value) {
  return String(value ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

function basename(path) {
  return String(path || '').split(/[\\/]/).filter(Boolean).pop() || 'reference.png';
}

function icon(name, size = 16, cls = '') {
  return `<i data-lucide="${name}" width="${size}" height="${size}" class="${cls}"></i>`;
}

function activeSettings() {
  return state.app?.settings || {
    providers: [{
      id: 'default',
      name: 'GeekAI Proxy',
      baseUrl: 'https://geekai.co/api/v1',
      apiKey: '',
      model: 'gpt-image-2',
      adapter: 'async_generations',
    }],
    activeProviderId: 'default',
  };
}

function activeProvider() {
  const settings = activeSettings();
  return settings.providers.find((p) => p.id === settings.activeProviderId) || settings.providers[0];
}

function loadRuntimeTimings() {
  try {
    const parsed = JSON.parse(localStorage.getItem(TIMING_STORAGE_KEY) || '{}');
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

function saveRuntimeTimings() {
  try {
    localStorage.setItem(TIMING_STORAGE_KEY, JSON.stringify(state.runtimeTimings || {}));
  } catch {
    // best-effort only
  }
}

function parseTimestamp(value) {
  const ts = Date.parse(value);
  return Number.isFinite(ts) ? ts : null;
}

function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '估算中';
  const total = Math.max(0, Math.round(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const rest = total % 60;
  if (hours > 0) return `${hours}小时${String(minutes).padStart(2, '0')}分`;
  if (minutes > 0) return `${minutes}分${String(rest).padStart(2, '0')}秒`;
  return `${rest}秒`;
}

function formatWaited(seconds) {
  return `已等待 ${formatDuration(seconds)}`;
}

function timingRecordFor(item) {
  if (!item?.id) return null;
  return state.runtimeTimings[item.id] || null;
}

function knownDurationSeconds(item) {
  const direct = Number(item?.durationSeconds);
  if (Number.isFinite(direct) && direct >= 0) return Math.round(direct);
  const record = timingRecordFor(item);
  const stored = Number(record?.durationSeconds);
  if (Number.isFinite(stored) && stored >= 0) return Math.round(stored);
  return null;
}

function recentSuccessDurations(history = []) {
  const samples = [];
  for (const item of history) {
    if (item?.status !== 'completed') continue;
    const duration = knownDurationSeconds(item);
    if (!Number.isFinite(duration)) continue;
    samples.push(duration);
    if (samples.length >= 5) break;
  }
  return samples;
}

function averageDuration(history = []) {
  const samples = recentSuccessDurations(history);
  if (!samples.length) return null;
  return samples.reduce((sum, value) => sum + value, 0) / samples.length;
}

function timingSummary(item, history = []) {
  if (!item) return { kind: 'idle', primary: '', secondary: '', note: '' };
  const duration = knownDurationSeconds(item);
  if (item.status === 'pending') {
    const createdAtMs = parseTimestamp(item.createdAt);
    const waitedSeconds = createdAtMs ? Math.max(0, (Date.now() - createdAtMs) / 1000) : null;
    const averageSeconds = averageDuration(history);
    if (!Number.isFinite(averageSeconds)) {
      return {
        kind: 'pending',
        primary: waitedSeconds == null ? '等待中' : formatWaited(waitedSeconds),
        secondary: '估算中',
        note: 'pending',
      };
    }
    const estimatedTotal = averageSeconds;
    const remaining = Math.max(0, estimatedTotal - (waitedSeconds || 0));
    return {
      kind: 'pending',
      primary: waitedSeconds == null ? '等待中' : formatWaited(waitedSeconds),
      secondary: `预计总耗时 ${formatDuration(estimatedTotal)} · 预计剩余 ${formatDuration(remaining)}`,
      note: 'ETA',
    };
  }
  if (duration != null) {
    return {
      kind: item.status === 'failed' ? 'failed' : 'completed',
      primary: `耗时 ${formatDuration(duration)}`,
      secondary: item.status === 'failed' ? '失败任务已结束' : '任务已完成',
      note: 'durationSeconds',
    };
  }
  return {
    kind: item.status === 'failed' ? 'failed' : 'completed',
    primary: '耗时待同步',
    secondary: item.status === 'failed' ? '失败任务已结束' : '任务已完成',
    note: 'durationSeconds',
  };
}

function syncRuntimeTimings(history = []) {
  let dirty = false;
  const now = Date.now();
  for (const item of history) {
    if (!item?.id) continue;
    const createdAtMs = parseTimestamp(item.createdAt);
    const existing = state.runtimeTimings[item.id] || {};
    const next = { ...existing };
    if (item.status === 'pending') {
      if (!Number.isFinite(Number(next.startedAtMs))) {
        next.startedAtMs = createdAtMs || now;
        dirty = true;
      }
      next.lastSeenStatus = 'pending';
    } else {
      const shouldCaptureDuration = Number.isFinite(Number(next.startedAtMs)) && !Number.isFinite(Number(next.durationSeconds));
      if (shouldCaptureDuration) {
        next.completedAtMs = now;
        next.durationSeconds = Math.max(0, Math.round((now - Number(next.startedAtMs)) / 1000));
        dirty = true;
      } else if (!Number.isFinite(Number(next.durationSeconds)) && Number.isFinite(Number(item.durationSeconds))) {
        next.durationSeconds = Math.max(0, Math.round(Number(item.durationSeconds)));
        dirty = true;
      }
      next.lastSeenStatus = item.status;
    }
    state.runtimeTimings[item.id] = next;
  }
  if (dirty) saveRuntimeTimings();
}

function captureFocusState() {
  const active = document.activeElement;
  if (!active) return null;
  const snapshot = {
    id: active.id || null,
    scrollTop: active.scrollTop ?? null,
    selectionStart: null,
    selectionEnd: null,
    value: null,
  };
  if (typeof active.selectionStart === 'number') snapshot.selectionStart = active.selectionStart;
  if (typeof active.selectionEnd === 'number') snapshot.selectionEnd = active.selectionEnd;
  if ('value' in active) snapshot.value = active.value;
  return snapshot;
}

function restoreFocusState(snapshot) {
  if (!snapshot?.id) return;
  const el = document.getElementById(snapshot.id);
  if (!el) return;
  if (snapshot.value != null && 'value' in el) {
    try { el.value = snapshot.value; } catch {}
  }
  if (typeof el.focus === 'function') el.focus({ preventScroll: true });
  if (typeof snapshot.selectionStart === 'number' && typeof snapshot.selectionEnd === 'number' && typeof el.setSelectionRange === 'function') {
    try { el.setSelectionRange(snapshot.selectionStart, snapshot.selectionEnd); } catch {}
  }
  if (snapshot.scrollTop != null && 'scrollTop' in el) {
    try { el.scrollTop = snapshot.scrollTop; } catch {}
  }
}

function ensureAutoRefresh() {
  const history = state.app?.history || [];
  const shouldPoll = history.some((item) => item?.status === 'pending');
  if (shouldPoll && !state.refreshTimer) {
    state.refreshTimer = window.setInterval(() => {
      refreshState({ silent: true, preserveFocus: true });
    }, AUTO_REFRESH_MS);
  } else if (!shouldPoll && state.refreshTimer) {
    window.clearInterval(state.refreshTimer);
    state.refreshTimer = null;
  }
}

async function refreshState({ silent = false, preserveFocus = false } = {}) {
  if (state.refreshInFlight) {
    state.refreshQueued = true;
    return;
  }
  const focusSnapshot = preserveFocus ? captureFocusState() : null;
  state.refreshInFlight = true;
  try {
    state.app = await invoke('get_app_state');
    syncRuntimeTimings(state.app?.history || []);
    if (!silent) setStatus('状态已刷新', 'circle-dot');
  } catch (err) {
    if (!silent) {
      state.app = { settings: activeSettings(), history: [] };
      setStatus(`加载状态失败：${String(err)}`, 'alert-circle');
    }
  } finally {
    state.refreshInFlight = false;
    render({ preserveFocus: focusSnapshot });
    if (state.refreshQueued) {
      state.refreshQueued = false;
      queueMicrotask(() => refreshState({ silent: true, preserveFocus: true }));
    }
  }
}

async function loadState(options = {}) {
  await refreshState(options);
}

function setStatus(text, iconName = 'circle-dot', spinning = false) {
  state.statusText = text;
  state.statusIcon = iconName;
  state.statusSpin = spinning;
  const el = document.querySelector('#statusLine');
  if (!el) return;
  el.innerHTML = `${icon(iconName, 14, spinning ? 'spin' : '')}<span>${esc(text)}</span>`;
  createIcons({ icons });
}

function refsFromHistory(item) {
  return (item?.referencePaths || []).slice(0, MAX_REFS).map((path, index) => ({
    id: `history-ref-${item.id}-${index}`,
    name: basename(path),
    mime: 'image/*',
    sizeBytes: 0,
    storedPath: path,
    createdAt: item.createdAt || '',
  }));
}

function refillFromHistory(item) {
  if (!item) return;
  state.selectedHistoryId = item.id;
  state.prompt = item.prompt || '';
  state.size = sizes.includes(item.size) ? item.size : state.size;
  state.refs = refsFromHistory(item);
  setStatus(`已回填历史：${item.referencePaths?.length || 0} 张参考图`, 'history');
  render();
}

function startWindowDrag(event) {
  if (event.button !== 0) return;
  const topbar = event.target.closest('.topbar');
  if (!topbar) return;
  if (event.target.closest('button,input,select,textarea,a,[role="button"],.top-actions')) return;
  event.preventDefault();
  getCurrentWindow().startDragging().catch((err) => {
    console.warn('[image-lab] window drag failed', err);
  });
}

function render({ preserveFocus = null } = {}) {
  const provider = activeProvider();
  const history = state.app?.history || [];
  const selected = history.find((item) => item.id === state.selectedHistoryId) || history[0] || null;
  const inferredMode = state.refs.length > 0 ? '图生图' : '文生图';

  syncRuntimeTimings(history);
  const selectedTiming = timingSummary(selected, history);
  document.querySelector('#app').innerHTML = `
    <div class="app">
      <header class="topbar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <div class="brand-mark" data-tauri-drag-region>像</div>
          <div class="brand-title" data-tauri-drag-region><strong data-tauri-drag-region>像境工坊</strong><span data-tauri-drag-region>local image studio</span></div>
        </div>
        <div class="provider-strip" data-tauri-drag-region>
          <span class="pill" data-tauri-drag-region>${icon('plug', 14)} Provider <strong data-tauri-drag-region>${esc(provider?.name || '-')}</strong></span>
          <span class="pill" data-tauri-drag-region>${icon('box', 14)} Model <strong data-tauri-drag-region>${esc(provider?.model || '-')}</strong></span>
          <span class="pill" data-tauri-drag-region>${icon('route', 14)} Adapter <strong data-tauri-drag-region>${esc(provider?.adapter || '-')}</strong></span>
        </div>
        <div class="top-actions">
          <button class="icon-button" data-action="open-settings" title="Provider 设置">${icon('settings', 18)}</button>
          <button class="icon-button" data-action="refresh" title="刷新">${icon('refresh-cw', 18)}</button>
        </div>
      </header>

      <main class="workspace">
        <aside class="left">
          <section class="section">
            <div class="section-head"><span>Prompt</span><span>${state.prompt.length} / 8000</span></div>
            <div class="section-body">
              <div class="field prompt-field">
                <textarea id="prompt" maxlength="8000" placeholder="写你想生成的画面。参考图在下面直接添加。">${esc(state.prompt)}</textarea>
              </div>
              <div class="compact-controls">
                <div class="field size-field">
                  <label>尺寸</label>
                  <select id="size">${sizes.map((s) => `<option ${s === state.size ? 'selected' : ''}>${s}</option>`).join('')}</select>
                </div>
                <span class="mode-chip">${inferredMode}</span>
              </div>
              <div class="control-row">
                <button class="primary" data-action="generate" ${state.generating ? 'disabled' : ''}>${icon(state.generating ? 'loader-2' : 'sparkles', 17, state.generating ? 'spin' : '')}生成</button>
                <button data-action="clear-refs">${icon('trash-2', 16)}清空参考图</button>
              </div>
              <div class="status-line" id="statusLine">${icon(state.statusIcon, 14, state.statusSpin ? 'spin' : '')}<span>${esc(state.statusText)}</span></div>
            </div>
          </section>

          <section class="section">
            <div class="section-head"><span>References</span><span>${state.refs.length} / ${MAX_REFS}</span></div>
            <div class="section-body">
              <div class="dropzone" id="dropzone">
                <div><strong>拖入图片，或点击选择</strong><span>支持多图。也可以直接粘贴图片。</span></div>
                <input id="fileInput" type="file" accept="image/*" multiple hidden>
              </div>
              <div class="ref-grid">
                ${state.refs.map((ref) => `
                  <div class="ref">
                    <img src="${fileUrl(ref.storedPath)}" alt="${esc(ref.name)}">
                    <button data-remove-ref="${esc(ref.id)}" title="移除参考图">${icon('x', 14)}</button>
                  </div>
                `).join('')}
              </div>
            </div>
          </section>
        </aside>

        <section class="stage">
          <div class="canvas">
            ${renderPreview(selected, history)}
          </div>
          <div class="stage-actions">
            <div class="meta-grid">
              <span>${esc(selected?.status || 'idle')}</span>
              <span>${esc(selected?.size || state.size)}</span>
              <span>refs ${selected?.referencePaths?.length ?? state.refs.length}</span>
              <span>${esc(selectedTiming.secondary ? `${selectedTiming.primary} · ${selectedTiming.secondary}` : selectedTiming.primary)}</span>
            </div>
            <div class="control-row">
              <button data-action="copy-prompt">${icon('copy', 16)}复制提示词</button>
              <button data-action="download" ${selected?.status === 'completed' ? '' : 'disabled'}>${icon('download', 16)}下载</button>
              <button data-action="query" ${selected?.status === 'pending' ? '' : 'disabled'}>${icon('refresh-cw', 16)}查询</button>
            </div>
          </div>
        </section>

        <aside class="right">
          <section class="section">
            <div class="section-head">
              <span>History</span>
              <button class="icon-button" data-action="clear-history" title="清空历史">${icon('archive-x', 16)}</button>
            </div>
            <div class="section-body">
              <div class="history-list">
                ${history.length ? history.map((item) => renderHistoryItem(item, selected?.id, history)).join('') : '<div class="empty-small">暂无生成记录</div>'}
              </div>
            </div>
          </section>
        </aside>
      </main>
      ${state.settingsOpen ? renderSettings(provider) : ''}
    </div>
  `;
  bindEvents();
  createIcons({ icons });
  ensureAutoRefresh();
  restoreFocusState(preserveFocus);
}

function renderPreview(item, history = []) {
  const timing = timingSummary(item, history);
  if (!item) {
    return `<div class="placeholder">${icon('image', 42)}<strong>等待生成</strong><span>文生图或图生图都会在这里预览</span></div>`;
  }
  if (item.status === 'pending') {
    return `<div class="placeholder">${icon('loader-2', 42, 'spin')}<strong>生成中</strong><span>${esc(timing.primary)} · ${esc(timing.secondary)}</span></div>`;
  }
  if (item.status === 'failed') {
    return `<div class="placeholder failed">${icon('alert-circle', 42)}<strong>生成失败</strong><span>${esc(item.error || 'unknown error')}</span><span>${esc(timing.primary)}</span></div>`;
  }
  return `
    <div class="result-shell">
      <img class="result-img" src="${fileUrl(item.storedPath)}" alt="${esc(item.prompt)}">
      <div class="result-overlay">
        <div class="result-overlay-copy">
          <span>${esc(item.prompt).slice(0, 42)}</span>
          <strong>${esc(timing.primary)}</strong>
        </div>
        <strong>${esc(item.size)} / ${item.referencePaths?.length || 0} refs</strong>
      </div>
    </div>
  `;
}

function renderHistoryItem(item, activeId, history = []) {
  const thumb = item.status === 'completed'
    ? `<img src="${fileUrl(item.storedPath)}" alt="">`
    : `<span>${item.status === 'pending' ? icon('loader-2', 18, 'spin') : icon('alert-circle', 18)}</span>`;
  const timing = timingSummary(item, history);
  return `
    <div class="history-item ${activeId === item.id ? 'active' : ''}" data-history="${esc(item.id)}">
      <div class="thumb">${thumb}</div>
      <div class="history-meta">
        <strong>${esc(item.prompt || '无标题生成')}</strong>
        <span>${esc(item.size)} · ${(item.referencePaths || []).length} refs · ${esc(item.providerName || '-')}</span>
        <span>${esc(item.status === 'pending' ? `${timing.primary}${timing.secondary ? ` · ${timing.secondary}` : ''}` : timing.primary)}</span>
      </div>
    </div>
  `;
}

function renderSettings(provider) {
  return `
    <div class="settings open">
      <div class="dialog" role="dialog" aria-modal="true">
        <div class="dialog-head">
          <strong>Provider 设置</strong>
          <button class="icon-button" data-action="close-settings" title="关闭">${icon('x', 18)}</button>
        </div>
        <div class="dialog-body">
          <div class="provider-row">
            <div class="two-col">
              <div class="field"><label>名称</label><input id="providerName" value="${esc(provider?.name || '')}"></div>
              <div class="field"><label>模型</label><input id="modelName" value="${esc(provider?.model || '')}"></div>
            </div>
            <div class="field"><label>Base URL</label><input id="baseUrl" value="${esc(provider?.baseUrl || '')}"></div>
            <div class="two-col">
              <div class="field">
                <label>Adapter</label>
                <select id="adapter">${adapters.map(([value, label]) => `<option value="${value}" ${provider?.adapter === value ? 'selected' : ''}>${label}</option>`).join('')}</select>
              </div>
              <div class="field"><label>API Key</label><input id="apiKey" type="password" value="${esc(provider?.apiKey || '')}"></div>
            </div>
            <p class="hint">adapter 用来处理官方 OpenAI 与兼容代理的图片字段差异。API Key 仅保存在本机 app data。</p>
            <div class="control-row">
              <button class="primary" data-action="save-settings">${icon('save', 16)}保存</button>
            </div>
          </div>
        </div>
      </div>
    </div>
  `;
}

function bindEvents() {
  document.querySelector('.topbar')?.addEventListener('mousedown', startWindowDrag);
  document.querySelector('#prompt')?.addEventListener('input', (event) => {
    state.prompt = event.target.value;
    document.querySelector('.section-head span:last-child').textContent = `${state.prompt.length} / 8000`;
  });
  document.querySelector('#size')?.addEventListener('change', (event) => { state.size = event.target.value; render(); });
  document.querySelector('#dropzone')?.addEventListener('click', () => document.querySelector('#fileInput')?.click());
  document.querySelector('#fileInput')?.addEventListener('change', (event) => saveFiles(event.target.files));
  document.querySelector('#dropzone')?.addEventListener('dragover', (event) => {
    event.preventDefault();
    event.currentTarget.classList.add('dragging');
  });
  document.querySelector('#dropzone')?.addEventListener('dragleave', (event) => event.currentTarget.classList.remove('dragging'));
  document.querySelector('#dropzone')?.addEventListener('drop', (event) => {
    event.preventDefault();
    event.currentTarget.classList.remove('dragging');
    saveFiles(event.dataTransfer.files);
  });
}

document.addEventListener('paste', (event) => {
  const files = Array.from(event.clipboardData?.files || []);
  if (files.some((file) => file.type.startsWith('image/'))) saveFiles(files);
});

document.addEventListener('click', async (event) => {
  const actionEl = event.target.closest('[data-action]');
  const removeEl = event.target.closest('[data-remove-ref]');
  const historyEl = event.target.closest('[data-history]');
  if (removeEl) {
    state.refs = state.refs.filter((ref) => ref.id !== removeEl.dataset.removeRef);
    render();
    return;
  }
  if (historyEl) {
    const item = (state.app?.history || []).find((entry) => entry.id === historyEl.dataset.history);
    refillFromHistory(item);
    return;
  }
  if (!actionEl) return;
  const action = actionEl.dataset.action;
  if (action === 'open-settings') { state.settingsOpen = true; render(); }
  if (action === 'close-settings') { state.settingsOpen = false; render(); }
  if (action === 'refresh') await loadState();
  if (action === 'clear-refs') { state.refs = []; render(); }
  if (action === 'generate') await generate();
  if (action === 'query') await querySelected();
  if (action === 'download') await downloadSelected();
  if (action === 'copy-prompt') await navigator.clipboard.writeText(state.prompt);
  if (action === 'clear-history') await clearHistory();
  if (action === 'save-settings') await saveSettings();
});

async function saveFiles(fileList) {
  const files = Array.from(fileList || []).filter((file) => file.type.startsWith('image/'));
  for (const file of files) {
    if (state.refs.length >= MAX_REFS) break;
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const ref = await invoke('save_reference_image', {
      input: { fileName: file.name || 'reference.png', mime: file.type || 'image/png', bytes },
    });
    state.refs.push(ref);
  }
  render();
}

async function generate() {
  if (!state.prompt.trim()) { setStatus('请先输入提示词', 'alert-circle'); return; }
  state.generating = true;
  render();
  setStatus('提交生成请求', 'loader-2', true);
  try {
    console.info('[image-lab] generate submit', {
      size: state.size,
      refs: state.refs.length,
      adapter: activeProvider()?.adapter,
    });
    const item = await invoke('generate_image', {
      prompt: state.prompt.trim(),
      size: state.size,
      referencePaths: state.refs.map((ref) => ref.storedPath),
    });
    state.selectedHistoryId = item.id;
    await loadState();
    setStatus(item.status === 'pending' ? '已提交，后台生成中' : '生成完成', item.status === 'pending' ? 'loader-2' : 'check-circle', item.status === 'pending');
  } catch (err) {
    console.error('[image-lab] generate failed', err);
    setStatus(String(err), 'alert-circle');
  } finally {
    state.generating = false;
    render();
  }
}

async function querySelected() {
  const id = state.selectedHistoryId || state.app?.history?.[0]?.id;
  if (!id) return;
  try {
    await invoke('query_image_task', { historyId: id });
    await loadState();
  } catch (err) {
    setStatus(String(err), 'alert-circle');
  }
}

async function downloadSelected() {
  const id = state.selectedHistoryId || state.app?.history?.[0]?.id;
  if (!id) return;
  try {
    const path = await invoke('download_result', { historyId: id });
    setStatus(`已保存：${path}`, 'download');
  } catch (err) {
    setStatus(String(err), 'alert-circle');
  }
}

async function clearHistory() {
  await invoke('clear_history');
  state.selectedHistoryId = null;
  await loadState();
}

async function saveSettings() {
  const current = activeSettings();
  const provider = activeProvider();
  const nextProvider = {
    ...(provider || { id: 'default' }),
    name: document.querySelector('#providerName').value.trim() || 'Provider',
    model: document.querySelector('#modelName').value.trim() || 'gpt-image-2',
    baseUrl: document.querySelector('#baseUrl').value.trim(),
    apiKey: document.querySelector('#apiKey').value.trim(),
    adapter: document.querySelector('#adapter').value,
  };
  const next = {
    ...current,
    providers: [nextProvider],
    activeProviderId: nextProvider.id,
  };
  state.app = await invoke('update_settings', { settings: next });
  state.settingsOpen = false;
  render();
}

loadState();
