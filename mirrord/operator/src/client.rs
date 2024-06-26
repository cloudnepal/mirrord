use std::{
    fmt::{self, Display},
    io,
};

use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use http::request::Request;
use kube::{
    api::{ListParams, PostParams},
    Api, Client, Resource,
};
use mirrord_analytics::{AnalyticsHash, AnalyticsOperatorProperties, Reporter};
use mirrord_auth::{
    certificate::Certificate,
    credential_store::{CredentialStoreSync, UserIdentity},
    credentials::LicenseValidity,
    error::AuthenticationError,
};
use mirrord_config::{
    feature::network::incoming::ConcurrentSteal,
    target::{Target, TargetConfig},
    LayerConfig,
};
use mirrord_kube::{
    api::kubernetes::{create_kube_api, get_k8s_resource_api},
    error::KubeApiError,
};
use mirrord_progress::Progress;
use mirrord_protocol::{ClientMessage, DaemonMessage};
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_tungstenite::tungstenite::{Error as TungsteniteError, Message};
use tracing::{debug, error, info, warn};

use crate::crd::{
    CopyTargetCrd, CopyTargetSpec, MirrordOperatorCrd, OperatorFeatures, SessionCrd, TargetCrd,
    OPERATOR_STATUS_NAME,
};

static CONNECTION_CHANNEL_SIZE: usize = 1000;

pub use http::Error as HttpError;

/// Operations performed on the operator via [`kube`] API.
#[derive(Debug)]
pub enum OperatorOperation {
    FindingOperator,
    FindingTarget,
    WebsocketConnection,
    CopyingTarget,
    GettingStatus,
    SessionManagement,
    ListingTargets,
}

impl Display for OperatorOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let as_str = match self {
            Self::FindingOperator => "finding operator",
            Self::FindingTarget => "finding target",
            Self::WebsocketConnection => "creating a websocket connection",
            Self::CopyingTarget => "copying target",
            Self::GettingStatus => "getting status",
            Self::SessionManagement => "session management",
            Self::ListingTargets => "listing targets",
        };

        f.write_str(as_str)
    }
}

#[derive(Debug, Error)]
pub enum OperatorApiError {
    #[error("failed to build a websocket connect request: {0}")]
    ConnectRequestBuildError(HttpError),

    #[error("failed to create mirrord operator API: {0}")]
    CreateApiError(KubeApiError),

    #[error("{operation} failed: {error}")]
    KubeError {
        error: kube::Error,
        operation: OperatorOperation,
    },

    #[error("mirrord operator {operator_version} does not support feature {feature}")]
    UnsupportedFeature {
        feature: String,
        operator_version: String,
    },

    #[error("{operation} failed with code {}: {}", status.code, status.reason)]
    StatusFailure {
        operation: OperatorOperation,
        status: Box<kube::core::Status>,
    },

    #[error("mirrord operator license expired")]
    NoLicense,
}

type Result<T, E = OperatorApiError> = std::result::Result<T, E>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OperatorSessionMetadata {
    client_certificate: Option<Certificate>,
    session_id: u64,
    fingerprint: Option<String>,
    operator_features: Vec<OperatorFeatures>,
    protocol_version: Option<semver::Version>,
    copy_pod_enabled: Option<bool>,
}

impl OperatorSessionMetadata {
    fn new(
        client_certificate: Option<Certificate>,
        fingerprint: Option<String>,
        operator_features: Vec<OperatorFeatures>,
        protocol_version: Option<semver::Version>,
        copy_pod_enabled: Option<bool>,
    ) -> Self {
        Self {
            client_certificate,
            session_id: rand::random(),
            fingerprint,
            operator_features,
            protocol_version,
            copy_pod_enabled,
        }
    }

    fn client_credentials(&self) -> io::Result<Option<String>> {
        self.client_certificate
            .as_ref()
            .map(|cert| {
                cert.encode_der()
                    .map(|bytes| general_purpose::STANDARD.encode(bytes))
            })
            .transpose()
    }

