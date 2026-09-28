use aes::Aes128;
use cbc::Decryptor;
use chromiumoxide::{browser::{Browser, BrowserConfig}, fetcher::{BrowserFetcher, BrowserFetcherOptions}};
use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use reqwest::{header, Client, Url};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex}};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::RwLock;
use tokio::{fs::File, io::AsyncWriteExt};

fn debug_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/video_scout_debug.log") {
        let _ = writeln!(f, "{}", msg);
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaItem {
    id: String,
    url: String,
    kind: String,
    source_url: String,
    captured_at: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadUpdate {
    id: String,
    filename: String,
    status: String,
    received: u64,
    total: Option<u64>,
    error: Option<String>,
    /// "segments" = received/total are segment counts (HLS), "bytes" = byte counts.
    #[serde(default)]
    unit: Option<String>,
    /// Cumulative bytes downloaded (HLS segment downloads).
    #[serde(default)]
    received_bytes: Option<u64>,
    /// Estimated total bytes (HLS, extrapolated from average segment size).
    #[serde(default)]
    total_bytes: Option<u64>,
}

#[tauri::command]
fn report_page_links(app: AppHandle, payload: PageLinksPayload) -> Result<(), String> {
    let _ = app.emit("browser-page-links", payload);
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageLinksPayload {
    url: String,
    title: String,
    links: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRequest {
    id: String,
    url: String,
    source_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    id: String,
    url: String,
    filename: String,
    referer: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    browser_source: String,
    listening_mode: String,
    local_chromium_path: String,
    #[serde(default)]
    download_dir: String,
    #[serde(default)]
    llm_api_url: String,
    #[serde(default)]
    llm_api_key: String,
    #[serde(default)]
    llm_model: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            browser_source: "managed".into(),
            listening_mode: "headless".into(),
            local_chromium_path: String::new(),
            download_dir: String::new(),
            llm_api_url: String::new(),
            llm_api_key: String::new(),
            llm_model: String::new(),
        }
    }
}

struct AppState {
    media: Mutex<HashMap<String, MediaItem>>,
    downloads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>,
    settings: RwLock<AppSettings>,
  settings_path: PathBuf,
    headless_page_url: Mutex<String>,
    headless_browser: tokio::sync::Mutex<Option<Arc<Browser>>>,
}

impl AppState {
   fn new(settings: AppSettings, settings_path: PathBuf) -> Self {
        Self {
            media: Mutex::default(),
            downloads: Mutex::default(),
            settings: RwLock::new(settings),
            settings_path,
            headless_page_url: Mutex::default(),
            headless_browser: tokio::sync::Mutex::new(None),
        }
    }
}

fn classify_media(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let path = parsed.path().to_ascii_lowercase();
    for (suffix, kind) in [
        (".m3u8", "m3u8"), (".mpd", "mpd"), (".mp4", "mp4"),
        (".m4v", "m4v"), (".mov", "mov"), (".webm", "webm"), (".flv", "flv"),
    ] {
        if path.ends_with(suffix) { return Some(kind.to_string()); }
    }
    None
}

fn is_allowed_url(url: &str) -> bool {
    Url::parse(url).map(|parsed| matches!(parsed.scheme(), "http" | "https")).unwrap_or(false)
}

fn emit_download(app: &AppHandle, update: DownloadUpdate) {
    let _ = app.emit("download-progress", update);
}

fn safe_filename(filename: &str) -> String {
    let cleaned: String = filename.chars().map(|character| {
        if character.is_control() || matches!(character, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { character }
    }).collect();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() { "video.bin".to_string() } else { cleaned.to_string() }
}

fn app_download_dir(app: &AppHandle, settings: &AppSettings) -> Result<PathBuf, String> {
    let path = if !settings.download_dir.is_empty() {
        PathBuf::from(&settings.download_dir)
    } else {
        let mut p = app.path().download_dir().map_err(|error| error.to_string())?;
        p.push("Video Scout");
        p
    };
    std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn emit_captured(app: &AppHandle, state: &AppState, url: String, source_url: String, mime_type: Option<&str>) -> Result<(), String> {
    if !is_allowed_url(&url) { debug_log(&format!("[emit_captured] URL not allowed: {url}")); return Ok(()); }
    // Skip HLS/DASH segment streams (.ts, .m4s, etc.) — only capture playlists and full video files.
    if let Some(mime) = mime_type {
        let mime_lower = mime.to_ascii_lowercase();
        if mime_lower == "video/mp2t" || mime_lower == "video/iso.segment" || mime_lower.contains("fragmented") {
            debug_log(&format!("[emit_captured] skipping segment: {url} ({mime_lower})"));
            return Ok(());
        }
    }
    let kind = classify_media(&url).or_else(|| {
        let mime = mime_type?.to_ascii_lowercase();
        if mime.contains("mpegurl") { Some("m3u8".to_string()) }
        else if mime.contains("dash+xml") { Some("mpd".to_string()) }
        else if mime.starts_with("video/") { Some(mime.split('/').nth(1)?.split(';').next()?.to_string()) }
        else { None }
    });
    let Some(kind) = kind else { debug_log(&format!("[emit_captured] no kind for: {url}")); return Ok(()); };
    let item = MediaItem { id: url.clone(), url: url.clone(), kind: kind.clone(), source_url, captured_at: chrono_time() };
    debug_log(&format!("[emit_captured] inserting: {url} kind={kind}"));
    state.media.lock().map_err(|_| "媒体列表锁定失败".to_string())?.insert(item.id.clone(), item.clone());
    let emit_result = app.emit("media-found", item);
    debug_log(&format!("[emit_captured] emit result: {:?}", emit_result.is_ok()));
    Ok(())
}

fn media_from_resource_type(resource_type: &str, mime_type: &str, url: &str) -> bool {
    classify_media(url).is_some()
        || resource_type.contains("Media")
        || resource_type.contains("Manifest")
        || mime_type.to_ascii_lowercase().contains("mpegurl")
        || mime_type.to_ascii_lowercase().contains("dash+xml")
        || mime_type.to_ascii_lowercase().starts_with("video/")
}

async fn browser_executable(app: &AppHandle, settings: &AppSettings) -> Result<PathBuf, String> {
    if settings.browser_source == "local" {
        let path = PathBuf::from(settings.local_chromium_path.trim());
        if settings.local_chromium_path.trim().is_empty() {
            return Err("请先设置本机 Chromium 可执行文件路径".into());
        }
        if !path.is_file() {
            return Err("指定的 Chromium 可执行文件不存在".into());
        }
        return Ok(path);
    }
    let mut cache = app.path().app_cache_dir().map_err(|error| { debug_log(&format!("[browser_executable] app_cache_dir failed: {error}")); error.to_string() })?;
    cache.push("chromium");
    debug_log(&format!("[browser_executable] cache dir: {}", cache.display()));
    tokio::fs::create_dir_all(&cache).await.map_err(|error| { debug_log(&format!("[browser_executable] create_dir_all failed: {error}")); error.to_string() })?;
    let fetcher = BrowserFetcher::new(BrowserFetcherOptions::builder().with_path(cache).build().map_err(|error| { debug_log(&format!("[browser_executable] fetcher options failed: {error}")); error.to_string() })?);
    debug_log("[browser_executable] fetching chromium (this may take a while)...");
    let installation = fetcher.fetch().await.map_err(|error| { debug_log(&format!("[browser_executable] fetch failed: {error}")); format!("Chromium 下载失败: {error}") })?;
    debug_log(&format!("[browser_executable] executable: {}", installation.executable_path.display()));
    Ok(installation.executable_path)
}

async fn ensure_headless_browser(app: &AppHandle, state: &AppState, settings: &AppSettings) -> Result<(), String> {
    let mut browser_slot = state.headless_browser.lock().await;
    if browser_slot.is_some() { debug_log("[ensure_headless_browser] browser already exists"); return Ok(()); }
    debug_log("[ensure_headless_browser] launching Chromium...");
    let executable = browser_executable(app, settings).await.map_err(|error| { debug_log(&format!("[ensure_headless_browser] browser_executable failed: {error}")); error })?;
    debug_log(&format!("[ensure_headless_browser] executable: {}", executable.display()));
    debug_log("[ensure_headless_browser] calling Browser::launch...");
    // Use a dedicated user-data-dir and clear stale Chrome singleton locks left by previous runs.
    let user_data_dir = std::env::temp_dir().join("video-scout-chrome-profile");
    let _ = std::fs::create_dir_all(&user_data_dir);
    for lock in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(user_data_dir.join(lock));
    }
    debug_log(&format!("[ensure_headless_browser] user_data_dir: {}", user_data_dir.display()));
    let launch_config = BrowserConfig::builder()
        .chrome_executable(executable)
        .no_sandbox()
        .new_headless_mode()
        .user_data_dir(user_data_dir)
        .hide()
        .args(["--autoplay-policy=no-user-gesture-required", "--disable-background-timer-throttling", "--disable-dev-shm-usage"])
        .build().map_err(|error| { debug_log(&format!("[ensure_headless_browser] config build failed: {error}")); error.to_string() })?;
    let launch_result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        Browser::launch(launch_config),
    ).await;
    let (browser, handler) = match launch_result {
        Ok(Ok(bh)) => { debug_log("[ensure_headless_browser] Chromium launched"); bh }
        Ok(Err(error)) => { debug_log(&format!("[ensure_headless_browser] launch failed: {error}")); return Err(format!("启动 Chromium 失败: {error}")); }
        Err(_) => { debug_log("[ensure_headless_browser] launch timed out (15s)"); return Err("启动 Chromium 超时（15秒），请检查 Chrome 是否被其他程序占用".to_string()); }
    };
    debug_log("[ensure_headless_browser] spawning handler driver");
    let browser = Arc::new(browser);
    *browser_slot = Some(browser.clone());
    // Drive the CDP Handler in a dedicated task. This is the ONLY place that polls the Handler.
    tauri::async_runtime::spawn(async move {
        use futures_util::StreamExt;
        let mut handler = handler;
        while let Some(result) = handler.next().await {
            if let Err(error) = result {
                debug_log(&format!("[handler] error: {error}"));
            }
        }
        debug_log("[handler] stream ended");
    });
    Ok(())
}

/// After navigation, some sites show a Cloudflare/anti-bot interstitial ("Just a moment...",
/// "请稍候...") before redirecting to the real page. Poll the document title for a bit and
/// return once it stops looking like a challenge page (or the timeout elapses).
async fn wait_for_challenge_page(page: &chromiumoxide::Page, timeout: std::time::Duration) {
    let is_challenge_title = |title: &str| {
        let lower = title.to_ascii_lowercase();
        title.contains("请稍候") || title.contains("正在验证") || lower.contains("just a moment") || lower.contains("attention required") || lower.contains("checking your browser")
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let title = page.evaluate("document.title").await.ok().and_then(|value| value.into_value::<String>().ok()).unwrap_or_default();
        if !is_challenge_title(&title) {
            if !title.is_empty() { debug_log(&format!("[wait_for_challenge_page] cleared, title={title:?}")); }
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            debug_log(&format!("[wait_for_challenge_page] still on challenge page after timeout, title={title:?}"));
            return;
        }
        debug_log(&format!("[wait_for_challenge_page] waiting, title={title:?}"));
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}

async fn open_headless_page(app: AppHandle, state: &AppState, url: &str, settings: &AppSettings) -> Result<(), String> {
    debug_log(&format!("[open_headless_page] url={url}"));
    ensure_headless_browser(&app, state, settings).await?;
    debug_log("[open_headless_page] browser ready, setting page url");
    *state.headless_page_url.lock().map_err(|_| "页面地址锁定失败".to_string())? = url.to_string();
    let browser_slot = state.headless_browser.lock().await;
    let browser = browser_slot.as_ref().ok_or("Chromium 初始化失败")?;
    debug_log("[open_headless_page] creating new page");
    let page = browser.new_page("about:blank").await.map_err(|error| { debug_log(&format!("[open_headless_page] new_page failed: {error}")); error.to_string() })?;
    debug_log("[open_headless_page] enabling stealth mode");
    if let Err(error) = page.enable_stealth_mode().await { debug_log(&format!("[open_headless_page] enable_stealth_mode failed: {error}")); }
    debug_log("[open_headless_page] page created, injecting script");
    page.evaluate_on_new_document("document.addEventListener('DOMContentLoaded', () => document.querySelectorAll('video').forEach(video => { video.muted = true; video.play().catch(() => {}); }));").await
        .map_err(|error| error.to_string())?;
    debug_log("[open_headless_page] enabling network");
    page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default()).await
        .map_err(|error| error.to_string())?;

    // Register network event listeners BEFORE navigation to catch all requests.
    debug_log("[open_headless_page] registering request listener");
    let mut requests = page
        .event_listener::<chromiumoxide::cdp::browser_protocol::network::EventRequestWillBeSent>()
        .await
        .map_err(|error| { debug_log(&format!("[open_headless_page] request listener failed: {error}")); format!("注册请求监听失败: {error}") })?;
    debug_log("[open_headless_page] registering response listener");
    let mut responses = page
        .event_listener::<chromiumoxide::cdp::browser_protocol::network::EventResponseReceived>()
        .await
        .map_err(|error| { debug_log(&format!("[open_headless_page] response listener failed: {error}")); format!("注册响应监听失败: {error}") })?;

    debug_log("[open_headless_page] listeners registered, spawning consumer + navigating");

    // Spawn the listener consumer BEFORE goto so it catches events during navigation.
    let media_app = app.clone();
    tauri::async_runtime::spawn(async move {
        use futures_util::StreamExt;
        loop {
            tokio::select! {
                response = responses.next() => {
                    let Some(response) = response else { break; };
                    if media_from_resource_type(&format!("{:?}", response.r#type), &response.response.mime_type, &response.response.url) {
                        debug_log(&format!("[listener] media response: {} ({})", response.response.url, response.response.mime_type));
                        let source_url = media_app.state::<AppState>().headless_page_url.lock().map(|url| url.clone()).unwrap_or_default();
                        let _ = emit_captured(&media_app, &media_app.state::<AppState>(), response.response.url.clone(), source_url, Some(&response.response.mime_type));
                    }
                }
                request = requests.next() => {
                                let Some(request) = request else { break; };
                                let req_url = &request.request.url;
                                let is_ts = req_url.ends_with(".ts") || req_url.ends_with(".m4s");
                                if classify_media(req_url).is_some() && !is_ts {
                        debug_log(&format!("[listener] media request: {}", request.request.url));
                        let source_url = media_app.state::<AppState>().headless_page_url.lock().map(|url| url.clone()).unwrap_or_default();
                        let _ = emit_captured(&media_app, &media_app.state::<AppState>(), request.request.url.clone(), source_url, None);
                    }
                }
            }
        }
        debug_log("[listener] stream ended");
    });

    // Navigate with a generous timeout — don't block forever if page is slow.
    let goto_future = page.goto(url);
    match tokio::time::timeout(std::time::Duration::from_secs(30), goto_future).await {
        Ok(Ok(_)) => {
            debug_log("[open_headless_page] navigation completed");
            wait_for_challenge_page(&page, std::time::Duration::from_secs(12)).await;
            // Some pages redirect (301/302, or JS location change) to a different URL before
            // settling — e.g. player pages that bounce through an auth/CDN hop. If we keep using
            // the originally-typed URL as the "source" for captured media, the Referer we later
            // send when downloading may not match what the real (post-redirect) page would have
            // sent, and some CDNs reject mismatched referers. Track the final URL instead.
            if let Some(final_url) = detect_redirect(&page, url).await {
                debug_log(&format!("[open_headless_page] redirected: {url} -> {final_url}"));
                if let Ok(mut current) = state.headless_page_url.lock() {
                    *current = final_url;
                }
            }
        }
        Ok(Err(error)) => {
            debug_log(&format!("[open_headless_page] navigation error: {error}"));
            return Err(format!("页面导航失败: {error}"));
        }
        Err(_) => debug_log("[open_headless_page] navigation timed out, listeners still active"),
    }
    Ok(())
}

async fn open_visible_page_inner(app: AppHandle, url: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("browser") {
        window.navigate(Url::parse(&url).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    } else {
        let initialization_script = r#"
          (() => {
            const isMedia = (url, type = '') => /\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(url || '') || /mpegurl|dash\+xml|video\//i.test(type || '');
            const send = (url, type = '') => {
              try {
                const absoluteUrl = new URL(url, location.href).href;
                if (!isMedia(absoluteUrl, type)) return;
                const invoke = window.__TAURI_INTERNALS__?.invoke;
                if (typeof invoke !== 'function') {
                  console.error('[Video Scout] Tauri invoke unavailable', location.href);
                  return;
                }
                invoke('capture_media', { payload: { id: absoluteUrl, url: absoluteUrl, sourceUrl: location.href } }).catch((error) => console.error('[Video Scout] capture failed', error));
              } catch (error) { console.error('[Video Scout] capture error', error); }
            };
            const originalFetch = window.fetch;
            window.fetch = function(...args) {
              const requestUrl = typeof args[0] === 'string' ? args[0] : args[0]?.url;
              send(requestUrl || '');
              return originalFetch.apply(this, args).then((response) => {
                const contentType = response.headers.get('content-type') || '';
                if (isMedia(response.url, contentType)) send(response.url, contentType);
                return response;
              });
            };
            const originalOpen = XMLHttpRequest.prototype.open;
            XMLHttpRequest.prototype.open = function(method, url, ...rest) {
              this.addEventListener('load', () => send(this.responseURL || new URL(url, location.href).href, this.getResponseHeader('content-type') || ''));
              send(url);
              return originalOpen.call(this, method, url, ...rest);
            };
            const inspect = () => document.querySelectorAll('video, source').forEach((element) => {
              const url = element.currentSrc || element.src;
              if (url) send(url);
            });
            const extractAndSendLinks = () => {
              try {
                const links = [];
                const seen = new Set();
                for (const a of document.querySelectorAll('a[href]')) {
                  let href; try { href = new URL(a.href, location.href).href; } catch { continue; }
                  if (!href.startsWith('http')) continue;
                  const text = (a.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60);
                  const key = href + '|' + text;
                  if (seen.has(key)) continue;
                  seen.add(key);
                  links.push(text + '|' + href);
                }
                const invoke = window.__TAURI_INTERNALS__?.invoke;
                if (typeof invoke === 'function' && links.length > 0) {
                  invoke('report_page_links', { payload: { url: location.href, title: document.title, links: links.slice(0, 600) } }).catch(() => {});
                }
              } catch {}
            };
            new MutationObserver(inspect).observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ['src'] });
            document.addEventListener('play', inspect, true);
            window.addEventListener('load', () => setTimeout(extractAndSendLinks, 1500));
            document.addEventListener('DOMContentLoaded', () => setTimeout(extractAndSendLinks, 1000));
            inspect();
          })();
        "#;
        WebviewWindowBuilder::new(&app, "browser", WebviewUrl::External(Url::parse(&url).map_err(|error| error.to_string())?))
            .title("页面探测器")
            .inner_size(1100.0, 760.0)
            .visible(true)
            .initialization_script_for_all_frames(initialization_script)
            .on_navigation(|_| true)
            .on_page_load(|webview, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    let page_url = payload.url().to_string();
                    let script = format!(
                        r#"(() => {{
                          const candidates = [];
                          const add = (url) => {{ try {{ const u = new URL(url, location.href); if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(u.href)) candidates.push(u.href); }} catch {{}} }};
                          add(location.href);
                          document.querySelectorAll('video, source').forEach((el) => add(el.currentSrc || el.src));
                          for (const frame of document.querySelectorAll('iframe')) {{ try {{ add(frame.contentWindow.location.href); }} catch {{}} }}
                          const text = document.documentElement?.innerHTML || '';
                          for (const match of text.matchAll(/(?:https?:)?[^\\s\\"'<>\\\\]+\\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\\?[^\\s\\"'<>\\\\]*)?/ig)) add(match[0]);
                          for (const frame of document.querySelectorAll('iframe')) {{ try {{ const frameUrl = frame.contentWindow.location.href; const frameText = frame.contentDocument?.documentElement?.innerHTML || ''; for (const match of frameText.matchAll(/(?:https?:)?[^\\s\\"'<>\\\\]+\\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\\?[^\\s\\"'<>\\\\]*)?/ig)) candidates.push(new URL(match[0], frameUrl).href); }} catch {{}} }}
                          for (const url of new Set(candidates)) window.__TAURI_INTERNALS__?.invoke('capture_media', {{ payload: {{ id: url, url, sourceUrl: {source_url:?} }} }}).catch(() => {{}});
                        }})();"#,
                        source_url = page_url
                    );
                    let _ = webview.eval(&script);
                }
            })
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn open_page(app: AppHandle, state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    if !is_allowed_url(&url) { return Err("请输入有效的 http 或 https 网页地址".to_string()); }
    let settings = state.settings.read().await.clone();
    debug_log(&format!("[open_page] url={url}, mode={}", settings.listening_mode));
    if settings.listening_mode == "headless" {
        open_headless_page(app, &state, &url, &settings).await
    } else {
        open_visible_page_inner(app, url).await
    }
}

#[tauri::command]
fn capture_media(app: AppHandle, state: tauri::State<'_, AppState>, payload: CaptureRequest) -> Result<(), String> {
    if payload.id != payload.url { return Ok(()); }
    emit_captured(&app, &state, payload.url, payload.source_url, None)
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, AppState>) -> Result<AppSettings, String> {
    Ok(state.settings.read().await.clone())
}

#[tauri::command]
async fn save_settings(state: tauri::State<'_, AppState>, settings: AppSettings) -> Result<(), String> {
    if !matches!(settings.browser_source.as_str(), "managed" | "local") { return Err("无效的 Chromium 来源设置".into()); }
    if !matches!(settings.listening_mode.as_str(), "headless" | "visible") { return Err("无效的监听模式设置".into()); }
    let settings_path = state.settings_path.clone();
    let settings_json = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    if let Some(parent) = settings_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
    }
    tokio::fs::write(settings_path, settings_json).await.map_err(|error| error.to_string())?;
    *state.settings.write().await = settings;
    Ok(())
}

#[tauri::command]
async fn open_visible_page(app: AppHandle, _state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    if !is_allowed_url(&url) { return Err("请输入有效的 http 或 https 网页地址".into()); }
    open_visible_page_inner(app, url).await
}

fn chrono_time() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string()
}

#[tauri::command]
fn get_media(state: tauri::State<'_, AppState>) -> Result<Vec<MediaItem>, String> {
    Ok(state.media.lock().map_err(|_| "媒体列表锁定失败".to_string())?.values().cloned().collect())
}

#[tauri::command]
async fn start_download(app: AppHandle, state: tauri::State<'_, AppState>, payload: DownloadRequest) -> Result<(), String> {
    if !is_allowed_url(&payload.url) { return Err("只支持 http 或 https 媒体地址".to_string()); }
    let settings = state.settings.read().await.clone();
    let output_dir = app_download_dir(&app, &settings)?;
    let cancel = Arc::new(tokio::sync::Mutex::new(false));
    state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?.insert(payload.id.clone(), cancel.clone());
    let app_handle = app.clone();
    let app_for_cleanup = app.clone();
    tauri::async_runtime::spawn(async move {
        let task_id = payload.id.clone();
        let task_name = payload.filename.clone();
        if let Err(error) = download_media(app_handle.clone(), payload, output_dir, cancel.clone()).await {
            emit_download(&app_handle, DownloadUpdate { id: task_id.clone(), filename: task_name, status: "failed".into(), received: 0, total: None, error: Some(error), unit: None, received_bytes: None, total_bytes: None });
        }
        if let Ok(mut downloads) = app_for_cleanup.state::<AppState>().downloads.lock() { downloads.remove(&task_id); }
    });
    Ok(())
}

async fn download_media(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>) -> Result<(), String> {
    if classify_media(&payload.url).as_deref() == Some("m3u8") { download_hls(app, payload, output_dir, cancel).await }
    else { download_direct(app, payload, output_dir, cancel).await }
}

fn build_client(referer: Option<&str>) -> Result<Client, String> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::USER_AGENT, header::HeaderValue::from_static("Mozilla/5.0 (compatible; VideoScout/0.1)"));
    if let Some(referer) = referer { headers.insert(header::REFERER, header::HeaderValue::from_str(referer).map_err(|error| error.to_string())?); }
    Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())
}

async fn download_direct(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>) -> Result<(), String> {
    let client = build_client(payload.referer.as_deref())?;
    let response = client.get(&payload.url).send().await.map_err(|error| error.to_string())?;
    if !response.status().is_success() { return Err(format!("下载请求返回 {}", response.status())); }
    let total = response.content_length();
    let path = output_dir.join(safe_filename(&payload.filename));
    let mut file = File::create(&path).await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut received = 0;
    while let Some(chunk) = stream.next().await {
        if *cancel.lock().await { let _ = tokio::fs::remove_file(&path).await; return Err("下载已取消".to_string()); }
        let chunk = chunk.map_err(|error| error.to_string())?;
        file.write_all(&chunk).await.map_err(|error| error.to_string())?;
        received += chunk.len() as u64;
        emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "downloading".into(), received, total, error: None, unit: Some("bytes".into()), received_bytes: None, total_bytes: None });
    }
    file.flush().await.map_err(|error| error.to_string())?;
    emit_download(&app, DownloadUpdate { id: payload.id, filename: payload.filename, status: "complete".into(), received, total, error: None, unit: Some("bytes".into()), received_bytes: None, total_bytes: None });
    Ok(())
}

