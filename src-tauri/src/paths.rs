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

/// dsh 的历史共享设置文件（dsh ≤0.1.6 的唯一配置落点，主题与语言偏好都在里面）。
///
/// 0.1.7 起 dsh 不再用它：启动时改名成 [`dsh_imported_settings_path`]，各 section
/// 搬进活动 profile 的补丁文档。这里仍然保留，是因为回退链要认旧版本安装。
pub(crate) fn dsh_settings_path() -> PathBuf {
    dsh_home().join("settings.yaml")
}

/// 旧设置文件被 dsh 迁移后的残留名（`settings.yaml.imported`）。
///
/// dsh 的迁移是「先把文件改名，再逐 section 写入 profile」——改名到写完之间
/// 有窗口期，写入失败被拒的 section 也只会留在改名后的文件里，所以它是回退链
/// 的最后一环。
pub(crate) fn dsh_imported_settings_path() -> PathBuf {
    dsh_home().join("settings.yaml.imported")
}

/// dsh 的 profile 目录树：`$DSH_HOME/profiles`，每个 profile 一个子目录。
pub(crate) fn dsh_profiles_dir() -> PathBuf {
    dsh_home().join("profiles")
}
