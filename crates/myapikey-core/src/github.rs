//! GitHub 集成：Device Flow 授权（RFC 8628）、token 存取与刷新、
//! 私有同步仓库的创建/识别，以及 Contents API 的 base64 编解码。
//!
//! D25（M4）起同步不再走 git——认证以 Bearer header 由 [`crate::remote`]
//! 使用；本模块不再提供 git 凭据注入（原 GitAuth/git_envs 已随系统 git
//! 依赖一并移除）。

use crate::error::{Error, Result};
use serde::Deserialize;

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
/// 刷新令牌的凭据条目账号（与 access token 分开存放；轮换后整体覆盖）。
const KEYCHAIN_REFRESH_ACCOUNT: &str = "github-refresh";

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

/// 设备流授权结果：access token + 可选刷新令牌（OAuth App 开启
/// "过期 token" 选项时返回；access 约 8 小时过期，refresh 约 6 个月）。
#[derive(Debug, Clone)]
pub struct AuthTokens {
    /// 访问令牌（写入 keychain `github` 条目，git 注入用）。
    pub access_token: String,
    /// 刷新令牌（写入 keychain `github-refresh` 条目，失效时自动换新）。
    pub refresh_token: Option<String>,
    /// 颁发的 scope（GitHub 回传；`(none)` 表示零权限令牌——调用方
    /// 应自行展示，core 不做任何 stdout 输出）。
    pub scope: Option<String>,
}

/// 轮询直到用户完成授权，返回 access token 与可选 refresh token。
///
/// 阻塞直到：成功 / 用户拒绝 / 设备码过期 / 网络持续失败。
///
/// # Errors
/// - [`Error::Usage`]：用户拒绝授权、码过期或 GitHub 返回错误。
pub fn poll_device_token(code: &DeviceCode, client_id: &str) -> Result<AuthTokens> {
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
                // **必须重复声明 scope**：GitHub 不会从发码请求继承——
                // 缺失时颁发的令牌 scope 为空（能识别身份却读不了私有
                // 仓库，"授权成功但同步 403" 的根因，2026-10-30 修复）
                ("scope", SCOPE),
            ],
        )?;
        // 成功：{"access_token":...,"token_type":"bearer","scope":"repo"
        //        [,"refresh_token":...,"expires_in":...]}（过期模式）
        if let Ok(ok) = serde_json::from_str::<TokenOk>(&resp) {
            if let Some(rt) = &ok.refresh_token {
                // 刷新令牌当场入 keychain：8 小时后 access 过期时自动续期全靠它
                let _ = keychain_set_refresh_token(rt);
            }
            return Ok(AuthTokens {
                access_token: ok.access_token,
                refresh_token: ok.refresh_token,
                scope: ok.scope,
            });
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

/// token 端点响应。OAuth App 开启"过期 token"选项后，响应会携带
/// `refresh_token` 与 `expires_in`（未开启则缺省——两种形状都必须能解析）。
#[derive(Debug, Deserialize)]
struct TokenOk {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 捕获以保留语义完整；主动续期（临近过期刷）留待后续
    pub expires_in: Option<u64>,
    /// 令牌实际获得的权限（如 "repo"）——空串/缺失 = 零权限令牌。
    #[serde(default)]
    pub scope: Option<String>,
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

/// 清除 GitHub token（access 与 refresh 两个条目一并删除）。
///
/// 只删 access 会留下 refresh 残留——自动续期（`try_refresh_and_store`）
/// 可拿它换回有效令牌，"清除授权/解绑"对用户承诺的干净状态就被悄悄
/// 破坏（2026-10-31 修复）。
///
/// 返回是否清到了任一条目。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_token() -> Result<bool> {
    let mut deleted = false;
    for account in [KEYCHAIN_ACCOUNT, KEYCHAIN_REFRESH_ACCOUNT] {
        let entry = keyring::Entry::new(crate::keychain::SERVICE, account).map_err(|e| {
            Error::Keyring(format!("failed to open the credential store entry: {e}"))
        })?;
        match entry.delete_credential() {
            Ok(()) => deleted = true,
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                return Err(Error::Keyring(format!(
                    "failed to delete the credential store entry: {e}"
                )))
            }
        }
    }
    Ok(deleted)
}

/// 读取已存的刷新令牌（没有则 `None`）。
pub fn keychain_refresh_token() -> Result<Option<String>> {
    let entry = match keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// 保存刷新令牌。
///
/// # Errors
/// - [`Error::Keyring`]：写入失败。
pub fn keychain_set_refresh_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| Error::Keyring(format!("failed to write to the credential store: {e}")))
}

/// 清除刷新令牌（forget 时与 access token 一起清）。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_refresh_token() -> Result<bool> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete the credential store entry: {e}"
        ))),
    }
}

/// 用刷新令牌换新的 access token（GitHub 会**轮换** refresh token——
/// 响应携带新的 refresh token，必须一并覆盖存储，旧刷新令牌作废）。
///
/// # Errors
/// - 无刷新令牌 / 刷新失败（`invalid_grant` 等，通常意味着必须重走
///   设备授权流）。
pub fn refresh_access_token(client_id: &str) -> Result<AuthTokens> {
    let Some(rt) = keychain_refresh_token()? else {
        return Err(Error::Usage(
            "no refresh token stored; run `mak init --github` to authorize".into(),
        ));
    };
    let resp = http_post_form(
        "https://github.com/login/oauth/access_token",
        &[
            ("client_id", client_id),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:refresh_token",
            ),
            ("refresh_token", rt.as_str()),
        ],
    )?;
    let ok: TokenOk = serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the GitHub refresh response: {e}")))?;
    if let Some(new_rt) = &ok.refresh_token {
        keychain_set_refresh_token(new_rt)?;
    }
    Ok(AuthTokens {
        access_token: ok.access_token,
        refresh_token: ok.refresh_token,
        scope: ok.scope,
    })
}

/// 自动续期：keychain 有刷新令牌时尝试刷新并写回 keychain（access +
/// 轮换后的 refresh）。返回是否成功续期——false = 无刷新令牌或刷新
/// 失败（调用方维持原错误与重授权指引）。
pub fn try_refresh_and_store(client_id: &str) -> bool {
    refresh_access_token(client_id).is_ok()
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

/// 标准 base64 解码（含填充；容忍换行折行，GitHub API 返回的
/// content 字段按 60 字符折行）。非法字符/长度报错。
///
/// 自实现理由同 [`base64_encode`]：不引入 base64 crate。
///
/// # Errors
/// 非法 base64（非表字符、长度不对齐、padding 位置错误）。
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            return Err(Error::Usage("invalid base64: dangling single char".into()));
        }
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = val(c).ok_or_else(|| {
                Error::Usage(format!("invalid base64 character: {:?}", c as char))
            })?;
            n |= u32::from(v) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
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
    //! 纯函数测试：base64 向量（RFC 4648）、设备码响应解析。
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
