//! 首次安装向导：环境检测、安装目录状态判定、流式安装官方仓库。
//!
//! 触发时机：启动时 `repo::locate_repo()` 失败（新用户，或仓库被移走/删除）。
//! 壳页面经 `shell_state.need_install` 得知后切到安装视图，整个流程
//! （检测环境 → 选目录 → 克隆/续装 → 构建 → 起服务器）都在壳页面内完成，
//! 替代旧版"弹原生目录框、要求用户自己先手动装好仓库"的硬核门槛。
//!
//! 目录判定是幂等核心（防重复安装/中断恢复）：
//! - empty：目录不存在或为空 → 全新克隆安装；
//! - dsh_valid：仓库 + node_modules + 构建产物齐全 → 直接登记使用；
//! - dsh_incomplete：是仓库但缺 node_modules 或构建产物（上次装了一半）
//!   → 断点续装，只跑 install + build；
//! - clone_broken：只剩 .git 的克隆残骸（clone 中断）→ 提示换目录，不替用户删；
//! - occupied：非空且与 dsh 无关 → 提示换目录，绝不动用户文件。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::logging::log;
use crate::repo;
use crate::server::{
    cleanup_stale_server, creation_flags_windows, preferred_port, start_server, ServerUrl,
};
use crate::update;

/// 首次安装标记：locate_repo 失败时置位，壳页面 bootstrap 查询后进入向导。
pub(crate) struct NeedInstall(pub Mutex<bool>);

/// 官方仓库地址：clone 与"克隆残骸"判定的唯一事实源。
const OFFICIAL_REPO_URL: &str = "https://github.com/deepseek-ai/deepseek-harness.git";

// ---------- 环境检测（git / node / pnpm） ----------

#[derive(Serialize, Clone)]
pub(crate) struct ToolInfo {
    /// 是否可用（node 版本过旧算不可用，原因写进 note）
    pub found: bool,
    /// 版本号原文首行（读不到为空串）
    pub version: String,
    /// 状态说明：""（正常）/ "missing" / "old"（版本过旧）/ "via_corepack"
    pub note: String,
}

#[derive(Serialize)]
pub(crate) struct EnvReport {
    pub git: ToolInfo,
    pub node: ToolInfo,
    pub pnpm: ToolInfo,
    /// 三件套全部可用（node 版本合格）：可以开始安装
    pub ok: bool,
}

/// 沿 PATH 找可执行文件。
/// Windows 上 CreateProcess 只认带扩展名的完整路径（.cmd 垫片不会自动匹配），
/// 所以每个候选名都要带扩展名。
#[cfg(windows)]
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let full = dir.join(name);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// 静默跑 `--version` 类探测命令，取 stdout（空则取 stderr）首行。
fn probe_version(exe: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = creation_flags_windows(&mut cmd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let text = if text.is_empty() {
        String::from_utf8_lossy(&out.stderr).trim().to_string()
    } else {
        text
    };
    text.lines().next().map(|s| s.to_string())
}

/// 定位 git：PATH 优先，常见安装目录兜底（刚装完 Git 没重开终端时 PATH 未刷新）。
fn locate_git() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = find_on_path(&["git.exe"]) {
            return Some(p);
        }
        for c in [
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files (x86)\Git\cmd\git.exe",
            r"C:\Soft\Git\cmd\git.exe",
        ] {
            let p = PathBuf::from(c);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from("git"))
    }
}

/// 定位 node：PATH 优先，官方安装目录兜底（刚装完 Node 没重开终端的场景）。
fn locate_node() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = find_on_path(&["node.exe"]) {
            return Some(p);
        }
        let p = PathBuf::from(r"C:\Program Files\nodejs\node.exe");
        if p.is_file() {
            return Some(p);
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from("node"))
    }
}

/// 判定 node 版本是否满足 dsh 的 engines：`^22.19.0 || >=24.0.0`。
/// 解析失败返回 None（版本号读不懂时不拦，交给构建阶段见分晓）。
fn node_version_ok(version_text: &str) -> Option<bool> {
    let v = version_text.trim().trim_start_matches(['v', 'V']);
    let mut it = v.split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next().unwrap_or("0").parse().ok()?;
    Some((major == 22 && minor >= 19) || major >= 24)
}

