//! This is the entry point of the Internet Computer. This deals with
//! accepting HTTP connections, parsing messages and forwarding them to the
//! correct components.
//!
//! As much as possible the naming of structs in this module should match the
//! naming used in the [Interface
//! Specification](https://sdk.dfinity.org/docs/interface-spec/index.html)
mod body;
mod call;
mod catch_up_package;
mod common;
mod dashboard;
mod health_status_refresher;
mod metrics;
mod pprof;
mod query;
mod read_state;
mod state_reader_executor;
mod status;
mod types;
mod validator_executor;

use crate::{
    call::CallService,
    catch_up_package::CatchUpPackageService,
    common::{
        get_cors_headers, get_root_threshold_public_key, make_plaintext_response,
        map_box_error_to_response,
    },
    dashboard::DashboardService,
    health_status_refresher::HealthStatusRefreshLayer,
    metrics::{
        LABEL_REQUEST_TYPE, LABEL_STATUS, LABEL_TYPE, REQUESTS_LABEL_NAMES, REQUESTS_NUM_LABELS,
    },
    query::QueryService,
    read_state::ReadStateService,
    state_reader_executor::StateReaderExecutor,
    status::StatusService,
    types::*,
    validator_executor::ValidatorExecutor,
};
use byte_unit::Byte;
use crossbeam::atomic::AtomicCell;
use http::method::Method;
use hyper::{
    client::HttpConnector, server::conn::Http, Body, Client, Request, Response, StatusCode,
};
use hyper_tls::HttpsConnector;
use ic_async_utils::{TcpAcceptor, WrappedTcpStream};
use ic_certification::validate_subnet_delegation_certificate;
use ic_config::http_handler::Config;
use ic_crypto_tls_interfaces::TlsHandshake;
use ic_crypto_tree_hash::{lookup_path, LabeledTree, Path};
use ic_crypto_utils_threshold_sig_der::parse_threshold_sig_key_from_der;
use ic_interfaces::{
    consensus_pool::ConsensusPoolCache,
    crypto::IngressSigVerifier,
    execution_environment::{IngressFilterService, QueryExecutionService},
};
use ic_interfaces_p2p::IngressIngestionService;
use ic_interfaces_registry::RegistryClient;
use ic_interfaces_state_manager::StateReader;
use ic_logger::{debug, error, fatal, info, warn, ReplicaLogger};
use ic_metrics::{histogram_vec_timer::HistogramVecTimer, MetricsRegistry};
use ic_registry_client_helpers::{
    crypto::CryptoRegistry, node::NodeRegistry, node_operator::ConnectionEndpoint,
    subnet::SubnetRegistry,
};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::ReplicatedState;
use ic_types::{
    malicious_flags::MaliciousFlags,
    messages::{
        Blob, Certificate, CertificateDelegation, HttpReadState, HttpReadStateContent,
        HttpReadStateResponse, HttpRequestEnvelope, ReplicaHealthStatus,
    },
    time::current_time_and_expiry_time,
    NodeId, SubnetId,
};
use metrics::HttpHandlerMetrics;
use rand::Rng;
use std::{
    convert::{Infallible, TryFrom},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{
    net::TcpListener,
    time::{sleep, timeout, Instant},
};
use tower::{
    load_shed::LoadShed, service_fn, util::BoxCloneService, BoxError, Service, ServiceBuilder,
    ServiceExt,
};

// Constants defining the limits of the HttpHandler.

// The http handler should apply backpresure when we lack a particular resources
// which is purely HttpHandler related (e.g. connections, file descritors).
//
// Current mechanisms for constrained resources include:
//
// 1. File descriptors. The limit can be checked by 'process_max_fds'
// Prometheus metric. The number of file descriptors used by the crate is
// controlled by 'MAX_OUTSTANDING_CONNECTIONS'.
//
// 2. Lock contention. Currently we don't use lock-free data structures
// (e.g. StateManager, RegistryClient), hence we can observe lock contention.
// 'MAX_REQUESTS_PER_SECOND_PER_CONNECTION' is used to control the risk of
// running into contention. A resonable value can be derived by looking what are
// the latencies for operations that hold locks (e.g. methods on the
// RegistryClient and StateManager).

// In the HttpHandler we can have at most 'MAX_OUTSTANDING_CONNECTIONS'
// live TCP connections. If we are at the limit, we won't
// accept new TCP connections.
const MAX_OUTSTANDING_CONNECTIONS: usize = 20000;

// Sets the SETTINGS_MAX_CONCURRENT_STREAMS option for HTTP2 connections.
const HTTP_MAX_CONCURRENT_STREAMS: u32 = 256;

// The maximum time we should wait for a peeking the first bytes on a TCP
// connection. Effectively, if we can't read the first bytes within the
// timeout the connection is broken.
// If you modify this constant please also adjust:
// - `ic_canister_client::agent::MAX_POLL_INTERVAL`,
// - `canister_test::canister::MAX_BACKOFF_INTERVAL`.
// See VER-1060 for details.
const MAX_TCP_PEEK_TIMEOUT_SECS: u64 = 11;

// Request with body size bigger than 'MAX_REQUEST_SIZE_BYTES' will be rejected
// and appropriate error code will be returned to the user.
pub(crate) const MAX_REQUEST_SIZE_BYTES: Byte = Byte::from_bytes(5 * 1024 * 1024); // 5MB

// If the request body is not received/parsed within
// 'MAX_REQUEST_RECEIVE_DURATION', then the request will be rejected and
// appropriate error code will be returned to the user.
pub(crate) const MAX_REQUEST_RECEIVE_DURATION: Duration = Duration::from_secs(300); // 5 min

const HTTP_DASHBOARD_URL_PATH: &str = "/_/dashboard";
const CONTENT_TYPE_CBOR: &str = "application/cbor";

// Placeholder used when we can't determine the approriate prometheus label.
const UNKNOWN_LABEL: &str = "unknown";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpError {
    pub status: StatusCode,
    pub message: String,
}

pub(crate) type EndpointService = BoxCloneService<Body, Response<Body>, BoxError>;

/// The struct that handles incoming HTTP requests for the IC replica.
/// This is collection of thread-safe data members.
#[derive(Clone)]
struct HttpHandler {
    registry_client: Arc<dyn RegistryClient>,
    call_service: EndpointService,
    query_service: EndpointService,
    catchup_service: EndpointService,
    dashboard_service: EndpointService,
    status_service: EndpointService,
    read_state_service: EndpointService,
    health_status_refresher: HealthStatusRefreshLayer,
}

// Crates a detached tokio blocking task that initializes the server (reading
// required state, etc).
fn start_server_initialization(
    log: ReplicaLogger,
    metrics: HttpHandlerMetrics,
    subnet_id: SubnetId,
    nns_subnet_id: SubnetId,
    registry_client: Arc<dyn RegistryClient>,
    state_reader_executor: StateReaderExecutor,
    delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
    health_status: Arc<AtomicCell<ReplicaHealthStatus>>,
    rt_handle: tokio::runtime::Handle,
) {
    rt_handle.spawn(async move {
        info!(log, "Initializing HTTP server...");
        // Sleep one second between retries, only log every 10th round.
        info!(log, "Waiting for certified state...");
        metrics
            .health_status_transitions_total
            .with_label_values(&[
                &health_status.load().to_string(),
                &ReplicaHealthStatus::WaitingForCertifiedState.to_string(),
            ])
            .inc();
        health_status.store(ReplicaHealthStatus::WaitingForCertifiedState);

        while common::get_latest_certified_state(&state_reader_executor)
            .await
            .is_none()
        {
            info!(every_n_seconds => 10, log, "Certified state is not yet available...");
            sleep(Duration::from_secs(1)).await;
        }
        info!(log, "Certified state is now available.");
        // Fetch the delegation from the NNS for this subnet to be
        // able to issue certificates.
        health_status.store(ReplicaHealthStatus::WaitingForRootDelegation);
        let loaded_delegation =
            load_root_delegation(&log, subnet_id, nns_subnet_id, registry_client).await;
        *delegation_from_nns.write().unwrap() = loaded_delegation;
        metrics
            .health_status_transitions_total
            .with_label_values(&[
                &health_status.load().to_string(),
                &ReplicaHealthStatus::Healthy.to_string(),
            ])
            .inc();
        health_status.store(ReplicaHealthStatus::Healthy);
        // IMPORTANT: The system-tests relies on this log message to understand when it
        // can start interacting with the replica. In the future, we plan to
        // have a dedicated instrumentation channel to communicate between the
        // replica and the testing framework, but for now, this is the best we can do.
        info!(log, "Ready for interaction.");
    });
}

fn create_port_file(path: PathBuf, port: u16) {
    // Figure out which port was assigned; write it to a temporary
    // file; and then rename the file to `path`.  We write to a
    // temporary file first to ensure that the write is atomic.  We
    // create the temporary file in the same directory as `path` as
    // `rename` between file systems in case of different
    // directories can fail.
    let dir = path.parent().unwrap_or_else(|| {
        panic!(
            "Could not get parent directory of port report file {}",
            path.display()
        )
    });
    let mut port_file = NamedTempFile::new_in(dir)
        .unwrap_or_else(|err| panic!("Could not open temporary port report file: {}", err));
    port_file
        .write_all(format!("{}", port).as_bytes())
        .unwrap_or_else(|err| {
            panic!(
                "Could not write to temporary port report file {}: {}",
                path.display(),
                err
            )
        });
    port_file.flush().unwrap_or_else(|err| {
        panic!(
            "Could not flush temporary port report file {}: {}",
            path.display(),
            err
        )
    });
    std::fs::rename(port_file, path.clone()).unwrap_or_else(|err| {
        panic!(
            "Could not rename temporary port report file {}: {}",
            path.display(),
            err
        )
    });
}

/// Creates HTTP server, binds to HTTP port and handles HTTP requests forever.
/// This ***async*** function ***never*** returns unless binding to the HTTP
/// port fails.
/// The function spawns a tokio task per connection.
#[allow(clippy::too_many_arguments)]
pub fn start_server(
    rt_handle: tokio::runtime::Handle,
    metrics_registry: MetricsRegistry,
    config: Config,
    ingress_filter: IngressFilterService,
    // ingress_sender and query_execution_service are external services with a concurrency limiter.
    // It is safe to clone them and pass them to a single-threaded context.
    ingress_sender: IngressIngestionService,
    query_execution_service: QueryExecutionService,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    registry_client: Arc<dyn RegistryClient>,
    tls_handshake: Arc<dyn TlsHandshake + Send + Sync>,
    ingress_verifier: Arc<dyn IngressSigVerifier + Send + Sync>,
    subnet_id: SubnetId,
    nns_subnet_id: SubnetId,
    log: ReplicaLogger,
    consensus_pool_cache: Arc<dyn ConsensusPoolCache>,
    subnet_type: SubnetType,
    malicious_flags: MaliciousFlags,
) {
    let metrics = HttpHandlerMetrics::new(&metrics_registry);

    let listen_addr = config.listen_addr;
    let port_file_path = config.port_file_path.clone();

    // TODO(OR4-60): temporarily listen on [::] so that we accept both IPv4 and
    // IPv6 connections. This requires net.ipv6.bindv6only = 0. Revert this once
    // we have rolled out IPv6 in prometheus and ic_p8s_service_discovery.
    let mut addr = "[::]:8080".parse::<SocketAddr>().unwrap();
    addr.set_port(listen_addr.port());
    info!(log, "Starting HTTP server...");
    rt_handle.clone().spawn(async move {
        let delegation_from_nns = Arc::new(RwLock::new(None));
        let health_status = Arc::new(AtomicCell::new(ReplicaHealthStatus::Starting));
        let state_reader_executor = StateReaderExecutor::new(state_reader);
        let validator_executor = ValidatorExecutor::new(ingress_verifier, log.clone());

        if !AtomicCell::<ReplicaHealthStatus>::is_lock_free() {
            error!(log, "Replica health status uses locks instead of atomics.");
        }

        let call_service = CallService::new_service(
            log.clone(),
            metrics.clone(),
            subnet_id,
            Arc::clone(&registry_client),
            validator_executor.clone(),
            ingress_sender,
            ingress_filter,
            malicious_flags.clone(),
        );
        let query_service = QueryService::new_service(
            log.clone(),
            metrics.clone(),
            Arc::clone(&health_status),
            Arc::clone(&delegation_from_nns),
            validator_executor.clone(),
            Arc::clone(&registry_client),
            query_execution_service,
            malicious_flags.clone(),
        );
        let read_state_service = ReadStateService::new_service(
            log.clone(),
            metrics.clone(),
            Arc::clone(&health_status),
            Arc::clone(&delegation_from_nns),
            state_reader_executor.clone(),
            validator_executor,
            Arc::clone(&registry_client),
            malicious_flags,
        );
        let status_service = StatusService::new_service(
            log.clone(),
            config.clone(),
            nns_subnet_id,
            Arc::clone(&registry_client),
            Arc::clone(&health_status),
        );
        let dashboard_service = DashboardService::new_service(
            config.clone(),
            subnet_type,
            state_reader_executor.clone(),
        );
        let catchup_service =
            CatchUpPackageService::new_service(metrics.clone(), consensus_pool_cache.clone());

        let health_status_refresher = HealthStatusRefreshLayer::new(
            log.clone(),
            metrics.clone(),
            Arc::clone(&health_status),
            consensus_pool_cache,
            state_reader_executor.clone(),
        );

        info!(log, "Binding HTTP server to address {}", addr);
        let tcp_listener = TcpListener::bind(addr).await.unwrap();

        start_server_initialization(
            log.clone(),
            metrics.clone(),
            subnet_id,
            nns_subnet_id,
            registry_client.clone(),
            state_reader_executor,
            Arc::clone(&delegation_from_nns),
            Arc::clone(&health_status),
            rt_handle.clone(),
        );

        let http_handler = HttpHandler {
            registry_client,
            call_service,
            query_service,
            status_service,
            catchup_service,
            dashboard_service,
            read_state_service,
            health_status_refresher,
        };
        let main_service = create_main_service(metrics.clone(), http_handler.clone());

        // If addr == 0, then a random port will be assigned. In this case it
        // is useful to report the randomly assigned port by writing it to a file.
        let local_addr = tcp_listener.local_addr().unwrap();
        if let Some(path) = port_file_path {
            create_port_file(path, local_addr.port());
        }
        let tcp_acceptor = TcpAcceptor::new(tcp_listener, MAX_OUTSTANDING_CONNECTIONS);

        let mut http = Http::new();
        http.http2_max_concurrent_streams(HTTP_MAX_CONCURRENT_STREAMS);
        loop {
            let log = log.clone();
            let http = http.clone();
            let http_handler = http_handler.clone();
            let tls_handshake = Arc::clone(&tls_handshake);
            let metrics = metrics.clone();
            match tcp_acceptor.accept().await {
                Ok((tcp_stream, _)) => {
                    metrics.connections_total.inc();
                    // Start recording connection setup duration.
                    let connection_start_time = Instant::now();
                    let svc = main_service.clone();
                    rt_handle.spawn(async move {
                        // Do a move of the permit so it gets dropped at the end of the scope.
                        let mut b = [0_u8; 1];
                        let app_layer = match timeout(
                            Duration::from_secs(MAX_TCP_PEEK_TIMEOUT_SECS),
                            tcp_stream.peek(&mut b),
                        )
                        .await
                        {
                            // The peek operation didn't timeout, and the peek oparation didn't return
                            // an error.
                            Ok(Ok(_)) => {
                                if b[0] == 22 {
                                    AppLayer::Https
                                } else {
                                    AppLayer::Http
                                }
                            }
                            Ok(Err(err)) => {
                                error!(log, "Can't peek into TCP stream, error = {}", err);
                                metrics.observe_connection_error(
                                    ConnectionError::Peek,
                                    connection_start_time,
                                );
                                AppLayer::Http
                            }
                            Err(err) => {
                                warn!(
                                    log,
                                    "TCP peeking timeout after {}s, error = {}",
                                    MAX_TCP_PEEK_TIMEOUT_SECS,
                                    err
                                );

                                metrics.observe_connection_error(
                                    ConnectionError::PeekTimeout,
                                    connection_start_time,
                                );
                                AppLayer::Http
                            }
                        };
                        serve_connection(
                            log,
                            svc,
                            app_layer,
                            http,
                            tcp_stream,
                            tls_handshake,
                            http_handler,
                            metrics,
                            connection_start_time,
                        )
                        .await;
                    });
                }
                // Don't exit the loop on a connection error. We will want to
                // continue serving.
                Err(err) => {
                    metrics.observe_connection_error(ConnectionError::Accept, Instant::now());
                    error!(log, "Can't accept TCP connection, error = {}", err);
                }
            }
        }
    });
}

fn create_main_service(
    metrics: HttpHandlerMetrics,
    http_handler: HttpHandler,
) -> BoxCloneService<Request<Body>, Response<Body>, Infallible> {
    let health_status_refresher = http_handler.health_status_refresher.clone();
    let route_service = service_fn(move |req: RequestWithTimer| {
        let http_handler = http_handler.clone();
        async move { Ok::<_, Infallible>(make_router(http_handler, req).await) }
    });

    BoxCloneService::new(
        ServiceBuilder::new()
            // Attach a timer as soon as we see a request.
            .map_request(move |request| {
                let _ = &metrics;
                // Start recording request duration.
                let request_timer = HistogramVecTimer::start_timer(
                    metrics.requests.clone(),
                    &REQUESTS_LABEL_NAMES,
                    [UNKNOWN_LABEL, UNKNOWN_LABEL, UNKNOWN_LABEL],
                );
                (request, request_timer)
            })
            .layer(health_status_refresher)
            .service(route_service)
            .map_result(move |result| {
                let (response, request_timer) = result.expect("Can't panic on infallible");
                let status = response.status();
                // This is a workaround for `StatusCode::as_str()` not returning a `&'static
                // str`. It ensures `request_timer` is dropped before `status`.
                let mut timer = request_timer;
                timer.set_label(LABEL_STATUS, status.as_str());
                Ok::<_, Infallible>(response)
            }),
    )
}

async fn serve_connection(
    log: ReplicaLogger,
    service: BoxCloneService<Request<Body>, Response<Body>, Infallible>,
    app_layer: AppLayer,
    http: Http,
    tcp_stream: WrappedTcpStream,
    tls_handshake: Arc<dyn TlsHandshake + Send + Sync>,
    http_handler: HttpHandler,
    metrics: HttpHandlerMetrics,
    connection_start_time: Instant,
) {
    let (tcp_stream, _counter) = tcp_stream.take();
    let connection_result = match app_layer {
        AppLayer::Https => {
            let peer_addr = tcp_stream.peer_addr();
            let tls_stream = match tls_handshake
                .perform_tls_server_handshake_without_client_auth(
                    tcp_stream,
                    http_handler.registry_client.get_latest_version(),
                )
                .await
            {
                Err(err) => {
                    metrics.observe_connection_error(
                        ConnectionError::TlsHandshake,
                        connection_start_time,
                    );
                    warn!(
                        log,
                        "TLS handshake failed, error = {}, peer_addr = {:?}", err, peer_addr,
                    );
                    return;
                }
                Ok(tls_stream) => tls_stream,
            };
            metrics.observe_successful_connection_setup(app_layer, connection_start_time);
            http.serve_connection(tls_stream, service).await
        }
        AppLayer::Http => {
            metrics.observe_successful_connection_setup(app_layer, connection_start_time);
            http.serve_connection(tcp_stream, service).await
        }
    };

    match connection_result {
        Err(err) => {
            metrics.observe_abrupt_conn_termination(app_layer, connection_start_time);
            info!(
                log,
                "The connection was closed abruptly after {:?}, error = {}",
                connection_start_time.elapsed(),
                err
            );
        }
        Ok(()) => metrics.observe_graceful_conn_termination(app_layer, connection_start_time),
    }
}

type RequestWithTimer = (
    Request<Body>,
    HistogramVecTimer<'static, REQUESTS_NUM_LABELS>,
);
type ResponseWithTimer = (
    Response<Body>,
    HistogramVecTimer<'static, REQUESTS_NUM_LABELS>,
);

fn set_timer_labels(
    timer: &mut HistogramVecTimer<'static, REQUESTS_NUM_LABELS>,
    api_req_type: ApiReqType,
) {
    timer.set_label(LABEL_TYPE, to_legacy_request_type(api_req_type));
    timer.set_label(LABEL_REQUEST_TYPE, api_req_type.into());
}

async fn make_router(
    http_handler: HttpHandler,
    (req, mut timer): RequestWithTimer,
) -> ResponseWithTimer {
    let call_service = http_handler.call_service.clone();
    let query_service = http_handler.query_service.clone();
    let status_service = http_handler.status_service.clone();
    let catch_up_package_service = http_handler.catchup_service.clone();
    let dashboard_service = http_handler.dashboard_service.clone();
    let read_state_service = http_handler.read_state_service.clone();

    let svc = match req.method().clone() {
        Method::POST => {
            // Check the content-type header
            if !req
                .headers()
                .get_all(http::header::CONTENT_TYPE)
                .iter()
                .any(|value| {
                    if let Ok(v) = value.to_str() {
                        return v.to_lowercase() == CONTENT_TYPE_CBOR;
                    }
                    false
                })
            {
                set_timer_labels(&mut timer, ApiReqType::InvalidArgument);
                return (
                    make_plaintext_response(
                        StatusCode::BAD_REQUEST,
                        format!("Unexpected content-type, expected {}.", CONTENT_TYPE_CBOR),
                    ),
                    timer,
                );
            }

            // Check the path
            let path = req.uri().path();
            match *path.split('/').collect::<Vec<&str>>().as_slice() {
                ["", "api", "v2", "canister", _, "call"] => {
                    set_timer_labels(&mut timer, ApiReqType::Call);
                    call_service
                }
                ["", "api", "v2", "canister", _, "query"] => {
                    set_timer_labels(&mut timer, ApiReqType::Query);
                    query_service
                }
                ["", "api", "v2", "canister", _, "read_state"] => {
                    set_timer_labels(&mut timer, ApiReqType::ReadState);
                    read_state_service
                }
                ["", "_", "catch_up_package"] => {
                    set_timer_labels(&mut timer, ApiReqType::CatchUpPackage);
                    catch_up_package_service
                }
                _ => {
                    set_timer_labels(&mut timer, ApiReqType::InvalidArgument);
                    return (
                        make_plaintext_response(
                            StatusCode::NOT_FOUND,
                            "Unexpected POST request path.".to_string(),
                        ),
                        timer,
                    );
                }
            }
        }
        Method::GET => match req.uri().path() {
            "/api/v2/status" => {
                set_timer_labels(&mut timer, ApiReqType::Status);
                status_service
            }
            "/" | "/_/" => {
                set_timer_labels(&mut timer, ApiReqType::RedirectToDashboard);
                return (redirect_to_dasboard_response(), timer);
            }
            HTTP_DASHBOARD_URL_PATH => {
                set_timer_labels(&mut timer, ApiReqType::Dashboard);
                dashboard_service
            }
            "/_/pprof" => {
                set_timer_labels(&mut timer, ApiReqType::PprofHome);
                return (pprof::home(), timer);
            }
            "/_/pprof/profile" => {
                set_timer_labels(&mut timer, ApiReqType::PprofProfile);
                return (pprof::cpu_profile(req.into_parts().0).await, timer);
            }
            "/_/pprof/flamegraph" => {
                set_timer_labels(&mut timer, ApiReqType::PprofFlamegraph);
                return (pprof::cpu_flamegraph(req.into_parts().0).await, timer);
            }
            _ => {
                set_timer_labels(&mut timer, ApiReqType::InvalidArgument);
                return (
                    make_plaintext_response(
                        StatusCode::NOT_FOUND,
                        "Unexpected GET request path.".to_string(),
                    ),
                    timer,
                );
            }
        },
        Method::OPTIONS => {
            set_timer_labels(&mut timer, ApiReqType::Options);
            return (no_content_response(), timer);
        }
        _ => {
            set_timer_labels(&mut timer, ApiReqType::InvalidArgument);
            return (
                make_plaintext_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!(
                        "Unsupported method: {}. supported methods: POST, GET, OPTIONS.",
                        req.method()
                    ),
                ),
                timer,
            );
        }
    };
    (
        LoadShed::new(svc)
            .ready()
            .await
            .expect("The load shedder must always be ready.")
            .call(req.into_body())
            .await
            .unwrap_or_else(|err| map_box_error_to_response(err)),
        timer,
    )
}

