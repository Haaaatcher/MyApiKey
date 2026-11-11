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
    let (status, http) = match &record.protocol {
        // Responses / Chat Completions 共享同一套 /models 端点与认证头
        Protocol::OpenAi | Protocol::OpenAiChat => probe_openai(record),
        // Messages / Text Completions 共享同一套 /v1/models 端点与认证头
        Protocol::Anthropic | Protocol::AnthropicComplete => probe_anthropic(record),
        // 自定义协议没有可依赖的探测端点约定
        Protocol::Custom { .. } => (CheckStatus::Unverifiable, None),
    };
    crate::log::debug(
        "probe",
        &format!("probe {}: {} ({:?})", record.name, status.label(), http),
    );
    (status, http)
}

/// 共享 blocking 客户端（连接池 + TLS 会话复用，2026-10-31 审查可选
/// 采纳：此前每条探测新建客户端，批量探测对同一主机的并发全都要重新
/// 握手）。探测遵循系统代理环境变量（reqwest 默认行为）。
fn client() -> reqwest::blocking::Client {
    static SHARED: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(TIMEOUT)
                .build()
                // 构造失败极端罕见（如 TLS 后端初始化失败），退化为无自定义配置
                .unwrap_or_else(|_| reqwest::blocking::Client::new())
        })
        .clone()
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

/// 批量探测的并发上限。记录量通常个位数到几十条，8 路并发已能同时
/// 压住所有慢端点；再高对总耗时没有增益（单条探测超时 10s 封顶），
/// 反而容易触发端点侧限流（429）让结果失真（2026-10-31 一键检测全部）。
pub const PROBE_CONCURRENCY: usize = 8;

/// 供 CLI/GUI 调用的并发批量探测入口：返回每条记录的
/// `(id, 结论, HTTP 状态码)`，顺序与输入一致。
///
/// 并发实现（2026-10-31 用户要求"一键检测全部 + 核心层高性能"）：
/// `std::thread::scope` 工作线程 + 原子游标任务队列，不引入线程池
/// 依赖；总耗时从"逐条之和"降为"约最慢一条"。
pub fn probe_all(records: &[Record]) -> Vec<(uuid::Uuid, CheckStatus, Option<u16>)> {
    crate::log::info(
        "probe",
        &format!("checking {} records concurrently", records.len()),
    );
    probe_all_with(
        records,
        PROBE_CONCURRENCY,
        &mut |_: uuid::Uuid, _: CheckStatus, _: Option<u16>| {},
    )
}

/// [`probe_all`] 的带回调版本：每条探测完成时立即触发 `on_each`
/// （从工作线程调用，内部已互斥串行化；回调里不要做重活）。
/// GUI 的批量进度展示走本入口，CLI 用一次性收集的 [`probe_all`] 即可。
///
/// # Panics
/// 工作线程内 `probe` 不会 panic（全部失败归为 `Unverifiable`）；互斥锁
/// 中毒时按 `into_inner` 恢复（持锁方不会 panic，中毒不可能发生）。
pub fn probe_all_with(
    records: &[Record],
    concurrency: usize,
    on_each: &mut (dyn FnMut(uuid::Uuid, CheckStatus, Option<u16>) + Send),
) -> Vec<(uuid::Uuid, CheckStatus, Option<u16>)> {
    if records.is_empty() {
        return Vec::new();
    }
    // 槽位预分配 + 每条写自己的下标：结果天然与输入同序，无需排序
    let mut slots: Vec<Option<(uuid::Uuid, CheckStatus, Option<u16>)>> =
        records.iter().map(|_| None).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(&mut slots);
    let callback = std::sync::Mutex::new(on_each);
    let workers = concurrency.min(records.len()).max(1);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let idx = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if idx >= records.len() {
                    break;
                }
                let record = &records[idx];
                let (status, http) = probe(record);
                let mut done = results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                done[idx] = Some((record.id, status, http));
                drop(done);
                let mut cb = callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cb(record.id, status, http);
            });
        }
    });

    slots
        .into_iter()
        .map(|s| s.expect("every slot must be filled"))
        .collect()
}

