import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type MediaItem = {
  id: string;
  url: string;
  kind: string;
  sourceUrl: string;
  capturedAt: string;
};

type DownloadUpdate = {
  id: string;
  filename: string;
  status: string;
  received: number;
  total?: number;
  error?: string;
  unit?: "segments" | "bytes";
  receivedBytes?: number;
  totalBytes?: number;
};

type DownloadTask = DownloadUpdate & { sourceUrl: string };

type AppSettings = {
  browserSource: "managed" | "local";
  listeningMode: "headless" | "visible";
  localChromiumPath: string;
  downloadDir: string;
  llmApiUrl: string;
  llmApiKey: string;
  llmModel: string;
};

type Episode = { title: string; url: string };
type BatchProgress = { stage: string; message: string; current?: number; total?: number };

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <aside class="sidebar">
    <div class="brand"><div class="brand-mark">V</div><div><strong>Video Scout</strong><span>DESKTOP MEDIA TOOL</span></div></div>
    <div class="side-label">工作区</div>
    <button class="nav-item active" data-view="探测器"><span class="nav-icon">⌕</span>视频探测器</button>
    <button class="nav-item" data-view="批量下载"><span class="nav-icon">≡</span>批量下载</button>
    <button class="nav-item" data-view="下载任务"><span class="nav-icon">⇩</span>下载任务<span class="nav-count" id="task-count">0</span></button>
    <button class="nav-item" data-view="设置"><span class="nav-icon">⚙</span>设置</button>
    <div class="sidebar-bottom"><span class="status-dot"></span><span>本机处理 · 隐私优先</span></div>
  </aside>
  <main class="main-shell">
    <header class="topbar"><div><span class="breadcrumb">工作区</span><span class="crumb-sep">/</span><strong id="page-title">视频探测器</strong></div><div class="topbar-right"><span class="platform-chip">跨平台桌面版</span><span class="avatar">VS</span></div></header>
    <section class="workspace">
      <div id="detector-view">
        <div class="hero"><div class="eyebrow"><span class="eyebrow-line"></span>MEDIA DISCOVERY</div><h1>发现网页中的<br><span>视频资源。</span></h1><p>输入网页地址，在内置浏览器中播放视频，自动捕获可下载的媒体流。</p></div>
        <form id="url-form" class="url-form"><span class="url-icon">↗</span><input id="url-input" type="url" placeholder="https://example.com/watch" autocomplete="url" required><button type="submit">打开并探测 <span>→</span></button></form>
        <div class="browser-tip"><span class="tip-icon">i</span>部分网页需要登录或点击播放后才会发出视频请求。</div>
        <div id="detect-banner" class="detect-banner" hidden><span class="detect-spinner"></span><div class="detect-info"><strong>正在探测媒体…</strong><small>无头浏览器加载页面并监听网络请求，捕获结果会实时出现在下方</small></div><div class="detect-bar"><i></i></div></div>
        <section class="media-section"><div class="section-head"><div><div class="section-kicker">捕获结果</div><h2>媒体请求 <span id="media-count">0</span></h2></div><div class="section-tools"><span class="live-indicator"><i></i>实时监听</span><button id="clear-media" class="quiet-button">清空</button></div></div><div id="media-list" class="media-list"></div></section>
      </div>
      <div id="batch-view" hidden>
        <div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>BATCH MODE · LLM</div><h1>批量下载</h1><p>输入剧集列表页地址，由 LLM 分析页面找出全部集数并统一下载。需先在设置中配置 LLM。</p></div>
        <form id="batch-form" class="url-form"><span class="url-icon">≡</span><input id="batch-url-input" type="url" placeholder="https://example.com/drama/123" autocomplete="url" required><button type="submit" id="batch-analyze-btn">分析剧集 <span>→</span></button></form>
        <div id="batch-progress" class="batch-progress" hidden></div>
        <section class="media-section" id="batch-result-section" hidden><div class="section-head"><div><div class="section-kicker">分析结果</div><h2>共 <span id="batch-count">0</span> 集</h2></div><div class="section-tools"><label class="batch-select-all"><input type="checkbox" id="batch-select-all" checked>全选</label><button id="batch-download-btn" class="save-settings">开始下载全部</button></div></div><div id="batch-list" class="batch-list"></div></section>
      </div>
      <div id="tasks-view" hidden><div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>DOWNLOAD CENTER</div><h1>下载任务</h1><p>查看任务状态和下载进度。</p></div><div id="task-list" class="task-list"></div></div>
      <div id="settings-view" hidden>
        <div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>APPLICATION SETTINGS</div><h1>设置</h1><p>选择媒体监听方式与 Chromium 来源。默认由应用下载 Chromium 并使用无头模式。</p></div>
        <form id="settings-form" class="settings-form">
          <label class="setting-row"><span><strong>浏览器来源</strong><small>应用管理会自动下载并缓存 Chromium</small></span><select id="browser-source"><option value="managed">随应用下载并管理</option><option value="local">使用本机 Chromium</option></select></label>
          <label class="setting-row"><span><strong>本机 Chromium 路径</strong><small>选择本机浏览器的可执行文件</small></span><input id="chromium-path" type="text" placeholder="/path/to/chromium" autocomplete="off"></label>
          <label class="setting-row"><span><strong>监听模式</strong><small>无头模式后台监听；可见模式可手动登录、点击播放</small></span><select id="listening-mode"><option value="headless">无头（默认）</option><option value="visible">可见浏览器</option></select></label>
          <label class="setting-row"><span><strong>下载目录</strong><small>留空则使用 ~/Downloads/Video Scout</small></span><input id="download-dir" type="text" placeholder="~/Downloads/Video Scout" autocomplete="off"></label>
          <div class="settings-group-title">LLM 配置（批量下载）</div>
          <label class="setting-row"><span><strong>API 地址</strong><small>OpenAI 兼容接口，如 https://api.deepseek.com/v1</small></span><input id="llm-api-url" type="url" placeholder="https://api.deepseek.com/v1" autocomplete="off"></label>
          <label class="setting-row"><span><strong>API Key</strong><small>仅保存在本机设置文件中</small></span><input id="llm-api-key" type="password" placeholder="sk-..." autocomplete="off"></label>
          <label class="setting-row"><span><strong>模型名称</strong><small>如 deepseek-chat、gpt-4o-mini</small></span><input id="llm-model" type="text" placeholder="deepseek-chat" autocomplete="off"></label>
          <div class="settings-actions"><button type="submit" class="save-settings">保存设置</button><span id="settings-status" role="status"></span></div>
        </form>
      </div>
    </section>
    <footer><span>Video Scout <span class="footer-sep">·</span> 媒体探测与下载</span><span>仅下载你拥有权限保存的内容</span></footer>
  </main>
