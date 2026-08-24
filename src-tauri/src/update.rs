//! 仓库自动更新：先 git fetch 对比（已最新则跳过），再 pull → install → build。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::logging::log;
use crate::repo;
use crate::server::{creation_flags_windows, kill_server, start_server, DSH_PORT};

/// 更新步骤标签（前端进度列表与此一一对应）：git pull → pnpm install → pnpm build。
const UPDATE_STEP_LABELS: &[&str] = &[
    "拉取官方最新代码（git pull）",
    "安装依赖（pnpm install）",
    "重新构建（pnpm run build）",
];

#[derive(Serialize, Clone)]
pub(crate) struct UpdateStepEvent {
    pub index: usize,
    pub status: String, // running | done | failed
}

#[derive(Serialize, Clone)]
pub(crate) struct UpdateLogEvent<'a> {
    pub text: &'a str,
}

#[derive(Serialize)]
pub(crate) struct UpdateResultPayload {
    pub ok: bool,
    pub failed_step: Option<String>,
    pub output_tail: Option<String>,
    /// 已是最新（fetch 对比后无更新）：前端提示并返回应用，不走更新流程。
    pub already_latest: bool,
}

/// 静默跑 git 子命令并取 stdout（不出控制台窗口）。
fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = creation_flags_windows(&mut cmd)
        .output()
        .map_err(|e| format!("无法运行 git: {e}"))?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr).chars().take(500).collect();
        return Err(format!("git {} 失败：{}", args.join(" "), tail));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// fetch 远端并对比本地 HEAD 与 upstream；一致则说明已是最新，无需更新。
fn check_latest(app: &AppHandle, repo: &Path) -> Result<bool, String> {
    log(app, "检查更新：git fetch origin");
    git_output(repo, &["fetch", "origin"])?;
    let local = git_output(repo, &["rev-parse", "HEAD"])?;
    let remote = git_output(repo, &["rev-parse", "@{u}"])?;
    log(app, &format!("版本对比 本地 {} vs 远端 {}", &local[..7.min(local.len())], &remote[..7.min(remote.len())]));
    Ok(local == remote)
}

#[derive(Serialize)]
pub(crate) struct CheckResultPayload {
    /// 远端是否有新版本（本地落后远端）。
    pub has_update: bool,
    /// 本地 HEAD 短 hash。
    pub local: String,
    /// 远端 upstream 短 hash。
    pub remote: String,
}

/// Tauri 命令：只检查不执行——fetch 并对比版本，返回结果由前端决定是否更新。
/// 不触碰服务器，用户"只想看看有没有新版本"的场景随时可安全返回。
#[tauri::command]
pub(crate) async fn check_update(app: AppHandle) -> Result<CheckResultPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log(&app, "检查更新：git fetch origin");
    git_output(&repo, &["fetch", "origin"])?;
    let local = git_output(&repo, &["rev-parse", "HEAD"])?;
    let remote = git_output(&repo, &["rev-parse", "@{u}"])?;
    log(&app, &format!("版本对比 本地 {} vs 远端 {}", &local[..7.min(local.len())], &remote[..7.min(remote.len())]));
    Ok(CheckResultPayload {
        has_update: local != remote,
        local: local[..7.min(local.len())].to_string(),
        remote: remote[..7.min(remote.len())].to_string(),
    })
}

/// 探测 pnpm 的实际调用方式，返回 (程序, 前置参数)。
/// Windows 上 CreateProcess 只认 .exe：npm 全局安装的 pnpm 只是 .cmd 垫片
/// （按 PATHEXT 找扩展名的约定只属于 shell，不属于操作系统 API），直接
/// spawn "pnpm" 必然失败——必须沿 PATH 找出垫片完整路径再启动；
/// 独立安装版是 pnpm.exe；都没有时回退 Node 自带的 corepack
/// （corepack pnpm … 等价于 pnpm …）。
/// 非 Windows 上 pnpm 是脚本，exec 可直接执行，无需探测。
fn locate_pnpm() -> Option<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        fn find_in_path(names: &[&str]) -> Option<PathBuf> {
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
        if let Some(pnpm) = find_in_path(&["pnpm.cmd", "pnpm.exe"]) {
            return Some((pnpm.to_string_lossy().into_owned(), vec![]));
        }
        if let Some(corepack) = find_in_path(&["corepack.cmd", "corepack.exe"]) {
            return Some((corepack.to_string_lossy().into_owned(), vec!["pnpm".into()]));
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(("pnpm".into(), vec![]))
    }
}

/// 组装三条更新命令。pnpm 两条复用探测结果（含 corepack 前置参数）。
fn update_commands(pnpm: &(String, Vec<String>)) -> Vec<(String, Vec<String>)> {
    let (program, prefix) = pnpm;
    vec![
        ("git".into(), vec!["pull".into(), "--ff-only".into()]),
        (program.clone(), [prefix.as_slice(), &["install".to_string()]].concat()),
        (program.clone(), [prefix.as_slice(), &["run".to_string(), "build".to_string()]].concat()),
    ]
}

