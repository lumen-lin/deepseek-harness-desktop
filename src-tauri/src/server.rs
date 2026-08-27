//! dsh web 服务器子进程：启动、就绪解析、结束、孤儿清理、存活监控。

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::logging::log;
use crate::repo;

/// 官方就绪信号行的前缀（与 Electron 版一致）。
const URL_LINE: &str = "dsh web: http://127.0.0.1:";
/// 就绪等待上限（秒）。
const START_TIMEOUT_SECS: u64 = 60;
/// 服务器端口：与官方 `dsh web` 默认端口一致（未传 --port 时 dsh 也用 3080）；
/// 被占用的场景回退随机端口（比如终端里已手动开着 dsh web）。
pub(crate) const DSH_PORT: u16 = 3080;

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

/// 解析 dsh 启动命令：优先构建产物 lib\bin.js，回退源码 tsx 模式。port 0 = OS 随机分配。
/// --no-open：官方 d66841ea 起 dsh web 就绪后默认用系统浏览器打开页面；
/// 桌面壳自己就有窗口，不加会每次启动多弹一个浏览器标签页。
fn server_command(repo: &Path, port: u16) -> Option<(String, Vec<String>)> {
    let port_arg = port.to_string();
    let built = repo.join("apps").join("cli").join("lib").join("bin.js");
    if built.is_file() {
        return Some(("node".into(), vec![built.to_string_lossy().into(), "web".into(), "--port".into(), port_arg, "--no-open".into()]));
    }
    let src = repo.join("apps").join("cli").join("src").join("bin.ts");
    if src.is_file() {
        return Some(("node".into(), vec![
            "--import".into(), "tsx/esm".into(),
            src.to_string_lossy().into(), "web".into(), "--port".into(), port_arg, "--no-open".into(),
        ]));
    }
    None
}

/// 启动 dsh web，读取 stdout/stderr 直到出现就绪行；返回就绪 URL。
pub(crate) fn start_server(app: &AppHandle, repo: &Path, port: u16) -> Result<String, String> {
    let (program, args) = server_command(repo, port)
        .ok_or_else(|| format!("在 {} 找不到 apps/cli 入口（lib/bin.js 或 src/bin.ts），请先在仓库执行 pnpm install 并构建", repo.display()))?;

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

    // 取出句柄引用来逐行读输出（child 已存入 ServerProc，此处重新借用）
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
    let url = loop {
        let recv = rx.recv_timeout(Duration::from_secs(START_TIMEOUT_SECS));
        match recv {
            Ok(line) => {
                log(&line);
                log_tail.push_back(line.clone());
                if log_tail.len() > 400 {
                    log_tail.pop_front();
                }
                if let Some(pos) = line.find(URL_LINE) {
                    let rest = &line[pos + URL_LINE.len()..];
                    if let Some(port) = rest.split(&[',', ' ', ')'][..]).next() {
                        break format!("http://127.0.0.1:{port}");
                    }
                }
            }
            Err(_) => {
                // 超时：从 ServerProc 取回句柄并杀进程，清除 pid 文件
                if let Some(state) = app.try_state::<ServerProc>() {
                    if let Some(mut child) = state.0.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                let _ = fs::remove_file(pid_file());
                return Err(format!("服务器 {START_TIMEOUT_SECS} 秒内未就绪。最近日志：\n{}", log_tail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")));
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
/// 校验进程名必须是 node.exe 才动手，防 PID 复用误杀无关进程。
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
            let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
            let pid_str = pid.to_string();
            let alive_node = stdout
                .lines()
                .any(|l| l.contains("node.exe") && l.contains(&pid_str));
            if alive_node {
                log(&format!("清理上次异常退出遗留的 dsh 服务器进程 pid={pid}"));
                let mut k = Command::new("taskkill");
                k.args(["/pid", &pid_str, "/T", "/F"]);
                let _ = creation_flags_windows(&mut k)
                    .spawn()
                    .and_then(|mut c| c.wait());
            }
        }
    }
    let _ = fs::remove_file(&path);
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
            log("服务器进程意外退出");
            let _ = handle.emit("server-dead", ());
        }
    });
}
