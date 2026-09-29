use aes::Aes128;
use cbc::Decryptor;
use chromiumoxide::{browser::{Browser, BrowserConfig}, fetcher::{BrowserFetcher, BrowserFetcherOptions}};
use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use reqwest::{header, Client, Url};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, VecDeque}, path::PathBuf, sync::{Arc, Mutex}};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::RwLock;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

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
    #[serde(default)]
    page_title: String,
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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    id: String,
    url: String,
    filename: String,
    referer: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRecord {
    id: String,
    url: String,
    filename: String,
    #[serde(default)]
    referer: Option<String>,
    #[serde(default)]
    subdir: Option<String>,
    status: String,
    #[serde(default)]
    received: u64,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    received_bytes: Option<u64>,
    #[serde(default)]
    total_bytes: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    created_at: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    browser_source: String,
    local_chromium_path: String,
    #[serde(default)]
    download_dir: String,
    #[serde(default)]
    llm_api_url: String,
    #[serde(default)]
    llm_api_key: String,
    #[serde(default)]
    llm_model: String,
    #[serde(default = "default_max_concurrent")]
    max_concurrent: u32,
    #[serde(default)]
    lan_enabled: bool,
    #[serde(default = "default_lan_port")]
    lan_port: u32,
}

fn default_max_concurrent() -> u32 { 3 }
fn default_lan_port() -> u32 { 8688 }

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            browser_source: "managed".into(),
            local_chromium_path: String::new(),
            download_dir: String::new(),
            llm_api_url: String::new(),
            llm_api_key: String::new(),
            llm_model: String::new(),
            max_concurrent: 3,
            lan_enabled: false,
            lan_port: 8688,
        }
    }
}

struct LanServer {
    port: u32,
    server: Arc<tiny_http::Server>,
}

struct QueuedDownload {
    payload: DownloadRequest,
    output_dir: PathBuf,
    resume: Option<(u64, u64)>,
}

struct AppState {
    media: Mutex<HashMap<String, MediaItem>>,
    downloads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>,
    download_queue: Mutex<VecDeque<QueuedDownload>>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    tasks_path: PathBuf,
    settings: RwLock<AppSettings>,
  settings_path: PathBuf,
    lan_server: Mutex<Option<Arc<LanServer>>>,
    headless_page_url: Mutex<String>,
    headless_browser: tokio::sync::Mutex<Option<Arc<Browser>>>,
}

