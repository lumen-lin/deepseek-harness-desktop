//! 壳页面 Tauri 命令、帧守卫注入脚本、外部链接转发、窗口状态与日志读取。
//!
//! **所有命令都要求调用方带上调用令牌**（`token` 参数，取自壳页面 URL 的 `?k=`）。
//! 原因见 `shell.rs` 顶部说明：Tauri 的 capability 只能按「来源 host:port」授权，
//! 而壳页面本身就跑在 127.0.0.1 上，ACL 无法区分「壳页面」与「本机另一个进程
//! 提供的同源页面」。令牌才是真正的边界，ACL 是第一层。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::logging::{log, log_dir};
use crate::repo;
use crate::server::{kill_server, ServerUrl};
use crate::shell;

/// restart_app 路径下跳过关闭确认（restart 会走窗口销毁流程，不能被确认框卡住）。
pub(crate) static SKIP_CLOSE_CONFIRM: AtomicBool = AtomicBool::new(false);

pub(crate) fn skip_close_confirm() -> bool {
    SKIP_CLOSE_CONFIRM.load(Ordering::SeqCst)
}

/// 校验调用令牌：每个壳命令的第一句。不匹配即拒绝并留日志。
pub(crate) fn guard(token: &str) -> Result<(), String> {
    if shell::token_matches(token) {
        Ok(())
    } else {
        log("拒绝调用：调用令牌无效（来源不是壳页面）");
        Err("无效的调用凭证".into())
    }
}

