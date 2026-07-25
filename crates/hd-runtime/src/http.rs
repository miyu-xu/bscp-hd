use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, RawQuery, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use hd_core::{
    AcquireDisplaySessionRequestV2, ActionRequestV2, ApiErrorV2, CONTROL_PROTOCOL_VERSION,
    CreateInstanceRequestV2, CreateOperationRequestV2, DiagnosticRequestV2, HealthResponseV2,
    HostRuntimeDescriptorV2, ReleaseDisplaySessionRequestV2, ShutdownHostRequestV2,
    UpdateDisplaySessionRequestV2, UpdateInstanceRequestV2,
};
use subtle::ConstantTimeEq as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::io::StreamReader;
use uuid::Uuid;

use crate::{HostError, HostService, StoreError, UploadError, store_apk_upload};

const JSON_BODY_LIMIT: usize = 1024 * 1024;
const MAX_HTTP_BODY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug)]
struct HttpState {
    host: Arc<HostService>,
    origin: String,
}

struct SecurityState {
    origin: String,
    host_header: String,
    bearer: Vec<u8>,
}

pub async fn run_host_http(host: Arc<HostService>, port: Option<u16>) -> Result<(), HttpError> {
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port.unwrap_or(0)))
            .await
            .map_err(HttpError::Bind)?;
    let address = listener.local_addr().map_err(HttpError::Bind)?;
    if !address.ip().is_loopback() {
        return Err(HttpError::NonLoopback(address));
    }
    let host_header = format!("127.0.0.1:{}", address.port());
    let origin = format!("http://{host_header}");
    let token = random_bearer()?;
    let descriptor = HostRuntimeDescriptorV2 {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        pid: std::process::id(),
        process_start_marker: hd_platform::process_start_marker(std::process::id())?,
        origin: origin.clone(),
        bearer_token: token.clone(),
        started_at: host.started_at(),
    };
    let descriptor_bytes = serde_json::to_vec_pretty(&descriptor).map_err(HttpError::Json)?;
    let openapi_value = openapi_document(&origin);
    hd_platform::write_owner_only(
        &host.paths().root.join("openapi-v2.json"),
        &serde_json::to_vec_pretty(&openapi_value).map_err(HttpError::Json)?,
    )?;
    // The descriptor is the client readiness signal, so publish it only after every file a
    // connected client can immediately read has been atomically materialized.
    hd_platform::write_owner_only(&host.paths().host_runtime_descriptor(), &descriptor_bytes)?;
    let _descriptor_guard = RuntimeDescriptorGuard {
        paths: host.paths().clone(),
        pid: descriptor.pid,
        process_start_marker: descriptor.process_start_marker.clone(),
    };

    let state = Arc::new(HttpState {
        host: Arc::clone(&host),
        origin: origin.clone(),
    });
    let security = Arc::new(SecurityState {
        origin,
        host_header,
        bearer: token.into_bytes(),
    });
    let router = Router::new()
        .route("/v2/health", get(health))
        .route("/v2/capabilities", get(capabilities))
        .route("/v2/instances", get(list_instances).post(create_instance))
        .route(
            "/v2/instances/{id}",
            get(get_instance).patch(update_instance),
        )
        .route("/v2/instances/{id}/operations", post(create_operation))
        .route("/v2/instances/{id}/actions", post(action))
        .route(
            "/v2/instances/{id}/display-session",
            post(acquire_display_session)
                .put(update_display_session)
                .delete(release_display_session),
        )
        .route("/v2/instances/{id}/screenshots", post(capture_screenshot))
        .route("/v2/operations", get(list_operations))
        .route("/v2/operations/{id}", get(get_operation))
        .route("/v2/uploads/apk", post(upload_apk))
        .route("/v2/diagnostics", post(collect_diagnostics))
        .route("/v2/events", get(events))
        .route("/v2/openapi.json", get(openapi))
        .route("/v2/shutdown", post(shutdown))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .layer(from_fn_with_state(security, security_middleware))
        .with_state(state);
    tracing::info!(
        event = "http.listen.succeeded",
        address = %address,
        "HD host API is listening on loopback"
    );
    let mut shutdown = host.shutdown_receiver();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(HttpError::Serve)
}

