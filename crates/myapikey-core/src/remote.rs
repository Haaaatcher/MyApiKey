//! 远端同步通道（D25）：GitHub Contents API 直连，**单文件 CAS**。
//!
//! 取代系统 git（M4）：远端被抽象为"一个文件 + blob sha 乐观并发"。
//! 合并语义不在此层——vault 数据层的 LWW+墓碑见 [`crate::merge`]，
//! 本模块只负责"按 sha 条件读写一个文件"。
//!
//! CAS 语义（GitHub Contents API 实测，POC 2026-10-29）：
//! - 上传时携带旧 blob sha = 乐观锁；远端已变 → `409 Conflict`
//!   （错误信息含远端现 sha 但**不含内容**，冲突后须重新拉取）；
//! - 不携带 sha = 创建，远端已存在同名文件 → 422；
//! - 每次 PUT 在远端产生一个 git commit（message 透传），GitHub
//!   网页历史照旧可见——历史与迁移兼容的关键。

use crate::error::{Error, Result};
use crate::github::base64_decode;

/// `Box<dyn SyncStore>` 自动获得 trait 能力（调用方装箱后可直接当
/// SyncStore 用，避免到处 `&*box` 解引用）。
impl SyncStore for Box<dyn SyncStore> {
    fn fetch_head(&self) -> Result<Option<RemoteHead>> {
        (**self).fetch_head()
    }
    fn put_blob(&self, message: &str, bytes: &[u8], expect_sha: Option<&str>) -> Result<String> {
        (**self).put_blob(message, bytes, expect_sha)
    }
    fn download_ref(&self, ref_sha: &str) -> Result<Vec<u8>> {
        (**self).download_ref(ref_sha)
    }
}

/// 远端单文件的当前状态。
#[derive(Debug, Clone)]
pub struct RemoteHead {
    /// 远端当前 blob sha（CAS 凭据；下次上传必须携带它）。
    pub sha: String,
    /// 文件内容（vault 密文）。
    pub bytes: Vec<u8>,
}

/// 同步远端抽象：传输层三操作。合并、加解密、状态记账都在调用方。
pub trait SyncStore {
    /// 远端当前状态；`None` = 远端还没有这个文件（空仓库/首次创建）。
    ///
    /// # Errors
    /// 网络失败、认证失败、仓库不存在等（冲突不可能出现在 GET 上）。
    fn fetch_head(&self) -> Result<Option<RemoteHead>>;

    /// 上传一个版本（CAS）。`expect_sha = None` 创建新文件，
    /// `Some(旧sha)` = 仅当远端当前仍是该 sha 时替换。
    ///
    /// 成功返回**新的 blob sha**。
    ///
    /// # Errors
    /// - [`Error::RemoteConflict`]：远端已变为其他版本（409）——调用方
    ///   应重新 [`SyncStore::fetch_head`] 并走合并流程；
    /// - 网络/认证/权限失败；远端已存在而未带 sha（422）也归此类提示。
    fn put_blob(&self, message: &str, bytes: &[u8], expect_sha: Option<&str>) -> Result<String>;

    /// 取历史版本内容（`?ref=<commit sha>`）。历史列表能力在
    /// GitHub 网页与 commits API，mak 当前只暴露单版本下载。
    ///
    /// # Errors
    /// 网络/认证失败；ref 不存在（404）。
    fn download_ref(&self, ref_sha: &str) -> Result<Vec<u8>>;
}

/// GitHub Contents API 实现。
///
/// `repo` = `"owner/name"`（由远端 URL 解析，见 [`parse_github_repo`]）。
pub struct GitHubStore {
    /// GitHub token（device flow 授权，repo scope）。
    pub token: String,
    /// `owner/name` 形式的仓库标识。
    pub repo: String,
}

impl GitHubStore {
    fn api_url(&self, query: Option<&str>) -> String {
        match query {
            Some(q) => format!(
                "https://api.github.com/repos/{}/contents/vault.mkv?{q}",
                self.repo
            ),
            None => format!(
                "https://api.github.com/repos/{}/contents/vault.mkv",
                self.repo
            ),
        }
    }