// ---------------------------------------------------------------------------
// 模型列表获取（2026-10-27 增补）
// ---------------------------------------------------------------------------

/// 拉取记录可用的模型名列表（`GET /models` 的 200 响应体 `data[].id`，
/// 两家协议响应形状一致）。
///
/// 与 [`probe`] 是同一个端点的两次利用：探测只取状态码，本函数取 body。
/// 失败（网络 / 非 200 / 解析不出 / Custom 协议）一律返回 `None`——
/// 调用方回落手输，**不报错不阻塞**（与 D5 探测的容错风格一致）。
///
/// 返回列表按服务端顺序保留、去重去空；数量上限 128（防御异常服务端
/// 返回海量条目撑爆 vault）。
pub fn fetch_models(record: &Record) -> Option<Vec<String>> {
    let result = fetch_models_inner(record);
    crate::log::debug(
        "probe",
        &format!(
            "fetch models {}: {} items",
            record.name,
            result.as_ref().map_or(0, Vec::len)
        ),
    );
    result
}

fn fetch_models_inner(record: &Record) -> Option<Vec<String>> {
    let body = match &record.protocol {
        // Responses / Chat Completions 共享同一套 models 端点与认证头
        Protocol::OpenAi | Protocol::OpenAiChat => {
            let mut got = None;
            for url in openai_candidate_urls(&record.base_url) {
                let mut req = client()
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", record.api_key));
                for (k, v) in &record.extra_headers {
                    req = req.header(k, v);
                }
                if let Ok(resp) = req.send() {
                    if resp.status().as_u16() == 404 {
                        continue; // 这个 URL 形态不对，换下一个候选
                    }
                    if resp.status().is_success() {
                        got = resp.text().ok();
                    }
                    // 非 200 且非 404：key/权限问题，列表拿不到
                    break;
                }
            }
            got
        }
        // Messages / Text Completions 共享同一套 models 端点与认证头
        Protocol::Anthropic | Protocol::AnthropicComplete => {
            let url = format!("{}/v1/models", record.base_url.trim_end_matches('/'));
            let mut req = client()
                .get(&url)
                .header("x-api-key", &record.api_key)
                .header("anthropic-version", "2023-06-01");
            for (k, v) in &record.extra_headers {
                req = req.header(k, v);
            }
            match req.send() {
                Ok(resp) if resp.status().is_success() => resp.text().ok(),
                _ => None,
            }
        }
        // 自定义协议没有已知的列表端点约定（与探测一致）
        Protocol::Custom { .. } => None,
    }?;
    let models = parse_models_body(&body);
    (!models.is_empty()).then_some(models)
}

