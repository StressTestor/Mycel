use std::{
    collections::BTreeMap,
    env, fmt, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use mycel_agent_protocol::{ProviderError, ProviderErrorKind, ProviderRequestAuth, SecretString};
use ring::{
    rand::SystemRandom,
    signature::{RsaKeyPair, RSA_PKCS1_SHA256},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::form_urlencoded;
use zeroize::Zeroizing;

use crate::{
    auth::{AuthFuture, RequestAuthProvider},
    error::{connection_error, invalid_request, malformed_error},
    http::{collect_body, HttpRequest, HttpTransport},
};

pub const GOOGLE_OAUTH_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const GOOGLE_APPLICATION_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const GOOGLE_ASSERTION_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const GOOGLE_ASSERTION_LIFETIME_SECONDS: u64 = 3_600;
const DEFAULT_EXPIRY_SKEW_SECONDS: u64 = 300;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 1_048_576;
const MAX_TOKEN_RESPONSE_BYTES: usize = 1_048_576;

pub trait UnixClock: Send + Sync {
    fn unix_seconds(&self) -> Result<u64, ProviderError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn unix_seconds(&self) -> Result<u64, ProviderError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| connection_error("system clock is before the Unix epoch"))
    }
}

#[derive(Clone)]
pub struct GoogleServiceAccountCredentials {
    client_email: String,
    private_key: SecretString,
    private_key_id: Option<String>,
}

impl fmt::Debug for GoogleServiceAccountCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleServiceAccountCredentials")
            .field("client_email", &self.client_email)
            .field("private_key", &"[REDACTED]")
            .field("private_key_id", &self.private_key_id)
            .field("token_uri", &GOOGLE_OAUTH_TOKEN_URI)
            .finish()
    }
}

