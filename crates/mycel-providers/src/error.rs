use std::{collections::BTreeMap, time::Duration};

use mycel_agent_protocol::{ProviderError, ProviderErrorKind};

use crate::http::RetryPolicy;

const RETRYABLE_STATUS: &[u16] = &[408, 409, 429, 500, 502, 503, 504, 529];

pub fn connection_error(message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Connection,
        message: message.into(),
        retryable: true,
        status_code: None,
        retry_after_ms: None,
    }
}

pub fn malformed_error(message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::MalformedResponse,
        message: message.into(),
        retryable: false,
        status_code: None,
        retry_after_ms: None,
    }
}

pub fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::InvalidRequest,
        message: message.into(),
        retryable: false,
        status_code: None,
        retry_after_ms: None,
    }
}

pub fn classify_http_error(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> ProviderError {
    let body_text = String::from_utf8_lossy(body);
    let detail = extract_error_message(&body_text).unwrap_or_else(|| body_text.trim().to_owned());
    let lower = detail.to_ascii_lowercase();
    let kind = if status == 401 || status == 403 {
        ProviderErrorKind::Authentication
    } else if status == 429 {
        ProviderErrorKind::RateLimit
    } else if (400..500).contains(&status) {
        ProviderErrorKind::InvalidRequest
    } else {
        ProviderErrorKind::Other
    };
    let retryable = RETRYABLE_STATUS.contains(&status)
        && !is_context_overflow(status, &lower)
        && !is_body_too_large(status, &lower)
        && !is_image_format_error(&lower);
    ProviderError {
        kind,
        message: if detail.is_empty() {
            format!("provider request failed with HTTP {status}")
        } else {
            format!("provider request failed with HTTP {status}: {detail}")
        },
        retryable,
        status_code: Some(status),
        retry_after_ms: parse_retry_after(headers),
    }
}

fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for candidate in [
        value.pointer("/error/message"),
        value.pointer("/message"),
        value.pointer("/detail"),
        value.pointer("/error"),
    ] {
        if let Some(value) = candidate.and_then(serde_json::Value::as_str) {
            return Some(value.to_owned());
        }
    }
    Some(value.to_string())
}

fn parse_retry_after(headers: &BTreeMap<String, String>) -> Option<u64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000))
}

fn is_context_overflow(status: u16, text: &str) -> bool {
    matches!(status, 400 | 413 | 422)
        && [
            "context_length_exceeded",
            "maximum context length",
            "context window",
            "too many tokens",
            "prompt is too long",
        ]
        .iter()
        .any(|pattern| text.contains(pattern))
}

fn is_body_too_large(status: u16, text: &str) -> bool {
    status == 413
        || [
            "request exceeds maximum size",
            "payload too large",
            "entity too large",
            "content too large",
            "request body too large",
        ]
        .iter()
        .any(|pattern| text.contains(pattern))
}

fn is_image_format_error(text: &str) -> bool {
    [
        "unsupported image",
        "invalid image format",
        "image format is not supported",
        "could not process image",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
        && !["too many images", "image size", "images are disabled"]
            .iter()
            .any(|pattern| text.contains(pattern))
}

pub(crate) fn retry_delay(
    error: &ProviderError,
    zero_based_attempt: u32,
    policy: RetryPolicy,
    random_unit: f64,
) -> Duration {
    if let Some(milliseconds) = error.retry_after_ms.filter(|value| *value > 0) {
        return Duration::from_millis(milliseconds);
    }
    let multiplier = 1_u32
        .checked_shl(zero_based_attempt.min(31))
        .unwrap_or(u32::MAX);
    let base = policy.base_delay.saturating_mul(multiplier);
    let capped = base.min(policy.max_delay);
    if policy.jitter {
        let jitter = 1.0 + random_unit.clamp(0.0, 1.0) * 0.25;
        Duration::from_secs_f64(capped.as_secs_f64() * jitter)
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_retryable_and_deterministic_errors() {
        let headers = BTreeMap::from([("retry-after".into(), "7".into())]);
        let rate = classify_http_error(429, &headers, br#"{"error":{"message":"slow"}}"#);
        assert_eq!(rate.kind, ProviderErrorKind::RateLimit);
        assert!(rate.retryable);
        assert_eq!(rate.retry_after_ms, Some(7000));

        let context = classify_http_error(
            413,
            &BTreeMap::new(),
            br#"{"error":{"message":"context_length_exceeded"}}"#,
        );
        assert_eq!(context.kind, ProviderErrorKind::InvalidRequest);
        assert!(!context.retryable);
    }

    #[test]
    fn retryable_status_matrix_matches_retained_runtime_policy() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(
                classify_http_error(status, &BTreeMap::new(), b"failure").retryable,
                "HTTP {status} must retry"
            );
        }
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(
                !classify_http_error(status, &BTreeMap::new(), b"failure").retryable,
                "HTTP {status} must not retry"
            );
        }
    }

    #[test]
    fn deterministic_size_context_and_image_errors_never_retry() {
        for (status, message) in [
            (413, "payload too large"),
            (422, "maximum context length exceeded"),
            (400, "invalid image format"),
        ] {
            let body = serde_json::json!({"error":{"message":message}}).to_string();
            let error = classify_http_error(status, &BTreeMap::new(), body.as_bytes());
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn retry_after_is_case_insensitive_and_ignores_http_dates() {
        let numeric = classify_http_error(
            429,
            &BTreeMap::from([("ReTrY-AfTeR".into(), "12".into())]),
            b"rate limited",
        );
        assert_eq!(numeric.retry_after_ms, Some(12_000));
        let date = classify_http_error(
            429,
            &BTreeMap::from([("retry-after".into(), "Wed, 21 Oct 2026 07:28:00 GMT".into())]),
            b"rate limited",
        );
        assert_eq!(date.retry_after_ms, None);
    }
}