`;

const mediaList = document.querySelector<HTMLDivElement>("#media-list")!;
const mediaCount = document.querySelector<HTMLSpanElement>("#media-count")!;
const taskList = document.querySelector<HTMLDivElement>("#task-list")!;
const tasks = new Map<string, DownloadTask>();
const media = new Map<string, MediaItem>();
let currentBrowserUrl = "";
let unlistenMedia: UnlistenFn | undefined;
let unlistenProgress: UnlistenFn | undefined;

function currentSettings(): AppSettings {
  return {
    browserSource: document.querySelector<HTMLSelectElement>("#browser-source")!.value as AppSettings["browserSource"],
    listeningMode: document.querySelector<HTMLSelectElement>("#listening-mode")!.value as AppSettings["listeningMode"],
    localChromiumPath: document.querySelector<HTMLInputElement>("#chromium-path")!.value.trim(),
    downloadDir: document.querySelector<HTMLInputElement>("#download-dir")!.value.trim(),
    llmApiUrl: document.querySelector<HTMLInputElement>("#llm-api-url")!.value.trim(),
    llmApiKey: document.querySelector<HTMLInputElement>("#llm-api-key")!.value.trim(),
    llmModel: document.querySelector<HTMLInputElement>("#llm-model")!.value.trim(),
  };
}

function updateBrowserSourceFields() {
  const isLocal = document.querySelector<HTMLSelectElement>("#browser-source")!.value === "local";
  document.querySelector<HTMLInputElement>("#chromium-path")!.disabled = !isLocal;
}

const GENERIC_SEGMENTS = new Set(["index", "video", "videos", "hls", "playlist", "main", "master", "media", "stream", "watch", "play", "vod", "output", "chunklist", "static", "assets", "file", "files", "data", "content", "src", "cdn"]);

function safeFilename(url: string, kind: string): string {
  const ext = kind === "m3u8" ? "ts" : kind;
  try {
    const parsed = new URL(url);
    const segments = decodeURIComponent(parsed.pathname).split("/").filter(Boolean);
    // Drop generic path segments (index, video, hls...) and keep meaningful ones for the name.
    const meaningful = segments
      .map((segment) => segment.replace(/\.[^.]+$/, ""))
      .filter((segment) => segment && !GENERIC_SEGMENTS.has(segment.toLowerCase()));
    if (meaningful.length) {
      const name = meaningful.slice(-2).join("_").replace(/[\\/:*?"<>|]+/g, "_").slice(0, 120);
      return `${name}.${ext}`;
    }
    const fallback = decodeURIComponent(segments.pop() || "video").replace(/[\\/:*?"<>|]+/g, "_").slice(0, 120);
    return `${fallback || "video"}.${ext}`;
  } catch {
    return `video.${ext}`;
  }
}

function prettyBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return "未知大小";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit += 1; }
  return `${size.toFixed(1)} ${units[unit]}`;
}

function isPlaylist(url: string) { return /\.m3u8(?:$|[?#])/i.test(url); }

function renderMedia() {
  mediaCount.textContent = String(media.size);
  mediaList.replaceChildren();
  if (!media.size) {
    mediaList.innerHTML = `<div class="empty-state"><div class="empty-visual"><span>◉</span><i></i><b></b></div><strong>还没有捕获到媒体</strong><p>打开网页并播放视频，捕获到的 MP4、M3U8 等资源会显示在这里。</p></div>`;
    return;
  }
  for (const item of [...media.values()].reverse()) {
    const card = document.createElement("article");
    card.className = "media-card";
    const icon = document.createElement("div");
    icon.className = `format-icon ${item.kind === "m3u8" ? "format-hls" : ""}`;
    icon.innerHTML = item.kind === "m3u8" ? "HLS" : item.kind.toUpperCase();
    const details = document.createElement("div");
    details.className = "media-details";
    const title = document.createElement("strong");
    title.textContent = safeFilename(item.url, item.kind);
    const url = document.createElement("div");
    url.className = "media-url";
    url.textContent = item.url;
    url.title = item.url;
    const meta = document.createElement("div");
    meta.className = "media-meta";
    meta.innerHTML = `<span>${item.kind.toUpperCase()}</span><i></i><span>来自 ${escapeHtml(new URL(item.sourceUrl).hostname)}</span>`;
    details.append(title, url, meta);
    const download = document.createElement("button");
    download.className = "download-button";
    download.innerHTML = isPlaylist(item.url) ? `下载视频 <span>↓</span>` : `下载 <span>↓</span>`;
    download.addEventListener("click", () => startDownload(item, download));
    card.append(icon, details, download);
    mediaList.append(card);
  }
}

function escapeHtml(value: string) {
  return value.replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character]!);
}

async function startDownload(item: MediaItem, button: HTMLButtonElement) {
  const id = crypto.randomUUID();
  const filename = safeFilename(item.url, item.kind);
  const task: DownloadTask = { id, filename, sourceUrl: item.url, status: "queued", received: 0 };
  tasks.set(id, task);
  renderTasks();
  button.disabled = true;
  button.textContent = "已加入任务";
  try {
    await invoke("start_download", { payload: { id, url: item.url, filename, referer: item.sourceUrl } });
  } catch (error) {
    task.status = "failed";
    task.error = String(error);
    renderTasks();
    button.disabled = false;
    button.innerHTML = `重试 <span>↻</span>`;
  }
}

function renderTasks() {
  document.querySelector("#task-count")!.textContent = String([...tasks.values()].filter((task) => task.status === "downloading" || task.status === "queued").length);
  taskList.replaceChildren();
  if (!tasks.size) {
    taskList.innerHTML = `<div class="empty-state task-empty"><div class="empty-visual"><span>⇩</span><i></i><b></b></div><strong>暂无下载任务</strong><p>从探测结果选择视频资源即可添加下载任务。</p></div>`;
    return;
  }
  for (const task of [...tasks.values()].reverse()) {
    const card = document.createElement("article");
    card.className = "task-card";
    const top = document.createElement("div");
    top.className = "task-card-top";
    const name = document.createElement("strong");
    name.textContent = task.filename;
    const state = document.createElement("span");
    state.className = `task-state state-${task.status}`;
    state.textContent = task.status === "complete" ? "已完成" : task.status === "failed" ? "失败" : task.status === "queued" ? "准备中" : "下载中";
    const actions = document.createElement("div");
    actions.className = "task-actions";
    if (task.status === "downloading" || task.status === "queued") {
      const cancelBtn = document.createElement("button");
      cancelBtn.className = "task-btn cancel-btn";
      cancelBtn.textContent = "取消";
      cancelBtn.addEventListener("click", () => { invoke("cancel_download", { id: task.id }); });
      actions.append(cancelBtn);
    }
    if (task.status === "complete" || task.status === "failed") {
      const delBtn = document.createElement("button");
      delBtn.className = "task-btn delete-btn";
      delBtn.textContent = "删除";
      delBtn.addEventListener("click", () => { tasks.delete(task.id); renderTasks(); });
      actions.append(delBtn);
    }
    top.append(name, state, actions);
    const progress = document.createElement("progress");
    progress.max = task.total && task.total > 0 ? task.total : 1;
    progress.value = task.total && task.total > 0 ? Math.min(task.received, task.total) : 0;
    if (task.status === "complete") progress.value = progress.max;
    const bottom = document.createElement("div");
    bottom.className = "task-card-bottom";
    if (task.unit === "segments") {
      const receivedSize = task.receivedBytes !== undefined ? prettyBytes(task.receivedBytes) : "";
      const totalSize = task.totalBytes !== undefined ? prettyBytes(task.totalBytes) : "";
      const size = receivedSize && totalSize ? `${receivedSize} / 约${totalSize}` : receivedSize;
      bottom.textContent = task.status === "complete"
        ? `${task.received} 个分片 · ${receivedSize} · 已保存到下载目录`
        : `${task.received} / ${task.total ?? "?"} 个分片 · ${size}`;
    } else {
      bottom.textContent = task.total ? `${prettyBytes(task.received)} / ${prettyBytes(task.total)}` : task.status === "complete" ? "已保存到下载目录" : prettyBytes(task.received);
    }
    if (task.error) bottom.textContent = task.error;
    card.append(top, progress, bottom);
    taskList.append(card);
  }
}

function formatUrl(input: string): string {
  const value = input.trim();
  const candidate = /^https?:\/\//i.test(value) ? value : `https://${value}`;
  const parsed = new URL(candidate);
  if (!/^https?:$/.test(parsed.protocol)) throw new Error("只支持 HTTP/HTTPS 地址");
  return parsed.toString();
}

