//! Provides aggregate data for the control API.
//! Efficient database aggregates for the desktop overview.

use std::collections::BTreeMap;

use chrono::Utc;
use sqlx::Row;

use crate::{
    model::{Overview, OverviewMetrics, TokenUsageBucket, TokenUsageGranularity},
    Result,
};

use super::Store;

const OVERVIEW_DAYS: u64 = 365;
const MAX_RANGE_BUCKETS: i64 = 60;
const MAX_EXPLICIT_BUCKETS: i64 = 1440;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

impl Store {
    pub async fn overview(
        &self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        model_hashes: Option<&str>,
        bucket_ms: Option<i64>,
        timezone_offset_minutes: Option<i32>,
    ) -> Result<Overview> {
        let timezone_offset_ms = i64::from(timezone_offset_minutes.unwrap_or(0)) * MINUTE_MS;
        let call_row = sqlx::query(
            "SELECT
                COUNT(*) AS llm_calls,
                COALESCE(SUM(status = 'completed'), 0) AS successful_calls,
                COALESCE(SUM(status != 'completed'), 0) AS failed_calls
             FROM llm_calls
             WHERE status != 'running'
               AND (? IS NULL OR created_at_ms >= ?)
               AND (? IS NULL OR created_at_ms < ?)
               AND (? IS NULL OR model_hash IN (SELECT value FROM json_each(?)))",
        )
        .bind(start_ms)
        .bind(start_ms)
        .bind(end_ms)
        .bind(end_ms)
        .bind(model_hashes)
        .bind(model_hashes)
        .fetch_one(&self.pool)
        .await?;
        let token_row = sqlx::query(&format!(
            "SELECT
                COALESCE(SUM({fresh_input}), 0) AS input_tokens,
                COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens,
                COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens,
                COALESCE(SUM(COALESCE(output_tokens, 0)), 0) AS output_tokens
             FROM llm_calls
             WHERE (? IS NULL OR created_at_ms >= ?)
               AND (? IS NULL OR created_at_ms < ?)
               AND (? IS NULL OR model_hash IN (SELECT value FROM json_each(?)))",
            fresh_input = fresh_input_sql(),
        ))
        .bind(start_ms)
        .bind(start_ms)
        .bind(end_ms)
        .bind(end_ms)
        .bind(model_hashes)
        .bind(model_hashes)
        .fetch_one(&self.pool)
        .await?;

        let input_tokens = non_negative(token_row.try_get("input_tokens")?);
        let cache_read_tokens = non_negative(token_row.try_get("cache_read_tokens")?);
        let cache_write_tokens = non_negative(token_row.try_get("cache_write_tokens")?);
        let output_tokens = non_negative(token_row.try_get("output_tokens")?);
        let prompt_tokens = saturating_sum(&[input_tokens, cache_read_tokens, cache_write_tokens]);
        let metrics = OverviewMetrics {
            llm_calls: call_row.try_get("llm_calls")?,
            successful_calls: call_row.try_get("successful_calls")?,
            failed_calls: call_row.try_get("failed_calls")?,
            token_usage: prompt_tokens.saturating_add(output_tokens),
            prompt_tokens,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        };

        let (token_usage_granularity, bucket_ms, series_start_ms, bucket_count) =
            token_usage_buckets(start_ms, end_ms, bucket_ms, timezone_offset_ms);
        let bucket_expression = format!(
            "((created_at_ms - {timezone_offset_ms}) / {bucket_ms}) * {bucket_ms} + {timezone_offset_ms}"
        );
        let rows = sqlx::query(&format!(
            "SELECT
                {bucket_expression} AS bucket_start_ms,
                COALESCE(SUM({fresh_input}), 0) AS input_tokens,
                COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens,
                COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens,
                COALESCE(SUM(COALESCE(output_tokens, 0)), 0) AS output_tokens
             FROM llm_calls
             WHERE created_at_ms >= ?
               AND (? IS NULL OR created_at_ms < ?)
               AND (? IS NULL OR model_hash IN (SELECT value FROM json_each(?)))
             GROUP BY bucket_start_ms
             ORDER BY bucket_start_ms",
            fresh_input = fresh_input_sql(),
        ))
        .bind(start_ms.unwrap_or(series_start_ms).max(series_start_ms))
        .bind(end_ms)
        .bind(end_ms)
        .bind(model_hashes)
        .bind(model_hashes)
        .fetch_all(&self.pool)
        .await?;
        let mut recorded = rows
            .into_iter()
            .map(|row| {
                let bucket_start_ms: i64 = row.try_get("bucket_start_ms")?;
                Ok((
                    bucket_start_ms,
                    TokenUsageBucket {
                        bucket_start_ms,
                        input_tokens: non_negative(row.try_get("input_tokens")?),
                        cache_read_tokens: non_negative(row.try_get("cache_read_tokens")?),
                        cache_write_tokens: non_negative(row.try_get("cache_write_tokens")?),
                        output_tokens: non_negative(row.try_get("output_tokens")?),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let token_usage_series = (0..bucket_count)
            .map(|offset| series_start_ms.saturating_add(offset.saturating_mul(bucket_ms)))
            .map(|bucket_start_ms| {
                recorded
                    .remove(&bucket_start_ms)
                    .unwrap_or(TokenUsageBucket {
                        bucket_start_ms,
                        ..TokenUsageBucket::default()
                    })
            })
            .collect();

        Ok(Overview {
            metrics,
            token_usage_granularity,
            token_usage_series,
        })
    }
}

fn token_usage_buckets(
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    requested_bucket_ms: Option<i64>,
    timezone_offset_ms: i64,
) -> (TokenUsageGranularity, i64, i64, i64) {
    if let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) {
        let duration_ms = end_ms.saturating_sub(start_ms).max(1);
        let explicit_bucket_ms = requested_bucket_ms.filter(|bucket_ms| *bucket_ms >= MINUTE_MS);
        let bucket_ms = explicit_bucket_ms.unwrap_or(if duration_ms <= HOUR_MS {
            MINUTE_MS
        } else if duration_ms <= MAX_RANGE_BUCKETS * HOUR_MS {
            HOUR_MS
        } else {
            DAY_MS
        });
        let granularity = if bucket_ms < HOUR_MS {
            TokenUsageGranularity::Minute
        } else if bucket_ms < DAY_MS {
            TokenUsageGranularity::Hour
        } else {
            TokenUsageGranularity::Day
        };
        let max_buckets = if explicit_bucket_ms.is_some() {
            MAX_EXPLICIT_BUCKETS
        } else {
            MAX_RANGE_BUCKETS
        };
        let last_bucket_ms = (end_ms.saturating_sub(1) - timezone_offset_ms).div_euclid(bucket_ms)
            * bucket_ms
            + timezone_offset_ms;
        let first_bucket_ms =
            (start_ms - timezone_offset_ms).div_euclid(bucket_ms) * bucket_ms + timezone_offset_ms;
        let bucket_count =
            ((last_bucket_ms - first_bucket_ms).div_euclid(bucket_ms) + 1).clamp(1, max_buckets);
        let series_start_ms =
            last_bucket_ms.saturating_sub((bucket_count - 1).saturating_mul(bucket_ms));
        return (granularity, bucket_ms, series_start_ms, bucket_count);
    }

    let now_ms = Utc::now().timestamp_millis();
    let today_start_ms =
        (now_ms - timezone_offset_ms).div_euclid(DAY_MS) * DAY_MS + timezone_offset_ms;
    let series_start_ms = today_start_ms.saturating_sub(
        i64::try_from(OVERVIEW_DAYS - 1)
            .unwrap_or(0)
            .saturating_mul(DAY_MS),
    );
    (
        TokenUsageGranularity::Day,
        DAY_MS,
        series_start_ms,
        i64::try_from(OVERVIEW_DAYS).unwrap_or(0),
    )
}

fn fresh_input_sql() -> &'static str {
    "CASE
        WHEN request_type = 'anthropic' THEN MAX(0, COALESCE(input_tokens, 0))
        ELSE MAX(0, COALESCE(input_tokens, 0)
            - COALESCE(cache_read_tokens, 0)
            - COALESCE(cache_write_tokens, 0))
     END"
}

fn non_negative(value: i64) -> i64 {
    value.max(0)
}

fn saturating_sum(values: &[i64]) -> i64 {
    values
        .iter()
        .fold(0_i64, |total, value| total.saturating_add(*value))
}
#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;

    #[test]
    fn token_usage_buckets_cover_ranges_boundaries_and_caps() {
        let start_ms = 1_800_000_000_000;
        let cases = [
            (
                "one hour",
                Some(start_ms),
                Some(start_ms + HOUR_MS),
                None,
                (TokenUsageGranularity::Minute, MINUTE_MS, start_ms, 60),
            ),
            (
                "minute boundary",
                Some(start_ms + MINUTE_MS - 1_000),
                Some(start_ms + MINUTE_MS + 1_000),
                None,
                (TokenUsageGranularity::Minute, MINUTE_MS, start_ms, 2),
            ),
            (
                "explicit fifteen-minute day",
                Some(start_ms),
                Some(start_ms + DAY_MS),
                Some(15 * MINUTE_MS),
                (TokenUsageGranularity::Minute, 15 * MINUTE_MS, start_ms, 96),
            ),
            (
                "explicit thirty-minute day",
                Some(start_ms),
                Some(start_ms + DAY_MS),
                Some(30 * MINUTE_MS),
                (TokenUsageGranularity::Minute, 30 * MINUTE_MS, start_ms, 48),
            ),
            (
                "explicit hourly month",
                Some(start_ms),
                Some(start_ms + 30 * DAY_MS),
                Some(HOUR_MS),
                (TokenUsageGranularity::Hour, HOUR_MS, start_ms, 720),
            ),
            (
                "explicit minute cap",
                Some(start_ms),
                Some(start_ms + 7 * DAY_MS),
                Some(MINUTE_MS),
                (
                    TokenUsageGranularity::Minute,
                    MINUTE_MS,
                    start_ms + 7 * DAY_MS - MAX_EXPLICIT_BUCKETS * MINUTE_MS,
                    MAX_EXPLICIT_BUCKETS,
                ),
            ),
        ];

        for (name, start, end, explicit_bucket_ms, expected) in cases {
            let actual = token_usage_buckets(start, end, explicit_bucket_ms, 0);
            assert_eq!(actual, expected, "case: {name}");
        }
    }

    #[test]
    fn token_usage_buckets_align_to_requested_timezone() {
        let start_ms = 1_800_000_000_000;
        let timezone_offset_ms = -8 * HOUR_MS;
        let (_, bucket_ms, series_start_ms, bucket_count) = token_usage_buckets(
            Some(start_ms),
            Some(start_ms + DAY_MS),
            Some(DAY_MS),
            timezone_offset_ms,
        );

        assert_eq!(bucket_ms, DAY_MS);
        assert_eq!(bucket_count, 2);
        assert_eq!(
            series_start_ms,
            (start_ms - timezone_offset_ms).div_euclid(DAY_MS) * DAY_MS + timezone_offset_ms
        );
    }

    #[tokio::test]
    async fn overview_aggregates_llm_calls_and_normalizes_token_usage() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("overview.db").display()
        ))
        .await
        .unwrap();
        let now = Utc::now().timestamp_millis();
        insert_call(&store, "openai", "openai-responses", now, [100, 20, 80, 0]).await;
        insert_call(&store, "anthropic", "anthropic", now, [30, 10, 50, 5]).await;
        sqlx::query("UPDATE llm_calls SET status = 'error' WHERE call_id = 'anthropic'")
            .execute(&store.pool)
            .await
            .unwrap();
        insert_call(
            &store,
            "old",
            "openai-chat",
            (Utc::now() - Duration::days(400)).timestamp_millis(),
            [10, 5, 0, 0],
        )
        .await;

