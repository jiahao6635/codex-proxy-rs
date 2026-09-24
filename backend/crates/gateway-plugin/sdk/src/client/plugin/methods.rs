//! 类型化业务方法目录；控制参数、敏感载荷、阶段与响应流在此固定。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    Capability as C, Stage as S,
    call::{
        auth, frontend_authentication as frontend, host, management, observation, policy, provider,
    },
};

use super::{
    Empty, Method,
    typed::{
        decode_execution_input, decode_metadata, decode_metadata_without_payload, decode_payload,
        encode_execution_stream, encode_metadata, encode_metadata_stream,
        encode_metadata_with_payload, encode_payload,
    },
};

pub const MANAGEMENT_REGISTER: Method<Empty, management::ManagementRegistration> = Method::new(
    "management.register",
    &[C::Management],
    &[S::Registration],
    decode_metadata_without_payload,
    encode_payload,
);
pub const MANAGEMENT_HANDLE: Method<management::ManagementRequest, management::ManagementResponse> =
    Method::new(
        "management.handle",
        &[C::Management],
        &[S::Management],
        decode_metadata,
        encode_metadata_with_payload,
    );
pub const MANAGEMENT_CALLBACK: Method<
    management::ManagementRequest,
    management::ManagementResponse,
> = Method::new(
    "management.callback",
    &[C::Management],
    &[S::PublicManagement],
    decode_metadata_without_payload,
    encode_metadata_with_payload,
);
pub const COMMAND_LINE_REGISTER: Method<Empty, management::CommandRegistration> = Method::new(
    "command_line.register",
    &[C::CommandLine],
    &[S::Registration],
    decode_metadata_without_payload,
    encode_payload,
);
pub const COMMAND_LINE_EXECUTE: Method<management::CommandInvocation, management::CommandResult> =
    Method::new(
        "command_line.execute",
        &[C::CommandLine],
        &[S::CommandLine],
        decode_payload,
        encode_payload,
    );
pub const ROUTE_MODEL: Method<policy::ModelRouteRequest, policy::ModelRouteDecision> = Method::new(
    "policy.route_model",
    &[C::ModelRouter],
    &[S::Routing],
    decode_metadata,
    encode_metadata,
);
pub const SCHEDULE_ACCOUNT: Method<
    policy::AccountScheduleRequest,
    policy::AccountScheduleDecision,
> = Method::new(
    "policy.schedule_account",
    &[C::Scheduler],
    &[S::Scheduling],
    decode_metadata_without_payload,
    encode_metadata,
);
pub const OBSERVE_REQUEST: Method<policy::ObserveRequest, Empty> = Method::new(
    "policy.observe_request",
    &[C::RequestLifecycle, C::Usage],
    &[S::Observation],
    decode_payload,
    encode_metadata,
);
pub const OBSERVE_WEBSOCKET: Method<observation::ObserveWebSocketResponse, Empty> = Method::new(
    "websocket.response_event",
    &[C::WebSocketObserver],
    &[S::Observation],
    decode_metadata,
    encode_metadata,
);
pub const FRONTEND_IDENTIFIER: Method<Empty, frontend::FrontendAuthenticationIdentifier> =
    Method::new(
        "frontend_auth.identifier",
        &[C::FrontendAuthentication],
        &[S::Registration],
        decode_metadata_without_payload,
        encode_metadata,
    );
pub const FRONTEND_AUTHENTICATE: Method<
    frontend::FrontendAuthenticationRequest,
    frontend::FrontendAuthenticationResult,
> = Method::new(
    "frontend_auth.authenticate",
    &[C::FrontendAuthentication],
    &[S::Authentication],
    decode_payload,
    encode_payload,
);
pub const STATE_MIGRATE: Method<host::StateMigrationRequest, host::StateMigrationResult> =
    Method::new(
        "plugin.state.migrate",
        &[],
        &[S::Configuration],
        decode_payload,
        encode_payload,
    );

pub const PREPARE_EXECUTION: Method<provider::ExecutionInput, provider::PreparedExecution> =
    Method::new(
        "provider.prepare",
        &[C::Executor],
        &[S::Attempt],
        decode_execution_input,
        encode_metadata,
    );
