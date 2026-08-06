use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use agent_client_protocol::{
    schema::{
        v1::{AuthMethod, AuthMethodId, AuthenticateRequest, InitializeRequest},
        ProtocolVersion,
    },
    AcpAgent, AcpAgentConfig, Agent, ConnectionTo, ErrorCode as AcpErrorCode,
};
use async_trait::async_trait;
use auth_broker::{
    AuthCapabilities, AuthChallenge, AuthDriver, AuthError, AuthInput, AuthProfileId, AuthProgress,
    AuthStatus, AuthStrategy, CredentialHandle,
};

const OAUTH_AUTHORIZATION_CODE_ID: &str = "oauth-authorization-code";
const OAUTH_DEVICE_CODE_ID: &str = "oauth-device-code";
const ENVIRONMENT_ID: &str = "environment";
const EXISTING_SESSION_ID: &str = "existing-session";

#[derive(Debug, Clone)]
pub struct AcpAuthDriver {
    launch: AcpAgentConfig,
    timeout: Duration,
}

impl AcpAuthDriver {
    pub fn new(command: impl Into<PathBuf>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            launch: AcpAgentConfig::new(command).args(args),
            timeout: Duration::from_secs(30),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn capabilities_from_auth_methods(methods: &[AuthMethod]) -> AuthCapabilities {
        let mut strategies = BTreeSet::from([AuthStrategy::None]);
        if !methods.is_empty() {
            strategies.insert(AuthStrategy::Advertised);
        }
        for method in methods {
            if let Some(strategy) = strategy_for_method(method) {
                strategies.insert(strategy);
            }
        }
        AuthCapabilities::new(strategies)
    }

    pub fn select_method_for_strategy(
        methods: &[AuthMethod],
        strategy: AuthStrategy,
    ) -> Result<Option<AuthMethodId>, AuthError> {
        match strategy {
            AuthStrategy::None | AuthStrategy::ExistingSession => Ok(None),
            AuthStrategy::Advertised => Ok(methods.first().map(|method| method.id().clone())),
            AuthStrategy::OAuthAuthorizationCode => {
                method_for_id(methods, OAUTH_AUTHORIZATION_CODE_ID).map(Some)
            }
            AuthStrategy::OAuthDeviceCode => method_for_id(methods, OAUTH_DEVICE_CODE_ID).map(Some),
            AuthStrategy::Environment => method_for_id(methods, ENVIRONMENT_ID).map(Some),
        }
    }

    async fn initialize(&self) -> Result<Vec<AuthMethod>, AuthError> {
        let launch = self.launch.clone();
        let agent = AcpAgent::new(launch);
        let initialized = tokio::time::timeout(self.timeout, async {
            agent_client_protocol::Client
                .builder()
                .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await
                })
                .await
        })
        .await
        .map_err(|_| AuthError::Transport("ACP initialize timed out".into()))?
        .map_err(|error| AuthError::Transport(error.to_string()))?;
        Ok(initialized.auth_methods)
    }

    async fn authenticate(
        &self,
        profile: &AuthProfileId,
        strategy: AuthStrategy,
        method_id: AuthMethodId,
    ) -> Result<AuthProgress, AuthError> {
        let launch = self.launch.clone();
        let agent = AcpAgent::new(launch);
        let method_id_for_error = method_id.0.to_string();
        let profile = profile.clone();
        let outcome = tokio::time::timeout(self.timeout, async {
            agent_client_protocol::Client
                .builder()
                .connect_with(agent, move |connection: ConnectionTo<Agent>| {
                    let method_id = method_id.clone();
                    let method_id_for_error = method_id_for_error.clone();
                    let profile = profile.clone();
                    async move {
                        connection
                            .send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        match connection
                            .send_request(AuthenticateRequest::new(method_id.clone()))
                            .block_task()
                            .await
                        {
                            Ok(_response) => {
                                Ok(AuthenticateOutcome::Authenticated(CredentialHandle::new(
                                    profile,
                                    strategy,
                                    Arc::new(method_id.0.to_string()),
                                )))
                            }
                            Err(error) if error.code == AcpErrorCode::AuthRequired => {
                                Ok(AuthenticateOutcome::Pending(AuthChallenge {
                                    id: method_id_for_error,
                                    strategy,
                                    verification_uri: None,
                                    user_code: None,
                                }))
                            }
                            Err(_) => Ok(AuthenticateOutcome::Rejected),
                        }
                    }
                })
                .await
        })
        .await
        .map_err(|_| AuthError::Transport("ACP authenticate timed out".into()))?
        .map_err(|error| AuthError::Transport(error.to_string()))?;
        match outcome {
            AuthenticateOutcome::Authenticated(handle) => Ok(AuthProgress::Authenticated(handle)),
            AuthenticateOutcome::Pending(challenge) => Ok(AuthProgress::Pending(challenge)),
            AuthenticateOutcome::Rejected => Err(AuthError::Rejected),
        }
    }
}