async fn download_hls(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>) -> Result<(), String> {
    let client = build_client(payload.referer.as_deref())?;
    let mut playlist_url = Url::parse(&payload.url).map_err(|error| error.to_string())?;
    let mut playlist = client.get(playlist_url.clone()).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?.text().await.map_err(|error| error.to_string())?;
    if playlist.lines().any(|line| line.starts_with("#EXT-X-STREAM-INF")) {
        let lines: Vec<&str> = playlist.lines().collect();
        let mut variants = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.starts_with("#EXT-X-STREAM-INF") {
                if let Some(uri) = lines.iter().skip(index + 1).find(|candidate| !candidate.starts_with('#')) {
                    let bandwidth = line.split("BANDWIDTH=").nth(1).and_then(|part| part.split(',').next()).and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
                    if let Ok(url) = playlist_url.join(uri.trim()) { variants.push((bandwidth, url)); }
                }
            }
        }
        playlist_url = variants.into_iter().max_by_key(|variant| variant.0).map(|variant| variant.1).ok_or("主播放列表没有可用的视频流")?;
        playlist = client.get(playlist_url.clone()).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?.text().await.map_err(|error| error.to_string())?;
    }
    let media_sequence = playlist.lines().find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:").and_then(|value| value.parse::<u64>().ok())).unwrap_or(0);
    let mut segments = Vec::new();
    let mut current_key: Option<HlsKey> = None;
    for line in playlist.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attribute(attrs, "METHOD").unwrap_or_default();
            if method != "NONE" {
                if method != "AES-128" { return Err(format!("暂不支持 {} 加密方式", method)); }
                let uri = attribute(attrs, "URI").map(|value| playlist_url.join(&value).map_err(|error| error.to_string())).transpose()?;
                current_key = Some(HlsKey { method, uri: uri.map(|url| url.to_string()), iv: attribute(attrs, "IV") });
            } else { current_key = None; }
        } else if line.starts_with("#EXT-X-MAP") { return Err("该 fMP4/CMAF HLS 使用初始化分片，当前版本暂不支持".to_string()); }
        else if !line.starts_with('#') { segments.push((playlist_url.join(line).map_err(|error| error.to_string())?, current_key.clone())); }
    }
    if segments.is_empty() { return Err("播放列表中没有媒体分片".to_string()); }
    if segments.len() > 10_000 { return Err("分片数量超过 10000，已停止以避免占用过多资源".to_string()); }
    // Merged segment stream is an MPEG-TS video, not a playlist — use .ts extension.
    let safe_name = safe_filename(&payload.filename);
    let filename = safe_name
        .strip_suffix(".m3u8").or_else(|| safe_name.strip_suffix(".mpd"))
        .map(|stem| format!("{stem}.ts"))
        .unwrap_or(if safe_name.contains('.') { safe_name } else { format!("{safe_name}.ts") });
    let path = output_dir.join(filename.clone());
    let mut output = File::create(&path).await.map_err(|error| error.to_string())?;
    let total = segments.len() as u64;
    let mut received = 0u64;
    let mut received_bytes = 0u64;
    for (index, (segment_url, key)) in segments.iter().enumerate() {
        if *cancel.lock().await { let _ = tokio::fs::remove_file(&path).await; return Err("下载已取消".to_string()); }
        // Retry each segment up to 3 times so a transient network error doesn't abort the whole download.
        let mut segment_bytes = None;
        let mut last_error = String::new();
        for attempt in 0..3u32 {
            match async {
                let response = client.get(segment_url.clone()).send().await?;
                let response = response.error_for_status()?;
                Ok::<_, reqwest::Error>(response.bytes().await?.to_vec())
            }.await {
                Ok(bytes) => { segment_bytes = Some(bytes); break; }
                Err(error) => {
                    last_error = error.to_string();
                    debug_log(&format!("[hls] segment {index} attempt {} failed: {error}", attempt + 1));
                    if attempt < 2 { tokio::time::sleep(std::time::Duration::from_millis(500u64 * (attempt as u64 + 1))).await; }
                }
            }
        }
        let Some(mut bytes) = segment_bytes else {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(format!("分片 {}/{} 下载失败: {last_error}", index + 1, total));
        };
        if let Some(key) = key { decrypt_aes128(&client, key, media_sequence + index as u64, &mut bytes).await?; }
        output.write_all(&bytes).await.map_err(|error| error.to_string())?;
        received += 1;
        received_bytes += bytes.len() as u64;
        if received % 10 == 0 || received == total {
            let estimated_total = received_bytes * total / received.max(1);
            emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "downloading".into(), received, total: Some(total), error: None, unit: Some("segments".into()), received_bytes: Some(received_bytes), total_bytes: Some(estimated_total) });
        }
    }
    output.flush().await.map_err(|error| error.to_string())?;
    emit_download(&app, DownloadUpdate { id: payload.id, filename, status: "complete".into(), received, total: Some(total), error: None, unit: Some("segments".into()), received_bytes: Some(received_bytes), total_bytes: Some(received_bytes) });
    Ok(())
}

