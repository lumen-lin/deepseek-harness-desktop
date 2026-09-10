//! dsh web 服务器子进程：启动、就绪解析、结束、孤儿清理、存活监控。

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::logging::log;
use crate::nav;
use crate::repo;

/// 就绪等待上限（秒，默认值）。注意这是**总**时长，不是"每条输出之间的静默时长"。
/// 慢机器上首次冷启动（tsx 现场编译）可能更久，可用环境变量 `DSH_START_TIMEOUT` 覆盖。
const DEFAULT_START_TIMEOUT_SECS: u64 = 60;
/// 服务器端口默认值：与官方 `dsh web` 一致（不传 --port 时 dsh 也用 3080）。
/// 被占用时自动回退随机端口（比如终端里已经手动开着 dsh web）。
/// 可用环境变量 `DSH_PORT` 覆盖。
const DEFAULT_DSH_PORT: u16 = 3080;

/// 首选端口：环境变量优先，其次默认值。
/// 端口属于"跟部署环境相关"的东西，不该写死在代码里 —— 用户如果在别处固定了端口，壳要能跟着走。
pub(crate) fn preferred_port() -> u16 {
    read_env_u16("DSH_PORT").unwrap_or(DEFAULT_DSH_PORT)
}

/// 就绪等待上限（秒）：环境变量优先，其次默认值。
fn start_timeout_secs() -> u64 {
    read_env_u64("DSH_START_TIMEOUT")
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_START_TIMEOUT_SECS)
}

/// 读环境变量为 u64（缺失 / 空白 / 非法一律 None）。
fn read_env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// 读环境变量为 u16（0 视为非法，免得把"端口 0"当成用户意图）。
fn read_env_u16(name: &str) -> Option<u16> {
    let v = read_env_u64(name)?;
    u16::try_from(v).ok().filter(|p| *p > 0)
}

/// 全局可变状态：服务器子进程句柄（用于退出时杀进程树）。
pub(crate) struct ServerProc(pub Mutex<Option<Child>>);
/// 当前就绪 URL：壳页面（webview）被刷新后经 shell_state 查询恢复用。
pub(crate) struct ServerUrl(pub Mutex<Option<String>>);

#[derive(Serialize, Clone)]
pub(crate) struct ServerReadyPayload {
    pub url: String,
}

/// 服务器 pid 文件：崩溃残留时供下次启动清理。
fn pid_file() -> PathBuf {
    repo::data_dir().join("server.pid")
}

/// Windows 下隐藏子进程控制台窗口的公共封装。
#[allow(unused_mut)]
pub(crate) fn creation_flags_windows(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        cmd
    }
}

/// dsh CLI 入口候选（按优先级尝试）及各自的启动方式。
///
/// 做成一张表而不是写死一条 `if` 链：上游若调整目录结构，往这里加一行即可，
/// 不必再动函数体；报错信息也会把尝试过的路径全列出来，便于定位。
struct CliEntry {
    /// 相对仓库根的入口路径
    rel: &'static str,
    /// .ts 源码入口需要先加载 tsx 运行时
    needs_tsx: bool,
}

const CLI_ENTRIES: &[CliEntry] = &[
    // 正常路径：pnpm run build 之后的产物
    CliEntry { rel: "apps/cli/lib/bin.js", needs_tsx: false },
    // 兜底：尚未构建时直接跑源码（要求仓库里已安装 tsx）
    CliEntry { rel: "apps/cli/src/bin.ts", needs_tsx: true },
];

/// 组装 dsh 启动命令。port 0 = 交给 OS 随机分配。
///
/// `--no-open`：官方 d66841ea 起 dsh web 就绪后会默认用系统浏览器打开页面；
/// 桌面壳自己就有窗口，不加会每次启动多弹一个浏览器标签页。
fn server_command(repo: &Path, port: u16) -> Option<(String, Vec<String>)> {
    for entry in CLI_ENTRIES {
        let path = repo.join(entry.rel);
        if !path.is_file() {
            continue;
        }
        let mut args: Vec<String> = Vec::new();
        if entry.needs_tsx {
            args.push("--import".into());
            args.push("tsx/esm".into());
        }
        args.push(path.to_string_lossy().into_owned());
        args.push("web".into());
        args.push("--port".into());
        args.push(port.to_string());
        args.push("--no-open".into());
        return Some(("node".into(), args));
    }
    None
}

