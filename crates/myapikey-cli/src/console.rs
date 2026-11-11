//! Windows 控制台编码守卫。
//!
//! 问题（真实反馈）：在代码页为 GBK(936) 的 PowerShell / cmd 中，
//! 程序输出的 UTF-8 中文被按 GBK 解码，显示为乱码
//! （如「新主密码:」→「鑼頒富濱滂欙:」）。
//!
//! 修复方式：程序启动时把**当前控制台**的输入/输出代码页临时切到
//! UTF-8(65001)，进程退出（含 panic）时恢复原值——
//! - 只影响本控制台会话，不改系统默认；
//! - 恢复而不是永久修改，用户会话里其他依赖 GBK 的老工具不受影响；
//! - 无控制台（输出被管道重定向）时 GetConsoleCP 返回 0，自动跳过。
//!
//! 实现为直接 kernel32 FFI（约 15 行），不引入 windows-sys 依赖
//! （AGENTS.md：新依赖需征得同意，这里不值得）。

#[cfg(windows)]
mod imp {
    // 直接声明所需的两个 kernel32 函数，避免整包 windows-sys 依赖。
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleCP() -> u32;
        fn SetConsoleCP(code_page: u32) -> i32;
        fn GetConsoleOutputCP() -> u32;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
    }

    /// UTF-8 代码页。
    const CP_UTF8: u32 = 65001;

    /// 运行期守卫：Drop 时恢复进入前的代码页。
    pub struct ConsoleGuard {
        in_cp: u32,
        out_cp: u32,
    }

    impl Drop for ConsoleGuard {
        fn drop(&mut self) {
            // 恢复失败（控制台已消失等）无需处理：会话随进程一起结束
            // SAFETY: in_cp 来自构造时对同一控制台的 GetConsoleCP
            unsafe { SetConsoleCP(self.in_cp) };
            // SAFETY: out_cp 同上,来自 GetConsoleOutputCP
            unsafe { SetConsoleOutputCP(self.out_cp) };
        }
    }

    /// 把当前控制台切到 UTF-8，返回恢复守卫。
    /// 无控制台（输出重定向到管道/文件）时两个 Get 都返回 0，跳过切换；
    /// 返回的守卫 Drop 时恢复原值（0 值恢复调用无害）。
    pub(super) fn enter_utf8() -> ConsoleGuard {
        // SAFETY: GetConsoleCP 仅读取当前进程控制台的输入代码页属性
        let in_cp = unsafe { GetConsoleCP() };
        // SAFETY: GetConsoleOutputCP 仅读取输出代码页属性
        let out_cp = unsafe { GetConsoleOutputCP() };
        if in_cp != 0 && in_cp != CP_UTF8 {
            // SAFETY: CP_UTF8 是合法代码页;0 = 无控制台已被上面跳过
            unsafe { SetConsoleCP(CP_UTF8) };
        }
        if out_cp != 0 && out_cp != CP_UTF8 {
            // SAFETY: 同上,对输出侧代码页
            unsafe { SetConsoleOutputCP(CP_UTF8) };
        }
        ConsoleGuard { in_cp, out_cp }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 平台的空实现（终端均为 UTF-8）。
    pub struct ConsoleGuard;
    pub fn enter_utf8() -> ConsoleGuard {
        ConsoleGuard
    }
}

pub use imp::ConsoleGuard;

/// 供入口调用的便捷函数：进入 UTF-8 控制台并返回守卫。
/// 调用方必须把守卫保存在栈上直到所有输出完成。
pub fn enter_utf8() -> ConsoleGuard {
    imp::enter_utf8()
}