#[derive(Clone)]
struct HlsKey { method: String, uri: Option<String>, iv: Option<String> }

fn attribute(input: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let remaining = &input[input.find(&prefix)? + prefix.len()..];
    if let Some(quoted) = remaining.strip_prefix('"') { Some(quoted.split('"').next()?.to_string()) }
    else { Some(remaining.split(',').next()?.to_string()) }
}

async fn decrypt_aes128(client: &Client, key: &HlsKey, sequence: u64, bytes: &mut Vec<u8>) -> Result<(), String> {
    if key.method != "AES-128" { return Err(format!("暂不支持 {} 加密方式", key.method)); }
    let key_url = key.uri.as_ref().ok_or("加密 HLS 缺少密钥地址")?;
    let key_bytes = client.get(key_url).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?.bytes().await.map_err(|error| error.to_string())?;
    if key_bytes.len() != 16 { return Err("AES-128 密钥长度不正确".to_string()); }
    let iv = if let Some(value) = &key.iv {
        let mut iv = [0u8; 16];
        let padded = format!("{:0>32}", value.trim_start_matches("0x"));
        for (index, pair) in padded.as_bytes().chunks(2).enumerate() { iv[index] = u8::from_str_radix(std::str::from_utf8(pair).map_err(|error| error.to_string())?, 16).map_err(|error| error.to_string())?; }
        iv
    } else { let mut iv = [0u8; 16]; iv[8..].copy_from_slice(&sequence.to_be_bytes()); iv };
    let mut buffer = std::mem::take(bytes);
    let decrypted = Decryptor::<Aes128>::new_from_slices(&key_bytes, &iv).map_err(|error| error.to_string())?.decrypt_padded_mut::<Pkcs7>(&mut buffer).map_err(|error| error.to_string())?;
    bytes.extend_from_slice(decrypted);
    Ok(())
}

