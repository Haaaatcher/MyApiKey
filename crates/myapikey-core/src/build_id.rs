//! 构建指纹：识别「运行中的二进制是否为上次记录过的同一构建」。
//!
//! 用途：GUI 解锁页的钥匙串引导提示（D57）——ad-hoc 签名下每次重新
//! 构建都是新二进制身份，首次访问钥匙串条目会弹授权窗（D56/D57 治理
//! 的残余弹窗面）。指纹不一致 = 新构建 = 需要向用户预告授权弹窗并
//! 建议点「始终允许」；记录一次后不再打扰，直到下次构建。
//!
//! 指纹 = 可执行文件的 `(字节数, 修改时刻)`：重建必然改变二者之一，
//! 而无需引入哈希依赖（语义上「识别新构建」用元数据足够，误报的
//! 代价只是多显示一次提示条，无功能影响）。

use std::path::Path;

/// 指纹记录文件（放应用 config 目录，内容两行：字节数 + Unix 秒）。
const BUILD_ID_FILE: &str = "build-id.txt";

/// 判断当前可执行文件是否为上次记录过的同一构建（首次运行恒为「新
/// 构建」）。新构建时写回当前指纹并返回 `true`；同一构建返回 `false`。
///
/// `config_dir`：状态文件目录（须已存在）；`exe`：当前可执行文件路径
/// （调用方用 `std::env::current_exe()` 取得，测试可注入任意文件）。
///
/// 降级策略：元数据读取失败 / 指纹文件 IO 失败一律返回 `false`——
/// 引导提示是锦上添花，绝不因它失败或打扰用户。
pub fn is_new_build(config_dir: &Path, exe: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(exe) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let record = config_dir.join(BUILD_ID_FILE);
    if let Ok(prev) = std::fs::read_to_string(&record) {
        // 格式不符视为无记录（首次/损坏），走「新构建」分支重写
        let mut lines = prev.lines();
        if lines.next() == Some(&meta.len().to_string()) && lines.next() == Some(&secs.to_string())
        {
            return false;
        }
    }
    // 写回失败静默：下次启动会再次提示一次（代价可忽略），不值得为它失败
    let _ = std::fs::write(record, format!("{}\n{}\n", meta.len(), secs));
    true
}

#[cfg(test)]
mod tests {
    //! 纯逻辑测试：注入临时目录与任意文件，不碰真实可执行文件。

    use super::*;

    /// 同一文件第二次查询应返回 false（指纹已记录）；文件变化后应返回
    /// true（识别为新构建）。
    #[test]
    fn fingerprint_stable_until_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "mak-buildid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fake-exe");
        std::fs::write(&exe, b"binary-v1").unwrap();

        assert!(is_new_build(&dir, &exe), "首次应为新构建");
        assert!(!is_new_build(&dir, &exe), "同一构建第二次查询不应提示");

        // 重建：内容变化带动 mtime/size 变化
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&exe, b"binary-v2-with-different-size").unwrap();
        assert!(is_new_build(&dir, &exe), "文件变化应识别为新构建");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// exe 不存在（读取失败）应降级为 false，不 panic 也不写记录。
    #[test]
    fn missing_exe_degrades_to_false() {
        let dir = std::env::temp_dir().join(format!("mak-buildid-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_new_build(&dir, &dir.join("no-such-exe")));
        assert!(!dir.join(BUILD_ID_FILE).exists(), "失败路径不应写记录");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
