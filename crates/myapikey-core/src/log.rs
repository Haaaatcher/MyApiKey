//! 文件日志（GUI 运行追踪，2026-11-05 用户需求）：错误详情落文件而非
//! 悬停/弹窗——排查有据可查，界面只保留简短现象。
//!
//! - 级别：`off`（关闭）/ `error` / `info` / `debug`，由设置页控制
//!   （config `log_level`，默认 `info`），变更即时生效（原子量）。
//! - 文件：`<config_dir>/logs/mak-YYYY-MM-DD.log`（按天分文件），追加写；
//!   启动时清理 7 天前的旧文件（[`LOG_KEEP_DAYS`]，防无限膨胀）。
//! - 路径：[`init`] 由 GUI 启动时注入；未 init（CLI 前台输出即日志）时
//!   所有写入为空操作。
//! - **铁律**：任何日志不得包含 api_key / 解锁密码 / 令牌明文——只记
//!   状态、状态码与错误链（`Error` 的 Display 本身已按 AGENTS 规则
//!   不含凭据）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// 级别序：off(0) < error(1) < info(2) < debug(3)。
static LEVEL: AtomicU8 = AtomicU8::new(2); // 默认 info

/// 日志目录（GUI 启动时注入；None = 未初始化,写入空操作）。
static LOG_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 日志文件保留天数。
const LOG_KEEP_DAYS: i64 = 7;

/// 级别字符串 → 数值（未知值回退 info,与 config 默认一致）。
fn parse_level(s: &str) -> u8 {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => 0,
        "error" => 1,
        "debug" => 3,
        _ => 2, // "info" 与一切未知值
    }
}

/// 设置页/启动时按 config 字符串设置级别（即时生效）。
pub fn set_level(level: &str) {
    LEVEL.store(parse_level(level), Ordering::Relaxed);
}

/// 注入日志目录并清理过期文件（GUI 启动时调用一次）。
pub fn init(config_dir: &std::path::Path) {
    let logs = config_dir.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    // 清理 7 天前的旧文件:按文件名日期判断失败（手改文件名）则按
    // 修改时间兜底;清理失败静默（日志清理不值得报错打断启动）
    if let Ok(entries) = std::fs::read_dir(&logs) {
        let cutoff = std::time::SystemTime::now()
            - std::time::Duration::from_secs(60 * 60 * 24 * LOG_KEEP_DAYS as u64);
        for e in entries.flatten() {
            let stale = std::fs::metadata(e.path())
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    *LOG_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(logs);
}

/// 写一条日志（内部通用）。`level_num` 低于当前级别 → 空操作;
/// 未 init / 打开失败 → 空操作（日志永不干扰主流程）。
fn write(level_num: u8, tag: &str, module: &str, msg: &str) {
    if LEVEL.load(Ordering::Relaxed) < level_num {
        return;
    }
    let dir = LOG_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(dir) = dir else { return };
    let day = chrono::Local::now().format("%Y-%m-%d");
    let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    // 追加写 + 串行锁:GUI 写入是事件级低频,重开文件无性能压力,
    // 换取按天轮转的零状态实现
    let _guard = LOG_APPEND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let line = format!("[{stamp}] [{tag}] [{module}] {msg}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("mak-{day}.log")))
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// 追加串行锁：多线程（启动续期线程 / 自动锁线程 / spawn_blocking）并发写。
static LOG_APPEND_LOCK: Mutex<()> = Mutex::new(());

/// 错误级：失败与被打断的操作（级别设为 error/off 时才可见/不可见）。
pub fn error(module: &str, msg: &str) {
    write(1, "ERROR", module, msg);
}

/// 信息级：关键状态转换（授权完成、同步成功等）。
pub fn info(module: &str, msg: &str) {
    write(2, "INFO", module, msg);
}

/// 调试级：诊断细节（状态码、探测分支），默认级别不可见。
pub fn debug(module: &str, msg: &str) {
    write(3, "DEBUG", module, msg);
}
