//! deepseek-harness 仓库定位与壳自身数据目录。

use std::fs;
use std::path::{Path, PathBuf};

use crate::logging::log_dir;

/// 校验目录是否为 deepseek-harness 仓库根（apps\cli\package.json 存在）。
pub(crate) fn is_repo_root(p: &Path) -> bool {
    p.join("apps").join("cli").join("package.json").is_file()
}

/// 数据目录：配置（仓库位置）、pid 文件等。开发模式在 desktop-tauri\data，打包后在 exe 旁 data。
pub(crate) fn data_dir() -> PathBuf {
    log_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("data")
}

fn read_repo_config() -> Option<String> {
    let text = fs::read_to_string(data_dir().join("config.json")).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.get("repoRoot")?.as_str().map(String::from)
}

pub(crate) fn write_repo_config(repo: &Path) {
    let _ = fs::create_dir_all(data_dir());
    let json = serde_json::json!({ "repoRoot": repo.to_string_lossy() });
    let _ = fs::write(data_dir().join("config.json"), json.to_string());
}

/// 按优先级定位仓库：DSH_REPO 环境变量 > 上次保存 > exe 周边候选。
pub(crate) fn locate_repo() -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("DSH_REPO") {
        let p = PathBuf::from(from_env);
        if is_repo_root(&p) {
            return Some(p);
        }
        return None; // 显式指定无效：交给用户手选
    }
    if let Some(saved) = read_repo_config() {
        let p = PathBuf::from(&saved);
        if is_repo_root(&p) {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    // 开发模式：target\debug → 向上到 desktop-tauri 再到根；打包模式：exe 所在目录逐级向上
    for _ in 0..5 {
        let candidate = dir.join("deepseek-harness");
        if is_repo_root(&candidate) {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// 读仓库 dsh 版本号（apps\cli\package.json 的 version），关于对话框展示用。
pub(crate) fn repo_dsh_version(repo: &Path) -> String {
    fs::read_to_string(repo.join("apps").join("cli").join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("version")?.as_str().map(String::from))
        .unwrap_or_default()
}
