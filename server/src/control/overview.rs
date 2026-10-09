//! Implements control dashboard overview endpoints.
//! HTTP handler for the desktop overview aggregates.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;

use crate::{model::Overview, Result};

use super::ControlService;

#[derive(Debug, Default, Deserialize)]
pub struct OverviewRange {
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    model_hashes: Option<String>,
    bucket_ms: Option<i64>,
    timezone_offset_minutes: Option<i32>,
}

pub async fn get(
    State(service): State<ControlService>,
    Query(range): Query<OverviewRange>,
) -> Result<Json<Overview>> {
    Ok(Json(
        service
            .overview(
                range.start_ms,
                range.end_ms,
                range.model_hashes.as_deref(),
                range.bucket_ms,
                range.timezone_offset_minutes,
            )
            .await?,
    ))
}
