//! Defines Rojo's HTTP API, all under /api. These endpoints generally return
//! JSON.

use std::{collections::HashMap, fs, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use futures::{sink::SinkExt, stream::StreamExt};
use hyper::{
    body::{self, Bytes, HttpBody},
    Body, Method, Request, Response, StatusCode,
};
use hyper_tungstenite::{
    is_upgrade_request,
    tungstenite::{protocol::frame::coding::CloseCode, protocol::CloseFrame, Message},
    upgrade, HyperWebsocket,
};
use opener::OpenError;
use rbx_dom_weak::{
    types::{Ref, Variant},
    InstanceBuilder, UstrMap, WeakDom,
};

use crate::{
    change_processor::PublishedPatch,
    message_queue::MessageQueue,
    serve_session::ServeSession,
    snapshot::{InstanceWithMeta, PatchSet, PatchUpdate},
    web::{
        interface::{
            ErrorResponse, Instance, MessagesPacket, OpenResponse, ReadResponse,
            ServerInfoResponse, SocketPacket, SocketPacketBody, SocketPacketType, SubscribeMessage,
            WriteRequest, WriteResponse, PROTOCOL_VERSION, SERVER_VERSION,
        },
        origin::canonical,
        util::{deserialize_msgpack, msgpack, msgpack_ok, serialize_msgpack},
    },
    web_api::{
        InstanceUpdate, RefPatchRequest, RefPatchResponse, SerializeRequest, SerializeResponse,
    },
};

/// How long `/api/read` keeps the history after its cursor, so a client that
/// waits for the user to confirm the initial sync can still subscribe.
const READ_CURSOR_GRACE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub async fn call(
    serve_session: Arc<ServeSession>,
    studio: Option<Arc<super::studio::StudioBridge>>,
    remote_addr: SocketAddr,
    request: Request<Body>,
) -> Response<Body> {
    with_base64_transfer(request, TransferLimits::API, |request| {
        call_guarded(serve_session, studio, remote_addr, request)
    })
    .await
}

/// Whether a request opted into base64 bodies with `x-rojo-content-transfer`.
pub(super) fn wants_base64(request: &Request<Body>) -> bool {
    request
        .headers()
        .get("x-rojo-content-transfer")
        .is_some_and(|value| value == "base64")
}

/// Bounds on an encoded request body, which is read before the handler (and
/// any authentication it does) runs.
pub(super) struct TransferLimits {
    max_body: usize,
    timeout: Option<std::time::Duration>,
}

impl TransferLimits {
    const API: Self = Self {
        max_body: MAX_REQUEST_BODY,
        timeout: None,
    };

    /// Studio control bodies are at most 8 MiB once decoded, and the bridge
    /// reads its own bodies with a 10-second timeout. Reading an encoded body
    /// happens before the bridge checks the token, so apply both here too.
    pub(super) const STUDIO: Self = Self {
        max_body: super::studio::MAX_BODY.div_ceil(3) * 4 + 4096,
        timeout: Some(std::time::Duration::from_secs(10)),
    };
}

/// Decodes an opted-in request body, runs `handler`, and encodes its response.
/// Requests without the header are passed through unchanged.
pub(super) async fn with_base64_transfer<F, Fut>(
    request: Request<Body>,
    limits: TransferLimits,
    handler: F,
) -> Response<Body>
where
    F: FnOnce(Request<Body>) -> Fut,
    Fut: std::future::Future<Output = Response<Body>>,
{
    if !wants_base64(&request) {
        return handler(request).await;
    }
    // Studio RawStream truncates binary bodies at NUL. Opt in per request so
    // existing clients retain the original MessagePack wire format. Every
    // response to an opted-in request is encoded, errors included, because the
    // client decodes the body before inspecting its status.
    if is_upgrade_request(&request) {
        return encode_base64(msgpack(
            ErrorResponse::bad_request("Base64 transfer is only for finite HTTP requests"),
            StatusCode::BAD_REQUEST,
        ))
        .await;
    }
    let (parts, body) = request.into_parts();
    let read = read_body_within(body, limits.max_body);
    let read = match limits.timeout {
        Some(limit) => tokio::time::timeout(limit, read).await.unwrap_or_else(|_| {
            Err(Box::new(msgpack(
                ErrorResponse::bad_request("Request body timed out"),
                StatusCode::REQUEST_TIMEOUT,
            )))
        }),
        None => read.await,
    };
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(response) => return encode_base64(*response).await,
    };
    let decoded = match data_encoding::BASE64.decode(&bytes) {
        Ok(bytes) => bytes,
        Err(_) => {
            return encode_base64(msgpack(
                ErrorResponse::bad_request("Invalid base64 request body"),
                StatusCode::BAD_REQUEST,
            ))
            .await
        }
    };
    let response = handler(Request::from_parts(parts, Body::from(decoded))).await;
    encode_base64(response).await
}

