const downloadContainer = document.querySelector("#downloads");
const playlistContainer = document.querySelector("#playlists");
const summary = document.querySelector("#summary");
let selectedState = "all";
let activeTabId;
let playlistItems = [];

function messageBackground(message) {
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage(message, (response) => {
      if (chrome.runtime.lastError) reject(new Error(chrome.runtime.lastError.message));
      else if (response?.error) reject(new Error(response.error));
      else resolve(response);
    });
  });
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return "未知大小";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit += 1; }
  return `${size.toFixed(1)} ${units[unit]}`;
}

function filenameFromUrl(url, extension = ".ts") {
  try {
    const raw = decodeURIComponent(new URL(url).pathname.split("/").filter(Boolean).pop() || "video");
    const base = raw.replace(/\.m3u8(?:$|[?#])/i, "").replace(/[\\/:*?"<>|]+/g, "_").slice(0, 100);
    return `${base || "video"}${extension}`;
  } catch { return `video${extension}`; }
}

function isPlaylist(url) { return /\.m3u8(?:$|[?#])/i.test(url); }

function addEmpty(container, text) {
  const empty = document.createElement("div");
  empty.className = "empty";
  empty.textContent = text;
  container.append(empty);
}

async function startM3u8Download(item, button) {
  button.disabled = true;
  button.textContent = "读取播放列表…";
  try {
    let playlistUrl = item.url;
    let playlist = await messageBackground({ type: "FETCH_TEXT", url: playlistUrl });
    let lines = playlist.text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);

    if (lines.some((line) => line.startsWith("#EXT-X-STREAM-INF"))) {
      const variants = [];
      for (let i = 0; i < lines.length; i += 1) {
        if (!lines[i].startsWith("#EXT-X-STREAM-INF")) continue;
        const nextUri = lines.slice(i + 1).find((line) => !line.startsWith("#"));
        if (!nextUri) continue;
        const bandwidth = Number(lines[i].match(/(?:AVERAGE-BANDWIDTH|BANDWIDTH)=(\d+)/)?.[1] || 0);
        variants.push({ url: new URL(nextUri, playlistUrl).href, bandwidth });
      }
      if (!variants.length) throw new Error("主播放列表中没有可用的视频流");
      playlistUrl = variants.sort((a, b) => b.bandwidth - a.bandwidth)[0].url;
      playlist = await messageBackground({ type: "FETCH_TEXT", url: playlistUrl });
      lines = playlist.text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    }

    if (lines.some((line) => line.startsWith("#EXT-X-KEY") && !/METHOD=NONE/.test(line))) {
      throw new Error("此视频使用了加密 HLS，目前不支持解密下载");
    }
    if (lines.some((line) => line.startsWith("#EXT-X-MAP"))) {
      throw new Error("此 fMP4/CMAF HLS 需要额外的初始化片段，目前暂不支持");
    }

    const segments = lines.filter((line) => !line.startsWith("#")).map((line) => new URL(line, playlistUrl).href);
    if (!segments.length) throw new Error("播放列表里没有视频分片");

    const maxSegments = 5000;
    if (segments.length > maxSegments) throw new Error(`分片数量过多（${segments.length}），为避免占用过多内存已停止`);
    const total = segments.length;
    const parts = new Array(total);
    let nextIndex = 0;
    let completed = 0;
    let failed;
    button.textContent = `下载分片 0/${total}`;

    async function worker() {
      while (true) {
        const index = nextIndex++;
        if (index >= total || failed) return;
        try {
          const result = await messageBackground({ type: "FETCH_SEGMENT", url: segments[index] });
          const binary = atob(result.data);
          const bytes = new Uint8Array(binary.length);
          for (let offset = 0; offset < binary.length; offset += 1) bytes[offset] = binary.charCodeAt(offset);
          parts[index] = bytes;
          completed += 1;
          button.textContent = `下载分片 ${completed}/${total}`;
        } catch (error) { failed = error; }
      }
    }

    await Promise.all(Array.from({ length: Math.min(6, total) }, worker));
    if (failed) throw failed;
    button.textContent = "合并视频…";
    const blob = new Blob(parts, { type: "video/mp2t" });
    const data = await blob.arrayBuffer();
    const response = await messageBackground({
      type: "DOWNLOAD_BLOB",
      data: Array.from(new Uint8Array(data)),
      filename: filenameFromUrl(item.url),
      mimeType: "video/mp2t",
    });
    button.textContent = `已开始下载（${formatBytes(blob.size)}）`;
    return response;
  } catch (error) {
    button.disabled = false;
    button.textContent = "重试下载";
    summary.textContent = error.message;
  }
}

function renderPlaylists() {
  playlistContainer.replaceChildren();
  if (!playlistItems.length) {
    addEmpty(playlistContainer, "当前标签页还没有识别到 M3U8。打开视频播放后，点击“读取当前页面”。");
    return;
  }
  playlistItems.filter((item) => isPlaylist(item.url)).forEach((item) => {
    const card = document.createElement("article");
    card.className = "playlist-card";
    const top = document.createElement("div");
    top.className = "playlist-top";
    const info = document.createElement("div");
    info.style.minWidth = "0";
    const name = document.createElement("div");
    name.className = "playlist-name";
    name.textContent = filenameFromUrl(item.url, "");
    const meta = document.createElement("div");
    meta.className = "playlist-meta";
    meta.textContent = `M3U8 · 捕获于 ${new Date(item.seenAt).toLocaleTimeString()}`;
    const url = document.createElement("div");
    url.className = "playlist-url";
    url.textContent = item.url;
    info.append(name, meta, url);
    const button = document.createElement("button");
    button.className = "download";
    button.textContent = "下载完整视频";
    button.addEventListener("click", () => startM3u8Download(item, button));
    top.append(info, button);
    card.append(top);
    playlistContainer.append(card);
  });
}

function loadPlaylists() {
  chrome.tabs.query({ active: true, currentWindow: true }, async (tabs) => {
    activeTabId = tabs[0]?.id;
    if (activeTabId == null) return;
    try {
      const response = await messageBackground({ type: "GET_MEDIA", tabId: activeTabId });
      playlistItems = response.items ?? [];
      renderPlaylists();
    } catch (error) { summary.textContent = error.message; }
  });
}

function stateText(item) {
  if (item.state === "complete") return "已完成";
  if (item.state === "interrupted") return "失败";
  return item.paused ? "已暂停" : "下载中";
}

function renderTask(item) {
  const card = document.createElement("article");
  card.className = "task";
  const top = document.createElement("div");
  top.className = "task-top";
  const name = document.createElement("div");
  name.className = "task-name";
  name.textContent = item.filename?.split(/[\\/]/).pop() || item.url;
  name.title = item.filename || item.url;
  const status = document.createElement("span");
  status.className = `task-status ${item.state}`;
  status.textContent = stateText(item);
  top.append(name, status);
  const meta = document.createElement("div");
  meta.className = "task-meta";
  meta.textContent = item.startTime ? new Date(item.startTime).toLocaleString() : item.url;
  card.append(top, meta);

  if (item.state === "in_progress") {
    const progress = document.createElement("progress");
    progress.max = item.totalBytes > 0 ? item.totalBytes : 1;
    progress.value = item.totalBytes > 0 ? item.bytesReceived : 0;
    card.append(progress);
  }
  const bottom = document.createElement("div");
  bottom.className = "task-bottom";
  const size = document.createElement("span");
  size.textContent = `${formatBytes(item.bytesReceived)} / ${formatBytes(item.totalBytes)}`;
  const actions = document.createElement("div");
  actions.className = "task-actions";
  if (item.state === "in_progress") {
    const action = item.paused ? "resume" : "pause";
    const button = document.createElement("button");
    button.textContent = item.paused ? "继续" : "暂停";
    button.addEventListener("click", () => chrome.downloads[action](item.id));
    const cancel = document.createElement("button");
    cancel.className = "danger";
    cancel.textContent = "取消";
    cancel.addEventListener("click", () => chrome.downloads.cancel(item.id));
    actions.append(button, cancel);
  }
  if (item.state === "interrupted" && item.canResume) {
    const retry = document.createElement("button");
    retry.textContent = "重试";
    retry.addEventListener("click", () => chrome.downloads.resume(item.id));
    actions.append(retry);
  }
  if (item.state === "complete" && item.exists) {
    const reveal = document.createElement("button");
    reveal.textContent = "在文件夹中显示";
    reveal.addEventListener("click", () => chrome.downloads.show(item.id));
    actions.append(reveal);
  }
  bottom.append(size, actions);
  card.append(bottom);
  if (item.state === "interrupted" && item.error) {
    const error = document.createElement("div");
    error.className = "task-error";
    error.textContent = `原因：${item.error}`;
    card.append(error);
  }
  downloadContainer.append(card);
}

function renderDownloads() {
  chrome.downloads.search({ orderBy: ["-startTime"], limit: 100 }, (items) => {
    if (chrome.runtime.lastError) return;
    const activeCount = items.filter((item) => item.state === "in_progress").length;
    summary.textContent = `${activeCount} 个下载进行中 · 共 ${items.length} 条下载记录`;
    const filtered = selectedState === "all" ? items : items.filter((item) => item.state === selectedState);
    downloadContainer.replaceChildren();
    if (!filtered.length) addEmpty(downloadContainer, "没有符合条件的下载记录");
    else filtered.forEach(renderTask);
  });
}

document.querySelector("#refresh").addEventListener("click", () => { loadPlaylists(); renderDownloads(); });
document.querySelector("#load-playlists").addEventListener("click", loadPlaylists);
document.querySelectorAll(".filter").forEach((button) => button.addEventListener("click", () => {
  selectedState = button.dataset.state;
  document.querySelectorAll(".filter").forEach((filter) => filter.classList.toggle("active", filter === button));
  renderDownloads();
}));
chrome.downloads.onCreated.addListener(renderDownloads);
chrome.downloads.onChanged.addListener(renderDownloads);
chrome.downloads.onErased.addListener(renderDownloads);
loadPlaylists();
renderDownloads();
