//! dsh 侧偏好读取：把 dsh 各版本的配置版式统一成一个取值接口。
//!
//! dsh 0.1.7 换了配置落点（上游 `packages/settings/settings/src/index.ts` 的
//! `importLegacyDocument`）：启动时把 `$DSH_HOME/settings.yaml` 改名成
//! `settings.yaml.imported`，再把各 section 搬进「活动 profile 的补丁文档」
//! `$DSH_HOME/profiles/<profile>/cordis.patch.yml`。两种版式的形状并不一样：
//!
//! ```text
//! ≤0.1.6  settings.yaml                       映射：节名 → 该节的配置
//!   ui-theme:
//!     preference: dark
//!
//! ≥0.1.7  profiles/web/cordis.patch.yml       序列：补丁条目数组
//!   - id: ui-theme
//!     name: "@deepseek-ai/dsh-client-ui-theme"
//!     config:
//!       preference: dark
//! ```
//!
//! 壳原本只认旧路径，dsh 一升级就再也读不到值，于是主题回退「跟随系统」——
//! 系统是浅色时壳永远浅色，而 dsh 本体照旧深色（2026-09-24 的实机故障：
//! 标题栏浅色 `#edeef4`、内容区深色 `#0c0c0d`）。这里两种版式都认，并把
//! 迁移残留文件放在回退链末尾。

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::logging::log;
use crate::paths;

/// 壳自己启动的 profile。`server.rs` 执行的是 `dsh web`，CLI 把 `web` 子命令
/// 映射到同名 profile（`apps/cli/src/args.ts` 的 `parse`）。
pub(crate) const SHELL_PROFILE: &str = "web";

/// profile 补丁文档的固定文件名（对齐 dsh 的 `PROFILE_PATCH_FILENAME`）。
pub(crate) const PROFILE_PATCH_FILENAME: &str = "cordis.patch.yml";

/// 一次读取要用到的候选文档与监听目录。
pub(crate) struct Sources {
    /// 候选配置文档，按「新格式 → 旧格式 → 迁移残留」的优先级排列。
    pub(crate) files: Vec<PathBuf>,
    /// 各候选文档所在目录（去重）。监听目录而不是文件本身：dsh 写配置走的是
    /// 原子改名（`writeFileAtomic`），盯着文件会在这类顶替后失效。
    pub(crate) dirs: Vec<PathBuf>,
}

/// 列出候选配置文档与对应监听目录。
pub(crate) fn sources() -> Sources {
    let mut files = patch_documents();
    files.push(paths::dsh_settings_path());
    files.push(paths::dsh_imported_settings_path());

    let mut dirs: Vec<PathBuf> = Vec::new();
    for file in &files {
        if let Some(dir) = file.parent() {
            let dir = dir.to_path_buf();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    Sources { files, dirs }
}

/// 各 profile 的补丁文档，壳自己的 profile 排在最前。
///
/// 用扫描而不是硬编码单个 profile：将来 profile 改名或新增时，不至于又一次
/// 「偏好静默读空」——那正是本次故障的成因。目录不存在（dsh 还没建出该
/// profile）时只跳过，绝不主动创建：凭空多出一个空 profile 目录会让 dsh 的
/// profile 解析产生歧义。
fn patch_documents() -> Vec<PathBuf> {
    let root = paths::dsh_profiles_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_profile_dir_name(name))
        .collect();
    // 壳自己的 profile 提到最前，其余按名字排序让顺序稳定。
    names.sort_by_key(|name| (name != SHELL_PROFILE, name.clone()));
    names
        .into_iter()
        .map(|name| root.join(name).join(PROFILE_PATCH_FILENAME))
        .collect()
}

/// 目录名是否是 profile：`profiles/node_modules` 是各 profile 共享的依赖树、
/// 不是 profile 本身，点开头的隐藏目录同理。把它们算进来只会多出几个永远不
/// 存在的候选文件，还会让监听器去盯 pnpm 频繁改动的依赖目录。
fn is_profile_dir_name(name: &str) -> bool {
    name != "node_modules" && !name.starts_with('.')
}

/// 读一个配置项 `entry.key`，例如 `("ui-theme", "preference")`。
///
/// 返回 `None` 表示候选文档里都没有这个值（文件缺失、版式不认识、该项被删掉
/// 都算），由调用方决定回退值。某一份文档没有该 entry 时会继续往后找，所以
/// 「新格式里删掉 ui-theme」也能落回旧文件上。
pub(crate) fn entry_string(entry: &str, key: &str) -> Option<String> {
    first_match(&sources().files, entry, key)
}

/// 按优先级在候选文档里找第一个命中的 `entry.key`。
fn first_match(files: &[PathBuf], entry: &str, key: &str) -> Option<String> {
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
            continue;
        };
        if let Some(found) = lookup(&value, entry, key) {
            return Some(found);
        }
    }
    None
}

