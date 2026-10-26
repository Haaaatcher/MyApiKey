//! GitHub 集成：OAuth Device Flow 鉴权 + 仓库自动创建/识别。
//!
//! ## 解决的问题
//!
//! `mak init --github` 让用户免去“去 GitHub 网页建仓库 → 复制 URL”的手工步骤：
//! 终端显示 8 位设备码 → 用户在浏览器确认 → 程序自动检测/创建私有同步仓库并接入。
//!
//! ## Device Flow（RFC 8628，与 `gh auth login` 同款）
//!
//! 1. `POST https://github.com/login/device/code`（只需 client_id）→ 得到
//!    `device_code` / `user_code` / `verification_uri` / `interval`；
//! 2. 用户在浏览器打开 verification_uri 输入 user_code 授权；
//! 3. 程序轮询 `POST https://github.com/login/oauth/access_token` 直到成功，
//!    得到 `repo` scope 的 access_token；
//! 4. token 存入系统凭据库（account = `github`），主密码同等待遇：不落盘、可清除。
//!
//! ## 安全边界
//!
//! - client_id 是公开信息（可硬编码）；**client_secret 不参与** device flow；
//! - token 请求的 scope 只有 `repo`（GitHub 私有仓库创建与推送所需的最小常见集，
//!   这是 GitHub 的模型限制，无法按单仓库收窄）；
//! - token 只用于：调 GitHub API（查身份/建仓库）与 git HTTP 操作的 Authorization
//!   头（经环境变量注入，见 [`GitAuth`]），绝不写入仓库 URL、`.git/config` 或命令行参数；
//! - git 日常 push 的其他凭据来源（GCM/SSH）不受影响。

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// GitHub OAuth App 的 client_id。
///
/// Device flow 只需要 client_id（client_secret 不参与）。
/// 此值为作者注册的官方应用；特殊部署可用环境变量
/// `MYAPIKEY_GITHUB_CLIENT_ID` 覆盖（开发/测试用）。
pub const CLIENT_ID: &str = "Ov23liGzxUrdUwczY0LV";

/// 覆盖 client_id 的环境变量名。
pub const CLIENT_ID_ENV: &str = "MYAPIKEY_GITHUB_CLIENT_ID";

/// `--github` 不带名字时的默认仓库名。
pub const DEFAULT_REPO_NAME: &str = "myapikey-vault";

/// 凭据库中 GitHub token 的 account 名（与主密码的 `default` 区分开）。
const KEYCHAIN_ACCOUNT: &str = "github";

/// 设备码请求的 scope：创建私有仓库 + git 推送所需。
const SCOPE: &str = "repo";

/// 设备授权发起后返回的信息。
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    /// 轮询换 token 用的码（保密，不能展示给用户看）。
    pub device_code: String,
    /// 用户在浏览器输入的 8 位码（展示给用户）。
    pub user_code: String,
    /// 用户要打开的地址（固定为 https://github.com/login/device）。
    pub verification_uri: String,
    /// 整个流程的有效秒数（GitHub 默认 900）。
    #[serde(default)]
    pub expires_in: u64,
    /// 轮询间隔秒数（GitHub 默认 5，遇 slow_down 会要求加长）。
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// 取生效的 client_id（环境变量 > 内置官方应用值）。
///
/// # Errors
/// 理论上不失败（内置值恒存在）；保留 `Result` 以稳定调用方签名。
pub fn effective_client_id() -> Result<String> {
    if let Ok(id) = std::env::var(CLIENT_ID_ENV) {
        if !id.trim().is_empty() {
            return Ok(id.trim().to_string());
        }
    }
    Ok(CLIENT_ID.to_string())
}

/// 用系统默认浏览器打开 URL（零依赖实现：Windows `start` / macOS `open` / Linux `xdg-open`）。
///
/// # Errors
/// - [`Error::Usage`]：没有可用的打开命令（如无头 Linux）。失败不致命——
///   调用方应继续显示 verification_uri 让用户手动打开。
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        // start 的第一个引号参数是窗口标题，必须给一个空标题占位
        c.args(["/c", "start", "", url]);
        // GUI 程序里 cmd 会闪一个控制台黑窗（浏览器本身照常打开），消除它
        crate::subprocess::no_window(&mut c);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    match cmd.status() {
        Ok(st) if st.success() => Ok(()),
        // 无头环境等：给调用方可降级的错误
        _ => Err(Error::Usage(
            "cannot open a browser automatically; please open the link manually".into(),
        )),
    }
}

