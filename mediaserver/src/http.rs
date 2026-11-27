//! The private API, on axum: a route per operation, and every request first through `guard` (at
//! most MAX_ACTIVE at once, the whole body read, the secret checked, the connection closed after
//! the response). A handler validates what it can alone (the path, the JSON, that the SDP parses)
//! and hands the rest to the network thread as one command, awaiting its reply. The JSON of every
//! body is here, as serde types.
//!
//!   POST   /calls                                 {"ufrags": ["<a>", "<b>"]} -> {"id", "candidate"}
//!   GET    /calls/{id}                            -> {"sides": [...], "idle_ms"}
//!   DELETE /calls/{id}
//!   POST   /rooms                                 -> {"id"}
//!   GET    /rooms/{id}                            -> {"participants": [...]}
//!   DELETE /rooms/{id}
//!   POST   /rooms/{id}/publish/{user}             offer SDP -> answer SDP
//!   POST   /rooms/{id}/subscribe/{user}/{other}   offer SDP -> answer SDP
//!   DELETE /rooms/{id}/participants/{user}
//!
//! Every error is `{"error": "..."}` with its status, the router's own (no such route, a method
//! the route does not take) included.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Bytes, HttpBody};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{delete, get, post};
use axum::Router;
use http_body_util::LengthLimitError;
use serde::{Deserialize, Serialize};
use serde_json::error::Category;
use str0m::change::SdpOffer;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::call::valid_ufrag;
use crate::net::{parse_id, ApiError, Body, Command, Handle, Id, Reply, Response};
use crate::room::valid_user;
use crate::Log;

const MAX_BODY: usize = 256 * 1024;
/// Requests at once; more get 503 (the API is the backend's, a few at a time).
const MAX_ACTIVE: usize = 64;
/// How long a body may take to arrive.
const BODY_WITHIN: Duration = Duration::from_secs(10);
/// How long a request waits for the network thread before it answers 503.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// POST /calls's body.
#[derive(Deserialize)]
struct CallRequest {
    ufrags: [String; 2],
}

/// 201 to POST /calls.
#[derive(Serialize)]
pub(crate) struct CallCreated<'a> {
    pub(crate) id: String,
    pub(crate) candidate: &'a str,
}

/// GET /calls/{id}.
#[derive(Serialize)]
pub(crate) struct CallInfo<'a> {
    pub(crate) sides: [SideInfo<'a>; 2],
    pub(crate) idle_ms: u64,
}

#[derive(Serialize)]
pub(crate) struct SideInfo<'a> {
    pub(crate) ufrag: &'a str,
    /// Latched.
    pub(crate) connected: bool,
    /// What came from it.
    pub(crate) packets: u64,
    pub(crate) bytes: u64,
}

/// 201 to POST /rooms.
#[derive(Serialize)]
pub(crate) struct RoomCreated {
    pub(crate) id: String,
}

/// GET /rooms/{id}.
#[derive(Serialize)]
pub(crate) struct RoomInfo<'a> {
    pub(crate) participants: Vec<ParticipantInfo<'a>>,
}

#[derive(Serialize)]
pub(crate) struct ParticipantInfo<'a> {
    pub(crate) id: &'a str,
    pub(crate) publishing: bool,
    /// One of its connections is.
    pub(crate) connected: bool,
    /// Who subscribes to it.
    pub(crate) subscribers: Vec<&'a str>,
    /// What crossed the UDP port from / to that phone, closed connections included.
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// A JSON answer of the network thread.
pub(crate) fn json(status: u16, value: &impl Serialize) -> Response {
    match serde_json::to_string(value) {
        Ok(text) => Ok((status, Body::Json(text))),
        Err(e) => Err(ApiError::new(500, format!("the answer does not serialize: {e}"))),
    }
}

struct Api {
    net: Arc<Handle>,
    secret: String,
    active: Semaphore,
}