/// 启动 dsh web 服务器。
///
/// `preferred_port` 是首选端口（通常是 3080）；若子进程因端口被占用而失败，
/// 内部自动以随机端口重试一次——端口回退收在这里，调用方不必再写
/// `.or_else(|_| start_server(..., 0))`，也就不会出现"靠匹配错误文本判断
/// 端口占用"这种脆弱写法。
pub(crate) fn start_server(app: &AppHandle, repo: &Path, preferred_port: u16) -> Result<String, String> {
    match start_server_once(app, repo, preferred_port) {
        Ok(url) => Ok(url),
        Err(e) if preferred_port != 0 && looks_like_port_in_use(&e) => {
            log(&format!("端口 {preferred_port} 被占用，回退随机端口重试"));
            start_server_once(app, repo, 0)
        }
        Err(e) => Err(e),
    }
}

/// 失败输出看起来像"端口被占用"吗。
/// node 的报错文案是 `EADDRINUSE: address already in use :::3080`，
/// 这里同时认关键错误码与英文描述，二者任一命中即可。
fn looks_like_port_in_use(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    s.contains("eaddrinuse") || s.contains("address already in use")
}

/// 收尾一个"启动了但没能就绪"的子进程：杀掉、取退出码、清 pid 文件。
fn abort_started_server(app: &AppHandle) -> Option<i32> {
    let code = if let Some(state) = app.try_state::<ServerProc>() {
        if let Some(mut child) = state.0.lock().unwrap().take() {
            let _ = child.kill();
            child.wait().ok().and_then(|s| s.code())
        } else {
            None
        }
    } else {
        None
    };
    let _ = fs::remove_file(pid_file());
    code
}

/// 把环形缓冲里的日志拼成一段文本（给错误信息用）。
fn tail_text(log_tail: &VecDeque<String>) -> String {
    log_tail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
}

/// dsh 就绪信号的**提示词**：只用来"优先看一眼"，不是必要条件。
///
/// 以前这里写的是完整格式 `"dsh web: http://127.0.0.1:"`，把壳的启动绑死在
/// 上游的输出文案上 —— 文案一改、格式一变，壳就再也认不出就绪行。
/// 现在只把关键词当作"往哪儿看"的线索，真正的判据是下面的 URL 结构。
const READY_HINT: &str = "dsh web";

/// 从一行输出里识别"服务器已就绪"并取出 URL；不是就绪行返回 None。
///
/// 判据是 **URL 结构**而不是输出文案：
/// - 端口 = 紧跟 `:` 的连续数字，所以 `3080/?token=…` 这种"端口后跟路径与查询"的写法照样成立；
/// - 路径与 query 原样保留（丢掉 `?token=` 会让 dsh 的会话鉴权失效）；
/// - 只认回环地址，避免把日志里别的 URL 误当成服务地址。
fn parse_ready_url(line: &str) -> Option<String> {
    // 先就近提示词找（最准确）；找不到再全行扫描 —— 上游改文案时靠这条兜底活下去
    if let Some(pos) = line.find(READY_HINT) {
        if let Some(url) = extract_loopback_url(&line[pos..]) {
            return Some(url);
        }
    }
    extract_loopback_url(line)
}

/// 在片段里找第一个 `http://<loopback>:<port>[/path][?query]` 并返回。
/// 端口取自 `:` 之后的连续数字，其余部分（路径、查询）原样拼回。
fn extract_loopback_url(haystack: &str) -> Option<String> {
    for host in ["127.0.0.1", "localhost"] {
        let needle = format!("http://{host}:");
        let Some(pos) = haystack.find(&needle) else { continue };
        let after = &haystack[pos + needle.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(port) = digits.parse::<u16>() else {
            // 这一段不是合法端口（例如 http://127.0.0.1:abc），继续找下一个
            continue;
        };
        // URL 一路延到遇到空白或收尾标点为止
        let tail = &after[digits.len()..];
        let cut = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ',' | ')' | ']' | '"' | '\'' | '`'))
            .unwrap_or(tail.len());
        nav::allow_port(port);
        return Some(format!("http://{host}:{digits}{}", &tail[..cut]));
    }
    None
}