#[tauri::command]
fn cancel_download(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    if let Some(cancel) = state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?.get(&id) {
        let cancel = cancel.clone();
        tauri::async_runtime::spawn(async move { *cancel.lock().await = true; });
    }
    Ok(())
}

#[tauri::command]
fn clear_media(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.media.lock().map_err(|_| "媒体列表锁定失败".to_string())?.clear();
    Ok(())
}

// ===== LLM 批量剧集分析与下载 =====

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchProgress {
    stage: String, // "analyzing" | "capturing" | "done" | "error"
    message: String,
    current: Option<u64>,
    total: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Episode {
    title: String,
    url: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeResult {
    episodes: Vec<Episode>,
    steps: Vec<String>,
}

const EPISODE_SYSTEM_PROMPT: &str = r#"你是视频站点分析助手。用户会给你一个网页的信息（URL、标题、链接列表，格式 text|href）。你的任务是找出所有"剧集播放页"的链接。

只输出一个 JSON 对象，不要输出任何其他文字：
- 如果链接列表已包含剧集播放页：{"action":"done","episodes":[{"title":"第1集","url":"绝对URL"},...]}
- 如果需要先打开某个页面才能看到剧集列表（例如当前页是详情页、首页或需要进入播放列表页）：{"action":"open","url":"要打开的页面URL","reason":"原因"}
- 如果确实找不到剧集链接：{"action":"fail","reason":"原因"}

规则：
1. episodes 里的 url 必须来自提供的链接列表（把相对路径转为绝对路径），不要编造 URL
2. title 优先用链接文本（如"第1集"），没有则用 URL 最后一段
3. 按集数/顺序排列，尽量覆盖全部集数
4. 只输出 JSON"#;

/// Truncate long strings for log readability while keeping enough context to debug.
fn truncate_for_log(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len { text.to_string() }
    else { format!("{}…(共{}字符)", text.chars().take(max_len).collect::<String>(), text.chars().count()) }
}

async fn llm_chat(settings: &AppSettings, system: &str, user: &str) -> Result<String, String> {
    if settings.llm_api_url.is_empty() || settings.llm_model.is_empty() {
        debug_log("[llm_chat] missing llm_api_url or llm_model in settings");
        return Err("请先在设置中配置 LLM API 地址和模型".to_string());
    }
    let endpoint = if settings.llm_api_url.ends_with("/chat/completions") {
        settings.llm_api_url.clone()
    } else {
        format!("{}/chat/completions", settings.llm_api_url.trim_end_matches('/'))
    };
    debug_log(&format!("[llm_chat] endpoint={endpoint} model={} user_len={}", settings.llm_model, user.len()));
    debug_log(&format!("[llm_chat] user message: {}", truncate_for_log(user, 2000)));
    let mut headers = header::HeaderMap::new();
    if !settings.llm_api_key.is_empty() {
        headers.insert(header::AUTHORIZATION, header::HeaderValue::from_str(&format!("Bearer {}", settings.llm_api_key)).map_err(|error| error.to_string())?);
    }
    let client = Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(120))
        .build().map_err(|error| error.to_string())?;
    let body = serde_json::json!({
        "model": settings.llm_model,
        "temperature": 0.2,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let response = match client.post(&endpoint).json(&body).send().await {
        Ok(response) => response,
        Err(error) => { debug_log(&format!("[llm_chat] request failed: {error}")); return Err(format!("LLM 请求失败: {error}")); }
    };
    let status = response.status();
    debug_log(&format!("[llm_chat] response status: {status}"));
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        debug_log(&format!("[llm_chat] error body: {}", truncate_for_log(&text, 2000)));
        return Err(format!("LLM 返回 {status}: {text}"));
    }
    let raw_text = response.text().await.map_err(|error| { debug_log(&format!("[llm_chat] failed to read response body: {error}")); format!("LLM 响应读取失败: {error}") })?;
    debug_log(&format!("[llm_chat] raw response: {}", truncate_for_log(&raw_text, 2000)));
    let payload: serde_json::Value = serde_json::from_str(&raw_text).map_err(|error| { debug_log(&format!("[llm_chat] JSON parse failed: {error}")); format!("LLM 响应解析失败: {error}") })?;
    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .map(|content| content.to_string())
        .ok_or_else(|| "LLM 响应缺少 content".to_string())?;
    debug_log(&format!("[llm_chat] content: {}", truncate_for_log(&content, 2000)));
    Ok(content)
}

/// Extract the first JSON object from LLM output (handles ```json fences and prose around it).
fn extract_llm_json(text: &str) -> Option<serde_json::Value> {
    let cleaned = if let Some(start) = text.find('{') {
        let Some(end) = text.rfind('}') else { debug_log("[extract_llm_json] no closing brace found"); return None; };
        &text[start..=end]
    } else { debug_log("[extract_llm_json] no opening brace found"); return None; };
    match serde_json::from_str(cleaned) {
        Ok(value) => Some(value),
        Err(error) => { debug_log(&format!("[extract_llm_json] JSON parse failed: {error}; cleaned={}", truncate_for_log(cleaned, 1000))); None }
    }
}

/// After a successful goto(), return the page's current URL if it differs from the requested one (i.e. a redirect happened).
async fn detect_redirect(page: &chromiumoxide::Page, requested_url: &str) -> Option<String> {
    match page.url().await {
        Ok(Some(final_url)) if final_url != requested_url => Some(final_url),
        _ => None,
    }
}

/// Open a page in the headless browser and return (title, "text|href" link list, redirected_to) for LLM analysis.
async fn fetch_page_context(browser: &Arc<Browser>, url: &str) -> Result<(String, String, Option<String>), String> {
    debug_log(&format!("[fetch_page_context] url={url}"));
    let page = browser.new_page("about:blank").await.map_err(|error| { debug_log(&format!("[fetch_page_context] new_page failed: {error}")); format!("创建页面失败: {error}") })?;
    if let Err(error) = page.enable_stealth_mode().await { debug_log(&format!("[fetch_page_context] enable_stealth_mode failed: {error}")); }
    page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default()).await.map_err(|error| error.to_string())?;
    let goto = page.goto(url);
    match tokio::time::timeout(std::time::Duration::from_secs(30), goto).await {
        Ok(Ok(_)) => { debug_log("[fetch_page_context] navigation completed"); wait_for_challenge_page(&page, std::time::Duration::from_secs(12)).await; }
        Ok(Err(error)) => { debug_log(&format!("[fetch_page_context] navigation error: {error}")); return Err(format!("页面导航失败: {error}")); }
        Err(_) => { debug_log("[fetch_page_context] navigation timed out (30s), proceeding anyway"); } // proceed even if navigation is slow — the DOM may still be usable
    }
    // Many listing/player pages redirect via JS (`location.href = ...`) or a meta-refresh
    // *after* the initial document has loaded, not as a plain HTTP 301/302 — so goto() alone
    // can resolve before that redirect fires. Extraction runs a little after goto returns
    // (evaluate() itself takes a beat), so re-check the URL here, once the page has had a
    // chance to settle, rather than immediately after goto.
    let extract_js = r#"(() => {
        const links = [];
        const seen = new Set();
        for (const a of document.querySelectorAll('a[href]')) {
            let href; try { href = new URL(a.href, location.href).href; } catch { continue; }
            if (!href.startsWith('http')) continue;
            const text = (a.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60);
            const key = href + '|' + text;
            if (seen.has(key)) continue;
            seen.add(key);
            links.push(text + '|' + href);
        }
        return JSON.stringify({ title: document.title, links: links.slice(0, 600), href: location.href });
    })()"#;
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), page.evaluate(extract_js)).await
        .map_err(|_| { debug_log("[fetch_page_context] extraction timed out (10s)"); "页面内容提取超时".to_string() })?
        .map_err(|error| { debug_log(&format!("[fetch_page_context] extraction failed: {error}")); format!("页面内容提取失败: {error}") })?;
    let value: String = result.into_value().map_err(|error| error.to_string())?;
    let redirected_to = detect_redirect(&page, url).await;
    let _ = page.close().await;
    let parsed: serde_json::Value = serde_json::from_str(&value).map_err(|error| error.to_string())?;
    let title = parsed["title"].as_str().unwrap_or_default().to_string();
    let links = parsed["links"].as_array().cloned().unwrap_or_default()
        .iter().filter_map(|link| link.as_str()).collect::<Vec<_>>().join("\n");
    // Prefer location.href (reflects any JS redirect the page made while rendering) over the
    // CDP-reported page.url(), falling back to the latter if the script result is unusable.
    let final_href = parsed["href"].as_str().filter(|href| !href.is_empty()).map(|href| href.to_string());
    let redirected_to = match final_href {
        Some(href) if href != url => Some(href),
        Some(_) => None,
        None => redirected_to,
    };
    debug_log(&format!("[fetch_page_context] title={title:?} link_count={} redirected_to={:?}", links.lines().count(), redirected_to));
    Ok((title, links, redirected_to))
}