async fn security_middleware(
    State(security): State<Arc<SecurityState>>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uuid>().ok())
        .unwrap_or_else(Uuid::new_v4);
    let host_valid = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == security.host_header);
    if !host_valid {
        return security_rejection(
            StatusCode::BAD_REQUEST,
            "host_header_rejected",
            "Host header does not match the loopback listener",
            request_id,
            &security,
            false,
        );
    }
    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin.is_some_and(|value| value != security.origin) {
        return security_rejection(
            StatusCode::FORBIDDEN,
            "origin_rejected",
            "Origin is not the HD loopback origin",
            request_id,
            &security,
            true,
        );
    }
    if let Some(length) = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > MAX_HTTP_BODY_BYTES
    {
        return security_rejection(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds the host limit",
            request_id,
            &security,
            origin.is_some(),
        );
    }
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_security_headers(
            response.headers_mut(),
            &security,
            origin.is_some(),
            request_id,
        );
        return response;
    }
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::as_bytes);
    let authorized = supplied.is_some_and(|value| {
        value.len() == security.bearer.len() && value.ct_eq(&security.bearer).unwrap_u8() == 1
    });
    if !authorized {
        return security_rejection(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer authentication is required",
            request_id,
            &security,
            origin.is_some(),
        );
    }
    let include_cors = origin.is_some();
    let mut response = next.run(request).await;
    add_security_headers(response.headers_mut(), &security, include_cors, request_id);
    response
}

fn add_security_headers(
    headers: &mut HeaderMap,
    security: &SecurityState,
    include_cors: bool,
    request_id: Uuid,
) {
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        headers.insert("x-request-id", value);
    }
    if include_cors {
        if let Ok(value) = HeaderValue::from_str(&security.origin) {
            headers.insert("access-control-allow-origin", value);
        }
        headers.insert("vary", HeaderValue::from_static("Origin"));
        headers.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, PATCH, OPTIONS"),
        );
        headers.insert(
            "access-control-allow-headers",
            HeaderValue::from_static(
                "Authorization, Content-Type, Idempotency-Key, X-Content-SHA256, X-File-Name, X-Request-ID",
            ),
        );
    }
}

fn security_error(status: StatusCode, code: &str, message: &str, request_id: Uuid) -> Response {
    let mut response = (
        status,
        Json(ApiErrorV2::new(code, message).with_detail("request_id", request_id.to_string())),
    )
        .into_response();
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn security_rejection(
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: Uuid,
    security: &SecurityState,
    include_cors: bool,
) -> Response {
    let mut response = security_error(status, code, message, request_id);
    add_security_headers(response.headers_mut(), security, include_cors, request_id);
    response
}

async fn health(State(state): State<Arc<HttpState>>) -> Json<HealthResponseV2> {
    Json(HealthResponseV2 {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        service: "hd-host".to_owned(),
        pid: std::process::id(),
        started_at: state.host.started_at(),
    })
}

async fn capabilities(
    State(state): State<Arc<HttpState>>,
    RawQuery(query): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let instance_id = match query.as_deref() {
        None | Some("") => None,
        Some(value) => {
            let raw = value.strip_prefix("instance_id=").ok_or_else(|| {
                ApiHttpError::input("invalid_query", "only instance_id is accepted")
            })?;
            if raw.contains('&') || raw.is_empty() {
                return Err(ApiHttpError::input(
                    "invalid_query",
                    "instance_id must appear exactly once",
                ));
            }
            Some(parse_uuid(raw)?)
        }
    };
    Ok(Json(state.host.capabilities(instance_id).await?))
}

async fn list_instances(State(state): State<Arc<HttpState>>) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.host.list_instances()?))
}