/// 执行一步更新命令，输出逐行推给前端；返回 (是否成功, 输出尾部)。
fn run_update_step(app: &AppHandle, repo: &Path, index: usize, label: &str, program: &str, args: &[String]) -> (bool, String) {
    log(app, &format!("更新步骤[{label}] 开始"));
    let _ = app.emit("update-step", UpdateStepEvent { index, status: "running".into() });

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = creation_flags_windows(&mut cmd).spawn() else {
        return (false, format!("无法启动 {program}（未安装或不在 PATH）"));
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut streams: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
    if let Some(s) = stdout { streams.push(Box::new(s)); }
    if let Some(s) = stderr { streams.push(Box::new(s)); }
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() { break; }
            }
        });
    }
    drop(tx);

    let mut output = String::new();
    while let Ok(line) = rx.recv() {
        output.push_str(&line);
        output.push('\n');
        log(app, &line);
        let _ = app.emit("update-log", UpdateLogEvent { text: &format!("{line}\n") });
        if output.len() > 6000 {
            output = output[output.len() - 6000..].to_string();
        }
    }
    let status = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    let ok = status == 0;
    let _ = app.emit("update-step", UpdateStepEvent { index, status: if ok { "done".into() } else { "failed".into() } });
    log(app, &format!("更新步骤[{label}] {}", if ok { "完成" } else { "失败" }));
    (ok, output)
}

/// 更新进行中标记：进入构建段（杀服务器后）置位，CloseRequested 据此拦截关闭，
/// 防止用户在构建期间误关窗口导致构建中断、产物半成品（白屏事故根因）。
static UPDATING: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_updating() -> bool {
    UPDATING.load(Ordering::SeqCst)
}

/// RAII 守卫：任何返回路径（含 panic）都复位 UPDATING。
struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

/// Tauri 命令：执行完整更新流程。前端 invoke，事件驱动进度。
/// force = true 时跳过"已是最新"短路，强制重新执行 install + build
/// （用于构建曾被中断、产物与源码脱节的自救场景）。
#[tauri::command]
pub(crate) async fn run_update(app: AppHandle, force: Option<bool>) -> Result<UpdateResultPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    let force = force.unwrap_or(false);

    // 先 fetch 对比：已是最新且非强制重建则直接返回，服务器原样在跑，应用不受影响
    if check_latest(&app, &repo)? && !force {
        log(&app, "已是最新版本，跳过更新流程");
        return Ok(UpdateResultPayload { ok: true, failed_step: None, output_tail: None, already_latest: true });
    }
    if force {
        log(&app, "强制重建：跳过版本检查，直接执行构建流程");
    }

    // 先探测 pnpm 调用方式（.cmd 垫片 / .exe / corepack）再杀服务器：
    // 探测失败直接返回，服务器原样在跑，应用不受影响（若先杀后探，
    // 失败路径上服务器已死且无人重启，应用就残废了）
    let pnpm = locate_pnpm().ok_or(
        "找不到 pnpm 或 corepack：请安装 Node.js（自带 corepack），或在终端运行 npm install -g pnpm",
    )?;
    log(&app, &format!("pnpm 调用方式: {} {}", pnpm.0, pnpm.1.join(" ")));
    let commands = update_commands(&pnpm);

    kill_server(&app);
    UPDATING.store(true, Ordering::SeqCst);
    let _guard = UpdateGuard;

    let mut failure: Option<(usize, String)> = None;
    for (i, (program, args)) in commands.iter().enumerate() {
        let (ok, output) = run_update_step(&app, &repo, i, UPDATE_STEP_LABELS[i], program, args);
        if !ok {
            failure = Some((i, output));
            break;
        }
    }

/// 更新失败后恢复服务器（回退旧版，固定端口优先，占用则回退随机端口）。
/// 成功路径不调用：成功后由前端 exit_app 直接退出（服务器已在构建前关闭）。
fn restore_server(app: &AppHandle, repo: &Path) {
    match start_server(app, repo, DSH_PORT).or_else(|_| start_server(app, repo, 0)) {
        Ok(url) => {
            let _ = app.emit("server-restored", serde_json::json!({ "url": url }));
        }
        Err(e) => {
            log(app, &format!("服务器恢复失败: {e}"));
        }
    }
}

    match failure {
        None => {
            // 成功：不恢复服务器、不重启——前端提示后调用 exit_app 直接退出，
            // 用户重新打开应用即使用新版本（服务器已随构建前关闭，保持关闭状态）
            log(&app, "更新完成（服务器保持关闭，应用即将退出）");
            Ok(UpdateResultPayload { ok: true, failed_step: None, output_tail: None, already_latest: false })
        }
        Some((i, output)) => {
            restore_server(&app, &repo);
            Ok(UpdateResultPayload {
                ok: false,
                failed_step: Some(UPDATE_STEP_LABELS[i].to_string()),
                output_tail: Some(output.chars().rev().take(1500).collect::<String>().chars().rev().collect()),
                already_latest: false,
            })
        }
    }
}