document.querySelector<HTMLFormElement>("#url-form")!.addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = document.querySelector<HTMLInputElement>("#url-input")!;
  const submitBtn = document.querySelector<HTMLButtonElement>("#url-form button[type=submit]")!;
  const detectBanner = document.querySelector<HTMLDivElement>("#detect-banner")!;
  try {
    currentBrowserUrl = formatUrl(input.value);
    input.value = currentBrowserUrl;
    submitBtn.disabled = true;
    submitBtn.innerHTML = '探测中… <span class="spin">⟳</span>';
    detectBanner.hidden = false;
    await invoke("open_page", { url: currentBrowserUrl });
  } catch (error) {
    if (currentSettings().listeningMode === "headless") {
      const openVisible = window.confirm(`无头浏览器未能打开页面：${String(error)}\n\n是否改用可见浏览器？`);
      if (openVisible) {
        try {
          await invoke("open_visible_page", { url: currentBrowserUrl });
          const savedSettings = currentSettings();
          savedSettings.listeningMode = "visible";
          await invoke("save_settings", { settings: savedSettings });
          document.querySelector<HTMLSelectElement>("#listening-mode")!.value = "visible";
          return;
        } catch (visibleError) {
          input.setCustomValidity(String(visibleError));
        }
      } else {
        input.setCustomValidity(String(error));
      }
    } else {
      input.setCustomValidity(String(error));
    }
    input.reportValidity();
    input.addEventListener("input", () => input.setCustomValidity(""), { once: true });
  } finally {
    detectBanner.hidden = true;
    submitBtn.disabled = false;
    submitBtn.innerHTML = '打开并探测 <span>→</span>';
  }
});