/// 用系统默认方式打开目录。
/// 交给 opener 插件而不是自己拼 `explorer` 命令行：路径含空格、引号、非 ASCII 时
/// 手工拼接容易出错，插件已处理这些边界情况。
fn open_in_explorer(path: &str) -> Result<(), String> {
    if !std::path::Path::new(path).is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Tauri 命令：打开 deepseek-harness 仓库目录（帮助菜单）。
#[tauri::command]
pub(crate) fn open_repo_dir(token: String) -> Result<(), String> {
    guard(&token)?;
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log("打开仓库目录");
    open_in_explorer(&repo.to_string_lossy())
}

/// Tauri 命令：打开数据与日志目录（帮助菜单）。
#[tauri::command]
pub(crate) fn open_logs_dir(token: String) -> Result<(), String> {
    guard(&token)?;
    log("打开日志目录");
    open_in_explorer(&log_dir().to_string_lossy())
}

/// Tauri 命令：读取运行日志尾部（内置日志查看窗展示用）。
///
/// 上限 200KB：日志窗每 2 秒轮询一次，返回体越大前端重建 DOM 越吃力；
/// 200KB 足够覆盖日常排查，真要全量可以直接打开日志文件看。
#[tauri::command]
pub(crate) fn read_log(token: String) -> Result<String, String> {
    guard(&token)?;
    use std::io::{Read, Seek, SeekFrom};
    const MAX_BYTES: u64 = 200_000;
    let path = log_dir().join("desktop.log");
    let mut file = std::fs::File::open(&path).map_err(|e| format!("无法打开日志文件: {e}"))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let skip = len.saturating_sub(MAX_BYTES);
    file.seek(SeekFrom::Start(skip)).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity((len - skip) as usize);
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if skip > 0 {
        // 掐掉被截断的首行残片，并提示只显示了末尾
        if let Some(pos) = text.find('\n') {
            text.drain(..=pos);
        }
        text.insert_str(0, "…（日志过长，仅显示末尾部分）\n");
    }
    Ok(text)
}

// ---------- 窗口大小与位置记忆 ----------

#[derive(Serialize, Deserialize)]
pub(crate) struct WindowState {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub maximized: bool,
}

fn window_state_path() -> PathBuf {
    repo::data_dir().join("window.json")
}

/// 启动时读取上次退出时的窗口状态（无记录返回 None，用默认尺寸居中）。
pub(crate) fn load_window_state() -> Option<WindowState> {
    let text = std::fs::read_to_string(window_state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// 记录当前窗口物理坐标/尺寸与最大化状态（CloseRequested 与 Destroyed 时调用；
/// 物理像素存储，跨 DPI 显示器恢复可能偏移，可接受）。
pub(crate) fn save_window_state(win: &tauri::WebviewWindow) {
    let Ok(pos) = win.outer_position() else { return };
    let Ok(size) = win.inner_size() else { return };
    let state = WindowState {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
        maximized: win.is_maximized().unwrap_or(false),
    };
    let _ = std::fs::create_dir_all(repo::data_dir());
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = std::fs::write(window_state_path(), json);
    }
}

#[derive(Serialize)]
pub(crate) struct VersionInfo {
    pub shell: String,
    pub dsh: String,
    pub repo: String,
}

/// Tauri 命令：版本信息（关于对话框展示）。
#[tauri::command]
pub(crate) fn version_info(token: String) -> Result<VersionInfo, String> {
    guard(&token)?;
    let repo = repo::locate_repo();
    Ok(VersionInfo {
        shell: env!("CARGO_PKG_VERSION").to_string(),
        dsh: repo.as_deref().map(repo::repo_dsh_version).unwrap_or_default(),
        repo: repo.map(|r| r.to_string_lossy().into_owned()).unwrap_or_default(),
    })
}

/// Tauri 命令：壳页面诊断上报（iframe 加载完成等关键节点写入日志，便于验证）。
#[tauri::command]
pub(crate) fn shell_report(token: String, msg: String) -> Result<(), String> {
    guard(&token)?;
    log(&format!("[壳页面] {msg}"));
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct ShellState {
    pub url: Option<String>,
}

/// Tauri 命令：查询服务器状态。webview 刷新后 server-ready 事件已错过，
/// 壳页面加载时主动查询恢复（不再重复显示首启提示）。
#[tauri::command]
pub(crate) fn shell_state(
    token: String,
    state: tauri::State<'_, ServerUrl>,
) -> Result<ShellState, String> {
    guard(&token)?;
    Ok(ShellState { url: state.0.lock().unwrap().clone() })
}

/// 注入到每个页面/iframe（js_init_script_on_all_frames）：
/// 1. 屏蔽无用的默认右键菜单（刷新/另存为/打印等；输入区保留右键粘贴）
/// 2. 拦截 F5 / Ctrl+R——刷新壳页面会丢失状态，刷新 iframe 无意义
/// 3. 外部链接拦截：点击指向非本地地址的 <a> 时阻止默认导航
///    - iframe（dsh 页面）：postMessage 给壳页面（window.top）转发。
///      WebView2 上 tauri 把全局 API 注入了所有 frame，iframe 里
///      window.__TAURI__ 是存在的，但它既没有调用令牌、来源端口也不在
///      nav 白名单里，直接调用没有意义，交给壳页面处理更清晰
///    - 主 frame（壳页面）：直接 invoke Rust 命令开系统浏览器（带调用令牌）
///      （on_new_window 兜底 target=_blank / window.open）
pub(crate) const FRAME_GUARD_JS: &str = r#"
    document.addEventListener('contextmenu', function (e) {
        var t = e.target;
        var editable = t && (t.isContentEditable || /^(INPUT|TEXTAREA)$/.test(t.tagName));
        if (!editable) e.preventDefault();
    }, true);
    document.addEventListener('keydown', function (e) {
        if (e.key === 'F5' || (e.ctrlKey && !e.shiftKey && (e.key === 'r' || e.key === 'R'))) {
            e.preventDefault();
        }
    }, true);
    document.addEventListener('click', function (e) {
        var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
        if (!a) return;
        var href = a.href || '';
        if (!/^https?:\/\//.test(href)) return;
        if (/^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:\d+)?(\/|$)/.test(href)) return;
        e.preventDefault();
        e.stopPropagation();
        if (window.parent && window.parent !== window) {
            // iframe（含 dsh 页面内更深层的嵌套 frame）：一律 postMessage 给壳页面
            var top = window.top || window.parent;
            try { top.postMessage({ __dshShellOpenExternal: href }, '*'); } catch (_) {}
        } else if (window.__TAURI__ && window.__TAURI__.core) {
            var m = /[?&]k=([^&#]+)/.exec(location.search || '');
            var tk = m ? decodeURIComponent(m[1]) : '';
            window.__TAURI__.core.invoke('open_external', { url: href, token: tk }).catch(function () {});
        }
    }, true);
"#;

/// Tauri 命令：用系统默认浏览器打开外部 URL（仅 http/https）。
/// 由注入脚本（主 frame）或壳页面（转发 iframe 的 postMessage）调用。
#[tauri::command]
pub(crate) fn open_external(token: String, url: String) -> Result<(), String> {
    guard(&token)?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http/https 链接".into());
    }
    log(&format!("外部链接转浏览器: {url}"));
    tauri_plugin_opener::open_url(&url, None::<String>).map_err(|e| e.to_string())
}

/// Tauri 命令：更新成功后由前端调用 —— 杀服务器并重启应用。
/// 关闭确认会被 SKIP_CLOSE_CONFIRM 放行（restart 走窗口销毁流程）。
#[tauri::command]
pub(crate) fn restart_app(token: String, app: AppHandle) -> Result<(), String> {
    guard(&token)?;
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    kill_server(&app);
    app.restart()
}

/// Tauri 命令：前端自定义关闭模态的「退出」按钮 —— 确认后结束整个应用。
/// 主窗口销毁后若仅剩系统托盘，Tauri 不会自动退出进程，必须显式 exit，
/// 否则点「退出」只是关了窗口、程序仍在后台运行（托盘残留）。
/// SKIP 标志防止再次触发关闭确认。
#[tauri::command]
pub(crate) fn confirm_close(token: String, app: AppHandle) -> Result<(), String> {
    guard(&token)?;
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    kill_server(&app);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.destroy();
    }
    app.exit(0);
    Ok(())
}

/// 托盘菜单「退出」：用户已通过菜单明确表达退出意图，不再二次弹窗
/// （窗口可能正藏在托盘里，前端弹窗根本看不到），直接结束进程。
/// 这条路径不经过前端，因此不做令牌校验——托盘菜单只存在于本进程窗口内，
/// 不构成对外调用面。
pub(crate) fn force_exit(app: &AppHandle) {
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    kill_server(app);
    app.exit(0);
}