/// 解析 list-models 响应体（纯函数，可测试）：`{"data": [{"id": …}, …]}`。
/// 两家协议形状一致；取 `id` 非空项，保序去重，上限 128 条。
pub fn parse_models_body(body: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        data: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        #[serde(default)]
        id: String,
    }
    let parsed: Resp = serde_json::from_str(body).unwrap_or(Resp { data: vec![] });
    let mut out: Vec<String> = Vec::new();
    for e in parsed.data {
        let id = e.id.trim().to_string();
        if id.is_empty() || out.contains(&id) {
            continue;
        }
        out.push(id);
        if out.len() >= 128 {
            break;
        }
    }
    out
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
        // 模型列表同样不发起网络请求
        assert!(fetch_models(&r).is_none());
    }

    /// list-models 响应体解析：常规形状 / 保序去重去空 / 坏 body 空。
    #[test]
    fn models_body_parsing() {
        let body = r#"{"object":"list","data":[{"id":"GLM-5.2"},{"id":"GLM-5.1"},{"id":"GLM-5.2"},{"id":""},{"id":" "}]}"#;
        assert_eq!(parse_models_body(body), vec!["GLM-5.2", "GLM-5.1"]);
        // Anthropic 同形状（display_name 等额外字段忽略）
        let body = r#"{"data":[{"id":"claude-opus-5","display_name":"Opus"}],"has_more":false}"#;
        assert_eq!(parse_models_body(body), vec!["claude-opus-5"]);
        // 坏 body / 空列表 → 空
        assert_eq!(parse_models_body("{broken"), Vec::<String>::new());
        assert_eq!(parse_models_body(r#"{"data":[]}"#), Vec::<String>::new());
    }

    /// 解析上限 128 条（防御异常服务端海量返回撑爆 vault）。
    #[test]
    fn models_body_capped_at_128() {
        let ids: Vec<String> = (0..500).map(|i| format!("m-{i}")).collect();
        let body = format!(
            "{{\"data\":[{}]}}",
            ids.iter()
                .map(|i| format!("{{\"id\":\"{i}\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let parsed = parse_models_body(&body);
        assert_eq!(parsed.len(), 128);
        assert_eq!(parsed[0], "m-0");
        assert_eq!(parsed[127], "m-127");
    }

    /// 本地假端点：每个请求由独立线程固定延迟 `delay_ms` 后回 401
    /// （并发响应真并发）。accept 循环刻意不退出——probe 对连接失败
    /// 会重试第二个候选 URL，服务提前关闭会让重试撞墙；handle 直接
    /// 丢弃（detach），随测试进程结束回收。
    fn slow_unauthorized_endpoint(delay_ms: u64) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                std::thread::spawn(move || {
                    // 先排空请求再延迟响应：Windows 对"接收缓冲区还有
                    // 未读数据就关闭"的连接直接回 RST，会把写出的响应
                    // 一并吞掉（close-with-unread-data 陷阱）
                    let mut buf = [0u8; 2048];
                    let _ = std::io::Read::read(&mut stream, &mut buf);
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    let _ = std::io::Write::write_all(
                        &mut stream,
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                    );
                    let _ = std::net::TcpStream::shutdown(&stream, std::net::Shutdown::Both);
                });
            }
        });
        format!("http://{addr}")
    }

    fn probe_record_named(name: &str, base: &str) -> Record {
        Record::new(
            name.to_string(),
            Protocol::OpenAiChat,
            base.to_string(),
            "sk-test".into(),
        )
    }

    /// 一键检测全部（并发批量探测）：8 条记录打同一个延迟 150ms 的本地
    /// 端点——并发下总耗时 ≈ 单条（~150ms），顺序执行则需 ~1.2s。断言
    /// 结果全覆盖、与输入同序、结论正确，且并发耗时显著低于顺序基线
    /// （2026-10-31 高性能批量探测回归）。
    #[test]
    fn probe_all_concurrent_covers_all_in_order_and_is_fast() {
        let base = slow_unauthorized_endpoint(150);
        let records: Vec<Record> = (0..8)
            .map(|i| probe_record_named(&format!("r{i}"), &base))
            .collect();
        let expect_ids: Vec<uuid::Uuid> = records.iter().map(|r| r.id).collect();

        let started = std::time::Instant::now();
        let results = probe_all(&records);
        let elapsed = started.elapsed();

        // 全覆盖 + 保序 + 结论（401 → Invalid）
        assert_eq!(results.len(), 8);
        let got_ids: Vec<uuid::Uuid> = results.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(got_ids, expect_ids, "结果必须与输入同序");
        for (_, status, http) in &results {
            assert_eq!(*status, CheckStatus::Invalid);
            assert_eq!(*http, Some(401));
        }
        // 并发（≈最慢一条 ~150ms+ 余量）显著快于顺序基线（8×150ms）
        assert!(
            elapsed < std::time::Duration::from_millis(600),
            "并发批量应在 ~单条时长内完成，实际 {elapsed:?}"
        );
    }

    /// 带回调版本：每条完成即触发，次数与记录数一致（GUI 进度展示依赖）。
    #[test]
    fn probe_all_with_reports_each_record_once() {
        let base = slow_unauthorized_endpoint(0);
        let records: Vec<Record> = (0..3)
            .map(|i| probe_record_named(&format!("c{i}"), &base))
            .collect();
        let mut seen: Vec<uuid::Uuid> = Vec::new();
        let results = probe_all_with(&records, 4, &mut |id, _, _| seen.push(id));
        assert_eq!(seen.len(), 3, "回调每条恰好一次");
        assert_eq!(results.len(), 3);
    }
}