/// 单次启动尝试：spawn 子进程，逐行读输出直到出现就绪行。
fn start_server_once(app: &AppHandle, repo: &Path, port: u16) -> Result<String, String> {
    let (program, args) = server_command(repo, port).ok_or_else(|| {
        format!(
            "在 {} 找不到 dsh CLI 入口（已尝试：{}）。请先在仓库执行 pnpm install 并构建",
            repo.display(),
            CLI_ENTRIES.iter().map(|e| e.rel).collect::<Vec<_>>().join("、")
        )
    })?;

    log(&format!("启动服务器: {program} {}", args.iter().map(|a| {
        if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }
    }).collect::<Vec<_>>().join(" ")));

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // node 是控制台程序：GUI 进程直接 spawn 会弹出新的控制台/终端窗口，
    // 必须加 CREATE_NO_WINDOW（输出经管道读取，不依赖控制台）
    let mut child = creation_flags_windows(&mut cmd)
        .spawn()
        .map_err(|e| format!("无法启动 {program}: {e}\n请确认 Node.js 已安装且在 PATH 中"))?;

    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // 立即登记句柄与 pid 文件：启动等待期间（最长 60s）若用户退出，
    // kill_server 能找到并终止子进程，避免孤儿进程泄漏
    if let Some(state) = app.try_state::<ServerProc>() {
        *state.0.lock().unwrap() = Some(child);
    }
    let _ = fs::write(pid_file(), pid.to_string());

    // 起线程逐行读输出找就绪行（stdout 与 stderr 都可能打出）
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut streams: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
    if let Some(s) = stdout { streams.push(Box::new(s)); }
    if let Some(s) = stderr { streams.push(Box::new(s)); }
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    let mut log_tail: VecDeque<String> = VecDeque::with_capacity(401);
    // 总超时用绝对截止时间算：若按"每次 recv 各自等 60 秒"，一个活着但永不就绪、
    // 又持续打日志的进程会让超时永远不到期，用户就卡在加载页且没有任何提示。
    let timeout_secs = start_timeout_secs();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let url = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            abort_started_server(app);
            return Err(format!(
                "服务器 {timeout_secs} 秒内未就绪。最近日志：\n{}",
                tail_text(&log_tail)
            ));
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                log(&line);
                log_tail.push_back(line.clone());
                if log_tail.len() > 400 {
                    log_tail.pop_front();
                }
                if let Some(url) = parse_ready_url(&line) {
                    break url;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                abort_started_server(app);
                return Err(format!(
                    "服务器 {timeout_secs} 秒内未就绪。最近日志：\n{}",
                    tail_text(&log_tail)
                ));
            }
            // 所有写入端都关了 = 子进程的输出流已全部 EOF，进程已经退出
            Err(RecvTimeoutError::Disconnected) => {
                let code = abort_started_server(app);
                return Err(format!(
                    "服务器进程提前退出（退出码 {}）。最近日志：\n{}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "未知".into()),
                    tail_text(&log_tail)
                ));
            }
        }
    };

    log(&format!("服务器就绪: {url} (pid={pid})"));
    Ok(url)
}

