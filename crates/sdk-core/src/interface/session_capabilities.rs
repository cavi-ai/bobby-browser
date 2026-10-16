//! Discovery combines observed implementation state with the same live grants used by dispatch.
use super::*;
use types::{
    SessionCapabilities, SessionConnectionState, SessionFeatureAvailability,
    SessionFeatureUnavailableReason as Reason,
};
use worker_pool::EngineFeature;

impl AuthenticatedRuntime {
    pub(super) async fn inspect_session_capabilities(
        &self,
        ctx: RequestContext,
        id: SessionId,
    ) -> InterfaceResult<SessionCapabilities> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadSession)?;
        self.require_owned_session(&ctx, &id)?;
        self.inner.sessions.get(&id).await.map_err(|_| {
            error_with(
                &ctx,
                InterfaceErrorCode::NotFound,
                "runtime resource was not found",
            )
        })?;
        let observation = self.inner.sessions.feature_observation(&id).await;
        // Authorization and ownership can change while observation awaits a lock.
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadSession)?;
        self.require_owned_session(&ctx, &id)?;
        let session = self.inner.sessions.get(&id).await.map_err(|_| {
            error_with(
                &ctx,
                InterfaceErrorCode::NotFound,
                "runtime resource was not found",
            )
        })?;
        let connection = observation
            .as_ref()
            .map(|o| o.connection)
            .unwrap_or_default();
        let support = observation.as_ref().and_then(|o| o.support.as_ref());
        let writable = self
            .idempotency
            .ensure_writable(&ctx.correlation_id)
            .is_ok()
            && self
                .lifecycle_idempotency
                .ensure_writable(&ctx.correlation_id)
                .is_ok();
        let features = EngineFeature::ALL
            .iter()
            .map(|feature| {
                let supported = support.map(|support| support.features.contains(feature));
                let javascript = *feature == EngineFeature::JavaScriptEvaluation;
                let mut required = InterfaceOperation::SubmitCommand.required().to_vec();
                if javascript {
                    required.push(Capability::JavascriptEvaluate);
                }
                let granted = required.iter().all(|capability| {
                    self.authorization
                        .require_capability(&ctx, *capability)
                        .is_ok()
                });
                let reason = if supported == Some(false) {
                    Some(Reason::UnsupportedByEngine)
                } else if !granted {
                    Some(Reason::MissingCapability)
                } else if javascript && !session.execution_policy.javascript_evaluation {
                    Some(Reason::PolicyDisabled)
                } else if !writable {
                    Some(Reason::StorageUnavailable)
                } else if observation.is_none() {
                    Some(Reason::WorkerUnavailable)
                } else if support.is_none() {
                    Some(Reason::MetadataUnavailable)
                } else if connection == SessionConnectionState::Disconnected {
                    Some(Reason::Disconnected)
                } else if connection == SessionConnectionState::Unknown {
                    Some(Reason::ConnectionUnknown)
                } else {
                    None
                };
                SessionFeatureAvailability {
                    name: feature.name().into(),
                    supported,
                    available: reason.is_none(),
                    reason,
                    required_capabilities: required,
                    policy_requirement: javascript.then(|| "javascriptEvaluation".into()),
                }
            })
            .collect();
        Ok(SessionCapabilities {
            schema_version: SessionCapabilities::SCHEMA_VERSION,
            session_id: id,
            observed_at: Utc::now(),
            engine: support.map(|s| s.engine_name().into()),
            connection,
            features,
        })
    }
}