/// ReAct loop: open pages and let the LLM find episode play-page URLs.
#[tauri::command]
async fn analyze_episodes(app: AppHandle, state: tauri::State<'_, AppState>, url: String) -> Result<AnalyzeResult, String> {
    debug_log(&format!("[analyze_episodes] start url={url}"));
    if !is_allowed_url(&url) { debug_log(&format!("[analyze_episodes] rejected invalid url: {url}")); return Err("请输入有效的 http 或 https 网页地址".to_string()); }
    let settings = state.settings.read().await.clone();
    ensure_headless_browser(&app, &state, &settings).await?;
    let browser = state.headless_browser.lock().await.clone().ok_or("Chromium 初始化失败")?;

    let mut steps = Vec::new();
    let mut current_url = url;
    let mut last_error: Option<String> = None;
    for round in 1..=6u32 {
        debug_log(&format!("[analyze_episodes] round {round} current_url={current_url}"));
        let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("第 {round} 轮：打开页面并提取链接…"), current: Some(round as u64), total: Some(6) });
        let (title, links, redirected_to) = fetch_page_context(&browser, &current_url).await
            .map_err(|error| { debug_log(&format!("[analyze_episodes] round {round} fetch_page_context failed: {error}")); format!("第 {round} 轮打开 {current_url} 失败：{error}") })?;
        if links.is_empty() { debug_log(&format!("[analyze_episodes] round {round} no links extracted")); return Err(format!("第 {round} 轮：页面「{title}」（{current_url}）没有提取到任何链接")); }
        if let Some(final_url) = redirected_to {
            steps.push(format!("检测到页面跳转：{current_url} → {final_url}"));
            current_url = final_url;
        }
        steps.push(format!("第 {round} 轮：打开 {current_url}（标题：{title}，{lines} 个链接）", lines = links.lines().count()));

        let user_message = format!("页面 URL: {current_url}\n页面标题: {title}\n\n链接列表（格式 文本|URL）：\n{links}");
        let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("第 {round} 轮：LLM 分析 {lines} 个链接…", lines = links.lines().count()), current: Some(round as u64), total: Some(6) });
        let reply = llm_chat(&settings, EPISODE_SYSTEM_PROMPT, &user_message).await
            .map_err(|error| { debug_log(&format!("[analyze_episodes] round {round} llm_chat failed: {error}")); format!("第 {round} 轮 AI 分析失败：{error}") })?;
        let Some(json) = extract_llm_json(&reply) else {
            let message = format!("第 {round} 轮：LLM 输出无法解析为 JSON，重试中");
            debug_log(&format!("[analyze_episodes] round {round} unparsable LLM reply: {}", truncate_for_log(&reply, 1000)));
            steps.push(message.clone());
            last_error = Some(format!("第 {round} 轮 AI 分析失败：LLM 输出无法解析为 JSON"));
            continue;
        };
        debug_log(&format!("[analyze_episodes] round {round} action={:?}", json["action"].as_str()));
        match json["action"].as_str().unwrap_or_default() {
            "done" => {
                let episodes = json["episodes"].as_array().cloned().unwrap_or_default().iter().filter_map(|item| {
                    let url = item["url"].as_str()?.to_string();
                    if !is_allowed_url(&url) { return None; }
                    let title = item["title"].as_str().map(|value| value.to_string())
                        .unwrap_or_else(|| url.rsplit('/').next().unwrap_or("剧集").to_string());
                    Some(Episode { title, url })
                }).collect::<Vec<_>>();
                if episodes.is_empty() { debug_log(&format!("[analyze_episodes] round {round} action=done but 0 valid episodes")); return Err(format!("第 {round} 轮：AI 判定完成但没有找到任何有效的剧集链接")); }
                debug_log(&format!("[analyze_episodes] done: {} episodes found", episodes.len()));
                steps.push(format!("分析完成：找到 {} 集", episodes.len()));
                let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("找到 {} 集", episodes.len()), current: None, total: None });
                return Ok(AnalyzeResult { episodes, steps });
            }
            "open" => {
                let next = json["url"].as_str().unwrap_or_default().to_string();
                let reason = json["reason"].as_str().unwrap_or_default().to_string();
                if !is_allowed_url(&next) { debug_log(&format!("[analyze_episodes] round {round} action=open invalid url: {next}")); return Err(format!("第 {round} 轮：AI 要求打开非法地址: {next}")); }
                steps.push(format!("第 {round} 轮：需要打开 {next}（{reason}）"));
                current_url = next;
            }
            _ => {
                let reason = json["reason"].as_str().unwrap_or("LLM 未能识别页面结构");
                debug_log(&format!("[analyze_episodes] round {round} action=fail/unknown reason={reason}"));
                return Err(format!("第 {round} 轮 AI 判定失败：{reason}"));
            }
        }
    }
    debug_log("[analyze_episodes] exhausted 6 rounds without resolution");
    Err(last_error.unwrap_or_else(|| "分析轮数已达上限（6 轮），未能确定剧集列表".to_string()))
}

