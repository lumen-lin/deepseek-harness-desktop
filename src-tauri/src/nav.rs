//! 导航白名单：主窗口只允许停在本进程自己起的本地服务上。
//!
//! ## 为什么必须收窄
//!
//! 壳页面由本地服务站提供（`http://127.0.0.1:<壳端口>`），dsh 页面跑在它的
//! iframe 里（`http://127.0.0.1:<dsh端口>`）。除此之外**不允许**导航到任何地址：
//! 本机上任何进程都能在 127.0.0.1 起一个 HTTP 服务，若白名单只判断
//! "host 是不是 127.0.0.1"，那个页面就能顶替壳页面占据主窗口——而在 Tauri 的
//! ACL 眼里它同样是 127.0.0.1 来源，等于白拿壳的全部 IPC 权限。
//! 所以这里要求 URL 的**端口**必须是本进程登记过的服务端口。
//!
//! 端口在服务真正启动成功后才登记（`allow_port`），不依赖任何编译期常量，
//! 因此与「端口被占用就回退随机端口」的逻辑天然兼容。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn allowed() -> &'static Mutex<HashSet<u16>> {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    PORTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 登记一个「可信端口」：主窗口可以被导航到该端口上的地址。
/// 壳服务站与 dsh 服务在各自启动成功后调用。
pub(crate) fn allow_port(port: u16) {
    if let Ok(mut set) = allowed().lock() {
        set.insert(port);
    }
}

/// 主窗口是否允许导航到该 URL。
///
/// - `tauri://` / `about:`：内部空页，放行；
/// - http(s)：host 必须是回环地址（127.0.0.1 / localhost / ::1），
///   且**端口**必须是已登记的可信端口。不带端口的回环地址（如 `http://localhost/`）
///   指向的是 80 端口，不是我们的服务，一律拒绝。
pub(crate) fn is_allowed(url: &str) -> bool {
    if url.starts_with("tauri://") || url.starts_with("about:") {
        return true;
    }
    let after_scheme = match url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        Some(rest) => rest,
        None => return false,
    };
    // authority = host[:port]，取第一个 '/' 之前的部分
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let Some((host, port)) = split_host_port(authority) else {
        return false;
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return false;
    }
    let Some(port) = port else { return false };
    allowed().lock().map(|set| set.contains(&port)).unwrap_or(false)
}

/// 从 authority 拆出 `(host, port)`。
/// 支持 `127.0.0.1:3080` / `localhost:3080` / `[::1]:3080`；无端口时 port 为 None。
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if authority.starts_with('[') {
        // IPv6 字面量：[::1] 或 [::1]:3080
        let end = authority.find(']')?;
        let host = &authority[1..end];
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok());
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h, p.parse::<u16>().ok())),
        None => Some((authority, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_registered_local_ports() {
        allow_port(3080);
        allow_port(51234);
        // 回环 + 已登记端口
        assert!(is_allowed("http://127.0.0.1:3080/"));
        assert!(is_allowed("http://127.0.0.1:3080/index.html?k=abc"));
        assert!(is_allowed("http://localhost:51234/x"));
        // 未登记端口：本机别的程序起的页面，必须拒绝
        assert!(!is_allowed("http://127.0.0.1:9999/"));
        // 无端口 = 80，不是我们的服务
        assert!(!is_allowed("http://localhost/"));
        // 外部地址
        assert!(!is_allowed("https://evil.com/127.0.0.1:3080"));
        assert!(!is_allowed("http://127.0.0.1.evil.com:3080/"));
        // 带 userinfo 的混淆写法
        assert!(!is_allowed("http://user@127.0.0.1:3080/"));
        // 内部协议
        assert!(is_allowed("about:blank"));
    }
}
