const MEDIA_PATTERN = /\.(?:m3u8|mp4|m4v|mov|webm|mpd|flv)(?:$|[?#])/i;
const REQUEST_TYPES = new Set(["media", "xmlhttprequest", "other"]);
const MAX_PER_TAB = 200;
const mediaByTab = new Map();

function isVideoRequest(url, type) {
  if (!REQUEST_TYPES.has(type)) return false;
  try {
    const parsed = new URL(url);
    if (MEDIA_PATTERN.test(parsed.href)) return true;
    const path = parsed.pathname.toLowerCase();
    return /(?:\.m3u8|\.mpd|\.mp4|\.m4v|\.mov|\.webm|\.flv)$/.test(path);
  } catch {
    return false;
  }
}

function remember(details) {
  if (details.tabId < 0 || !isVideoRequest(details.url, details.type)) return;
  const list = mediaByTab.get(details.tabId) ?? [];
  if (list.some((item) => item.url === details.url)) return;
  list.unshift({ url: details.url, type: details.type, seenAt: Date.now() });
  mediaByTab.set(details.tabId, list.slice(0, MAX_PER_TAB));
}

chrome.webRequest.onBeforeRequest.addListener(remember, { urls: ["<all_urls>"] });

chrome.tabs.onRemoved.addListener((tabId) => mediaByTab.delete(tabId));

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message?.type === "GET_MEDIA") {
    sendResponse({ items: mediaByTab.get(message.tabId) ?? [] });
    return;
  }

  if (message?.type === "DOWNLOAD_MEDIA") {
    if (/\.m3u8(?:$|[?#])/i.test(message.url)) {
      sendResponse({ error: "请在下载管理页面选择该 M3U8 并下载完整视频。" });
      return;
    }
    if (/^blob:/i.test(message.url)) {
      sendResponse({ error: "此视频地址为页面临时 blob 流，暂不支持直接下载。" });
      return;
    }
    chrome.downloads.download({
      url: message.url,
      filename: message.filename,
      saveAs: true,
    }, (downloadId) => {
      if (chrome.runtime.lastError) {
        sendResponse({ error: chrome.runtime.lastError.message });
        return;
      }
      sendResponse({ downloadId });
    });
    return true;
  }

  if (message?.type === "FETCH_TEXT") {
    fetch(message.url, { credentials: "include" }).then(async (response) => {
      if (!response.ok) throw new Error(`请求失败 (${response.status})`);
      sendResponse({ text: await response.text() });
    }).catch((error) => sendResponse({ error: error.message }));
    return true;
  }

  if (message?.type === "FETCH_SEGMENT") {
    fetch(message.url, { credentials: "include" }).then(async (response) => {
      if (!response.ok) throw new Error(`分片请求失败 (${response.status})`);
      const buffer = await response.arrayBuffer();
      const bytes = new Uint8Array(buffer);
      let binary = "";
      const chunkSize = 0x8000;
      for (let offset = 0; offset < bytes.length; offset += chunkSize) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize));
      }
      sendResponse({ data: btoa(binary) });
    }).catch((error) => sendResponse({ error: error.message }));
    return true;
  }

  if (message?.type === "DOWNLOAD_BLOB") {
    const url = URL.createObjectURL(new Blob([new Uint8Array(message.data)], { type: message.mimeType || "video/mp2t" }));
    chrome.downloads.download({ url, filename: message.filename, saveAs: true }, (downloadId) => {
      if (chrome.runtime.lastError) {
        URL.revokeObjectURL(url);
        sendResponse({ error: chrome.runtime.lastError.message });
        return;
      }
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
      sendResponse({ downloadId });
    });
    return true;
  }

  if (message?.type === "CLEAR_MEDIA") {
    mediaByTab.delete(message.tabId);
    sendResponse({ ok: true });
  }
});