    /// 统一请求通道：JSON 序列化 + Bearer 认证，返回 (状态码, 响应体)。
    ///
    /// **连接类失败自动重试**（DNS/TCP/TLS/超时——间歇性网络抖动的
    /// 典型形态，大陆直连 GitHub 的常态）：最多 3 次尝试、指数退避
    /// 1s/2s。HTTP 状态码错误（401/403/404/409/422/5xx）**不重试**——
    /// 语义失败重试无意义，交由调用方按状态码处理。
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String)> {
        const MAX_ATTEMPTS: usize = 3;
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| Error::Usage("internal bug: invalid HTTP method".into()))?;

        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                // 指数退避：第 2 次前 1s，第 3 次前 2s
                std::thread::sleep(std::time::Duration::from_secs(1 << (attempt - 1)));
            }
            let mut r = client.request(method.clone(), url);
            if let Some(b) = &body {
                r = r.json(b);
            }
            match r
                .header(
                    "User-Agent",
                    concat!("myapikey/", env!("CARGO_PKG_VERSION")),
                )
                .header("Accept", "application/vnd.github+json")
                .bearer_auth(&self.token)
                .send()
            {
                Ok(resp) => {
                    let st = resp.status().as_u16();
                    let text = resp.text().unwrap_or_default();
                    return Ok((st, text));
                }
                Err(e) => last_err = Some(e),
            }
        }
        let e = last_err.expect("loop ran at least once");
        Err(Error::Usage(format!(
            "network error talking to GitHub after {MAX_ATTEMPTS} attempts: {e}"
        )))
    }

    fn api_error(st: u16, body: &str) -> Error {
        if st == 0 {
            // 请求根本没发出去/未收到响应：网络层失败（reqwest 的错误文本）
            return Error::Usage(format!("network error talking to GitHub: {body}"));
        }
        let summary: String = body.chars().take(300).collect();
        Error::Usage(format!("GitHub API returned {st}: {summary}"))
    }

    /// 401/403 的归类：403 有多种成因（令牌失效 / 限流 / 缺 UA 被拒 /
    /// 权限不足），一律归为"过期"会掩盖真实原因——因此透传响应原文
    /// （GitHub 的 message 通常自解释，如缺 UA、rate limit、resource
    /// not accessible），本方法只做"是否限流"的归类与限量提示。
    fn auth_or_limit(st: u16, body: &str) -> Error {
        let lower = body.to_ascii_lowercase();
        if lower.contains("rate limit") {
            return Error::Usage(
                "GitHub API rate limit exceeded; wait a minute and try again".into(),
            );
        }
        let summary: String = body.chars().take(200).collect();
        Error::Usage(format!(
            "GitHub rejected the request (HTTP {st}): {summary}
               Common causes: expired token (re-authorize with `mak init`),              missing User-Agent (client bug), or insufficient permissions."
        ))
    }
}

impl SyncStore for GitHubStore {
    fn fetch_head(&self) -> Result<Option<RemoteHead>> {
        let (st, body) = self.request("GET", &self.api_url(None), None)?;
        match st {
            200 => {
                #[derive(serde::Deserialize)]
                struct File {
                    sha: String,
                    content: String,
                }
                let f: File = serde_json::from_str(&body)
                    .map_err(|e| Error::Usage(format!("bad contents response: {e}")))?;
                // GitHub 返回的 base64 带 \n 折行，解码前过滤
                let clean: String = f.content.chars().filter(|c| !c.is_whitespace()).collect();
                let bytes = base64_decode(&clean)?;
                Ok(Some(RemoteHead { sha: f.sha, bytes }))
            }
            404 => Ok(None),
            s if s == 401 || s == 403 => Err(Self::auth_or_limit(s, &body)),
            s => Err(Self::api_error(s, &body)),
        }
    }

    fn put_blob(&self, message: &str, bytes: &[u8], expect_sha: Option<&str>) -> Result<String> {
        let mut body = serde_json::json!({
            "message": message,
            "content": crate::github::base64_encode(bytes),
        });
        if let Some(sha) = expect_sha {
            body["sha"] = serde_json::Value::String(sha.to_string());
        }
        let (st, resp) = self.request("PUT", &self.api_url(None), Some(body))?;
        match st {
            200 | 201 => {
                #[derive(serde::Deserialize)]
                struct PutResp {
                    content: Content,
                }
                #[derive(serde::Deserialize)]
                struct Content {
                    sha: String,
                }
                let r: PutResp = serde_json::from_str(&resp)
                    .map_err(|e| Error::Usage(format!("bad contents PUT response: {e}")))?;
                Ok(r.content.sha)
            }
            // 实测：陈旧 sha → 409；无 sha 而文件已存在 → 422。两者都是
            // "远端不处于调用方以为的状态"，调用方拉取合并即可
            409 | 422 => Err(Error::RemoteConflict),
            s if s == 401 || s == 403 => Err(Self::auth_or_limit(s, &resp)),
            s => Err(Self::api_error(s, &resp)),
        }
    }

