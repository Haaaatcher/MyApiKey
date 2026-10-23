//! 数据模型：一条凭证记录与协议标签。
//!
//! 核心设计决策（ADR D1，详见 docs/design.md）：
//! - 记录就是 `名称 + 协议标签 + base_url + api_key`（外加可选的附加头与备注）；
//! - **协议只是用户手填的标签**，仅用于展示与 `probe` 时决定请求的形状，
//!   全库不做任何基于 key 前缀或 URL 形态的自动推断——三方平台不守约定，推断必然出错；
//! - `base_url` 原样保存、原样使用，不做归一化；
//! - 时间戳用 [`chrono::DateTime<Utc>`] 序列化为 RFC 3339，多设备时钟差异
//!   由 LWW 语义天然容忍（见 [`crate::merge`]）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// 凭证所属的协议标签。
///
/// `Custom(String)` 允许用户填写任意自定义值（如 "azure"、某个网关的专有协议名）。
/// 自定义值不参与 key 校验探测（[`crate::probe`] 对其直接跳过），
/// 它存在的意义只是让用户在列表里一眼分清记录的类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Protocol {
    /// OpenAI 风格协议（`Authorization: Bearer`，`{base}/chat/completions`）。
    OpenAi,
    /// Anthropic 风格协议（`x-api-key` + `anthropic-version`，`{base}/v1/messages`）。
    Anthropic,
    /// 用户自定义标签，仅用于展示。
    /// 结构体变体（而非 `Custom(String)`）是因为 serde 的内部标签
    /// 不支持包装裸字符串的新类型变体。
    Custom {
        /// 自定义标签文本。
        name: String,
    },
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::OpenAi => write!(f, "openai"),
            Protocol::Anthropic => write!(f, "anthropic"),
            // 展示时明确标注来源是用户自定义，避免与内置标签混淆
            Protocol::Custom { name } => write!(f, "custom:{name}"),
        }
    }
}

/// 一条凭证记录：一个 `base_url + api_key` 对及其元数据。
///
/// `api_key` 是敏感字段。安全约束见 crate 顶部文档：
/// 只能进内存、vault 密文与用户显式要求的输出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// 记录的唯一标识（UUID v7，时间有序，跨设备生成也不会冲突）。
    /// 一旦创建永不改变，删除后重建会得到新的 id。
    pub id: Uuid,

    /// 用户可读的名字，如 “OpenAI 主力号”。不要求唯一（查找时按精确匹配取第一条）。
    pub name: String,

    /// 协议标签（用户手填，不推断）。
    pub protocol: Protocol,

    /// 平台提供的 base URL，原样保存不做归一化。
    pub base_url: String,

    /// API key 明文。仅存在于内存与解密后的 vault 中。
    pub api_key: String,

    /// 附加请求头（可选），例如 Anthropic 的 `anthropic-workspace-id`、
    /// OpenAI 的 `OpenAI-Project`。键排序存储（BTreeMap）保证序列化稳定，
    /// 同一份数据在任何设备上加密前的明文都一致，便于测试与调试。
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,

    /// 自由备注（可选）。
    #[serde(default)]
    pub notes: Option<String>,

    /// 创建时间（RFC 3339）。仅作展示，不参与合并裁决。
    pub created_at: DateTime<Utc>,

    /// 最后修改时间（RFC 3339）。**同步合并的唯一裁决依据**（LWW），
    /// 任何字段变更（包括删除）都会刷新它。
    pub updated_at: DateTime<Utc>,

    /// 删除墓碑：`Some(时间)` 表示该记录已在某设备上被删除。
    /// 多设备同步时墓碑随数据一起传播，防止“已删记录从旧设备复活”；
    /// 超过保留期（默认 30 天，见 [`crate::config`]）后由
    /// [`crate::merge::merge_records`] 统一清理。
    #[serde(default)]
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Record {
    /// 创建一条新记录：生成 UUID v7、时间戳取当前时刻。
    ///
    /// 调用方只需填业务字段；`extra_headers` 为空、`notes` 为 `None`、
    /// `deleted_at` 为 `None` 都由本函数给出正确初值。
    pub fn new(name: String, protocol: Protocol, base_url: String, api_key: String) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7(),
            name,
            protocol,
            base_url,
            api_key,
            extra_headers: BTreeMap::new(),
            notes: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    /// 记录当前是否处于“已被删除”状态（墓碑）。
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// 把记录标记为删除（打墓碑）：同时刷新 `deleted_at` 与 `updated_at`。
    ///
    /// 两个时间都刷新的原因：合并裁决只看 `updated_at`，删除若不刷新它，
    /// 旧设备上一份更“新”的存活副本会赢过墓碑，导致已删数据复活——
    /// 这正是墓碑机制要防的场景。
    pub fn mark_deleted(&mut self) {
        let now = Utc::now();
        self.deleted_at = Some(now);
        self.updated_at = now;
    }

    /// 对 api_key 做掩码显示：保留前 4 后 4 个字符，中间用 `...` 代替。
    ///
    /// 用于 `list` 等不展示完整 key 的场合。key 过短（≤ 8 字符）时只显示
    /// 前 2 个字符加 `...`，保证短 key 也不被完整泄露。
    pub fn masked_key(&self) -> String {
        // 用字符（而非字节）切片：key 理论上可能是任意 Unicode 字符串，
        // 字节切片在多字节边界上会 panic
        let chars: Vec<char> = self.api_key.chars().collect();
        if chars.len() <= 8 {
            let head: String = chars.iter().take(2).collect();
            format!("{head}...")
        } else {
            let head: String = chars.iter().take(4).collect();
            let tail: String = chars.iter().skip(chars.len() - 4).collect();
            format!("{head}...{tail}")
        }
    }
}