async fn create_instance(
    State(state): State<Arc<HttpState>>,
    payload: Result<Json<CreateInstanceRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let request = json_payload(payload)?;
    let record = state.host.create_instance(request)?;
    Ok((StatusCode::CREATED, Json(record)))
}

async fn get_instance(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.host.get_instance(parse_uuid(&id)?)?))
}

async fn update_instance(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    payload: Result<Json<UpdateInstanceRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let id = parse_uuid(&id)?;
    Ok(Json(
        state
            .host
            .update_instance(id, json_payload(payload)?)
            .await?,
    ))
}

async fn create_operation(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    payload: Result<Json<CreateOperationRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let idempotency = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiHttpError::input("idempotency_key_required", "Idempotency-Key is required")
        })?;
    if idempotency.is_empty()
        || idempotency.len() > 128
        || !idempotency
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiHttpError::input(
            "invalid_idempotency_key",
            "Idempotency-Key must use 1..=128 safe ASCII characters",
        ));
    }
    let operation = state.host.create_operation(
        parse_uuid(&id)?,
        json_payload(payload)?.operation,
        idempotency,
    )?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

async fn action(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    payload: Result<Json<ActionRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        state
            .host
            .action(parse_uuid(&id)?, json_payload(payload)?)
            .await?,
    ))
}

async fn acquire_display_session(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    payload: Result<Json<AcquireDisplaySessionRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .host
                .acquire_display_session(parse_uuid(&id)?, json_payload(payload)?)
                .await?,
        ),
    ))
}

async fn update_display_session(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    payload: Result<Json<UpdateDisplaySessionRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        state
            .host
            .update_display_session(parse_uuid(&id)?, json_payload(payload)?)
            .await?,
    ))
}

async fn release_display_session(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
    payload: Result<Json<ReleaseDisplaySessionRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    state
        .host
        .release_display_session(parse_uuid(&id)?, json_payload(payload)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn capture_screenshot(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<impl IntoResponse> {
    Ok((
        StatusCode::CREATED,
        Json(state.host.capture_screenshot(parse_uuid(&id)?).await?),
    ))
}

async fn list_operations(State(state): State<Arc<HttpState>>) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.host.list_operations()?))
}

async fn get_operation(
    State(state): State<Arc<HttpState>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.host.operation(parse_uuid(&id)?)?))
}

async fn upload_apk(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let file_name = required_header(&headers, "x-file-name")?;
    let sha256 = required_header(&headers, "x-content-sha256")?;
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let reader = StreamReader::new(stream);
    let upload = store_apk_upload(
        state.host.paths(),
        state.host.store(),
        &file_name,
        &sha256,
        reader,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(upload)))
}

async fn collect_diagnostics(
    State(state): State<Arc<HttpState>>,
    payload: Result<Json<DiagnosticRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let request = json_payload(payload)?;
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .host
                .collect_diagnostics(request.instance_id, request.include_guest_logs)
                .await?,
        ),
    ))
}

async fn events(
    State(state): State<Arc<HttpState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.host.subscribe()).filter_map(|event| async move {
        match event {
            Ok(event) => serde_json::to_string(&event)
                .ok()
                .map(|data| Ok(Event::default().event("host_event_v2").data(data))),
            Err(_) => Some(Ok(Event::default()
                .event("stream_lagged")
                .data("resync required"))),
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn openapi(State(state): State<Arc<HttpState>>) -> Json<serde_json::Value> {
    Json(openapi_document(&state.origin))
}

async fn shutdown(
    State(state): State<Arc<HttpState>>,
    payload: Result<Json<ShutdownHostRequestV2>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    state
        .host
        .request_shutdown(json_payload(payload)?.stop_all)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ApiErrorV2::new(
            "route_not_found",
            "API route was not found",
        )),
    )
        .into_response()
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(ApiErrorV2::new(
            "method_not_allowed",
            "HTTP method is not allowed for this route",
        )),
    )
        .into_response()
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiHttpError> {
    payload.map(|Json(value)| value).map_err(|error| {
        ApiHttpError::input("invalid_json", &format!("invalid JSON request: {error}"))
    })
}

fn parse_uuid(value: &str) -> Result<Uuid, ApiHttpError> {
    value
        .parse()
        .map_err(|_| ApiHttpError::input("invalid_uuid", "path identifier is not a UUID"))
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, ApiHttpError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| ApiHttpError::input("required_header", &format!("{name} is required")))
}