/// 发起设备授权，返回设备码信息。
///
/// # Errors
/// - [`Error::Usage`]：client_id 未配置；
/// - [`Error::Io`] 之外的网络错误统一包装为 [`Error::Usage`] 并带 GitHub 返回的说明。
pub fn start_device_flow(client_id: &str) -> Result<DeviceCode> {
    let resp = http_post_form(
        "https://github.com/login/device/code",
        &[("client_id", client_id), ("scope", SCOPE)],
    )?;
    serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the GitHub device-code response: {e}")))
}

/// 轮询直到用户完成授权，返回 access_token。
///
/// 阻塞直到：成功 / 用户拒绝 / 设备码过期 / 网络持续失败。
///
/// # Errors
/// - [`Error::Usage`]：用户拒绝授权、码过期或 GitHub 返回错误。
pub fn poll_device_token(code: &DeviceCode, client_id: &str) -> Result<String> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(code.expires_in.max(60));
    let mut interval = code.interval.max(1);
    loop {
        // 先查 deadline 再 sleep：过期立即返回，不多等一轮 interval
        if std::time::Instant::now() > deadline {
            return Err(Error::Usage(
                "device code expired; please run the command again".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let resp = http_post_form(
            "https://github.com/login/oauth/access_token",
            &[
                ("client_id", client_id),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )?;
        // 成功：{"access_token":...,"token_type":"bearer","scope":"repo"}
        if let Ok(ok) = serde_json::from_str::<TokenOk>(&resp) {
            return Ok(ok.access_token);
        }
        // 失败：{"error":"authorization_pending" | "slow_down" | "expired_token" | "access_denied"}
        match serde_json::from_str::<TokenErr>(&resp) {
            Ok(e) => match e.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval += 5;
                    continue;
                }
                "expired_token" => {
                    return Err(Error::Usage(
                        "device code expired; please run the command again".into(),
                    ))
                }
                "access_denied" => {
                    return Err(Error::Usage(
                        "authorization was denied in the browser".into(),
                    ));
                }
                other => {
                    return Err(Error::Usage(format!(
                        "GitHub authorization failed: {other}"
                    )))
                }
            },
            Err(_) => {
                return Err(Error::Usage(
                    "unrecognized GitHub authorization response".into(),
                ))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenOk {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct TokenErr {
    error: String,
}

/// 用 token 查当前用户 login 名。
///
/// # Errors
/// - [`Error::Usage`]：token 无效或 API 失败（附响应摘要）。
pub fn api_login(token: &str) -> Result<String> {
    let resp = api_get(token, "https://api.github.com/user")?;
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let u: User = serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the GitHub user response: {e}")))?;
    Ok(u.login)
}

/// 检测仓库存在性，不存在则创建私有仓库；返回 https 克隆地址。
///
/// # Errors
/// - [`Error::Usage`]：API 失败（无权限/重名冲突等，附响应摘要）。
pub fn api_ensure_repo(token: &str, owner: &str, name: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{owner}/{name}");
    // 200 = 已存在，直接复用（第二台设备接入、误删重建都走这里）
    if let Ok(resp) = api_get(token, &url) {
        #[derive(Deserialize)]
        struct Repo {
            clone_url: String,
        }
        let r: Repo = serde_json::from_str(&resp).map_err(|e| {
            Error::Usage(format!("cannot parse the GitHub repository response: {e}"))
        })?;
        return Ok(r.clone_url);
    }

    // 404 → 创建私有仓库
    let body = serde_json::json!({
        "name": name,
        "private": true,
        "description": "MyApiKey encrypted vault (auto-created)",
        "auto_init": false,
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .post("https://api.github.com/user/repos")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        // GitHub API 强制要求 User-Agent
        .header("User-Agent", "MyApiKey-CLI")
        .json(&body)
        .send()
        .map_err(|e| Error::Usage(format!("repository creation request failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if status != 201 {
        return Err(Error::Usage(format!(
            "failed to create the GitHub repository (HTTP {status}): {}",
            truncate_str(&text, 200)
        )));
    }
    #[derive(Deserialize)]
    struct Repo {
        clone_url: String,
    }
    let r: Repo = serde_json::from_str(&text).map_err(|e| {
        Error::Usage(format!(
            "cannot parse the repository creation response: {e}"
        ))
    })?;
    Ok(r.clone_url)
}

// ---------------------------------------------------------------------------
// 凭据库中的 GitHub token
// ---------------------------------------------------------------------------

/// 读取已存的 GitHub token（没有则 `None`）。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常（读不到条目不算异常，返回 `None`）。
pub fn keychain_token() -> Result<Option<String>> {
    let entry = match keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// 保存 GitHub token 到凭据库。
///
/// # Errors
/// - [`Error::Keyring`]：写入失败。
pub fn keychain_set_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| Error::Keyring(format!("failed to write to the credential store: {e}")))
}

/// 清除 GitHub token。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_token() -> Result<bool> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete the credential store entry: {e}"
        ))),
    }
}

/// 探测 token 当前是否有效（GET /user，只关心 HTTP 状态码）。
/// - `Some(true)`：有效；
/// - `Some(false)`：401，token 已失效（授权被撤销 / 过期）；
/// - `None`：网络等未知错误——不能断定失效，调用方应保留 token 不动作。
fn token_is_valid(token: &str) -> Option<bool> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "MyApiKey")
        .send()
        .ok()?;
    match resp.status().as_u16() {
        200 => Some(true),
        401 => Some(false),
        _ => None,
    }
}

/// 取"确认仍有效"的 GitHub token。
///
/// 与 [`keychain_token`] 的差别：读出后先验证。已存的 token 失效（401）时
/// **顺手清除条目**并返回 `None`，让调用方回落到重新设备授权——否则每次
/// 都带着坏 token 撞 401，且 git 对私有仓库只报语焉不详的
/// "Repository not found"。网络异常等无法判定的情况按"有效"处理，不误删。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常。
pub fn valid_keychain_token() -> Result<Option<String>> {
    let Some(token) = keychain_token()? else {
        return Ok(None);
    };
    match token_is_valid(&token) {
        Some(false) => {
            let _ = keychain_delete_token();
            Ok(None)
        }
        // 有效，或网络原因无法判定：交给调用方使用
        _ => Ok(Some(token)),
    }
}

// ---------------------------------------------------------------------------
// git 认证注入（详见 store::GitAuth；本模块只负责描述“给 git 用什么”）
// ---------------------------------------------------------------------------

/// 把 GitHub token 注入 git HTTP 认证的载体。
///
/// 实现：`http.https://github.com/.extraheader = "AUTHORIZATION: basic <b64>"`
/// 经 **环境变量**（`GIT_CONFIG_*`）传给 git——
/// 不出现在命令行参数（防进程列表泄露）、不写入 `.git/config`（防落盘）、
/// 不出现在 remote URL（防错误信息泄露）。
#[derive(Clone)]
pub struct GitAuth {
    /// 配置键。
    key: String,
    /// 配置值（含凭据）。手写 Debug 防止 base64 凭据意外进日志。
    value: String,
}

impl std::fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // value 含可解码的凭据，任何日志输出一律遮蔽
        f.debug_struct("GitAuth")
            .field("key", &self.key)
            .field("value", &"[已遮蔽]")
            .finish()
    }
}

