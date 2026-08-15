use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::{stream, StreamExt};
use mycel_agent_protocol::{
    ProviderError, ProviderErrorKind, ProviderEventStream, ProviderStreamEvent,
};

use crate::error::{classify_http_error, connection_error, retry_delay};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send>>;
pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpResponse, TransportError>> + Send + 'a>>;
pub type RetrySleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = self
            .headers
            .keys()
            .map(|name| (name, "[REDACTED]"))
            .collect::<BTreeMap<_, _>>();
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: ByteStream,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub message: String,
    pub timeout: bool,
}

impl TransportError {
    pub fn connection(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            timeout: false,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            timeout: true,
        }
    }
}

pub trait HttpTransport: Send + Sync {
    fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a>;
}

#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_redirect_policy(reqwest::redirect::Policy::limited(5))
    }

    /// Builds a transport suitable for requests carrying catalog credentials.
    /// Discovery code rejects 3xx responses instead of forwarding bearer
    /// credentials to a redirect target.
    pub fn without_redirects() -> Result<Self, ProviderError> {
        Self::with_redirect_policy(reqwest::redirect::Policy::none())
    }

    fn with_redirect_policy(policy: reqwest::redirect::Policy) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(policy)
            .build()
            .map_err(|error| {
                connection_error(format!("could not construct HTTP client: {error}"))
            })?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
        Box::pin(async move {
            let method =
                reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|error| {
                    TransportError::connection(format!("invalid HTTP method: {error}"))
                })?;
            let mut builder = self
                .client
                .request(method, &request.url)
                .timeout(request.timeout)
                .body(request.body);
            for (name, value) in request.headers {
                builder = builder.header(&name, value);
            }
            let response = builder.send().await.map_err(map_reqwest_error)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
                })
                .collect();
            let body = response
                .bytes_stream()
                .map(|chunk| chunk.map_err(map_reqwest_error));
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body),
            })
        })
    }
}

fn map_reqwest_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::timeout(error.to_string())
    } else {
        TransportError::connection(error.to_string())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(32),
            jitter: true,
        }
    }
}

pub trait RetryRuntime: Send + Sync {
    fn sleep<'a>(&'a self, duration: Duration) -> RetrySleepFuture<'a>;
    fn random_unit(&self) -> f64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioRetryRuntime;

impl RetryRuntime for TokioRetryRuntime {
    fn sleep<'a>(&'a self, duration: Duration) -> RetrySleepFuture<'a> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn random_unit(&self) -> f64 {
        crate::random::retry_random_unit()
    }
}

pub async fn send_with_retry(
    transport: &Arc<dyn HttpTransport>,
    request: HttpRequest,
    policy: RetryPolicy,
) -> Result<HttpResponse, ProviderError> {
    send_with_retry_runtime(transport, request, policy, &TokioRetryRuntime).await
}

pub async fn send_with_retry_runtime(
    transport: &Arc<dyn HttpTransport>,
    request: HttpRequest,
    policy: RetryPolicy,
    runtime: &dyn RetryRuntime,
) -> Result<HttpResponse, ProviderError> {
    let attempts = policy.max_attempts.max(1);
    for attempt in 0..attempts {
        match transport.send(request.clone()).await {
            Ok(response) if (200..300).contains(&response.status) => return Ok(response),
            Ok(response) => {
                let status = response.status;
                let headers = response.headers.clone();
                let body = collect_body(response.body, 1_048_576).await?;
                let error = classify_http_error(status, &headers, &body);
                if !error.retryable || attempt + 1 == attempts {
                    return Err(error);
                }
                runtime
                    .sleep(retry_delay(&error, attempt, policy, runtime.random_unit()))
                    .await;
            }
            Err(error) => {
                let mapped = connection_error(error.message);
                if attempt + 1 == attempts {
                    return Err(mapped);
                }
                runtime
                    .sleep(retry_delay(&mapped, attempt, policy, runtime.random_unit()))
                    .await;
            }
        }
    }
    unreachable!("retry loop always returns")
}

pub async fn collect_body(mut body: ByteStream, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| connection_error(error.message))?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(ProviderError {
                kind: ProviderErrorKind::MalformedResponse,
                message: format!("provider response exceeds {limit} bytes"),
                retryable: false,
                status_code: None,
                retry_after_ms: None,
            });
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

type EventDecoder =
    Box<dyn FnMut(&str, &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>) + Send>;

struct SseState {
    body: ByteStream,
    buffer: Vec<u8>,
    queue: VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    decoder: EventDecoder,
    eof: bool,
}

pub fn decode_sse(body: ByteStream, decoder: EventDecoder) -> ProviderEventStream {
    let state = SseState {
        body,
        buffer: Vec::new(),
        queue: VecDeque::new(),
        decoder,
        eof: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some((item, state));
            }
            if let Some(event) = take_sse_event(&mut state.buffer, state.eof) {
                if let Some(data) = sse_data(&event) {
                    (state.decoder)(&data, &mut state.queue);
                }
                continue;
            }
            if state.eof {
                return None;
            }
            match state.body.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    state.eof = true;
                    state.queue.push_back(Err(connection_error(error.message)));
                }
                None => state.eof = true,
            }
        }
    }))
}

