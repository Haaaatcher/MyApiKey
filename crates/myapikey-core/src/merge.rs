//! 多设备记录合并：last-write-wins（LWW）+ 删除墓碑。
//!
//! ## 语义（docs/design.md §5.3）
//!
//! 合并以**记录**为单位，按 `id` 配对：
//! - 只有一边有 → 保留；
//! - 两边都有 → `updated_at` 新者胜；
//! - `updated_at` 完全相等（罕见，两设备同一毫秒各改一次）→ 存活副本胜
//!   （宁可保留数据也不误删，删除需要“删除动作在时间上更晚”这一明确信号）。
//!
//! 墓碑与存活副本的冲突同样按上述时间规则裁决：
//! `mark_deleted` 会同时刷新 `updated_at`，所以“删除”天然是一个带时间戳的写操作——
//! 删除晚于对方最后修改 → 墓碑胜（已删数据不复活）；
//! 对方在删除之后又修改过 → 存活副本胜（用户的最新意志是保留）。
//!
//! ## 墓碑清理
//!
//! `deleted_at` 距 `now` 超过保留期（默认 30 天，见 [`crate::config::AppConfig`]）
//! 的墓碑在合并时被丢弃。保留期的意义：给长期离线的设备留出
//! “拿到墓碑”的时间窗——清理太早，离线设备上的旧副本会在下次同步时复活。
//!
//! ## 时间可信度说明
//!
//! LWW 依赖各设备时钟大致正确（NTP）。设备时钟严重超前会造成“未来时间戳”
//! 压制后续所有修改；这是 LWW 的固有代价，设计上选择接受（记录量小、
//! 单用户多设备、时钟错乱可自查），不引入向量时钟的复杂度。

use crate::model::{Record, VaultData};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;

/// 把两台设备各自解密出的记录集合并为一份。
///
/// `now` 由调用方传入（而非内部取当前时间），便于测试构造时间场景。
/// `tombstone_days` 为墓碑保留期天数。
///
/// 返回的记录顺序稳定：按 `updated_at` 降序（最近修改在前），
/// 相同时间按 `id` 字符串序——保证任何设备上合并结果完全一致，
/// 序列化后字节相同，避免无意义的 diff。
pub fn merge_records(
    local: &[Record],
    remote: &[Record],
    now: DateTime<Utc>,
    tombstone_days: u32,
) -> Vec<Record> {
    // id → 较新副本。先放 local，再尝试用 remote 覆盖（仅在 remote 更新时）
    let mut by_id: HashMap<Uuid, Record> = HashMap::new();
    for r in local {
        by_id.insert(r.id, r.clone());
    }
    for r in remote {
        match by_id.get(&r.id) {
            Some(existing) => {
                if wins_over(r, existing) {
                    by_id.insert(r.id, r.clone());
                }
            }
            None => {
                by_id.insert(r.id, r.clone());
            }
        }
    }

    // 墓碑清理：deleted_at 早于 (now - 保留期) 的记录整体丢弃
    let cutoff = now - chrono::Duration::days(i64::from(tombstone_days));
    let mut merged: Vec<Record> = by_id
        .into_values()
        .filter(|r| match r.deleted_at {
            None => true,
            Some(d) => d > cutoff,
        })
        .collect();

    // 稳定排序：任何设备合并同一对输入得到同一输出
    merged.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.simple().to_string().cmp(&b.id.simple().to_string()))
    });
    merged
}

/// LWW 裁决：`a` 是否应胜过 `b`。
///
/// 相等时间戳时返回 `false`（即保留 `b`、丢弃 `a`）——
/// `b` 是先放入 map 的 local 副本，配合调用方的“存活优先”测试约定。
/// 注意：相等时若一方是墓碑一方存活，调用序决定胜负会不稳定，
/// 因此这里显式处理：相等时**存活副本胜**，与文档语义一致。
fn wins_over(a: &Record, b: &Record) -> bool {
    match a.updated_at.cmp(&b.updated_at) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => !a.is_deleted() && b.is_deleted(),
    }
}

/// 便捷封装：直接合并两份 VaultData。
pub fn merge_vaults(
    local: &VaultData,
    remote: &VaultData,
    now: DateTime<Utc>,
    tombstone_days: u32,
) -> VaultData {
    VaultData {
        records: merge_records(&local.records, &remote.records, now, tombstone_days),
    }
}

#[cfg(test)]
mod tests {
    //! 合并语义的全部关键场景：新旧覆盖、墓碑胜负、清理、确定性。
    //!
    //! 注意：测试中“同一条记录”的两份副本必须显式共享同一个 id
    //!（`Record::new` 每次生成新 id，不能直接当同一记录用）。

    use super::*;
    use crate::model::Protocol;

    /// 构造指定 id 的记录（同一逻辑记录在两台设备上的副本请传同一个 id）。
    fn rec(id: Uuid, name: &str, updated: &str) -> Record {
        let mut r = Record::new(
            name.into(),
            Protocol::OpenAi,
            "https://x.example".into(),
            "sk-x".into(),
        );
        r.id = id;
        r.updated_at = DateTime::parse_from_rfc3339(updated)
            .expect("测试时间非法")
            .with_timezone(&Utc);
        r
    }