impl GitAuth {
    /// 用 GitHub token 构造。`x-access-token` 是 GitHub 约定的
    /// “token 当密码”场景下的规范用户名。
    pub fn github_token(token: &str) -> Self {
        Self {
            key: "http.https://github.com/.extraheader".to_string(),
            value: format!(
                "AUTHORIZATION: basic {}",
                base64_encode(format!("x-access-token:{token}").as_bytes())
            ),
        }
    }

    /// 输出为要注入的环境变量集合。
    pub fn git_envs(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("GIT_CONFIG_COUNT".to_string(), "1".to_string());
        m.insert("GIT_CONFIG_KEY_0".to_string(), self.key.clone());
        m.insert("GIT_CONFIG_VALUE_0".to_string(), self.value.clone());
        m
    }
}

// ---------------------------------------------------------------------------
// 内部小工具
// ---------------------------------------------------------------------------

/// OAuth 端点的表单 POST（GitHub 要求 Accept: application/json 才返回 JSON）。
fn http_post_form(url: &str, form: &[(&str, &str)]) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .post(url)
        .header("Accept", "application/json")
        .form(form)
        .send()
        .map_err(|e| Error::Usage(format!("request to {url} failed: {e}")))?;
    Ok(resp.text().unwrap_or_default())
}

/// 带 token 的 API GET，非 2xx 返回 Err（错误体带摘要）。
fn api_get(token: &str, url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "MyApiKey-CLI")
        .send()
        .map_err(|e| Error::Usage(format!("request to {url} failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(Error::Usage(format!(
            "GitHub API returned HTTP {status}: {}",
            truncate_str(&text, 200)
        )));
    }
    Ok(text)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 标准 base64 编码（含填充，RFC 4648）。