fn random_bearer() -> Result<String, HttpError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| HttpError::Random(error.to_string()))?;
    Ok(hex::encode(bytes))
}

fn remove_runtime_descriptor_if_owned(
    paths: &hd_platform::DataPaths,
    pid: u32,
    process_start_marker: &str,
) -> Result<(), HttpError> {
    let path = paths.host_runtime_descriptor();
    let bytes = match hd_platform::read_regular_nofollow_limited(&path, 64 * 1024) {
        Ok(bytes) => bytes,
        Err(hd_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(HttpError::Platform(error)),
    };
    let descriptor: HostRuntimeDescriptorV2 =
        serde_json::from_slice(&bytes).map_err(HttpError::Json)?;
    if descriptor.pid == pid && descriptor.process_start_marker == process_start_marker {
        std::fs::remove_file(&path).map_err(HttpError::Cleanup)?;
    }
    Ok(())
}

struct RuntimeDescriptorGuard {
    paths: hd_platform::DataPaths,
    pid: u32,
    process_start_marker: String,
}

impl Drop for RuntimeDescriptorGuard {
    fn drop(&mut self) {
        if let Err(error) =
            remove_runtime_descriptor_if_owned(&self.paths, self.pid, &self.process_start_marker)
        {
            tracing::warn!(
                event = "http.descriptor.cleanup.failed",
                %error,
                "failed to remove the owned host runtime descriptor"
            );
        }
    }
}

fn openapi_document(origin: &str) -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "HD Host API", "version": "2.0.0"},
        "servers": [{"url": origin}],
        "security": [{"bearerAuth": []}],
        "paths": openapi_paths(),
        "components": {
            "securitySchemes": {"bearerAuth": {"type": "http", "scheme": "bearer"}},
            "schemas": openapi_schemas()
        },
        "x-hd-security": {"bind": "127.0.0.1", "hostHeader": "exact", "origin": "exact-or-absent", "maxJsonBytes": JSON_BODY_LIMIT, "maxUploadBytes": MAX_HTTP_BODY_BYTES}
    })
}

