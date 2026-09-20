//! 把插件资源的额度指标聚合成一行中文摘要,追加到 Cursor 模型 hover 备注末尾。
use std::collections::HashMap;

use super::descriptor::{LocalizedText, PluginResourceView};

/// 额度行前缀。
const LINE_PREFIX: &str = "额度：";

/// 取 LocalizedText 的中文文案;缺中文时回退英文,再退到任意可用文案。
fn zh_text(value: &LocalizedText) -> String {
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
    count: usize,
}

impl MetricBucket {
    /// 均值片段:`标签 82%`;非百分比单位保留单位,空单位只给数值。
    fn segment(&self) -> String {
        let average = self.total / self.count as f64;
        let value = if self.unit == "percent" {
            format!("{average:.0}%")
        } else if self.unit.is_empty() {
            format!("{average:.0}")
        } else {
            format!("{average:.0} {}", self.unit)
        };
        if self.label.is_empty() {
            value
        } else {
            format!("{} {value}", self.label)
        }
    }
}

/// 把同一资源类型下各账号的展示指标聚合成一行:同名指标跨账号取平均,
/// 顺序保持首次出现次序;没有任何指标时返回 None(不追加额度行)。
pub fn quota_line(accounts: &[PluginResourceView]) -> Option<String> {
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
                    count: 0,
                }
            });
            bucket.total += metric.value;
            bucket.count += 1;
        }
    }
    if order.is_empty() {
        return None;
    }
    let line = order
        .iter()
        .map(|id| buckets[id].segment())
        .collect::<Vec<_>>()
        .join(" · ");
    Some(format!("{LINE_PREFIX}{line}"))
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
            description: serde_json::Value::Null,
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
            reset_at_ms: None,
        }
    }

    #[test]
    fn averages_same_metric_across_accounts() {
        let line = quota_line(&[
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
        assert_eq!(line, "额度：周额度 70% · 5 小时窗口 95%");
    }

    #[test]
    fn metrics_present_in_only_one_account_still_show() {
        let line = quota_line(&[
            account(vec![metric("weekly", "周额度", 82.4)]),
            account(Vec::new()),
        ])
        .unwrap();
        assert_eq!(line, "额度：周额度 82%");
    }

    #[test]
    fn missing_metrics_produce_no_line() {
        assert_eq!(quota_line(&[account(Vec::new())]), None);
        assert_eq!(quota_line(&[]), None);
    }

    #[test]
    fn falls_back_to_plain_string_labels() {
        let mut plain = metric("weekly", "周额度", 50.0);
        plain.label = serde_json::Value::String("Weekly".into());
        let line = quota_line(&[account(vec![plain])]).unwrap();
        assert_eq!(line, "额度：Weekly 50%");
    }
}
