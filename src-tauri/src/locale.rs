//! dsh 语言偏好读取与跟随（与 dsh 应用共享同一份 settings.yaml）。
//! 壳页面（标题栏/菜单/加载页/更新页/模态框）根据 dsh 的语言设置同步中英文，
//! 机制与主题跟随一致：监听 settings.yaml 变化 → 广播 locale-changed 事件。

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tauri::{AppHandle, Emitter};

use crate::logging::log;
use crate::paths;

fn settings_path() -> PathBuf {
    // 路径只在 paths 里定义一次，避免与 theme.rs 各写一份
    paths::dsh_settings_path()
}

/// 读 locale.preference（zh/en 等），失败或未知回 zh（保持壳默认中文，
/// 避免用户从未设置语言时界面突然变英文）。
/// serde_yaml 正式解析取字段；dsh 未来若改字段名，此处拿不到值也只是
/// 语言跟随失效、回退中文，不影响壳的其他功能。
fn read_locale_preference() -> String {
    let Ok(text) = fs::read_to_string(settings_path()) else {
        return "zh".into();
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return "zh".into();
    };
    if let Some(pref) = value
        .get("locale")
        .and_then(|locale| locale.get("preference"))
        .and_then(|p| p.as_str())
    {
        let lower = pref.to_ascii_lowercase();
        if lower.starts_with("zh") {
            return "zh".into();
        }
        if lower.starts_with("en") {
            return "en".into();
        }
    }
    "zh".into()
}

/// 读取当前语言偏好并广播给壳页面（初始化时调用，壳页面据此渲染文案）。
pub(crate) fn apply_locale(app: &AppHandle) {
    let locale = read_locale_preference();
    log(&format!("dsh 语言偏好: {locale}"));
    let _ = app.emit("locale-changed", locale);
}

/// 语言跟随：notify 监听 settings.yaml 变化（dsh 应用内切换语言时
/// 立即生效；应用外的文件修改同样覆盖）。
/// 实现要点与主题跟随一致：dsh 保存设置是「写临时文件 + rename 顶替」
/// （原子写入），监听单个文件会在顶替后失效，所以监听 ~/.dsh 目录、
/// 按路径过滤出 settings.yaml 的事件；保留 30s 兜底重查；监听器建立
/// 失败（罕见）时回退纯轮询。
pub(crate) fn spawn_locale_watcher(handle: AppHandle) {
    std::thread::spawn(move || {
        use notify::Watcher;
        let settings = settings_path();
        let mut last = read_locale_preference();
        let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
        let watcher = notify::recommended_watcher(tx).and_then(|mut w| {
            let dir = paths::dsh_home();
            let _ = std::fs::create_dir_all(&dir);
            w.watch(&dir, notify::RecursiveMode::NonRecursive)?;
            Ok(w)
        });
        match watcher {
            Ok(w) => {
                let _watcher = w; // 保持监听器存活
                log("语言监听已建立（目录监听 + 30s 兜底）");
                loop {
                    let hit = match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(Ok(ev)) => ev.paths.iter().any(|p| p == &settings),
                        Ok(Err(_)) | Err(_) => true, // 事件错误或超时：兜底重查
                    };
                    if hit {
                        // 稍等写入完全落地再读（事件先于文件内容可见的边角情况）
                        std::thread::sleep(Duration::from_millis(50));
                        let now = read_locale_preference();
                        if now != last {
                            log(&format!("语言偏好变化（文件）: {now}"));
                            last = now.clone();
                            let _ = handle.emit("locale-changed", now);
                        }
                    }
                }
            }
            Err(e) => {
                log(&format!("语言监听不可用（{e}），回退 500ms 轮询"));
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let now = read_locale_preference();
                    if now != last {
                        log(&format!("语言偏好变化（文件）: {now}"));
                        last = now.clone();
                        let _ = handle.emit("locale-changed", now);
                    }
                }
            }
        }
    });
}

/// 当前语言偏好（托盘菜单等 Rust 侧文案用；与壳页面同一数据源）。
pub(crate) fn current_locale() -> String {
    read_locale_preference()
}

/// Tauri 命令：供壳页面初始化时查询当前语言（webview 刷新后事件已错过）。
#[tauri::command]
pub(crate) fn get_locale(token: String) -> Result<String, String> {
    crate::commands::guard(&token)?;
    Ok(read_locale_preference())
}
