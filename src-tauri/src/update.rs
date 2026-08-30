//! 仓库自动更新：先 git fetch 对比（已最新则跳过），再 pull → install → build。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::logging::log;
use crate::repo;
use crate::server::{creation_flags_windows, kill_server, start_server, DSH_PORT, ServerUrl};

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
    /// 失败且 HEAD 已前移（pull 成功、后续步骤失败）时提供更新前完整
    /// commit：前端显示「回滚到更新前」按钮，一键退回旧版源码。
    pub prev_head: Option<String>,
    /// pull 被本地未提交改动阻止时已自动 stash：提示用户可用 git stash pop 找回。
    pub stashed_changes: bool,
    /// 更新失败后已自动回滚源码并重建成功：服务器已恢复为更新前版本运行。
    pub auto_rolled_back: bool,
    /// 回滚后的重建也失败（连更新前版本都构建不过）：需用户手动介入。
    pub rollback_rebuild_failed: bool,
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

/// fetch 远端并返回 (本地 HEAD, 远端 upstream, 是否一致)。
fn fetch_and_compare(repo: &Path) -> Result<(String, String, bool), String> {
    log("检查更新：git fetch origin");
    git_output(repo, &["fetch", "origin"])?;
    let local = git_output(repo, &["rev-parse", "HEAD"])?;
    let remote = git_output(repo, &["rev-parse", "@{u}"])?;
    log(&format!(
        "版本对比 本地 {} vs 远端 {}",
        &local[..7.min(local.len())],
        &remote[..7.min(remote.len())]
    ));
    let latest = local == remote;
    Ok((local, remote, latest))
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
pub(crate) async fn check_update() -> Result<CheckResultPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    let (local, remote, _latest) = fetch_and_compare(&repo)?;
    Ok(CheckResultPayload {
        has_update: local != remote,
        local: local[..7.min(local.len())].to_string(),
        remote: remote[..7.min(remote.len())].to_string(),
    })
}

/// 判断 git pull 失败输出是否为「本地未提交改动阻止合并」类冲突。
fn looks_like_local_change_conflict(output: &str) -> bool {
    let s = output.to_lowercase();
    [
        "your local changes",
        "would be overwritten by merge",
        "untracked working tree files",
        "please commit your changes or stash",
    ]
    .iter()
    .any(|k| s.contains(k))
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
        // confirmModulesPurge：本壳以管道捕获输出（非 TTY），pnpm 在需要清理
        // modules 目录时会中止并等待终端确认，表现为"安装无声失败"。
        // 显式关闭该确认，保证非交互场景下安装能继续。
        (
            program.clone(),
            [
                prefix.as_slice(),
                &["install".to_string(), "--config.confirmModulesPurge=false".to_string()],
            ]
            .concat(),
        ),
        (program.clone(), [prefix.as_slice(), &["run".to_string(), "build".to_string()]].concat()),
    ]
}

/// 执行一步更新命令，输出逐行推给前端；返回 (是否成功, 输出尾部)。
fn run_update_step(app: &AppHandle, repo: &Path, index: usize, label: &str, program: &str, args: &[String]) -> (bool, String) {
    log(&format!("更新步骤[{label}] 开始"));
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
        log(&line);
        let _ = app.emit("update-log", UpdateLogEvent { text: &format!("{line}\n") });
        if output.len() > 6000 {
            // 必须按字符边界截断：字节切片落在多字节字符中间会 panic，
            // 而 release 是 panic=abort，进程会瞬间消失（窗口"自动关闭"事故根因）
            let mut start = output.len() - 6000;
            while !output.is_char_boundary(start) {
                start += 1;
            }
            output = output[start..].to_string();
        }
    }
    let status = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    let ok = status == 0;
    let _ = app.emit("update-step", UpdateStepEvent { index, status: if ok { "done".into() } else { "failed".into() } });
    log(&format!("更新步骤[{label}] {}", if ok { "完成" } else { "失败" }));
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

/// 取字符串末尾最多 n 个字符（按字符边界，不会切断多字节字符）。
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        chars[chars.len() - n..].iter().collect()
    }
}

/// 更新失败后恢复服务器（回退旧版，固定端口优先，占用则回退随机端口）。
/// 成功路径不调用：成功后壳页面保留更新结果，由用户手动点击「重启服务」。
fn restore_server(app: &AppHandle, repo: &Path) {
    // 防御性：确保旧进程已清理（正常流程上游已杀，此处兜底）
    kill_server(app);
    match start_server(app, repo, DSH_PORT).or_else(|_| start_server(app, repo, 0)) {
        Ok(url) => {
            let _ = app.emit("server-restored", serde_json::json!({ "url": url }));
        }
        Err(e) => {
            log(&format!("服务器恢复失败: {e}"));
        }
    }
}

