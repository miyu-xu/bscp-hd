use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use hd_core::{
    AcquireDisplaySessionRequestV2, ActionRequestV2, ApiErrorV2, CONTROL_PROTOCOL_VERSION,
    CreateInstanceRequestV2, CreateOperationRequestV2, DiagnosticBundleResponseV2,
    DiagnosticRequestV2, DisplaySessionV2, DisplayViewportV2, HealthResponseV2, HostCapabilitiesV2,
    HostRuntimeDescriptorV2, InstanceActionV2, InstanceRecordV2, InstanceSummaryV2,
    NativeDisplayTargetV2, OperationKindV2, OperationRecordV2, OperationStateV2,
    ReleaseDisplaySessionRequestV2, ScreenshotRecordV2, ShutdownHostRequestV2,
    UpdateDisplaySessionRequestV2, UpdateInstanceRequestV2, UploadRecordV2, WorkerIdentityV2,
};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, HOST, HeaderMap, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const DESCRIPTOR_LIMIT: u64 = 64 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const HOST_START_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct HostClientV2 {
    paths: hd_platform::DataPaths,
    descriptor: HostRuntimeDescriptorV2,
    http: reqwest::Client,
}

impl std::fmt::Debug for HostClientV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostClientV2")
            .field("paths", &self.paths)
            .field("origin", &self.descriptor.origin)
            .field("host_pid", &self.descriptor.pid)
            .field("host_started_at", &self.descriptor.started_at)
            .finish_non_exhaustive()
    }
}