    fn download_ref(&self, ref_sha: &str) -> Result<Vec<u8>> {
        let (st, body) =
            self.request("GET", &self.api_url(Some(&format!("ref={ref_sha}"))), None)?;
        match st {
            200 => {
                #[derive(serde::Deserialize)]
                struct File {
                    content: String,
                }
                let f: File = serde_json::from_str(&body)
                    .map_err(|e| Error::Usage(format!("bad contents response: {e}")))?;
                let clean: String = f.content.chars().filter(|c| !c.is_whitespace()).collect();
                base64_decode(&clean)
            }
            s => Err(Self::api_error(s, &body)),
        }
    }
}

/// 从远端地址解析 `owner/name`。兼容历史形态（旧版把 https clone URL
/// 存进 config）：`https://github.com/{owner}/{name}[.git]`；
/// 也直接接受裸 `owner/name`。
///
/// # Errors
/// 非 GitHub 形态（M4 后远端仅支持 GitHub 私有仓库）。
pub fn parse_github_repo(url: &str) -> Result<String> {
    let s = url.trim().trim_end_matches("/").trim_end_matches(".git");
    let tail = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .unwrap_or(s);
    // SSH 形态（git@github.com:owner/name）显式拒绝：按 / 切会把它
    // 解析成 "git@github.com:owner/name" 这种垃圾 repo id，后续 API
    // 全部 404、错误语焉不详（2026-10-31 审查修复）
    if tail.contains(':') || tail.starts_with("git@") {
        return Err(Error::Usage(format!(
            "`{url}` is not supported: SSH clone URLs are not accepted; \
             use owner/name or a github.com URL"
        )));
    }
    let parts: Vec<&str> = tail.split('/').collect();
    match parts.as_slice() {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(format!("{owner}/{name}")),
        _ => Err(Error::Usage(format!(
            "`{url}` is not a GitHub repository (expected owner/name or a github.com URL); \
             syncing only supports GitHub private repositories"
        ))),
    }
}

#[cfg(test)]
pub(crate) mod mem {
    //! 进程内 `SyncStore` 实现：单文件 CAS 语义的最小模拟，供
    //! sync 全流程测试（双设备收敛、竞态冲突）使用。
    use super::*;
    use std::cell::RefCell;

    /// 可注入竞态的内存远端：`conflict_next` 置位后，下一次上传无视
    /// expect_sha 直接按冲突处理（模拟"另一台设备抢先上传"）。
    #[derive(Default)]
    pub struct MemStore(pub RefCell<MemState>);

    #[derive(Default)]
    pub struct MemState {
        pub sha: String,
        pub bytes: Vec<u8>,
        pub history: Vec<(String, Vec<u8>)>,
        pub conflict_next: bool,
    }

    impl SyncStore for MemStore {
        fn fetch_head(&self) -> Result<Option<RemoteHead>> {
            let st = self.0.borrow();
            Ok(if st.sha.is_empty() {
                None
            } else {
                Some(RemoteHead {
                    sha: st.sha.clone(),
                    bytes: st.bytes.clone(),
                })
            })
        }

        fn put_blob(&self, message: &str, bytes: &[u8], expect: Option<&str>) -> Result<String> {
            let mut st = self.0.borrow_mut();
            let conflict = st.conflict_next
                || match expect {
                    Some(e) => *e != st.sha,
                    None => !st.sha.is_empty(),
                };
            if conflict {
                st.conflict_next = false; // 一次性竞态注入：只在下一次上传生效
                return Err(Error::RemoteConflict);
            }
            let n = st.history.len() + 1;
            let sha = format!("sha-{n:02}-{:02x}", bytes.len());
            st.history.push((message.to_string(), bytes.to_vec()));
            st.sha = sha.clone();
            st.bytes = bytes.to_vec();
            st.conflict_next = false;
            Ok(sha)
        }

        fn download_ref(&self, ref_sha: &str) -> Result<Vec<u8>> {
            let st = self.0.borrow();
            st.history
                .iter()
                .find(|(s, _)| s == ref_sha)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| Error::Usage(format!("ref {ref_sha} not found")))
        }
    }
}