impl GoogleServiceAccountCredentials {
    pub fn new(
        client_email: impl Into<String>,
        private_key: SecretString,
        private_key_id: Option<String>,
    ) -> Result<Self, ProviderError> {
        let credentials = Self {
            client_email: client_email.into(),
            private_key,
            private_key_id,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, ProviderError> {
        let document: ServiceAccountDocument = serde_json::from_slice(bytes)
            .map_err(|_| invalid_request("invalid Google service-account credential JSON"))?;
        if document.credential_type != "service_account" {
            return Err(invalid_request(
                "Google credential JSON is not a service account",
            ));
        }
        if document.token_uri != GOOGLE_OAUTH_TOKEN_URI {
            return Err(invalid_request(
                "Google service-account token_uri must use the standard OAuth token endpoint",
            ));
        }
        Self::new(
            document.client_email,
            SecretString::new(document.private_key),
            document.private_key_id,
        )
    }

    fn validate(&self) -> Result<(), ProviderError> {
        if self.client_email.trim() != self.client_email
            || self.client_email.is_empty()
            || !self.client_email.contains('@')
            || self.client_email.chars().any(char::is_control)
        {
            return Err(invalid_request(
                "Google service-account client_email is invalid",
            ));
        }
        if self
            .private_key_id
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(invalid_request(
                "Google service-account private_key_id is invalid",
            ));
        }
        decode_pkcs8_pem(self.private_key.expose()).map(|_| ())
    }
}

#[derive(Clone, Debug)]
pub enum GoogleServiceAccountCredentialSource {
    Inline(GoogleServiceAccountCredentials),
    File(PathBuf),
    ApplicationDefault,
}

impl GoogleServiceAccountCredentialSource {
    pub fn resolve(
        &self,
        application_default_override: Option<&Path>,
    ) -> Result<GoogleServiceAccountCredentials, ProviderError> {
        match self {
            Self::Inline(credentials) => Ok(credentials.clone()),
            Self::File(path) => read_credential_file(path),
            Self::ApplicationDefault => {
                let path = if let Some(path) = application_default_override {
                    path.to_path_buf()
                } else {
                    env::var_os(GOOGLE_APPLICATION_CREDENTIALS)
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .ok_or_else(|| {
                            invalid_request(
                                "GOOGLE_APPLICATION_CREDENTIALS is required for application-default service-account credentials",
                            )
                        })?
                };
                read_credential_file(&path)
            }
        }
    }
}

#[derive(Deserialize)]
struct ServiceAccountDocument {
    #[serde(rename = "type")]
    credential_type: String,
    client_email: String,
    private_key: String,
    #[serde(default)]
    private_key_id: Option<String>,
    token_uri: String,
}

fn read_credential_file(path: &Path) -> Result<GoogleServiceAccountCredentials, ProviderError> {
    if path.as_os_str().is_empty() {
        return Err(invalid_request(
            "Google service-account credential path is empty",
        ));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        invalid_request(format!(
            "could not inspect Google service-account credential file {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(invalid_request(format!(
            "Google service-account credential path is not a file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(invalid_request(
            "Google service-account credential file exceeds 1 MiB",
        ));
    }
    let bytes = Zeroizing::new(fs::read(path).map_err(|error| {
        invalid_request(format!(
            "could not read Google service-account credential file {}: {error}",
            path.display()
        ))
    })?);
    GoogleServiceAccountCredentials::from_json(&bytes)
}

#[derive(Clone)]
struct CachedAccessToken {
    access_token: SecretString,
    expires_at: u64,
}

pub struct GoogleServiceAccountTokenProvider {
    credentials: GoogleServiceAccountCredentials,
    transport: Arc<dyn HttpTransport>,
    clock: Arc<dyn UnixClock>,
    expiry_skew_seconds: u64,
    cached: Mutex<Option<CachedAccessToken>>,
    generation: AtomicU64,
    signer: Arc<dyn AssertionSigner>,
}

impl GoogleServiceAccountTokenProvider {
    pub fn new(
        credentials: GoogleServiceAccountCredentials,
        transport: Arc<dyn HttpTransport>,
        clock: Arc<dyn UnixClock>,
    ) -> Result<Self, ProviderError> {
        let signer: Arc<dyn AssertionSigner> = Arc::new(RingAssertionSigner);
        signer.validate(&credentials.private_key)?;
        Ok(Self {
            credentials,
            transport,
            clock,
            expiry_skew_seconds: DEFAULT_EXPIRY_SKEW_SECONDS,
            cached: Mutex::new(None),
            generation: AtomicU64::new(0),
            signer,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_signer(
        credentials: GoogleServiceAccountCredentials,
        transport: Arc<dyn HttpTransport>,
        clock: Arc<dyn UnixClock>,
        signer: Arc<dyn AssertionSigner>,
    ) -> Result<Self, ProviderError> {
        signer.validate(&credentials.private_key)?;
        Ok(Self {
            credentials,
            transport,
            clock,
            expiry_skew_seconds: DEFAULT_EXPIRY_SKEW_SECONDS,
            cached: Mutex::new(None),
            generation: AtomicU64::new(0),
            signer,
        })
    }

    async fn resolve(
        &self,
        force_refresh: bool,
        observed_generation: u64,
    ) -> Result<CachedAccessToken, ProviderError> {
        let mut cached = self.cached.lock().await;
        let now = self.clock.unix_seconds()?;
        if force_refresh && self.generation.load(Ordering::Acquire) != observed_generation {
            return cached.clone().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "Google access token refresh completed without a token",
                )
            });
        }
        if !force_refresh {
            if let Some(token) = cached
                .as_ref()
                .filter(|token| token.expires_at > now.saturating_add(self.expiry_skew_seconds))
            {
                return Ok(token.clone());
            }
        }
        let token = self.mint(now).await?;
        *cached = Some(token.clone());
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(token)
    }

    async fn mint(&self, now: u64) -> Result<CachedAccessToken, ProviderError> {
        let assertion = self.create_assertion(now)?;
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", GOOGLE_ASSERTION_GRANT)
            .append_pair("assertion", &assertion)
            .finish()
            .into_bytes();
        let response = self
            .transport
            .send(HttpRequest {
                method: "POST".into(),
                url: GOOGLE_OAUTH_TOKEN_URI.into(),
                headers: BTreeMap::from([
                    ("accept".into(), "application/json".into()),
                    (
                        "content-type".into(),
                        "application/x-www-form-urlencoded".into(),
                    ),
                ]),
                body,
                timeout: Duration::from_secs(30),
            })
            .await
            .map_err(|error| {
                if error.timeout {
                    connection_error("Google OAuth token request timed out")
                } else {
                    connection_error("Google OAuth token request failed")
                }
            })?;
        let status = response.status;
        let bytes = Zeroizing::new(
            collect_body(response.body, MAX_TOKEN_RESPONSE_BYTES)
                .await
                .map_err(|_| malformed_error("invalid Google OAuth token response body"))?,
        );
        if !(200..300).contains(&status) {
            return Err(token_endpoint_error(status));
        }
        let response: TokenResponse = serde_json::from_slice(&bytes)
            .map_err(|_| malformed_error("invalid Google OAuth token response JSON"))?;
        if response.access_token.is_empty() {
            return Err(malformed_error(
                "Google OAuth token response has an empty access_token",
            ));
        }
        if !response.token_type.eq_ignore_ascii_case("bearer") {
            return Err(malformed_error(
                "Google OAuth token response has an unsupported token_type",
            ));
        }
        if response.expires_in <= self.expiry_skew_seconds {
            return Err(malformed_error(
                "Google OAuth token lifetime does not exceed the refresh skew",
            ));
        }
        let expires_at = now.checked_add(response.expires_in).ok_or_else(|| {
            malformed_error("Google OAuth token expiry exceeds the supported clock range")
        })?;
        Ok(CachedAccessToken {
            access_token: SecretString::new(response.access_token),
            expires_at,
        })
    }