impl HostClientV2 {
    pub async fn connect(paths: hd_platform::DataPaths) -> Result<Self, ClientError> {
        paths.validate_root()?;
        let descriptor_path = paths.host_runtime_descriptor();
        let descriptor: HostRuntimeDescriptorV2 =
            serde_json::from_slice(&read_regular_limited(&descriptor_path, DESCRIPTOR_LIMIT)?)?;
        validate_descriptor(&descriptor)?;
        let identity = WorkerIdentityV2 {
            pid: descriptor.pid,
            process_start_marker: descriptor.process_start_marker.clone(),
            nonce: Uuid::nil(),
        };
        if !hd_platform::process_identity_is_alive(&identity) {
            return Err(ClientError::StaleDescriptor);
        }
        let mut headers = HeaderMap::new();
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", descriptor.bearer_token)).map_err(
                |_| ClientError::Descriptor("bearer token is not a valid header".to_owned()),
            )?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(
            HOST,
            HeaderValue::from_str(
                descriptor
                    .origin
                    .strip_prefix("http://")
                    .ok_or_else(|| ClientError::Descriptor("origin is not HTTP".to_owned()))?,
            )
            .map_err(|_| ClientError::Descriptor("origin Host is invalid".to_owned()))?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("bscp-hd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ClientError::Transport)?;
        let client = Self {
            paths,
            descriptor,
            http,
        };
        let health = client.health().await?;
        if health.protocol_version != CONTROL_PROTOCOL_VERSION
            || health.service != "hd-host"
            || health.pid != client.descriptor.pid
            || health.started_at != client.descriptor.started_at
        {
            return Err(ClientError::HealthIdentity);
        }
        Ok(client)
    }

    pub async fn connect_or_start(paths: hd_platform::DataPaths) -> Result<Self, ClientError> {
        paths.validate_root()?;
        let started = Instant::now();
        let mut spawned = false;
        let mut last_error = match Self::connect(paths.clone()).await {
            Ok(client) => return Ok(client),
            Err(error) => error,
        };
        loop {
            if !spawned {
                spawn_host(&paths)?;
                spawned = true;
            }
            if started.elapsed() >= HOST_START_TIMEOUT {
                return Err(ClientError::HostStartTimeout(last_error.to_string()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            match Self::connect(paths.clone()).await {
                Ok(client) => return Ok(client),
                Err(error) => last_error = error,
            }
        }
    }

    pub fn descriptor(&self) -> &HostRuntimeDescriptorV2 {
        &self.descriptor
    }

    pub async fn health(&self) -> Result<HealthResponseV2, ClientError> {
        self.get_json("/v2/health").await
    }

    pub async fn capabilities(
        &self,
        instance_id: Option<Uuid>,
    ) -> Result<HostCapabilitiesV2, ClientError> {
        let path = instance_id.map_or_else(
            || "/v2/capabilities".to_owned(),
            |id| format!("/v2/capabilities?instance_id={id}"),
        );
        // The first discovery for an immutable Guest bundle verifies every file. A production
        // Android rootfs can be tens of GiB, so it must not inherit the short control-plane
        // timeout used by health/list/show requests.
        self.send_json(
            self.http
                .get(self.url(&path)?)
                .timeout(Duration::from_mins(30)),
        )
        .await
    }

    pub async fn list_instances(&self) -> Result<Vec<InstanceSummaryV2>, ClientError> {
        self.get_json("/v2/instances").await
    }

    pub async fn get_instance(&self, id: Uuid) -> Result<InstanceRecordV2, ClientError> {
        self.get_json(&format!("/v2/instances/{id}")).await
    }

    pub async fn create_instance(
        &self,
        request: &CreateInstanceRequestV2,
    ) -> Result<InstanceRecordV2, ClientError> {
        self.post_json("/v2/instances", request).await
    }

    pub async fn update_instance(
        &self,
        id: Uuid,
        request: &UpdateInstanceRequestV2,
    ) -> Result<InstanceRecordV2, ClientError> {
        self.send_json(
            self.http
                .patch(self.url(&format!("/v2/instances/{id}"))?)
                .json(request),
        )
        .await
    }

    pub async fn create_operation(
        &self,
        id: Uuid,
        operation: OperationKindV2,
        idempotency_key: &str,
    ) -> Result<OperationRecordV2, ClientError> {
        if idempotency_key.is_empty()
            || idempotency_key.len() > 128
            || !idempotency_key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ClientError::Input(
                "idempotency key must use 1..=128 safe ASCII characters".to_owned(),
            ));
        }
        self.send_json(
            self.http
                .post(self.url(&format!("/v2/instances/{id}/operations"))?)
                .header("idempotency-key", idempotency_key)
                .json(&CreateOperationRequestV2 { operation }),
        )
        .await
    }

    pub async fn operation(&self, id: Uuid) -> Result<OperationRecordV2, ClientError> {
        self.get_json(&format!("/v2/operations/{id}")).await
    }

    pub async fn list_operations(&self) -> Result<Vec<OperationRecordV2>, ClientError> {
        self.get_json("/v2/operations").await
    }

    pub async fn wait_operation(
        &self,
        id: Uuid,
        timeout: Duration,
    ) -> Result<OperationRecordV2, ClientError> {
        const MAX_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 50;

        let started = Instant::now();
        let mut consecutive_transport_failures = 0_u32;
        loop {
            let operation = match self.operation(id).await {
                Ok(operation) => {
                    consecutive_transport_failures = 0;
                    operation
                }
                Err(ClientError::Transport(_))
                    if consecutive_transport_failures < MAX_CONSECUTIVE_TRANSPORT_FAILURES
                        && started.elapsed() < timeout =>
                {
                    consecutive_transport_failures += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match operation.state {
                OperationStateV2::Succeeded => return Ok(operation),
                OperationStateV2::Failed | OperationStateV2::Cancelled => {
                    return Err(ClientError::OperationFailed(
                        operation.error.unwrap_or_else(|| {
                            ApiErrorV2::new("operation_failed", "operation failed without detail")
                        }),
                    ));
                }
                OperationStateV2::Queued | OperationStateV2::Running => {}
            }
            if started.elapsed() >= timeout {
                return Err(ClientError::OperationTimeout(id));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn action(
        &self,
        id: Uuid,
        action: InstanceActionV2,
    ) -> Result<hd_core::WorkerStatusV2, ClientError> {
        self.post_json(
            &format!("/v2/instances/{id}/actions"),
            &ActionRequestV2 { action },
        )
        .await
    }

    pub async fn acquire_display_session(
        &self,
        id: Uuid,
        target: NativeDisplayTargetV2,
        viewport: DisplayViewportV2,
    ) -> Result<DisplaySessionV2, ClientError> {
        self.post_json(
            &format!("/v2/instances/{id}/display-session"),
            &AcquireDisplaySessionRequestV2 { target, viewport },
        )
        .await
    }

    pub async fn update_display_session(
        &self,
        id: Uuid,
        session_token: String,
        viewport: DisplayViewportV2,
    ) -> Result<DisplaySessionV2, ClientError> {
        self.send_json(
            self.http
                .put(self.url(&format!("/v2/instances/{id}/display-session"))?)
                .json(&UpdateDisplaySessionRequestV2 {
                    session_token,
                    viewport,
                }),
        )
        .await
    }

    pub async fn release_display_session(
        &self,
        id: Uuid,
        session_token: String,
    ) -> Result<(), ClientError> {
        let response = self
            .http
            .delete(self.url(&format!("/v2/instances/{id}/display-session"))?)
            .json(&ReleaseDisplaySessionRequestV2 { session_token })
            .send()
            .await
            .map_err(ClientError::Transport)?;
        ensure_empty_success(response).await
    }

    pub async fn capture_screenshot(&self, id: Uuid) -> Result<ScreenshotRecordV2, ClientError> {
        self.post_json(&format!("/v2/instances/{id}/screenshots"), &())
            .await
    }

    pub async fn upload_apk(&self, path: &Path) -> Result<UploadRecordV2, ClientError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|source| ClientError::Io {
                operation: "inspect APK",
                path: path.to_owned(),
                source,
            })?;
        if !metadata.is_file() {
            return Err(ClientError::Input(
                "APK path is not a regular file".to_owned(),
            ));
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
            })
            .ok_or_else(|| ClientError::Input("APK file name must end in .apk".to_owned()))?;
        let digest_path = path.to_owned();
        let sha256 = tokio::task::spawn_blocking(move || crate::sha256_file(&digest_path))
            .await
            .map_err(|error| ClientError::Task(error.to_string()))??;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|source| ClientError::Io {
                operation: "open APK",
                path: path.to_owned(),
                source,
            })?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        self.send_json(
            self.http
                .post(self.url("/v2/uploads/apk")?)
                .timeout(Duration::from_mins(30))
                .header("x-file-name", file_name)
                .header("x-content-sha256", sha256)
                .header(CONTENT_LENGTH, metadata.len())
                .body(body),
        )
        .await
    }

    pub async fn collect_diagnostics(
        &self,
        request: &DiagnosticRequestV2,
    ) -> Result<DiagnosticBundleResponseV2, ClientError> {
        self.send_json(
            self.http
                .post(self.url("/v2/diagnostics")?)
                .timeout(Duration::from_mins(5))
                .json(request),
        )
        .await
    }

    pub async fn shutdown(&self, stop_all: bool) -> Result<(), ClientError> {
        let response = self
            .http
            .post(self.url("/v2/shutdown")?)
            .json(&ShutdownHostRequestV2 { stop_all })
            .send()
            .await
            .map_err(ClientError::Transport)?;
        ensure_empty_success(response).await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send_json(self.http.get(self.url(path)?)).await
    }

    async fn post_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        self.send_json(self.http.post(self.url(path)?).json(body))
            .await
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let response = request.send().await.map_err(ClientError::Transport)?;
        decode_json_response(response).await
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, ClientError> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(ClientError::Input("API path is not absolute".to_owned()));
        }
        reqwest::Url::parse(&format!("{}{}", self.descriptor.origin, path))
            .map_err(ClientError::Url)
    }
}

async fn decode_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    let status = response.status();
    let bytes = read_response_limited(response).await?;
    if !status.is_success() {
        let error = serde_json::from_slice::<ApiErrorV2>(&bytes).unwrap_or_else(|_| {
            ApiErrorV2::new(
                "invalid_error_response",
                format!("host returned HTTP {status} without a valid API error"),
            )
        });
        return Err(ClientError::Api { status, error });
    }
    serde_json::from_slice(&bytes).map_err(ClientError::Json)
}