fn openapi_paths() -> serde_json::Value {
    let id_parameter = serde_json::json!({
        "name": "id", "in": "path", "required": true,
        "schema": {"type": "string", "format": "uuid"}
    });
    serde_json::json!({
        "/v2/health": {"get": {
            "operationId": "health",
            "responses": {"200": json_response("healthy", "HealthResponseV2")}
        }},
        "/v2/capabilities": {"get": {
            "operationId": "capabilities",
            "parameters": [{"name": "instance_id", "in": "query", "required": false, "schema": {"type": "string", "format": "uuid"}}],
            "responses": {"200": json_response("host capabilities", "HostCapabilitiesV2")}
        }},
        "/v2/instances": {
            "get": {"operationId": "listInstances", "responses": {"200": json_array_response("instances", "InstanceRecordV2")}},
            "post": {"operationId": "createInstance", "requestBody": json_request("CreateInstanceRequestV2"), "responses": {"201": json_response("created", "InstanceRecordV2")}}
        },
        "/v2/instances/{id}": {
            "parameters": [id_parameter.clone()],
            "get": {"operationId": "getInstance", "responses": {"200": json_response("instance", "InstanceRecordV2")}},
            "patch": {"operationId": "updateInstance", "requestBody": json_request("UpdateInstanceRequestV2"), "responses": {"200": json_response("updated", "InstanceRecordV2")}}
        },
        "/v2/instances/{id}/operations": {
            "parameters": [id_parameter.clone()],
            "post": {
                "operationId": "createOperation",
                "parameters": [{"name": "Idempotency-Key", "in": "header", "required": true, "schema": {"type": "string", "minLength": 1, "maxLength": 128, "pattern": "^[A-Za-z0-9_.-]+$"}}],
                "requestBody": json_request("CreateOperationRequestV2"),
                "responses": {"202": json_response("accepted", "OperationRecordV2")}
            }
        },
        "/v2/instances/{id}/actions": {
            "parameters": [id_parameter.clone()],
            "post": {"operationId": "typedAction", "requestBody": json_request("ActionRequestV2"), "responses": {"200": json_response("applied and read back", "ActionResultV2")}}
        },
        "/v2/operations": {"get": {
            "operationId": "listOperations",
            "responses": {"200": json_array_response("operations", "OperationRecordV2")}
        }},
        "/v2/operations/{id}": {
            "parameters": [id_parameter],
            "get": {"operationId": "getOperation", "responses": {"200": json_response("operation", "OperationRecordV2")}}
        },
        "/v2/uploads/apk": {"post": {
            "operationId": "uploadApk",
            "parameters": [
                {"name": "X-File-Name", "in": "header", "required": true, "schema": {"type": "string", "minLength": 1, "maxLength": 128}},
                {"name": "X-Content-Sha256", "in": "header", "required": true, "schema": {"type": "string", "pattern": "^[0-9a-f]{64}$"}}
            ],
            "requestBody": {"required": true, "content": {"application/vnd.android.package-archive": {"schema": {"type": "string", "format": "binary"}}}},
            "responses": {"201": json_response("verified immutable upload", "UploadRecordV2")}
        }},
        "/v2/diagnostics": {"post": {
            "operationId": "collectDiagnostics",
            "requestBody": json_request("DiagnosticRequestV2"),
            "responses": {"201": json_response("redacted diagnostic bundle", "DiagnosticBundleResponseV2")}
        }},
        "/v2/events": {"get": {
            "operationId": "events",
            "responses": {"200": {"description": "SSE event stream", "content": {"text/event-stream": {"schema": {"type": "string"}}}}}
        }},
        "/v2/openapi.json": {"get": {
            "operationId": "openapi",
            "responses": {"200": {"description": "this document", "content": {"application/json": {"schema": {"type": "object"}}}}}
        }},
        "/v2/shutdown": {"post": {
            "operationId": "shutdown",
            "requestBody": json_request("ShutdownHostRequestV2"),
            "responses": {"202": {"description": "accepted"}}
        }}
    })
}

fn schema_reference(name: &str) -> serde_json::Value {
    serde_json::json!({"$ref": format!("#/components/schemas/{name}")})
}

fn json_request(name: &str) -> serde_json::Value {
    serde_json::json!({
        "required": true,
        "content": {"application/json": {"schema": schema_reference(name)}}
    })
}

fn json_response(description: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "description": description,
        "content": {"application/json": {"schema": schema_reference(name)}}
    })
}

fn json_array_response(description: &str, item: &str) -> serde_json::Value {
    serde_json::json!({
        "description": description,
        "content": {"application/json": {"schema": {"type": "array", "items": schema_reference(item)}}}
    })
}

