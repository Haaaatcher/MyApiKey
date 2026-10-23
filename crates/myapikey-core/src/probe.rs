//! key 有效性探测：按协议标签调用 `GET /models`。
//!
//! ## 为什么是 /models
//!
//! 两家协议都提供该端点：纯元数据、不消耗 token、响应小。
//! 三方平台不保证实现它——所以探测结论里专门有“无法验证”这一档，
//! **探测失败绝不阻塞保存**（ADR D5）。
//!
//! ## 请求形状（按协议，docs/design.md §6）
//!
//! - `OpenAi`：`GET {base}/models`，头 `Authorization: Bearer <key>`。
//!   base 不以 `/v1` 结尾时依次尝试 `{base}/models` 与 `{base}/v1/models`
//!   （三方平台对 /v1 的约定不统一，ADR D1：不做归一化，尝试两种）。
//! - `Anthropic`：`GET {base}/v1/models`，头 `x-api-key: <key>` 与
//!   `anthropic-version: 2023-06-01`（服务端必填）。
//! - `Custom(_)`：不探测（自定义协议没有已知的探测方式）。
//!
//! ## 状态码 → 结论
//!
//! | HTTP | 结论 |
//! |---|---|
//! | 200 | [`CheckStatus::Valid`] |
//! | 401 | [`CheckStatus::Invalid`] |
//! | 403 | [`CheckStatus::NoPermission`]（key 被接受但无权限） |
//! | 429 | [`CheckStatus::RateLimited`]（key 有效但限额/限流） |
//! | 其他/无响应 | [`CheckStatus::Unverifiable`] |
//!
//! 记录上的 `extra_headers` 会原样附加到探测请求
//! （例如多 workspace key 必须带 `anthropic-workspace-id` 才能得到正确响应）。

use crate::model::{Protocol, Record};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 单次探测的超时时间。校验是辅助功能，不值得让用户等更久。
const TIMEOUT: Duration = Duration::from_secs(10);

/// 探测结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// 200：key 有效（服务端正常返回模型列表）。
    Valid,
    /// 401：key 无效/被删/过期。
    Invalid,
    /// 403：key 本身有效，但没有访问权限。
    NoPermission,
    /// 429：key 有效，但配额用尽或被限流。
    RateLimited,
    /// 无法验证：端点不存在（404）、网络失败、协议不支持探测等。
    Unverifiable,
}

impl CheckStatus {
    /// 给 CLI/GUI 展示用的短英文标签（用户可见文本，见 AGENTS.md 语言分层）。
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Valid => "valid",
            CheckStatus::Invalid => "invalid",
            CheckStatus::NoPermission => "no permission",
            CheckStatus::RateLimited => "rate limited",
            CheckStatus::Unverifiable => "unverifiable",
        }
    }
}

/// 对一条记录发起探测，返回（结论, HTTP 状态码）。
///
/// `http` 为 `None` 表示请求根本没得到 HTTP 响应（超时/DNS/连接失败）。
/// 本函数**不返回 Err**：所有失败都归类为“无法验证”，调用方无需区分错误。
pub fn probe(record: &Record) -> (CheckStatus, Option<u16>) {
    match &record.protocol {
        Protocol::OpenAi => probe_openai(record),
        Protocol::Anthropic => probe_anthropic(record),
        // 自定义协议没有可依赖的探测端点约定
        Protocol::Custom { .. } => (CheckStatus::Unverifiable, None),
    }
}

/// 构造 blocking 客户端。探测遵循系统代理环境变量（reqwest 默认行为）。
fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        // 构造失败极端罕见（如 TLS 后端初始化失败），退化为无自定义配置
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// OpenAI 协议探测。URL 构造逻辑独立成纯函数以便测试（见 [`openai_candidate_urls`]）。
fn probe_openai(record: &Record) -> (CheckStatus, Option<u16>) {
    for url in openai_candidate_urls(&record.base_url) {
        let mut req = client()
            .get(&url)
            .header("Authorization", format!("Bearer {}", record.api_key));
        for (k, v) in &record.extra_headers {
            req = req.header(k, v);
        }
        if let Ok(resp) = req.send() {
            let code = resp.status().as_u16();
            // 404 说明这个 URL 形态不对，换下一个候选；其他状态码已经是“答案”
            if code != 404 {
                return (status_from_http(code), Some(code));
            }
        }
    }
    (CheckStatus::Unverifiable, None)
}