fn base64_response(mut parts: hyper::http::response::Parts, bytes: &[u8]) -> Response<Body> {
    parts.headers.remove(hyper::header::CONTENT_LENGTH);
    parts.headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain"),
    );
    parts.headers.insert(
        "x-rojo-content-transfer",
        hyper::header::HeaderValue::from_static("base64"),
    );
    Response::from_parts(parts, Body::from(data_encoding::BASE64.encode(bytes)))
}

pub(super) async fn encode_base64(response: Response<Body>) -> Response<Body> {
    let (parts, body) = response.into_parts();
    match body::to_bytes(body).await {
        Ok(bytes) => base64_response(parts, &bytes),
        Err(_) => {
            let (parts, _) = msgpack((), StatusCode::INTERNAL_SERVER_ERROR).into_parts();
            let error = serialize_msgpack(ErrorResponse::internal_error(
                "Could not encode response body",
            ))
            .unwrap_or_default();
            base64_response(parts, &error)
        }
    }
}

/// Waits out Git index transactions and rejects requests once the workspace
/// was switched to another checkout, so Studio cannot sync against it.
async fn call_guarded(
    serve_session: Arc<ServeSession>,
    studio: Option<Arc<super::studio::StudioBridge>>,
    remote_addr: SocketAddr,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(studio) = &studio {
        studio.wait_for_git().await;
        if !studio.identity_matches().await {
            return msgpack(
                ErrorResponse::bad_request(super::studio::IDENTITY_CHANGED),
                StatusCode::CONFLICT,
            );
        }
    }
    call_inner(serve_session, studio, remote_addr, request).await
}

async fn call_inner(
    serve_session: Arc<ServeSession>,
    studio: Option<Arc<super::studio::StudioBridge>>,
    remote_addr: SocketAddr,
    mut request: Request<Body>,
) -> Response<Body> {
    let service = ApiService::new(serve_session, studio, remote_addr);

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/api/rojo") => {
            // Only this route reveals pairing, and deciding it can read the
            // system's socket table, so other routes skip it.
            let pairing = match &service.studio {
                Some(studio) => studio.pairing(remote_addr, &request).await,
                None => None,
            };
            service.handle_api_rojo(pairing).await
        }
        (&Method::GET, path) if path.starts_with("/api/read/") => {
            service.handle_api_read(request).await
        }
        (&Method::GET, path) if path.starts_with("/api/socket/") => {
            if is_upgrade_request(&request) {
                service.handle_api_socket(&mut request).await
            } else {
                msgpack(
                    ErrorResponse::bad_request(
                        "/api/socket must be called as a websocket upgrade request",
                    ),
                    StatusCode::BAD_REQUEST,
                )
            }
        }
        (&Method::POST, "/api/serialize") => service.handle_api_serialize(request).await,
        (&Method::POST, "/api/ref-patch") => service.handle_api_ref_patch(request).await,

        (&Method::POST, path) if path.starts_with("/api/open/") => {
            service.handle_api_open(request).await
        }
        (&Method::POST, "/api/write") => service.handle_api_write(request).await,

        (_method, path) => msgpack(
            ErrorResponse::not_found(format!("Route not found: {}", path)),
            StatusCode::NOT_FOUND,
        ),
    }
}

