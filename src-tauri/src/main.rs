// DeepSeek Harness 桌面端（Tauri 壳）。
// 职责：定位 deepseek-harness 仓库 → 启动 `dsh web` → 解析官方就绪信号
// `dsh web: http://127.0.0.1:<port>` → 窗口加载该地址。
// 仓库更新（git pull/切版本 → pnpm install + clean + build）后在更新页
// 点击「重启服务」即可生效。
//
// 两个安全边界（都由本文件串起来，细节见各自模块）：
// 1. `nav` —— 主窗口只允许停在「本进程自己起的服务端口」上；
// 2. `shell` 的调用令牌 —— 所有壳命令都要求调用方带令牌，挡住同源页面。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod install;
mod locale;
mod logging;
mod nav;
mod paths;
mod repo;
mod server;
mod shell;
mod theme;
mod update;

use std::path::PathBuf;
use std::sync::Mutex;

use logging::log;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{window::Color, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

fn main() {
    // panic 钩子：release 是 panic=abort，进程瞬间消失且无任何界面反馈；
    // 把 panic 信息写进日志（钩子在 abort 前执行），窗口"无故关闭"时可查证
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>().map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string payload>".into());
        let entry = format!("[PANIC] {msg} @ {loc}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logging::log_dir().join("desktop.log"))
            .and_then(|mut f| f.write_all(entry.as_bytes()));
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 再次启动：聚焦已有窗口（不开第二个实例/第二台服务器）
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // 帧守卫脚本必须注入所有 frame：js_init_script 只进主 frame，dsh 页面
        // 全在 iframe 里拿不到（这是上一版外链拦截失败的原因）
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("frame-guard")
                .js_init_script_on_all_frames(commands::FRAME_GUARD_JS)
                .build(),
        )
        .manage(server::ServerProc(Mutex::new(None)))
        .manage(server::ServerUrl(Mutex::new(None)))
        .manage(install::NeedInstall(Mutex::new(false)))
        .invoke_handler(tauri::generate_handler![
            update::check_update,
            update::run_update,
            locale::get_locale,
            commands::version_info,
            commands::open_repo_dir,
            commands::open_logs_dir,
            commands::shell_report,
            commands::shell_state,
            commands::open_external,
            commands::restart_app,
            commands::confirm_close,
            commands::read_log,
            update::restart_server,
            update::rollback_update,
            install::check_env,
            install::inspect_install_dir,
            install::default_install_dir,
            install::pick_install_dir,
            install::run_install,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 壳页面改由本地服务站提供，不再走 Tauri 自定义协议：
            // dsh ≥0.1.2 的浏览器会话 cookie 带 SameSite=Strict，只在「请求站点
            // 等于顶层站点」时发送。壳页面若在 tauri:// 下，dsh iframe 属于跨站
            // 上下文，cookie 既不落地也不回传，首页与 /api 全部 401。
            // 两者同处 127.0.0.1（SameSite 只看 host 不看端口）即可同站。
            // 详见 src/shell.rs 顶部说明。
            let shell_page_src = shell::start_shell_server()?;
            let shell_page: tauri::Url = match shell_page_src.parse() {
                Ok(u) => u,
                Err(e) => return Err(format!("壳页面 URL 非法: {e}").into()),
            };

            // 主窗口用代码创建（而非 tauri.conf.json）：需要挂 on_new_window /
            // on_navigation 两个 handler，它们只存在于 Builder 上。
            // - on_new_window：iframe 里 target=_blank / window.open 的链接
            //   （如插件市场的源码/更新说明链接）→ 转交系统默认浏览器
            // - on_navigation：只放行本进程自己的服务端口（nav 模块），
            //   其余地址一律转浏览器并阻止 webview 自己导航
            let main = tauri::WebviewWindowBuilder::new(
                &handle,
                "main",
                tauri::WebviewUrl::External(shell_page),
            )
            .title("DeepSeek Harness")
            .inner_size(1440.0, 900.0)
            .min_inner_size(980.0, 600.0)
            .visible(false)
            .center()
            .decorations(false)
            // 把拖放事件还给网页：Tauri 默认在窗口上注册原生拖放处理器
            // （Windows 上会顺带把 WebView2 的 AllowExternalDrop 关掉），
            // 导致 dsh 聊天框的 HTML5 拖放（拖图入聊天框）收不到事件。
            // 壳自己不需要壳级拖放功能，关掉原生处理器即可恢复网页拖放。
            .disable_drag_drop_handler()
            .background_color(if theme::native_theme_prefers_dark() {
                Color(0x14, 0x16, 0x1c, 0xff)
            } else {
                Color(0xf5, 0xf6, 0xfa, 0xff)
            })
            .on_new_window(move |url, _features| {
                let u = url.to_string();
                log(&format!("外部链接转浏览器: {u}"));
                let _ = tauri_plugin_opener::open_url(&u, None::<String>);
                tauri::webview::NewWindowResponse::Deny
            })
            .on_navigation(|url| {
                let s = url.as_str();
                let local = nav::is_allowed(s);
                if !local {
                    log(&format!("拦截非本地导航，转系统浏览器: {s}"));
                    let _ = tauri_plugin_opener::open_url(s, None::<String>);
                }
                local
            })
            .build()?;

            // 恢复上次退出时的窗口大小与位置（无记录则保持默认 1440×900 居中）
            let win_state = commands::load_window_state();
            if let Some(st) = &win_state {
                if !st.maximized {
                    let _ = main.set_position(tauri::PhysicalPosition::new(st.x, st.y));
                    let _ = main.set_size(tauri::PhysicalSize::new(st.w, st.h));
                }
            }

            // 先应用 dsh 主题与语言偏好（首帧一次到位），随后立即显示窗口：
            // 服务器启动要几秒，加载页必须先出现，双击才有即时反馈
            theme::apply_theme_preference(app.handle());
            locale::apply_locale(app.handle());
            let _ = main.show();
            if let Some(true) = win_state.as_ref().map(|s| s.maximized) {
                let _ = main.maximize();
            }
            log("窗口已显示（服务器后台启动中）");

            // 服务器存活监控：意外退出时通知壳页面显示断线页。
            // 与仓库定位无关，先起来，等 start_backend 把子进程登记进 ServerProc。
            server::spawn_health_watcher(handle.clone());

            // 主题跟随：监听 settings.yaml 变化
            theme::spawn_theme_watcher(handle.clone());
            // 语言跟随：监听 settings.yaml 变化（dsh 切换中英文时壳同步）
            locale::spawn_locale_watcher(handle.clone());

            // 窗口事件：点 X = 隐藏到托盘（服务与会话继续后台运行，防误关）；
            // 真正退出走托盘菜单「退出」→ 复用壳页面确认弹窗 → confirm_close。
            // 销毁时杀服务器进程树
            {
                let handle = app.handle().clone();
                let main_win = main.clone();
                main.on_window_event(move |event| match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        // restart_app / confirm_close 路径：直接放行（destroy 不走此处）
                        if commands::skip_close_confirm() {
                            return;
                        }
                        commands::save_window_state(&main_win);
                        api.prevent_close();
                        // 交给壳页面弹窗让用户选择：隐藏到托盘 或 退出
                        let _ = main_win.emit(
                            "close-requested",
                            serde_json::json!({ "updating": update::is_updating(), "source": "titlebar" }),
                        );
                    }
                    tauri::WindowEvent::Destroyed => {
                        commands::save_window_state(&main_win);
                        server::kill_server(&handle);
                    }
                    _ => {}
                });
            }

            // 系统托盘：左键单击恢复窗口，右键菜单（显示/退出）
            {
                let zh = locale::current_locale().starts_with("zh");
                let (show_label, quit_label) = if zh {
                    ("显示主窗口", "退出")
                } else {
                    ("Show Window", "Quit")
                };
                let show_item =
                    MenuItem::with_id(app.handle(), "tray-show", show_label, true, None::<&str>)?;
                let quit_item =
                    MenuItem::with_id(app.handle(), "tray-quit", quit_label, true, None::<&str>)?;
                let tray_menu = Menu::with_items(app.handle(), &[&show_item, &quit_item])?;
                let mut tray = TrayIconBuilder::with_id("dsh-tray")
                    .menu(&tray_menu)
                    .show_menu_on_left_click(false)
                    .tooltip("DeepSeek Harness")
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "tray-show" => {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        "tray-quit" => {
                            // 更新中不允许退出：把主窗口调到前台显示拦截提示
                            if update::is_updating() {
                                if let Some(win) = app.get_webview_window("main") {
                                    let _ = win.show();
                                    let _ = win.set_focus();
                                    let _ = win.emit(
                                        "close-requested",
                                        serde_json::json!({ "updating": true, "source": "tray" }),
                                    );
                                }
                                return;
                            }
                            // 明确的菜单操作即视为确认，直接退出（窗口可能藏在
                            // 托盘里，前端弹窗看不到，不能依赖它确认）
                            commands::force_exit(app);
                        }
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    });
                if let Some(icon) = app.default_window_icon().cloned() {
                    tray = tray.icon(icon);
                }
                tray.build(app.handle())?;
                log("系统托盘已创建（点 X 隐藏到托盘）");
            }

            // 仓库定位：失败（新用户/仓库被移走）则标记"首次安装"，
            // 壳页面 bootstrap 查询 shell_state 后进入安装向导（检测环境 →
            // 选目录 → 克隆/续装 → 构建），装完自动起服务器进应用
            match repo::locate_repo() {
                Some(r) => start_backend(handle.clone(), r),
                None => {
                    log("未找到 dsh 仓库，进入首次安装向导");
                    if let Some(state) = handle.try_state::<install::NeedInstall>() {
                        *state.0.lock().unwrap() = true;
                    }
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 仓库就绪后拉起后端：写配置 → 后台线程启动 dsh web → 广播就绪事件。
///
/// 服务器启动放后台线程：setup 立即返回，主线程事件循环保持运转，加载页
/// （转圈动画）即时渲染——若在 setup 里阻塞等服务器，窗口会冻结成白框直到
/// 就绪，主观上"启动慢"正是这么来的。错误弹窗用 blocking API，官方要求
/// 在非主线程调用，本线程正合适。
fn start_backend(handle: tauri::AppHandle, repo: PathBuf) {
    log(&format!("仓库目录: {}", repo.display()));
    repo::write_repo_config(&repo);

    std::thread::spawn(move || {
        // 先清理上次异常退出（崩溃/被强杀）可能遗留的服务器进程
        server::cleanup_stale_server();

        // 端口被占用时的随机端口回退已经收在 start_server 内部，
        // 这里不必再判断错误文本里有没有 EADDRINUSE
        let url = match server::start_server(&handle, &repo, server::preferred_port()) {
            Ok(u) => u,
            Err(e) => {
                log(&format!("启动失败: {e}"));
                let _ = handle.dialog()
                    .message(format!("DeepSeek Harness 启动失败\n\n{e}"))
                    .kind(MessageDialogKind::Error)
                    .blocking_show();
                handle.exit(1);
                return;
            }
        };

        // 记录就绪 URL（webview 刷新后壳页面经 shell_state 恢复），
        // 再通知壳页面装载 iframe
        if let Some(state) = handle.try_state::<server::ServerUrl>() {
            *state.0.lock().unwrap() = Some(url.clone());
        }
        let _ = handle.emit("server-ready", server::ServerReadyPayload { url });
    });
}