/// 定位 pnpm：先走 update 模块的常规探测（PATH 上 pnpm.cmd/pnpm.exe → corepack），
/// 找不到再试 node 安装目录旁的 pnpm/corepack（PATH 未刷新场景的最后兜底）。
fn locate_pnpm_ex() -> Option<(String, Vec<String>)> {
    if let Some(p) = update::locate_pnpm() {
        return Some(p);
    }
    #[cfg(windows)]
    {
        if let Some(node) = locate_node() {
            if let Some(dir) = node.parent() {
                let pnpm = dir.join("pnpm.cmd");
                if pnpm.is_file() {
                    return Some((pnpm.to_string_lossy().into_owned(), vec![]));
                }
                let corepack = dir.join("corepack.cmd");
                if corepack.is_file() {
                    return Some((corepack.to_string_lossy().into_owned(), vec!["pnpm".into()]));
                }
            }
        }
    }
    None
}

fn detect_git() -> ToolInfo {
    match locate_git() {
        Some(exe) => ToolInfo {
            found: true,
            version: probe_version(&exe, &["--version"]).unwrap_or_default(),
            note: String::new(),
        },
        None => ToolInfo { found: false, version: String::new(), note: "missing".into() },
    }
}

fn detect_node() -> ToolInfo {
    match locate_node() {
        Some(exe) => {
            let ver = probe_version(&exe, &["--version"]).unwrap_or_default();
            match node_version_ok(&ver) {
                Some(true) => ToolInfo { found: true, version: ver, note: String::new() },
                // 版本过旧：dsh 的 engines 硬性要求，装下去 build 必挂，提前拦
                Some(false) => ToolInfo { found: false, version: ver, note: "old".into() },
                // 版本号读不懂：node 在就放行
                None => ToolInfo { found: true, version: ver, note: String::new() },
            }
        }
        None => ToolInfo { found: false, version: String::new(), note: "missing".into() },
    }
}

fn detect_pnpm() -> ToolInfo {
    match locate_pnpm_ex() {
        Some((program, prefix)) => {
            let mut args = prefix.clone();
            args.push("--version".into());
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let ver = probe_version(Path::new(&program), &arg_refs).unwrap_or_default();
            ToolInfo {
                found: true,
                version: ver,
                note: if prefix.is_empty() { String::new() } else { "via_corepack".into() },
            }
        }
        None => ToolInfo { found: false, version: String::new(), note: "missing".into() },
    }
}

/// Tauri 命令：检测 git / node / pnpm 三件套（向导第一步自动调用）。
#[tauri::command]
pub(crate) fn check_env(token: String) -> Result<EnvReport, String> {
    crate::commands::guard(&token)?;
    let git = detect_git();
    let node = detect_node();
    let pnpm = detect_pnpm();
    let ok = git.found && node.found && pnpm.found;
    log(&format!(
        "环境检测: git={} node={}({}) pnpm={}({}) → ok={}",
        git.found, node.version, node.note, pnpm.version, pnpm.note, ok
    ));
    Ok(EnvReport { git, node, pnpm, ok })
}

// ---------- 安装目录判定（幂等核心） ----------

#[derive(Serialize)]
pub(crate) struct InspectResult {
    /// empty / dsh_valid / dsh_incomplete / clone_broken / occupied / invalid
    pub state: String,
    /// dsh_valid 时为版本号；dsh_incomplete 时为缺失项说明；其余为空
    pub detail: String,
}

/// 判定安装目录状态（纯只读，不做任何修改）。
fn inspect_dir(dir: &Path) -> InspectResult {
    if !dir.exists() {
        // 不存在：git clone 会自动创建
        return InspectResult { state: "empty".into(), detail: String::new() };
    }
    if !dir.is_dir() {
        return InspectResult { state: "invalid".into(), detail: String::new() };
    }
    if repo::is_repo_root(dir) {
        let has_modules = dir.join("node_modules").is_dir();
        // 构建产物以 dsh CLI 入口为准（与 server.rs 的 CLI_ENTRIES 正常路径一致）
        let has_build = dir.join("apps").join("cli").join("lib").join("bin.js").is_file();
        if has_modules && has_build {
            return InspectResult { state: "dsh_valid".into(), detail: repo::repo_dsh_version(dir) };
        }
        let mut missing: Vec<&str> = Vec::new();
        if !has_modules {
            missing.push("node_modules");
        }
        if !has_build {
            missing.push("构建产物");
        }
        return InspectResult { state: "dsh_incomplete".into(), detail: missing.join("、") };
    }
    if dir.join(".git").exists() {
        // 只剩 .git 没有工作区：是官方仓库的克隆残骸，还是用户自己的 git 项目？
        // 看 remote——只有官方残骸才提示"上次克隆中断"，别人的仓库算 occupied。
        let remote = update::git_output(dir, &["remote", "get-url", "origin"]).unwrap_or_default();
        if remote.contains("deepseek-ai/deepseek-harness") {
            return InspectResult { state: "clone_broken".into(), detail: String::new() };
        }
        return InspectResult { state: "occupied".into(), detail: String::new() };
    }
    let is_empty = fs::read_dir(dir).map(|mut it| it.next().is_none()).unwrap_or(false);
    InspectResult {
        state: if is_empty { "empty" } else { "occupied" }.into(),
        detail: String::new(),
    }
}