// Fetches a delegation from the NNS subnet to allow this subnet to issue
// certificates on its behalf. On the NNS subnet this method is a no-op.
async fn load_root_delegation(
    log: &ReplicaLogger,
    subnet_id: SubnetId,
    nns_subnet_id: SubnetId,
    registry_client: Arc<dyn RegistryClient>,
) -> Option<CertificateDelegation> {
    if subnet_id == nns_subnet_id {
        info!(log, "On the NNS subnet. Skipping fetching the delegation.");
        // On the NNS subnet. No delegation needs to be fetched.
        return None;
    }

    let mut fetching_root_delagation_attempts = 0;
    loop {
        fetching_root_delagation_attempts += 1;
        info!(
            log,
            "Fetching delegation from the nns subnet. Attempts: {}.",
            fetching_root_delagation_attempts
        );

        async fn log_err_and_backoff(log: &ReplicaLogger, err: impl std::fmt::Display) {
            // Fetching the NNS delegation failed. Do a random backoff and try again.
            let backoff = Duration::from_secs(rand::thread_rng().gen_range(1..15));
            warn!(
                log,
                "Fetching delegation from nns subnet failed. Retrying again in {} seconds...\n\
                    Error received: {}",
                backoff.as_secs(),
                err
            );
            sleep(backoff).await
        }

        let (peer_id, node) =
            match get_random_node_from_nns_subnet(registry_client.clone(), nns_subnet_id).await {
                Ok(node_topology) => node_topology,
                Err(err) => {
                    fatal!(
                        log,
                        "Could not find a node from the root subnet to talk to. Error :{}",
                        err
                    );
                }
            };

        let envelope = HttpRequestEnvelope {
            content: HttpReadStateContent::ReadState {
                read_state: HttpReadState {
                    sender: Blob(vec![4]),
                    paths: vec![
                        Path::new(vec![
                            b"subnet".into(),
                            subnet_id.get().into(),
                            b"public_key".into(),
                        ]),
                        Path::new(vec![
                            b"subnet".into(),
                            subnet_id.get().into(),
                            b"canister_ranges".into(),
                        ]),
                    ],
                    ingress_expiry: current_time_and_expiry_time().1.as_nanos_since_unix_epoch(),
                    nonce: None,
                },
            },
            sender_pubkey: None,
            sender_sig: None,
            sender_delegation: None,
        };

        let body = serde_cbor::ser::to_vec(&envelope).unwrap();

        let registry_version = registry_client.get_latest_version();
        let peer_cert = match registry_client.get_tls_certificate(peer_id, registry_version) {
            Ok(Some(cert)) => cert.certificate_der,
            Err(err) => {
                log_err_and_backoff(log, err).await;
                continue;
            }
            Ok(None) => {
                log_err_and_backoff(log, &format!("Certificate for node_id = {} is missing from registry at registry version = {}.", peer_id, registry_version)).await;
                continue;
            }
        };
        let peer_cert = match native_tls::Certificate::from_der(&peer_cert) {
            Ok(cert) => cert,
            Err(err) => {
                log_err_and_backoff(log, &err).await;
                continue;
            }
        };
        let native_tls_connector = native_tls::TlsConnector::builder()
            .use_sni(false)
            .add_root_certificate(peer_cert)
            .disable_built_in_roots(true)
            .build()
            .expect("failed to build tls connector");
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false);
        let mut https_connector =
            HttpsConnector::from((http_connector, native_tls_connector.into()));
        https_connector.https_only(false);
        let http_client = Client::builder().build::<_, hyper::Body>(https_connector);

        let ip_addr = node.ip_addr.parse().unwrap();
        // any effective canister id can be used when invoking read_state here
        let address = format!(
            // TODO(NET-1280)
            "http://{}/api/v2/canister/aaaaa-aa/read_state",
            SocketAddr::new(ip_addr, node.port as u16)
        );
        info!(
            log,
            "Attempt to fetch delegation from root subnet node with url `{}`", address
        );

        let nns_request = match Request::builder()
            .method(hyper::Method::POST)
            .uri(&address)
            .header(hyper::header::CONTENT_TYPE, CONTENT_TYPE_CBOR)
            .body(Body::from(body))
        {
            Ok(r) => r,
            Err(err) => {
                log_err_and_backoff(log, &err).await;
                continue;
            }
        };

        let raw_response_res = match http_client.request(nns_request).await {
            Ok(res) => res,
            Err(err) => {
                log_err_and_backoff(log, &err).await;
                continue;
            }
        };

        let raw_response = match hyper::body::to_bytes(raw_response_res).await {
            Ok(raw_response) => raw_response,
            Err(err) => {
                // Fetching the NNS delegation failed. Do a random backoff and try again.
                log_err_and_backoff(log, &err).await;
                continue;
            }
        };

        debug!(log, "Response from nns subnet: {:?}", raw_response);

        let response: HttpReadStateResponse = match serde_cbor::from_slice(&raw_response) {
            Ok(r) => r,
            Err(e) => {
                log_err_and_backoff(log, &e).await;
                continue;
            }
        };

        let parsed_delegation: Certificate = match serde_cbor::from_slice(&response.certificate) {
            Ok(r) => r,
            Err(e) => {
                log_err_and_backoff(
                    log,
                    &format!("failed to parse delegation certificate: {}", e),
                )
                .await;
                continue;
            }
        };

        let labeled_tree = match LabeledTree::try_from(parsed_delegation.tree) {
            Ok(r) => r,
            Err(e) => {
                log_err_and_backoff(
                    log,
                    &format!("invalid hash tree in the delegation certificate: {:?}", e),
                )
                .await;
                continue;
            }
        };

        let own_public_key_from_registry = match registry_client
            .get_threshold_signing_public_key_for_subnet(subnet_id, registry_version)
        {
            Ok(Some(pk)) => pk,
            Ok(None) => {
                log_err_and_backoff(
                    log,
                    &format!("subnet {} public key from registry is empty", subnet_id),
                )
                .await;
                continue;
            }
            Err(err) => {
                log_err_and_backoff(
                    log,
                    &format!(
                        "subnet {} public key could not be extracted from registry: {:?}",
                        subnet_id, err,
                    ),
                )
                .await;
                continue;
            }
        };

        match lookup_path(
            &labeled_tree,
            &[b"subnet", subnet_id.get_ref().as_ref(), b"public_key"],
        ) {
            Some(LabeledTree::Leaf(pk_bytes)) => {
                let public_key_from_certificate = match parse_threshold_sig_key_from_der(pk_bytes) {
                    Ok(pk) => pk,
                    Err(err) => {
                        log_err_and_backoff(log, &err).await;
                        continue;
                    }
                };

                if public_key_from_certificate != own_public_key_from_registry {
                    log_err_and_backoff(
                        log,
                        &format!(
                            "mismatch of registry and certificate public keys for subnet {}",
                            subnet_id
                        ),
                    )
                    .await;
                    continue;
                }
            }
            _ => {
                log_err_and_backoff(
                    log,
                    &format!(
                        "subnet {} public key could not be extracted from certificate",
                        subnet_id
                    ),
                )
                .await;
                continue;
            }
        }
        let root_threshold_public_key = match get_root_threshold_public_key(
            log,
            registry_client.clone(),
            registry_version,
            &nns_subnet_id,
        ) {
            Some(public_key) => public_key,
            None => {
                log_err_and_backoff(
                    log,
                    "could not retrieve threshold root public key from registry".to_string(),
                )
                .await;
                continue;
            }
        };
        if let Err(err) = validate_subnet_delegation_certificate(
            &response.certificate,
            &subnet_id,
            &root_threshold_public_key,
        ) {
            log_err_and_backoff(
                log,
                &format!("invalid subnet delegation certificate: {:?} ", err),
            )
            .await;
            continue;
        }

        let delegation = CertificateDelegation {
            subnet_id: Blob(subnet_id.get().to_vec()),
            certificate: response.certificate,
        };

        info!(log, "Setting NNS delegation to: {:?}", delegation);
        return Some(delegation);
    }
}