/// Serves the API on `listener` until the runtime it runs on is shut down.
pub(crate) async fn serve(listener: TcpListener, net: Arc<Handle>, secret: String, log: Log) {
    let api = Arc::new(Api { net, secret, active: Semaphore::new(MAX_ACTIVE) });
    let routes = Router::new()
        .route("/calls", post(create_call))
        .route("/calls/{id}", get(get_call).delete(delete_call))
        .route("/rooms", post(create_room))
        .route("/rooms/{id}", get(get_room).delete(delete_room))
        .route("/rooms/{id}/publish/{user}", post(publish))
        .route("/rooms/{id}/subscribe/{user}/{other}", post(subscribe))
        .route("/rooms/{id}/participants/{user}", delete(delete_participant))
        .method_not_allowed_fallback(|| async { ApiError::new(405, "method not allowed") })
        .fallback(|| async { ApiError::new(404, "no such route") })
        .with_state(api.clone());
    // The guard in front of the routing, not inside each route: without the secret, nothing
    // tells the routes apart (not even a 405's Allow).
    let router = Router::new().fallback_service(routes).layer(middleware::from_fn_with_state(api, guard));
    if let Err(e) = axum::serve(listener, router).await {
        log_line!(log, ERROR, "http: {e}");
    }
}

/// Every request, before its route. The body is read whole first, as an answer must not leave
/// one unread on a connection it closes; one request per connection (`Connection: close`).
async fn guard(State(api): State<Arc<Api>>, request: Request, next: Next) -> HttpResponse {
    let mut response = match api.active.try_acquire() {
        Err(_) => ApiError::new(503, "too many requests at once").into_response(),
        Ok(_permit) => match api.admit(request).await {
            Ok(request) => next.run(request).await,
            Err(error) => error.into_response(),
        },
    };
    response.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

impl Api {
    async fn admit(&self, request: Request) -> Result<Request, ApiError> {
        let (parts, body) = request.into_parts();
        let too_large = || ApiError::new(413, "the body is too large");
        // A Content-Length over the limit is refused before a byte of the body is asked for.
        if body.size_hint().lower() > MAX_BODY as u64 {
            return Err(too_large());
        }
        let body = match timeout(BODY_WITHIN, to_bytes(body, MAX_BODY)).await {
            Ok(Ok(body)) => body,
            Ok(Err(e)) if std::error::Error::source(&e).is_some_and(|s| s.is::<LengthLimitError>()) => {
                return Err(too_large())
            }
            Ok(Err(_)) => return Err(ApiError::new(400, "the body could not be read")),
            Err(_) => return Err(ApiError::new(408, "the body did not arrive in time")),
        };
        if !self.authorized(parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())) {
            return Err(ApiError::new(401, "a valid Authorization: Bearer <secret> is required"));
        }
        Ok(Request::from_parts(parts, body.into()))
    }

    /// The comparison does not stop at the first wrong byte.
    fn authorized(&self, header: Option<&str>) -> bool {
        let Some(token) = header.and_then(|h| h.strip_prefix("Bearer ")) else { return false };
        let (a, b) = (token.trim().as_bytes(), self.secret.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    /// One command to the network thread, and its answer.
    async fn ask(&self, make: impl FnOnce(Reply) -> Command) -> Result<HttpResponse, ApiError> {
        let answer = self.net.ask(make).ok_or_else(|| ApiError::new(503, "the server is stopping"))?;
        let (status, body) = match timeout(ANSWER_WITHIN, answer).await {
            Ok(Ok(response)) => response?,
            // Its reply was dropped (it stopped), or it is stuck.
            _ => return Err(ApiError::new(503, "the network thread did not answer")),
        };
        let status = status_code(status);
        Ok(match body {
            Body::Json(json) => (status, [(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Body::Sdp(sdp) => (status, [(header::CONTENT_TYPE, "application/sdp")], sdp).into_response(),
            Body::Empty => status.into_response(),
        })
    }
}

type Answer = Result<HttpResponse, ApiError>;
type Shared = State<Arc<Api>>;

async fn create_call(State(api): Shared, body: Bytes) -> Answer {
    let ufrags = call_ufrags(&body)?;
    api.ask(|reply| Command::CreateCall { ufrags, reply }).await
}

async fn get_call(State(api): Shared, path: Result<Path<String>, PathRejection>) -> Answer {
    let id = id(&path?.0, "call")?;
    api.ask(|reply| Command::GetCall { id, reply }).await
}

async fn delete_call(State(api): Shared, path: Result<Path<String>, PathRejection>) -> Answer {
    let id = id(&path?.0, "call")?;
    api.ask(|reply| Command::DeleteCall { id, reply }).await
}

async fn create_room(State(api): Shared) -> Answer {
    api.ask(|reply| Command::CreateRoom { reply }).await
}

async fn get_room(State(api): Shared, path: Result<Path<String>, PathRejection>) -> Answer {
    let id = id(&path?.0, "room")?;
    api.ask(|reply| Command::GetRoom { id, reply }).await
}

async fn delete_room(State(api): Shared, path: Result<Path<String>, PathRejection>) -> Answer {
    let id = id(&path?.0, "room")?;
    api.ask(|reply| Command::DeleteRoom { id, reply }).await
}

async fn publish(State(api): Shared, path: Result<Path<(String, String)>, PathRejection>, body: Bytes) -> Answer {
    let Path((room, who)) = path?;
    let (room, user) = (id(&room, "room")?, user(who)?);
    let offer = offer(&body)?;
    api.ask(|reply| Command::Publish { room, user, offer, reply }).await
}

async fn subscribe(
    State(api): Shared,
    path: Result<Path<(String, String, String)>, PathRejection>,
    body: Bytes,
) -> Answer {
    let Path((room, who, other)) = path?;
    let (room, user, other) = (id(&room, "room")?, user(who)?, user(other)?);
    let offer = offer(&body)?;
    api.ask(|reply| Command::Subscribe { room, user, other, offer, reply }).await
}

async fn delete_participant(State(api): Shared, path: Result<Path<(String, String)>, PathRejection>) -> Answer {
    let Path((room, who)) = path?;
    let (room, user) = (id(&room, "room")?, user(who)?);
    api.ask(|reply| Command::DeleteParticipant { room, user, reply }).await
}

fn id(text: &str, what: &str) -> Result<Id, ApiError> {
    parse_id(text).ok_or_else(|| ApiError::new(404, format!("no such {what}")))
}

fn user(text: String) -> Result<String, ApiError> {
    if valid_user(&text) {
        Ok(text)
    } else {
        Err(ApiError::new(400, "a participant id is 1 to 64 of A-Z a-z 0-9 . _ ~ -"))
    }
}

fn call_ufrags(body: &[u8]) -> Result<[String; 2], ApiError> {
    let request: CallRequest = serde_json::from_slice(body).map_err(|e| match e.classify() {
        Category::Data => ApiError::new(400, "expected {\"ufrags\": [\"<a>\", \"<b>\"]}"),
        _ => ApiError::new(400, format!("malformed JSON: {e}")),
    })?;
    let [a, b] = request.ufrags;
    if !valid_ufrag(&a) || !valid_ufrag(&b) {
        return Err(ApiError::new(400, "a ufrag is 4 to 256 of A-Z a-z 0-9 + /"));
    }
    if a == b {
        return Err(ApiError::new(400, "the two ufrags are the same"));
    }
    Ok([a, b])
}

fn offer(body: &[u8]) -> Result<Box<SdpOffer>, ApiError> {
    let text = std::str::from_utf8(body).map_err(|_| ApiError::new(400, "the offer is not UTF-8"))?;
    let offer = SdpOffer::from_sdp_string(text).map_err(|e| ApiError::new(400, format!("bad SDP: {e}")))?;
    Ok(Box::new(offer))
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// A path segment that does not percent-decode to UTF-8 (the router's own rejection, as JSON).
impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> ApiError {
        ApiError::new(rejection.status().as_u16(), rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> HttpResponse {
        let body = serde_json::to_string(&ErrorBody { error: &self.message }).unwrap_or_default();
        let mut response =
            (status_code(self.status), [(header::CONTENT_TYPE, "application/json")], body).into_response();
        if self.status == 401 {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