pub struct ApiService {
    serve_session: Arc<ServeSession>,
    remote_addr: SocketAddr,
    studio: Option<Arc<super::studio::StudioBridge>>,
}

impl ApiService {
    pub fn new(
        serve_session: Arc<ServeSession>,
        studio: Option<Arc<super::studio::StudioBridge>>,
        remote_addr: SocketAddr,
    ) -> Self {
        ApiService {
            serve_session,
            remote_addr,
            studio,
        }
    }

    /// Get a summary of information about the server
    async fn handle_api_rojo(&self, pairing: Option<serde_json::Value>) -> Response<Body> {
        let tree = self.serve_session.tree();
        let root_instance_id = tree.get_root_id();

        msgpack_ok(&ServerInfoResponse {
            studio_bridge: pairing,
            server_version: SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            session_id: self.serve_session.session_id(),
            project_name: self.serve_session.project_name().to_owned(),
            expected_place_ids: self.serve_session.serve_place_ids().cloned(),
            unexpected_place_ids: self.serve_session.blocked_place_ids().cloned(),
            place_id: self.serve_session.place_id(),
            game_id: self.serve_session.game_id(),
            root_instance_id,
        })
    }

    /// Handle WebSocket upgrade for real-time message streaming
    async fn handle_api_socket(&self, request: &mut Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/socket/".len()..];
        let input_cursor: u32 = match argument.parse() {
            Ok(v) => v,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Malformed message cursor: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Clients may name the session they read from so that a socket opened
        // against a restarted server is rejected even when the cursor happens
        // to be valid there.
        let expected_session_id = request.uri().query().and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("sessionId=").map(percent_decode))
        });

        // Upgrade the connection to WebSocket
        let (response, websocket) = match upgrade(request, None) {
            Ok(result) => result,
            Err(err) => {
                return msgpack(
                    ErrorResponse::internal_error(format!("WebSocket upgrade failed: {}", err)),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        let serve_session = Arc::clone(&self.serve_session);
        let studio = self.studio.clone();

        // Spawn a task to handle the WebSocket connection
        tokio::spawn(async move {
            if let Err(e) = handle_websocket_subscription(
                serve_session,
                studio,
                websocket,
                input_cursor,
                expected_session_id,
            )
            .await
            {
                log::error!("Error in websocket subscription: {}", e);
            }
        });

        response
    }

    async fn handle_api_write(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let tree_mutation_sender = self.serve_session.tree_mutation_sender();

        let body = match read_request_body(request.into_body()).await {
            Ok(body) => body,
            Err(response) => return *response,
        };

        let request: WriteRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let updated_instances = request
            .updated
            .into_iter()
            .map(|update| PatchUpdate {
                id: update.id,
                changed_class_name: update.changed_class_name,
                changed_name: update.changed_name,
                changed_properties: update.changed_properties,
                changed_metadata: None,
            })
            .collect();

        tree_mutation_sender
            .send(PatchSet {
                removed_instances: Vec::new(),
                added_instances: Vec::new(),
                updated_instances,
            })
            .unwrap();

        msgpack_ok(WriteResponse { session_id })
    }

    async fn handle_api_read(&self, request: Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/read/".len()..];
        let requested_ids: Result<Vec<Ref>, _> = argument.split(',').map(Ref::from_str).collect();

        let requested_ids = match requested_ids {
            Ok(ids) => ids,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Malformed ID list"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let message_queue = self.serve_session.message_queue();
        let tree = self.serve_session.tree();
        let message_cursor = message_queue.cursor_held_for(READ_CURSOR_GRACE);

        let mut instances = HashMap::new();

        for id in requested_ids {
            if let Some(instance) = tree.get_instance(id) {
                instances.insert(id, Instance::from_rojo_instance(instance));

                for descendant in tree.descendants(id) {
                    instances.insert(descendant.id(), Instance::from_rojo_instance(descendant));
                }
            }
        }

        msgpack_ok(ReadResponse {
            session_id: self.serve_session.session_id(),
            message_cursor,
            instances,
        })
    }

    /// Accepts a list of IDs and returns them serialized as a binary model.
    /// The model is sent in a schema that causes Roblox to deserialize it as
    /// a Luau `buffer`.
    ///
    /// The returned model is a folder that contains ObjectValues with names
    /// that correspond to the requested Instances. These values have their
    /// `Value` property set to point to the requested Instance. Requested IDs
    /// that are no longer in the tree are left out of the model and listed in
    /// `missingIds` instead.
    async fn handle_api_serialize(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = match read_request_body(request.into_body()).await {
            Ok(body) => body,
            Err(response) => return *response,
        };

        let request: SerializeRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut response_dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let mut missing_ids = Vec::new();

        let tree = self.serve_session.tree();
        for id in &request.ids {
            // The fallback batches every instance a patch failed to apply, and
            // a later patch may already have removed some of them. Skipping
            // those keeps the rest of the batch usable.
            let Some(instance) = tree.get_instance(*id) else {
                missing_ids.push(*id);
                continue;
            };
            let clone = response_dom.insert(
                Ref::none(),
                InstanceBuilder::new(instance.class_name())
                    .with_name(instance.name())
                    .with_properties(instance.properties().clone()),
            );
            let object_value = response_dom.insert(
                response_dom.root_ref(),
                InstanceBuilder::new("ObjectValue")
                    .with_name(id.to_string())
                    .with_property("Value", clone),
            );

            let mut child_ref = clone;
            if let Some(parent_class) = parent_requirements(&instance.class_name()) {
                child_ref = response_dom.insert(object_value, InstanceBuilder::new(parent_class));
                response_dom.transfer_within(clone, child_ref);
            }

            response_dom.transfer_within(child_ref, object_value);
        }
        drop(tree);

        let mut source = Vec::new();
        rbx_binary::to_writer(&mut source, &response_dom, &[response_dom.root_ref()]).unwrap();

        msgpack_ok(SerializeResponse {
            session_id: self.serve_session.session_id(),
            model_contents: source,
            missing_ids,
        })
    }

    /// Returns a list of all referent properties that point towards the
    /// provided IDs. Used because the plugin does not store a RojoTree,
    /// and referent properties need to be updated after the serialize
    /// endpoint is used.
    async fn handle_api_ref_patch(self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = match read_request_body(request.into_body()).await {
            Ok(body) => body,
            Err(response) => return *response,
        };

        let request: RefPatchRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut instance_updates: HashMap<Ref, InstanceUpdate> = HashMap::new();

        let tree = self.serve_session.tree();
        for instance in tree.descendants(tree.get_root_id()) {
            for (prop_name, prop_value) in instance.properties() {
                let Variant::Ref(prop_value) = prop_value else {
                    continue;
                };
                if let Some(target_id) = request.ids.get(prop_value) {
                    let instance_id = instance.id();
                    let update =
                        instance_updates
                            .entry(instance_id)
                            .or_insert_with(|| InstanceUpdate {
                                id: instance_id,
                                changed_class_name: None,
                                changed_name: None,
                                changed_metadata: None,
                                changed_properties: UstrMap::default(),
                            });
                    update
                        .changed_properties
                        .insert(*prop_name, Some(Variant::Ref(*target_id)));
                }
            }
        }

        msgpack_ok(RefPatchResponse {
            session_id: self.serve_session.session_id(),
            patch: SubscribeMessage {
                added: HashMap::new(),
                removed: Vec::new(),
                updated: instance_updates.into_values().collect(),
            },
        })
    }

    /// Open a script with the given ID in the user's default text editor.
    async fn handle_api_open(&self, request: Request<Body>) -> Response<Body> {
        // Opening a file launches a local program, so it must never be reachable
        // by a remote client even when the server is bound to an exposed address.
        //
        // `remote_addr` is the immediate peer, which is the best locality signal
        // we have: the legitimate caller is a sandboxed Roblox plugin whose only
        // credential is being able to reach the port, so there is no secret to
        // authenticate it with. A connection forwarded over loopback by an
        // SSH/Tailscale tunnel or a local reverse proxy therefore appears local
        // and is allowed. That is delegated trust rather than a bypass: by
        // standing up that tunnel or proxy the user has decided the remote end is
        // trusted, and reachability is bounded by that hop's own authentication
        // (e.g. SSH keys or Tailscale ACLs). This gate only stops direct,
        // unauthenticated peers.
        //
        // An IPv4 client reaching a dual-stack (`::`) bind appears as an
        // IPv4-mapped IPv6 peer (`::ffff:127.0.0.1`), so canonicalize to the bare
        // IPv4 form before the loopback test, matching `origin`'s handling.
        if !canonical(self.remote_addr.ip()).is_loopback() {
            return msgpack(
                ErrorResponse::forbidden("/api/open is only available to local clients"),
                StatusCode::FORBIDDEN,
            );
        }

        let argument = &request.uri().path()["/api/open/".len()..];
        let requested_id = match Ref::from_str(argument) {
            Ok(id) => id,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Invalid instance ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let tree = self.serve_session.tree();

        let instance = match tree.get_instance(requested_id) {
            Some(instance) => instance,
            None => {
                return msgpack(
                    ErrorResponse::bad_request("Instance not found"),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        let script_path = match pick_script_path(instance) {
            Some(path) => path,
            None => {
                return msgpack(
                    ErrorResponse::bad_request(
                        "No appropriate file could be found to open this script",
                    ),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        match opener::open(&script_path) {
            Ok(()) => {}
            Err(error) => match error {
                OpenError::Io(io_error) => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            "Attempting to open {} failed because of the following io error: {}",
                            script_path.display(),
                            io_error
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
                OpenError::ExitStatus {
                    cmd,
                    status,
                    stderr,
                } => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            r#"The command '{}' to open '{}' failed with the error code '{}'.
                            Error logs:
                            {}"#,
                            cmd,
                            script_path.display(),
                            status,
                            stderr
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
            },
        };

        msgpack_ok(OpenResponse {
            session_id: self.serve_session.session_id(),
        })
    }
}

/// Requests larger than this are rejected rather than buffered in memory.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

async fn read_request_body(body: Body) -> Result<Bytes, Box<Response<Body>>> {
    read_body_within(body, MAX_REQUEST_BODY).await
}

async fn read_body_within(mut body: Body, max: usize) -> Result<Bytes, Box<Response<Body>>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|err| {
            Box::new(msgpack(
                ErrorResponse::bad_request(format!("Failed to read request body: {}", err)),
                StatusCode::BAD_REQUEST,
            ))
        })?;
        if bytes.len() + chunk.len() > max {
            return Err(Box::new(msgpack(
                ErrorResponse::bad_request(format!(
                    "Request body exceeds {} MiB",
                    max.div_ceil(1024 * 1024)
                )),
                StatusCode::PAYLOAD_TOO_LARGE,
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(bytes))
}

/// If this instance is represented by a script, try to find the correct .lua or .luau
/// file to open to edit it.
fn pick_script_path(instance: InstanceWithMeta<'_>) -> Option<PathBuf> {
    match instance.class_name().as_str() {
        "Script" | "LocalScript" | "ModuleScript" => {}
        _ => return None,
    }

    // Pick the first listed relevant path that has an extension of .lua or .luau that
    // exists.
    instance
        .metadata()
        .relevant_paths
        .iter()
        .find(|path| {
            // We should only ever open Lua or Luau files to be safe.
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("lua") => {}
                Some("luau") => {}
                _ => return false,
            }

            fs::metadata(path)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        })
        .map(|path| path.to_owned())
}

/// Subscribes to messages after `cursor`, or returns the close frame asking
/// the client to resync when those messages were trimmed from history.
fn subscribe_or_resync(
    message_queue: &MessageQueue<PublishedPatch>,
    cursor: u32,
) -> Result<futures::channel::oneshot::Receiver<(u32, Vec<PublishedPatch>)>, CloseFrame<'static>> {
    message_queue.subscribe(cursor).map_err(|stale| {
        // The changes this client missed cannot be replayed. The plugin ends
        // the session with this reason, and reconnecting reads the current
        // tree, which resyncs it.
        log::warn!("Closing a subscription that fell behind: {stale}");
        CloseFrame {
            code: CloseCode::Restart,
            reason: "Subscription history is stale; reconnect to resync".into(),
        }
    })
}

/// How long one WebSocket message may take to send. A client that stops
/// reading fills the connection's buffers, and its subscription must not wait
/// on it forever while holding the history it has not received.
const SOCKET_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Sends a message, giving up after [`SOCKET_SEND_TIMEOUT`]. Returns whether
/// it was sent.
async fn send_bounded<S>(socket: &mut S, message: Message) -> bool
where
    S: futures::Sink<Message> + Unpin,
{
    send_within(socket, message, SOCKET_SEND_TIMEOUT).await
}

async fn send_within<S>(socket: &mut S, message: Message, limit: std::time::Duration) -> bool
where
    S: futures::Sink<Message> + Unpin,
{
    matches!(
        tokio::time::timeout(limit, socket.send(message)).await,
        Ok(Ok(()))
    )
}

/// Handle WebSocket connection for streaming subscription messages
async fn handle_websocket_subscription(
    serve_session: Arc<ServeSession>,
    studio: Option<Arc<super::studio::StudioBridge>>,
    websocket: HyperWebsocket,
    input_cursor: u32,
    expected_session_id: Option<String>,
) -> anyhow::Result<()> {
    let mut websocket = websocket.await?;

    let session_id = serve_session.session_id();
    let message_queue = serve_session.message_queue();

    // A cursor past the end of the queue was issued by a different server run,
    // for example when the server restarted between the client's read and this
    // subscription. Waiting for the queue to catch up would silently skip the
    // first changes made on this server, so the client is told to resync. The
    // queue only grows, so checking once here is enough.
    let session_changed = expected_session_id
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&session_id.to_string()));
    if session_changed || input_cursor > message_queue.cursor() {
        log::debug!(
            "Rejecting WebSocket subscription from a previous server session (cursor {input_cursor})"
        );
        send_bounded(
            &mut websocket,
            Message::Close(Some(CloseFrame {
                code: CloseCode::Restart,
                reason: SESSION_CHANGED_CLOSE_REASON.into(),
            })),
        )
        .await;
        return Ok(());
    }

    log::debug!(
        "WebSocket subscription established for session {}",
        session_id
    );

    // Now continuously listen for new messages using select to handle both incoming messages
    // and WebSocket control messages concurrently
    let mut cursor = input_cursor;
    // The subscription is not registered while a batch waits for Git, which
    // can take seconds during a large checkout or merge. Holding the cursor
    // keeps the queue from trimming the history this socket still needs.
    let hold = message_queue.hold(cursor);
    loop {
        let receiver = match subscribe_or_resync(message_queue, cursor) {
            Ok(receiver) => receiver,
            Err(close) => {
                send_bounded(&mut websocket, Message::Close(Some(close))).await;
                break;
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)), if studio.is_some() => {
                if let Some(studio) = &studio {
                    if studio.git_busy().await || studio.identity_matches().await {
                        continue;
                    }
                    send_bounded(&mut websocket, Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: super::studio::IDENTITY_CHANGED.into() }))).await;
                    break;
                }
            }
            // Handle new messages from the message queue
            result = receiver => {
                match result {
                    Ok((new_cursor, messages)) => {
                        if let Some(studio) = &studio {
                            // Hold these patches until Git finishes (bounded for stale
                            // locks), so the branch identity is checked before sending.
                            studio.wait_for_git().await;
                            if !studio.identity_matches().await {
                                send_bounded(&mut websocket, Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: super::studio::IDENTITY_CHANGED.into() }))).await;
                                break;
                            }
                        }
                        if !messages.is_empty() {
                            // Patches that only touched data the plugin never
                            // sees (such as relevant paths) would make it open an
                            // undo recording and run callbacks for nothing. The
                            // packet is still sent so its cursor advances.
                            let messages: Vec<_> = messages
                                .into_iter()
                                .map(|patch| patch.message)
                                .filter(|message| !message.is_empty())
                                .collect();
                            let msgpack_message = serialize_msgpack(SocketPacket {
                                session_id,
                                packet_type: SocketPacketType::Messages,
                                body: SocketPacketBody::Messages(MessagesPacket {
                                    message_cursor: new_cursor,
                                    messages,
                                }),
                            });

                            let msgpack_message = match msgpack_message {
                                Ok(message) => message,
                                Err(error) => {
                                    // Last resort: the batch could not be encoded, so the
                                    // plugin would silently fall out of sync. Ask it to
                                    // reconnect, which resyncs from a fresh snapshot.
                                    send_bounded(&mut websocket, Message::Close(Some(CloseFrame {
                                        code: CloseCode::Restart,
                                        reason: "Failed to encode subscription messages; reconnect to resync".into(),
                                    }))).await;
                                    return Err(error);
                                }
                            };

                            log::debug!("Sending batch of messages over WebSocket subscription");

                            if !send_bounded(&mut websocket, Message::Binary(msgpack_message)).await {
                                // The client disconnected or stopped reading.
                                // Dropping the socket also releases its
                                // history hold, which would otherwise keep
                                // the queue from trimming for every client.
                                log::debug!("WebSocket subscription closed or stopped reading");
                                break;
                            }
                            cursor = new_cursor;
                            hold.advance(cursor);
                        }
                    }
                    Err(_) => {
                        // Message queue disconnected
                        log::debug!("Message queue disconnected; closing WebSocket subscription");
                        send_bounded(&mut websocket, Message::Close(None)).await;
                        break;
                    }
                }
            }

            // Handle incoming WebSocket messages (ping/pong/close)
            msg = websocket.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) => {
                        log::debug!("WebSocket subscription closed by client");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // tungstenite handles pong automatically
                        log::debug!("Received ping: {:?}", data);
                    }
                    Some(Ok(Message::Pong(data))) => {
                        log::debug!("Received pong: {:?}", data);
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        // Ignore text/binary messages from client for subscription endpoint
                        // TODO: Use this for bidirectional sync or requesting fallbacks?
                        log::debug!("Ignoring message from client since we don't use it for anything yet: {:?}", msg);
                    }
                    Some(Ok(Message::Frame(_))) => {
                        // This should never happen according to tungstenite docs
                        unreachable!();
                    }
                    Some(Err(e)) => {
                        log::error!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        // WebSocket stream ended
                        log::debug!("WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Close reason sent when a subscription belongs to a previous server run. The
/// plugin shows it after explaining that the connection closed and needs a
/// reconnect, so it only has to say why.
const SESSION_CHANGED_CLOSE_REASON: &str = "The Rojo server restarted since this session started.";

/// Certain Instances MUST be a child of specific classes. This function
/// tracks that information for the Serialize endpoint.
///
/// If a parent requirement exists, it will be returned.
/// Otherwise returns `None`.
fn parent_requirements(class: &str) -> Option<&str> {
    Some(match class {
        "Attachment" | "Bone" => "Part",
        "Animator" => "Humanoid",
        "BaseWrap" | "WrapLayer" | "WrapTarget" | "WrapDeformer" => "MeshPart",
        _ => return None,
    })
}

/// Decodes `%XX` escapes in a query value. Clients may percent-encode the
/// session ID (Roblox's `HttpService:UrlEncode` escapes `-`), and comparing the
/// raw text would reject every such connection. Invalid escapes are kept as-is.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escape {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_encoded_session_ids() {
        assert_eq!(percent_decode("1d4b%2Dee43%2d9c"), "1d4b-ee43-9c");
        assert_eq!(percent_decode("plain-id"), "plain-id");
        assert_eq!(percent_decode("bad%zzend%2"), "bad%zzend%2");
    }

    /// A socket whose peer stopped reading: sends never complete.
    struct Stalled;

    impl futures::Sink<Message> for Stalled {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn sends_to_a_client_that_stopped_reading_give_up() {
        let limit = std::time::Duration::from_millis(50);
        let started = std::time::Instant::now();
        assert!(!send_within(&mut Stalled, Message::Binary(vec![0; 16]), limit).await);
        assert!(started.elapsed() >= limit);
        let mut sent = Vec::new();
        assert!(send_within(&mut sent, Message::Binary(vec![1]), limit).await);
        assert_eq!(sent, vec![Message::Binary(vec![1])]);
    }
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    enum ApiErrorKind {
        BadRequest,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ApiError {
        kind: ApiErrorKind,
        details: String,
    }

    #[test]
    fn subscriptions_older_than_the_retained_history_must_resync() {
        use crate::snapshot::{AppliedPatchSet, InstanceSnapshot, RojoTree};

        let tree = RojoTree::new(InstanceSnapshot::new().name("ROOT").class_name("ROOT"));
        let queue = MessageQueue::with_limits(1, usize::MAX);
        let patch = || {
            let mut patch = AppliedPatchSet::new();
            patch.removed.push(Ref::new());
            PublishedPatch::new(&tree, patch)
        };
        queue.push_messages(&[patch(), patch()]);

        let close = subscribe_or_resync(&queue, 0).unwrap_err();
        assert_eq!(close.code, CloseCode::Restart);
        assert!(close.reason.contains("resync"));

        let (cursor, messages) = subscribe_or_resync(&queue, 1)
            .unwrap()
            .try_recv()
            .unwrap()
            .unwrap();
        assert_eq!(cursor, 2);
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn read_request_body_returns_body_bytes() {
        let body = read_request_body(Body::from("hello")).await.unwrap();

        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn read_request_body_rejects_oversized_bodies() {
        let response = read_request_body(Body::from(vec![0; MAX_REQUEST_BODY + 1]))
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn base64_errors_keep_the_transfer_encoding() {
        let response = encode_base64(msgpack(
            ErrorResponse::bad_request("Invalid base64 request body"),
            StatusCode::BAD_REQUEST,
        ))
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-rojo-content-transfer"], "base64");
        let body = body::to_bytes(response.into_body()).await.unwrap();
        let decoded = data_encoding::BASE64.decode(&body).unwrap();
        let error: ApiError = deserialize_msgpack(&decoded).unwrap();
        assert_eq!(error.kind, ApiErrorKind::BadRequest);
    }

    #[tokio::test]
    async fn read_request_body_returns_structured_bad_request_for_body_errors() {
        let (sender, body) = Body::channel();
        sender.abort();

        let response = read_request_body(body).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = body::to_bytes(response.into_body()).await.unwrap();
        let error: ApiError = deserialize_msgpack(&body).unwrap();
        assert_eq!(error.kind, ApiErrorKind::BadRequest);
        assert!(error.details.contains("Failed to read request body"));
    }
}