async fn get_random_node_from_nns_subnet(
    registry_client: Arc<dyn RegistryClient>,
    nns_subnet_id: SubnetId,
) -> Result<(NodeId, ConnectionEndpoint), String> {
    use rand::seq::SliceRandom;

    let nns_nodes = match registry_client
        .get_node_ids_on_subnet(nns_subnet_id, registry_client.get_latest_version())
    {
        Ok(Some(nns_nodes)) => Ok(nns_nodes),
        Ok(None) => Err("No nns nodes found.".to_string()),
        Err(err) => Err(format!("Failed to get nns nodes from registry: {}", err)),
    }?;

    // Randomly choose a node from the nns subnet.
    let mut rng = rand::thread_rng();
    let nns_node = nns_nodes.choose(&mut rng).ok_or(format!(
        "Failed to choose random nns node. NNS node list: {:?}",
        nns_nodes
    ))?;
    match registry_client.get_transport_info(*nns_node, registry_client.get_latest_version()) {
        Ok(Some(node)) => Ok((*nns_node, node.http.ok_or("No http endpoint for node")?)),
        Ok(None) => Err(format!(
            "No transport info found for nns node. {}",
            nns_node
        )),
        Err(err) => Err(format!(
            "Failed to get transport info for nns node {}. Err: {}",
            nns_node, err
        )),
    }
}

fn no_content_response() -> Response<Body> {
    let mut response = Response::new(Body::from(""));
    *response.status_mut() = StatusCode::NO_CONTENT;
    *response.headers_mut() = get_cors_headers();
    response
}

fn redirect_to_dasboard_response() -> Response<Body> {
    // The empty string is simply to uniformize the return type with the cases where
    // the response is not empty.
    let mut response = Response::new(Body::from(""));
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(
        hyper::header::LOCATION,
        hyper::header::HeaderValue::from_static(HTTP_DASHBOARD_URL_PATH),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify that ReplicatedStateHealth is represented as an atomic by crossbeam.
    #[test]
    fn test_replica_state_atomic() {
        assert!(AtomicCell::<ReplicaHealthStatus>::is_lock_free());
    }
}
