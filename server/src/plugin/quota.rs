//! 把插件资源的额度指标聚合成一段中文摘要,追加到 Cursor 模型 hover 备注末尾。
use std::collections::HashMap;

use super::descriptor::{LocalizedText, PluginResourceView};

/// 额度块首行。
const HEADING: &str = "额度：";
/// 每条额度窗口的行首。
const BULLET: &str = "- ";

/// 取 LocalizedText 的中文文案;缺中文时回退英文,再退到任意可用文案。
pub(crate) fn zh_text(value: &LocalizedText) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(map) => ["zh-CN", "en-US"]
            .iter()
            .find_map(|locale| map.get(*locale))
            .or_else(|| map.values().next())
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// 同名指标跨账号的聚合桶。
struct MetricBucket {
    label: String,
    unit: String,
    total: f64,
    /// count 指标的总量合计,与带了总量的账号数配对;percent 指标不使用。
    total_quota: f64,
    total_quota_count: usize,
    /// 各账号里最早的重置时间;该条目行尾据此标注。
    earliest_reset_ms: Option<i64>,
    count: usize,
}

/// 本地时区的 `YYYY-MM-DD HH:MM`,与桌面 hover 的重置时间格式一致。
fn format_reset(reset_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(reset_ms)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

impl MetricBucket {
    /// 有最早重置时间时行尾追加 `(YYYY-MM-DD HH:MM 重置)`。
    fn segment(&self) -> String {
        let average = self.total / self.count as f64;
        let value = if self.unit == "percent" {
            format!("{average:.0}%")
        } else if self.total_quota_count > 0 {
            // 总量只在带了总量的账号间取均值,缺总量的账号不稀释。
            let quota = self.total_quota / self.total_quota_count as f64;
            format!("{average:.0}/{quota:.0}")
        } else {
            format!("{average:.0}")
        };
        let value = if self.label.is_empty() {
            value
        } else {
            format!("{} {value}", self.label)
        };
        match self.earliest_reset_ms {
            Some(reset_ms) => format!("{value}（{} 重置）", format_reset(reset_ms)),
            None => value,
        }
    }
}

/// 把同一资源类型下各账号的展示指标聚合为额度块:首行 `额度：`,其下每条指标
/// 一行 `- 标签 82%`;同名指标跨账号取平均,顺序保持首次出现次序。
/// 没有任何指标时返回 None(不追加额度块)。
pub fn quota_block(accounts: &[PluginResourceView]) -> Option<String> {
    let mut order = Vec::new();
    let mut buckets = HashMap::<&str, MetricBucket>::new();
    for account in accounts {
        for metric in &account.metrics {
            let bucket = buckets.entry(metric.id.as_str()).or_insert_with(|| {
                order.push(metric.id.as_str());
                MetricBucket {
                    label: zh_text(&metric.label),
                    unit: metric.unit.clone(),
                    total: 0.0,
                    total_quota: 0.0,
                    total_quota_count: 0,
                    earliest_reset_ms: None,
                    count: 0,
                }
            });
            bucket.total += metric.value;
            if let Some(total) = metric.total {
                bucket.total_quota += total;
                bucket.total_quota_count += 1;
            }
            if let Some(reset_ms) = metric.reset_at_ms {
                bucket.earliest_reset_ms = Some(
                    bucket
                        .earliest_reset_ms
                        .map_or(reset_ms, |earliest| earliest.min(reset_ms)),
                );
            }
            bucket.count += 1;
        }
    }
    if order.is_empty() {
        return None;
    }
    let items = order
        .iter()
        .map(|id| format!("{BULLET}{}", buckets[id].segment()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{HEADING}\n{items}"))
}

#[cfg(test)]
mod tests {
    use super::{
        super::{descriptor::ResourceMetric, state::ResourceState},
        *,
    };

    fn account(metrics: Vec<ResourceMetric>) -> PluginResourceView {
        PluginResourceView {
            id: "resource".into(),
            state: ResourceState::Ready,
            display_name: "account".into(),
            tier: None,
            metrics,
            created_at_ms: 0,
        }
    }

    fn metric(id: &str, zh_label: &str, value: f64) -> ResourceMetric {
        ResourceMetric {
            id: id.into(),
            label: serde_json::json!({ "zh-CN": zh_label, "en-US": id }),
            unit: "percent".into(),
            value,
            total: None,
            reset_at_ms: None,
        }
    }

    #[test]
    fn averages_same_metric_across_accounts() {
        let block = quota_block(&[
            account(vec![
                metric("weekly", "周额度", 80.0),
                metric("five-hour", "5 小时窗口", 90.0),
            ]),
            account(vec![
                metric("weekly", "周额度", 60.0),
                metric("five-hour", "5 小时窗口", 100.0),
            ]),
        ])
        .unwrap();
        assert_eq!(block, "额度：\n- 周额度 70%\n- 5 小时窗口 95%");
    }

    #[test]
    fn metrics_present_in_only_one_account_still_show() {
        let block = quota_block(&[
            account(vec![metric("weekly", "周额度", 82.4)]),
            account(Vec::new()),
        ])
        .unwrap();
        assert_eq!(block, "额度：\n- 周额度 82%");
    }

    #[test]
    fn count_quota_averages_only_accounts_with_a_total() {
        let mut with_total = metric("reset-cards", "重置卡", 12.0);
        with_total.unit = "count".into();
        with_total.total = Some(100.0);
        let mut without_total = metric("reset-cards", "重置卡", 6.0);
        without_total.unit = "count".into();
        // 缺总量的账号不稀释总量均值:12/100 与 6 平均为 9/100,而不是 9/50。
        let block =
            quota_block(&[account(vec![with_total]), account(vec![without_total])]).unwrap();
        assert_eq!(block, "额度：\n- 重置卡 9/100");
    }

    #[test]
    fn earliest_reset_appends_a_reset_marker_to_the_item() {
        let mut single = metric("weekly", "周额度", 80.0);
        single.reset_at_ms = Some(1_792_753_794_979);
        let block = quota_block(&[account(vec![single])]).unwrap();
        assert_eq!(
            block,
            format!(
                "额度：\n- 周额度 80%（{} 重置）",
                format_reset(1_792_753_794_979)
            )
        );

        // 多账号取最早的重置时间。
        let mut later = metric("weekly", "周额度", 60.0);
        later.reset_at_ms = Some(1_800_000_000_000);
        let mut earlier = metric("weekly", "周额度", 60.0);
        earlier.reset_at_ms = Some(1_700_000_000_000);
        let block = quota_block(&[account(vec![later]), account(vec![earlier])]).unwrap();
        assert_eq!(
            block,
            format!(
                "额度：\n- 周额度 60%（{} 重置）",
                format_reset(1_700_000_000_000)
            )
        );
    }

    #[test]
    fn reset_marker_stays_on_its_own_item() {
        // 每个窗口的重置时间跟随各自条目,不提到整个额度块末尾。
        let mut five_hour = metric("five-hour", "5 小时窗口", 100.0);
        five_hour.reset_at_ms = Some(1_792_753_794_979);
        let mut weekly = metric("weekly", "周额度", 89.0);
        weekly.reset_at_ms = Some(1_793_200_000_000);
        let block = quota_block(&[account(vec![five_hour, weekly])]).unwrap();
        assert_eq!(
            block,
            format!(
                "额度：\n- 5 小时窗口 100%（{} 重置）\n- 周额度 89%（{} 重置）",
                format_reset(1_792_753_794_979),
                format_reset(1_793_200_000_000)
            )
        );
    }

    #[test]
    fn missing_metrics_produce_no_block() {
        assert_eq!(quota_block(&[account(Vec::new())]), None);
        assert_eq!(quota_block(&[]), None);
    }

    #[test]
    fn falls_back_to_plain_string_labels() {
        let mut plain = metric("weekly", "周额度", 50.0);
        plain.label = serde_json::Value::String("Weekly".into());
        let block = quota_block(&[account(vec![plain])]).unwrap();
        assert_eq!(block, "额度：\n- Weekly 50%");
    }
}