pub const EXECUTE: Method<provider::ExecutePrepared, Empty> = Method::new(
    "provider.execute",
    &[C::Executor],
    &[S::Execution],
    decode_metadata_without_payload,
    encode_execution_stream,
);
pub const DISCARD_EXECUTION: Method<provider::ExecutePrepared, Empty> = Method::new(
    "provider.discard",
    &[C::Executor],
    &[S::Attempt],
    decode_metadata_without_payload,
    encode_metadata,
);
pub const CONNECTION_TEST: Method<ConnectionTest, provider::ProbeOperation> = Method::new(
    "provider.connection_test",
    &[C::Executor],
    &[S::Configuration],
    decode_metadata_without_payload,
    encode_metadata,
);
pub const MODELS: Method<provider::account::AccountRequest, provider::models::AccountModels> =
    Method::new(
        "provider.models",
        &[C::Models],
        &[S::Management, S::Maintenance],
        decode_payload,
        encode_payload,
    );
pub const QUOTA: Method<provider::account::AccountRequest, provider::quota::Quota> = Method::new(
    "provider.quota",
    &[C::Quota],
    &[S::Management, S::Maintenance],
    decode_payload,
    encode_payload,
);
pub const IMPORT_CREDENTIALS: Method<Map<String, Value>, auth::ImportedCredentials> = Method::new(
    "provider.credentials.import",
    &[C::Authentication],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const EXPORT_CREDENTIALS: Method<Vec<auth::ExportCredential>, Map<String, Value>> = Method::new(
    "provider.credentials.export",
    &[C::Authentication],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const ROTATE_CREDENTIALS: Method<auth::RotateCredential, auth::CredentialFacts> = Method::new(
    "provider.credentials.rotate",
    &[C::Authentication],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const REFRESH_CREDENTIALS: Method<auth::RotateCredential, auth::CredentialFacts> = Method::new(
    "provider.credentials.refresh",
    &[C::Authentication],
    &[S::Management, S::Maintenance],
    decode_payload,
    encode_payload,
);
pub const LOGIN_START: Method<auth::LoginStart, auth::LoginStarted> = Method::new(
    "provider.login.start",
    &[C::Authentication],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const LOGIN_POLL: Method<auth::LoginPoll, auth::LoginPollResult> = Method::new(
    "provider.login.poll",
    &[C::Authentication],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const ACCOUNT_CONFIGURATION: Method<
    provider::account::AccountRequest,
    provider::account::AccountConfiguration,
> = Method::new(
    "provider.account_configuration",
    &[C::Authentication],
    &[S::Configuration],
    decode_payload,
    encode_payload,
);
pub const ACCOUNT_CHANGED: Method<provider::account::AccountInvalidation, Empty> = Method::new(
    "provider.account_changed",
    &[C::AccountManagement],
    &[S::Observation],
    decode_payload,
    encode_metadata,
);
pub const PROFILE: Method<provider::account::AccountRequest, provider::account::Profile> =
    Method::new(
        "provider.profile",
        &[C::AccountManagement],
        &[S::Management],
        decode_payload,
        encode_payload,
    );
pub const SUBSCRIPTION: Method<
    provider::account::AccountRequest,
    Option<provider::account::Subscription>,
> = Method::new(
    "provider.subscription",
    &[C::AccountManagement],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const AVATAR: Method<provider::account::AccountRequest, provider::account::Avatar> =
    Method::new(
        "provider.avatar",
        &[C::AccountManagement],
        &[S::Management],
        decode_payload,
        encode_metadata_stream,
    );
pub const RESET_CREDITS: Method<
    provider::account::AccountRequest,
    provider::reset_credits::Reply<provider::reset_credits::Credits>,
> = Method::new(
    "provider.reset_credits",
    &[C::AccountManagement],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const CONSUME_RESET_CREDIT: Method<
    provider::reset_credits::ConsumeRequest,
    provider::reset_credits::Reply<provider::reset_credits::ConsumeResult>,
> = Method::new(
    "provider.consume_reset_credit",
    &[C::AccountManagement],
    &[S::Management],
    decode_payload,
    encode_payload,
);
pub const REFRESH_REQUEST_PROFILES: Method<Empty, provider::RequestProfileRefresh> = Method::new(
    "provider.request_profiles.refresh",
    &[C::RequestProfile],
    &[S::Maintenance],
    decode_payload,
    encode_payload,
);

/// 宿主连接测试的非敏感业务参数。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionTest {
    pub model: String,
    pub input: String,
}
