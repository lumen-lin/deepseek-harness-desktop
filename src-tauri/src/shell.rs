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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::logging::log;

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
/// @returns 主窗口应加载的壳页面 URL（`http://127.0.0.1:<port>/<secret>/index.html`）。
pub(crate) fn start_shell_server() -> Result<String, String> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("无法启动壳页面服务站: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("无法查询壳页面服务站端口: {e}"))?
        .port();

    let secret = random_secret();
    let secret_path = format!("/{secret}/index.html");
    let url = format!("http://127.0.0.1:{port}{secret_path}");

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
