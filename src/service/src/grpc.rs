// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! gRPC transport for the [client](crate::client) module.

use async_stream::stream;
use async_trait::async_trait;
use futures::future::{self, BoxFuture};
use futures::stream::{Stream, StreamExt, TryStreamExt};
use http::uri::PathAndQuery;
use hyper_util::rt::TokioIo;
use mz_ore::metric;
use mz_ore::metrics::{DeleteOnDropGauge, MetricsRegistry, UIntGaugeVec};
use mz_ore::netio::{Listener, SocketAddr, SocketAddrType};
use mz_proto::{ProtoType, RustType};
use prometheus::core::AtomicU64;
use semver::Version;
use std::error::Error;
use std::fmt::{self, Debug};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::net::UnixStream;
use tokio::select;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::{oneshot, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::body::BoxBody;
use tonic::codegen::InterceptedService;
use tonic::metadata::AsciiMetadataValue;
use tonic::server::NamedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{IntoStreamingRequest, Request, Response, Status, Streaming};
use tower::Service;
use tracing::{debug, error, info, warn};

use crate::client::{GenericClient, Partitionable, Partitioned};
use crate::codec::{StatCodec, StatsCollector};
use crate::params::GrpcClientParameters;

include!(concat!(env!("OUT_DIR"), "/mz_service.params.rs"));

// Use with generated servers Server::new(Svc).max_decoding_message_size
pub const MAX_GRPC_MESSAGE_SIZE: usize = usize::MAX;

pub type ClientTransport = InterceptedService<Channel, VersionAttachInterceptor>;

/// Types that we send and receive over a service endpoint.
pub trait ProtoServiceTypes: Debug + Clone + Send {
    type PC: prost::Message + Clone + 'static;
    type PR: prost::Message + Clone + Default + 'static;
    type STATS: StatsCollector<Self::PC, Self::PR> + 'static;
    const URL: &'static str;
}

/// A client to a remote dataflow server using gRPC and protobuf based
/// communication.
///
/// The client opens a connection using the proto client stubs that are
/// generated by tonic from a service definition. When the client is connected,
/// it will call automatically the only RPC defined in the service description,
/// encapsulated by the `BidiProtoClient` trait. This trait bound is not on the
/// `Client` type parameter here, but it IS on the impl blocks. Bidirectional
/// protobuf RPC sets up two streams that persist after the RPC has returned: A
/// Request (Command) stream (for us, backed by a unbounded mpsc queue) going
/// from this instance to the server and a response stream coming back
/// (represented directly as a `Streaming<Response>` instance). The recv and send
/// functions interact with the two mpsc channels or the streaming instance
/// respectively.
#[derive(Debug)]
pub struct GrpcClient<G>
where
    G: ProtoServiceTypes,
{
    /// The sender for commands.
    tx: UnboundedSender<G::PC>,
    /// The receiver for responses.
    rx: Streaming<G::PR>,
}

impl<G> GrpcClient<G>
where
    G: ProtoServiceTypes,
{
    /// Connects to the server at the given address, announcing the specified
    /// client version.
    pub async fn connect(
        addr: String,
        version: Version,
        metrics: G::STATS,
        params: &GrpcClientParameters,
    ) -> Result<Self, anyhow::Error> {
        debug!("GrpcClient {}: Attempt to connect", addr);

        let channel = match SocketAddrType::guess(&addr) {
            SocketAddrType::Inet => {
                let mut endpoint = Endpoint::new(format!("http://{}", addr))?;
                if let Some(connect_timeout) = params.connect_timeout {
                    endpoint = endpoint.connect_timeout(connect_timeout);
                }
                if let Some(keep_alive_timeout) = params.http2_keep_alive_timeout {
                    endpoint = endpoint.keep_alive_timeout(keep_alive_timeout);
                }
                if let Some(keep_alive_interval) = params.http2_keep_alive_interval {
                    endpoint = endpoint.http2_keep_alive_interval(keep_alive_interval);
                }
                endpoint.connect().await?
            }
            SocketAddrType::Unix => {
                let addr = addr.clone();
                Endpoint::from_static("http://localhost") // URI is ignored
                    .connect_with_connector(tower::service_fn(move |_| {
                        let addr = addr.clone();
                        async { UnixStream::connect(addr).await.map(TokioIo::new) }
                    }))
                    .await?
            }
        };
        let service = InterceptedService::new(channel, VersionAttachInterceptor::new(version));
        let mut client = BidiProtoClient::new(service, G::URL, metrics);
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = client
            .establish_bidi_stream(UnboundedReceiverStream::new(rx))
            .await?
            .into_inner();
        info!("GrpcClient {}: connected", &addr);
        Ok(GrpcClient { tx, rx })
    }

    /// Like [`GrpcClient::connect`], but for multiple partitioned servers.
    pub async fn connect_partitioned<C, R>(
        dests: Vec<(String, G::STATS)>,
        version: Version,
        params: &GrpcClientParameters,
    ) -> Result<Partitioned<Self, C, R>, anyhow::Error>
    where
        (C, R): Partitionable<C, R>,
    {
        let clients = future::try_join_all(
            dests
                .into_iter()
                .map(|(addr, metrics)| Self::connect(addr, version.clone(), metrics, params)),
        )
        .await?;
        Ok(Partitioned::new(clients))
    }
}

#[async_trait]
impl<G, C, R> GenericClient<C, R> for GrpcClient<G>
where
    C: RustType<G::PC> + Send + Sync + 'static,
    R: RustType<G::PR> + Send + Sync + 'static,
    G: ProtoServiceTypes,
{
    async fn send(&mut self, cmd: C) -> Result<(), anyhow::Error> {
        self.tx.send(cmd.into_proto())?;
        Ok(())
    }

    /// # Cancel safety
    ///
    /// This method is cancel safe. If `recv` is used as the event in a [`tokio::select!`]
    /// statement and some other branch completes first, it is guaranteed that no messages were
    /// received by this client.
    async fn recv(&mut self) -> Result<Option<R>, anyhow::Error> {
        // `TryStreamExt::try_next` is cancel safe. The returned future only holds onto a
        // reference to the underlying stream, so dropping it will never lose a value.
        match self.rx.try_next().await? {
            None => Ok(None),
            Some(response) => Ok(Some(response.into_rust()?)),
        }
    }
}

/// Encapsulates the core functionality of a tonic gRPC client for a service
/// that exposes a single bidirectional RPC stream.
///
/// The client calls back into the StatsCollector on each command send and
/// response receive.
///
/// See the documentation on [`GrpcClient`] for details.
pub struct BidiProtoClient<PC, PR, S>
where
    PC: prost::Message + 'static,
    PR: Default + prost::Message + 'static,
    S: StatsCollector<PC, PR>,
{
    inner: tonic::client::Grpc<ClientTransport>,
    path: &'static str,
    codec: StatCodec<PC, PR, S>,
}

impl<PC, PR, S> BidiProtoClient<PC, PR, S>
where
    PC: Clone + prost::Message + 'static,
    PR: Clone + Default + prost::Message + 'static,
    S: StatsCollector<PC, PR> + 'static,
{
    fn new(inner: ClientTransport, path: &'static str, stats_collector: S) -> Self
    where
        Self: Sized,
    {
        let inner = tonic::client::Grpc::new(inner)
            .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_SIZE);
        let codec = StatCodec::new(stats_collector);
        BidiProtoClient { inner, path, codec }
    }

    async fn establish_bidi_stream(
        &mut self,
        rx: UnboundedReceiverStream<PC>,
    ) -> Result<Response<Streaming<PR>>, Status> {
        self.inner.ready().await.map_err(|e| {
            tonic::Status::new(
                tonic::Code::Unknown,
                format!("Service was not ready: {}", e),
            )
        })?;
        let path = PathAndQuery::from_static(self.path);
        self.inner
            .streaming(rx.into_streaming_request(), path, self.codec.clone())
            .await
    }
}

/// A gRPC server that stitches a gRPC service with a single bidirectional
/// stream to a [`GenericClient`].
///
/// It is the counterpart of [`GrpcClient`].
///
/// To use, implement the tonic-generated `ProtoService` trait for this type.
/// The implementation of the bidirectional stream method should call
/// [`GrpcServer::forward_bidi_stream`] to stitch the bidirectional stream to
/// the client underlying this server.
pub struct GrpcServer<F> {
    state: Arc<GrpcServerState<F>>,
}

struct GrpcServerState<F> {
    cancel_tx: Mutex<oneshot::Sender<()>>,
    client_builder: F,
    metrics: PerGrpcServerMetrics,
}

impl<F, G> GrpcServer<F>
where
    F: Fn() -> G + Send + Sync + 'static,
{
    /// Starts the server, listening for gRPC connections on `listen_addr`.
    ///
    /// The trait bounds on `S` are intimidating, but it is the return type of
    /// `service_builder`, which is a function that
    /// turns a `GrpcServer<ProtoCommandType, ProtoResponseType>` into a
    /// [`Service`] that represents a gRPC server. This is always encapsulated
    /// by the tonic-generated `ProtoServer::new` method for a specific Protobuf
    /// service.
    pub fn serve<S, Fs>(
        metrics: &GrpcServerMetrics,
        listen_addr: SocketAddr,
        version: Version,
        host: Option<String>,
        client_builder: F,
        service_builder: Fs,
    ) -> impl Future<Output = Result<(), anyhow::Error>>
    where
        S: Service<
                http::Request<BoxBody>,
                Response = http::Response<BoxBody>,
                Error = std::convert::Infallible,
            > + NamedService
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
        Fs: FnOnce(Self) -> S + Send + 'static,
    {
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        let state = GrpcServerState {
            cancel_tx: Mutex::new(cancel_tx),
            client_builder,
            metrics: metrics.for_server(S::NAME),
        };
        let server = Self {
            state: Arc::new(state),
        };
        let service = service_builder(server);

        if host.is_none() {
            warn!("no host provided; request destination host checking is disabled");
        }
        let validation = RequestValidationLayer { version, host };

        info!("Starting to listen on {}", listen_addr);

        async {
            let listener = Listener::bind(listen_addr).await?;

            Server::builder()
                .layer(validation)
                .add_service(service)
                .serve_with_incoming(listener)
                .await?;
            Ok(())
        }
    }

    /// Handles a bidirectional stream request by forwarding commands to and
    /// responses from the server's underlying client.
    ///
    /// Call this method from the implementation of the tonic-generated
    /// `ProtoService`.
    pub async fn forward_bidi_stream<C, R, PC, PR>(
        &self,
        request: Request<Streaming<PC>>,
    ) -> Result<Response<ResponseStream<PR>>, Status>
    where
        G: GenericClient<C, R> + 'static,
        C: RustType<PC> + Send + Sync + 'static + fmt::Debug,
        R: RustType<PR> + Send + Sync + 'static + fmt::Debug,
        PC: fmt::Debug + Send + Sync + 'static,
        PR: fmt::Debug + Send + Sync + 'static,
    {
        info!("GrpcServer: remote client connected");

        // Install our cancellation token. This may drop an existing
        // cancellation token. We're allowed to run until someone else drops our
        // cancellation token.
        //
        // TODO(benesch): rather than blindly dropping the existing cancellation
        // token, we should check epochs, and only drop the existing connection
        // if it is at a lower epoch.
        // See: https://github.com/MaterializeInc/materialize/issues/13377
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        *self.state.cancel_tx.lock().await = cancel_tx;

        // Construct a new client and forward commands and responses until
        // canceled.
        let mut request = request.into_inner();
        let state = Arc::clone(&self.state);
        let stream = stream! {
            let mut client = (state.client_builder)();
            loop {
                select! {
                    command = request.next() => {
                        let command = match command {
                            None => break,
                            Some(Ok(command)) => command,
                            Some(Err(e)) => {
                                error!("error handling client: {e}");
                                break;
                            }
                        };

                        match UNIX_EPOCH.elapsed() {
                            Ok(ts) => state.metrics.last_command_received.set(ts.as_secs()),
                            Err(e) => error!("failed to get system time: {e}"),
                        }

                        let command = match command.into_rust() {
                            Ok(command) => command,
                            Err(e) => {
                                error!("error converting command from protobuf: {}", e);
                                break;
                            }
                        };

                        if let Err(e) = client.send(command).await {
                            yield Err(Status::unknown(e.to_string()));
                        }
                    }
                    response = client.recv() => {
                        match response {
                            Ok(Some(response)) => yield Ok(response.into_proto()),
                            Ok(None) => break,
                            Err(e) => yield Err(Status::unknown(e.to_string())),
                        }
                    }
                    _ = &mut cancel_rx => break,
                }
            }
        };
        Ok(Response::new(ResponseStream::new(stream)))
    }
}

/// A stream returning responses to GRPC clients.
///
/// This is defined as a struct, rather than a type alias, so that we can define a `Drop` impl that
/// logs stream termination.
pub struct ResponseStream<PR>(Pin<Box<dyn Stream<Item = Result<PR, Status>> + Send>>);

impl<PR> ResponseStream<PR> {
    fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<PR, Status>> + Send + 'static,
    {
        Self(Box::pin(stream))
    }
}

impl<PR> Stream for ResponseStream<PR> {
    type Item = Result<PR, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx)
    }
}

