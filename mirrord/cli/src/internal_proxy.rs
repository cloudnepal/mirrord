//! Internal proxy is accepting connection from local layers and forward it to agent
//! while having 1:1 relationship - each layer connection is another agent connection.
//!
//! This might be changed later on.
//!
//! The main advantage of this design is that we remove kube logic from the layer itself,
//! thus eliminating bugs that happen due to mix of remote env vars in our code
//! (previously was solved using envguard which wasn't good enough)
//!
//! The proxy will either directly connect to an existing agent (currently only used for tests),
//! or let the [`OperatorApi`](mirrord_operator::client::OperatorApi) handle the connection.

use std::{
    env,
    io::Write,
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use mirrord_analytics::{AnalyticsError, AnalyticsReporter, CollectAnalytics, Reporter};
use mirrord_config::LayerConfig;
use mirrord_intproxy::{
    agent_conn::{AgentConnectInfo, AgentConnection},
    IntProxy,
};
use mirrord_protocol::{ClientMessage, DaemonMessage, LogLevel};
use nix::{
    libc,
    sys::resource::{setrlimit, Resource},
};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, log::trace, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    connection::AGENT_CONNECT_INFO_ENV_KEY,
    error::{CliError, InternalProxySetupError, Result},
};

unsafe fn redirect_fd_to_dev_null(fd: libc::c_int) {
    let devnull_fd = libc::open(b"/dev/null\0" as *const [u8; 10] as _, libc::O_RDWR);
    libc::dup2(devnull_fd, fd);
    libc::close(devnull_fd);
}

unsafe fn detach_io() -> Result<()> {
    // Create a new session for the proxy process, detaching from the original terminal.
    // This makes the process not to receive signals from the "mirrord" process or it's parent
    // terminal fixes some side effects such as https://github.com/metalbear-co/mirrord/issues/1232
    nix::unistd::setsid().map_err(InternalProxySetupError::SetSidError)?;

    // flush before redirection
    {
        // best effort
        let _ = std::io::stdout().lock().flush();
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        redirect_fd_to_dev_null(fd);
    }
    Ok(())
}

/// Print the port for the caller (mirrord cli execution flow) so it can pass it
/// back to the layer instances via env var.
fn print_port(listener: &TcpListener) -> Result<()> {
    let port = listener
        .local_addr()
        .map_err(InternalProxySetupError::LocalPortError)?
        .port();
    println!("{port}\n");
    Ok(())
}

/// Creates a listening socket using socket2
/// to control the backlog and manage scenarios where
/// the proxy is under heavy load.
/// <https://github.com/metalbear-co/mirrord/issues/1716#issuecomment-1663736500>
/// in macOS backlog is documented to be hardcoded limited to 128.
fn create_listen_socket() -> Result<TcpListener, InternalProxySetupError> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .map_err(InternalProxySetupError::ListenError)?;

    socket
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            0,
        )))
        .map_err(InternalProxySetupError::ListenError)?;
    socket
        .listen(1024)
        .map_err(InternalProxySetupError::ListenError)?;

    socket
        .set_nonblocking(true)
        .map_err(InternalProxySetupError::ListenError)?;

    // socket2 -> std -> tokio
    TcpListener::from_std(socket.into()).map_err(InternalProxySetupError::ListenError)
}

fn get_agent_connect_info() -> Result<Option<AgentConnectInfo>> {
    let Ok(var) = env::var(AGENT_CONNECT_INFO_ENV_KEY) else {
        return Ok(None);
    };

    serde_json::from_str(&var).map_err(|e| CliError::ConnectInfoLoadFailed(var, e))
}