/// Open one episode page in the headless browser and capture its media (playlist) URL and any redirect.
async fn capture_episode_media(browser: &Arc<Browser>, page_url: &str) -> Result<(Option<String>, Option<String>), String> {
    debug_log(&format!("[capture_episode_media] page_url={page_url}"));
    let page = browser.new_page("about:blank").await.map_err(|error| { debug_log(&format!("[capture_episode_media] new_page failed: {error}")); format!("创建页面失败: {error}") })?;
    if let Err(error) = page.enable_stealth_mode().await { debug_log(&format!("[capture_episode_media] enable_stealth_mode failed: {error}")); }
    page.evaluate_on_new_document("document.addEventListener('DOMContentLoaded', () => document.querySelectorAll('video').forEach(video => { video.muted = true; video.play().catch(() => {}); }));").await.map_err(|error| error.to_string())?;
    page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default()).await.map_err(|error| error.to_string())?;
    let mut requests = page.event_listener::<chromiumoxide::cdp::browser_protocol::network::EventRequestWillBeSent>().await.map_err(|error| error.to_string())?;
    let mut responses = page.event_listener::<chromiumoxide::cdp::browser_protocol::network::EventResponseReceived>().await.map_err(|error| error.to_string())?;

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    let request_sender = sender.clone();
    tauri::async_runtime::spawn(async move {
        use futures_util::StreamExt;
        loop {
            tokio::select! {
                response = responses.next() => {
                    let Some(response) = response else { break; };
                    let url = response.response.url.clone();
                    let mime = response.response.mime_type.to_ascii_lowercase();
                    if classify_media(&url).as_deref() == Some("m3u8") || classify_media(&url).as_deref() == Some("mpd") || mime.contains("mpegurl") || mime.contains("dash+xml") {
                        let _ = sender.send(url);
                    }
                }
                request = requests.next() => {
                    let Some(request) = request else { break; };
                    let url = request.request.url.clone();
                    let is_segment = url.ends_with(".ts") || url.ends_with(".m4s");
                    if !is_segment && matches!(classify_media(&url).as_deref(), Some("m3u8") | Some("mpd")) {
                        let _ = request_sender.send(url);
                    }
                }
            }
        }
    });

    let goto = page.goto(page_url);
    match tokio::time::timeout(std::time::Duration::from_secs(30), goto).await {
        Ok(Ok(_)) => { debug_log("[capture_episode_media] navigation completed"); wait_for_challenge_page(&page, std::time::Duration::from_secs(20)).await; }
        Ok(Err(error)) => { debug_log(&format!("[capture_episode_media] navigation error: {error}")); let _ = page.close().await; return Err(format!("页面导航失败: {error}")); }
        Err(_) => { debug_log("[capture_episode_media] navigation timed out (30s), proceeding anyway"); }
    }
    // Collect media for up to 15s; return as soon as a playlist shows up.
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut first: Option<String> = None;
        while let Some(url) = receiver.recv().await {
            if matches!(classify_media(&url).as_deref(), Some("m3u8") | Some("mpd")) { return Some(url); }
            if first.is_none() { first = Some(url); }
        }
        first
    }).await.unwrap_or(None);
    // Re-check the URL only after media collection finishes (not right after goto): episode
    // pages commonly redirect via JS (`location.href = ...`) a moment after the initial
    // document loads, which goto() alone would miss.
    let redirected_to = detect_redirect(&page, page_url).await;
    let _ = page.close().await;
    debug_log(&format!("[capture_episode_media] result={result:?} redirected_to={redirected_to:?}"));
    Ok((result, redirected_to))
}

