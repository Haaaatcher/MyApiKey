//! `makara` 二进制入口。

use std::io::IsTerminal;

fn main() {
    if let Err(e) = myapikey_cli::run() {
        // 错误行红色 ✗（视觉语言第三层：绿 ✔ 成功 / 黄 ⚠ 警告 / 红 ✗ 失败）；
        // 非 TTY（管道/CI）退化为素文本，不污染日志
        let tty = std::io::stderr().is_terminal();
        let mark = if tty {
            "\x1b[31m\u{2717}\x1b[0m"
        } else {
            "✗"
        };
        eprintln!("{mark} Error: {e}");
        std::process::exit(1);
    }
}