/// Main entry point for the internal proxy.
/// It listens for inbound layer connect and forwards to agent.
pub(crate) async fn proxy(watch: drain::Watch) -> Result<()> {
    let config = LayerConfig::from_env()?;

    if let Some(ref log_destination) = config.internal_proxy.log_destination {
        let output_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_destination)
            .map_err(CliError::OpenIntProxyLogFile)?;
        let tracing_registry = tracing_subscriber::fmt()
            .with_writer(output_file)
            .with_ansi(false);
        if let Some(ref log_level) = config.internal_proxy.log_level {
            tracing_registry
                .with_env_filter(EnvFilter::builder().parse_lossy(log_level))
                .init();
        } else {
            tracing_registry.init();
        }
    }

    // According to https://wilsonmar.github.io/maximum-limits/ this is the limit on macOS
    // so we assume Linux can be higher and set to that.
    if let Err(err) = setrlimit(Resource::RLIMIT_NOFILE, 12288, 12288) {
        warn!(?err, "Failed to set the file descriptor limit");
    }

    let agent_connect_info = get_agent_connect_info()?;

    let mut analytics = AnalyticsReporter::new(config.telemetry, watch);
    (&config).collect_analytics(analytics.get_mut());

    // Let it assign port for us then print it for the user.
    let listener = create_listen_socket()?;

    // Create a main connection, that will be held until proxy is closed.
    // This will guarantee agent staying alive and will enable us to
    // make the agent close on last connection close immediately (will help in tests)
    let main_connection = connect_and_ping(&config, agent_connect_info.clone(), &mut analytics)
        .await
        .inspect_err(|_| analytics.set_error(AnalyticsError::AgentConnection))?;

    let (main_connection_cancellation_token, main_connection_task_join) =
        create_ping_loop(main_connection);

    print_port(&listener)?;

    unsafe {
        detach_io()?;
    }

    let first_connection_timeout = Duration::from_secs(config.internal_proxy.start_idle_timeout);
    let consecutive_connection_timeout = Duration::from_secs(config.internal_proxy.idle_timeout);

    IntProxy::new(&config, agent_connect_info, listener)
        .await?
        .run(first_connection_timeout, consecutive_connection_timeout)
        .await?;

    main_connection_cancellation_token.cancel();

    trace!("intproxy joining main connection task");
    match main_connection_task_join.await {
        Ok(Err(err)) => Err(err.into()),
        Err(err) => {
            error!("internal_proxy connection panicked {err}");

            Err(InternalProxySetupError::AgentClosedConnection.into())
        }
        _ => Ok(()),
    }
}

/// Connect and send ping - this is useful when working using k8s
/// port forward since it only creates the connection after
/// sending the first message
async fn connect_and_ping(
    config: &LayerConfig,
    agent_connect_info: Option<AgentConnectInfo>,
    analytics: &mut AnalyticsReporter,
) -> Result<(mpsc::Sender<ClientMessage>, mpsc::Receiver<DaemonMessage>)> {
    let AgentConnection {
        agent_tx,
        mut agent_rx,
    } = AgentConnection::new(config, agent_connect_info, analytics).await?;
    ping(&agent_tx, &mut agent_rx).await?;
    Ok((agent_tx, agent_rx))
}

/// Sends a ping the connection and expects a pong.
async fn ping(
    sender: &mpsc::Sender<ClientMessage>,
    receiver: &mut mpsc::Receiver<DaemonMessage>,
) -> Result<(), InternalProxySetupError> {
    sender.send(ClientMessage::Ping).await?;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match receiver.recv().await {
                Some(DaemonMessage::Pong) => break,
                Some(DaemonMessage::LogMessage(msg)) => match msg.level {
                    LogLevel::Error => error!("Agent log: {}", msg.message),
                    LogLevel::Warn => warn!("Agent log: {}", msg.message),
                },
                other => {
                    error!(?other, "Invalid ping response");
                    return Err(InternalProxySetupError::NoPong(format!("{other:?}")));
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| InternalProxySetupError::NoPong("Timeout in pong".to_string()))?
}

fn create_ping_loop(
    mut connection: (mpsc::Sender<ClientMessage>, mpsc::Receiver<DaemonMessage>),
) -> (
    CancellationToken,
    JoinHandle<Result<(), InternalProxySetupError>>,
) {
    let cancellation_token = CancellationToken::new();

    let join_handle = tokio::spawn({
        let cancellation_token = cancellation_token.clone();

        async move {
            let mut main_keep_interval = tokio::time::interval(Duration::from_secs(30));
            main_keep_interval.tick().await;

            loop {
                tokio::select! {
                    _ = main_keep_interval.tick() => {
                        if let Err(err) = ping(&connection.0, &mut connection.1).await {
                            cancellation_token.cancel();

                            return Err(err);
                        }
                    }
                    _ = cancellation_token.cancelled() => {
                        break;
                    }
                }
            }

            Ok(())
        }
    });

    (cancellation_token, join_handle)
}