async fn ensure_empty_success(response: reqwest::Response) -> Result<(), ClientError> {
    let status = response.status();
    let bytes = read_response_limited(response).await?;
    if status.is_success() {
        return Ok(());
    }
    let error = serde_json::from_slice::<ApiErrorV2>(&bytes).unwrap_or_else(|_| {
        ApiErrorV2::new(
            "invalid_error_response",
            format!("host returned HTTP {status}"),
        )
    });
    Err(ClientError::Api { status, error })
}

async fn read_response_limited(response: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(ClientError::ResponseTooLarge);
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ClientError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(ClientError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_descriptor(descriptor: &HostRuntimeDescriptorV2) -> Result<(), ClientError> {
    if descriptor.protocol_version != CONTROL_PROTOCOL_VERSION
        || descriptor.bearer_token.len() != 64
        || !descriptor
            .bearer_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ClientError::Descriptor(
            "runtime descriptor version or token is invalid".to_owned(),
        ));
    }
    let origin = reqwest::Url::parse(&descriptor.origin).map_err(ClientError::Url)?;
    if origin.scheme() != "http"
        || origin.host_str() != Some("127.0.0.1")
        || origin.port().is_none()
        || origin.path() != "/"
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(ClientError::Descriptor(
            "runtime origin is not an exact IPv4 loopback origin".to_owned(),
        ));
    }
    Ok(())
}

