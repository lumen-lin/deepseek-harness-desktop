//! 共享路径工具：应用基目录、用户 home、dsh 配置目录。

use std::path::PathBuf;

/// 应用基目录：打包后为 exe 所在目录，开发模式从 target/debug|release 向上回到项目根。
///
/// 开发模式用 `cfg!(debug_assertions)` 判定，而不是看目录名是否叫
/// debug/release——后者在用户恰好把 exe 装在 `D:\apps\release\` 这类目录时
/// 会误判，把日志与配置写到上三级目录去。
pub(crate) fn app_base_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let mut d = exe.parent().map(PathBuf::from).unwrap_or_default();
        if cfg!(debug_assertions) {
            // target/debug/<exe> 或 target/release/<exe> → 上三级回到项目根
            for _ in 0..3 {
                if !d.pop() {
                    break;
                }
            }
        }
        d
    } else {
        PathBuf::from(".")
    }
}

/// 用户 home 目录：Windows 用 USERPROFILE，类 Unix 用 HOME。
pub(crate) fn dirs_home() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// dsh 配置目录：DSH_HOME 环境变量覆盖，默认 ~/.dsh。
pub(crate) fn dsh_home() -> PathBuf {
    if let Ok(env) = std::env::var("DSH_HOME") {
        let trimmed = env.trim().to_string();
        if !trimmed.is_empty() {
            let p = if trimmed == "~" {
                dirs_home()
            } else if let Some(rest) = trimmed
                .strip_prefix("~/")
                .or_else(|| trimmed.strip_prefix("~\\"))
            {
                dirs_home().join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            return p;
        }
    }
    dirs_home().join(".dsh")
}
