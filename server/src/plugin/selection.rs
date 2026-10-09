//! 候选账号的选择规则:过滤、排序、亲和与故障转移判定的纯函数。
//! 不触碰插件运行时与持久化状态,便于直接单测;所有权与执行仍归 registry。
use super::state::ResourceRecord;

#[cfg(test)]
use super::state::ResourceState;

/// 只保留当前可用的候选:Ready,或冷却已到期。
pub(super) fn ready(records: &[ResourceRecord], now_ms: i64) -> Vec<ResourceRecord> {
    records
        .iter()
        .filter(|record| record.state.is_ready(now_ms))
        .cloned()
        .collect()
}

/// 套餐优先级:pro/ultra/premium/advanced 类标签排前,其余排后。
fn plan_priority(private_data: &serde_json::Value) -> u8 {
    let label = private_data
        .get("quota")
        .and_then(|q| q.get("planLabel"))
        .and_then(|l| l.as_str())
        .unwrap_or("");
    let lower = label.to_lowercase();
    if lower.contains("pro")
        || lower.contains("ultra")
        || lower.contains("premium")
        || lower.contains("advanced")
    {
        0
    } else {
        1
    }
}

/// 套餐优先,ID 作为次序的决胜键,保证候选集不变时顺序稳定。
pub(super) fn sort_stable(records: &mut [ResourceRecord]) {
    records.sort_by(|a, b| {
        plan_priority(&a.private_data)
            .cmp(&plan_priority(&b.private_data))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// 过滤就绪候选并按套餐优先稳定排序;顺序即故障转移的尝试顺序。
pub(super) fn order_candidates(active: &[ResourceRecord], now_ms: i64) -> Vec<ResourceRecord> {
    let mut candidates = ready(active, now_ms);
    sort_stable(&mut candidates);
    candidates
}

/// 会话亲和的首选下标:同 key 在候选数量不变时恒定。
pub(super) fn affinity_index(key: &str, len: usize) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % len
}

/// Applies the preferred account or round-robin position without changing
/// the remaining candidates' relative order.
pub(super) fn rotate_candidates(
    records: &mut [ResourceRecord],
    affinity: Option<&str>,
    turn: usize,
) {
    if records.is_empty() {
        return;
    }
    let start = affinity.map_or(turn % records.len(), |key| {
        affinity_index(key, records.len())
    });
    records.rotate_left(start);
}

/// 故障转移判定:仅在尚未发出任何事件且还有下一候选时允许切换,
/// 已输出后禁止重放。
pub(super) fn can_failover(emitted: bool, attempt: usize, attempts: usize) -> bool {
    !emitted && attempt + 1 < attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, state: ResourceState, private_data: serde_json::Value) -> ResourceRecord {
        ResourceRecord {
            id: id.to_owned(),
            key: id.to_owned(),
            private_data,
            state,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn pro(id: &str) -> ResourceRecord {
        record(
            id,
            ResourceState::Ready,
            serde_json::json!({"quota": {"planLabel": "Pro"}}),
        )
    }

    fn free(id: &str) -> ResourceRecord {
        record(id, ResourceState::Ready, serde_json::json!({}))
    }

    #[test]
    fn ready_keeps_ready_and_expired_cooling_but_not_future_cooling_or_invalid() {
        let records = vec![
            pro("a"),
            record(
                "b",
                ResourceState::Cooling {
                    retry_at_ms: Some(500),
                    message: None,
                },
                serde_json::json!({}),
            ),
            record(
                "c",
                ResourceState::Cooling {
                    retry_at_ms: Some(1_500),
                    message: None,
                },
                serde_json::json!({}),
            ),
            record(
                "d",
                ResourceState::Invalid {
                    message: Some("revoked".into()),
                },
                serde_json::json!({}),
            ),
        ];
        let ready = ready(&records, 1_000);
        let ids: Vec<_> = ready.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn sort_stable_orders_by_plan_then_id() {
        let mut records = vec![free("z"), pro("m"), free("a"), pro("b")];
        sort_stable(&mut records);
        let ids: Vec<_> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "m", "a", "z"]);
    }

    #[test]
    fn order_candidates_filters_then_sorts() {
        let records = vec![
            free("z"),
            record(
                "x",
                ResourceState::Cooling {
                    retry_at_ms: Some(2_000),
                    message: None,
                },
                serde_json::json!({}),
            ),
            pro("m"),
        ];
        let ordered = order_candidates(&records, 1_000);
        let ids: Vec<_> = ordered.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["m", "z"]);
    }

    #[test]
    fn affinity_index_is_stable_and_in_range() {
        let first = affinity_index("session-1", 4);
        for _ in 0..8 {
            assert_eq!(affinity_index("session-1", 4), first);
        }
        for key in ["s1", "s2", "s3", "another-session", "第四个会话"] {
            assert!(affinity_index(key, 4) < 4);
        }
    }

    #[test]
    fn failover_only_before_any_output_and_with_remaining_candidates() {
        assert!(can_failover(false, 0, 3));
        assert!(can_failover(false, 1, 3));
        // 已输出后禁止重放。
        assert!(!can_failover(true, 0, 3));
        // 最后一个候选失败即终止,没有下一候选。
        assert!(!can_failover(false, 2, 3));
        assert!(!can_failover(false, 0, 1));
    }
}