/// Tauri 命令：检查用户选定的安装目录（前端输入变化时调用）。
#[tauri::command]
pub(crate) fn inspect_install_dir(token: String, dir: String) -> Result<InspectResult, String> {
    crate::commands::guard(&token)?;
    let d = dir.trim();
    if d.is_empty() {
        return Ok(InspectResult { state: "invalid".into(), detail: String::new() });
    }
    Ok(inspect_dir(Path::new(d)))
}

/// Tauri 命令：默认安装位置（exe 旁的 deepseek-harness 目录，前端预填）。
#[tauri::command]
pub(crate) fn default_install_dir(token: String) -> Result<String, String> {
    crate::commands::guard(&token)?;
    Ok(crate::paths::app_base_dir()
        .join("deepseek-harness")
        .to_string_lossy()
        .into_owned())
}

/// Tauri 命令：弹目录选择框（非阻塞 pick_folder 回调 + mpsc 桥接；阻塞等待放
/// 在 spawn_blocking 线程里，不占异步运行时的 worker）。
#[tauri::command]
pub(crate) async fn pick_install_dir(token: String, app: AppHandle) -> Result<Option<String>, String> {
    crate::commands::guard(&token)?;
    let zh = crate::locale::current_locale().starts_with("zh");
    let title = if zh { "选择 dsh 安装目录" } else { "Choose dsh install folder" };
    tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        app.dialog().file().set_title(title).pick_folder(move |picked| {
            let _ = tx.send(picked.map(|p| p.to_string()));
        });
        rx.recv().map_err(|e| format!("目录选择异常: {e}"))
    })
    .await
    .map_err(|e| format!("目录选择异常终止: {e}"))?
}

// ---------- 流式安装 ----------

/// 安装步骤标签（与前端步骤列表按 index 一一对应）。
/// resume（断点续装）跳过第 0 步，只用后两条。
const INSTALL_STEP_LABELS: &[&str] = &[
    "克隆官方仓库（git clone）",
    "安装依赖（pnpm install）",
    "构建（pnpm run build）",
];

#[derive(Serialize)]
pub(crate) struct InstallResult {
    pub ok: bool,
    pub failed_step: Option<String>,
    pub output_tail: Option<String>,
    /// 安装成功但服务器启动失败：仓库已装好并登记，重开应用即可正常进入
    pub server_start_error: Option<String>,
}

/// Tauri 命令：执行安装。mode = fresh（克隆+装依赖+构建）/ resume（续装）/ use（登记已有仓库直接用）。
#[tauri::command]
pub(crate) async fn run_install(
    token: String,
    app: AppHandle,
    dir: String,
    mode: String,
) -> Result<InstallResult, String> {
    crate::commands::guard(&token)?;
    tauri::async_runtime::spawn_blocking(move || run_install_blocking(app, dir, mode))
        .await
        .map_err(|e| format!("安装任务异常终止: {e}"))?
}

