//! 终端表格对齐辅助：按**显示宽度**（而非字符数）补空格。
//!
//! 问题背景：中文字符在终端占 2 列，而 Rust 格式化 `{:<18}` 按字符数补空格，
//! 含中文的列必然错位。本模块提供宽度感知的填充与截断。
//!
//! 宽度判定为自实现的东亚宽字符区间表（覆盖 CJK 统一表意、扩展、
//! 全角符号、谚文、假名等常用区段；emoji 等复杂情况不在工具界面文案范围），
//! 不引入 unicode-width 依赖（AGENTS.md：新依赖需征得同意）。

/// 该字符在终端中占的列数（宽字符 2，其余 1）。
pub fn char_width(c: char) -> usize {
    if is_wide(c) {
        2
    } else {
        1
    }
}

/// 东亚宽字符判定（Unicode 中标记为 East Asian Wide/Fullwidth 的主要区段）。
fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        // 谚文兼容字母 / CJK 部首与符号
        0x1100..=0x115F
        | 0x2E80..=0x303E
        // 假名、注音、CJK 兼容
        | 0x3041..=0x33FF
        // CJK 扩展 A
        | 0x3400..=0x4DBF
        // CJK 统一表意（日常中文主体）
        | 0x4E00..=0x9FFF
        // 彝文
        | 0xA000..=0xA4CF
        // 谚文音节
        | 0xAC00..=0xD7A3
        // CJK 兼容表意
        | 0xF900..=0xFAFF
        // CJK 兼容形式（竖排形式）
        | 0xFE30..=0xFE4F
        // 全角 ASCII / 全角符号（！＂＃…，全角冒号等）
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        // CJK 扩展 B 及以后（第二/三平面）
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

/// 字符串的终端显示宽度（各字符宽度之和）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度右补空格；超过 `width` 时不截断（截断用 [`truncate_width`]）。
pub fn pad_to(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// 按显示宽度截断：保留的前缀加 `…`（宽 1）后总宽度不超过 `max`。
pub fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = char_width(c);
        // 内容 + '…'（宽 1）的总显示宽度不超过 max
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    //! 宽度表、填充、截断的行为验证（中英文混排是核心场景）。

    use super::*;

    #[test]
    fn width_of_mixed_text() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4, "每个汉字占 2 列");
        assert_eq!(display_width("a中b"), 4);
        assert_eq!(display_width("："), 2, "全角标点占 2 列");
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn pad_makes_equal_display_width() {
        let a = pad_to("abc", 10);
        let b = pad_to("中文", 10);
        assert_eq!(display_width(&a), 10);
        assert_eq!(display_width(&b), 10);
        assert_eq!(a, "abc       ");
        assert_eq!(b, "中文      ");
        // 超宽不截断
        assert_eq!(pad_to("中中中中中中", 4), "中中中中中中");
    }

    #[test]
    fn truncate_by_display_width() {
        assert_eq!(truncate_width("abc", 6), "abc");
        assert_eq!(truncate_width("abcdefgh", 6), "abcde…");
        // 6 个汉字宽 12 列 → 截到宽度 4 加省略号
        assert_eq!(truncate_width("中中中中中中", 6), "中中…");
        // 短串原样
        assert_eq!(truncate_width("中文", 10), "中文");
    }
}
