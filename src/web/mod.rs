//! Defines the Rojo web interface. This is what the Roblox Studio plugin
//! communicates with. Eventually, we'll make this API stable, produce better
//! documentation for it, and open it up for other consumers.

mod api;
mod assets;
pub mod interface;
pub(crate) mod model_check;
mod origin;
pub(crate) mod studio;
mod verify;
mod ui;
mod untrusted_model;
mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use hyper::{
    server::{conn::AddrStream, Server},
    service::{make_service_fn, service_fn},
    Body, Request,
};
use tokio::runtime::Runtime;

use crate::serve_session::ServeSession;

pub struct LiveServer {
    serve_session: Arc<ServeSession>,
    studio: Option<Arc<studio::StudioBridge>>,
}

impl LiveServer {
    pub fn new(serve_session: Arc<ServeSession>) -> Self {
        LiveServer {
            serve_session,
            studio: None,
        }
    }

    pub fn with_studio_token_file(
        mut self,
        path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<Self> {
        self.studio = Some(studio::StudioBridge::new(
            &self.serve_session,
            path.as_ref(),
        )?);
        Ok(self)
    }

    /// Starts the server on the given address, blocking until it stops.
    ///
    /// `allowed_hosts` are extra `Host`/`Origin` values to accept in addition to
    /// localhost and the bind address (see [`origin::allowed_hosts`]).
    ///
    /// `on_listening` is invoked once the server has successfully bound to the
    /// address, so callers can defer printing any "listening" message until
    /// after binding can no longer fail (e.g. due to the port being in use).
    pub fn start(
        self,
        address: SocketAddr,
        allowed_hosts: Vec<String>,
        on_listening: impl FnOnce(),
    ) -> anyhow::Result<()> {
        let serve_session = Arc::clone(&self.serve_session);
        let studio = self.studio.clone();
        let allowed_hosts = origin::allowed_hosts(address.ip(), address.port(), &allowed_hosts);

        let make_service = make_service_fn(move |conn: &AddrStream| {
            let serve_session = Arc::clone(&serve_session);
            let allowed_hosts = allowed_hosts.clone();
            let remote_addr = conn.remote_addr();
            let local_addr = conn.local_addr();
            let studio = studio.clone();

            async move {
                let service = move |mut req: Request<Body>| {
                    let serve_session = Arc::clone(&serve_session);
                    let allowed_hosts = allowed_hosts.clone();
                    let studio = studio.clone();
                    req.extensions_mut().insert(studio::LocalAddr(local_addr));

                    async move {
                        // Reject cross-origin requests before doing any work, to
                        // defend the local server against DNS rebinding.
                        if let Some(response) =
                            origin::check_request_origin(&req, allowed_hosts.as_ref())
                        {
                            // Deliberately not base64 encoded even when asked:
                            // the transfer header would identify a Rojo server
                            // to the prober this generic 404 is hiding from.
                            // The plugin treats an unencoded response that has
                            // headers as a server without base64 support and
                            // sends plain requests to that origin from then on.
                            return Ok::<_, Infallible>(response);
                        }

                        if req.uri().path().starts_with("/api/studio/") {
                            return Ok::<_, Infallible>(
                                api::with_base64_transfer(req, api::TransferLimits::STUDIO, |req| async move {
                                    match studio {
                                        Some(studio) => studio.call(remote_addr, req).await,
                                        None => util::json(
                                            serde_json::json!({"error":"Studio control is disabled. Start rojo serve with --studio-token-file."}),
                                            hyper::StatusCode::NOT_FOUND,
                                        ),
                                    }
                                })
                                .await,
                            );
                        }
                        if req.uri().path().starts_with("/api") {
                            Ok::<_, Infallible>(
                                api::call(serve_session, studio, remote_addr, req).await,
                            )
                        } else {
                            Ok::<_, Infallible>(ui::call(serve_session, req).await)
                        }
                    }
                };

                Ok::<_, Infallible>(service_fn(service))
            }
        });

        let rt = Runtime::new().context("Failed to start the async runtime for the web server")?;
        let _guard = rt.enter();
        let server = Server::try_bind(&address)
            .with_context(|| {
                format!(
                    "Could not start the Rojo server on {address}.\n\
                     The address may already be in use or reserved. Another Rojo server might already \
                     be running, or another program may be using that port.\n\
                     You can pick a different port with the --port option."
                )
            })?
            .serve(make_service);

        // Binding succeeded, so it's now safe to tell the user we're listening.
        on_listening();

        rt.block_on(server)
            .context("The Rojo web server encountered a fatal error")?;

        Ok(())
    }
}