/// 在单份文档里找 `entry.key`，新旧两种版式都试。
fn lookup(value: &serde_yaml::Value, entry: &str, key: &str) -> Option<String> {
    // 旧格式：顶层是映射，节名直接就是 key。
    if let Some(found) = value
        .get(entry)
        .and_then(|section| section.get(key))
        .and_then(|field| field.as_str())
    {
        return Some(found.to_owned());
    }
    // 新格式：顶层是序列，逐条补丁按 `id` 匹配，值在 `config` 下。
    for patch in value.as_sequence()?.iter() {
        if patch.get("id").and_then(|id| id.as_str()) != Some(entry) {
            continue;
        }
        if let Some(found) = patch
            .get("config")
            .and_then(|config| config.get(key))
            .and_then(|field| field.as_str())
        {
            return Some(found.to_owned());
        }
    }
    None
}

/// 起一个「配置变了就回调」的跟随线程：监听候选文档所在目录，另有 30s 兜底
/// 重查（防文件系统事件偶发丢失、也顺带发现新建的 profile 目录）；监听器建不
/// 起来（罕见）时回退 500ms 纯轮询。回调只在取值真的变化时触发一次。
///
/// 主题与语言两条跟随线共用它：两边的监听逻辑逐字一样，只有「取值怎么解析」
/// 和「变了之后干什么」不同。
///
/// `label` 只用于日志（如「主题」）；`read` 是取值函数；`on_change` 在新值上执行。
pub(crate) fn spawn_watch(
    label: &str,
    read: fn() -> String,
    on_change: impl Fn(String) + Send + 'static,
) {
    let label = label.to_owned();
    std::thread::spawn(move || {
        use notify::Watcher;
        let sources = sources();
        let mut last = read();
        let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
        let watcher = notify::recommended_watcher(tx).and_then(|mut w| {
            // 只有 dsh 家目录可以创建（它本来就该存在）；profile 目录不存在就跳过，
            // 由兜底重查接管。
            let home = paths::dsh_home();
            let _ = std::fs::create_dir_all(&home);
            let mut watched = 0usize;
            for dir in &sources.dirs {
                if dir.exists() && w.watch(dir, notify::RecursiveMode::NonRecursive).is_ok() {
                    watched += 1;
                }
            }
            if watched == 0 {
                let err: notify::Error =
                    std::io::Error::new(std::io::ErrorKind::NotFound, "没有可监听的配置目录").into();
                return Err(err);
            }
            Ok(w)
        });
        match watcher {
            Ok(w) => {
                let _watcher = w; // 保持监听器存活
                log(&format!(
                    "{label}监听已建立（{} 个目录 + 30s 兜底）",
                    sources.dirs.len()
                ));
                loop {
                    let hit = match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(Ok(ev)) => ev.paths.iter().any(|p| sources.files.contains(p)),
                        Ok(Err(_)) | Err(_) => true, // 事件错误或超时：兜底重查
                    };
                    if !hit {
                        continue;
                    }
                    // 稍等写入完全落地再读（事件先于文件内容可见的边角情况）
                    std::thread::sleep(Duration::from_millis(50));
                    let now = read();
                    if now != last {
                        log(&format!("{label}偏好变化（文件）: {now}"));
                        last = now.clone();
                        on_change(now);
                    }
                }
            }
            Err(e) => {
                log(&format!("{label}监听不可用（{e}），回退 500ms 轮询"));
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let now = read();
                    if now != last {
                        log(&format!("{label}偏好变化（文件）: {now}"));
                        last = now.clone();
                        on_change(now);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsh ≥0.1.7 的版式：补丁条目数组。
    const PATCH_DOC: &str = r#"
- id: ui-settings-general
  name: "@deepseek-ai/dsh-client-ui-settings-general"
  config:
    welcomeNoticeVersion: 2026-08-13.1
- id: ui-theme
  name: "@deepseek-ai/dsh-client-ui-theme"
  config:
    preference: dark
    fontSize: 15
- id: locale
  name: "@deepseek-ai/dsh-client-locale"
  config:
    preference: zh
"#;

    /// dsh ≤0.1.6 的版式：节名映射。
    const LEGACY_DOC: &str = "ui-theme:\n  preference: light\n  fontSize: 15\nlocale:\n  preference: en\n";

    fn parse(text: &str) -> serde_yaml::Value {
        serde_yaml::from_str(text).expect("测试用的 YAML 必须能解析")
    }

    #[test]
    fn reads_patch_document() {
        let value = parse(PATCH_DOC);
        assert_eq!(lookup(&value, "ui-theme", "preference").as_deref(), Some("dark"));
        assert_eq!(lookup(&value, "locale", "preference").as_deref(), Some("zh"));
        // 值在较深一层，不能把同级其他字段误当成目标
        assert_eq!(lookup(&value, "ui-theme", "fontSize"), None);
    }

    #[test]
    fn reads_legacy_document() {
        let value = parse(LEGACY_DOC);
        assert_eq!(lookup(&value, "ui-theme", "preference").as_deref(), Some("light"));
        assert_eq!(lookup(&value, "locale", "preference").as_deref(), Some("en"));
    }

    #[test]
    fn missing_entry_is_none() {
        // 只有 fontSize、没有 preference：不能瞎猜一个值，交给调用方回退
        let value = parse("- id: ui-theme\n  config:\n    fontSize: 15\n");
        assert_eq!(lookup(&value, "ui-theme", "preference"), None);
        // 补丁条目可能只有 id/disabled，没有 config
        let disabled = parse("- id: ui-theme\n  disabled: true\n");
        assert_eq!(lookup(&disabled, "ui-theme", "preference"), None);
        // 顶层既不是映射也不含该节
        assert_eq!(lookup(&parse("[]\n"), "ui-theme", "preference"), None);
    }

    #[test]
    fn node_modules_is_not_a_profile() {
        // profiles/ 下只有真 profile 才算候选；实测 `~/.dsh/profiles/node_modules`
        // 确实存在（各 profile 共享的依赖树），曾被当成 profile 扫进去
        assert!(is_profile_dir_name("web"));
        assert!(is_profile_dir_name("tui"));
        assert!(!is_profile_dir_name("node_modules"));
        assert!(!is_profile_dir_name(".git"));
    }

    #[test]
    fn falls_through_to_later_candidate() {
        let dir = std::env::temp_dir().join(format!("dsh-settings-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("cordis.patch.yml");
        let second = dir.join("settings.yaml");
        // 第一份是迁移后的空文档（entry 已被删掉），值只留在第二份里
        fs::write(&first, "[]\n").unwrap();
        fs::write(&second, LEGACY_DOC).unwrap();

        let files = vec![first.clone(), second.clone()];
        assert_eq!(first_match(&files, "locale", "preference").as_deref(), Some("en"));
        // 候选里根本没有该 entry 时返回 None，而不是错误的值
        assert_eq!(first_match(&files, "ui-conversation", "preference"), None);
        // 第一份文件不存在也不能中断回退
        assert_eq!(
            first_match(&[dir.join("nope.yml"), second], "ui-theme", "preference").as_deref(),
            Some("light")
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