#[async_trait]
impl AuthDriver for AcpAuthDriver {
    async fn discover(&self, _profile: &AuthProfileId) -> Result<AuthCapabilities, AuthError> {
        let methods = self.initialize().await?;
        Ok(Self::capabilities_from_auth_methods(&methods))
    }

    async fn begin(
        &self,
        profile: &AuthProfileId,
        strategy: AuthStrategy,
    ) -> Result<AuthProgress, AuthError> {
        if strategy == AuthStrategy::None {
            return Ok(AuthProgress::Authenticated(CredentialHandle::new(
                profile.clone(),
                strategy,
                Arc::new(()),
            )));
        }
        let methods = self.initialize().await?;
        let Some(method_id) = Self::select_method_for_strategy(&methods, strategy)? else {
            return Ok(AuthProgress::Authenticated(CredentialHandle::new(
                profile.clone(),
                strategy,
                Arc::new(()),
            )));
        };
        self.authenticate(profile, strategy, method_id).await
    }

    async fn continue_auth(
        &self,
        _challenge: &AuthChallenge,
        _input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        Err(AuthError::InvalidTransition)
    }

    async fn refresh(&self, handle: &CredentialHandle) -> Result<AuthProgress, AuthError> {
        match self.health(handle).await {
            AuthStatus::Healthy => Ok(AuthProgress::Authenticated(handle.clone())),
            AuthStatus::ReauthenticationRequired => {
                self.begin(handle.profile(), handle.strategy()).await
            }
            AuthStatus::Unavailable => Err(AuthError::Transport("ACP harness unavailable".into())),
            AuthStatus::PendingUserAction | AuthStatus::Revoked => {
                Err(AuthError::InvalidTransition)
            }
        }
    }

    async fn revoke(&self, _handle: CredentialHandle) -> Result<(), AuthError> {
        Ok(())
    }

    async fn health(&self, handle: &CredentialHandle) -> AuthStatus {
        let Ok(methods) = self.initialize().await else {
            return AuthStatus::Unavailable;
        };
        let Some(method_id) = handle.payload::<String>() else {
            return AuthStatus::Healthy;
        };
        if methods
            .iter()
            .any(|method| method.id().0.as_ref() == method_id)
        {
            AuthStatus::Healthy
        } else {
            AuthStatus::ReauthenticationRequired
        }
    }
}

enum AuthenticateOutcome {
    Authenticated(CredentialHandle),
    Pending(AuthChallenge),
    Rejected,
}

fn strategy_for_method(method: &AuthMethod) -> Option<AuthStrategy> {
    match method.id().0.as_ref() {
        OAUTH_AUTHORIZATION_CODE_ID => Some(AuthStrategy::OAuthAuthorizationCode),
        OAUTH_DEVICE_CODE_ID => Some(AuthStrategy::OAuthDeviceCode),
        ENVIRONMENT_ID => Some(AuthStrategy::Environment),
        EXISTING_SESSION_ID => Some(AuthStrategy::ExistingSession),
        _ => None,
    }
}

fn method_for_id(methods: &[AuthMethod], id: &str) -> Result<AuthMethodId, AuthError> {
    methods
        .iter()
        .find(|method| method.id().0.as_ref() == id)
        .map(|method| method.id().clone())
        .ok_or(AuthError::UnsupportedStrategy)
}