    fn set_operator_properties<R: Reporter>(&self, analytics: &mut R) {
        let client_hash = self
            .client_certificate
            .as_ref()
            .map(|cert| cert.public_key_data())
            .as_deref()
            .map(AnalyticsHash::from_bytes);

        analytics.set_operator_properties(AnalyticsOperatorProperties {
            client_hash,
            license_hash: self.fingerprint.as_deref().map(AnalyticsHash::from_base64),
        });
    }

    fn proxy_feature_enabled(&self) -> bool {
        self.operator_features.contains(&OperatorFeatures::ProxyApi)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum OperatorSessionTarget {
    Raw(TargetCrd),
    Copied(CopyTargetCrd),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OperatorSessionInformation {
    target: OperatorSessionTarget,
    metadata: OperatorSessionMetadata,
}

pub struct OperatorApi {
    client: Client,
    target_api: Api<TargetCrd>,
    copy_target_api: Api<CopyTargetCrd>,
    target_namespace: Option<String>,
    target_config: TargetConfig,
    on_concurrent_steal: ConcurrentSteal,
}

/// Connection to existing operator session.
pub struct OperatorSessionConnection {
    /// For sending messages to the operator.
    pub tx: Sender<ClientMessage>,
    /// For receiving messages from the operator.
    pub rx: Receiver<DaemonMessage>,
    /// Additional data about the session.
    pub info: OperatorSessionInformation,
}

/// Allows us to access the operator's [`SessionCrd`] [`Api`].
pub async fn session_api(config: Option<String>) -> Result<Api<SessionCrd>> {
    let kube_api: Client = create_kube_api(false, config, None)
        .await
        .map_err(OperatorApiError::CreateApiError)?;

    Ok(Api::all(kube_api))
}

impl OperatorApi {
    /// We allow copied pods to live only for 30 seconds before the internal proxy connects.
    const COPIED_POD_IDLE_TTL: u32 = 30;

    /// Checks used config against operator specification.
    fn check_config(config: &LayerConfig, operator: &MirrordOperatorCrd) -> Result<()> {
        if config.feature.copy_target.enabled && !operator.spec.copy_target_enabled.unwrap_or(false)
        {
            return Err(OperatorApiError::UnsupportedFeature {
                feature: "copy target".into(),
                operator_version: operator.spec.operator_version.clone(),
            });
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(api))]
    pub async fn get_client_certificate(
        api: &OperatorApi,
        operator: &MirrordOperatorCrd,
    ) -> Result<Option<Certificate>, AuthenticationError> {
        let Some(fingerprint) = operator.spec.license.fingerprint.clone() else {
            return Ok(None);
        };

        let subscription_id = operator.spec.license.subscription_id.clone();

        let mut credential_store = CredentialStoreSync::open().await?;
        credential_store
            .get_client_certificate::<MirrordOperatorCrd>(&api.client, fingerprint, subscription_id)
            .await
            .map(Some)
    }

    /// Creates new [`OperatorSessionConnection`] based on the given [`LayerConfig`].
    /// Keep in mind that some failures here won't stop mirrord from hooking into the process
    /// and working, it'll just work without the operator.
    ///
    /// For a fuller documentation, see the docs in `operator/service/src/main.rs::listen`.
    ///
    /// - `copy_target`: When this feature is enabled, `target` validation is done in the operator.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn create_session<P, R: Reporter>(
        config: &LayerConfig,
        progress: &P,
        analytics: &mut R,
    ) -> Result<OperatorSessionConnection>
    where
        P: Progress + Send + Sync,
    {
        let operator_api = OperatorApi::new(config).await?;

        let operator = operator_api.fetch_operator().await?;

        // Warns the user if their license is close to expiring or fallback to OSS if expired
        let Some(days_until_expiration) =
            DateTime::from_naive_date(operator.spec.license.expire_at).days_until_expiration()
        else {
            let no_license_message = "No valid license found for mirrord for Teams, falling back to OSS usage. Visit https://app.metalbear.co to purchase or renew your license.";
            progress.warning(no_license_message);
            warn!(no_license_message);

            return Err(OperatorApiError::NoLicense);
        };

        let expires_soon =
            days_until_expiration <= <DateTime<Utc> as LicenseValidity>::CLOSE_TO_EXPIRATION_DAYS;
        let is_trial = operator.spec.license.name.contains("(Trial)");

        if is_trial && expires_soon {
            let expiring_soon = (days_until_expiration > 0)
                .then(|| {
                    format!(
                        "soon, in {days_until_expiration} day{}",
                        if days_until_expiration > 1 { "s" } else { "" }
                    )
                })
                .unwrap_or_else(|| "today".to_string());

            let expiring_message = format!("Operator license will expire {expiring_soon}!",);

            progress.warning(&expiring_message);
            warn!(expiring_message);
        } else if is_trial {
            let good_validity_message =
                format!("Operator license is valid for {days_until_expiration} more days.");

            progress.info(&good_validity_message);
            info!(good_validity_message);
        }

        Self::check_config(config, &operator)?;

        let client_certificate = Self::get_client_certificate(&operator_api, &operator)
            .await
            .ok()
            .flatten();
        let metadata = OperatorSessionMetadata::new(
            client_certificate,
            operator.spec.license.fingerprint,
            operator.spec.features.unwrap_or_default(),
            operator
                .spec
                .protocol_version
                .and_then(|str_version| str_version.parse().ok()),
            operator.spec.copy_target_enabled,
        );

        metadata.set_operator_properties(analytics);

        let mut version_progress = progress.subtask("comparing versions");
        let operator_version = Version::parse(&operator.spec.operator_version)
            .expect("failed to parse operator version from operator crd"); // TODO: Remove expect

        let mirrord_version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        if operator_version > mirrord_version {
            // we make two sub tasks since it looks best this way
            version_progress.warning(
                    &format!(
                        "Your mirrord plugin/CLI version {} does not match the operator version {}. This can lead to unforeseen issues.",
                        mirrord_version,
                        operator_version));
            version_progress.success(None);
            version_progress = progress.subtask("comparing versions");
            version_progress.warning(
                "Consider updating your mirrord plugin/CLI to match the operator version.",
            );
        }
        version_progress.success(None);

        let target_to_connect = if config.feature.copy_target.enabled {
            // We do not validate the `target` here, it's up to the operator.
            let mut copy_progress = progress.subtask("copying target");
            let copied = operator_api
                .copy_target(
                    &metadata,
                    config.target.path.clone().unwrap_or(Target::Targetless),
                    config.feature.copy_target.scale_down,
                )
                .await?;
            copy_progress.success(None);

            OperatorSessionTarget::Copied(copied)
        } else {
            let raw_target = operator_api.fetch_target().await?;
            OperatorSessionTarget::Raw(raw_target)
        };

        let session_info = OperatorSessionInformation {
            target: target_to_connect,
            metadata,
        };
        let connection = operator_api.connect_target(session_info).await?;

        Ok(connection)
    }

    /// Connects to exisiting operator session based on the given [`LayerConfig`] and
    /// [`OperatorSessionInformation`].
    pub async fn connect<R: Reporter>(
        config: &LayerConfig,
        session_information: OperatorSessionInformation,
        analytics: &mut R,
    ) -> Result<OperatorSessionConnection> {
        session_information
            .metadata
            .set_operator_properties(analytics);

        let operator_api = OperatorApi::new(config).await?;
        operator_api.connect_target(session_information).await
    }

    pub async fn new(config: &LayerConfig) -> Result<Self> {
        let target_config = config.target.clone();
        let on_concurrent_steal = config.feature.network.incoming.on_concurrent_steal;

        let client = create_kube_api(
            config.accept_invalid_certificates,
            config.kubeconfig.clone(),
            config.kube_context.clone(),
        )
        .await
        .map_err(OperatorApiError::CreateApiError)?;

        let target_namespace = if target_config.path.is_some() {
            target_config.namespace.clone()
        } else {
            // When targetless, pass agent namespace to operator so that it knows where to create
            // the agent (the operator does not get the agent config).
            config.agent.namespace.clone()
        };

        let target_api: Api<TargetCrd> = get_k8s_resource_api(&client, target_namespace.as_deref());
        let copy_target_api: Api<CopyTargetCrd> =
            get_k8s_resource_api(&client, target_namespace.as_deref());

        Ok(OperatorApi {
            client,
            target_api,
            copy_target_api,
            target_namespace,
            target_config,
            on_concurrent_steal,
        })
    }

    #[tracing::instrument(level = "trace", skip(self), ret)]
    async fn fetch_operator(&self) -> Result<MirrordOperatorCrd> {
        let api: Api<MirrordOperatorCrd> = Api::all(self.client.clone());
        api.get(OPERATOR_STATUS_NAME)
            .await
            .map_err(|error| OperatorApiError::KubeError {
                error,
                operation: OperatorOperation::FindingOperator,
            })
    }

    /// See `operator/controller/src/target.rs::TargetProvider::get_resource`.
    #[tracing::instrument(level = "trace", fields(self.target_config), skip(self))]
    async fn fetch_target(&self) -> Result<TargetCrd> {
        let target_name = TargetCrd::target_name_by_config(&self.target_config);
        self.target_api
            .get(&target_name)
            .await
            .map_err(|error| OperatorApiError::KubeError {
                error,
                operation: OperatorOperation::FindingTarget,
            })
    }

    /// Returns a namespace of the target.
    fn namespace(&self) -> &str {
        self.target_namespace
            .as_deref()
            .unwrap_or_else(|| self.client.default_namespace())
    }

    /// Returns a connection url for the given [`OperatorSessionInformation`].
    /// This can be used to create a websocket connection with the operator.
    #[tracing::instrument(level = "debug", skip(self), ret)]
    fn connect_url(&self, session: &OperatorSessionInformation) -> String {
        match (session.metadata.proxy_feature_enabled(), &session.target) {
            (true, OperatorSessionTarget::Raw(target)) => {
                let dt = &();
                let namespace = self.namespace();
                let api_version = TargetCrd::api_version(dt);
                let plural = TargetCrd::plural(dt);

                format!(
                    "/apis/{api_version}/proxy/namespaces/{namespace}/{plural}/{}?on_concurrent_steal={}&connect=true",
                    target.name(),
                    self.on_concurrent_steal,
                )
            }
            (false, OperatorSessionTarget::Raw(target)) => {
                format!(
                    "{}/{}?on_concurrent_steal={}&connect=true",
                    self.target_api.resource_url(),
                    target.name(),
                    self.on_concurrent_steal,
                )
            }
            (true, OperatorSessionTarget::Copied(target)) => {
                let dt = &();
                let namespace = self.namespace();
                let api_version = CopyTargetCrd::api_version(dt);
                let plural = CopyTargetCrd::plural(dt);

                format!(
                    "/apis/{api_version}/proxy/namespaces/{namespace}/{plural}/{}?connect=true",
                    target
                        .meta()
                        .name
                        .as_ref()
                        .expect("missing 'copytarget' name"),
                )
            }
            (false, OperatorSessionTarget::Copied(target)) => {
                format!(
                    "{}/{}?connect=true",
                    self.copy_target_api.resource_url(),
                    target
                        .meta()
                        .name
                        .as_ref()
                        .expect("missing 'copytarget' name"),
                )
            }
        }
    }

    /// Create websocket connection to operator.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn connect_target(
        &self,
        session_info: OperatorSessionInformation,
    ) -> Result<OperatorSessionConnection> {
        let UserIdentity { name, hostname } = UserIdentity::load();

        let request = {
            let mut builder = Request::builder()
                .uri(self.connect_url(&session_info))
                .header("x-session-id", session_info.metadata.session_id.to_string());

            // Replace non-ascii (not supported in headers) chars and trim headers.
            if let Some(name) = name {
                builder = builder.header(
                    "x-client-name",
                    name.replace(|c: char| !c.is_ascii(), "").trim(),
                );
            };

            if let Some(hostname) = hostname {
                builder = builder.header(
                    "x-client-hostname",
                    hostname.replace(|c: char| !c.is_ascii(), "").trim(),
                );
            };

            match session_info.metadata.client_credentials() {
                Ok(Some(credentials)) => {
                    builder = builder.header("x-client-der", credentials);
                }
                Ok(None) => {}
                Err(err) => {
                    debug!("CredentialStore error: {err}");
                }
            }

            builder
                .body(vec![])
                .map_err(OperatorApiError::ConnectRequestBuildError)?
        };

        let connection = upgrade::connect_ws(&self.client, request)
            .await
            .map_err(|error| OperatorApiError::KubeError {
                error,
                operation: OperatorOperation::WebsocketConnection,
            })?;

        let (tx, rx) =
            ConnectionWrapper::wrap(connection, session_info.metadata.protocol_version.clone());

        Ok(OperatorSessionConnection {
            tx,
            rx,
            info: session_info,
        })
    }

    /// Creates a new [`CopyTargetCrd`] resource using the operator.
    /// This should create a new dummy pod out of the given [`Target`].
    ///
    /// # Note
    ///
    /// `copy_target` feature is not available for all target types.
    /// Target type compatibility is checked by the operator.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn copy_target(
        &self,
        session_metadata: &OperatorSessionMetadata,
        target: Target,
        scale_down: bool,
    ) -> Result<CopyTargetCrd> {
        let name = TargetCrd::target_name(&target);

        let requested = CopyTargetCrd::new(
            &name,
            CopyTargetSpec {
                target,
                idle_ttl: Some(Self::COPIED_POD_IDLE_TTL),
                scale_down,
            },
        );

        self.copy_target_api
            .create(&PostParams::default(), &requested)
            .await
            .map_err(|error| OperatorApiError::KubeError {
                error,
                operation: OperatorOperation::CopyingTarget,
            })
    }