    /// 在 `at` 时刻删除（同时刷新 updated_at，模拟 mark_deleted 的真实行为）。
    fn deleted(mut r: Record, at: &str) -> Record {
        r.updated_at = DateTime::parse_from_rfc3339(at)
            .expect("测试时间非法")
            .with_timezone(&Utc);
        r.deleted_at = Some(r.updated_at);
        r
    }

    fn nid() -> Uuid {
        Uuid::now_v7()
    }

    const NOW: &str = "2026-10-23T12:00:00Z";

    #[test]
    fn newer_edit_wins() {
        let id = nid();
        let local = vec![rec(id, "a", "2026-10-23T10:00:00Z")];
        let remote = vec![rec(id, "a", "2026-10-23T11:00:00Z")];
        let out = merge_records(&local, &remote, now(), 30);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].updated_at.to_rfc3339(), "2026-10-23T11:00:00+00:00");
    }

    #[test]
    fn disjoint_records_union() {
        let local = vec![rec(nid(), "a", "2026-10-01T00:00:00Z")];
        let remote = vec![rec(nid(), "b", "2026-10-02T00:00:00Z")];
        let out = merge_records(&local, &remote, now(), 30);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn newer_tombstone_beats_older_alive() {
        // 设备 A 在 11:00 删除；设备 B 手里是 10:00 的旧副本 → 删除生效
        let id = nid();
        let local = vec![deleted(
            rec(id, "a", "2026-10-01T00:00:00Z"),
            "2026-10-23T11:00:00Z",
        )];
        let remote = vec![rec(id, "a", "2026-10-23T10:00:00Z")];
        let out = merge_records(&local, &remote, now(), 30);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_deleted(), "更晚的墓碑必须胜过更早的存活副本");
    }

    #[test]
    fn edit_after_deletion_resurrects() {
        // 设备 A 在 10:00 删除；设备 B 在 11:00 又改了这条 → 保留（用户最新意志）
        let id = nid();
        let local = vec![deleted(
            rec(id, "a", "2026-10-01T00:00:00Z"),
            "2026-10-23T10:00:00Z",
        )];
        let remote = vec![rec(id, "a", "2026-10-23T11:00:00Z")];
        let out = merge_records(&local, &remote, now(), 30);
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_deleted(), "删除之后的再编辑应胜过墓碑");
    }

    #[test]
    fn stale_tombstone_is_purged_after_grace() {
        // 删除发生在 40 天前，保留期 30 天 → 墓碑被清理，记录彻底消失
        let id = nid();
        let local = vec![deleted(
            rec(id, "a", "2026-09-01T00:00:00Z"),
            "2026-09-13T00:00:00Z",
        )];
        let out = merge_records(&local, &[], now(), 30);
        assert!(out.is_empty(), "过保留期的墓碑应被清理");
    }

    #[test]
    fn fresh_tombstone_survives_grace() {
        // 删除发生在 10 天前 → 墓碑保留
        let id = nid();
        let local = vec![deleted(
            rec(id, "a", "2026-09-01T00:00:00Z"),
            "2026-10-13T00:00:00Z",
        )];
        let out = merge_records(&local, &[], now(), 30);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn equal_timestamp_alive_wins() {
        let id = nid();
        let d = "2026-10-23T10:00:00Z";
        let tomb = deleted(rec(id, "a", "2026-10-01T00:00:00Z"), d);
        let alive = rec(id, "a", d);
        // 两种调用序都应得到存活副本——确定性
        let out1 = merge_records(
            std::slice::from_ref(&tomb),
            std::slice::from_ref(&alive),
            now(),
            30,
        );
        let out2 = merge_records(
            std::slice::from_ref(&alive),
            std::slice::from_ref(&tomb),
            now(),
            30,
        );
        assert_eq!(out1.len(), 1);
        assert!(!out1[0].is_deleted());
        assert_eq!(out2.len(), 1);
        assert!(!out2[0].is_deleted());
    }

    #[test]
    fn merge_is_symmetric_byte_stable() {
        let (ia, ib, ic, idd) = (nid(), nid(), nid(), nid());
        let local = vec![
            rec(ia, "a", "2026-10-20T00:00:00Z"),
            deleted(rec(ib, "b", "2026-10-01T00:00:00Z"), "2026-10-22T00:00:00Z"),
            rec(ic, "c", "2026-10-23T08:00:00Z"),
        ];
        let remote = vec![
            rec(ia, "a", "2026-10-21T00:00:00Z"),
            rec(ib, "b", "2026-10-19T00:00:00Z"),
            rec(idd, "d", "2026-10-18T00:00:00Z"),
        ];
        let o1 = merge_records(&local, &remote, now(), 30);
        let o2 = merge_records(&remote, &local, now(), 30);
        // 两台设备各自合并必须得到相同结果（排序保证字节稳定）
        let j1 = serde_json::to_string(&o1).expect("序列化失败");
        let j2 = serde_json::to_string(&o2).expect("序列化失败");
        assert_eq!(j1, j2, "合并结果必须是交换的且顺序确定");
        // 内容抽查：a 取 remote 新值、b 为墓碑、c/d 并存 → 共 4 条
        assert_eq!(o1.len(), 4);
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(NOW)
            .expect("固定时间非法")
            .with_timezone(&Utc)
    }
}
