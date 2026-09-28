const list = document.querySelector("#list");
const status = document.querySelector("#status");
const count = document.querySelector("#count");
const refreshButton = document.querySelector("#refresh");
const clearButton = document.querySelector("#clear");
let activeTabId;

function filenameFromUrl(url, index) {
  try {
    const pathname = new URL(url).pathname;
    const lastPart = decodeURIComponent(pathname.split("/").filter(Boolean).pop() || "");
    const clean = lastPart.replace(/[\\/:*?"<>|]+/g, "_").slice(0, 120);
    return clean || `video-${index + 1}.mp4`;
  } catch {
    return `video-${index + 1}.mp4`;
  }
}

function formatType(url) {
  const match = url.match(/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i);
  return match ? match[1].toUpperCase() : "媒体流";
}

function downloadMedia(url, filename, button) {
  button.disabled = true;
  button.textContent = "处理中";
  chrome.runtime.sendMessage({ type: "DOWNLOAD_MEDIA", url, filename }, (response) => {
    if (chrome.runtime.lastError || response?.error) {
      button.disabled = false;
      button.textContent = "重试";
      status.textContent = response?.error || chrome.runtime.lastError.message;
      return;
    }
    button.textContent = "已开始";
    setTimeout(() => {
      button.disabled = false;
      button.textContent = "下载";
    }, 1500);
  });
}

function addItem(item, index) {
  const card = document.createElement("article");
  card.className = "item";

  const info = document.createElement("div");
  info.className = "item-info";
  const name = document.createElement("div");
  name.className = "item-name";
  name.textContent = filenameFromUrl(item.url, index);
  name.title = item.url;
  const meta = document.createElement("div");
  meta.className = "item-meta";
  meta.textContent = `${formatType(item.url)} · ${new Date(item.seenAt).toLocaleTimeString()}`;
  info.append(name, meta);

  const download = document.createElement("button");
  download.className = "download";
  download.textContent = "下载";
  download.addEventListener("click", () => {
    if (/\.m3u8(?:$|[?#])/i.test(item.url)) {
      chrome.tabs.create({ url: chrome.runtime.getURL("downloads.html") });
      return;
    }
    downloadMedia(item.url, filenameFromUrl(item.url, index), download);
  });
  card.append(info, download);
  list.append(card);
}

function loadMedia() {
  if (activeTabId == null) return;
  status.textContent = "正在查找媒体请求…";
  chrome.runtime.sendMessage({ type: "GET_MEDIA", tabId: activeTabId }, (response) => {
    if (chrome.runtime.lastError) {
      status.textContent = "无法读取资源，请重新打开扩展";
      return;
    }
    const items = response?.items ?? [];
    list.replaceChildren();
    count.textContent = `${items.length} 个资源`;
    status.textContent = items.length ? "发现当前页面请求的媒体" : "未发现视频请求，请播放视频后刷新";
    if (!items.length) {
      const empty = document.createElement("div");
      empty.className = "empty";
      empty.textContent = "播放页面中的视频后，点击右上角刷新";
      list.append(empty);
      return;
    }
    items.forEach(addItem);
  });
}

refreshButton.addEventListener("click", loadMedia);
clearButton.addEventListener("click", () => {
  chrome.runtime.sendMessage({ type: "CLEAR_MEDIA", tabId: activeTabId }, loadMedia);
});

chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
  activeTabId = tabs[0]?.id;
  loadMedia();
});