/// Probe every episode page, capture its media URL, and queue downloads.
#[tauri::command]
async fn batch_download(app: AppHandle, state: tauri::State<'_, AppState>, episodes: Vec<Episode>) -> Result<(), String> {
    debug_log(&format!("[batch_download] starting with {} episodes", episodes.len()));
    if episodes.is_empty() { return Err("剧集列表为空".to_string()); }
    let settings = state.settings.read().await.clone();
    let output_dir = app_download_dir(&app, &settings)?;
    ensure_headless_browser(&app, &state, &settings).await?;
    let browser = state.headless_browser.lock().await.clone().ok_or("Chromium 初始化失败")?;

    let total = episodes.len() as u64;
    let mut captured = 0u64;
    for (index, episode) in episodes.iter().enumerate() {
        let _ = app.emit("batch-progress", BatchProgress {
            stage: "capturing".into(),
            message: format!("探测「{}」", episode.title),
            current: Some(index as u64 + 1),
            total: Some(total),
        });
        match capture_episode_media(&browser, &episode.url).await {
            Ok((Some(media_url), redirected_to)) => {
                captured += 1;
                // If the episode page redirected (e.g. through an auth/CDN hop), the media's
                // Referer must be the real page that ended up loading it — otherwise some CDNs
                // reject the request as a referer mismatch. Same applies to the source_url we
                // record for this captured item.
                let effective_page_url = redirected_to.clone().unwrap_or_else(|| episode.url.clone());
                let redirect_note = redirected_to.as_deref().map(|u| format!("（页面跳转至 {u}）")).unwrap_or_default();
                debug_log(&format!("[batch] episode '{}' -> {media_url}{redirect_note}", episode.title));
                let _ = emit_captured(&app, &state, media_url.clone(), effective_page_url.clone(), None);
                let payload = DownloadRequest {
                    id: format!("batch-{}-{}", chrono_time(), index),
                    url: media_url,
                    filename: format!("{}.ts", episode.title),
                    referer: Some(effective_page_url),
                };
                let cancel = Arc::new(tokio::sync::Mutex::new(false));
                state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?.insert(payload.id.clone(), cancel.clone());
                let app_handle = app.clone();
                let task_id = payload.id.clone();
                let task_dir = output_dir.clone();
                let task_name = episode.title.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = download_media(app_handle.clone(), payload, task_dir, cancel).await {
                        let _ = app_handle.emit("download-progress", DownloadUpdate { id: task_id, filename: task_name, status: "failed".into(), received: 0, total: None, error: Some(error), unit: None, received_bytes: None, total_bytes: None });
                    }
                });
            }
            Ok((None, _)) => {
                let message = format!("「{}」未探测到播放地址（可能是页面未播放或未使用 m3u8/mpd 格式）", episode.title);
                debug_log(&format!("[batch] {message}"));
                let _ = app.emit("batch-progress", BatchProgress { stage: "capturing".into(), message, current: Some(index as u64 + 1), total: Some(total) });
            }
            Err(error) => {
                let message = format!("「{}」探测失败：{error}", episode.title);
                debug_log(&format!("[batch] {message}"));
                let _ = app.emit("batch-progress", BatchProgress { stage: "capturing".into(), message, current: Some(index as u64 + 1), total: Some(total) });
            }
        }
    }
    let _ = app.emit("batch-progress", BatchProgress { stage: "done".into(), message: format!("完成：{captured}/{total} 集已加入下载", captured = captured, total = total), current: Some(captured), total: Some(total) });
    Ok(())
}