/// 回滚源码到基线提交并重新构建（跳过 git pull，只跑 install + build）。
///
/// 存在意义：pull 成功但 install/build 失败时，源码已是新版、产物却是半成品，
/// 此时直接启动服务器必然崩溃（loader 找不到缺失的 lib）。只有把源码退回能
/// 构建的版本并完整重建，才能恢复出一个可用的服务——这是"更新失败后残废"
/// 的自救路径。
fn rollback_and_rebuild(app: &AppHandle, repo: &Path, base: &str, pnpm: &(String, Vec<String>)) -> bool {
    let short = &base[..8.min(base.len())];
    log(&format!("自动回滚源码到更新前版本 {short}"));
    if let Err(e) = git_output(repo, &["reset", "--hard", base]) {
        log(&format!("回滚失败（git reset）: {e}"));
        return false;
    }
    let commands = update_commands(pnpm);
    // 跳过步骤 0（git pull）：源码已回到基线，只需重建依赖与产物
    for (i, (program, args)) in commands.iter().enumerate().skip(1) {
        let label = format!("回滚重建 · {}", UPDATE_STEP_LABELS[i]);
        let (ok, _) = run_update_step(app, repo, i, &label, program, args);
        if !ok {
            log(&format!("回滚重建失败于：{}", UPDATE_STEP_LABELS[i]));
            return false;
        }
    }
    log("回滚重建完成：源码已回到更新前版本，产物完整");
    true
}

/// Tauri 命令：执行完整更新流程。前端 invoke，事件驱动进度。
/// force = true 时跳过"已是最新"短路，强制重新执行 install + build
/// （用于构建曾被中断、产物与源码脱节的自救场景）。
#[tauri::command]
pub(crate) async fn run_update(app: AppHandle, force: Option<bool>) -> Result<UpdateResultPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    let force = force.unwrap_or(false);

    // 先 fetch 对比：已是最新且非强制重建则直接返回，服务器原样在跑，应用不受影响
    let (_local, _remote, latest) = fetch_and_compare(&repo)?;
    if latest && !force {
        log("已是最新版本，跳过更新流程");
        return Ok(UpdateResultPayload {
            ok: true, failed_step: None, output_tail: None, already_latest: true,
            prev_head: None, stashed_changes: false,
            auto_rolled_back: false, rollback_rebuild_failed: false,
        });
    }
    if force {
        log("强制重建：跳过版本检查，直接执行构建流程");
    }

    // 先探测 pnpm 调用方式（.cmd 垫片 / .exe / corepack）再杀服务器：
    // 探测失败直接返回，服务器原样在跑，应用不受影响（若先杀后探，
    // 失败路径上服务器已死且无人重启，应用就残废了）
    let pnpm = locate_pnpm().ok_or(
        "找不到 pnpm 或 corepack：请安装 Node.js（自带 corepack），或在终端运行 npm install -g pnpm",
    )?;
    log(&format!("pnpm 调用方式: {} {}", pnpm.0, pnpm.1.join(" ")));
    let commands = update_commands(&pnpm);

    // 更新前基线提交：pull 成功但后续步骤失败时，供「回滚到更新前」使用
    let prev_head = git_output(&repo, &["rev-parse", "HEAD"]).ok();

    kill_server(&app);
    UPDATING.store(true, Ordering::SeqCst);
    let _guard = UpdateGuard;

    // stash 自动善后只试一次；step 不递增即重跑当前步（目前只有 git pull 需要）
    let mut failure: Option<(usize, String)> = None;
    let mut stashed_changes = false;
    let mut step = 0usize;
    while step < commands.len() {
        let (program, args) = &commands[step];
        let (ok, output) = run_update_step(&app, &repo, step, UPDATE_STEP_LABELS[step], program, args);
        if ok {
            step += 1;
            continue;
        }
        // git pull 被本地未提交改动阻止：自动 stash（含未跟踪文件）后重试一次。
        // 不自动 pop——恢复时机由用户决定，避免与新代码冲突
        if step == 0 && !stashed_changes && looks_like_local_change_conflict(&output) {
            log("检测到本地未提交改动阻止 git pull，自动执行 git stash 暂存");
            match git_output(&repo, &["stash", "push", "--include-untracked", "-m", "dsh-shell auto-stash"]) {
                Ok(_) => {
                    stashed_changes = true;
                    continue;
                }
                Err(e) => log(&format!("git stash 失败: {e}")),
            }
        }
        failure = Some((step, output));
        break;
    }

    match failure {
        None => {
            // 成功：不恢复服务器、不退出应用——壳页面保留更新结果，
            // 前端显示「重启服务」按钮，用户手动调用 restart_server 以新版启动
            log("更新完成（等待用户手动重启服务）");
            Ok(UpdateResultPayload {
                ok: true, failed_step: None, output_tail: None, already_latest: false,
                prev_head: None, stashed_changes,
                auto_rolled_back: false, rollback_rebuild_failed: false,
            })
        }
        Some((step_index, output)) => {
            // HEAD 已前移（pull 成功、install/build 失败）→ 可回滚到更新前
            let head_now = git_output(&repo, &["rev-parse", "HEAD"]).ok();
            let rollback_base = match (&prev_head, &head_now) {
                (Some(a), Some(b)) if a != b => Some(a.clone()),
                _ => None,
            };

            // 源码已前移 + 后续步骤失败 = 产物半成品，直接起服务必然崩溃
            // （loader 找不到缺失模块）。先回滚重建出可用产物，再恢复服务。
            let mut auto_rolled_back = false;
            let mut rollback_rebuild_failed = false;
            if let Some(base) = &rollback_base {
                if rollback_and_rebuild(&app, &repo, base, &pnpm) {
                    auto_rolled_back = true;
                } else {
                    rollback_rebuild_failed = true;
                    log("回滚重建未成功：更新前版本同样无法构建，需手动处理");
                }
            }
            restore_server(&app, &repo);
            Ok(UpdateResultPayload {
                ok: false,
                failed_step: Some(UPDATE_STEP_LABELS[step_index].to_string()),
                output_tail: Some(tail_chars(&output, 1500)),
                already_latest: false,
                prev_head: rollback_base,
                stashed_changes,
                auto_rolled_back,
                rollback_rebuild_failed,
            })
        }
    }
}

