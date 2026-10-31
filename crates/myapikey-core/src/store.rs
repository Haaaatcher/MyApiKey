//! vault 本地存储（D25 瘦身）：文件布局常量与 URL 校验。
//!
//! 历史：本模块曾是"本地 git 仓库管理层"（init/commit/fetch/merge/push
//! 的全部子进程封装，ADR D8）。M4（D25）移除系统 git 依赖后，同步改由
//! [`crate::remote`] 的 GitHub Contents API 直连承担，合并语义在
//! [`crate::merge`]——本模块只剩文件布局与 URL 校验两个纯函数职责。

/// vault 密文文件名（位于 `<config_dir>/repo/` 下）。
pub const VAULT_FILE: &str = "vault.mkv";

/// 校验 base URL：仅 http/https 且主机非空。
///
/// 这是**记录字段**的校验（api_key 之外另一件要手填的东西），与远端
/// 地址校验（[`crate::remote::parse_github_repo`]）是两回事。
///
/// 宽松理由：base_url 是用户手填的标签性信息（协议、路径形态千差万
/// 别——`/v1`、`/compatible-mode/v1`、`/api/anthropic`），mak 不做协议
/// 推断（AGENTS.md 红线），只拦截明显写错的形态。
pub fn is_valid_http_url(s: &str) -> bool {
    let t = s.trim();
    let Some(rest) = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
    else {
        return false;
    };
    if t.chars().any(char::is_whitespace) {
        return false;
    }
    let host_end = rest.find(['/', '?', ':']).unwrap_or(rest.len());
    !rest[..host_end].is_empty()
}

#[cfg(test)]
mod tests {
    use super::is_valid_http_url;

    #[test]
    fn http_url_validation() {
        assert!(is_valid_http_url("https://api.example.com/v1"));
        assert!(is_valid_http_url("http://localhost:8080"));
        assert!(!is_valid_http_url("ftp://x"));
        assert!(!is_valid_http_url("https://"));
        assert!(!is_valid_http_url("not a url"));
    }
}