///
/// 自实现而非引入 base64 crate：全项目只为这一处 26 行逻辑，
/// 不值得新增一个依赖（AGENTS.md：新依赖需征得同意）。
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 源码仓库的 Releases API（版本检查用，公开端点无需认证）。
pub const SOURCE_RELEASES_API: &str =
    "https://api.github.com/repos/Haaaatcher/MyApiKey/releases/latest";

/// 查询源码仓库的最新 Release tag（如 `v0.2.0`）。
///
/// 匿名调用 GitHub API 的限额是 60 次/小时/IP——个人工具的检查频率
/// 远够；若将来提示频繁失败，考虑带 token 或降低检查频率。
///
/// 返回 `Ok(None)` = 还没有发布过任何 Release（404）。CLI 的 `mak version`
/// 用它提示新版本；查询失败由调用方降级为"无法检查"，不阻塞命令本身。
///
/// # Errors
/// 网络 / 超时（15 秒）/ 响应解析失败。
pub fn latest_release_tag() -> Result<Option<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .get(SOURCE_RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "MyApiKey-CLI")
        .send()
        .map_err(|e| Error::Usage(format!("release check request failed: {e}")))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    let text = resp
        .error_for_status()
        .map_err(|e| Error::Usage(e.to_string()))?
        .text()
        .map_err(|e| Error::Usage(format!("failed to read the release info: {e}")))?;
    let tag = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|e| Error::Usage(format!("failed to parse the release info: {e}")))?
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(tag)
}

#[cfg(test)]
mod tests {
    //! 纯函数测试：base64 向量（RFC 4648）、GitAuth 环境形态、设备码响应解析。
    //! 真实 OAuth 流程无法在单测中覆盖，由手动冒烟验证。

    use super::*;

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn git_auth_env_shape_and_no_leak_in_debug() {
        let auth = GitAuth::github_token("tok123");
        let envs = auth.git_envs();
        assert_eq!(envs.get("GIT_CONFIG_COUNT").map(String::as_str), Some("1"));
        assert_eq!(
            envs.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("http.https://github.com/.extraheader")
        );
        let v = envs.get("GIT_CONFIG_VALUE_0").unwrap();
        assert!(v.starts_with("AUTHORIZATION: basic "), "值必须是 Basic 头");
        // Debug 输出（可能进日志）只含字段名，不含 value 内容本身以外的结构；
        // value 字段名出现在 Debug 里无法避免，但断言值没有以明文 token 形式裸奔：
        assert!(
            !format!("{auth:?}").contains("tok123"),
            "Debug 输出不应包含明文 token"
        );
    }

    #[test]
    fn device_code_response_parses() {
        let raw = r#"{
            "device_code": "dc_abc",
            "user_code": "AB12-CD34",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let dc: DeviceCode = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(dc.user_code, "AB12-CD34");
        assert_eq!(dc.interval, 5);
    }

    #[test]
    fn token_error_response_parses() {
        let raw = r#"{"error":"authorization_pending"}"#;
        let e: TokenErr = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(e.error, "authorization_pending");
    }
}