fn take_sse_event(buffer: &mut Vec<u8>, eof: bool) -> Option<Vec<u8>> {
    let split = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| (index, 4))
        });
    if let Some((index, delimiter_len)) = split {
        let event = buffer[..index].to_vec();
        buffer.drain(..index + delimiter_len);
        return Some(event);
    }
    if eof && !buffer.is_empty() {
        return Some(std::mem::take(buffer));
    }
    None
}

fn sse_data(event: &[u8]) -> Option<String> {
    let event = String::from_utf8(event.to_vec()).ok()?;
    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>();
    (!data.is_empty()).then(|| data.join("\n"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingRetryRuntime {
        sleeps: std::sync::Mutex<Vec<Duration>>,
        random_unit: f64,
    }

    impl RetryRuntime for RecordingRetryRuntime {
        fn sleep<'a>(&'a self, duration: Duration) -> RetrySleepFuture<'a> {
            self.sleeps.lock().expect("sleeps").push(duration);
            Box::pin(async {})
        }

        fn random_unit(&self) -> f64 {
            self.random_unit
        }
    }

    pub(crate) fn body(chunks: &[&[u8]]) -> ByteStream {
        let chunks = chunks
            .iter()
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        Box::pin(stream::iter(chunks))
    }

    #[tokio::test]
    async fn sse_decoder_handles_chunked_utf8_and_crlf() {
        let body = body(&[
            b"data: {\"x\":\"",
            &[0xf0, 0x9f],
            &[0x8d, 0x84],
            b"\"}\r\n\r\n",
        ]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut stream = decode_sse(
            body,
            Box::new(move |data, _| sink.lock().expect("lock").push(data.to_owned())),
        );
        assert!(stream.next().await.is_none());
        assert_eq!(seen.lock().expect("lock").as_slice(), ["{\"x\":\"🍄\"}"]);
    }

    struct RetryTransport {
        attempts: AtomicUsize,
    }

    impl HttpTransport for RetryTransport {
        fn send<'a>(&'a self, _request: HttpRequest) -> TransportFuture<'a> {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Ok(HttpResponse {
                        status: 429,
                        headers: BTreeMap::new(),
                        body: body(&[br#"{"error":{"message":"slow down"}}"#]),
                    })
                } else {
                    Ok(HttpResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        body: body(&[b"ok"]),
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn retry_transport_replays_retryable_status() {
        let transport: Arc<dyn HttpTransport> = Arc::new(RetryTransport {
            attempts: AtomicUsize::new(0),
        });
        let response = send_with_retry(
            &transport,
            HttpRequest {
                method: "POST".into(),
                url: "https://local.test".into(),
                headers: BTreeMap::new(),
                body: Vec::new(),
                timeout: Duration::from_secs(1),
            },
            RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                jitter: false,
            },
        )
        .await
        .expect("retry succeeds");
        assert_eq!(collect_body(response.body, 16).await.expect("body"), b"ok");
    }

    struct AlwaysFailTransport {
        attempts: AtomicUsize,
        status: u16,
        retry_after: Option<&'static str>,
    }

    impl HttpTransport for AlwaysFailTransport {
        fn send<'a>(&'a self, _request: HttpRequest) -> TransportFuture<'a> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                let headers = self
                    .retry_after
                    .map(|value| BTreeMap::from([("retry-after".into(), value.into())]))
                    .unwrap_or_default();
                Ok(HttpResponse {
                    status: self.status,
                    headers,
                    body: body(&[br#"{"error":{"message":"unavailable"}}"#]),
                })
            })
        }
    }

    fn empty_request() -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            url: "https://local.test".into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn retry_runtime_enforces_ten_attempt_cap_and_deterministic_jitter() {
        let concrete = Arc::new(AlwaysFailTransport {
            attempts: AtomicUsize::new(0),
            status: 503,
            retry_after: None,
        });
        let transport: Arc<dyn HttpTransport> = concrete.clone();
        let runtime = RecordingRetryRuntime {
            sleeps: std::sync::Mutex::new(Vec::new()),
            random_unit: 0.5,
        };
        let error = match send_with_retry_runtime(
            &transport,
            empty_request(),
            RetryPolicy::default(),
            &runtime,
        )
        .await
        {
            Ok(_) => panic!("all attempts must fail"),
            Err(error) => error,
        };
        assert_eq!(error.status_code, Some(503));
        assert_eq!(concrete.attempts.load(Ordering::SeqCst), 10);
        let sleeps = runtime.sleeps.lock().expect("sleeps");
        assert_eq!(sleeps.len(), 9);
        assert_eq!(sleeps[0], Duration::from_micros(562_500));
        assert_eq!(sleeps[8], Duration::from_secs(36));
    }

    #[tokio::test]
    async fn retry_after_overrides_backoff_without_jitter() {
        let concrete = Arc::new(AlwaysFailTransport {
            attempts: AtomicUsize::new(0),
            status: 429,
            retry_after: Some("7"),
        });
        let transport: Arc<dyn HttpTransport> = concrete;
        let runtime = RecordingRetryRuntime {
            sleeps: std::sync::Mutex::new(Vec::new()),
            random_unit: 1.0,
        };
        let _ = send_with_retry_runtime(
            &transport,
            empty_request(),
            RetryPolicy {
                max_attempts: 2,
                ..RetryPolicy::default()
            },
            &runtime,
        )
        .await;
        assert_eq!(
            runtime.sleeps.lock().expect("sleeps").as_slice(),
            [Duration::from_secs(7)]
        );
    }
}