/// Anthropic 协议探测：`{base}/v1/models` + 必填版本头。
fn probe_anthropic(record: &Record) -> (CheckStatus, Option<u16>) {
    let url = format!("{}/v1/models", record.base_url.trim_end_matches('/'));
    let mut req = client()
        .get(&url)
        .header("x-api-key", &record.api_key)
        // 服务端必填的版本头（现行唯一推荐值，调研 2026-10）
        .header("anthropic-version", "2023-06-01");
    for (k, v) in &record.extra_headers {
        req = req.header(k, v);
    }
    match req.send() {
        Ok(resp) => {
            let code = resp.status().as_u16();
            (status_from_http(code), Some(code))
        }
        Err(_) => (CheckStatus::Unverifiable, None),
    }
}

/// OpenAI 协议的候选 URL 列表（纯函数，可测试）。
///
/// - base 以 `/v1` 或 `/v1/` 结尾：只试 `{base}/models`（标准形态）；
/// - 否则：先试 `{base}/models`，404 再试 `{base}/v1/models`
///   （覆盖 base 给的是 `https://host` 而服务实际挂在 `/v1` 下的平台）。
pub fn openai_candidate_urls(base: &str) -> Vec<String> {
    let b = base.trim_end_matches('/');
    let with_v1 = b.ends_with("/v1");
    let mut urls = Vec::new();
    urls.push(format!("{b}/models"));
    if !with_v1 {
        urls.push(format!("{b}/v1/models"));
    }
    urls
}

/// HTTP 状态码 → 探测结论（纯函数，可测试）。
pub fn status_from_http(code: u16) -> CheckStatus {
    match code {
        200..=299 => CheckStatus::Valid,
        401 => CheckStatus::Invalid,
        403 => CheckStatus::NoPermission,
        429 => CheckStatus::RateLimited,
        _ => CheckStatus::Unverifiable,
    }
}

/// 供 CLI 调用的批量探测入口：返回每条记录的 `(id, 结论, HTTP 状态码)`。
/// 逐条顺序执行（记录量小，并发收益抵不过复杂度）。
pub fn probe_all<'a>(
    records: impl Iterator<Item = &'a Record>,
) -> Vec<(uuid::Uuid, CheckStatus, Option<u16>)> {
    records
        .map(|r| {
            let (status, http) = probe(r);
            (r.id, status, http)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! 只测纯函数（URL 构造、状态码映射）；真实网络探测由手动冒烟覆盖，
    //! 不在单测中依赖外网。

    use super::*;

    #[test]
    fn openai_urls_with_v1_base() {
        assert_eq!(
            openai_candidate_urls("https://api.openai.com/v1"),
            vec!["https://api.openai.com/v1/models"]
        );
    }

    #[test]
    fn openai_urls_without_v1_base_tries_both() {
        assert_eq!(
            openai_candidate_urls("https://gateway.example.com/"),
            vec![
                "https://gateway.example.com/models",
                "https://gateway.example.com/v1/models"
            ]
        );
    }

    #[test]
    fn http_code_mapping() {
        assert_eq!(status_from_http(200), CheckStatus::Valid);
        assert_eq!(status_from_http(401), CheckStatus::Invalid);
        assert_eq!(status_from_http(403), CheckStatus::NoPermission);
        assert_eq!(status_from_http(429), CheckStatus::RateLimited);
        assert_eq!(status_from_http(404), CheckStatus::Unverifiable);
        assert_eq!(status_from_http(500), CheckStatus::Unverifiable);
    }

    #[test]
    fn custom_protocol_is_unverifiable_without_network() {
        let mut r = Record::new(
            "x".into(),
            Protocol::Custom {
                name: "azure".into(),
            },
            "https://x".into(),
            "k".into(),
        );
        r.protocol = Protocol::Custom {
            name: "azure".into(),
        };
        // 不会发起任何网络请求
        let (status, http) = probe(&r);
        assert_eq!(status, CheckStatus::Unverifiable);
        assert!(http.is_none());
    }
}
