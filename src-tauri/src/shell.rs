//! 壳页面本地服务站。
//!
//! ## 为什么需要它
//!
//! dsh ≥ 0.1.2 给 Web 界面加了浏览器会话鉴权：首次访问 `?token=<启动令牌>`
//! 时下发一枚会话 cookie，之后首页和 `/api` 请求都靠这枚 cookie 放行。
//! 这枚 cookie 带 `HttpOnly; SameSite=Strict`，而 `SameSite=Strict` 只在
//! 「请求站点 == 顶层站点」时才随请求发送。
//!
//! 壳页面原先由 Tauri 自定义协议（`tauri://` / `http://tauri.localhost`）提供，
//! dsh 页面装在它的 iframe 里 —— 顶层站点是 tauri 协议、iframe 是 127.0.0.1，
//! 属于跨站上下文，浏览器既不存储也不回传这枚 cookie，于是首页和所有
//! `/api` 调用一律 401，页面只剩一句
//! `dsh web authentication required; reopen the URL printed by dsh web.`
//!
//! 对策：把壳页面也放到 127.0.0.1 上。SameSite 的站点判定只看 host、不看端口，
//! 所以壳页面（`http://127.0.0.1:<壳端口>`）与 dsh（`http://127.0.0.1:3080`）
//! 互为同站，iframe 里的 cookie 就正常生效了。官方用系统浏览器打开时能用，
//! 正是因为那时是顶层导航。
//!
//! ## 调用令牌（token）
//!
//! 壳页面跑在 127.0.0.1 上，而 Tauri 的 ACL 只能按「来源 host:port」授权，
//! 无法区分同一 host 下的不同页面——只要来源是 127.0.0.1，任何页面都符合
//! capability 里 `http://127.0.0.1:*/*` 的条件。因此权限边界不能只靠 ACL：
//! 壳页面 URL 里带一枚随机 token（`?k=<secret>`），所有自定义命令都要求
//! 调用方带上它，Rust 侧校验后才执行。token 只随「秘密路径」下的壳页面下发，
//! 本机其它进程即便猜到端口也拿不到壳页面 HTML，自然拿不到 token。
//! 配合 `nav.rs` 的端口白名单，构成两道独立的边界。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;

use crate::logging::log;
use crate::nav;

/// 本进程的调用令牌：壳页面从 URL 的 `?k=` 取，自定义命令调用时回传。
static TOKEN: OnceLock<String> = OnceLock::new();

/// 校验壳命令调用凭证。token 未初始化（服务站还没起来）时一律拒绝。
pub(crate) fn token_matches(candidate: &str) -> bool {
    match TOKEN.get() {
        Some(expected) => {
            // 长度先比一遍再比内容，避免明显不匹配时的逐字节比较（本地场景足够）
            expected.len() == candidate.len() && expected == candidate
        }
        None => false,
    }
}

/// 壳页面（单文件，CSS/JS 全内联），编译期内嵌，运行期不读磁盘。
const SHELL_HTML: &str = include_str!("../../ui/index.html");

/// 与 tauri.conf.json 的 `app.security.csp` 一致：页面改由本服务站提供后，
/// Tauri 不再自动附加 CSP 响应头，这里手动补上。
const CSP: &str = concat!(
    "default-src 'self'; ",
    "script-src 'self' 'unsafe-inline'; ",
    "style-src 'self' 'unsafe-inline'; ",
    "img-src 'self' data: https:; ",
    "frame-src http://127.0.0.1:* http://localhost:*; ",
    "connect-src 'self' ipc: http://ipc.localhost",
);

/// 一次性随机路径段：服务站暴露在 127.0.0.1 上，加一段无法猜测的路径，
/// 本机其它进程即便扫到端口也拿不到带 IPC 权限的壳页面。
/// 路径不参与同源/SameSite 判定，加它不影响鉴权修复。
fn random_secret() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let a = RandomState::new().hash_one(nanos);
    let b = RandomState::new().hash_one(std::process::id());
    format!("{a:x}{b:x}")
}

/// 处理一次请求：只认 GET 且路径必须是约定的秘密路径，其余一律 404/405。
fn serve(mut stream: TcpStream, secret_path: String) {
    let mut buf = [0u8; 8192];
    let read = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let header = String::from_utf8_lossy(&buf[..read]);
    let mut parts = header.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    // 去掉查询串与锚点（只用到路径部分）
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or(target)
        .to_string();

    let (status, body): (&str, &[u8]) = if method != "GET" {
        ("405 Method Not Allowed", b"")
    } else if path == secret_path {
        ("200 OK", SHELL_HTML.as_bytes())
    } else {
        ("404 Not Found", b"")
    };

    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Content-Security-Policy: {CSP}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// 启动只服务于壳页面的本地 HTTP 服务站（绑定 127.0.0.1，端口由系统分配）。
///
/// 返回主窗口应加载的 URL
/// （`http://127.0.0.1:<port>/<secret>/index.html?k=<secret>`）。
/// 端口在这里就登记进导航白名单了，不必外传。
/// 注意日志里**不打印** secret：它就是调用令牌，落到日志文件等于泄露。
pub(crate) fn start_shell_server() -> Result<String, String> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("无法启动壳页面服务站: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("无法查询壳页面服务站端口: {e}"))?
        .port();

    let secret = random_secret();
    let secret_path = format!("/{secret}/index.html");
    let url = format!("http://127.0.0.1:{port}{secret_path}?k={secret}");
    // 令牌先落地再启用窗口：命令校验一律以这里的值为准
    let _ = TOKEN.set(secret);
    // 壳页面端口登记进导航白名单（否则主窗口首次导航就被自己拦下）
    nav::allow_port(port);

    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let path = secret_path.clone();
                    std::thread::spawn(move || serve(stream, path));
                }
                Err(e) => log(&format!("壳页面服务站接受连接失败: {e}")),
            }
        }
    });

    log(&format!("壳页面服务站就绪: http://127.0.0.1:{port}/<secret>/index.html"));
    Ok(url)
}