    /// List targets using the operator
    #[tracing::instrument(level = "trace", ret)]
    pub async fn list_targets(config: &LayerConfig) -> Result<Vec<TargetCrd>> {
        let client = create_kube_api(
            config.accept_invalid_certificates,
            config.kubeconfig.clone(),
            config.kube_context.clone(),
        )
        .await
        .map_err(OperatorApiError::CreateApiError)?;

        let target_api: Api<TargetCrd> =
            get_k8s_resource_api(&client, config.target.namespace.as_deref());
        target_api
            .list(&ListParams::default())
            .await
            .map_err(|error| OperatorApiError::KubeError {
                error,
                operation: OperatorOperation::ListingTargets,
            })
            .map(|list| list.items)
    }
}

#[derive(Error, Debug)]
enum ConnectionWrapperError {
    #[error(transparent)]
    DecodeError(#[from] bincode::error::DecodeError),
    #[error(transparent)]
    EncodeError(#[from] bincode::error::EncodeError),
    #[error(transparent)]
    WsError(#[from] TungsteniteError),
    #[error("invalid message: {0:?}")]
    InvalidMessage(Message),
    #[error("message channel is closed")]
    ChannelClosed,
}

pub struct ConnectionWrapper<T> {
    connection: T,
    client_rx: Receiver<ClientMessage>,
    daemon_tx: Sender<DaemonMessage>,
    protocol_version: Option<semver::Version>,
}

impl<T> ConnectionWrapper<T>
where
    for<'stream> T: StreamExt<Item = Result<Message, TungsteniteError>>
        + SinkExt<Message, Error = TungsteniteError>
        + Send
        + Unpin
        + 'stream,
{
    fn wrap(
        connection: T,
        protocol_version: Option<semver::Version>,
    ) -> (Sender<ClientMessage>, Receiver<DaemonMessage>) {
        let (client_tx, client_rx) = mpsc::channel(CONNECTION_CHANNEL_SIZE);
        let (daemon_tx, daemon_rx) = mpsc::channel(CONNECTION_CHANNEL_SIZE);

        let connection_wrapper = ConnectionWrapper {
            protocol_version,
            connection,
            client_rx,
            daemon_tx,
        };

        tokio::spawn(async move {
            if let Err(err) = connection_wrapper.start().await {
                error!("{err:?}")
            }
        });

        (client_tx, daemon_rx)
    }

    async fn handle_client_message(
        &mut self,
        client_message: ClientMessage,
    ) -> Result<(), ConnectionWrapperError> {
        let payload = bincode::encode_to_vec(client_message, bincode::config::standard())?;

        self.connection.send(payload.into()).await?;

        Ok(())
    }

    async fn handle_daemon_message(
        &mut self,
        daemon_message: Result<Message, TungsteniteError>,
    ) -> Result<(), ConnectionWrapperError> {
        match daemon_message? {
            Message::Binary(payload) => {
                let (daemon_message, _) = bincode::decode_from_slice::<DaemonMessage, _>(
                    &payload,
                    bincode::config::standard(),
                )?;

                self.daemon_tx
                    .send(daemon_message)
                    .await
                    .map_err(|_| ConnectionWrapperError::ChannelClosed)
            }
            message => Err(ConnectionWrapperError::InvalidMessage(message)),
        }
    }

    async fn start(mut self) -> Result<(), ConnectionWrapperError> {
        loop {
            tokio::select! {
                client_message = self.client_rx.recv() => {
                    match client_message {
                        Some(ClientMessage::SwitchProtocolVersion(version)) => {
                            if let Some(operator_protocol_version) = self.protocol_version.as_ref() {
                                self.handle_client_message(ClientMessage::SwitchProtocolVersion(operator_protocol_version.min(&version).clone())).await?;
                            } else {
                                self.daemon_tx
                                    .send(DaemonMessage::SwitchProtocolVersionResponse(
                                        "1.2.1".parse().expect("Bad static version"),
                                    ))
                                    .await
                                    .map_err(|_| ConnectionWrapperError::ChannelClosed)?;
                            }
                        }
                        Some(client_message) => self.handle_client_message(client_message).await?,
                        None => break,
                    }
                }
                daemon_message = self.connection.next() => {
                    match daemon_message {
                        Some(daemon_message) => self.handle_daemon_message(daemon_message).await?,
                        None => break,
                    }
                }
            }
        }

        let _ = self.connection.send(Message::Close(None)).await;

        Ok(())
    }
}

mod upgrade {
    //! Code copied from [`kube::client`] and adjusted.
    //!
    //! Just like original [`Client::connect`] function, [`connect_ws`] creates a
    //! WebSockets connection. However, original function swallows
    //! [`ErrorResponse`] sent by the operator and returns flat
    //! [`UpgradeConnectionError`]. [`connect_ws`] attempts to
    //! recover the [`ErrorResponse`] - if operator response code is not
    //! [`StatusCode::SWITCHING_PROTOCOLS`], it tries to read
    //! response body and deserialize it.

    use base64::Engine;
    use http::{HeaderValue, Request, Response, StatusCode};
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;
    use kube::{
        client::{Body, UpgradeConnectionError},
        core::ErrorResponse,
        Client, Error, Result,
    };
    use tokio_tungstenite::{tungstenite::protocol::Role, WebSocketStream};

    const WS_PROTOCOL: &str = "v4.channel.k8s.io";

    // Verify upgrade response according to RFC6455.
    // Based on `tungstenite` and added subprotocol verification.
    async fn verify_response(res: Response<Body>, key: &HeaderValue) -> Result<Response<Body>> {
        let status = res.status();

        if status != StatusCode::SWITCHING_PROTOCOLS {
            if status.is_client_error() || status.is_server_error() {
                let error_response = res
                    .into_body()
                    .collect()
                    .await
                    .ok()
                    .map(|body| body.to_bytes())
                    .and_then(|body_bytes| {
                        serde_json::from_slice::<ErrorResponse>(&body_bytes).ok()
                    });

                if let Some(error_response) = error_response {
                    return Err(Error::Api(error_response));
                }
            }

            return Err(Error::UpgradeConnection(
                UpgradeConnectionError::ProtocolSwitch(status),
            ));
        }

        let headers = res.headers();
        if !headers
            .get(http::header::UPGRADE)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        {
            return Err(Error::UpgradeConnection(
                UpgradeConnectionError::MissingUpgradeWebSocketHeader,
            ));
        }

        if !headers
            .get(http::header::CONNECTION)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.eq_ignore_ascii_case("Upgrade"))
            .unwrap_or(false)
        {
            return Err(Error::UpgradeConnection(
                UpgradeConnectionError::MissingConnectionUpgradeHeader,
            ));
        }

        let accept_key = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_ref());
        if !headers
            .get(http::header::SEC_WEBSOCKET_ACCEPT)
            .map(|h| h == &accept_key)
            .unwrap_or(false)
        {
            return Err(Error::UpgradeConnection(
                UpgradeConnectionError::SecWebSocketAcceptKeyMismatch,
            ));
        }

        // Make sure that the server returned the correct subprotocol.
        if !headers
            .get(http::header::SEC_WEBSOCKET_PROTOCOL)
            .map(|h| h == WS_PROTOCOL)
            .unwrap_or(false)
        {
            return Err(Error::UpgradeConnection(
                UpgradeConnectionError::SecWebSocketProtocolMismatch,
            ));
        }

        Ok(res)
    }

    /// Generate a random key for the `Sec-WebSocket-Key` header.
    /// This must be nonce consisting of a randomly selected 16-byte value in base64.
    fn sec_websocket_key() -> HeaderValue {
        let random: [u8; 16] = rand::random();
        base64::engine::general_purpose::STANDARD
            .encode(random)
            .parse()
            .expect("should be valid")
    }

    pub async fn connect_ws(
        client: &Client,
        request: Request<Vec<u8>>,
    ) -> kube::Result<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>> {
        let (mut parts, body) = request.into_parts();
        parts.headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        parts
            .headers
            .insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        parts.headers.insert(
            http::header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );
        let key = sec_websocket_key();
        parts
            .headers
            .insert(http::header::SEC_WEBSOCKET_KEY, key.clone());
        // Use the binary subprotocol v4, to get JSON `Status` object in `error` channel (3).
        // There's no official documentation about this protocol, but it's described in
        // [`k8s.io/apiserver/pkg/util/wsstream/conn.go`](https://git.io/JLQED).
        // There's a comment about v4 and `Status` object in
        // [`kublet/cri/streaming/remotecommand/httpstream.go`](https://git.io/JLQEh).
        parts.headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(WS_PROTOCOL),
        );

        let res = client
            .send(Request::from_parts(parts, Body::from(body)))
            .await?;
        let res = verify_response(res, &key).await?;
        match hyper::upgrade::on(res).await {
            Ok(upgraded) => {
                Ok(
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None)
                        .await,
                )
            }

            Err(e) => Err(Error::UpgradeConnection(
                UpgradeConnectionError::GetPendingUpgrade(e),
            )),
        }
    }
}