/// 结束服务器进程树（Windows 用 taskkill /T 带走子进程）。
pub(crate) fn kill_server(app: &AppHandle) {
    if let Some(state) = app.try_state::<ServerProc>() {
        let mut guard = state.0.lock().unwrap();
        if let Some(mut child) = guard.take() {
            let pid = child.id();
            log(&format!("结束服务器进程 pid={pid}"));
            #[cfg(windows)]
            {
                let mut cmd = Command::new("taskkill");
                cmd.args(["/pid", &pid.to_string(), "/T", "/F"]);
                // 等待 taskkill 完成，避免产生孤儿 taskkill 进程
                let _ = creation_flags_windows(&mut cmd)
                    .spawn()
                    .and_then(|mut c| c.wait());
            }
            #[cfg(not(windows))]
            {
                // 仅 kill 直接子进程；node 派生的更深层子进程可能残留
                //（本项目定位 Windows，跨平台仅保证不panic）
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
    let _ = fs::remove_file(pid_file());
}

/// 启动时清理上次异常退出（崩溃/被强杀）遗留的服务器进程。
///
/// 防误杀：PID 会被系统复用，所以不能只看"这个 PID 还在"就动手。
/// 这里要求 tasklist 返回的映像名必须是 node.exe **且 PID 列精确相等**
/// （按子串匹配会把 pid=123 命中到 1234 上）。
pub(crate) fn cleanup_stale_server() {
    let path = pid_file();
    let Ok(text) = fs::read_to_string(&path) else { return };
    let Ok(pid) = text.trim().parse::<u32>() else {
        let _ = fs::remove_file(&path);
        return;
    };
    #[cfg(windows)]
    {
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        if let Ok(out) = creation_flags_windows(&mut cmd).output() {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            if pid_is_node(&stdout, pid) {
                log(&format!("清理上次异常退出遗留的 dsh 服务器进程 pid={pid}"));
                let mut k = Command::new("taskkill");
                k.args(["/pid", &pid.to_string(), "/T", "/F"]);
                let _ = creation_flags_windows(&mut k)
                    .spawn()
                    .and_then(|mut c| c.wait());
            } else {
                log(&format!("pid 文件里的 {pid} 不是 node.exe（多半是 PID 复用），跳过清理"));
            }
        }
    }
    let _ = fs::remove_file(&path);
}

/// tasklist 的 CSV 输出里是否存在「映像名 = node.exe 且 PID = pid」的那一行。
/// 形如：`"node.exe","1234","Console","1","50,000 K"`（最后一段含逗号但在引号内）。
/// 语言无关：只比对前两列，不依赖任何本地化文案。
fn pid_is_node(csv: &str, pid: u32) -> bool {
    let want = pid.to_string();
    csv.lines().any(|line| {
        let cols: Vec<&str> = line.split("\",\"").map(|c| c.trim_matches('"')).collect();
        cols.len() >= 2 && cols[0].eq_ignore_ascii_case("node.exe") && cols[1] == want
    })
}

/// 存活监控：每 2 秒 try_wait 一次。服务器意外退出（不是 kill_server 正常结束）
/// 时 emit server-dead，壳页面据此显示断线页。kill_server 会 take 走子进程
/// （状态为 None），因此正常关闭/更新杀服务器不会误报。
/// 服务器被重启（restore_server / restart_server）后句柄重新存入，监控自动恢复。
pub(crate) fn spawn_health_watcher(handle: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let dead = {
            let Some(state) = handle.try_state::<ServerProc>() else {
                continue;
            };
            let mut guard = state.0.lock().unwrap();
            match guard.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                None => false,
            }
        };
        if dead {
            // 取出句柄，避免每 2 秒重复报警
            if let Some(state) = handle.try_state::<ServerProc>() {
                let _ = state.0.lock().unwrap().take();
            }
            // 进程已经没了，pid 文件留着只会让下次启动误判：PID 若被系统复用给
            // 另一个 node 进程，cleanup_stale_server 会把它当成遗留进程杀掉。
            let _ = fs::remove_file(pid_file());
            log("服务器进程意外退出");
            let _ = handle.emit("server-dead", ());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_url_variants() {
        // 老版：只有端口
        assert_eq!(
            parse_ready_url("dsh web: http://127.0.0.1:3080"),
            Some("http://127.0.0.1:3080".to_string())
        );
        // 0.1.5-rc.1 的实测输出：端口后紧跟路径与令牌（1.2.7 正是死在这一行上）
        let real = "dsh web: http://127.0.0.1:3080/?token=jk1o92ee6fG6KREUCH_bUudl7Cw748QCWiXm8tJF3No";
        assert_eq!(
            parse_ready_url(real),
            Some("http://127.0.0.1:3080/?token=jk1o92ee6fG6KREUCH_bUudl7Cw748QCWiXm8tJF3No".to_string())
        );
        // 随机端口 / 尾随标点
        assert_eq!(
            parse_ready_url("dsh web: http://127.0.0.1:51234, ready"),
            Some("http://127.0.0.1:51234".to_string())
        );
        assert_eq!(
            parse_ready_url("dsh web: http://127.0.0.1:51234)"),
            Some("http://127.0.0.1:51234".to_string())
        );
        // 上游换了输出文案 —— 没有 "dsh web" 前缀也要能认出来（结构匹配的意义所在）
        assert_eq!(
            parse_ready_url("  \u{279c}  ready at http://127.0.0.1:3080/"),
            Some("http://127.0.0.1:3080/".to_string())
        );
        // host 写成 localhost 同样支持
        assert_eq!(
            parse_ready_url("dsh web: http://localhost:3080/?token=x"),
            Some("http://localhost:3080/?token=x".to_string())
        );
        // 被引号包起来
        assert_eq!(
            parse_ready_url("open \"http://127.0.0.1:3080/?token=x\" to continue"),
            Some("http://127.0.0.1:3080/?token=x".to_string())
        );
        // 端口非法 / 行里没有自己的服务地址：必须返回 None，不能误判
        assert_eq!(parse_ready_url("dsh web: http://127.0.0.1:abc"), None);
        assert_eq!(parse_ready_url("dsh web: http://127.0.0.1:"), None);
        assert_eq!(parse_ready_url("server listening on port 3080"), None);
        assert_eq!(parse_ready_url("see https://example.com:443/path"), None);
    }

    #[test]
    fn detects_port_in_use_output() {
        assert!(looks_like_port_in_use("Error: listen EADDRINUSE: address already in use :::3080"));
        assert!(looks_like_port_in_use("EADDRINUSE"));
        assert!(!looks_like_port_in_use("服务器 60 秒内未就绪"));
    }

    #[test]
    fn pid_match_is_column_exact() {
        let csv = "\"node.exe\",\"1234\",\"Console\",\"1\",\"50,000 K\"\r\n";
        assert!(pid_is_node(csv, 1234));
        // 子串匹配的经典误判：123 不该命中 1234
        assert!(!pid_is_node(csv, 123));
        // 别的进程占着这个 PID
        assert!(!pid_is_node("\"chrome.exe\",\"1234\",\"Console\",\"1\",\"1 K\"\r\n", 1234));
        // 没有匹配任务时的提示行
        assert!(!pid_is_node("INFO: No tasks are running which match the specified criteria.\r\n", 1234));
    }
}