document.querySelector("#clear-media")!.addEventListener("click", async () => {
  media.clear();
  await invoke("clear_media");
  renderMedia();
});

document.querySelector<HTMLSelectElement>("#browser-source")!.addEventListener("change", updateBrowserSourceFields);
document.querySelector<HTMLFormElement>("#settings-form")!.addEventListener("submit", async (event) => {
  event.preventDefault();
  const status = document.querySelector<HTMLSpanElement>("#settings-status")!;
  try {
    const settings = currentSettings();
    if (settings.browserSource === "local" && !settings.localChromiumPath) throw new Error("请输入本机 Chromium 可执行文件路径");
    await invoke("save_settings", { settings });
    status.textContent = "设置已保存";
    status.className = "settings-success";
  } catch (error) {
    status.textContent = String(error);
    status.className = "settings-error";
  }
});

// ===== 批量下载（LLM 分析） =====

let episodes: Episode[] = [];
let batchBusy = false;

const batchProgressEl = () => document.querySelector<HTMLDivElement>("#batch-progress")!;

function showBatchProgress(text: string, error = false) {
  const element = batchProgressEl();
  element.hidden = false;
  element.classList.toggle("batch-error", error);
  element.textContent = text;
}

function renderEpisodes() {
  const list = document.querySelector<HTMLDivElement>("#batch-list")!;
  document.querySelector<HTMLSpanElement>("#batch-count")!.textContent = String(episodes.length);
  document.querySelector<HTMLDivElement>("#batch-result-section")!.hidden = episodes.length === 0;
  list.innerHTML = episodes
    .map((episode, index) => `<label class="batch-item"><input type="checkbox" data-index="${index}" checked><span class="batch-title">${episode.title.replace(/</g, "&lt;")}</span><span class="batch-url">${episode.url.replace(/</g, "&lt;")}</span></label>`)
    .join("");
}