fn openapi_schemas() -> serde_json::Value {
    serde_json::json!({
        "InstanceSpecV2": {
            "type": "object",
            "required": ["schema_version", "id", "name", "cpu_count", "memory_mib", "display", "adb", "boot", "devices", "restart_policy", "labels"],
            "properties": {
                "schema_version": {"type": "integer", "const": 2},
                "id": {"type": "string", "format": "uuid"},
                "name": {"type": "string", "minLength": 1, "maxLength": 80},
                "cpu_count": {"type": "integer", "minimum": 1, "maximum": 256},
                "memory_mib": {"type": "integer", "minimum": 2048, "maximum": 1_048_576},
                "display": schema_reference("DisplayConfigV2"),
                "adb": schema_reference("AdbConfigV2"),
                "boot": schema_reference("BootConfigV2"),
                "devices": schema_reference("DeviceConfigV2"),
                "restart_policy": {"type": "string", "enum": ["never", "on_failure"]},
                "artifacts": {"oneOf": [schema_reference("ArtifactSelectionV2"), {"type": "null"}]},
                "labels": {"type": "object", "maxProperties": 32, "additionalProperties": {"type": "string", "maxLength": 256}}
            },
            "additionalProperties": false
        },
        "DisplayConfigV2": object_with_properties(
            ["width", "height", "dpi", "refresh_rate_hz", "orientation", "vsync", "show_host_fps"],
            &serde_json::json!({
                "width": {"type": "integer", "minimum": 320, "maximum": 8192},
                "height": {"type": "integer", "minimum": 320, "maximum": 8192},
                "dpi": {"type": "integer", "minimum": 72, "maximum": 960},
                "refresh_rate_hz": {"type": "integer", "enum": [30, 60, 90, 120]},
                "orientation": {"type": "string", "enum": ["portrait", "landscape", "reverse_portrait", "reverse_landscape"]},
                "vsync": {"type": "string", "enum": ["on", "off"]},
                "show_host_fps": {"type": "boolean"}
            })
        ),
        "AdbConfigV2": object_with_properties(
            ["mode", "host_port", "executable"],
            &serde_json::json!({
                "mode": {"type": "string", "enum": ["disabled", "loopback"]},
                "host_port": {"type": ["integer", "null"], "minimum": 1, "maximum": 65535},
                "executable": {"type": ["string", "null"]}
            })
        ),
        "ArtifactSelectionV2": object_with_properties(
            ["store_root", "guest_bundle_digest", "host_bundle_digest"],
            &serde_json::json!({
                "store_root": {"type": "string", "minLength": 1},
                "guest_bundle_digest": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                "host_bundle_digest": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
            })
        ),
        "BootConfigV2": object_with_properties(
            ["kernel_log_level", "panic_timeout_seconds", "boot_animation"],
            &serde_json::json!({
                "kernel_log_level": {"type": "integer", "minimum": 0, "maximum": 7},
                "panic_timeout_seconds": {"type": "integer", "minimum": 0, "maximum": 300},
                "boot_animation": {"type": "boolean"}
            })
        ),
        "DeviceConfigV2": object_with_properties(
            ["bluetooth", "nfc", "uwb", "modem", "gnss", "sensors", "network", "audio", "camera", "power"],
            &serde_json::json!({
                "bluetooth": {"type": "boolean"}, "nfc": {"type": "boolean"},
                "uwb": {"type": "boolean"}, "modem": {"type": "boolean"},
                "gnss": {"type": "boolean"}, "sensors": {"type": "boolean"},
                "network": {"type": "boolean"}, "audio": {"type": "boolean"},
                "camera": {"type": "boolean"}, "power": {"type": "boolean"}
            })
        ),
        "CreateInstanceRequestV2": object_with_properties(["spec"], &serde_json::json!({"spec": schema_reference("InstanceSpecV2")})),
        "UpdateInstanceRequestV2": object_with_properties(
            ["expected_revision", "spec"],
            &serde_json::json!({"expected_revision": {"type": "integer", "minimum": 0}, "spec": schema_reference("InstanceSpecV2")})
        ),
        "CreateOperationRequestV2": object_with_properties(["operation"], &serde_json::json!({"operation": schema_reference("OperationKindV2")})),
        "ActionRequestV2": object_with_properties(["action"], &serde_json::json!({"action": schema_reference("InstanceActionV2")})),
        "DiagnosticRequestV2": object_with_properties(
            ["instance_id", "include_guest_logs"],
            &serde_json::json!({"instance_id": {"type": ["string", "null"], "format": "uuid"}, "include_guest_logs": {"type": "boolean"}})
        ),
        "ShutdownHostRequestV2": object_with_properties(["stop_all"], &serde_json::json!({"stop_all": {"type": "boolean"}})),
        "OperationKindV2": {
            "type": "object", "required": ["operation"],
            "properties": {"operation": {"type": "string", "enum": ["start", "stop", "restart", "pause", "resume", "reconfigure", "install_apk", "collect_diagnostics", "delete"]}, "parameters": {"type": "object"}},
            "additionalProperties": false
        },
        "InstanceActionV2": {
            "type": "object", "required": ["action", "parameters"],
            "properties": {"action": {"type": "string", "enum": ["key", "rotate", "set_location", "set_battery", "set_network_condition", "inject_sensor", "bluetooth_peer", "nfc_tag"]}, "parameters": {"type": "object"}},
            "additionalProperties": false
        },
        "ApiErrorV2": {
            "type": "object", "required": ["code", "message", "retryable", "details"],
            "properties": {"code": {"type": "string"}, "message": {"type": "string"}, "retryable": {"type": "boolean"}, "details": {"type": "object", "additionalProperties": {"type": "string"}}},
            "additionalProperties": false
        },
        "HealthResponseV2": {"type": "object"}, "HostCapabilitiesV2": {"type": "object"},
        "InstanceRecordV2": {"type": "object"}, "OperationRecordV2": {"type": "object"},
        "ActionResultV2": {"type": "object"}, "UploadRecordV2": {"type": "object"},
        "DiagnosticBundleResponseV2": {"type": "object"}
    })
}