    fn create_assertion(&self, now: u64) -> Result<String, ProviderError> {
        let expires_at = now
            .checked_add(GOOGLE_ASSERTION_LIFETIME_SECONDS)
            .ok_or_else(|| connection_error("system clock exceeds the supported JWT range"))?;
        let header = JwtHeader {
            algorithm: "RS256",
            token_type: "JWT",
            key_id: self.credentials.private_key_id.as_deref(),
        };
        let claims = JwtClaims {
            issuer: &self.credentials.client_email,
            scope: GOOGLE_CLOUD_PLATFORM_SCOPE,
            audience: GOOGLE_OAUTH_TOKEN_URI,
            issued_at: now,
            expires_at,
        };
        let encoded_header = encode_json(&header)?;
        let encoded_claims = encode_json(&claims)?;
        let signing_input = format!("{encoded_header}.{encoded_claims}");
        let signature = self
            .signer
            .sign(&self.credentials.private_key, signing_input.as_bytes())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

impl RequestAuthProvider for GoogleServiceAccountTokenProvider {
    fn request_auth<'a>(&'a self, force_refresh: bool) -> AuthFuture<'a> {
        let observed_generation = self.generation.load(Ordering::Acquire);
        Box::pin(async move {
            let token = self.resolve(force_refresh, observed_generation).await?;
            Ok(ProviderRequestAuth {
                api_key: Some(token.access_token),
                headers: BTreeMap::new(),
            })
        })
    }
}

#[derive(Serialize)]
struct JwtHeader<'a> {
    #[serde(rename = "alg")]
    algorithm: &'a str,
    #[serde(rename = "typ")]
    token_type: &'a str,
    #[serde(rename = "kid", skip_serializing_if = "Option::is_none")]
    key_id: Option<&'a str>,
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    #[serde(rename = "iss")]
    issuer: &'a str,
    scope: &'a str,
    #[serde(rename = "aud")]
    audience: &'a str,
    #[serde(rename = "iat")]
    issued_at: u64,
    #[serde(rename = "exp")]
    expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    token_type: String,
}

fn encode_json(value: &impl Serialize) -> Result<String, ProviderError> {
    serde_json::to_vec(value)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .map_err(|_| malformed_error("could not encode Google service-account assertion"))
}

fn decode_pkcs8_pem(pem: &str) -> Result<Zeroizing<Vec<u8>>, ProviderError> {
    const BEGIN_LABEL: &str = concat!("-----BEGIN ", "PRIVATE KEY-----");
    const END_LABEL: &str = concat!("-----END ", "PRIVATE KEY-----");
    let mut lines = pem.lines();
    if lines.next() != Some(BEGIN_LABEL) {
        return Err(invalid_request(
            "Google service-account key must be PKCS#8 PEM",
        ));
    }
    let mut encoded = Zeroizing::new(String::new());
    let mut ended = false;
    for line in lines.by_ref() {
        if line == END_LABEL {
            ended = true;
            break;
        }
        let line = line.trim();
        if line.is_empty() || !line.bytes().all(is_base64_byte) {
            return Err(invalid_request("Google service-account key is invalid"));
        }
        encoded.push_str(line);
    }
    if !ended || lines.any(|line| !line.trim().is_empty()) {
        return Err(invalid_request("Google service-account key is invalid"));
    }
    STANDARD
        .decode(encoded.as_bytes())
        .map(Zeroizing::new)
        .map_err(|_| invalid_request("Google service-account key contains invalid base64"))
}

fn is_base64_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b'+' | b'/' | b'=')
}

pub(crate) trait AssertionSigner: Send + Sync {
    fn validate(&self, private_key: &SecretString) -> Result<(), ProviderError>;
    fn sign(&self, private_key: &SecretString, message: &[u8]) -> Result<Vec<u8>, ProviderError>;
}

#[derive(Clone, Copy, Debug)]
struct RingAssertionSigner;

impl AssertionSigner for RingAssertionSigner {
    fn validate(&self, private_key: &SecretString) -> Result<(), ProviderError> {
        let der = decode_pkcs8_pem(private_key.expose())?;
        RsaKeyPair::from_pkcs8(&der)
            .map(|_| ())
            .map_err(|_| invalid_request("Google service-account key is invalid"))
    }