impl<PR> Drop for ResponseStream<PR> {
    fn drop(&mut self) {
        info!("GrpcServer: response stream disconnected");
    }
}

/// Metrics for a [`GrpcServer`].
#[derive(Debug)]
pub struct GrpcServerMetrics {
    last_command_received: UIntGaugeVec,
}

impl GrpcServerMetrics {
    /// Registers the GRPC server metrics into a `registry`.
    pub fn register_with(registry: &MetricsRegistry) -> Self {
        Self {
            last_command_received: registry.register(metric!(
                name: "mz_grpc_server_last_command_received",
                help: "The time at which the server received its last command.",
                var_labels: ["server_name"],
            )),
        }
    }

    fn for_server(&self, name: &'static str) -> PerGrpcServerMetrics {
        PerGrpcServerMetrics {
            last_command_received: self
                .last_command_received
                .get_delete_on_drop_metric(vec![name]),
        }
    }
}

#[derive(Debug)]
struct PerGrpcServerMetrics {
    last_command_received: DeleteOnDropGauge<'static, AtomicU64, Vec<&'static str>>,
}

const VERSION_HEADER_KEY: &str = "x-mz-version";

/// A gRPC interceptor that attaches a version as metadata to each request.
#[derive(Debug, Clone)]
pub struct VersionAttachInterceptor {
    version: AsciiMetadataValue,
}