impl AppState {
   fn new(settings: AppSettings, settings_path: PathBuf, tasks: HashMap<String, TaskRecord>, tasks_path: PathBuf) -> Self {
        Self {
            media: Mutex::default(),
            downloads: Mutex::default(),
            download_queue: Mutex::default(),
            tasks: Mutex::new(tasks),
            tasks_path,
            settings: RwLock::new(settings),
            settings_path,
            lan_server: Mutex::default(),
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

fn save_tasks_locked(tasks: &HashMap<String, TaskRecord>, path: &PathBuf) {
    let mut records: Vec<&TaskRecord> = tasks.values().collect();
    records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    match serde_json::to_vec_pretty(&records) {
        Ok(json) => { if let Err(error) = std::fs::write(path, json) { debug_log(&format!("[tasks] save failed: {error}")); } }
        Err(error) => debug_log(&format!("[tasks] serialize failed: {error}")),
    }
}

fn emit_download(app: &AppHandle, update: DownloadUpdate) {
    // 同步更新持久化任务记录：状态变化必落盘；下载中仅 HLS 分片边界落盘（直链逐 chunk 太频繁）
    let state = app.state::<AppState>();
    if let Ok(mut tasks) = state.tasks.lock() {
        if let Some(record) = tasks.get_mut(&update.id) {
            record.status = update.status.clone();
            record.filename = update.filename.clone();
            record.received = update.received;
            record.total = update.total;
            record.unit = update.unit.clone();
            record.received_bytes = update.received_bytes;
            record.total_bytes = update.total_bytes;
            record.error = update.error.clone();
            if update.status != "downloading" || update.unit.as_deref() == Some("segments") {
                save_tasks_locked(&tasks, &state.tasks_path);
            }
        }
    }
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

fn emit_captured(app: &AppHandle, state: &AppState, url: String, source_url: String, mime_type: Option<&str>, page_title: Option<&str>) -> Result<(), String> {
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
    let item = MediaItem { id: url.clone(), url: url.clone(), kind: kind.clone(), source_url, captured_at: chrono_time(), page_title: page_title.unwrap_or_default().to_string() };
    debug_log(&format!("[emit_captured] inserting: {url} kind={kind}"));
    state.media.lock().map_err(|_| "媒体列表锁定失败".to_string())?.insert(item.id.clone(), item.clone());
    let emit_result = app.emit("media-found", item);
    debug_log(&format!("[emit_captured] emit result: {:?}", emit_result.is_ok()));
    // 探测成功后自动关闭可视探测窗口（无头模式没有该窗口，get 返回 None）
    if let Some(window) = app.get_webview_window("browser") {
        debug_log("[emit_captured] media found, closing browser window");
        let _ = window.close();
    }
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
    debug_log(&format!("[browser_executable] browser_source={:?} local_chromium_path={:?}", settings.browser_source, settings.local_chromium_path));
    if settings.browser_source == "local" {
        let path = PathBuf::from(settings.local_chromium_path.trim());
        if settings.local_chromium_path.trim().is_empty() {
            return Err("请先设置本机 Chromium 可执行文件路径".into());
        }
        if !path.is_file() {
            return Err("指定的 Chromium 可执行文件不存在".into());
        }
        debug_log(&format!("[browser_executable] using local Chrome: {}", path.display()));
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
                        let _ = emit_captured(&media_app, &media_app.state::<AppState>(), response.response.url.clone(), source_url, Some(&response.response.mime_type), None);
                    }
                }
                request = requests.next() => {
                                let Some(request) = request else { break; };
                                let req_url = &request.request.url;
                                let is_ts = req_url.ends_with(".ts") || req_url.ends_with(".m4s");
                                if classify_media(req_url).is_some() && !is_ts {
                        debug_log(&format!("[listener] media request: {}", request.request.url));
                        let source_url = media_app.state::<AppState>().headless_page_url.lock().map(|url| url.clone()).unwrap_or_default();
                        let _ = emit_captured(&media_app, &media_app.state::<AppState>(), request.request.url.clone(), source_url, None, None);
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
            let source_url = state.headless_page_url.lock().map(|u| u.clone()).unwrap_or_default();
            let scan_script = r#"(() => {
                const results = [];
                const add = (url) => { try { const u = new URL(url, location.href); if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(u.href)) results.push(u.href); } catch {} };
                const addFromParams = (href) => { try { const u = new URL(href); for (const v of u.searchParams.values()) { try { const mu = new URL(v, href); if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(mu.href)) results.push(mu.href); } catch {} } } catch {} };
                add(location.href); addFromParams(location.href);
                document.querySelectorAll('video, source').forEach(el => add(el.currentSrc || el.src));
                document.querySelectorAll('iframe').forEach(f => { try { add(f.contentWindow.location.href); addFromParams(f.contentWindow.location.href); } catch {} });
                const raw = document.documentElement?.innerHTML || '';
                const text = raw.replace(/\\\//g, '/');
                const re = /(?:https?:)?[^\s"'<>\\]+\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\?[^\s"'<>\\]*)?/ig;
                for (const m of text.matchAll(re)) add(m[0]);
                return [...new Set(results)];
            })()"#;
            if let Ok(Ok(value)) = tokio::time::timeout(std::time::Duration::from_secs(5), page.evaluate(scan_script)).await {
                if let Ok(urls) = value.into_value::<Vec<String>>() {
                    for media_url in urls {
                        debug_log(&format!("[open_headless_page] html scan found: {media_url}"));
                        let _ = emit_captured(&app, state, media_url, source_url.clone(), None, None);
                    }
                }
            }
            // 网络监听和 DOM 扫描都会漏掉藏在播放器配置里的分享链接（无媒体扩展名，需二次请求解析），
            // 用与可视窗口相同的 Rust 侧静态解析兜底
            let (rust_found, page_title) = scan_page_html_for_media(&source_url).await;
            for media_url in rust_found {
                debug_log(&format!("[open_headless_page] rust scan found: {media_url}"));
                let _ = emit_captured(&app, state, media_url, source_url.clone(), None, page_title.as_deref());
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

async fn open_visible_page_inner(app: AppHandle, _state: &AppState, url: String) -> Result<(), String> {
    let parsed = Url::parse(&url).map_err(|error| error.to_string())?;
    // Reuse the existing "browser" window: close() is async on macOS, so close+rebuild
    // races and intermittently fails with "label already exists", which the user sees
    // as a probe error.
    let mut skip_build = false;
    if let Some(window) = app.get_webview_window("browser") {
        match window.navigate(parsed.clone()) {
            Ok(()) => {
                debug_log("[open_visible_page] reusing existing window via navigate");
                skip_build = true;
            }
            Err(error) => {
                debug_log(&format!("[open_visible_page] navigate failed, rebuilding window: {error}"));
                let _ = window.close();
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
    }
    if !skip_build {
        let initialization_script = r#"
          (() => {
            const vlog = (msg) => { try { window.__TAURI_INTERNALS__?.invoke('visible_log', { message: '[' + location.href.slice(0, 80) + '] ' + msg }).catch(() => {}); } catch {} };
            vlog('init script loaded');
            const isMedia = (url, type = '') => /\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(url || '') || /mpegurl|dash\+xml|video\//i.test(type || '');
            const send = (url, type = '') => {
              try {
                const absoluteUrl = new URL(url, location.href).href;
                if (!isMedia(absoluteUrl, type)) return;
                vlog('media found: ' + absoluteUrl);
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
            const scanHtmlForMedia = () => {
              try {
                const raw = document.documentElement?.innerHTML || '';
                const text = raw.replace(/\\\//g, '/');
                vlog('scanHtmlForMedia: html length=' + text.length);
                const re = /(?:https?:)?[^\s\"'<>\\]+\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\?[^\s\"'<>\\]*)?/ig;
                var matchCount = 0;
                for (const match of text.matchAll(re)) {
                  matchCount++;
                  try { send(new URL(match[0], location.href).href); } catch { send(match[0]); }
                }
                vlog('scanHtmlForMedia: found ' + matchCount + ' regex matches');
                try { const params = new URL(location.href).searchParams; for (const v of params.values()) { if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(v)) send(new URL(v, location.href).href); } } catch {}
              } catch (e) { vlog('scanHtmlForMedia error: ' + e); }
            };
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
                vlog('extractAndSendLinks: found ' + links.length + ' links, title=' + document.title);
                const invoke = window.__TAURI_INTERNALS__?.invoke;
                if (typeof invoke === 'function' && links.length > 0) {
                  invoke('report_page_links', { payload: { url: location.href, title: document.title, links: links.slice(0, 600) } }).catch(() => {});
                }
              } catch (e) { vlog('extractAndSendLinks error: ' + e); }
            };
            var lastLinkCount = 0;
            var debounceTimer = null;
            const extractDebounced = () => {
              if (debounceTimer) clearTimeout(debounceTimer);
              debounceTimer = setTimeout(() => { debounceTimer = null; extractAndSendLinks(); }, 3000);
            };
            new MutationObserver((records) => {
              inspect();
              if (!debounceTimer) {
                for (const r of records) {
                  for (const n of r.addedNodes) {
                    if (n.nodeType === 1 && (n.tagName === 'A' || n.querySelector?.('a'))) {
                      const cur = document.querySelectorAll('a[href]').length;
                      if (cur !== lastLinkCount) { lastLinkCount = cur; extractDebounced(); }
                      return;
                    }
                  }
                }
              }
            }).observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ['src'] });
            document.addEventListener('play', inspect, true);
            window.addEventListener('load', () => { vlog('window load event'); setTimeout(() => { inspect(); scanHtmlForMedia(); }, 2000); setTimeout(extractAndSendLinks, 5000); });
            vlog('init script executing at ' + location.href);
            inspect();
            scanHtmlForMedia();
          })();
        "#;
        WebviewWindowBuilder::new(&app, "browser", WebviewUrl::External(parsed))
            .title("页面探测器")
            .inner_size(1100.0, 760.0)
            .visible(true)
            .initialization_script_for_all_frames(initialization_script)
            .on_navigation(|_| true)
            .on_page_load(|webview, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    let page_url = payload.url().to_string();
                    debug_log(&format!("[on_page_load] fired for url={page_url}"));
                    let script = format!(
                        r#"(() => {{
                          const candidates = [];
                          const add = (url) => {{ try {{ const u = new URL(url, location.href); if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(u.href)) candidates.push(u.href); }} catch {{}} }};
                          const addFromParams = (href) => {{ try {{ const u = new URL(href); for (const v of u.searchParams.values()) {{ try {{ const mu = new URL(v, href); if (/\.(m3u8|mpd|mp4|m4v|mov|webm|flv)(?:$|[?#])/i.test(mu.href)) candidates.push(mu.href); }} catch {{}} }} }} catch {{}} }};
                          add(location.href);
                          addFromParams(location.href);
                          document.querySelectorAll('video, source').forEach((el) => add(el.currentSrc || el.src));
                          for (const frame of document.querySelectorAll('iframe')) {{ try {{ const furl = frame.contentWindow.location.href; add(furl); addFromParams(furl); }} catch {{}} }}
                          const raw = document.documentElement?.innerHTML || '';
                          const text = raw.replace(/\\\\\//g, '/');
                          for (const match of text.matchAll(/(?:https?:)?[^\\s\\"'<>\\\\]+\\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\\?[^\\s\\"'<>\\\\]*)?/ig)) add(match[0]);
                          for (const frame of document.querySelectorAll('iframe')) {{ try {{ const frameUrl = frame.contentWindow.location.href; const frameRaw = frame.contentDocument?.documentElement?.innerHTML || ''; const frameText = frameRaw.replace(/\\\\\//g, '/'); for (const match of frameText.matchAll(/(?:https?:)?[^\\s\\"'<>\\\\]+\\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\\?[^\\s\\"'<>\\\\]*)?/ig)) candidates.push(new URL(match[0], frameUrl).href); }} catch {{}} }}
                          try {{ window.__TAURI_INTERNALS__?.invoke('visible_log', {{ message: '[on_page_load] candidates=' + new Set(candidates).size + ' for ' + location.href }}).catch(() => {{}}); }} catch {{}}
                          for (const url of new Set(candidates)) window.__TAURI_INTERNALS__?.invoke('capture_media', {{ payload: {{ id: url, url, sourceUrl: {source_url:?} }} }}).catch(() => {{}});
                        }})();"#,
                        source_url = page_url
                    );
                    let _ = webview.eval(&script);
                }
            })
            .build()
            .map_err(|error| { debug_log(&format!("[open_visible_page] window build failed: {error}")); error.to_string() })?;
    }
    // Also scan the page HTML from Rust side (bypasses JS injection issues)
    let scan_url = url.clone();
    let scan_app = app.clone();
    tokio::spawn(async move {
        let (media_urls, page_title) = scan_page_html_for_media(&scan_url).await;
        let state = scan_app.state::<AppState>();
        let title_ref = page_title.as_deref();
        for media_url in media_urls {
            debug_log(&format!("[open_visible_page] rust scan found: {media_url}"));
            let _ = emit_captured(&scan_app, &state, media_url, scan_url.clone(), None, title_ref);
        }
    });
    Ok(())
}

/// Fetch a page's HTML from Rust and scan for media URLs (m3u8, mp4, etc.).
/// This bypasses JS injection issues in external webviews.
/// Also extracts the video title from `player_aaaa` JS variables when present.
async fn scan_page_html_for_media(url: &str) -> (Vec<String>, Option<String>) {
    let client = match Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(20))
        .build() {
        Ok(c) => c,
        Err(e) => { debug_log(&format!("[scan_page_html_for_media] client build failed: {e}")); return (vec![], None); }
    };
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => { debug_log(&format!("[scan_page_html_for_media] fetch failed: {e}")); return (vec![], None); }
    };
    if !response.status().is_success() {
        debug_log(&format!("[scan_page_html_for_media] status={}", response.status()));
        return (vec![], None);
    }
    let html = match response.text().await {
        Ok(t) => t,
        Err(e) => { debug_log(&format!("[scan_page_html_for_media] read body failed: {e}")); return (vec![], None); }
    };
    debug_log(&format!("[scan_page_html_for_media] html length={}", html.len()));

    // Unescape JSON-escaped strings (\/ → /) so that URLs inside JS variables
    // like `player_aaaa={"url":"https:\/\/...\/index.m3u8"}` can be matched.
    let unescaped = html.replace("\\/", "/");

    let mut results = Vec::new();
    let re = regex::Regex::new(r#"(?i)(?:https?:)?[^\s"'<>\\]+\.(?:m3u8|mpd|mp4|m4v|mov|webm|flv)(?:\?[^\s"'<>\\]*)?"#).unwrap();
    for cap in re.captures_iter(&unescaped) {
        let matched = cap.get(0).unwrap().as_str();
        let absolute = if matched.starts_with("//") {
            format!("https:{}", matched)
        } else if matched.starts_with("/") {
            if let Ok(base) = Url::parse(url) {
                format!("{}://{}{}", base.scheme(), base.host_str().unwrap_or(""), matched)
            } else {
                matched.to_string()
            }
        } else if matched.starts_with("http") {
            matched.to_string()
        } else {
            continue;
        };
        if !results.contains(&absolute) {
            debug_log(&format!("[scan_page_html_for_media] found: {absolute}"));
            results.push(absolute);
        }
    }

    // Extract video URL and title from `player_aaaa` JS variable (common on Chinese video sites).
    // The URL here is the real playable source, more reliable than regex-scanning the HTML.
    let (player_url, player_title) = extract_player_info(&unescaped);
    let resolved_url = if let Some(ref purl) = player_url {
        resolve_share_url(&client, purl, url).await.or_else(|| Some(purl.clone()))
    } else {
        None
    };
    if let Some(ref rurl) = resolved_url {
        if !results.contains(rurl) {
            debug_log(&format!("[scan_page_html_for_media] player url (priority): {rurl}"));
            results.insert(0, rurl.clone());
        }
    }
    let page_title = player_title.or_else(|| extract_page_title(&html));
    if let Some(ref title) = page_title {
        debug_log(&format!("[scan_page_html_for_media] page_title={title:?}"));
    }
    (results, page_title)
}

/// If a player URL is a "share" link (e.g. `https://vip.ffzy-plays.com/share/...`),
/// fetch the share page and extract the real m3u8 URL from its JS variables.
async fn resolve_share_url(client: &Client, url: &str, referer: &str) -> Option<String> {
    if !url.contains("/share/") {
        debug_log(&format!("[resolve_share_url] not a share URL: {url}"));
        return None;
    }
    debug_log(&format!("[resolve_share_url] resolving: {url}"));
    // The share host rejects or drops some direct requests; retry a few times
    // with the play page as Referer, like a real browser would send.
    let mut html: Option<String> = None;
    for attempt in 1..=3u8 {
        if attempt > 1 {
            debug_log(&format!("[resolve_share_url] retry attempt {attempt}"));
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
        let mut request = client.get(url);
        if !referer.is_empty() { request = request.header(header::REFERER, referer); }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                match response.text().await {
                    Ok(text) if !text.trim().is_empty() => { debug_log(&format!("[resolve_share_url] html length={}", text.len())); html = Some(text); break; }
                    Ok(_) => debug_log("[resolve_share_url] empty body"),
                    Err(e) => debug_log(&format!("[resolve_share_url] read body failed: {e}")),
                }
            }
            Ok(response) => debug_log(&format!("[resolve_share_url] status={}", response.status())),
            Err(e) => debug_log(&format!("[resolve_share_url] fetch failed: {e}")),
        }
    }
    let html = html?;
    let unescaped = html.replace("\\/", "/");
    let re = match regex::Regex::new(r#"(?i)const\s+url\s*=\s*"([^"]+\.m3u8[^"]*)""#) {
        Ok(r) => r,
        Err(e) => {
            debug_log(&format!("[resolve_share_url] regex build failed: {e}"));
            return None;
        }
    };
    let Some(cap) = re.captures(&unescaped) else {
        debug_log(&format!("[resolve_share_url] no m3u8 URL found in share page"));
        return None;
    };
    let path = cap.get(1)?.as_str();
    debug_log(&format!("[resolve_share_url] extracted path: {path}"));
    let base = Url::parse(url).ok()?;
    let resolved = if path.starts_with("http") {
        path.to_string()
    } else {
        base.join(path).ok()?.to_string()
    };
    debug_log(&format!("[resolve_share_url] resolved to: {resolved}"));
    Some(resolved)
}

/// Extract video URL and episode title from `player_aaaa` or similar JS variables.
/// Returns (video_url, episode_title).
fn extract_player_info(html: &str) -> (Option<String>, Option<String>) {
    let re = match regex::Regex::new(r#"(?i)var\s+player_\w+\s*=\s*(\{[^<]+\})\s*[;<]"#) {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let Some(cap) = re.captures(html) else { return (None, None); };
    let Some(json_str) = cap.get(1).map(|m| m.as_str()) else { return (None, None); };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else { return (None, None); };
    let video_url = parsed["url"].as_str().map(|s| s.to_string()).filter(|s| s.starts_with("http"));
    let episode_title = parsed["nid"].as_u64().map(|id| {
        let show_name = parsed["vod_data"]["vod_name"].as_str().unwrap_or("");
        if show_name.is_empty() {
            format!("第{id}集")
        } else {
            format!("{show_name} 第{id}集")
        }
    });
    if let Some(ref url) = video_url { debug_log(&format!("[extract_player_info] url={url}")); }
    if let Some(ref title) = episode_title { debug_log(&format!("[extract_player_info] title={title}")); }
    (video_url, episode_title)
}

fn extract_page_title(html: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)<title[^>]*>([^<]*)</title>").unwrap();
    let title = re.captures(html)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|t| !t.is_empty())?;
    let cleaned: String = title.chars().map(|c| {
        if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }
    }).collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

#[tauri::command]
async fn open_page(app: AppHandle, state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    if !is_allowed_url(&url) { return Err("请输入有效的 http 或 https 网页地址".to_string()); }
    let settings = state.settings.read().await.clone();
    debug_log(&format!("[open_page] url={url}"));
    open_headless_page(app, &state, &url, &settings).await
}

#[tauri::command]
fn capture_media(app: AppHandle, state: tauri::State<'_, AppState>, payload: CaptureRequest) -> Result<(), String> {
    debug_log(&format!("[capture_media] url={} source={}", payload.url, payload.source_url));
    if payload.id != payload.url { return Ok(()); }
    emit_captured(&app, &state, payload.url, payload.source_url, None, None)
}

#[tauri::command]
fn visible_log(message: String) -> Result<(), String> {
    debug_log(&format!("[visible] {message}"));
    Ok(())
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, AppState>) -> Result<AppSettings, String> {
    Ok(state.settings.read().await.clone())
}

#[tauri::command]
async fn save_settings(app: AppHandle, state: tauri::State<'_, AppState>, settings: AppSettings) -> Result<(), String> {
    if !matches!(settings.browser_source.as_str(), "managed" | "local") { return Err("无效的 Chromium 来源设置".into()); }
    let mut settings = settings;
    settings.max_concurrent = settings.max_concurrent.clamp(1, 16);
    settings.lan_port = settings.lan_port.clamp(1024, 65535);
    let settings_path = state.settings_path.clone();
    let settings_json = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    if let Some(parent) = settings_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
    }
    tokio::fs::write(settings_path, settings_json).await.map_err(|error| error.to_string())?;
    *state.settings.write().await = settings.clone();
    if settings.lan_enabled { start_lan_server(&app, settings.lan_port)?; } else { stop_lan_server(&app); }
    Ok(())
}

#[tauri::command]
async fn open_visible_page(app: AppHandle, state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    if !is_allowed_url(&url) { return Err("请输入有效的 http 或 https 网页地址".into()); }
    open_visible_page_inner(app, &state, url).await
}

fn chrono_time() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string()
}

#[tauri::command]
fn get_media(state: tauri::State<'_, AppState>) -> Result<Vec<MediaItem>, String> {
    Ok(state.media.lock().map_err(|_| "媒体列表锁定失败".to_string())?.values().cloned().collect())
}

/// 启动一个下载任务的后台协程（取消令牌须已在 downloads 中注册）。resume 为 (已完成单元数, 已完成字节数)。
fn spawn_with_token(app: &AppHandle, payload: DownloadRequest, output_dir: PathBuf, resume: Option<(u64, u64)>, cancel: Arc<tokio::sync::Mutex<bool>>) {
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let task_id = payload.id.clone();
        let task_name = payload.filename.clone();
        if let Err(error) = download_media(app_handle.clone(), payload, output_dir, cancel.clone(), resume).await {
            // 保留已下载进度，避免失败事件把分片数清零（影响后续断点续传）
            let state = app_handle.state::<AppState>();
            let (received, total, unit, received_bytes, total_bytes) = state.tasks.lock().ok()
                .and_then(|tasks| tasks.get(&task_id).map(|record| (record.received, record.total, record.unit.clone(), record.received_bytes, record.total_bytes)))
                .unwrap_or((0, None, None, None, None));
            emit_download(&app_handle, DownloadUpdate { id: task_id.clone(), filename: task_name, status: "failed".into(), received, total, error: Some(error), unit, received_bytes, total_bytes });
        }
        if let Ok(mut downloads) = app_handle.state::<AppState>().downloads.lock() { downloads.remove(&task_id); }
        drain_download_queue(&app_handle).await;
    });
}

/// 任务结束后补位：队列中有等待任务且并发未满时依次启动。
async fn drain_download_queue(app: &AppHandle) {
    let state = app.state::<AppState>();
    let max = state.settings.read().await.max_concurrent.max(1) as usize;
    loop {
        let (queued, cancel) = {
            let mut downloads = match state.downloads.lock() { Ok(downloads) => downloads, Err(_) => return };
            if downloads.len() >= max { return; }
            let next = match state.download_queue.lock() { Ok(mut queue) => queue.pop_front(), Err(_) => None };
            let Some(next) = next else { return };
            let cancel = Arc::new(tokio::sync::Mutex::new(false));
            downloads.insert(next.payload.id.clone(), cancel.clone());
            (next, cancel)
        };
        let (received, total, unit, received_bytes, total_bytes) = state.tasks.lock().ok()
            .and_then(|tasks| tasks.get(&queued.payload.id).map(|record| (record.received, record.total, record.unit.clone(), record.received_bytes, record.total_bytes)))
            .unwrap_or((0, None, None, None, None));
        emit_download(app, DownloadUpdate { id: queued.payload.id.clone(), filename: queued.payload.filename.clone(), status: "downloading".into(), received, total, error: None, unit, received_bytes, total_bytes });
        spawn_with_token(app, queued.payload, queued.output_dir, queued.resume, cancel);
    }
}

/// 提交下载：并发未满则立即启动，否则进入等待队列（状态置为 queued）。
async fn enqueue_or_spawn(app: &AppHandle, state: &AppState, payload: DownloadRequest, output_dir: PathBuf, resume: Option<(u64, u64)>) -> Result<(), String> {
    let max = state.settings.read().await.max_concurrent.max(1) as usize;
    let cancel = Arc::new(tokio::sync::Mutex::new(false));
    {
        let mut downloads = state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?;
        if downloads.contains_key(&payload.id) { return Err("任务正在下载中".to_string()); }
        if downloads.len() >= max {
            drop(downloads);
            state.download_queue.lock().map_err(|_| "下载队列锁定失败".to_string())?
                .push_back(QueuedDownload { payload: payload.clone(), output_dir, resume });
            let (received, total, unit, received_bytes, total_bytes) = state.tasks.lock().ok()
                .and_then(|tasks| tasks.get(&payload.id).map(|record| (record.received, record.total, record.unit.clone(), record.received_bytes, record.total_bytes)))
                .unwrap_or((0, None, None, None, None));
            emit_download(app, DownloadUpdate { id: payload.id, filename: payload.filename, status: "queued".into(), received, total, error: None, unit, received_bytes, total_bytes });
            return Ok(());
        }
        downloads.insert(payload.id.clone(), cancel.clone());
    }
    spawn_with_token(app, payload, output_dir, resume, cancel);
    Ok(())
}

#[tauri::command]
async fn start_download(app: AppHandle, state: tauri::State<'_, AppState>, payload: DownloadRequest) -> Result<(), String> {
    if !is_allowed_url(&payload.url) { return Err("只支持 http 或 https 媒体地址".to_string()); }
    let settings = state.settings.read().await.clone();
    let output_dir = app_download_dir(&app, &settings)?;
    {
        let mut tasks = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?;
        tasks.insert(payload.id.clone(), TaskRecord {
            id: payload.id.clone(), url: payload.url.clone(), filename: payload.filename.clone(),
            referer: payload.referer.clone(), subdir: None, status: "downloading".into(),
            received: 0, total: None, unit: None, received_bytes: None, total_bytes: None, error: None,
            created_at: chrono_time(),
        });
        save_tasks_locked(&tasks, &state.tasks_path);
    }
    enqueue_or_spawn(&app, state.inner(), payload, output_dir, None).await
}

/// 重试/继续下载：有进度的任务从断点续传，否则重新下载。
#[tauri::command]
async fn retry_download(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let record = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?.get(&id).cloned()
        .ok_or_else(|| "任务不存在或已被删除".to_string())?;
    if state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?.contains_key(&id) {
        return Err("任务正在下载中".to_string());
    }
    if state.download_queue.lock().map_err(|_| "下载队列锁定失败".to_string())?.iter().any(|item| item.payload.id == id) {
        return Err("任务已在下载队列中等待".to_string());
    }
    let settings = state.settings.read().await.clone();
    let base_dir = app_download_dir(&app, &settings)?;
    let output_dir = match &record.subdir {
        Some(name) => {
            let dir = base_dir.join(safe_filename(name));
            std::fs::create_dir_all(&dir).map_err(|error| format!("创建子目录失败: {error}"))?;
            dir
        }
        None => base_dir,
    };
    let resume = if record.received > 0 { Some((record.received, record.received_bytes.unwrap_or(0))) } else { None };
    emit_download(&app, DownloadUpdate { id: record.id.clone(), filename: record.filename.clone(), status: "downloading".into(), received: record.received, total: record.total, error: None, unit: record.unit.clone(), received_bytes: record.received_bytes, total_bytes: record.total_bytes });
    let payload = DownloadRequest { id: record.id, url: record.url, filename: record.filename, referer: record.referer };
    enqueue_or_spawn(&app, state.inner(), payload, output_dir, resume).await
}

/// 删除任务记录；未完成的任务同时清理半成品文件，已完成文件保留。
#[tauri::command]
async fn delete_task(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let record = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?.remove(&id);
    if let Ok(tasks) = state.tasks.lock() { save_tasks_locked(&tasks, &state.tasks_path); }
    if let Some(record) = record {
        if record.status != "complete" {
            let settings = state.settings.read().await.clone();
            if let Ok(base_dir) = app_download_dir(&app, &settings) {
                let dir = match &record.subdir { Some(name) => base_dir.join(safe_filename(name)), None => base_dir };
                let on_disk = if classify_media(&record.url).as_deref() == Some("m3u8") { hls_output_filename(&record.filename) } else { safe_filename(&record.filename) };
                let path = dir.join(on_disk);
                if path.exists() { let _ = std::fs::remove_file(path); }
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn get_tasks(state: tauri::State<'_, AppState>) -> Result<Vec<TaskRecord>, String> {
    let tasks = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?;
    let mut records: Vec<TaskRecord> = tasks.values().cloned().collect();
    records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(records)
}

/// 一键清除已完成任务（只删任务记录，已下载的文件保留）
#[tauri::command]
fn clear_completed_tasks(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut tasks = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?;
    tasks.retain(|_, record| record.status != "complete");
    save_tasks_locked(&tasks, &state.tasks_path);
    Ok(())
}

/// 在系统文件管理器中显示任务对应的文件（文件不存在则打开下载目录）
#[tauri::command]
async fn show_in_folder(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let record = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?.get(&id).cloned()
        .ok_or_else(|| "任务不存在或已被删除".to_string())?;
    let settings = state.settings.read().await.clone();
    let base_dir = app_download_dir(&app, &settings)?;
    let dir = match &record.subdir { Some(name) => base_dir.join(safe_filename(name)), None => base_dir.clone() };
    let on_disk = if classify_media(&record.url).as_deref() == Some("m3u8") { hls_output_filename(&record.filename) } else { safe_filename(&record.filename) };
    reveal_in_folder(dir.join(on_disk), dir)
}

fn reveal_in_folder(path: PathBuf, dir: PathBuf) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let result = if path.exists() {
            std::process::Command::new("open").arg("-R").arg(&path).status()
        } else {
            std::process::Command::new("open").arg(&dir).status()
        };
        return result.map_err(|error| error.to_string()).map(|_| ());
    }
    #[cfg(target_os = "windows")]
    {
        let result = if path.exists() {
            std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).status()
        } else {
            std::process::Command::new("explorer").arg(&dir).status()
        };
        return result.map_err(|error| error.to_string()).map(|_| ());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = path;
        return std::process::Command::new("xdg-open").arg(&dir).status().map_err(|error| error.to_string()).map(|_| ());
    }
}

async fn download_media(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>, resume: Option<(u64, u64)>) -> Result<(), String> {
    if classify_media(&payload.url).as_deref() == Some("m3u8") { download_hls(app, payload, output_dir, cancel, resume).await }
    else { download_direct(app, payload, output_dir, cancel, resume).await }
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

async fn download_direct(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>, resume: Option<(u64, u64)>) -> Result<(), String> {
    let client = build_client(payload.referer.as_deref())?;
    let path = output_dir.join(safe_filename(&payload.filename));
    // 断点续传：已有半成品文件时从文件大小处继续
    let mut start_at = 0u64;
    if resume.is_some() {
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.len() > 0 { start_at = meta.len(); debug_log(&format!("[download_direct] resuming at {start_at} bytes")); }
        }
    }
    let mut request = client.get(&payload.url);
    if start_at > 0 { request = request.header(header::RANGE, format!("bytes={start_at}-")); }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() && status.as_u16() != 206 { return Err(format!("下载请求返回 {status}")); }
    // 服务器支持 Range 会返回 206，追加写入；忽略 Range（200）则从头下载
    let (mut file, mut received) = if start_at > 0 && status.as_u16() == 206 {
        (OpenOptions::new().append(true).open(&path).await.map_err(|error| error.to_string())?, start_at)
    } else {
        (File::create(&path).await.map_err(|error| error.to_string())?, 0u64)
    };
    let total = response.content_length().map(|remaining| remaining + received);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if *cancel.lock().await {
            // 保留半成品文件以便续传
            file.flush().await.ok();
            emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "canceled".into(), received, total, error: None, unit: Some("bytes".into()), received_bytes: None, total_bytes: None });
            return Ok(());
        }
        let chunk = chunk.map_err(|error| error.to_string())?;
        file.write_all(&chunk).await.map_err(|error| error.to_string())?;
        received += chunk.len() as u64;
        emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "downloading".into(), received, total, error: None, unit: Some("bytes".into()), received_bytes: None, total_bytes: None });
    }
    file.flush().await.map_err(|error| error.to_string())?;
    emit_download(&app, DownloadUpdate { id: payload.id, filename: payload.filename, status: "complete".into(), received, total, error: None, unit: Some("bytes".into()), received_bytes: None, total_bytes: None });
    Ok(())
}

/// Merged segment stream is an MPEG-TS video, not a playlist — use .ts extension.
fn hls_output_filename(filename: &str) -> String {
    let safe_name = safe_filename(filename);
    safe_name
        .strip_suffix(".m3u8").or_else(|| safe_name.strip_suffix(".mpd"))
        .map(|stem| format!("{stem}.ts"))
        .unwrap_or(if safe_name.contains('.') { safe_name } else { format!("{safe_name}.ts") })
}

async fn download_hls(app: AppHandle, payload: DownloadRequest, output_dir: PathBuf, cancel: Arc<tokio::sync::Mutex<bool>>, resume: Option<(u64, u64)>) -> Result<(), String> {
    let client = build_client(payload.referer.as_deref())?;
    let mut playlist_url = Url::parse(&payload.url).map_err(|error| error.to_string())?;
    let mut playlist = client.get(playlist_url.clone()).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?.text().await.map_err(|error| error.to_string())?;
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return Err("该地址返回的不是有效的 m3u8 播放列表（可能是网页或错误页面）".to_string());
    }
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
        if !playlist.trim_start().starts_with("#EXTM3U") {
            return Err("视频流播放列表返回的内容无效（可能是网页或错误页面）".to_string());
        }
    }
    let media_sequence = playlist.lines().find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:").and_then(|value| value.parse::<u64>().ok())).unwrap_or(0);
    let mut segments = Vec::new();
    let mut current_key: Option<HlsKey> = None;
    for line in playlist.lines().map(str::trim).filter(|line| !line.is_empty()) {
        // ENDLIST 之后的行均为服务器写入的脏数据（曾出现残片 URL 被误认为分片导致 404），直接停止解析
        if line.starts_with("#EXT-X-ENDLIST") { break; }
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
    let filename = hls_output_filename(&payload.filename);
    let path = output_dir.join(filename.clone());
    let total = segments.len() as u64;
    // 断点续传：截断到已确认的完整分片边界（防止上次中断残留半个分片），跳过已下载分片
    let mut skip = 0usize;
    let mut received = 0u64;
    let mut received_bytes = 0u64;
    let mut output = match resume {
        Some((done_segments, done_bytes)) if done_segments > 0 => {
            match OpenOptions::new().read(true).write(true).open(&path).await {
                Ok(_) if done_segments >= total => {
                    emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: filename.clone(), status: "complete".into(), received: total, total: Some(total), error: None, unit: Some("segments".into()), received_bytes: Some(done_bytes), total_bytes: Some(done_bytes) });
                    return Ok(());
                }
                Ok(file) => {
                    let len = file.metadata().await.map_err(|error| error.to_string())?.len();
                    let keep = done_bytes.min(len);
                    if keep > 0 {
                        file.set_len(keep).await.map_err(|error| error.to_string())?;
                        drop(file);
                        debug_log(&format!("[download_hls] resuming: skip {done_segments}/{total} segments, keep {keep} bytes"));
                        skip = done_segments as usize;
                        received = done_segments;
                        received_bytes = keep;
                        OpenOptions::new().append(true).open(&path).await.map_err(|error| error.to_string())?
                    } else {
                        drop(file);
                        File::create(&path).await.map_err(|error| error.to_string())?
                    }
                }
                Err(_) => File::create(&path).await.map_err(|error| error.to_string())?,
            }
        }
        _ => File::create(&path).await.map_err(|error| error.to_string())?,
    };
    for (index, (segment_url, key)) in segments.iter().enumerate().skip(skip) {
        if *cancel.lock().await {
            // 保留半成品文件以便续传
            output.flush().await.ok();
            emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "canceled".into(), received, total: Some(total), error: None, unit: Some("segments".into()), received_bytes: Some(received_bytes), total_bytes: None });
            return Ok(());
        }
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
            output.flush().await.ok();
            let message = format!("分片 {}/{} 下载失败: {last_error}", index + 1, total);
            emit_download(&app, DownloadUpdate { id: payload.id.clone(), filename: payload.filename.clone(), status: "failed".into(), received, total: Some(total), error: Some(message.clone()), unit: Some("segments".into()), received_bytes: Some(received_bytes), total_bytes: None });
            return Err(message);
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
fn cancel_download(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    if let Some(cancel) = state.downloads.lock().map_err(|_| "下载队列锁定失败".to_string())?.get(&id) {
        let cancel = cancel.clone();
        tauri::async_runtime::spawn(async move { *cancel.lock().await = true; });
        return Ok(());
    }
    // 排队中的任务直接从队列移除，无需经过下载循环
    let mut queue = state.download_queue.lock().map_err(|_| "下载队列锁定失败".to_string())?;
    if let Some(position) = queue.iter().position(|item| item.payload.id == id) {
        queue.remove(position);
        drop(queue);
        let (filename, received, total, unit, received_bytes, total_bytes) = state.tasks.lock().ok()
            .and_then(|tasks| tasks.get(&id).map(|record| (record.filename.clone(), record.received, record.total, record.unit.clone(), record.received_bytes, record.total_bytes)))
            .ok_or_else(|| "任务不存在或已被删除".to_string())?;
        emit_download(&app, DownloadUpdate { id, filename, status: "canceled".into(), received, total, error: None, unit, received_bytes, total_bytes });
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
    #[serde(default)]
    show: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeResult {
    episodes: Vec<Episode>,
    steps: Vec<String>,
}

const EPISODE_SYSTEM_PROMPT: &str = r#"你是视频站点分析助手。用户会给你一个网页的信息（URL、标题、链接列表，格式 text|href）。你的任务是找出所有"剧集播放页"的链接。

只输出一个 JSON 对象，不要输出任何其他文字：
- 如果链接列表已包含剧集播放页：{"action":"done","show":"作品名称","episodes":[{"title":"第1集","url":"绝对URL"},...]}
- 如果需要先打开某个页面才能看到剧集列表（例如当前页是详情页、首页或需要进入播放列表页）：{"action":"open","url":"要打开的页面URL","reason":"原因"}
- 如果确实找不到剧集链接：{"action":"fail","reason":"原因"}

规则：
1. episodes 里的 url 必须来自提供的链接列表（把相对路径转为绝对路径），不要编造 URL
2. title 优先用链接文本（如"第1集"），没有则用 URL 最后一段
3. show 是作品本身的名称（如"光阴之外"），从页面标题或链接文本中提取；不包含"第X集"、季数、线路、清晰度、站点名等信息
4. 按集数/顺序排列，尽量覆盖全部集数
5. 只输出 JSON"#;

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
        // Kimi 兼容模式只接受 0.0/0.6/1.0，0.0 对各家 OpenAI 兼容接口都安全
        "temperature": 0.0,
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
                let show = json["show"].as_str().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
                let episodes = json["episodes"].as_array().cloned().unwrap_or_default().iter().filter_map(|item| {
                    let url = item["url"].as_str()?.to_string();
                    if !is_allowed_url(&url) { return None; }
                    let title = item["title"].as_str().map(|value| value.to_string())
                        .unwrap_or_else(|| url.rsplit('/').next().unwrap_or("剧集").to_string());
                    Some(Episode { title, url, show: show.clone() })
                }).collect::<Vec<_>>();
                if episodes.is_empty() { debug_log(&format!("[analyze_episodes] round {round} action=done but 0 valid episodes")); return Err(format!("第 {round} 轮：AI 判定完成但没有找到任何有效的剧集链接")); }
                debug_log(&format!("[analyze_episodes] done: {} episodes found, show={show:?}", episodes.len()));
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
    // If no media was captured from network requests, scan the page HTML for player variables
    let result = if result.is_none() {
        debug_log("[capture_episode_media] no media from network, scanning HTML");
        let (media_urls, _page_title) = scan_page_html_for_media(page_url).await;
        media_urls.into_iter().next()
    } else {
        result
    };
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
async fn batch_download(app: AppHandle, state: tauri::State<'_, AppState>, episodes: Vec<Episode>, subdir: Option<String>) -> Result<(), String> {
    debug_log(&format!("[batch_download] starting with {} episodes, subdir={subdir:?}", episodes.len()));
    if episodes.is_empty() { return Err("剧集列表为空".to_string()); }
    let subdir = subdir.map(|name| name.trim().to_string()).filter(|name| !name.is_empty());
    let settings = state.settings.read().await.clone();
    let base_dir = app_download_dir(&app, &settings)?;
    let output_dir = match &subdir {
        Some(name) => {
            let dir = base_dir.join(safe_filename(name));
            std::fs::create_dir_all(&dir).map_err(|error| format!("创建子目录失败: {error}"))?;
            dir
        }
        None => base_dir,
    };
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
                let _ = emit_captured(&app, &state, media_url.clone(), effective_page_url.clone(), None, None);
                let base_name = match &episode.show {
                    Some(show) if !episode.title.contains(show.as_str()) => format!("{} {}", show, episode.title),
                    _ => episode.title.clone(),
                };
                let payload = DownloadRequest {
                    id: format!("batch-{}-{}", chrono_time(), index),
                    url: media_url,
                    filename: format!("{base_name}.ts"),
                    referer: Some(effective_page_url),
                };
                {
                    let mut tasks = state.tasks.lock().map_err(|_| "任务列表锁定失败".to_string())?;
                    tasks.insert(payload.id.clone(), TaskRecord {
                        id: payload.id.clone(), url: payload.url.clone(), filename: payload.filename.clone(),
                        referer: payload.referer.clone(), subdir: subdir.clone(), status: "downloading".into(),
                        received: 0, total: None, unit: None, received_bytes: None, total_bytes: None, error: None,
                        created_at: chrono_time(),
                    });
                    save_tasks_locked(&tasks, &state.tasks_path);
                }
                if let Err(error) = enqueue_or_spawn(&app, &state, payload, output_dir.clone(), None).await {
                    debug_log(&format!("[batch] spawn download failed for '{}': {error}", episode.title));
                }
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
async fn analyze_page_links(app: AppHandle, state: tauri::State<'_, AppState>, url: String, html_or_links: String, title: Option<String>) -> Result<AnalyzeResult, String> {
    let settings = state.settings.read().await.clone();
    let lines = html_or_links.lines().count();
    let page_title = title.unwrap_or_default();
    let user_message = format!("页面 URL: {url}\n页面标题: {page_title}\n\n链接列表（格式 文本|URL）：\n{html_or_links}");
    let _ = app.emit("batch-progress", BatchProgress { stage: "analyzing".into(), message: format!("LLM 正在分析 {lines} 个链接…"), current: Some(1), total: Some(1) });
    let reply = llm_chat(&settings, EPISODE_SYSTEM_PROMPT, &user_message).await
        .map_err(|error| format!("AI 分析失败：{error}"))?;
    let Some(json) = extract_llm_json(&reply) else {
        return Err("LLM 输出无法解析为 JSON".to_string());
    };
    match json["action"].as_str().unwrap_or_default() {
        "done" => {
            let show = json["show"].as_str().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
            let episodes = json["episodes"].as_array().cloned().unwrap_or_default().iter().filter_map(|item| {
                let url = item["url"].as_str()?.to_string();
                if !is_allowed_url(&url) { return None; }
                let title = item["title"].as_str().map(|value| value.to_string())
                    .unwrap_or_else(|| url.rsplit('/').next().unwrap_or("剧集").to_string());
                Some(Episode { title, url, show: show.clone() })
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

// ===== 局域网下载服务 =====

fn start_lan_server(app: &AppHandle, port: u32) -> Result<(), String> {
    let state = app.state::<AppState>();
    if let Ok(guard) = state.lan_server.lock() {
        if let Some(running) = guard.as_ref() {
            if running.port == port { return Ok(()); }
        }
    }
    stop_lan_server(app);
    let server = tiny_http::Server::http(("0.0.0.0", port as u16)).map_err(|error| format!("端口 {port} 启动失败: {error}"))?;
    let lan = Arc::new(LanServer { port, server: Arc::new(server) });
    *state.lan_server.lock().map_err(|_| "服务状态锁定失败".to_string())? = Some(lan.clone());
    let app_handle = app.clone();
    std::thread::spawn(move || {
        for request in lan.server.incoming_requests() {
            let app = app_handle.clone();
            std::thread::spawn(move || handle_lan_request(&app, request));
        }
        debug_log("[lan] server loop exited");
    });
    debug_log(&format!("[lan] server started on port {port}"));
    Ok(())
}

fn stop_lan_server(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Ok(mut guard) = state.lan_server.lock() {
        if let Some(lan) = guard.take() {
            lan.server.unblock();
            debug_log("[lan] server stopped");
        }
    };
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LanInfo {
    running: bool,
    port: u32,
    urls: Vec<String>,
}

#[tauri::command]
fn get_lan_info(state: tauri::State<'_, AppState>) -> Result<LanInfo, String> {
    let running = state.lan_server.lock().map_err(|_| "服务状态锁定失败".to_string())?;
    let (running, port) = match running.as_ref() { Some(lan) => (true, lan.port), None => (false, 0) };
    let urls = if running { local_lan_urls(port) } else { vec![] };
    Ok(LanInfo { running, port, urls })
}

fn local_lan_urls(port: u32) -> Vec<String> {
    // UDP connect 不发送数据，只为让系统选出到公网路由对应的本机局域网 IP
    let mut urls = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind(("0.0.0.0", 0)) {
        if socket.connect(("8.8.8.8", 80)).is_ok() {
            if let Ok(addr) = socket.local_addr() {
                urls.push(format!("http://{}:{port}", addr.ip()));
            }
        }
    }
    urls
}

type LanResponse = tiny_http::Response<Box<dyn std::io::Read + Send>>;

fn handle_lan_request(app: &AppHandle, mut request: tiny_http::Request) {
    let method = request.method().as_str().to_string();
    let raw_url = request.url().to_string();
    let mut url_parts = raw_url.splitn(2, '?');
    let path = url_parts.next().unwrap_or("/").to_string();
    let query = url_parts.next().unwrap_or("").to_string();
    let response: LanResponse = match (method.as_str(), path.as_str()) {
        ("GET", "/") => text_response(LAN_PAGE.to_string(), "text/html; charset=utf-8"),
        ("GET", "/api/tasks") => json_response(lan_tasks_json(app)),
        ("GET", "/api/dirs") => json_response(lan_dirs_json(app)),
        ("GET", "/api/files") => json_response(lan_files_json(app)),
        ("GET", "/files") => {
            let rel = query.split('&').find_map(|pair| pair.strip_prefix("path=")).unwrap_or("");
            lan_serve_file(app, &urlencoding_decode(rel))
        }
        ("POST", "/api/download") => {
            let mut body = String::new();
            match std::io::Read::read_to_string(&mut request.as_reader(), &mut body) {
                Ok(_) => json_response(tauri::async_runtime::block_on(lan_submit(app, &body))),
                Err(_) => json_status("{\"error\":\"读取请求失败\"}".to_string(), 400),
            }
        }
        _ => json_status("{\"error\":\"not found\"}".to_string(), 404),
    };
    let _ = request.respond(response);
}

fn text_response(body: String, content_type: &str) -> LanResponse {
    let bytes = body.into_bytes();
    let header = tiny_http::Header::from_bytes("Content-Type".as_bytes(), content_type.as_bytes()).unwrap();
    tiny_http::Response::new(tiny_http::StatusCode(200), vec![header], Box::new(std::io::Cursor::new(bytes.clone())), Some(bytes.len()), None)
}

fn json_response(json: String) -> LanResponse {
    text_response(json, "application/json; charset=utf-8")
}

fn json_status(json: String, code: u16) -> LanResponse {
    let bytes = json.into_bytes();
    let header = tiny_http::Header::from_bytes("Content-Type".as_bytes(), &b"application/json; charset=utf-8"[..]).unwrap();
    tiny_http::Response::new(tiny_http::StatusCode(code), vec![header], Box::new(std::io::Cursor::new(bytes.clone())), Some(bytes.len()), None)
}

/// 把相对路径解析为下载目录内的真实文件，拒绝路径穿越。
fn resolve_lan_file(app: &AppHandle, rel: &str) -> Result<(PathBuf, String), String> {
    let rel = rel.trim().trim_matches('/');
    if rel.is_empty() || rel.contains("..") || rel.contains('\\') { return Err("非法路径".to_string()); }
    let state = app.state::<AppState>();
    let settings = tauri::async_runtime::block_on(state.settings.read()).clone();
    let base = app_download_dir(app, &settings)?;
    let base_canonical = base.canonicalize().map_err(|error| error.to_string())?;
    let full = base.join(rel).canonicalize().map_err(|_| "文件不存在".to_string())?;
    if !full.starts_with(&base_canonical) || !full.is_file() { return Err("非法路径".to_string()); }
    let name = full.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "download".to_string());
    Ok((full, name))
}

/// Content-Disposition filename* 的 UTF-8 百分号编码
fn pct_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') { output.push(*byte as char); }
        else { output.push_str(&format!("%{byte:02X}")); }
    }
    output
}

fn file_response(file: std::fs::File, size: usize, download_name: &str) -> LanResponse {
    let disposition = format!("attachment; filename=\"download\"; filename*=UTF-8''{}", pct_encode(download_name));
    let headers = vec![
        tiny_http::Header::from_bytes("Content-Type".as_bytes(), &b"application/octet-stream"[..]).unwrap(),
        tiny_http::Header::from_bytes("Content-Disposition".as_bytes(), disposition.as_bytes()).unwrap(),
    ];
    tiny_http::Response::new(tiny_http::StatusCode(200), headers, Box::new(file), Some(size), None)
}

fn lan_serve_file(app: &AppHandle, rel: &str) -> LanResponse {
    match resolve_lan_file(app, rel) {
        Ok((full, name)) => match std::fs::File::open(&full) {
            Ok(file) => {
                let size = full.metadata().map(|meta| meta.len() as usize).unwrap_or(0);
                file_response(file, size, &name)
            }
            Err(_) => json_status("{\"error\":\"文件读取失败\"}".to_string(), 500),
        },
        Err(error) => json_status(serde_json::json!({"error": error}).to_string(), 400),
    }
}

/// 列出下载目录内容：根目录一组 + 每个子目录一组，文件按修改时间倒序。
fn lan_files_json(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    let settings = tauri::async_runtime::block_on(state.settings.read()).clone();
    let base = match app_download_dir(app, &settings) { Ok(dir) => dir, Err(_) => return "{\"groups\":[]}".to_string() };
    let collect_files = |dir: &PathBuf| -> Vec<serde_json::Value> {
        let mut files: Vec<serde_json::Value> = std::fs::read_dir(dir).map(|entries| entries.flatten().filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() { return None; }
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with('.') { return None; }
            let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            Some(serde_json::json!({"name": name, "size": meta.len(), "modified": modified}))
        }).collect()).unwrap_or_default();
        files.sort_by(|a, b| b["modified"].as_u64().cmp(&a["modified"].as_u64()));
        files
    };
    let mut groups = vec![serde_json::json!({"dir": "", "files": collect_files(&base)})];
    let mut subdirs: Vec<String> = std::fs::read_dir(&base).map(|entries| entries.flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect()).unwrap_or_default();
    subdirs.sort();
    for dir in subdirs {
        groups.push(serde_json::json!({"dir": dir, "files": collect_files(&base.join(&dir))}));
    }
    serde_json::json!({"groups": groups}).to_string()
}

fn lan_tasks_json(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    let mut records: Vec<TaskRecord> = state.tasks.lock().map(|tasks| tasks.values().cloned().collect()).unwrap_or_default();
    records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    serde_json::to_string(&records).unwrap_or_else(|_| "[]".to_string())
}

fn lan_dirs_json(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    let settings = tauri::async_runtime::block_on(state.settings.read()).clone();
    let dirs = app_download_dir(app, &settings).map(|base| {
        std::fs::read_dir(base).map(|entries| entries.flatten()
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect::<Vec<_>>()).unwrap_or_default()
    }).unwrap_or_default();
    serde_json::to_string(&dirs).unwrap_or_else(|_| "[]".to_string())
}

#[derive(Deserialize)]
struct LanSubmitRequest {
    urls: Vec<String>,
    #[serde(default)]
    subdir: Option<String>,
}

/// LAN 提交：直链直接入队；播放页地址先用无头浏览器探测媒体地址再入队。
async fn lan_submit(app: &AppHandle, body: &str) -> String {
    let request: LanSubmitRequest = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(_) => return "{\"error\":\"请求格式错误\"}".to_string(),
    };
    let urls: Vec<String> = request.urls.iter().map(|url| url.trim().to_string()).filter(|url| !url.is_empty()).take(50).collect();
    if urls.is_empty() { return "{\"error\":\"请先填写至少一个地址\"}".to_string(); }
    let subdir = request.subdir.map(|name| name.trim().to_string()).filter(|name| !name.is_empty());
    let state = app.state::<AppState>();
    let settings = state.settings.read().await.clone();
    let base_dir = match app_download_dir(app, &settings) { Ok(dir) => dir, Err(error) => return serde_json::json!({"error": error}).to_string() };
    let output_dir = match &subdir {
        Some(name) => {
            let dir = base_dir.join(safe_filename(name));
            if let Err(error) = std::fs::create_dir_all(&dir) { return serde_json::json!({"error": format!("创建子目录失败: {error}")}).to_string(); }
            dir
        }
        None => base_dir,
    };
    let mut accepted = 0u32;
    let mut failed: Vec<serde_json::Value> = Vec::new();
    let mut browser: Option<Arc<Browser>> = None;
    for (index, raw) in urls.iter().enumerate() {
        if !is_allowed_url(raw) { failed.push(serde_json::json!({"url": raw, "reason": "仅支持 http/https 地址"})); continue; }
        let (media_url, referer) = if classify_media(raw).is_some() {
            (raw.clone(), None)
        } else {
            if browser.is_none() {
                match ensure_headless_browser(app, state.inner(), &settings).await {
                    Ok(()) => browser = state.headless_browser.lock().await.clone(),
                    Err(error) => { failed.push(serde_json::json!({"url": raw, "reason": format!("浏览器初始化失败: {error}")})); continue; }
                }
            }
            let Some(instance) = browser.clone() else { failed.push(serde_json::json!({"url": raw, "reason": "浏览器不可用"})); continue; };
            match capture_episode_media(&instance, raw).await {
                Ok((Some(media), redirected)) => (media, Some(redirected.unwrap_or_else(|| raw.clone()))),
                Ok((None, _)) => { failed.push(serde_json::json!({"url": raw, "reason": "未探测到媒体地址"})); continue; }
                Err(error) => { failed.push(serde_json::json!({"url": raw, "reason": error})); continue; }
            }
        };
        let payload = DownloadRequest {
            id: format!("lan-{}-{}", chrono_time(), index),
            url: media_url.clone(),
            filename: lan_filename(&media_url),
            referer,
        };
        {
            let mut tasks = match state.tasks.lock() { Ok(tasks) => tasks, Err(_) => return "{\"error\":\"任务列表锁定失败\"}".to_string() };
            tasks.insert(payload.id.clone(), TaskRecord {
                id: payload.id.clone(), url: payload.url.clone(), filename: payload.filename.clone(),
                referer: payload.referer.clone(), subdir: subdir.clone(), status: "downloading".into(),
                received: 0, total: None, unit: None, received_bytes: None, total_bytes: None, error: None,
                created_at: chrono_time(),
            });
            save_tasks_locked(&tasks, &state.tasks_path);
        }
        match enqueue_or_spawn(app, state.inner(), payload, output_dir.clone(), None).await {
            Ok(()) => accepted += 1,
            Err(error) => { failed.push(serde_json::json!({"url": raw, "reason": error})); }
        }
    }
    debug_log(&format!("[lan] submit: {accepted} accepted, {} failed", failed.len()));
    serde_json::json!({"accepted": accepted, "failed": failed}).to_string()
}

/// 从媒体 URL 推导文件名：末段无意义（index/playlist 等）时用上一级目录名。
fn lan_filename(media_url: &str) -> String {
    const GENERIC: [&str; 7] = ["index", "playlist", "main", "master", "chunklist", "media", "video"];
    let parsed = Url::parse(media_url);
    let segments: Vec<String> = parsed.as_ref().map(|url| url.path_segments().map(|parts| parts.filter(|part| !part.is_empty()).map(|part| part.to_string()).collect()).unwrap_or_default()).unwrap_or_default();
    let ext = classify_media(media_url).map(|kind| if kind == "m3u8" { "ts".to_string() } else { kind }).unwrap_or_else(|| "mp4".to_string());
    let stem = segments.last().map(|segment| {
        let decoded = urlencoding_decode(segment);
        decoded.rsplit_once('.').map(|(stem, _)| stem.to_string()).unwrap_or(decoded)
    }).filter(|stem| !stem.is_empty() && !GENERIC.contains(&stem.to_ascii_lowercase().as_str()))
        .or_else(|| segments.get(segments.len().saturating_sub(2)).map(|segment| urlencoding_decode(segment)))
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| format!("video-{}", chrono_time()));
    safe_filename(&format!("{stem}.{ext}"))
}

fn urlencoding_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&input[index + 1..index + 3], 16) {
                output.push(value);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| input.to_string())
}

const LAN_PAGE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Video Scout · 局域网下载</title>
<style>
*{box-sizing:border-box;margin:0}
body{font-family:-apple-system,'PingFang SC',sans-serif;background:#f6f8fb;color:#1a2636;min-height:100vh;padding:20px 14px 40px}
.wrap{max-width:560px;margin:0 auto}
h1{font-size:20px;margin:14px 0 4px}
.sub{color:#8490a0;font-size:12px;margin-bottom:18px}
.card{background:#fff;border:1px solid #e8edf3;border-radius:12px;padding:16px;margin-bottom:16px}
textarea{width:100%;min-height:110px;border:1px solid #dfe6f0;border-radius:8px;padding:10px;font-size:13px;font-family:inherit;resize:vertical}
textarea:focus,.row input:focus{outline:none;border-color:#426bdb}
.row{display:flex;gap:8px;margin-top:10px}
.row input{flex:1;min-width:0;height:38px;border:1px solid #dfe6f0;border-radius:8px;padding:0 10px;font-size:13px}
button{height:38px;padding:0 16px;border:0;border-radius:8px;background:#426bdb;color:#fff;font-size:13px;font-weight:600;cursor:pointer}
button:disabled{opacity:.6}
.banner{display:none;margin-top:10px;padding:9px 12px;border-radius:8px;font-size:12px;background:#f3f7ff;border:1px solid #d5e0f5;color:#3d529e;word-break:break-all}
.banner.error{background:#fdf3f3;border-color:#f0c8c8;color:#b04a4a}
.section-title{font-size:12px;color:#8490a0;margin:18px 0 8px}
.task{padding:10px 0;border-bottom:1px solid #eef1f5}
.task:last-child{border-bottom:0}
.task-top{display:flex;justify-content:space-between;gap:10px;font-size:13px;font-weight:600}
.task-top span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.task-state{flex:none;font-weight:500;font-size:11px;color:#8490a0}
.task-meta{margin-top:4px;font-size:11px;color:#97a2af}
.task-bar{margin-top:6px;height:4px;border-radius:2px;background:#e9eef7;overflow:hidden}
.task-bar i{display:block;height:100%;background:#4a72d9;border-radius:2px}
.task-bar.paused i{background:#cfa54e}
.state-complete{color:#169b78}.state-failed{color:#c45f5f}
.group{border:1px solid #e8edf3;border-radius:10px;margin-bottom:10px;overflow:hidden}
.group-head{display:flex;align-items:center;gap:8px;padding:10px 12px;background:#f8fafd;border-bottom:1px solid #eef1f5}
.group-head label{display:flex;align-items:center;gap:7px;flex:1;min-width:0;font-size:13px;font-weight:600;cursor:pointer}
.group-head label span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.group-head small{flex:none;color:#97a2af;font-weight:400;font-size:11px}
.group-head button{height:28px;padding:0 10px;font-size:11px;flex:none;background:#eef3fd;color:#3d529e}
.file-row{display:flex;align-items:center;gap:8px;padding:9px 12px;border-bottom:1px solid #f2f5f9;font-size:12px;cursor:pointer}
.file-row:last-child{border-bottom:0}
.file-row .file-name{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.file-row .file-size{flex:none;color:#97a2af;font-size:11px}
input[type=checkbox]{accent-color:#426bdb;width:15px;height:15px;flex:none}
.file-actions{display:flex;align-items:center;gap:10px;margin-bottom:10px}
.file-actions .count{flex:1;color:#8490a0;font-size:12px}
</style>
</head>
<body>
<div class="wrap">
  <h1>Video Scout 局域网下载</h1>
  <div class="sub">提交播放页或直链地址，由电脑端探测并下载到本机</div>
  <div class="card">
    <textarea id="urls" placeholder="每行一个地址，支持播放页或直链（m3u8 / mp4…）"></textarea>
    <div class="row">
      <input id="subdir" list="dir-list" placeholder="保存到子目录（可选，不存在会自动创建）" autocomplete="off">
      <datalist id="dir-list"></datalist>
      <button id="submit">下载</button>
    </div>
    <div class="banner" id="banner"></div>
  </div>
  <div class="section-title">下载任务</div>
  <div class="card" id="tasks"></div>
  <div class="section-title">已下载的视频（可下载到本设备）</div>
  <div class="file-actions">
    <span class="count" id="sel-count">未选择文件</span>
    <button id="download-selected" disabled>下载选中</button>
  </div>
  <div id="files"></div>
</div>
<script>
const banner = document.getElementById('banner');
function showBanner(text, error) { banner.style.display = 'block'; banner.className = error ? 'banner error' : 'banner'; banner.textContent = text; }
const stateName = { downloading: '下载中', queued: '排队中', complete: '已完成', failed: '失败', canceled: '已取消', interrupted: '已中断' };
function esc(text) { return String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;'); }
function fmtSize(bytes) {
  if (!bytes) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes, unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return (value >= 100 ? Math.round(value) : value.toFixed(1)) + ' ' + units[unit];
}
async function refreshTasks() {
  try {
    const tasks = await (await fetch('/api/tasks')).json();
    const box = document.getElementById('tasks');
    if (!tasks.length) { box.innerHTML = '<div class="task-meta">暂无任务</div>'; return; }
    box.innerHTML = tasks.slice().reverse().map(task => {
      const known = task.total > 0;
      const pct = task.status === 'complete' ? 100 : known ? Math.round(Math.min(task.received / task.total, 1) * 100) : 0;
      const meta = task.error ? task.error : task.unit === 'segments' ? `${task.received} / ${task.total ?? '?'} 个分片` : '';
      const paused = ['failed', 'canceled', 'interrupted'].includes(task.status) ? ' paused' : '';
      return `<div class="task"><div class="task-top"><span>${esc(task.filename)}</span><span class="task-state state-${task.status}">${stateName[task.status] || task.status}${known || task.status === 'complete' ? ' ' + pct + '%' : ''}</span></div><div class="task-meta">${esc(meta)}</div><div class="task-bar${paused}"><i style="width:${pct}%"></i></div></div>`;
    }).join('');
  } catch (e) {}
}
async function refreshDirs() {
  try {
    const dirs = await (await fetch('/api/dirs')).json();
    document.getElementById('dir-list').innerHTML = dirs.map(dir => `<option value="${esc(dir)}"></option>`).join('');
  } catch (e) {}
}
async function refreshFiles() {
  try {
    const data = await (await fetch('/api/files')).json();
    renderFiles(data.groups || []);
  } catch (e) {}
}
function filePath(dir, name) { return dir ? dir + '/' + name : name; }
function renderFiles(groups) {
  const box = document.getElementById('files');
  const nonEmpty = groups.filter(group => group.files.length);
  if (!nonEmpty.length) { box.innerHTML = '<div class="card"><div class="task-meta">还没有已下载的视频</div></div>'; updateSelCount(); return; }
  box.innerHTML = nonEmpty.map((group, gi) => {
    const total = group.files.reduce((sum, file) => sum + file.size, 0);
    const rows = group.files.map(file => `<label class="file-row"><input type="checkbox" class="file-check" data-path="${esc(filePath(group.dir, file.name))}"><span class="file-name">${esc(file.name)}</span><span class="file-size">${fmtSize(file.size)}</span></label>`).join('');
    return `<div class="group"><div class="group-head"><label><input type="checkbox" class="group-check" data-group="${gi}"><span>${esc(group.dir || '下载根目录')}</span></label><small>${group.files.length} 个 · ${fmtSize(total)}</small><button type="button" class="group-download" data-group="${gi}">全部下载</button></div>${rows}</div>`;
  }).join('');
  box.querySelectorAll('.group-check').forEach(box2 => box2.addEventListener('change', () => {
    const group = nonEmpty[Number(box2.dataset.group)];
    group.files.forEach(file => {
      const input = document.querySelector(`.file-check[data-path="${CSS.escape(filePath(group.dir, file.name))}"]`);
      if (input) input.checked = box2.checked;
    });
    updateSelCount();
  }));
  box.querySelectorAll('.group-download').forEach(btn => btn.addEventListener('click', () => {
    const group = nonEmpty[Number(btn.dataset.group)];
    downloadPaths(group.files.map(file => filePath(group.dir, file.name)));
  }));
  box.querySelectorAll('.file-check').forEach(input => input.addEventListener('change', updateSelCount));
  updateSelCount();
}
function selectedPaths() { return [...document.querySelectorAll('.file-check:checked')].map(input => input.dataset.path); }
function updateSelCount() {
  const paths = selectedPaths();
  document.getElementById('sel-count').textContent = paths.length ? `已选 ${paths.length} 个文件` : '未选择文件';
  document.getElementById('download-selected').disabled = !paths.length;
}
function downloadPaths(paths) {
  if (!paths.length) return;
  showBanner(`开始逐个下载 ${paths.length} 个文件；若浏览器提示"允许多个文件下载"请点允许`);
  paths.forEach((path, index) => {
    setTimeout(() => {
      const iframe = document.createElement('iframe');
      iframe.style.display = 'none';
      iframe.src = '/files?path=' + encodeURIComponent(path);
      document.body.append(iframe);
      setTimeout(() => iframe.remove(), 60000);
    }, index * 400);
  });
}
document.getElementById('download-selected').addEventListener('click', () => downloadPaths(selectedPaths()));
document.getElementById('submit').addEventListener('click', async () => {
  const urls = document.getElementById('urls').value.split('\n').map(line => line.trim()).filter(Boolean);
  if (!urls.length) { showBanner('请先填写至少一个地址', true); return; }
  const button = document.getElementById('submit');
  button.disabled = true;
  showBanner('正在提交，播放页需要探测媒体地址，请稍候…');
  try {
    const result = await (await fetch('/api/download', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ urls, subdir: document.getElementById('subdir').value.trim() || null }) })).json();
    if (result.error) { showBanner(result.error, true); }
    else {
      const failText = result.failed && result.failed.length ? `，${result.failed.length} 个失败：` + result.failed.map(item => item.reason).join('；') : '';
      showBanner(`已接受 ${result.accepted} 个任务${failText}`, !!(result.failed && result.failed.length));
      if (result.accepted) document.getElementById('urls').value = '';
      refreshTasks();
    }
  } catch (e) { showBanner('提交失败：' + e, true); }
  button.disabled = false;
});
refreshTasks(); refreshDirs(); refreshFiles();
setInterval(refreshTasks, 3000); setInterval(refreshFiles, 8000);
</script>
</body>
</html>"##;

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
            let tasks_path = app.path().app_config_dir()?.join("tasks.json");
            let mut tasks: HashMap<String, TaskRecord> = std::fs::read(&tasks_path).ok()
                .and_then(|contents| serde_json::from_slice::<Vec<TaskRecord>>(&contents).ok())
                .unwrap_or_default()
                .into_iter().map(|record| (record.id.clone(), record)).collect();
            // 上次退出时仍在进行中的任务标记为已中断，等用户手动继续
            let mut dirty = false;
            for record in tasks.values_mut() {
                if record.status == "downloading" || record.status == "queued" {
                    record.status = "interrupted".into();
                    dirty = true;
                }
            }
            if dirty { save_tasks_locked(&tasks, &tasks_path); }
            debug_log(&format!("[setup] loaded {} persisted tasks", tasks.len()));
            let lan_enabled = settings.lan_enabled;
            let lan_port = settings.lan_port;
            app.manage(AppState::new(settings, settings_path, tasks, tasks_path));
            if lan_enabled {
                if let Err(error) = start_lan_server(&app.handle(), lan_port) {
                    debug_log(&format!("[setup] lan server autostart failed: {error}"));
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![open_page, open_visible_page, capture_media, visible_log, report_page_links, get_media, get_settings, save_settings, start_download, cancel_download, retry_download, delete_task, get_tasks, clear_completed_tasks, show_in_folder, clear_media, analyze_episodes, analyze_page_links, batch_download, get_lan_info])
        .run(tauri::generate_context!("Tauri.toml"))
        .expect("Video Scout failed to start");
}
