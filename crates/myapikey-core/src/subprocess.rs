//! 子进程启动的跨平台小工具。
//!
//! 职责：统一给所有内部子进程（开浏览器的 shell 命令）附加
//! "不创建控制台窗口" 的启动标志。
//!
//! 为什么需要：Windows 上，GUI 子系统的进程（如 Tauri 窗口程序
//! `windows_subsystem = "windows"`）本身没有控制台，它 spawn 的每个
//! 控制台程序子进程默认会被分配一个**新的控制台窗口**——
//! 表现为屏幕上不断闪现又消失的黑色小窗。加上 `CREATE_NO_WINDOW`
//! 后子进程完全无窗运行，stdout/stderr 仍走管道，行为不变。
//!
//! 对终端里运行的 CLI 完全无影响（其子进程本就不开新窗），
//! 对非 Windows 平台是空操作。

use std::process::Command;

/// Windows `CreateProcess` 的 `CREATE_NO_WINDOW` 标志位（0x08000000）。
///
/// 给命令附加该标志后，子进程不会创建控制台窗口（GUI 程序调
/// 外部命令时消除闪窗的关键）。仅在 Windows 上生效，其他平台调用是空操作。
pub fn no_window(cmd: &mut Command) {
    // Windows 的 CommandExt 提供 creation_flags；单独拆出 cfg 块，
    // 保证非 Windows 平台连该 trait 都不引入，零开销编译。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // creation_flags 是替换语义（默认 0），这里只设这一个标志即可。
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // 非 Windows 平台：吞掉未使用参数警告，函数体为空。
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}