document.querySelector<HTMLButtonElement>("#batch-select-all")!.addEventListener("change", (event) => {
  const checked = (event.target as HTMLInputElement).checked;
  document.querySelectorAll<HTMLInputElement>("#batch-list input[type=checkbox]").forEach((box) => { box.checked = checked; });
});

document.querySelector<HTMLFormElement>("#batch-form")!.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (batchBusy) return;
  const input = document.querySelector<HTMLInputElement>("#batch-url-input")!;
  const button = document.querySelector<HTMLButtonElement>("#batch-analyze-btn")!;
  const url = formatUrl(input.value);
  if (!url) return;
  batchBusy = true;
  button.disabled = true;
  button.innerHTML = '分析中… <span class="spin">⟳</span>';
  episodes = [];
  renderEpisodes();
  showBatchProgress("正在启动分析…");
  try {
    const result = await invoke<{ episodes: Episode[]; steps: string[] }>("analyze_episodes", { url });
    episodes = result.episodes;
    renderEpisodes();
    showBatchProgress(`分析完成：共 ${result.episodes.length} 集。确认列表后点击"开始下载全部"。`);
  } catch (error) {
    showBatchProgress(`分析失败：${String(error)}`, true);
  } finally {
    batchBusy = false;
    button.disabled = false;
    button.innerHTML = "分析剧集 <span>→</span>";
  }
});