fn run_install_blocking(app: AppHandle, dir: String, mode: String) -> Result<InstallResult, String> {
    let dir = dir.trim().to_string();
    if dir.is_empty() {
        return Err("安装目录为空".into());
    }
    let repo_dir = PathBuf::from(&dir);
    if !matches!(mode.as_str(), "fresh" | "resume" | "use") {
        return Err(format!("未知的安装模式: {mode}"));
    }

    // use：目录必须是完整安装，登记后直接起服务器（不跑任何构建步骤）
    if mode == "use" {
        if inspect_dir(&repo_dir).state != "dsh_valid" {
            return Err("所选目录不再是完整可用的 dsh 安装，请重新检查".into());
        }
        return finish_and_start(&app, &repo_dir);
    }

    let fresh = mode == "fresh";
    // 执行前复检：用户在 UI 上检查完之后可能又动过目录
    let current = inspect_dir(&repo_dir).state;
    let expect = if fresh { "empty" } else { "dsh_incomplete" };
    if current != expect {
        return Err(format!("目录状态已变化（当前：{current}），请重新检查目录"));
    }

    // fresh 需要 git；两种模式都需要 pnpm（探测失败先返回，不动任何东西）
    let git_exe = if fresh {
        Some(locate_git().ok_or("找不到 git：请先安装 Git for Windows（git-scm.com）后重试")?)
    } else {
        None
    };
    let pnpm = locate_pnpm_ex().ok_or(
        "找不到 pnpm 或 corepack：请安装 Node.js（自带 corepack），或在终端运行 npm install -g pnpm",
    )?;

    // resume 只用后两条标签（步骤 index 从 0 重排，前端按 mode 渲染对应数量）
    let labels: Vec<String> = if fresh {
        INSTALL_STEP_LABELS.iter().map(|s| s.to_string()).collect()
    } else {
        INSTALL_STEP_LABELS[1..].iter().map(|s| s.to_string()).collect()
    };
    let total = labels.len();

    // 安装期间置忙：窗口关闭被拦截（可隐藏到托盘让安装后台继续），
    // 防中途杀进程留下难以续装的残骸
    update::set_updating(true);
    struct BusyGuard;
    impl Drop for BusyGuard {
        fn drop(&mut self) {
            update::set_updating(false);
        }
    }
    let _guard = BusyGuard;

    let mut failure: Option<(usize, String)> = None;
    let mut step = 0usize;
    while step < total {
        let label = labels[step].clone();
        let is_clone = fresh && step == 0;
        let is_build = step == total - 1;

        if is_clone {
            let git = git_exe.as_ref().unwrap().to_string_lossy().into_owned();
            // 克隆带网络重试（国内访问 GitHub 常见一次性故障）：每次重来前
            // 先清掉自己刚留下的残骸（clone 失败后目录非空，直接重试必报错）
            let mut attempt = 0usize;
            loop {
                if repo_dir.exists() {
                    let _ = fs::remove_dir_all(&repo_dir);
                }
                if let Some(parent) = repo_dir.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let workdir = repo_dir
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                let args = vec![
                    "clone".to_string(),
                    // 管道下 git 默认不打进度，强制打开让用户看到百分比
                    "--progress".to_string(),
                    OFFICIAL_REPO_URL.to_string(),
                    dir.clone(),
                ];
                let (ok, output) = update::run_update_step(
                    &app, &workdir, step, &label, &git, &args, &[],
                    "install-step", "install-log",
                );
                if ok {
                    break;
                }
                if attempt < 2 && update::looks_like_network_error(&output) {
                    attempt += 1;
                    log(&format!("克隆遭遇网络故障，{} 秒后重试（第 {}/2 次）", attempt * 5, attempt));
                    std::thread::sleep(std::time::Duration::from_secs(attempt as u64 * 5));
                    continue;
                }
                failure = Some((step, output));
                break;
            }
        } else {
            // install / build（resume 模式下步骤 0 = install）
            let (program, prefix) = &pnpm;
            let mut args: Vec<String> = prefix.clone();
            if is_build {
                args.extend(["run".to_string(), "build".to_string()]);
            } else {
                // confirmModulesPurge：非 TTY 下 pnpm 清理 modules 前会等确认而假死，显式关闭
                args.extend(["install".to_string(), "--config.confirmModulesPurge=false".to_string()]);
            }
            // official profile：注入官方发布环境（DSH_CLIENT_TITLE 等），
            // 缺了它 dsh 界面会显示「DSH 本地构建」（与更新流程同一约定）
            let envs: update::StepEnvs =
                if is_build { &[("DSH_BUILD_CLIENT_PROFILE", "official")] } else { &[] };
            let (ok, output) = update::run_update_step(
                &app, &repo_dir, step, &label, program, &args, envs,
                "install-step", "install-log",
            );
            if !ok {
                failure = Some((step, output));
            }
        }

        if failure.is_some() {
            break;
        }
        step += 1;
    }

    match failure {
        None => {
            log("安装完成，登记仓库并启动服务器");
            finish_and_start(&app, &repo_dir)
        }
        Some((idx, output)) => Ok(InstallResult {
            ok: false,
            failed_step: Some(labels[idx].clone()),
            output_tail: Some(update::tail_chars(&output, 1500)),
            server_start_error: None,
        }),
    }
}

