//! deepseek-harness 仓库定位与壳自身数据目录。

use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

/// 校验目录是否为 deepseek-harness 仓库根（apps\cli\package.json 存在）。
pub(crate) fn is_repo_root(p: &Path) -> bool {
    p.join("apps").join("cli").join("package.json").is_file()
}

/// 数据目录：配置（仓库位置）、pid 文件等。开发模式在 desktop-tauri\data，打包后在 exe 旁 data。
pub(crate) fn data_dir() -> PathBuf {
    paths::app_base_dir().join("data")
}

fn read_repo_config() -> Option<String> {
    let base = paths::app_base_dir();
    let new_path = base.join("data").join("config.json");
    let legacy_path = base.join("config.json");

    // 新位置优先（exe 旁 data\config.json）
    if let Some(s) = parse_repo_root(&new_path) {
        return Some(s);
    }
    // 兼容旧版：早期版本把 config.json 直接写在 exe 旁（无 data 子目录）。
    // 命中即把旧配置迁移到新位置，避免长期保留两套兼容分支。
    if let Some(s) = parse_repo_root(&legacy_path) {
        if !new_path.exists() {
            let _ = fs::create_dir_all(base.join("data"));
            let _ = fs::copy(&legacy_path, &new_path);
        }
        return Some(s);
    }
    None
}

/// 从单个 config.json 解析 repoRoot（文件缺失/格式错误返回 None）。
fn parse_repo_root(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.get("repoRoot")?.as_str().map(String::from)
}

pub(crate) fn write_repo_config(repo: &Path) {
    // 合并写入而非整体覆盖：config.json 里还有其它字段（如坏版本黑名单）
    let mut cfg = read_config();
    cfg["repoRoot"] = serde_json::Value::String(repo.to_string_lossy().into_owned());
    write_config(&cfg);
}

/// 读整个 config.json（缺失或格式错误时返回空对象）。
fn read_config() -> serde_json::Value {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_config(v: &serde_json::Value) {
    let _ = fs::create_dir_all(data_dir());
    let _ = fs::write(config_path(), v.to_string());
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// 记录一个"构建失败"的远端 commit。
///
/// 上游偶尔会发布自身编译不过的版本（源码导出缺失等），这类失败与本地环境
/// 无关。记下来后，检查更新时可以提前警告，避免用户反复更新到同一个坏版本。
pub(crate) fn mark_bad_remote(commit: &str) {
    let mut cfg = read_config();
    let mut list: Vec<String> = cfg
        .get("badRemoteCommits")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if !list.iter().any(|c| c == commit) {
        list.push(commit.to_string());
        // 只保留最近若干个，避免无限增长
        while list.len() > 8 {
            list.remove(0);
        }
        cfg["badRemoteCommits"] = serde_json::json!(list);
        write_config(&cfg);
    }
}

/// 该 commit 是否曾被记录为"构建失败"。
pub(crate) fn is_bad_remote(commit: &str) -> bool {
    read_config()
        .get("badRemoteCommits")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).any(|c| c == commit))
        .unwrap_or(false)
}

/// 该 commit 已验证可构建：从黑名单移除（例如上游修复后重新发布）。
pub(crate) fn clear_bad_remote(commit: &str) {
    let mut cfg = read_config();
    let Some(arr) = cfg.get("badRemoteCommits").and_then(|v| v.as_array()).cloned() else {
        return;
    };
    let list: Vec<serde_json::Value> = arr
        .into_iter()
        .filter(|v| v.as_str().map(|s| s != commit).unwrap_or(true))
        .collect();
    cfg["badRemoteCommits"] = serde_json::Value::Array(list);
    write_config(&cfg);
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