impl VersionAttachInterceptor {
    fn new(version: Version) -> VersionAttachInterceptor {
        VersionAttachInterceptor {
            version: version
                .to_string()
                .try_into()
                .expect("semver versions are valid metadata values"),
        }
    }
}

impl Interceptor for VersionAttachInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert(VERSION_HEADER_KEY, self.version.clone());
        Ok(request)
    }
}

/// A `tower` layer that validates requests for compatibility with the server.
#[derive(Clone)]
struct RequestValidationLayer {
    version: Version,
    host: Option<String>,
}

impl<S> tower::Layer<S> for RequestValidationLayer {
    type Service = RequestValidation<S>;

    fn layer(&self, inner: S) -> Self::Service {
        let version = self
            .version
            .to_string()
            .try_into()
            .expect("version is a valid header value");
        RequestValidation {
            inner,
            version,
            host: self.host.clone(),
        }
    }
}

/// A `tower` middleware that validates requests for compatibility with the server.
#[derive(Clone)]
struct RequestValidation<S> {
    inner: S,
    version: http::HeaderValue,
    host: Option<String>,
}

impl<S, B> Service<http::Request<B>> for RequestValidation<S>
where
    S: Service<http::Request<B>, Error = Box<dyn Error + Send + Sync + 'static>>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<S::Response, S::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let error = |msg| {
            let error: S::Error = Box::new(Status::permission_denied(msg));
            Box::pin(future::ready(Err(error)))
        };

        let Some(req_version) = req.headers().get(VERSION_HEADER_KEY) else {
            return error("request missing version header".into());
        };
        if req_version != self.version {
            return error(format!(
                "request has version {req_version:?} but {:?} required",
                self.version
            ));
        }

        let req_host = req.uri().host();
        if let (Some(req_host), Some(host)) = (req_host, &self.host) {
            if req_host != host {
                return error(format!(
                    "request has host {req_host:?} but {host:?} required"
                ));
            }
        }

        Box::pin(self.inner.call(req))
    }
}
