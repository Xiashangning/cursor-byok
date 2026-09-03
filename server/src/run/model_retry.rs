//! Defines retry policy for one logical model call.

use std::time::Duration;

use axum::http::StatusCode;

use super::ModelCycleFailure;

pub(super) const MAX_MODEL_RETRIES: u32 = 8;
pub(super) const MODEL_RETRY_DELAY: Duration = Duration::from_secs(5);

pub(super) fn should_retry(failure: &ModelCycleFailure, retries: u32) -> bool {
    failure.retryable && retries < MAX_MODEL_RETRIES
}

/// Provider responses a repeat cannot fix: the provider refused the request
/// itself (bad key, unknown model, invalid body). 408 and 429 are the two 4xx
/// statuses that ask for another attempt, so they stay transient like 5xx.
pub(super) fn is_permanent_rejection(error: &crate::Error) -> bool {
    matches!(
        error,
        crate::Error::ProviderStatus { status, .. }
            if status.is_client_error()
                && !matches!(
                    *status,
                    StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
                )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{ModelCycleFailure, RunFailure};

    fn failure(retryable: bool) -> ModelCycleFailure {
        ModelCycleFailure {
            failure: RunFailure::Provider("failed".into()),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            usage: None,
            retryable,
        }
    }

    #[test]
    fn permits_eight_retries_after_the_initial_attempt() {
        let retryable = failure(true);
        for retries in 0..MAX_MODEL_RETRIES {
            assert!(should_retry(&retryable, retries));
        }
        assert!(!should_retry(&retryable, MAX_MODEL_RETRIES));
    }

    #[test]
    fn terminal_failures_never_retry() {
        assert!(!should_retry(&failure(false), 0));
    }

    fn status_error(status: u16) -> crate::Error {
        crate::Error::ProviderStatus {
            status: StatusCode::from_u16(status).unwrap(),
            message: format!("test {status}"),
        }
    }

    #[test]
    fn rejected_requests_are_permanent() {
        for status in [400, 401, 403, 404, 422] {
            assert!(is_permanent_rejection(&status_error(status)), "{status}");
        }
    }

    #[test]
    fn overload_timeouts_and_transport_failures_stay_transient() {
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(!is_permanent_rejection(&status_error(status)), "{status}");
        }
        assert!(!is_permanent_rejection(&crate::Error::Provider(
            "stream disconnected".into()
        )));
    }
}