#[tauri::command]
async fn analyze_page_links(app: AppHandle, state: tauri::State<'_, AppState>, url: String, html_or_links: String) -> Result<AnalyzeResult, String> {
    let settings = state.settings.read().await.clone();
    let lines = html_or_links.lines().count();
    let user_message = format!("页面 URL: {url}\n\n链接列表（格式 文本|URL）：\n{html_or_links}");
    let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("LLM 正在分析 {lines} 个链接…"), current: Some(1), total: Some(1) });
    let reply = llm_chat(&settings, EPISODE_SYSTEM_PROMPT, &user_message).await
        .map_err(|error| format!("AI 分析失败：{error}"))?;
    let Some(json) = extract_llm_json(&reply) else {
        return Err("LLM 输出无法解析为 JSON".to_string());
    };
    match json["action"].as_str().unwrap_or_default() {
        "done" => {
            let episodes = json["episodes"].as_array().cloned().unwrap_or_default().iter().filter_map(|item| {
                let url = item["url"].as_str()?.to_string();
                if !is_allowed_url(&url) { return None; }
                let title = item["title"].as_str().map(|value| value.to_string())
                    .unwrap_or_else(|| url.rsplit('/').next().unwrap_or("剧集").to_string());
                Some(Episode { title, url })
            }).collect::<Vec<_>>();
            if episodes.is_empty() { return Err("AI 判定完成但没有找到任何有效的剧集链接".to_string()); }
            let steps = vec![format!("从页面提取到 {} 个链接，AI 成功解析出 {} 集", lines, episodes.len())];
            let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("找到 {} 集", episodes.len()), current: None, total: None });
            Ok(AnalyzeResult { episodes, steps })
        }
        _ => {
            let reason = json["reason"].as_str().unwrap_or("未能从提供的链接中识别出剧集");
            Err(format!("AI 分析失败：{reason}"))
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            debug_log(&format!("[setup] settings_path: {}", settings_path.display()));
            let read_result = std::fs::read(&settings_path);
            debug_log(&format!("[setup] read ok: {} err: {:?}", read_result.is_ok(), read_result.as_ref().err()));
            let settings: AppSettings = read_result
                .ok()
                .and_then(|contents| {
                    let parsed = serde_json::from_slice::<AppSettings>(&contents);
                    debug_log(&format!("[setup] parse ok: {} err: {:?}", parsed.is_ok(), parsed.as_ref().err()));
                    parsed.ok()
                })
                .unwrap_or_default();
            debug_log(&format!("[setup] loaded llmApiUrl: {:?}", settings.llm_api_url));
            app.manage(AppState::new(settings, settings_path));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![open_page, open_visible_page, capture_media, report_page_links, get_media, get_settings, save_settings, start_download, cancel_download, clear_media, analyze_episodes, analyze_page_links, batch_download])
        .run(tauri::generate_context!("Tauri.toml"))
        .expect("Video Scout failed to start");
}
