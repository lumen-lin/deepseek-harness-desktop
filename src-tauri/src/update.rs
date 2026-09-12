//! 仓库自动更新：先 git fetch 对比（已最新则跳过），再 pull → install → build。

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::logging::log;
use crate::repo;
use crate::server::{creation_flags_windows, kill_server, preferred_port, start_server, ServerUrl};

/// 更新步骤标签（前端进度列表与此一一对应）：
/// git pull → pnpm install → pnpm run clean → pnpm run build。
///
/// clean 是后补的关键一步：`pnpm run build` 内部是「tsc -b 增量编译 → tsdown
/// 打包」，而 tsdown 的入口就是 tsc 的**上一次**产物（各包的 lib/types/*.js）。
/// 跨版本 git pull 后，这些旧产物不会自动失效——它们引用的是旧版源码的导出，
/// 于是 tsdown 拿着旧产物去匹配新源码，报 MISSING_EXPORT（"xxx 未被导出"）。
/// 先 clean 掉全部 lib 与 .tsbuildinfo，让 tsc 全量重编，产物才与源码一致。
const UPDATE_STEP_LABELS: &[&str] = &[
    "拉取官方最新代码（git pull）",
    "安装依赖（pnpm install）",
    "清理旧构建产物（pnpm run clean）",
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
pub(crate) fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
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

/// 清除残留的 .git/index.lock。
///
/// 存在意义：git 被强行中断（更新流程被打断、窗口被杀、手动 Ctrl+C）后会留下
/// index.lock 且不会自动清理。此后**任何写索引的操作**（pull / reset / checkout）
/// 都会以「Unable to create '.git/index.lock': File exists」直接失败，更新流程
/// 卡死在第一步，且报错信息与真实原因毫不相干。这里在每次写操作前主动清掉。
///
/// 保险：60 秒内新建的 lock 视为"另一个 git 正在运行"，不碰它。
fn clear_stale_git_lock(repo: &Path) {
    let lock = repo.join(".git").join("index.lock");
    let Ok(meta) = fs::metadata(&lock) else { return };
    let age = meta
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    match age {
        Some(secs) if secs < 60 => {
            log(&format!(".git/index.lock 存在但仅 {secs} 秒前创建，判为 git 正在运行，跳过清理"));
        }
        Some(secs) => {
            log(&format!("清除残留的 .git/index.lock（已存在 {secs} 秒，上次 git 操作被中断）"));
            let _ = fs::remove_file(&lock);
        }
        None => {
            log("清除残留的 .git/index.lock（无法读取时间，按陈旧锁处理）");
            let _ = fs::remove_file(&lock);
        }
    }
}

/// 还原被本地弄脏的托管文件（package.json / pnpm-lock.yaml）。
///
/// 存在意义：pnpm install 的副作用或手工微调常把这两个文件改脏。pull 时它们
/// 与上游改动冲突 → 触发自动 stash → 改动被藏起来（用户以为修好了，下次又冒
/// 出来），stash 还越堆越多。这两个文件属于「跟随上游」的托管文件，不是用户
/// 资产，还原它们让 pull 保持纯快进。其它文件的本地改动仍走原来的 stash 路径。
fn restore_managed_files(repo: &Path) {
    for name in ["package.json", "pnpm-lock.yaml"] {
        if !repo.join(name).is_file() {
            continue;
        }
        // git diff --quiet 在有改动时返回非 0：只有真脏了才还原，避免无谓调用
        if git_output(repo, &["diff", "--quiet", "--", name]).is_err() {
            log(&format!("还原本地改动：{name}（跟随上游的托管文件）"));
            if let Err(e) = git_output(repo, &["checkout", "--", name]) {
                log(&format!("还原 {name} 失败: {e}"));
            }
        }
    }
}

/// fetch 远端并返回 (本地 HEAD, 远端 upstream, 是否一致)。
fn fetch_and_compare(repo: &Path) -> Result<(String, String, bool), String> {
    // 写索引前先清陈旧锁：否则 fetch 后的 pull / reset 一定失败
    clear_stale_git_lock(repo);
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

/// 检查更新返回：本地 HEAD + 全部可选版本（master + 远端 dsh-* tags）。
/// 前端据此渲染版本下拉框，用户可自选更新目标（默认跟随 master）。
#[derive(Serialize, Clone)]
pub(crate) struct VersionEntry {
    /// 传给 run_update 的 target 值："master" 或 tag 名（如 "dsh-v0.1.0-rc.8"）。
    pub reference: String,
    /// 该版本指向的 commit 完整 hash（tag 为注解标签时取 peeled 目标）。
    pub commit: String,
    /// 版本类别："latest"（master）/ "alpha" / "rc" / "stable"。
    pub kind: String,
    /// 是否等于本地当前 HEAD。
    pub is_current: bool,
    /// 该版本此前是否被记录为"构建失败"。
    pub known_bad: bool,
}

#[derive(Serialize)]
pub(crate) struct VersionsPayload {
    pub local: String,
    pub entries: Vec<VersionEntry>,
}

/// Tauri 命令：只检查不执行——fetch（含 tags）并返回可选版本列表。
/// 不触碰服务器，用户"只想看看有什么版本"的场景随时可安全返回。
///
/// `git fetch` 是网络阻塞调用（国内可能几十秒），用它包一层 spawn_blocking，
/// 免得占着异步运行时的 worker 线程。
#[tauri::command]
pub(crate) async fn check_update(token: String) -> Result<VersionsPayload, String> {
    crate::commands::guard(&token)?;
    tauri::async_runtime::spawn_blocking(check_update_blocking)
        .await
        .map_err(|e| format!("检查更新异常终止: {e}"))?
}

fn check_update_blocking() -> Result<VersionsPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    clear_stale_git_lock(&repo);
    log("检查更新：git fetch origin --tags");
    // --force：本地 tag 可能与上游移动/删除的旧引用冲突，强制对齐远端
    git_output(&repo, &["fetch", "origin", "--tags", "--force"])?;
    let local = git_output(&repo, &["rev-parse", "HEAD"])?;
    // 黑名单读一次即可：下面要对 master + 每个 tag 判断，逐个读文件是几十次磁盘 IO
    let bad = repo::bad_remotes();

    let mut entries: Vec<VersionEntry> = Vec::new();
    // master（官方最新主线，跟随自动更新）
    if let Ok(m) = git_output(&repo, &["rev-parse", "origin/master"]) {
        if !m.is_empty() {
            entries.push(VersionEntry {
                reference: "master".into(),
                commit: m.clone(),
                kind: "latest".into(),
                is_current: m == local,
                known_bad: bad.contains(&m),
            });
        }
    }
    // 远端 dsh-* tags（release / rc / alpha 全列出来供选择，含历史稳定版）
    // %(*objectname) 是注解标签的 peeled 目标（真正的 commit）
    if let Ok(tags) = git_output(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)\t%(objectname)\t%(*objectname)",
            "refs/tags/dsh-*",
        ],
    ) {
        for line in tags.lines() {
            let mut it = line.split('\t');
            let full_ref = it.next().unwrap_or("");
            let obj = it.next().unwrap_or("");
            let peeled = it.next().unwrap_or("");
            let tag_name = full_ref.strip_prefix("refs/tags/").unwrap_or(full_ref);
            if tag_name.is_empty() {
                continue;
            }
            let commit = if peeled.is_empty() { obj } else { peeled };
            if commit.is_empty() {
                continue;
            }
            let kind = if tag_name.contains("-alpha") {
                "alpha"
            } else if tag_name.contains("-rc") {
                "rc"
            } else {
                "stable"
            };
            entries.push(VersionEntry {
                reference: tag_name.to_string(),
                commit: commit.to_string(),
                kind: kind.into(),
                is_current: commit == local,
                known_bad: bad.contains(commit),
            });
        }
    }
    // master 置顶，其余按 tag 名降序（版本号新者在前；tag 前缀 dsh-v + 定宽数字，
    // 字典序即可近似版本序）
    use std::cmp::Ordering;
    entries.sort_by(|a, b| match (a.kind == "latest", b.kind == "latest") {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => b.reference.cmp(&a.reference),
    });
    Ok(VersionsPayload {
        local: local.clone(),
        entries,
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

/// 判断 git 失败输出是否为临时网络故障（值得原样重试）。
///
/// 存在意义：国内访问 GitHub 常出现 "Empty reply from server"、
/// "CONNECT tunnel failed" 之类的一次性错误。不重试的话，用户点一次更新
/// 就看到「拉取代码失败」，误以为是功能坏了。
pub(crate) fn looks_like_network_error(output: &str) -> bool {
    let s = output.to_lowercase();
    [
        "empty reply from server",
        "connection timed out",
        "failed to connect",
        "couldn't connect to server",
        "could not connect to server",
        "connect tunnel failed",
        "connection reset by peer",
        "unable to access",
        "early eof",
        "rpc failed",
        "the remote end hung up",
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
pub(crate) fn locate_pnpm() -> Option<(String, Vec<String>)> {
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

/// 每步命令的额外环境变量（构建步骤注入 official profile，其余步为空）。
pub(crate) type StepEnvs = &'static [(&'static str, &'static str)];

/// 一条更新命令：程序、参数、环境变量、以及「失败是否可容忍」。
///
/// 只有 clean 可容忍失败：它只是删除产物，失败（例如目录被占用）最多导致
/// 后续重建不彻底，但绝不能因此中断更新——留着旧产物照样进 build 更糟的是
/// 直接把可用版本判死。clean 失败时记录日志并继续。
type UpdateCommand = (String, Vec<String>, StepEnvs, bool);

/// 组装更新命令。pnpm 三条复用探测结果（含 corepack 前置参数）。
fn update_commands(pnpm: &(String, Vec<String>)) -> Vec<UpdateCommand> {
    let (program, prefix) = pnpm;
    vec![
        ("git".into(), vec!["pull".into(), "--ff-only".into()], &[], false),
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
            &[],
            false,
        ),
        // clean：清掉各包 lib 产物与 .tsbuildinfo，强制 tsc 全量重编。
        // 不做这一步，tsdown 会把上一次的旧产物当入口打包，跨版本更新后必然
        // 报 MISSING_EXPORT（"某导出不存在"）——那是产物陈旧，不是源码有问题。
        // 上游 scripts/clean.ts 只删构建输出与孤儿目录，不碰 node_modules。
        (
            program.clone(),
            [prefix.as_slice(), &["run".to_string(), "clean".to_string()]].concat(),
            &[],
            true,
        ),
        // official profile：上游 scripts/build.ts 支持 --profile official /
        // DSH_BUILD_CLIENT_PROFILE=official，构建时注入官方发布环境
        // （DSH_CLIENT_TITLE=DeepSeek Harness 等，commit 与版本号自动取自
        // git HEAD 与 package.json）。缺了它前端回退显示「DSH 本地构建」，
        // 这就是侧边栏出现"本地构建"字样的原因。
        (
            program.clone(),
            [prefix.as_slice(), &["run".to_string(), "build".to_string()]].concat(),
            &[("DSH_BUILD_CLIENT_PROFILE", "official")],
            false,
        ),
    ]
}

/// 执行一步命令，输出逐行推给前端；返回 (是否成功, 输出尾部)。
/// 更新与首次安装共用：step_event / log_event 是前端进度与日志的事件名
/// （更新用 "update-step"/"update-log"，安装用 "install-step"/"install-log"）。
/// workdir 是命令工作目录（更新=仓库目录；安装的 clone 步骤=目标父目录）。
pub(crate) fn run_update_step(app: &AppHandle, workdir: &Path, index: usize, label: &str, program: &str, args: &[String], envs: StepEnvs, step_event: &str, log_event: &str) -> (bool, String) {
    log(&format!("执行步骤[{label}] 开始"));
    let _ = app.emit(step_event, UpdateStepEvent { index, status: "running".into() });

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(envs.iter().copied())
        .current_dir(workdir)
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
        let _ = app.emit(log_event, UpdateLogEvent { text: &format!("{line}\n") });
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
    let _ = app.emit(step_event, UpdateStepEvent { index, status: if ok { "done".into() } else { "failed".into() } });
    log(&format!("执行步骤[{label}] {}", if ok { "完成" } else { "失败" }));
    (ok, output)
}

/// 更新进行中标记：进入构建段（杀服务器后）置位，CloseRequested 据此拦截关闭，
/// 防止用户在构建期间误关窗口导致构建中断、产物半成品（白屏事故根因）。
static UPDATING: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_updating() -> bool {
    UPDATING.load(Ordering::SeqCst)
}

/// 置忙/闲标记：首次安装流程（install.rs）期间也要拦住窗口关闭，
/// 与更新共用同一标志（前端提示文案已泛化为"安装/更新"）。
pub(crate) fn set_updating(v: bool) {
    UPDATING.store(v, Ordering::SeqCst);
}

/// RAII 守卫：任何返回路径（含 panic）都复位 UPDATING。
struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

/// 取字符串末尾最多 n 个字符（按字符边界，不会切断多字节字符）。
pub(crate) fn tail_chars(s: &str, n: usize) -> String {
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
    // 端口回退（3080 被占用则改用随机端口）已收在 start_server 内部
    match start_server(app, repo, preferred_port()) {
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
    for (i, (program, args, envs, _tolerated)) in commands.iter().enumerate().skip(1) {
        let label = format!("回滚重建 · {}", UPDATE_STEP_LABELS[i]);
        let (ok, _) = run_update_step(app, repo, i, &label, program, args, envs, "update-step", "update-log");
        if !ok {
            // clean 失败可容忍（与正向更新同策略），其余失败即回滚重建失败
            if *_tolerated {
                log(&format!("回滚重建：步骤[{}]失败但可容忍，继续", UPDATE_STEP_LABELS[i]));
                continue;
            }
            log(&format!("回滚重建失败于：{}", UPDATE_STEP_LABELS[i]));
            return false;
        }
    }
    log("回滚重建完成：源码已回到更新前版本，产物完整");
    true
}

/// 目标版本字符串白名单：只允许 "master" 或形如 dsh-v1.2.3-alpha.1 的 tag 名。
/// 防注入：拒绝空串、超长、路径穿越（..）、@、前导 -/ 等危险字符。
fn is_safe_version_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && !s.starts_with('-')
        && !s.starts_with('/')
        && !s.contains("..")
        && !s.contains('@')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// 切换到指定 tag 时用的 (本地分支名, 远端 ref)。
///
/// 统一用固定的 "dsh-selected" 分支：重复切换用 -B 覆盖，避免每选一次就多一个
/// 分支；也不用 detached HEAD，因为后续操作需要分支。
///
/// 这里**只处理 tag**。跟随 master 走的是 `git pull --ff-only`（见 run_update）——
/// 那条路只做快进，不会覆盖本地分支上已提交的内容。早期版本对 master 也执行
/// `checkout -B master origin/master`，那是强制重置，会把用户在本地的提交丢掉。
fn target_branch_spec(tag: &str) -> (String, String) {
    ("dsh-selected".to_string(), format!("refs/tags/{tag}"))
}

/// Tauri 命令：执行完整更新流程。前端 invoke，事件驱动进度。
/// force = true 时跳过"已是最新"短路，强制重新执行 install + build
/// （用于构建曾被中断、产物与源码脱节的自救场景）。
/// target = 用户显式选择的版本（"master"=跟随最新 / tag 名）；缺省同 master，
/// 但显式指定时**跳过"已是最新"判断**——用户选它就是要切过去（含降级到旧稳定版）。
#[tauri::command]
pub(crate) async fn run_update(
    token: String,
    app: AppHandle,
    force: Option<bool>,
    target: Option<String>,
) -> Result<UpdateResultPayload, String> {
    crate::commands::guard(&token)?;
    // 整个流程是分钟级的阻塞操作（git + pnpm + 构建），
    // 扔到阻塞线程池执行，别占着异步运行时的 worker。
    tauri::async_runtime::spawn_blocking(move || run_update_blocking(app, force, target))
        .await
        .map_err(|e| format!("更新任务异常终止: {e}"))?
}

fn run_update_blocking(
    app: AppHandle,
    force: Option<bool>,
    target: Option<String>,
) -> Result<UpdateResultPayload, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    let force = force.unwrap_or(false);
    let chosen: Option<String> = match target {
        Some(t) if t.trim().is_empty() => None,
        Some(t) if !is_safe_version_ref(t.trim()) => return Err(format!("非法的版本目标: {t}")),
        Some(t) => Some(t.trim().to_string()),
        None => None,
    };
    let choosing = chosen.is_some();
    // 只有"切到具体 tag"才需要 checkout 覆盖分支；选 master（或未选）走快进 pull
    let switch_to_tag: Option<String> = chosen
        .as_deref()
        .filter(|t| *t != "master")
        .map(|t| t.to_string());

    // 未显式选版本：先 fetch 对比，已是最新且非强制重建则直接返回（服务器原样在跑）
    if !choosing {
        let (_, _, latest) = fetch_and_compare(&repo)?;
        if latest && !force {
            log("已是最新版本，跳过更新流程");
            return Ok(UpdateResultPayload {
                ok: true, failed_step: None, output_tail: None, already_latest: true,
                prev_head: None, stashed_changes: false,
                auto_rolled_back: false, rollback_rebuild_failed: false,
            });
        }
    } else {
        // 显式选版本：fetch tags（确保目标对象在本地可 checkout），并做存在性校验
        let want = chosen.as_deref().unwrap();
        log(&format!("检查更新：git fetch origin --tags（目标 {want}）"));
        clear_stale_git_lock(&repo);
        git_output(&repo, &["fetch", "origin", "--tags", "--force"])?;
        let spec = match switch_to_tag.as_deref() {
            Some(tag) => format!("refs/tags/{tag}"),
            None => "origin/master".to_string(),
        };
        if git_output(&repo, &["rev-parse", "--verify", &spec]).is_err() {
            return Err(format!("远端不存在所选版本: {want}"));
        }
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
    // 组装步骤命令：切 tag 时第 0 步是「git checkout -B dsh-selected refs/tags/<tag>」
    // （切到所选版本，含降级），否则保持默认的 `git pull --ff-only` 跟随 master。
    // force（强制重建）从步骤 1 开始、绝不触碰源码版本——保持既有语义不变。
    let base_commands = update_commands(&pnpm);
    let commands: Vec<UpdateCommand> = if let Some(tag) = &switch_to_tag {
        let (branch, spec) = target_branch_spec(tag);
        let mut v = vec![(
            "git".to_string(),
            vec!["checkout".to_string(), "-B".to_string(), branch, spec],
            &[] as StepEnvs,
            false,
        )];
        v.extend(base_commands[1..].iter().cloned());
        v
    } else {
        base_commands
    };
    // 步骤标签：切 tag 时第 0 步改称「切换版本」（前端步骤列表文本同步变化）
    let labels: Vec<String> = if let Some(tag) = &switch_to_tag {
        let mut v = vec![format!("切换到所选版本 {tag}")];
        v.extend(UPDATE_STEP_LABELS[1..].iter().map(|s| s.to_string()));
        v
    } else {
        UPDATE_STEP_LABELS.iter().map(|s| s.to_string()).collect()
    };

    // 更新前基线提交：pull 成功但后续步骤失败时，供「回滚到更新前」使用
    let prev_head = git_output(&repo, &["rev-parse", "HEAD"]).ok();

    // pull 前先清陈旧锁 + 还原托管文件：两者任一没做，pull 都会以与真实原因
    // 无关的报错失败（index.lock / 本地改动冲突），看似"更新功能坏了"
    clear_stale_git_lock(&repo);
    restore_managed_files(&repo);

    kill_server(&app);
    UPDATING.store(true, Ordering::SeqCst);
    let _guard = UpdateGuard;

    // stash 自动善后只试一次；step 不递增即重跑当前步（目前只有 git pull 需要）
    let mut failure: Option<(usize, String)> = None;
    let mut stashed_changes = false;
    let mut pull_retries = 0usize;
    // force（强制重建）= 只重建本地产物，绝不触碰源码版本：
    // 远端版本可能自身构建不过（上游 bug），一 pull 就把可用源码换成坏代码，
    // 反而把"重建自救"变成"再次变砖"。因此从步骤 1（install）开始。
    let mut step = if force { 1 } else { 0usize };
    if force {
        log("强制重建：跳过 git pull，仅用当前源码重装依赖并重建产物");
    }
    while step < commands.len() {
        let (program, args, envs, tolerated) = &commands[step];
        let (ok, output) = run_update_step(&app, &repo, step, &labels[step], program, args, envs, "update-step", "update-log");
        if ok {
            step += 1;
            continue;
        }
        // 可容忍步骤（clean）失败：记日志继续，不中断更新
        if *tolerated {
            log(&format!(
                "步骤[{}]失败但可容忍，继续下一步（失败输出尾部：{}）",
                labels[step],
                tail_chars(&output, 300)
            ));
            step += 1;
            continue;
        }
        // 第 0 步（git pull / git checkout）遭遇临时网络故障：等几秒原样重试（最多 3 次）
        if step == 0 && pull_retries < 3 && looks_like_network_error(&output) {
            pull_retries += 1;
            log(&format!(
                "第 0 步遭遇网络故障，{} 秒后重试（第 {}/3 次）：{}",
                pull_retries * 5,
                pull_retries,
                tail_chars(&output, 200)
            ));
            std::thread::sleep(std::time::Duration::from_secs(pull_retries as u64 * 5));
            continue;
        }
        // 第 0 步被本地未提交改动阻止（pull / checkout -B 都会因工作树改动失败）：
        // 自动 stash（含未跟踪文件）后重试一次。不自动 pop——恢复时机由用户决定。
        if step == 0 && !stashed_changes && looks_like_local_change_conflict(&output) {
            log("检测到本地未提交改动阻止版本切换，自动执行 git stash 暂存");
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
            // 本次更新的目标版本已验证可构建：从坏版本黑名单移除（成功即证其可构建，
            // 无论走 pull 还是显式选版本——HEAD 此时就是刚构建成功的目标版本）
            if let Ok(head) = git_output(&repo, &["rev-parse", "HEAD"]) {
                if prev_head.as_ref().map(|p| p != &head).unwrap_or(true) {
                    repo::clear_bad_remote(&head);
                }
            }
            Ok(UpdateResultPayload {
                ok: true, failed_step: None, output_tail: None, already_latest: false,
                prev_head: None, stashed_changes,
                auto_rolled_back: false, rollback_rebuild_failed: false,
            })
        }
        Some((step_index, output)) => {
            // HEAD 已前移（pull/checkout 成功、install/build 失败）→ 可回滚到更新前
            let head_now = git_output(&repo, &["rev-parse", "HEAD"]).ok();
            // 构建步骤失败：常见于上游源码自身编译不过（导出缺失等）。
            // 只有 HEAD 真的切到了新版本才记黑名单（否则失败可能是本地问题），
            // 之后检查更新时提前警告，避免反复更新到同一个坑。
            if step_index + 1 == UPDATE_STEP_LABELS.len() {
                if let Some(h) = &head_now {
                    if prev_head.as_ref() != Some(h) {
                        log(&format!(
                            "记录构建失败的版本 {}（后续检查更新将提示暂缓）",
                            &h[..7.min(h.len())]
                        ));
                        repo::mark_bad_remote(h);
                    }
                }
            }
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
                failed_step: Some(labels[step_index].clone()),
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
pub(crate) async fn rollback_update(
    token: String,
    app: AppHandle,
    commit: String,
) -> Result<(), String> {
    crate::commands::guard(&token)?;
    // reset + 重建同样是分钟级阻塞操作，放阻塞线程池
    tauri::async_runtime::spawn_blocking(move || rollback_update_blocking(app, commit))
        .await
        .map_err(|e| format!("回滚任务异常终止: {e}"))?
}

fn rollback_update_blocking(app: AppHandle, commit: String) -> Result<(), String> {
    let commit = commit.trim().to_lowercase();
    if commit.len() < 7 || commit.len() > 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("非法的提交哈希: {commit}"));
    }
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log(&format!("回滚到更新前提交 {}", &commit[..8.min(commit.len())]));
    kill_server(&app);
    // reset --hard 同样要写索引：先清陈旧锁，否则回滚也卡在同一处
    clear_stale_git_lock(&repo);
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
/// 固定端口优先，占用则回退随机端口（回退逻辑在 start_server 内部）；
/// 就绪后同步 ServerUrl 状态并广播 server-restored（壳页面据此重新装载 iframe），
/// URL 同时直接返回给调用方。
/// 走 spawn_blocking：start_server 最长可能要等 60 秒，不能占着异步 worker。
#[tauri::command]
pub(crate) async fn restart_server(token: String, app: AppHandle) -> Result<String, String> {
    crate::commands::guard(&token)?;
    tauri::async_runtime::spawn_blocking(move || restart_server_blocking(app))
        .await
        .map_err(|e| format!("重启服务异常终止: {e}"))?
}

fn restart_server_blocking(app: AppHandle) -> Result<String, String> {
    let repo = repo::locate_repo().ok_or("仓库位置不可用")?;
    log("手动重启 dsh web 服务器");
    // 先清理可能残留的旧服务器进程，避免双开
    kill_server(&app);
    match start_server(&app, &repo, preferred_port()) {
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
