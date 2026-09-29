import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type MediaItem = {
  id: string;
  url: string;
  kind: string;
  sourceUrl: string;
  capturedAt: string;
  pageTitle?: string;
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

type TaskRecord = {
  id: string;
  url: string;
  filename: string;
  referer?: string;
  subdir?: string;
  status: string;
  received: number;
  total?: number;
  unit?: "segments" | "bytes";
  receivedBytes?: number;
  totalBytes?: number;
  error?: string;
  createdAt: string;
};

type AppSettings = {
  browserSource: "managed" | "local";
  localChromiumPath: string;
  downloadDir: string;
  llmApiUrl: string;
  llmApiKey: string;
  llmModel: string;
  maxConcurrent: number;
  lanEnabled: boolean;
  lanPort: number;
};

type LanInfo = { running: boolean; port: number; urls: string[] };
type HistoryEntry = { id: string; url: string; kind: string; createdAt: string };

type Episode = { title: string; url: string; show?: string };
type BatchProgress = { stage: string; message: string; current?: number; total?: number };

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <aside class="sidebar">
    <div class="brand"><div class="brand-mark">V</div><div><strong>Video Scout</strong><span>DESKTOP MEDIA TOOL</span></div></div>
    <div class="side-label">工作区</div>
    <button class="nav-item active" data-view="探测器"><span class="nav-icon">⌕</span>视频探测器</button>
    <button class="nav-item" data-view="批量下载"><span class="nav-icon">≡</span>批量下载</button>
    <button class="nav-item" data-view="下载任务"><span class="nav-icon">⇩</span>下载任务<span class="nav-count" id="task-count">0</span></button>
    <button class="nav-item" data-view="局域网"><span class="nav-icon">⇄</span>局域网下载</button>
    <button class="nav-item" data-view="设置"><span class="nav-icon">⚙</span>设置</button>
    <div class="sidebar-bottom"><span class="status-dot"></span><span>本机处理 · 隐私优先</span></div>
  </aside>
  <main class="main-shell">
    <header class="topbar"><div><span class="breadcrumb">工作区</span><span class="crumb-sep">/</span><strong id="page-title">视频探测器</strong></div><div class="topbar-right"><button type="button" id="history-btn" title="查看处理过的网址历史">◷ 历史</button><span class="platform-chip">跨平台桌面版</span><span class="avatar">VS</span></div></header>
    <section class="workspace">
      <div id="detector-view">
        <div class="hero"><div class="eyebrow"><span class="eyebrow-line"></span>MEDIA DISCOVERY</div><h1>发现网页中的<br><span>视频资源。</span></h1><p>输入网页地址，在内置浏览器中播放视频，自动捕获可下载的媒体流。</p></div>
        <form id="url-form" class="url-form"><span class="url-icon">↗</span><input id="url-input" type="url" placeholder="https://example.com/watch" autocomplete="url" required><label class="visible-check" title="用原生可视窗口打开页面，可手动过 Cloudflare 验证或点击播放"><input type="checkbox" id="detect-visible">可视窗口</label><button type="submit">打开并探测 <span>→</span></button></form>
        <div class="browser-tip"><span class="tip-icon">i</span>部分网页需要登录或点击播放后才会发出视频请求。</div>
        <div id="detect-banner" class="detect-banner" hidden><span class="detect-spinner"></span><div class="detect-info"><strong>正在探测媒体…</strong><small>无头浏览器加载页面并监听网络请求，捕获结果会实时出现在下方</small></div><div class="detect-bar"><i></i></div></div>
        <section class="media-section"><div class="section-head"><div><div class="section-kicker">捕获结果</div><h2>媒体请求 <span id="media-count">0</span></h2></div><div class="section-tools"><span class="live-indicator"><i></i>实时监听</span><button id="clear-media" class="quiet-button">清空</button></div></div><div id="media-list" class="media-list"></div></section>
      </div>
      <div id="batch-view" hidden>
        <div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>BATCH MODE · LLM</div><h1>批量下载</h1><p>输入剧集列表页地址，由 LLM 分析页面找出全部集数并统一下载。需先在设置中配置 LLM。</p></div>
        <form id="batch-form" class="url-form">
          <span class="url-icon">≡</span>
          <input id="batch-url-input" type="url" placeholder="https://example.com/drama/123" autocomplete="url" required>
          <label class="visible-check" title="用原生可视窗口打开页面，遇到 Cloudflare 验证可手动点击，加载后自动提取剧集"><input type="checkbox" id="batch-visible">可视窗口</label>
          <button type="submit" id="batch-analyze-btn">开始分析 <span>→</span></button>
        </form>
        <div id="batch-progress" class="batch-progress" hidden></div>
        <section class="media-section" id="batch-result-section" hidden><div class="section-head"><div><div class="section-kicker">分析结果</div><h2>共 <span id="batch-count">0</span> 集</h2></div><div class="section-tools"><label class="batch-select-all"><input type="checkbox" id="batch-select-all" checked>全选</label><button id="batch-download-btn" class="save-settings">开始下载全部</button></div></div><div class="batch-subdir-row"><label class="batch-select-all"><input type="checkbox" id="batch-use-subdir">保存到子目录</label><input id="batch-subdir-name" type="text" placeholder="子目录名称（默认为作品名）" autocomplete="off"></div><div id="batch-list" class="batch-list"></div></section>
      </div>
      <div id="tasks-view" hidden><div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>DOWNLOAD CENTER</div><h1>下载任务</h1><p>查看任务状态和下载进度。</p></div><div class="task-toolbar" id="task-toolbar" hidden><label class="batch-select-all"><input type="checkbox" id="task-select-all">全选</label><button id="task-batch-retry" class="task-btn">继续/重试选中</button><button id="task-batch-delete" class="task-btn delete-btn">删除选中</button><button id="task-clear-complete" class="task-btn">清除已完成</button></div><div id="task-list" class="task-list"></div></div>
      <div id="lan-view" hidden>
        <div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>LAN SHARE</div><h1>局域网下载</h1><p>启动 HTTP 服务后，同一 Wi-Fi 下的手机/平板可用浏览器访问，提交播放页或直链地址并指定保存子目录，由本机下载。</p></div>
        <div class="lan-status" id="lan-status"><span class="status-dot lan-dot" id="lan-dot"></span><span id="lan-status-text">服务未开启</span></div>
        <div class="lan-urls" id="lan-urls" hidden></div>
        <form id="lan-form" class="settings-form">
          <label class="setting-row"><span><strong>启用局域网下载服务</strong><small>局域网内任何设备都能访问，请勿在不可信网络中开启</small></span><input type="checkbox" id="lan-enabled"></label>
          <label class="setting-row"><span><strong>服务端口</strong><small>修改后保存会自动重启服务</small></span><input id="lan-port" type="number" min="1024" max="65535" step="1" value="8688"></label>
          <div class="settings-actions"><button type="submit" class="save-settings">保存并应用</button><span id="lan-save-result"></span></div>
        </form>
      </div>
      <div id="settings-view" hidden>
        <div class="task-heading"><div class="eyebrow"><span class="eyebrow-line"></span>APPLICATION SETTINGS</div><h1>设置</h1><p>选择媒体监听方式与 Chromium 来源。默认由应用下载 Chromium 并使用无头模式。</p></div>
        <form id="settings-form" class="settings-form">
          <label class="setting-row"><span><strong>浏览器来源</strong><small>应用管理会自动下载并缓存 Chromium</small></span><select id="browser-source"><option value="managed">随应用下载并管理</option><option value="local">使用本机 Chromium</option></select></label>
          <label class="setting-row"><span><strong>本机 Chromium 路径</strong><small>选择本机浏览器的可执行文件</small></span><input id="chromium-path" type="text" placeholder="/path/to/chromium" autocomplete="off"></label>
          <label class="setting-row"><span><strong>下载目录</strong><small>留空则使用 ~/Downloads/Video Scout</small></span><input id="download-dir" type="text" placeholder="~/Downloads/Video Scout" autocomplete="off"></label>
          <label class="setting-row"><span><strong>并发下载数</strong><small>同时进行的下载任务数量，超出的任务排队等待</small></span><input id="max-concurrent" type="number" min="1" max="16" step="1" value="3"></label>
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
  <div class="drawer-overlay" id="history-overlay"></div>
  <aside class="history-drawer" id="history-drawer" aria-hidden="true">
    <div class="drawer-head"><div><div class="section-kicker">HISTORY</div><h2 id="history-title">探测历史</h2></div><button id="history-close" class="quiet-button" title="关闭">✕</button></div>
    <div class="drawer-tools"><span id="history-count">0 条记录</span><button id="history-clear" class="task-btn delete-btn">清空全部</button></div>
    <div id="history-list" class="history-list"></div>
  </aside>
`;

const mediaList = document.querySelector<HTMLDivElement>("#media-list")!;
const mediaCount = document.querySelector<HTMLSpanElement>("#media-count")!;
const taskList = document.querySelector<HTMLDivElement>("#task-list")!;
const tasks = new Map<string, DownloadTask>();
const selectedTasks = new Set<string>();
const media = new Map<string, MediaItem>();
let currentBrowserUrl = "";
let unlistenMedia: UnlistenFn | undefined;
let unlistenProgress: UnlistenFn | undefined;

function currentSettings(): AppSettings {
  return {
    browserSource: document.querySelector<HTMLSelectElement>("#browser-source")!.value as AppSettings["browserSource"],
    localChromiumPath: document.querySelector<HTMLInputElement>("#chromium-path")!.value.trim(),
    downloadDir: document.querySelector<HTMLInputElement>("#download-dir")!.value.trim(),
    llmApiUrl: document.querySelector<HTMLInputElement>("#llm-api-url")!.value.trim(),
    llmApiKey: document.querySelector<HTMLInputElement>("#llm-api-key")!.value.trim(),
    llmModel: document.querySelector<HTMLInputElement>("#llm-model")!.value.trim(),
    maxConcurrent: Math.max(1, Math.min(16, Number(document.querySelector<HTMLInputElement>("#max-concurrent")!.value) || 3)),
    lanEnabled: document.querySelector<HTMLInputElement>("#lan-enabled")!.checked,
    lanPort: Math.max(1024, Math.min(65535, Number(document.querySelector<HTMLInputElement>("#lan-port")!.value) || 8688)),
  };
}

function updateBrowserSourceFields() {
  const isLocal = document.querySelector<HTMLSelectElement>("#browser-source")!.value === "local";
  document.querySelector<HTMLInputElement>("#chromium-path")!.disabled = !isLocal;
}

const GENERIC_SEGMENTS = new Set(["index", "video", "videos", "hls", "playlist", "main", "master", "media", "stream", "watch", "play", "vod", "output", "chunklist", "static", "assets", "file", "files", "data", "content", "src", "cdn"]);

function safeFilename(url: string, kind: string, sourceUrl?: string, pageTitle?: string): string {
  const ext = kind === "m3u8" ? "ts" : kind;
  // Prefer the page title extracted from HTML — it's the most meaningful name.
  if (pageTitle && pageTitle.length > 1 && pageTitle.length < 120) {
    const cleaned = pageTitle.replace(/[\\/:*?"<>|]+/g, "_").trim().replace(/^\.+|\.+$/, "");
    if (cleaned.length > 1) return `${cleaned}.${ext}`;
  }
  // Try to extract a meaningful name from the source (page) URL
  if (sourceUrl) {
    try {
      const sourceParsed = new URL(sourceUrl);
      const sourceSegments = decodeURIComponent(sourceParsed.pathname).split("/").filter(Boolean);
      const sourceMeaningful = sourceSegments
        .map((segment) => segment.replace(/\.[^.]+$/, ""))
        .filter((segment) => segment && !GENERIC_SEGMENTS.has(segment.toLowerCase()));
      // For player pages like /player/321-1-221.html, extract the last meaningful segment
      const lastMeaningful = sourceMeaningful[sourceMeaningful.length - 1];
      if (lastMeaningful && lastMeaningful.length > 2 && lastMeaningful.length < 100) {
        return `${lastMeaningful.replace(/[\\/:*?"<>|]+/g, "_").slice(0, 120)}.${ext}`;
      }
    } catch {}
  }
  // Fall back to extracting from the media URL
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
    title.textContent = safeFilename(item.url, item.kind, item.sourceUrl, item.pageTitle);
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
  const filename = safeFilename(item.url, item.kind, item.sourceUrl, item.pageTitle);
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

async function retryTask(task: DownloadTask) {
  task.status = "downloading";
  task.error = undefined;
  renderTasks();
  try {
    await invoke("retry_download", { id: task.id });
  } catch (error) {
    task.status = "failed";
    task.error = String(error);
    renderTasks();
  }
}

document.querySelector<HTMLInputElement>("#task-select-all")!.addEventListener("change", (event) => {
  const checked = (event.target as HTMLInputElement).checked;
  selectedTasks.clear();
  if (checked) for (const id of tasks.keys()) selectedTasks.add(id);
  renderTasks();
});

document.querySelector<HTMLButtonElement>("#task-batch-retry")!.addEventListener("click", async () => {
  const retryable = [...selectedTasks].map((id) => tasks.get(id)).filter((task): task is DownloadTask =>
    !!task && (task.status === "failed" || task.status === "canceled" || task.status === "interrupted"));
  for (const task of retryable) await retryTask(task);
});

document.querySelector<HTMLButtonElement>("#task-batch-delete")!.addEventListener("click", () => {
  for (const id of selectedTasks) {
    tasks.delete(id);
    invoke("delete_task", { id }).catch(() => {});
  }
  selectedTasks.clear();
  renderTasks();
});

document.querySelector<HTMLButtonElement>("#task-clear-complete")!.addEventListener("click", () => {
  for (const [id, task] of tasks) if (task.status === "complete") tasks.delete(id);
  invoke("clear_completed_tasks").catch(() => {});
  renderTasks();
});

function renderTasks() {
  document.querySelector("#task-count")!.textContent = String([...tasks.values()].filter((task) => task.status === "downloading" || task.status === "queued").length);
  document.querySelector<HTMLDivElement>("#task-toolbar")!.hidden = tasks.size === 0;
  const selectAll = document.querySelector<HTMLInputElement>("#task-select-all");
  if (selectAll) selectAll.checked = tasks.size > 0 && [...tasks.keys()].every((id) => selectedTasks.has(id));
  taskList.replaceChildren();
  if (!tasks.size) {
    taskList.innerHTML = `<div class="empty-state task-empty"><div class="empty-visual"><span>⇩</span><i></i><b></b></div><strong>暂无下载任务</strong><p>从探测结果选择视频资源即可添加下载任务。</p></div>`;
    return;
  }
  for (const task of [...tasks.values()].reverse()) {
    const card = document.createElement("article");
    card.className = "task-card";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.className = "task-check";
    checkbox.checked = selectedTasks.has(task.id);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) selectedTasks.add(task.id); else selectedTasks.delete(task.id);
    });
    const main = document.createElement("div");
    main.className = "task-main";
    const name = document.createElement("strong");
    name.textContent = task.filename;
    name.title = task.filename;
    const meta = document.createElement("div");
    meta.className = "task-meta";
    const metaText = document.createElement("span");
    metaText.className = "task-meta-text";
    if (task.unit === "segments") {
      const receivedSize = task.receivedBytes !== undefined ? prettyBytes(task.receivedBytes) : "";
      const totalSize = task.totalBytes !== undefined ? prettyBytes(task.totalBytes) : "";
      const size = receivedSize && totalSize ? `${receivedSize} / 约${totalSize}` : receivedSize;
      metaText.textContent = task.status === "complete"
        ? `${task.received} 个分片 · ${receivedSize} · 已保存到下载目录`
        : `${task.received} / ${task.total ?? "?"} 个分片 · ${size}`;
    } else {
      metaText.textContent = task.total ? `${prettyBytes(task.received)} / ${prettyBytes(task.total)}` : task.status === "complete" ? "已保存到下载目录" : prettyBytes(task.received);
    }
    if (task.error) metaText.textContent = task.error;
    const knownTotal = task.total !== undefined && task.total > 0;
    const ratio = task.status === "complete" ? 1 : knownTotal ? Math.min(task.received / task.total!, 1) : 0;
    const indeterminate = !knownTotal && task.status !== "complete" && (task.status === "downloading" || task.status === "queued");
    const meter = document.createElement("div");
    meter.className = "task-meter";
    const fill = document.createElement("i");
    const pct = document.createElement("span");
    pct.className = "task-pct";
    if (indeterminate) {
      meter.classList.add("indeterminate");
    } else if (!knownTotal && task.status !== "complete") {
      meter.hidden = true;
    } else {
      fill.style.width = `${Math.round(ratio * 100)}%`;
      pct.textContent = `${Math.round(ratio * 100)}%`;
      if (task.status === "failed" || task.status === "canceled" || task.status === "interrupted") fill.classList.add("paused");
    }
    if (meter.hidden) pct.hidden = true;
    meter.append(fill);
    meta.append(meter, pct, metaText);
    main.append(name, meta);
    const state = document.createElement("span");
    state.className = `task-state state-${task.status}`;
    state.textContent = task.status === "complete" ? "已完成" : task.status === "failed" ? "失败" : task.status === "queued" ? "准备中" : task.status === "canceled" ? "已取消" : task.status === "interrupted" ? "已中断" : "下载中";
    const actions = document.createElement("div");
    actions.className = "task-actions";
    if (task.status === "downloading" || task.status === "queued") {
      const cancelBtn = document.createElement("button");
      cancelBtn.className = "task-btn cancel-btn";
      cancelBtn.textContent = "取消";
      cancelBtn.addEventListener("click", () => { invoke("cancel_download", { id: task.id }); });
      actions.append(cancelBtn);
    }
    if (task.status === "failed" || task.status === "canceled" || task.status === "interrupted") {
      const retryBtn = document.createElement("button");
      retryBtn.className = "task-btn retry-btn";
      retryBtn.textContent = task.received > 0 ? "继续下载" : "重试";
      retryBtn.addEventListener("click", () => { void retryTask(task); });
      actions.append(retryBtn);
    }
    if (task.status === "complete" || task.status === "failed" || task.status === "canceled" || task.status === "interrupted") {
      const folderBtn = document.createElement("button");
      folderBtn.className = "task-btn folder-btn";
      folderBtn.textContent = "文件夹";
      folderBtn.title = "打开所在文件夹";
      folderBtn.addEventListener("click", () => { invoke("show_in_folder", { id: task.id }).catch(() => {}); });
      const delBtn = document.createElement("button");
      delBtn.className = "task-btn delete-btn";
      delBtn.textContent = "删除";
      delBtn.addEventListener("click", () => { tasks.delete(task.id); invoke("delete_task", { id: task.id }).catch(() => {}); renderTasks(); });
      actions.append(folderBtn, delBtn);
    }
    card.append(checkbox, main, state, actions);
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
  const detectBanner = document.querySelector<HTMLDivElement>("#detect-banner")!;
  const visibleCheck = document.querySelector<HTMLInputElement>("#detect-visible")!;
  try {
    currentBrowserUrl = formatUrl(input.value);
    input.value = currentBrowserUrl;
    detectBanner.hidden = false;
    await invoke(visibleCheck.checked ? "open_visible_page" : "open_page", { url: currentBrowserUrl, kind: "detect" });
  } catch (error) {
    if (!visibleCheck.checked && window.confirm(`无头浏览器未能打开页面：${String(error)}\n\n是否改用可视化窗口？`)) {
      try {
        visibleCheck.checked = true;
        await invoke("open_visible_page", { url: currentBrowserUrl, kind: "detect" });
        return;
      } catch (visibleError) {
        input.setCustomValidity(String(visibleError));
      }
    } else {
      input.setCustomValidity(String(error));
    }
    input.reportValidity();
    input.addEventListener("input", () => input.setCustomValidity(""), { once: true });
  } finally {
    detectBanner.hidden = true;
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

// ===== 局域网下载服务 =====

async function refreshLanInfo() {
  try {
    const info = await invoke<LanInfo>("get_lan_info");
    document.querySelector<HTMLSpanElement>("#lan-status-text")!.textContent = info.running ? `服务运行中 · 端口 ${info.port}` : "服务未开启";
    document.querySelector<HTMLSpanElement>("#lan-dot")!.classList.toggle("lan-dot-off", !info.running);
    const urlsBox = document.querySelector<HTMLDivElement>("#lan-urls")!;
    urlsBox.hidden = !info.running;
    urlsBox.replaceChildren();
    for (const url of info.urls) {
      const row = document.createElement("div");
      row.className = "lan-url-row";
      const text = document.createElement("code");
      text.textContent = url;
      const copy = document.createElement("button");
      copy.type = "button";
      copy.className = "task-btn";
      copy.textContent = "复制";
      copy.addEventListener("click", () => { navigator.clipboard.writeText(url).catch(() => {}); copy.textContent = "已复制"; setTimeout(() => { copy.textContent = "复制"; }, 1500); });
      row.append(text, copy);
      urlsBox.append(row);
    }
  } catch {
    document.querySelector<HTMLSpanElement>("#lan-status-text")!.textContent = "状态获取失败";
  }
}

document.querySelector<HTMLFormElement>("#lan-form")!.addEventListener("submit", async (event) => {
  event.preventDefault();
  const result = document.querySelector<HTMLSpanElement>("#lan-save-result")!;
  try {
    await invoke("save_settings", { settings: currentSettings() });
    result.textContent = "已应用";
    result.className = "settings-success";
    await refreshLanInfo();
  } catch (error) {
    result.textContent = String(error);
    result.className = "settings-error";
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
  const subdirCheck = document.querySelector<HTMLInputElement>("#batch-use-subdir")!;
  const subdirInput = document.querySelector<HTMLInputElement>("#batch-subdir-name")!;
  const show = episodes.find((episode) => episode.show)?.show ?? "";
  if (show) {
    subdirInput.value = show;
    subdirCheck.checked = true;
  } else if (episodes.length === 0) {
    subdirInput.value = "";
    subdirCheck.checked = false;
  }
  const displayName = (episode: Episode) => {
    const name = episode.show && !episode.title.includes(episode.show) ? `${episode.show} ${episode.title}` : episode.title;
    return name.replace(/</g, "&lt;");
  };
  list.innerHTML = episodes
    .map((episode, index) => `<label class="batch-item"><input type="checkbox" data-index="${index}" checked><span class="batch-title">${displayName(episode)}</span><span class="batch-url">${episode.url.replace(/</g, "&lt;")}</span></label>`)
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
  if (document.querySelector<HTMLInputElement>("#batch-visible")!.checked) {
    try {
      const url = formatUrl(input.value);
      input.value = url;
      lastAnalyzedUrl = "";
      showBatchProgress("已在原生窗口打开页面。如有 Cloudflare 验证请手动点击，页面完全加载后会自动提取剧集列表…");
      await invoke("open_visible_page", { url, kind: "batch" });
    } catch (error) {
      showBatchProgress(`打开页面失败：${String(error)}`, true);
    }
    return;
  }
  batchBusy = true;
  episodes = [];
  renderEpisodes();
  showBatchProgress("正在启动分析…");
  try {
    const url = formatUrl(input.value);
    const result = await invoke<{ episodes: Episode[]; steps: string[] }>("analyze_episodes", { url });
    episodes = result.episodes;
    renderEpisodes();
    showBatchProgress(`分析完成：共 ${result.episodes.length} 集。确认列表后点击"开始下载全部"。`);
  } catch (error) {
    showBatchProgress(`分析失败：${String(error)}`, true);
  } finally {
    batchBusy = false;
  }
});

let unlistenPageLinks: UnlistenFn | undefined;
let lastAnalyzedUrl = "";

async function handlePageLinks(payload: { url: string; title: string; links: string[] }) {
  if (payload.links.length === 0) return;
  const lowerTitle = payload.title.toLowerCase();
  if (lowerTitle.includes("请稍候") || lowerTitle.includes("just a moment") || lowerTitle.includes("checking your browser")) {
    showBatchProgress(`页面正在通过验证（${payload.title}），请稍候…`);
    return;
  }
  if (payload.url === lastAnalyzedUrl) return;
  lastAnalyzedUrl = payload.url;
  showBatchProgress(`检测到页面「${payload.title}」已加载（共 ${payload.links.length} 个链接），正在呼叫 LLM 分析剧集…`);
  try {
    batchBusy = true;
    const button = document.querySelector<HTMLButtonElement>("#batch-analyze-btn")!;
    button.disabled = true;
    const linksText = payload.links.join("\n");
    const result = await invoke<{ episodes: Episode[]; steps: string[] }>("analyze_page_links", {
      url: payload.url,
      htmlOrLinks: linksText,
      title: payload.title,
    });
    episodes = result.episodes;
    renderEpisodes();
    showBatchProgress(`分析完成：共 ${result.episodes.length} 集！勾选确认后点击下方按钮开始下载。`);
  } catch (error) {
    showBatchProgress(`从可见窗口提取分析失败：${String(error)}`, true);
  } finally {
    batchBusy = false;
    const button = document.querySelector<HTMLButtonElement>("#batch-analyze-btn")!;
    button.disabled = false;
  }
}

document.querySelector<HTMLButtonElement>("#batch-download-btn")!.addEventListener("click", async () => {
  if (batchBusy || episodes.length === 0) return;
  const selected = [...document.querySelectorAll<HTMLInputElement>("#batch-list input[type=checkbox]:checked")]
    .map((box) => episodes[Number(box.dataset.index)])
    .filter(Boolean);
  if (selected.length === 0) { showBatchProgress("请先选择要下载的集数", true); return; }
  batchBusy = true;
  const button = document.querySelector<HTMLButtonElement>("#batch-download-btn")! as HTMLButtonElement;
  button.disabled = true;
  const useSubdir = document.querySelector<HTMLInputElement>("#batch-use-subdir")!.checked;
  const subdirName = document.querySelector<HTMLInputElement>("#batch-subdir-name")!.value.trim();
  try {
    await invoke("batch_download", { episodes: selected, subdir: useSubdir && subdirName ? subdirName : null });
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

// ===== URL 历史抽屉 =====

let historyKind: "detect" | "batch" = "detect";
let activeView = "探测器";

function formatHistoryTime(createdAt: string): string {
  const date = new Date(Number(createdAt) * 1000);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function renderHistory(entries: HistoryEntry[]) {
  const list = document.querySelector<HTMLDivElement>("#history-list")!;
  document.querySelector<HTMLSpanElement>("#history-count")!.textContent = `${entries.length} 条记录`;
  list.replaceChildren();
  if (entries.length === 0) {
    list.innerHTML = `<div class="empty-state task-empty"><strong>暂无历史记录</strong><p>处理过的网址会自动记录在这里。</p></div>`;
    return;
  }
  for (const entry of entries) {
    const item = document.createElement("div");
    item.className = "history-item";
    const urlButton = document.createElement("button");
    urlButton.type = "button";
    urlButton.className = "history-url";
    urlButton.title = "点击填入输入框";
    urlButton.textContent = entry.url;
    urlButton.addEventListener("click", () => {
      const input = document.querySelector<HTMLInputElement>(historyKind === "batch" ? "#batch-url-input" : "#url-input")!;
      input.value = entry.url;
      closeHistory();
    });
    const meta = document.createElement("div");
    meta.className = "history-meta";
    const time = document.createElement("time");
    time.textContent = formatHistoryTime(entry.createdAt);
    const copy = document.createElement("button");
    copy.type = "button";
    copy.className = "task-btn";
    copy.textContent = "复制";
    copy.addEventListener("click", () => {
      navigator.clipboard.writeText(entry.url).catch(() => {});
      copy.textContent = "已复制";
      setTimeout(() => { copy.textContent = "复制"; }, 1500);
    });
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "task-btn delete-btn";
    remove.textContent = "删除";
    remove.addEventListener("click", async () => {
      await invoke("delete_history", { id: entry.id });
      item.remove();
      const count = document.querySelector<HTMLSpanElement>("#history-count")!;
      count.textContent = `${Math.max(0, (parseInt(count.textContent) || 1) - 1)} 条记录`;
      if (!list.querySelector(".history-item")) renderHistory([]);
    });
    const actions = document.createElement("div");
    actions.className = "history-actions";
    actions.append(copy, remove);
    meta.append(time, actions);
    item.append(urlButton, meta);
    list.append(item);
  }
}

async function refreshHistory() {
  try {
    renderHistory(await invoke<HistoryEntry[]>("get_history", { kind: historyKind }));
  } catch {
    renderHistory([]);
  }
}

function openHistory(kind: "detect" | "batch") {
  historyKind = kind;
  document.querySelector<HTMLHeadingElement>("#history-title")!.textContent = kind === "batch" ? "批量下载历史" : "视频探测历史";
  document.querySelector<HTMLDivElement>("#history-overlay")!.classList.add("open");
  const drawer = document.querySelector<HTMLElement>("#history-drawer")!;
  drawer.classList.add("open");
  drawer.setAttribute("aria-hidden", "false");
  void refreshHistory();
}

function closeHistory() {
  document.querySelector<HTMLDivElement>("#history-overlay")!.classList.remove("open");
  const drawer = document.querySelector<HTMLElement>("#history-drawer")!;
  drawer.classList.remove("open");
  drawer.setAttribute("aria-hidden", "true");
}

document.querySelector<HTMLButtonElement>("#history-btn")!.addEventListener("click", () => {
  openHistory(activeView === "批量下载" ? "batch" : "detect");
});
document.querySelector<HTMLButtonElement>("#history-close")!.addEventListener("click", closeHistory);
document.querySelector<HTMLDivElement>("#history-overlay")!.addEventListener("click", closeHistory);
document.addEventListener("keydown", (event) => { if (event.key === "Escape") closeHistory(); });
document.querySelector<HTMLButtonElement>("#history-clear")!.addEventListener("click", async () => {
  if (!window.confirm("确定清空当前类别的全部历史记录？")) return;
  await invoke("clear_history", { kind: historyKind });
  renderHistory([]);
});

document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((button) => button.addEventListener("click", () => {
  const view = button.dataset.view;
  activeView = view ?? "探测器";
  document.querySelectorAll(".nav-item").forEach((item) => item.classList.toggle("active", item === button));
  document.querySelector<HTMLDivElement>("#detector-view")!.hidden = view !== "探测器";
  document.querySelector<HTMLDivElement>("#batch-view")!.hidden = view !== "批量下载";
  document.querySelector<HTMLDivElement>("#tasks-view")!.hidden = view !== "下载任务";
  document.querySelector<HTMLDivElement>("#lan-view")!.hidden = view !== "局域网";
  document.querySelector<HTMLDivElement>("#settings-view")!.hidden = view !== "设置";
  document.querySelector<HTMLSpanElement>("#page-title")!.textContent = view ?? "视频探测器";
  document.querySelector<HTMLButtonElement>("#history-btn")!.hidden = view !== "探测器" && view !== "批量下载";
  if (view === "局域网") void refreshLanInfo();
}));

async function initialize() {
  unlistenMedia = await listen<MediaItem>("media-found", ({ payload }) => { media.set(payload.id, payload); renderMedia(); });
  unlistenProgress = await listen<DownloadUpdate>("download-progress", ({ payload }) => {
    const existing = tasks.get(payload.id);
    if (existing) {
      Object.assign(existing, payload);
    } else {
      // Tasks queued by the backend (batch download) have no frontend-created
      // entry, so adopt them on their first progress event.
      tasks.set(payload.id, { sourceUrl: "", ...payload });
    }
    renderTasks();
  });
  unlistenPageLinks = await listen<{ url: string; title: string; links: string[] }>("browser-page-links", ({ payload }) => {
    void handlePageLinks(payload);
  });
  const [existing, settings, persistedTasks] = await Promise.all([
    invoke<MediaItem[]>("get_media"),
    invoke<AppSettings>("get_settings"),
    invoke<TaskRecord[]>("get_tasks"),
  ]);
  existing.forEach((item) => media.set(item.id, item));
  // 恢复上次会话的下载任务（后端已把进行中的标记为 interrupted）
  for (const record of persistedTasks) {
    tasks.set(record.id, {
      id: record.id,
      filename: record.filename,
      status: record.status,
      received: record.received,
      total: record.total,
      error: record.error,
      unit: record.unit,
      receivedBytes: record.receivedBytes,
      totalBytes: record.totalBytes,
      sourceUrl: record.url,
    });
  }
  document.querySelector<HTMLSelectElement>("#browser-source")!.value = settings.browserSource;
  document.querySelector<HTMLInputElement>("#chromium-path")!.value = settings.localChromiumPath;
  document.querySelector<HTMLInputElement>("#download-dir")!.value = settings.downloadDir ?? "";
  document.querySelector<HTMLInputElement>("#llm-api-url")!.value = settings.llmApiUrl ?? "";
  document.querySelector<HTMLInputElement>("#llm-api-key")!.value = settings.llmApiKey ?? "";
  document.querySelector<HTMLInputElement>("#llm-model")!.value = settings.llmModel ?? "";
  document.querySelector<HTMLInputElement>("#max-concurrent")!.value = String(settings.maxConcurrent ?? 3);
  document.querySelector<HTMLInputElement>("#lan-enabled")!.checked = settings.lanEnabled ?? false;
  document.querySelector<HTMLInputElement>("#lan-port")!.value = String(settings.lanPort ?? 8688);
  updateBrowserSourceFields();
  renderMedia();
  renderTasks();
  window.addEventListener("beforeunload", () => { unlistenMedia?.(); unlistenProgress?.(); unlistenPageLinks?.(); });
}

void initialize();
