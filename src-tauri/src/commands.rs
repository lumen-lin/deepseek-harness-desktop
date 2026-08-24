//! 壳页面 Tauri 命令、帧守卫注入脚本、外部链接转发。

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::logging::{log, log_dir};
use crate::repo;
use crate::server::{kill_server, ServerUrl};

/// restart_app 路径下跳过关闭确认（restart 会走窗口销毁流程，不能被确认框卡住）。
pub(crate) static SKIP_CLOSE_CONFIRM: AtomicBool = AtomicBool::new(false);

pub(crate) fn skip_close_confirm() -> bool {
    SKIP_CLOSE_CONFIRM.load(Ordering::SeqCst)
}

/// 在资源管理器中打开目录（Windows explorer / 类 Unix xdg-open）。
fn open_in_explorer(path: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        Command::new("explorer")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Tauri 命令：打开 deepseek-harness 仓库目录（帮助菜单）。
#[tauri::command]
pub(crate) fn open_repo_dir(app: AppHandle) -> Result<(), String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log(&app, "打开仓库目录");
    open_in_explorer(&repo.to_string_lossy())
}

/// Tauri 命令：打开数据与日志目录（帮助菜单）。
#[tauri::command]
pub(crate) fn open_logs_dir(app: AppHandle) -> Result<(), String> {
    log(&app, "打开日志目录");
    open_in_explorer(&log_dir().to_string_lossy())
}

#[derive(Serialize)]
pub(crate) struct VersionInfo {
    pub shell: String,
    pub dsh: String,
    pub repo: String,
}

/// Tauri 命令：版本信息（关于对话框展示）。
#[tauri::command]
pub(crate) fn version_info() -> VersionInfo {
    let repo = repo::locate_repo();
    VersionInfo {
        shell: env!("CARGO_PKG_VERSION").to_string(),
        dsh: repo.as_deref().map(repo::repo_dsh_version).unwrap_or_default(),
        repo: repo.map(|r| r.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

/// Tauri 命令：壳页面诊断上报（iframe 加载完成等关键节点写入日志，便于验证）。
#[tauri::command]
pub(crate) fn shell_report(app: AppHandle, msg: String) {
    log(&app, &format!("[壳页面] {msg}"));
}

#[derive(Serialize)]
pub(crate) struct ShellState {
    pub url: Option<String>,
}

/// Tauri 命令：查询服务器状态。webview 刷新后 server-ready 事件已错过，
/// 壳页面加载时主动查询恢复（不再重复显示首启提示）。
#[tauri::command]
pub(crate) fn shell_state(state: tauri::State<'_, ServerUrl>) -> ShellState {
    ShellState { url: state.0.lock().unwrap().clone() }
}

/// 注入到每个页面/iframe（js_init_script_on_all_frames）：
/// 1. 屏蔽无用的默认右键菜单（刷新/另存为/打印等；输入区保留右键粘贴）
/// 2. 拦截 F5 / Ctrl+R——刷新壳页面会丢失状态，刷新 iframe 无意义
/// 3. 外部链接拦截：点击指向非本地地址的 <a> 时阻止默认导航
///    - iframe（dsh 页面）：postMessage 给壳页面（window.top）转发。
///      注意不能先试 __TAURI__.core.invoke——WebView2 上 tauri 会把全局 API
///      注入所有 frame，iframe 里 window.__TAURI__ 存在，但来源
///      http://127.0.0.1:3080 无 IPC 权限会被 ACL 静默拒绝，必须走 postMessage
///    - 主 frame（壳页面）：直接 invoke Rust 命令开系统浏览器
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
        if (/^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:\d+)?\//.test(href)) return;
        e.preventDefault();
        e.stopPropagation();
        if (window.parent && window.parent !== window) {
            // iframe（含 dsh 页面内更深层的嵌套 frame）：一律 postMessage 给壳页面
            var top = window.top || window.parent;
            try { top.postMessage({ __dshShellOpenExternal: href }, '*'); } catch (_) {}
        } else if (window.__TAURI__ && window.__TAURI__.core) {
            window.__TAURI__.core.invoke('open_external', { url: href }).catch(function () {});
        }
    }, true);
"#;

/// Tauri 命令：用系统默认浏览器打开外部 URL（仅 http/https）。
/// 由注入脚本（主 frame）或壳页面（转发 iframe 的 postMessage）调用。
#[tauri::command]
pub(crate) fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http/https 链接".into());
    }
    log(&app, &format!("外部链接转浏览器: {url}"));
    tauri_plugin_opener::open_url(&url, None::<String>).map_err(|e| e.to_string())
}

/// Tauri 命令：更新成功后由前端调用 —— 杀服务器并重启应用。
/// 关闭确认会被 SKIP_CLOSE_CONFIRM 放行（restart 走窗口销毁流程）。
#[tauri::command]
pub(crate) fn restart_app(app: AppHandle) {
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    kill_server(&app);
    app.restart();
}

/// Tauri 命令：前端自定义关闭模态的「关闭」按钮 —— 确认后销毁主窗口
/// （Destroyed 事件里会杀服务器进程树）。SKIP 标志防止再次触发确认。
#[tauri::command]
pub(crate) fn confirm_close(app: AppHandle) {
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.destroy();
    }
}

/// Tauri 命令：更新完成后由前端调用 —— 杀服务器并直接退出应用（不重启）。
/// 用户重新打开即使用新版本。
#[tauri::command]
pub(crate) fn exit_app(app: AppHandle) {
    SKIP_CLOSE_CONFIRM.store(true, Ordering::SeqCst);
    kill_server(&app);
    app.exit(0);
}
