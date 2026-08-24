//! dsh 主题偏好读取与跟随（与 dsh 应用共享同一份 settings.yaml）。

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tauri::{window::Color, AppHandle, Manager};

use crate::logging::log;

fn dsh_home() -> PathBuf {
    if let Ok(env) = std::env::var("DSH_HOME") {
        let trimmed = env.trim().to_string();
        if !trimmed.is_empty() {
            let p = if trimmed == "~" {
                dirs_home()
            } else if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
                dirs_home().join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            return p;
        }
    }
    dirs_home().join(".dsh")
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 读 ui-theme.preference（light/dark/system），失败回 system（跟随系统）。
/// serde_yaml 正式解析取字段（此前是字符串 contains 匹配，注释或其他字段
/// 出现同样字样会误判）；dsh 未来若改字段名，此处拿不到值也只是主题跟随
/// 失效、回退跟随系统，不影响壳的其他功能。
fn read_theme_preference() -> String {
    let Ok(text) = fs::read_to_string(dsh_home().join("settings.yaml")) else {
        return "system".into();
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return "system".into();
    };
    if let Some(pref) = value
        .get("ui-theme")
        .and_then(|theme| theme.get("preference"))
        .and_then(|p| p.as_str())
        && ["light", "dark", "system"].contains(&pref)
    {
        return pref.into();
    }
    "system".into()
}

/// 读取 dsh 偏好并应用为窗口初始主题（开窗前调用，避免首帧闪色）。
pub(crate) fn apply_theme_preference(app: &AppHandle) {
    let pref = read_theme_preference();
    log(app, &format!("dsh 主题偏好: {pref}"));
    let dark = match pref.as_str() {
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

/// 主题跟随：notify 监听 settings.yaml 变化（dsh 应用内切换主题时
/// 立即生效；应用外的文件修改同样覆盖）。
/// 实现要点：dsh 保存设置是「写临时文件 + rename 顶替」（原子写入），
/// 监听单个文件会在顶替后失效，所以监听 ~/.dsh 目录、按路径过滤出
/// settings.yaml 的事件；保留 30s 兜底重查防文件系统事件偶发丢失；
/// 监听器建立失败（罕见）时回退纯轮询。
pub(crate) fn spawn_theme_watcher(handle: AppHandle) {
    std::thread::spawn(move || {
        use notify::Watcher;
        let settings = dsh_home().join("settings.yaml");
        let mut last = read_theme_preference();
        let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
        let watcher = notify::recommended_watcher(tx).and_then(|mut w| {
            let dir = dsh_home();
            let _ = std::fs::create_dir_all(&dir);
            w.watch(&dir, notify::RecursiveMode::NonRecursive)?;
            Ok(w)
        });
        match watcher {
            Ok(w) => {
                let _watcher = w; // 保持监听器存活
                log(&handle, "主题监听已建立（目录监听 + 30s 兜底）");
                loop {
                    let hit = match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(Ok(ev)) => ev.paths.iter().any(|p| p == &settings),
                        Ok(Err(_)) | Err(_) => true, // 事件错误或超时：兜底重查
                    };
                    if hit {
                        // 稍等写入完全落地再读（事件先于文件内容可见的边角情况）
                        std::thread::sleep(Duration::from_millis(50));
                        let now = read_theme_preference();
                        if now != last {
                            log(&handle, &format!("主题偏好变化（文件）: {now}"));
                            last = now;
                            apply_theme_preference(&handle);
                        }
                    }
                }
            }
            Err(e) => {
                log(&handle, &format!("主题监听不可用（{e}），回退 500ms 轮询"));
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let now = read_theme_preference();
                    if now != last {
                        log(&handle, &format!("主题偏好变化（文件）: {now}"));
                        last = now;
                        apply_theme_preference(&handle);
                    }
                }
            }
        }
    });
}
