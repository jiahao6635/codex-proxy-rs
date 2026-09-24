use std::{
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime},
};

use gateway_admin::{
    model::{AdminErrorKind, Revision},
    ports::plugins::PluginPreparation,
};
use gateway_core::{
    account::{AccountSelectionPolicy, OpaqueProviderData, RotationStrategy},
    engine::{
        AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
        provider::ProviderRequest,
    },
    lifecycle::CancellationToken,
    policy::ClientApiKeyId,
    routing::{ProviderKind, PublicModelId, RoutingContext, UpstreamModelId},
};
use serde_json::{Map, Value, json};

use crate::support::environment::{Environment, account_grant};

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("object fixture").clone()
}

fn request_profiles() -> Value {
    let presentation = |channel: &str, version: &str| {
        json!({
            "product":"Example CLI",
            "version":version,
            "build":format!("build-{version}"),
            "target":{"os_type":"linux","os_version":"6.8","arch":"x86_64","terminal":"headless"},
            "user_agent":format!("Example CLI/{version}"),
            "attributes":[{"label":"通道","value":channel}],
            "verified_at_ms":1_780_000_000_000_i64,
            "release":{"status":"current","checked_at_ms":1_780_000_000_100_i64,"latest_version":version},
        })
    };
    json!({
        "default_configuration":{"channel":"stable"},
        "options":[
            {
                "id":"stable","label":"稳定版","description":"默认核验版本",
                "configuration":{"channel":"stable"},
                "resolved":{"upstreamIdentity":"stable-wire"},
                "presentation":presentation("stable", "1.2.3"),
            },
            {
                "id":"beta","label":"测试版",
                "configuration":{"channel":"beta"},
                "resolved":{"upstreamIdentity":"beta-wire"},
                "presentation":presentation("beta", "1.3.0-beta.1"),
            }
        ]
    })
}

#[tokio::test]
async fn future_candidate_reads_current_profile_facts_and_cached_candidates_reject_stale_sources() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let (runtime, core) = environment
        .provider(
            json!({"request_profiles": request_profiles()}),
            vec![account_grant("accounts")],
        )
        .await;
    let mut candidate = environment
        .store
        .admin_ports()
        .plugins()
        .load_instances()
        .await
        .unwrap();
    let source_revision = candidate.config_revision;
    candidate.config_revision = Revision::new(source_revision.get() + 1).unwrap();
    candidate.instances[0].revision = candidate.config_revision;
    let prepared = runtime
        .prepare(source_revision, candidate.clone())
        .await
        .unwrap();
    let error = runtime
        .prepare(candidate.config_revision, candidate.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Conflict);
    assert_eq!(
        environment
            .store
            .admin_ports()
            .plugins()
            .load_instances()
            .await
            .unwrap()
            .config_revision,
        source_revision,
        "候选准备不能提前提交配置"
    );
    environment
        .set_request_profile("example", object(json!({"channel":"beta"})))
        .await;
    let error = runtime
        .prepare(source_revision, candidate)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        AdminErrorKind::Conflict,
        "缓存命中不能绕过源事实版本校验"
    );
    drop(prepared);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn request_profiles_validate_cached_candidates_and_reach_the_real_worker() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let marker = environment.directory.path().join("request-profile.json");
    let _account_id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({
                "request_profiles":request_profiles(),
                "request_profile_marker":marker,
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let provider_kind = ProviderKind::new("example").unwrap();
    let admin_registry = runtime.admin_registry(core.snapshots());
    assert_eq!(
        admin_registry.client_profile_providers().unwrap(),
        vec![provider_kind.clone()]
    );
    let admin = admin_registry.require(&provider_kind).unwrap();
    let options = admin.client_profile_options().unwrap();
    assert_eq!(
        options.expose_to_provider()["presets"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        !serde_json::to_string(options.expose_to_provider())
            .unwrap()
            .contains("stable-wire"),
        "管理选项不能暴露 Provider resolved 文档"
    );
    let stable = admin.default_client_profile().unwrap();
    assert_eq!(stable.expose_to_provider()["channel"], "stable");
    let beta = OpaqueProviderData::new(object(json!({"channel":"beta"})));
    let preview = admin.preview_client_profile(&beta).unwrap();
    assert_eq!(
        preview.expose_to_provider()["userAgent"],
        "Example CLI/1.3.0-beta.1"
    );
    assert_eq!(
        admin.configured_wire_profile(&beta).unwrap().user_agent,
        "Example CLI/1.3.0-beta.1"
    );

    environment
        .set_request_profile("example", object(json!({"channel":"removed"})))
        .await;
    let invalid_candidate = environment
        .store
        .admin_ports()
        .plugins()
        .load_instances()
        .await
        .unwrap();
    let error = runtime
        .prepare(invalid_candidate.config_revision, invalid_candidate)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Invalid);

    environment
        .set_request_profile("example", beta.expose_to_provider().clone())
        .await;
    let valid_candidate = environment
        .store
        .admin_ports()
        .plugins()
        .load_instances()
        .await
        .unwrap();
    let prepared = runtime
        .prepare(valid_candidate.config_revision, valid_candidate)
        .await
        .unwrap();
    assert_eq!(
        prepared.id(),
        core.snapshots()
            .acquire()
            .unwrap()
            .extensions()
            .unwrap()
            .id(),
        "仅全局选择变化应复用同一插件代次"
    );

    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers.get(&provider_kind).unwrap();
    let default = provider.default_request_profile().unwrap().unwrap();
    assert_eq!(
        default.expose_to_provider()["upstreamIdentity"],
        "stable-wire"
    );
    let resolved = provider.resolve_request_profile(&beta).unwrap();
    assert_eq!(
        resolved.expose_to_provider()["upstreamIdentity"],
        "beta-wire"
    );
    let model = UpstreamModelId::new("plugin-model").unwrap();
    let operation = admin
        .connection_test_operation(&model, "profile")
        .await
        .unwrap();
    let plan = snapshot
        .plan(
            &PublicModelId::new("plugin-model").unwrap(),
            &operation,
            snapshot.all_account_scope(),
            &RoutingContext::default(),
        )
        .unwrap();
    let request = RequestAttemptContext::new(
        ModelRequestId::new("req_plugin_request_profile").unwrap(),
        ClientApiKeyId::new("key_plugin_request_profile").unwrap(),
    )
    .with_request_profile(Some(resolved.clone()));
    let attempt = AttemptContext::new(
        request,
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(30),
        AccountSelectionPolicy::new(
            RotationStrategy::RoundRobin,
            NonZeroU32::MIN,
            Duration::ZERO,
        ),
        AccountAttemptContext::default().with_account_scope(snapshot.all_account_scope()),
        None,
        CancellationToken::new(),
    );
    let response = Arc::clone(provider)
        .execute(
            ProviderRequest::new(operation, plan.candidates()[0].clone()),
            attempt,
        )
        .await
        .unwrap();
    let sent: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert_eq!(sent, Value::Object(resolved.into_inner()));

    drop(response);
    drop(prepared);
    drop(providers);
    drop(snapshot);
    drop(admin);
    drop(admin_registry);
    drop(core);
    drop(runtime);
    environment.close().await;
}