/// Tauri 命令：一键回滚到更新前的提交（pull 成功但 install/build 失败的场景）。
/// 杀服务器 → reset --hard 回基线提交 → 重新拉起服务器（旧产物继续服务）。
/// 不自动 pop stash：本地改动是否恢复由用户自行决定，避免冲突。
#[tauri::command]
pub(crate) async fn rollback_update(app: AppHandle, commit: String) -> Result<(), String> {
    let commit = commit.trim().to_lowercase();
    if commit.len() < 7 || commit.len() > 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("非法的提交哈希: {commit}"));
    }
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log(&format!("回滚到更新前提交 {}", &commit[..8.min(commit.len())]));
    kill_server(&app);
    git_output(&repo, &["reset", "--hard", &commit]).map_err(|e| format!("git reset 失败: {e}"))?;
    // 只 reset 不够：node_modules 与产物仍是新版半成品，不重建服务器依旧起不来
    match locate_pnpm() {
        Some(pnpm) => {
            if !rollback_and_rebuild(&app, &repo, &commit, &pnpm) {
                log("回滚后重建失败：服务器可能仍无法启动，可尝试「强制重建」");
            }
        }
        None => log("未找到 pnpm，跳过重建：产物可能与回滚后的源码不匹配"),
    }
    restore_server(&app, &repo);
    log("已回滚到旧版源码，服务器以旧版本重新启动");
    Ok(())
}

/// Tauri 命令：手动重启 dsh web 服务器（更新成功后壳保持打开，由用户点击触发）。
/// 固定端口优先，占用则回退随机端口；就绪后同步 ServerUrl 状态并广播
/// server-restored（壳页面据此重新装载 iframe），URL 同时直接返回给调用方。
/// async：同步命令在主线程执行，start_server 最长阻塞 60 秒会冻结窗口。
#[tauri::command]
pub(crate) async fn restart_server(app: AppHandle) -> Result<String, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log("手动重启 dsh web 服务器");
    // 先清理可能残留的旧服务器进程，避免双开
    kill_server(&app);
    match start_server(&app, &repo, DSH_PORT).or_else(|_| start_server(&app, &repo, 0)) {
        Ok(url) => {
            if let Some(state) = app.try_state::<ServerUrl>() {
                *state.0.lock().unwrap() = Some(url.clone());
            }
            let _ = app.emit("server-restored", serde_json::json!({ "url": url }));
            log(&format!("服务器已重启: {url}"));
            Ok(url)
        }
        Err(e) => {
            log(&format!("服务器重启失败: {e}"));
            Err(e)
        }
    }
}