fn object_with_properties<const N: usize>(
    required: [&str; N],
    properties: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "object", "required": required.as_slice(), "properties": properties,
        "additionalProperties": false
    })
}

type ApiResult<T> = Result<T, ApiHttpError>;

#[derive(Debug)]
struct ApiHttpError {
    status: StatusCode,
    error: ApiErrorV2,
}

impl ApiHttpError {
    fn input(code: &str, message: &str) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            error: ApiErrorV2::new(code, message),
        }
    }
}

impl From<HostError> for ApiHttpError {
    fn from(error: HostError) -> Self {
        let status = match &error {
            HostError::InstanceNotFound(_)
            | HostError::OperationNotFound(_)
            | HostError::UploadNotFound(_) => StatusCode::NOT_FOUND,
            HostError::Busy(_) | HostError::InstanceMismatch | HostError::UploadDigestMismatch => {
                StatusCode::CONFLICT
            }
            HostError::Store(
                StoreError::RevisionConflict { .. }
                | StoreError::AlreadyExists(_)
                | StoreError::IdempotencyConflict,
            ) => StatusCode::CONFLICT,
            HostError::CapabilityBlocked => StatusCode::PRECONDITION_FAILED,
            HostError::Store(StoreError::Config(_)) | HostError::Action(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            error: error.api_error(),
        }
    }
}

impl From<UploadError> for ApiHttpError {
    fn from(error: UploadError) -> Self {
        let status = match &error {
            UploadError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            UploadError::InvalidFileName
            | UploadError::InvalidDigest
            | UploadError::NotApk
            | UploadError::InvalidApkStructure(_)
            | UploadError::DigestMismatch { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            error: ApiErrorV2::new("upload_failed", error.to_string()),
        }
    }
}

impl IntoResponse for ApiHttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("failed to bind host HTTP listener: {0}")]
    Bind(std::io::Error),
    #[error("host listener unexpectedly resolved to non-loopback address {0}")]
    NonLoopback(std::net::SocketAddr),
    #[error("host HTTP server failed: {0}")]
    Serve(std::io::Error),
    #[error("host runtime JSON failed: {0}")]
    Json(serde_json::Error),
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error("failed to remove host runtime descriptor: {0}")]
    Cleanup(std::io::Error),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_declares_all_security_boundaries() {
        let document = openapi_document("http://127.0.0.1:1234");
        assert_eq!(document["x-hd-security"]["hostHeader"], "exact");
        assert!(document["paths"]["/v2/instances/{id}/operations"].is_object());
    }
}
