//! dsh 语言偏好读取与跟随（与 dsh 应用共享同一份配置，读取细节见 settings.rs）。
//! 壳页面（标题栏/菜单/加载页/更新页/模态框）根据 dsh 的语言设置同步中英文，
//! 机制与主题跟随一致：配置变化 → 广播 locale-changed 事件。

use tauri::{AppHandle, Emitter};

use crate::logging::log;
use crate::settings;

/// 语言偏好在 dsh 配置里的条目名（旧版是 settings.yaml 的节名，新版是补丁条目的 id）。
const LOCALE_ENTRY: &str = "locale";

/// 读 `locale.preference`（zh/en 等），失败或未知回 zh（保持壳默认中文，
/// 避免用户从未设置语言时界面突然变英文）。
///
/// 版式适配统一由 settings.rs 处理；dsh 未来若改版式/字段名，此处拿不到值
/// 也只是语言跟随失效、回退中文，不影响壳的其他功能。
fn read_locale_preference() -> String {
    if let Some(pref) = settings::entry_string(LOCALE_ENTRY, "preference") {
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

/// 语言跟随：dsh 应用内切换语言时壳立即同步；应用外改配置文件同样覆盖。
/// 实现与主题跟随共用 settings::spawn_watch（它内部就是「目录监听 + 兜底重查」，
/// 从 settings.yaml 时代沿用至今，只是候选目录随 dsh 版本一起扩了）。
pub(crate) fn spawn_locale_watcher(handle: AppHandle) {
    settings::spawn_watch("语言", read_locale_preference, move |locale| {
        let _ = handle.emit("locale-changed", locale);
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