/// vault 解密后的内存形态：全部记录（含墓碑）。
///
/// 序列化为 vault 明文时的 JSON 形状是 `{"records":[...]}`——
/// 外面包一层对象而不是直接用数组，是为了给未来的格式演进
/// （如加入 per-vault 元数据）留出向后兼容的余地。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultData {
    /// 全部记录，包括已删除但未过保留期的墓碑记录。
    pub records: Vec<Record>,
}

impl VaultData {
    /// 构造空 vault（`init` 时使用）。
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// 只返回存活（未删除）记录的引用，`list` 用。
    pub fn alive_records(&self) -> impl Iterator<Item = &Record> {
        self.records.iter().filter(|r| !r.is_deleted())
    }

    /// 按 id 精确查找，或按名称在**存活记录**中精确查找（不区分大小写）。
    ///
    /// 名称匹配只看存活记录：已被删除的记录不应再被 `show`/`edit` 命中。
    /// 返回其在 `records` 中的下标，方便调用方原地修改。
    pub fn find(&self, id_or_name: &str) -> Option<usize> {
        // 先按 UUID 精确匹配（含墓碑——删除确认等场景需要看到它）
        if let Ok(id) = Uuid::parse_str(id_or_name) {
            if let Some(idx) = self.records.iter().position(|r| r.id == id) {
                return Some(idx);
            }
        }
        // 再按 UUID 前缀匹配（方便只敲前几位 id）
        if id_or_name.len() >= 4 {
            let lower = id_or_name.to_ascii_lowercase();
            if let Some(idx) = self
                .records
                .iter()
                .position(|r| r.id.simple().to_string().starts_with(&lower))
            {
                return Some(idx);
            }
        }
        // 最后按名称（大小写不敏感，仅存活记录）
        self.records
            .iter()
            .position(|r| !r.is_deleted() && r.name.eq_ignore_ascii_case(id_or_name))
    }
}

#[cfg(test)]
mod tests {
    //! 模型层测试：掩码不泄密、查找语义、墓碑时间刷新。

    use super::*;

    fn sample() -> Record {
        Record::new(
            "测试记录".into(),
            Protocol::OpenAi,
            "https://api.example.com/v1".into(),
            "sk-proj-abcdefghijklmnop1234".into(),
        )
    }

    #[test]
    fn masked_key_never_reveals_middle() {
        let r = sample();
        let m = r.masked_key();
        assert!(m.starts_with("sk-p"));
        assert!(m.ends_with("1234"));
        assert!(!m.contains("abcdefghij"));

        // 短 key 分支：只泄露前 2 个字符
        let mut short = sample();
        short.api_key = "abcd".into();
        assert_eq!(short.masked_key(), "ab...");

        // 多字节字符不应 panic，且同样掩码
        let mut uni = sample();
        uni.api_key = "密钥密钥密钥密钥".into();
        assert!(uni.masked_key().ends_with("..."));
    }

    #[test]
    fn mark_deleted_refreshes_both_timestamps() {
        let mut r = sample();
        let before = r.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.mark_deleted();
        assert!(r.deleted_at.is_some());
        assert!(
            r.updated_at > before,
            "删除必须刷新 updated_at，否则墓碑会在合并中输给旧副本"
        );
    }

    #[test]
    fn find_by_name_skips_deleted() {
        let mut data = VaultData::empty();
        let mut r = sample();
        r.name = "主力".into();
        r.mark_deleted();
        data.records.push(r);

        assert!(data.find("主力").is_none(), "已删除记录不应被名称命中");
        let idx = data.find(&data.records[0].id.to_string());
        assert!(
            idx.is_some(),
            "按完整 UUID 仍应命中墓碑（供删除确认等场景）"
        );
    }

    #[test]
    fn serde_roundtrip_preserves_all_fields() {
        let mut r = sample();
        r.extra_headers
            .insert("anthropic-workspace-id".into(), "wrkspc_abc".into());
        r.notes = Some("备注".into());
        r.protocol = Protocol::Custom {
            name: "azure".into(),
        };

        let json = serde_json::to_string(&r).expect("序列化失败");
        let back: Record = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(back.id, r.id);
        assert_eq!(back.protocol, r.protocol);
        assert_eq!(back.extra_headers, r.extra_headers);
        assert_eq!(back.notes, r.notes);
    }
}