document.querySelector<HTMLButtonElement>("#batch-download-btn")!.addEventListener("click", async () => {
  if (batchBusy || episodes.length === 0) return;
  const selected = [...document.querySelectorAll<HTMLInputElement>("#batch-list input[type=checkbox]:checked")]
    .map((box) => episodes[Number(box.dataset.index)])
    .filter(Boolean);
  if (selected.length === 0) { showBatchProgress("请先选择要下载的集数", true); return; }
  batchBusy = true;
  const button = document.querySelector<HTMLButtonElement>("#batch-download-btn")! as HTMLButtonElement;
  button.disabled = true;
  try {
    await invoke("batch_download", { episodes: selected });
  } catch (error) {
    showBatchProgress(`批量下载启动失败：${String(error)}`, true);
  } finally {
    batchBusy = false;
    button.disabled = false;
  }
});

void listen<BatchProgress>("batch-progress", ({ payload }) => {
  if (payload.stage === "done") {
    showBatchProgress(payload.message);
  } else if (payload.stage === "error") {
    showBatchProgress(payload.message, true);
  } else {
    const counter = payload.total !== undefined && payload.current !== undefined ? `（${payload.current}/${payload.total}）` : "";
    showBatchProgress(`${payload.message}${counter}`);
  }
});

document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((button) => button.addEventListener("click", () => {
  const view = button.dataset.view;
  document.querySelectorAll(".nav-item").forEach((item) => item.classList.toggle("active", item === button));
  document.querySelector<HTMLDivElement>("#detector-view")!.hidden = view !== "探测器";
  document.querySelector<HTMLDivElement>("#batch-view")!.hidden = view !== "批量下载";
  document.querySelector<HTMLDivElement>("#tasks-view")!.hidden = view !== "下载任务";
  document.querySelector<HTMLDivElement>("#settings-view")!.hidden = view !== "设置";
  document.querySelector<HTMLSpanElement>("#page-title")!.textContent = view ?? "视频探测器";
}));

async function initialize() {
  unlistenMedia = await listen<MediaItem>("media-found", ({ payload }) => { media.set(payload.id, payload); renderMedia(); });
  unlistenProgress = await listen<DownloadUpdate>("download-progress", ({ payload }) => {
    const task = tasks.get(payload.id);
    if (!task) return;
    Object.assign(task, payload);
    renderTasks();
  });
  const [existing, settings] = await Promise.all([
    invoke<MediaItem[]>("get_media"),
    invoke<AppSettings>("get_settings"),
  ]);
  existing.forEach((item) => media.set(item.id, item));
  document.querySelector<HTMLSelectElement>("#browser-source")!.value = settings.browserSource;
  document.querySelector<HTMLSelectElement>("#listening-mode")!.value = settings.listeningMode;
  document.querySelector<HTMLInputElement>("#chromium-path")!.value = settings.localChromiumPath;
  document.querySelector<HTMLInputElement>("#download-dir")!.value = settings.downloadDir ?? "";
  document.querySelector<HTMLInputElement>("#llm-api-url")!.value = settings.llmApiUrl ?? "";
  document.querySelector<HTMLInputElement>("#llm-api-key")!.value = settings.llmApiKey ?? "";
  document.querySelector<HTMLInputElement>("#llm-model")!.value = settings.llmModel ?? "";
  updateBrowserSourceFields();
  renderMedia();
  renderTasks();
  window.addEventListener("beforeunload", () => { unlistenMedia?.(); unlistenProgress?.(); });
}

void initialize();