fn spawn_host(paths: &hd_platform::DataPaths) -> Result<(), ClientError> {
    let executable =
        sibling_host().unwrap_or_else(|| PathBuf::from(hd_platform::executable_name("hd-host")));
    if executable.components().count() > 1 && !executable.is_file() {
        return Err(ClientError::HostExecutable(executable));
    }
    let arguments = vec![
        "--data-root".to_owned(),
        paths.root.to_string_lossy().into_owned(),
    ];
    hd_platform::spawn_detached(&executable, &arguments, &BTreeMap::default(), &paths.root)?;
    Ok(())
}

fn sibling_host() -> Option<PathBuf> {
    let candidate = std::env::current_exe()
        .ok()?
        .parent()?
        .join(hd_platform::executable_name("hd-host"));
    candidate.is_file().then_some(candidate)
}

fn read_regular_limited(path: &Path, maximum: u64) -> Result<Vec<u8>, ClientError> {
    hd_platform::read_regular_nofollow_limited(path, maximum).map_err(ClientError::Platform)
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("host runtime descriptor is unsafe: {0}")]
    UnsafeDescriptor(PathBuf),
    #[error("host runtime descriptor is invalid: {0}")]
    Descriptor(String),
    #[error("host runtime descriptor refers to a stale process")]
    StaleDescriptor,
    #[error("host health identity does not match its runtime descriptor")]
    HealthIdentity,
    #[error("HD host executable is missing: {0}")]
    HostExecutable(PathBuf),
    #[error("HD host did not become ready before the deadline: {0}")]
    HostStartTimeout(String),
    #[error("host response exceeded the client limit")]
    ResponseTooLarge,
    #[error("host API returned HTTP {status}: {error:?}")]
    Api {
        status: reqwest::StatusCode,
        error: ApiErrorV2,
    },
    #[error("operation {0} timed out")]
    OperationTimeout(Uuid),
    #[error("operation failed: {0:?}")]
    OperationFailed(ApiErrorV2),
    #[error("invalid client input: {0}")]
    Input(String),
    #[error("HTTP transport failed: {0}")]
    Transport(reqwest::Error),
    #[error("URL parsing failed: {0}")]
    Url(url::ParseError),
    #[error("JSON decoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("background task failed: {0}")]
    Task(String),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
    #[error(transparent)]
    Runtime(#[from] crate::ArtifactError),
}
