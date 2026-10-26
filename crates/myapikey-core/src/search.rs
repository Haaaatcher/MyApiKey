//! 记录搜索：fzf 风格模糊匹配 + 评分排序。
//!
//! ## 匹配规则（2026-10-25 决议，CLI 与 GUI 行为对齐；GUI 前端有一份
//! TypeScript 等价实现 `bestSearchScore`，改规则时两处必须同步）
//!
//! - 子串精确命中优先于子序列模糊命中；
//! - 参与 matching 的字段：名称（权重 30）、备注（权重 8）、URL 分词后的
//!   各段——域名段权重 10、路径段权重 4；
//! - **协议不参与**：协议是兼容性标签，DeepSeek 等都用 openai 协议，按它
//!   搜会把所有兼容服务商都命中，违背"搜服务商"的直觉；
//! - **URL 先剥 `https?://` 再按 `/` 分词**：协议前缀人人都有（h/t/p/s），
//!   不剥的话短查询会用子序列命中一切；分段同时阻断跨段的伪命中
//!   （h 在主机名、i 在路径拼成 "hi"）；
//! - **1-2 字符的短查询只做子串精确匹配**：子序列对短词只会制造噪音
//!   （"hi" 命中 anthropic 里分离的 h 与 i）。
//!
//! 两端分数数值不要求逐位一致（多字节文本上字节/码元度量的天然差异），
//! 一致性要求是**命中集合与排序结果相同**——TS mock 同此口径。
//!
//! 评分（子串）：`1000 - min(idx, 500) + 词首加 100`；
//! 评分（子序列，仅 3 字符起）：逐字符 `10 + 连续长度×4 + 词首 8 - 跳跃惩罚 min(gap, 5)`。

use crate::model::Record;

/// 一条记录对查询词的得分；`None` = 任何字段都不匹配。
fn score_field(text: &str, q: &str) -> Option<i64> {
    let idx = text.find(q)?;
    Some(1000 - idx.min(500) as i64 + if is_word_boundary(text, idx) { 100 } else { 0 })
}

/// 子序列匹配：query 每个字符按顺序出现在目标中即可；3 字符起启用。
fn score_subsequence(target: &str, q: &str) -> Option<i64> {
    // 按字符数而非字节数判短查询：两个汉字（6 字节）若按字节会 >=3 而
    // 绕过本守卫，恰好放过设计要拦的噪音（2026-10-26 审查修复；TS 端
    // mock 同语义）
    if q.chars().count() < 3 {
        return None;
    }
    let mut ti = 0usize;
    let mut score = 0i64;
    let mut run = 0i64;
    for ch in q.chars() {
        // 从 ti 起找下一个 ch（字符级推进，避免字节切片切进多字节字符）
        let found = target[ti..].find(ch)? + ti;
        run = if found == ti { run + 1 } else { 1 };
        score += 10
            + run * 4
            + if is_word_boundary(target, found) {
                8
            } else {
                0
            }
            - (found - ti).min(5) as i64;
        ti = found + ch.len_utf8();
    }
    Some(score)
}

/// 位置 i 是否为词首（开头，或位于分隔符 `- _ . / :` 空格之后）。
fn is_word_boundary(target: &str, i: usize) -> bool {
    i == 0 || "-_./: ".contains(target[..i].chars().next_back().unwrap())
}

/// 一条记录的总得分：各字段（含 URL 各段）独立评分取最高并叠加字段权重。
fn best_score(r: &Record, q: &str) -> Option<i64> {
    let q = q.to_lowercase();
    let mut fields: Vec<(String, i64)> = vec![
        (r.name.to_lowercase(), 30),
        (r.notes.as_deref().unwrap_or("").to_lowercase(), 8),
    ];
    // URL 分词：剥协议前缀后按 / 切段，域名段是服务商身份（权重高），
    // 路径段是端点细节（权重低但保留可搜，如 /openai/v1 里的 v1）
    let url_lower = r
        .base_url
        .to_lowercase()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    let segs: Vec<&str> = url_lower.split('/').filter(|s| !s.is_empty()).collect();
    for (i, seg) in segs.iter().enumerate() {
        fields.push(((*seg).to_string(), if i == 0 { 10 } else { 4 }));
    }

    fields
        .iter()
        .filter(|(text, _)| !text.is_empty())
        .filter_map(|(text, weight)| {
            score_field(text, &q)
                .or_else(|| score_subsequence(text, &q))
                .map(|s| s + weight)
        })
        .max()
}

