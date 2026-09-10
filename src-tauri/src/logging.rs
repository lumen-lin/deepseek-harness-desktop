//! 日志：写到 exe 旁 logs\desktop.log。1MB 单轮转；时间戳为可读的本地时间（UTC+8）。

use std::fs;
use std::path::PathBuf;

/// 单文件超过 1MB 时轮转为 desktop.log.1（更早的丢弃）。
const MAX_LOG_BYTES: u64 = 1_000_000;

/// 日志目录：exe 旁 logs\（打包后看不到 stdout）。
/// 开发模式向上回到项目根；打包模式用 exe 所在目录。
pub(crate) fn log_dir() -> PathBuf {
    let dir = crate::paths::app_base_dir().join("logs");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// unix 毫秒 → "YYYY-MM-DD HH:MM:SS"。固定 UTC+8（本机自用，未做时区探测）。
fn format_ts(ms: u128) -> String {
    const OFFSET_SECS: u64 = 8 * 3600;
    let secs = (ms / 1000) as u64 + OFFSET_SECS;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // civil_from_days（Howard Hinnant 公历算法）
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

pub(crate) fn log(line: &str) {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let entry = format!("[{}] {line}\n", format_ts(stamp));
    eprintln!("{entry}");
    let dir = log_dir();
    let path = dir.join("desktop.log");
    // 轮转：超过 1MB 改名 desktop.log.1（旧的 .1 丢弃）
    if let Ok(meta) = fs::metadata(&path)
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = fs::rename(&path, dir.join("desktop.log.1"));
    }
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(entry.as_bytes());
    }
}