    fn sign(&self, private_key: &SecretString, message: &[u8]) -> Result<Vec<u8>, ProviderError> {
        let der = decode_pkcs8_pem(private_key.expose())?;
        let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "Google service-account key is invalid",
            )
        })?;
        let mut signature = vec![0_u8; key_pair.public().modulus_len()];
        key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                message,
                &mut signature,
            )
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "could not sign Google service-account assertion",
                )
            })?;
        Ok(signature)
    }
}

fn token_endpoint_error(status: u16) -> ProviderError {
    let kind = if matches!(status, 400 | 401 | 403) {
        ProviderErrorKind::Authentication
    } else if status == 429 {
        ProviderErrorKind::RateLimit
    } else {
        ProviderErrorKind::Other
    };
    ProviderError {
        kind,
        message: format!("Google OAuth token exchange failed with HTTP {status}"),
        retryable: status == 429 || (500..600).contains(&status),
        status_code: Some(status),
        retry_after_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use bytes::Bytes;
    use futures_util::{future, stream, FutureExt};
    use tempfile::TempDir;

    use crate::http::{ByteStream, HttpResponse, TransportFuture};

    use super::*;

    type FakeResponse = (u16, Vec<Vec<u8>>);

    #[derive(Default)]
    struct FakeTransport {
        requests: StdMutex<Vec<HttpRequest>>,
        responses: StdMutex<VecDeque<FakeResponse>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, body: &str) {
            self.responses
                .lock()
                .expect("responses")
                .push_back((status, vec![body.as_bytes().to_vec()]));
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fixture response");
            future::ready(Ok(HttpResponse {
                status: response.0,
                headers: BTreeMap::new(),
                body: Box::pin(stream::iter(
                    response.1.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
                )) as ByteStream,
            }))
            .boxed()
        }
    }

    #[derive(Default)]
    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self(AtomicU64::new(now))
        }

        fn set(&self, now: u64) {
            self.0.store(now, Ordering::Release);
        }
    }

    impl UnixClock for FakeClock {
        fn unix_seconds(&self) -> Result<u64, ProviderError> {
            Ok(self.0.load(Ordering::Acquire))
        }
    }

    #[derive(Debug)]
    struct FakeSigner;

    impl AssertionSigner for FakeSigner {
        fn validate(&self, private_key: &SecretString) -> Result<(), ProviderError> {
            if private_key.expose() == fake_pem() {
                Ok(())
            } else {
                Err(invalid_request("unexpected test key"))
            }
        }

        fn sign(
            &self,
            private_key: &SecretString,
            message: &[u8],
        ) -> Result<Vec<u8>, ProviderError> {
            self.validate(private_key)?;
            assert!(message.starts_with(b"eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCI"));
            Ok(b"recorded-signature".to_vec())
        }
    }

    fn fake_pem() -> &'static str {
        concat!(
            "-----BEGIN ",
            "PRIVATE KEY-----\n",
            "dGVzdA==\n",
            "-----END ",
            "PRIVATE KEY-----"
        )
    }

    fn credentials() -> GoogleServiceAccountCredentials {
        GoogleServiceAccountCredentials::new(
            "mycel-test@example.iam.gserviceaccount.com",
            SecretString::new(fake_pem()),
            Some("test-key-id".into()),
        )
        .expect("credentials")
    }

    fn provider(
        transport: Arc<FakeTransport>,
        clock: Arc<FakeClock>,
    ) -> GoogleServiceAccountTokenProvider {
        GoogleServiceAccountTokenProvider::new_with_signer(
            credentials(),
            transport,
            clock,
            Arc::new(FakeSigner),
        )
        .expect("provider")
    }

    fn token_response(token: &str) -> String {
        format!("{{\"access_token\":\"{token}\",\"expires_in\":3600,\"token_type\":\"Bearer\"}}")
    }

    #[tokio::test]
    async fn mints_exact_assertion_request_and_redacts_secrets() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, &token_response("access-secret"));
        let provider = provider(transport.clone(), Arc::new(FakeClock::new(1_700_000_000)));

        let auth = provider.request_auth(false).await.expect("minted auth");
        assert_eq!(auth.api_key.expect("token").expose(), "access-secret");
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, GOOGLE_OAUTH_TOKEN_URI);
        assert_eq!(request.headers["accept"], "application/json");
        assert_eq!(
            request.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(request.timeout, Duration::from_secs(30));
        let form = form_urlencoded::parse(&request.body)
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(form["grant_type"], GOOGLE_ASSERTION_GRANT);
        let jwt = &form["assertion"];
        let parts = jwt.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("header base64"))
                .expect("header JSON");
        assert_eq!(
            header,
            serde_json::json!({"alg":"RS256","typ":"JWT","kid":"test-key-id"})
        );
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("claims base64"))
                .expect("claims JSON");
        assert_eq!(claims["iss"], "mycel-test@example.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], GOOGLE_CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims["aud"], GOOGLE_OAUTH_TOKEN_URI);
        assert_eq!(claims["iat"], 1_700_000_000_u64);
        assert_eq!(claims["exp"], 1_700_003_600_u64);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[2]).expect("signature"),
            b"recorded-signature"
        );

        let request_debug = format!("{request:?}");
        let credential_debug = format!("{:?}", credentials());
        assert!(!request_debug.contains(jwt));
        assert!(!request_debug.contains("access-secret"));
        assert!(!credential_debug.contains("dGVzdA"));
    }

    #[tokio::test]
    async fn coordinates_initial_expiry_and_forced_refreshes_exactly_once() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, &token_response("initial"));
        transport.response(200, &token_response("forced"));
        transport.response(200, &token_response("expired"));
        let clock = Arc::new(FakeClock::new(1_000));
        let provider = provider(transport.clone(), clock.clone());

        let (first, second) =
            tokio::join!(provider.request_auth(false), provider.request_auth(false));
        assert_eq!(
            first.expect("first").api_key.expect("token").expose(),
            "initial"
        );
        assert_eq!(
            second.expect("second").api_key.expect("token").expose(),
            "initial"
        );
        assert_eq!(transport.requests.lock().expect("requests").len(), 1);

        let forced = (0..8)
            .map(|_| provider.request_auth(true))
            .collect::<Vec<_>>();
        let forced = future::join_all(forced).await;
        assert!(forced.into_iter().all(|result| result
            .expect("forced")
            .api_key
            .expect("token")
            .expose()
            == "forced"));
        assert_eq!(transport.requests.lock().expect("requests").len(), 2);

        clock.set(4_300);
        assert_eq!(
            provider
                .request_auth(false)
                .await
                .expect("expiry refresh")
                .api_key
                .expect("token")
                .expose(),
            "expired"
        );
        assert_eq!(transport.requests.lock().expect("requests").len(), 3);
    }

    #[test]
    fn loads_explicit_and_application_default_files_and_rejects_bad_contracts() {
        let temp = TempDir::new().expect("temp");
        let path = temp.path().join("service-account.json");
        let document = serde_json::json!({
            "type":"service_account",
            "project_id":"project-1",
            "private_key_id":"test-key-id",
            "private_key":fake_pem(),
            "client_email":"mycel-test@example.iam.gserviceaccount.com",
            "client_id":"unused-standard-field",
            "auth_uri":"https://accounts.google.com/o/oauth2/auth",
            "token_uri":GOOGLE_OAUTH_TOKEN_URI
        });
        fs::write(&path, serde_json::to_vec(&document).expect("document")).expect("write");
        GoogleServiceAccountCredentialSource::File(path.clone())
            .resolve(None)
            .expect("explicit file");
        GoogleServiceAccountCredentialSource::ApplicationDefault
            .resolve(Some(&path))
            .expect("injected application default file");

        let mut wrong_audience = document.clone();
        wrong_audience["token_uri"] = serde_json::json!("https://attacker.invalid/token");
        assert_eq!(
            GoogleServiceAccountCredentials::from_json(
                &serde_json::to_vec(&wrong_audience).expect("JSON")
            )
            .expect_err("audience")
            .kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut wrong_type = document;
        wrong_type["type"] = serde_json::json!("authorized_user");
        assert_eq!(
            GoogleServiceAccountCredentials::from_json(
                &serde_json::to_vec(&wrong_type).expect("JSON")
            )
            .expect_err("type")
            .kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[tokio::test]
    async fn token_failures_are_sanitized_and_never_fall_back() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            401,
            "{\"error\":\"invalid_grant\",\"error_description\":\"leaked-secret\"}",
        );
        let provider = provider(transport, Arc::new(FakeClock::new(1_000)));
        let error = provider
            .request_auth(false)
            .await
            .expect_err("OAuth failure");
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(error.status_code, Some(401));
        assert!(!error.retryable);
        assert!(!error.message.contains("leaked-secret"));
    }

    #[test]
    fn production_signer_rejects_non_rsa_material_without_echoing_it() {
        let error = RingAssertionSigner
            .validate(&SecretString::new(fake_pem()))
            .expect_err("not an RSA key");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(!error.message.contains("dGVzdA"));
    }
}