/// 过滤并按得分降序排序；查询词为空时返回原顺序的全部记录。
pub fn search<'a>(records: impl IntoIterator<Item = &'a Record>, query: &str) -> Vec<&'a Record> {
    let query = query.trim();
    if query.is_empty() {
        return records.into_iter().collect();
    }
    let mut scored: Vec<(i64, &Record)> = records
        .into_iter()
        .filter_map(|r| best_score(r, query).map(|s| (s, r)))
        .collect();
    // sort_by 稳定：同分保持原顺序
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    //! 搜索规则回归：与 GUI 前端 TypeScript 实现行为对齐的边界用例。
    use super::*;

    fn rec(name: &str, url: &str, notes: &str, protocol: &str) -> Record {
        let mut r = Record::new(
            name.into(),
            crate::model::Protocol::from_label(protocol),
            url.into(),
            "sk-test-12345678".into(),
        );
        r.notes = (!notes.is_empty()).then(|| notes.to_string());
        r
    }

    #[test]
    fn substring_wins_and_protocol_is_excluded() {
        let records = [
            rec("Alpha", "https://api.openai.com/v1", "", "openai"),
            rec(
                "DeepSeek",
                "https://api.deepseek.com/openai/v1",
                "",
                "openai",
            ),
        ];
        // "openai" 只命中 URL/名称，不因协议全员命中
        let hits = search(records.iter(), "openai");
        assert_eq!(hits.len(), 2, "两条 URL 都含 openai");
        // Alpha 域名命中（权重 10）应排在 DeepSeek 路径段命中（权重 4）之前
        assert_eq!(hits[0].name, "Alpha");

        // 排除协议：查询词只出现在协议里时不应命中
        let only_proto = [rec(
            "Gemini",
            "https://generativelanguage.googleapis.com",
            "",
            "openai",
        )];
        assert!(search(only_proto.iter(), "openai").is_empty());
    }

    #[test]
    fn scheme_and_path_tokenization() {
        // 短查询 "hi" 不再命中 https 前缀的 h + api 的 i
        let records = [rec(
            "Claude Pro",
            "https://api.anthropic.com",
            "",
            "anthropic",
        )];
        assert!(search(records.iter(), "hi").is_empty());
        assert!(search(records.iter(), "http").is_empty());
        // "hi" 也不因子序列命中 anthropic 里的 h 与 i（短查询只做子串）
        let anth = [rec("Claude", "https://api.anthropic.com", "", "anthropic")];
        assert!(search(anth.iter(), "hi").is_empty());
        // 路径段可搜
        let with_path = [rec(
            "Beta",
            "https://gw.example.com/openai/v1",
            "",
            "openai",
        )];
        assert_eq!(search(with_path.iter(), "v1").len(), 1);
    }

    #[test]
    fn two_cjk_chars_stay_substring_only() {
        // 双汉字查询按字符数是"短查询"：不得启用子序列
        // （字节长度 6 曾绕过守卫，2026-10-26 审查修复的回归）
        let records = [rec("主力机", "https://api.example.com", "主账号", "openai")];
        // "主号" 是"主账号"的子序列但不是子串 → 不命中
        assert!(search(records.iter(), "主号").is_empty());
        // "主账" 是子串 → 命中
        assert_eq!(search(records.iter(), "主账").len(), 1);
    }

    #[test]
    fn notes_and_subsequence() {
        let records = [
            rec(
                "Alpha Prime",
                "https://api.example.com",
                "主力账号",
                "openai",
            ),
            rec("Beta", "https://gw.example.com", "备用", "openai"),
        ];
        let hits = search(records.iter(), "主力");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Alpha Prime");
        // 3 字符起子序列：gw 命中 gateway 型名称
        let gw = [rec("Beta Gateway", "https://api.example.com", "", "openai")];
        assert_eq!(search(gw.iter(), "gtw").len(), 1);
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let records = [
            rec("A", "https://a.example.com", "", "openai"),
            rec("B", "https://b.example.com", "", "openai"),
        ];
        assert_eq!(search(records.iter(), "").len(), 2);
        assert!(search(records.iter(), "zzz-no-match").is_empty());
    }
}
