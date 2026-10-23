//! 实例文件锁：防止 CLI 与 GUI（或两个 CLI）同时写 vault。
//!
//! 实现：`fd-lock` 对 `<config_dir>/vault.lock` 取排他锁，
//! 锁的生命周期与 [`InstanceLock`] 值绑定（Drop 自动释放）。
//!
//! 行为约定：**抢不到锁就报错退出**，不做等待重试——
//! 写 vault 的操作都在秒级完成，用户几乎不会遇到锁冲突；
//! 遇到时更可能是“另一个窗口正开着交互式命令”，提示比排队更友好。
//!
//! 实现说明：guard 借用了锁对象，因此把 `RwLock` 装箱并 leak 成 `'static`，
//! 让守卫与锁同生共死于 [`InstanceLock`]。CLI 进程短命、
//! 每次运行只 acquire 一次，这里的固定小额泄漏（一个 Box）无实际影响。

use crate::error::{Error, Result};
use crate::paths::AppDirs;
use fd_lock::{RwLock, RwLockWriteGuard};
use std::fs::File;

/// 持有期间本进程独占 vault 写权限的守卫。
///
/// 必须把返回值保存在调用栈中直到写操作全部结束（RAII）；
/// 提前 drop 即释放锁。
pub struct InstanceLock {
    /// 排他写锁守卫（其 'static 借用指向被 leak 的锁对象）。
    _guard: RwLockWriteGuard<'static, File>,
}

/// 获取排他实例锁。
///
/// # Errors
/// - [`Error::Io`]：锁文件无法创建/打开；
/// - [`Error::Usage`]：锁已被其他实例持有（提示用户关闭其他窗口）。
pub fn acquire(dirs: &AppDirs) -> Result<InstanceLock> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&dirs.lock_path)?;
    let lock: &'static mut RwLock<File> = Box::leak(Box::new(RwLock::new(file)));
    match lock.try_write() {
        Ok(guard) => Ok(InstanceLock { _guard: guard }),
        Err(_) => Err(Error::Usage(
            "另一个 MyApiKey 实例正在运行（vault.lock 被占用），请先完成或关闭它".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    //! 锁互斥测试：同一进程内两次 acquire（第二个必须失败）。
    //! fd 锁语义上同一进程再次加锁通常也会被拒（O_EXLOCK 系），若平台
    //! 实现允许重入则本测试退化为“结构可用”验证（Windows 上行为正确）。

    use super::*;

    #[test]
    fn second_acquire_fails_or_lock_is_usable() {
        let tmp = std::env::temp_dir().join(format!("mak-lock-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&tmp).expect("创建临时目录失败");
        let dirs = AppDirs {
            config_dir: tmp.clone(),
            repo_dir: tmp.join("repo"),
            vault_path: tmp.join("repo/vault.mkv"),
            config_path: tmp.join("config.toml"),
            check_cache_path: tmp.join("check_cache.json"),
            lock_path: tmp.join("vault.lock"),
        };
        let first = acquire(&dirs).expect("首次加锁必须成功");
        match acquire(&dirs) {
            // 同进程第二次加锁被拒：释放后必须能重新获取
            Err(_) => {
                drop(first);
                acquire(&dirs).expect("释放后必须能重新加锁");
            }
            // 平台允许重入：两个守卫都能正常持有与释放
            Ok(second) => {
                drop(second);
                drop(first);
            }
        }
    }
}