        let overview = store.overview(None, None, None, None, None).await.unwrap();
        assert_eq!(overview.metrics.llm_calls, 3);
        assert_eq!(overview.metrics.successful_calls, 2);
        assert_eq!(overview.metrics.failed_calls, 1);
        assert_eq!(overview.metrics.input_tokens, 60);
        assert_eq!(overview.metrics.cache_read_tokens, 130);
        assert_eq!(overview.metrics.cache_write_tokens, 5);
        assert_eq!(overview.metrics.output_tokens, 35);
        assert_eq!(overview.metrics.prompt_tokens, 195);
        assert_eq!(overview.metrics.token_usage, 230);
        assert_eq!(overview.token_usage_granularity, TokenUsageGranularity::Day);
        assert_eq!(overview.token_usage_series.len(), OVERVIEW_DAYS as usize);
        let today = overview.token_usage_series.last().unwrap();
        assert_eq!(today.input_tokens, 50);
        assert_eq!(today.cache_read_tokens, 130);
        assert_eq!(today.cache_write_tokens, 5);
        assert_eq!(today.output_tokens, 30);
        assert_eq!(today.total_tokens(), 215);
    }

    #[tokio::test]
    async fn overview_filters_metrics_and_usage_by_time_range() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("ranged-overview.db").display()
        ))
        .await
        .unwrap();
        let now = 1_800_000_030_000;
        insert_call(&store, "inside", "anthropic", now, [20, 5, 10, 2]).await;
        insert_call(
            &store,
            "outside",
            "anthropic",
            now - Duration::hours(2).num_milliseconds(),
            [100, 50, 40, 20],
        )
        .await;

        let overview = store
            .overview(Some(now - 1_000), Some(now + 1_000), None, None, None)
            .await
            .unwrap();

        assert_eq!(overview.metrics.llm_calls, 1);
        assert_eq!(overview.metrics.input_tokens, 20);
        assert_eq!(overview.metrics.cache_read_tokens, 10);
        assert_eq!(overview.metrics.cache_write_tokens, 2);
        assert_eq!(overview.metrics.output_tokens, 5);
        assert_eq!(
            overview.token_usage_granularity,
            TokenUsageGranularity::Minute
        );
        assert_eq!(overview.token_usage_series.len(), 1);
        assert_eq!(overview.token_usage_series[0].total_tokens(), 37);

        let filtered = store
            .overview(
                Some(now - 1_000),
                Some(now + 1_000),
                Some(r#"["missing-model"]"#),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(filtered.metrics.llm_calls, 0);
        assert_eq!(filtered.metrics.token_usage, 0);
        assert_eq!(filtered.token_usage_series[0].total_tokens(), 0);
    }

    #[tokio::test]
    async fn overview_honors_explicit_bucket_ms_within_range() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("bucketed-overview.db").display()
        ))
        .await
        .unwrap();
        let base_ms = 1_800_000_000_000;
        insert_call(
            &store,
            "before",
            "anthropic",
            base_ms - HOUR_MS,
            [500, 0, 0, 0],
        )
        .await;
        insert_call(
            &store,
            "first",
            "anthropic",
            base_ms + MINUTE_MS,
            [10, 0, 0, 0],
        )
        .await;
        insert_call(
            &store,
            "second",
            "anthropic",
            base_ms + 20 * MINUTE_MS,
            [30, 0, 0, 0],
        )
        .await;

        let overview = store
            .overview(
                Some(base_ms),
                Some(base_ms + HOUR_MS),
                None,
                Some(15 * MINUTE_MS),
                None,
            )
            .await
            .unwrap();

        assert_eq!(overview.metrics.llm_calls, 2);
        assert_eq!(
            overview.token_usage_granularity,
            TokenUsageGranularity::Minute
        );
        assert_eq!(overview.token_usage_series.len(), 4);
        assert_eq!(overview.token_usage_series[0].input_tokens, 10);
        assert_eq!(overview.token_usage_series[1].input_tokens, 30);
        assert_eq!(overview.token_usage_series[2].total_tokens(), 0);
    }

    async fn insert_call(
        store: &Store,
        call_id: &str,
        request_type: &str,
        created_at_ms: i64,
        usage: [i64; 4],
    ) {
        let [input_tokens, output_tokens, cache_read_tokens, cache_write_tokens] = usage;
        sqlx::query(
            "INSERT INTO llm_calls(
                call_id, run_id, conversation_id, provider_call_index, provider_type,
                provider_url, request_type, request_url, model_id, display_name, status,
                created_at_ms, input_tokens, output_tokens, cache_read_tokens,
                cache_write_tokens, message_count, tool_count, detailed)
             VALUES (?, 'completed', 'conversation', 0, ?, '', ?, '', 'model', 'Model',
                'completed', ?, ?, ?, ?, ?, 0, 0, 0)",
        )
        .bind(call_id)
        .bind(request_type)
        .bind(request_type)
        .bind(created_at_ms)
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(cache_read_tokens)
        .bind(cache_write_tokens)
        .execute(&store.pool)
        .await
        .unwrap();
    }
}