/// 装完/选用后的统一收尾：写配置 → 清标记 → 清残留服务器 → 起服务器 → 通知壳页面。
fn finish_and_start(app: &AppHandle, repo_dir: &Path) -> Result<InstallResult, String> {
    repo::write_repo_config(repo_dir);
    if let Some(state) = app.try_state::<NeedInstall>() {
        *state.0.lock().unwrap() = false;
    }
    cleanup_stale_server();
    match start_server(app, repo_dir, preferred_port()) {
        Ok(url) => {
            if let Some(state) = app.try_state::<ServerUrl>() {
                *state.0.lock().unwrap() = Some(url.clone());
            }
            let _ = app.emit("server-ready", crate::server::ServerReadyPayload { url });
            Ok(InstallResult { ok: true, failed_step: None, output_tail: None, server_start_error: None })
        }
        Err(e) => {
            // 仓库已装好并登记：本次只是服务器没起来，重开应用即可，不算安装失败
            log(&format!("安装后服务器启动失败（仓库已登记，重开应用可恢复）: {e}"));
            Ok(InstallResult {
                ok: false,
                failed_step: Some("启动服务".into()),
                output_tail: None,
                server_start_error: Some(e),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个"像 dsh 仓库"的目录（可控是否带 node_modules 与构建产物）。
    fn make_repo_like(dir: &Path, with_modules: bool, with_build: bool) {
        fs::create_dir_all(dir.join("apps").join("cli")).unwrap();
        fs::write(
            dir.join("apps").join("cli").join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"9.9.9"}"#,
        )
        .unwrap();
        if with_modules {
            fs::create_dir_all(dir.join("node_modules")).unwrap();
        }
        if with_build {
            fs::create_dir_all(dir.join("apps").join("cli").join("lib")).unwrap();
            fs::write(dir.join("apps").join("cli").join("lib").join("bin.js"), "//").unwrap();
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dsh-install-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn inspect_nonexistent_is_empty() {
        let d = test_dir("nonexistent");
        assert_eq!(inspect_dir(&d).state, "empty");
    }

    #[test]
    fn inspect_empty_dir_is_empty() {
        let d = test_dir("empty");
        fs::create_dir_all(&d).unwrap();
        assert_eq!(inspect_dir(&d).state, "empty");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inspect_full_repo_is_valid() {
        let d = test_dir("valid");
        make_repo_like(&d, true, true);
        let r = inspect_dir(&d);
        assert_eq!(r.state, "dsh_valid");
        assert_eq!(r.detail, "9.9.9");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inspect_repo_without_build_is_incomplete() {
        let d = test_dir("no-build");
        make_repo_like(&d, true, false);
        let r = inspect_dir(&d);
        assert_eq!(r.state, "dsh_incomplete");
        assert!(r.detail.contains("构建产物"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inspect_repo_without_modules_is_incomplete() {
        let d = test_dir("no-modules");
        make_repo_like(&d, false, true);
        let r = inspect_dir(&d);
        assert_eq!(r.state, "dsh_incomplete");
        assert!(r.detail.contains("node_modules"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inspect_occupied_dir_is_occupied() {
        let d = test_dir("occupied");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("readme.txt"), "hi").unwrap();
        assert_eq!(inspect_dir(&d).state, "occupied");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn node_version_rules() {
        // dsh engines: ^22.19.0 || >=24.0.0
        assert_eq!(node_version_ok("v22.19.0"), Some(true));
        assert_eq!(node_version_ok("v22.22.2"), Some(true));
        assert_eq!(node_version_ok("v22.18.9"), Some(false));
        assert_eq!(node_version_ok("v24.0.0"), Some(true));
        assert_eq!(node_version_ok("v23.5.0"), Some(false)); // 23 不在任一区间
        assert_eq!(node_version_ok("v20.11.0"), Some(false));
        assert_eq!(node_version_ok("garbage"), None);
    }
}
