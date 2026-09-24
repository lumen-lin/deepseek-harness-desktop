//! dsh 主题偏好读取与跟随（与 dsh 应用共享同一份配置，读取细节见 settings.rs）。

use tauri::{window::Color, AppHandle, Manager};

use crate::logging::log;
use crate::settings;

/// 主题偏好在 dsh 配置里的条目名（旧版是 settings.yaml 的节名，新版是补丁条目的 id）。
const THEME_ENTRY: &str = "ui-theme";

/// 读 `ui-theme.preference`（light/dark/system），失败回 system（跟随系统）。
///
/// 版式适配（settings.yaml ↔ profile 补丁文档）统一由 settings.rs 处理；
/// dsh 未来若再改版式/字段名，此处拿不到值也只是主题跟随失效、回退跟随系统，
/// 不影响壳的其他功能。
fn read_theme_preference() -> String {
    let Some(pref) = settings::entry_string(THEME_ENTRY, "preference") else {
        return "system".into();
    };
    if ["light", "dark", "system"].contains(&pref.as_str()) {
        return pref;
    }
    // 认不出的取值（dsh 将来加档位）按「跟随系统」处理，比按浅色更少误伤
    "system".into()
}

/// 读取 dsh 偏好并应用为窗口初始主题（开窗前调用，避免首帧闪色）。
pub(crate) fn apply_theme_preference(app: &AppHandle) {
    let pref = read_theme_preference();
    log(&format!("dsh 主题偏好: {pref}"));
    apply_preference(app, &pref);
}

/// 把已解析出的偏好落到窗口上：`set_theme` 决定 WebView2 的 prefers-color-scheme
/// （壳页面配色靠它），`set_background_color` 管页面画出来之前的窗口底色。
fn apply_preference(app: &AppHandle, pref: &str) {
    let dark = match pref {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None, // system：跟随系统
    };
    if let Some(dark) = dark {
        app.set_theme(if dark { Some(tauri::Theme::Dark) } else { Some(tauri::Theme::Light) });
    } else {
        app.set_theme(None);
    }
    if let Some(win) = app.get_webview_window("main") {
        let dark_now = dark.unwrap_or_else(|| win.theme().map(|t| t == tauri::Theme::Dark).unwrap_or(false));
        let color = if dark_now { Color(0x14, 0x16, 0x1c, 0xff) } else { Color(0xf5, 0xf6, 0xfa, 0xff) };
        let _ = win.set_background_color(Some(color));
    }
}

/// 建窗前的深浅判断：dsh 偏好显式指定时按偏好，否则跟随系统
/// （Windows 注册表 AppsUseLightTheme，0 为深色；读取失败按浅色）。
pub(crate) fn native_theme_prefers_dark() -> bool {
    match read_theme_preference().as_str() {
        "dark" => true,
        "light" => false,
        _ => {
            #[cfg(windows)]
            {
                use std::process::Command as StdCommand;
                StdCommand::new("reg")
                    .args([
                        "query",
                        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                        "/v", "AppsUseLightTheme",
                    ])
                    .output()
                    .map(|o| {
                        let text = String::from_utf8_lossy(&o.stdout);
                        text.contains("0x0")
                    })
                    .unwrap_or(false)
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
    }
}

/// 主题跟随：dsh 应用内切换主题时壳立即同步；应用外改配置文件同样覆盖。
pub(crate) fn spawn_theme_watcher(handle: AppHandle) {
    settings::spawn_watch("主题", read_theme_preference, move |pref| {
        apply_preference(&handle, &pref);
    });
}

