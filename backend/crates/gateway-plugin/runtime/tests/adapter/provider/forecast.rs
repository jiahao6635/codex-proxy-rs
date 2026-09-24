use chrono::{DateTime, Utc};
use gateway_admin::model::provider_credentials::{
    ProviderDocument, ProviderQuotaRequest, ProviderQuotaWindow, ProviderQuotaWindowRole,
    QuotaLocalUsageAttribution,
};
use gateway_core::{
    account::OpaqueProviderData,
    engine::{
        CommitRequirement,
        execution::{ClientTransport, ExecutionRequestMetadata, StartExecution},
    },
    error::ProviderErrorKind,
    operation::{GenerateRequest, Operation, ProtocolPayload},
    routing::{ConfigRevision, ProviderKind, PublicModelId},
};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

const RESET_AT_MS: i64 = 1_800_000_000_000;

fn forecast() -> Value {
    json!({"plan_type":"plugin-pro","windows":[{
        "key":"weekly","group":"chat","limit_id":"chat-main","role":"secondary",
        "account_wide":true,"window_seconds":604800,"used_percent":25.0,"reset_at_ms":RESET_AT_MS
    }]})
}

fn configuration(value: Value) -> Value {
    json!({
        "quota_url":"http://127.0.0.1:1/not-requested",
        "execution_events":[
            {"observation":{"quota_forecast":value}},
            {"facts":[{"type":"started","id":"forecast-response","model":"plugin-model"}]},
            {"facts":[{"type":"usage","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}},
                {"type":"completed","id":"forecast-response","model":"plugin-model","reason":"stop"}]}
        ]
    })
}

fn window() -> ProviderQuotaWindow {
    ProviderQuotaWindow {
        key: "weekly".into(),
        group: "chat".into(),
        label: "Weekly quota".into(),
        limit_id: Some("chat-main".into()),
        limit_name: None,
        role: Some(ProviderQuotaWindowRole::Secondary),
        local_usage_attribution: QuotaLocalUsageAttribution::AccountWide,
        window_seconds: Some(604800),
        used_percent: Some(50.0),
        reset_at: DateTime::<Utc>::from_timestamp_millis(RESET_AT_MS),
        limit_reached: false,
        local_usage: None,
        provider_data: None,
    }
}

async fn complete_request(core: &gateway_core::CoreBundle, key: &str) {
    let execution = core.execution_service();
    let client = execution.authenticate(key).unwrap();
    let mut started = execution
        .start(StartExecution {
            client,
            public_model: PublicModelId::new("plugin-model").unwrap(),
            operation: Operation::Generate(GenerateRequest::from_protocol_payload(
                ProtocolPayload::json_object(
                    "openai",
                    json!({"model":"plugin-model","input":"forecast fixture","stream":true})
                        .as_object()
                        .unwrap()
                        .clone(),
                )
                .unwrap(),
            )),
            metadata: ExecutionRequestMetadata {
                protocol: "openai".into(),
                endpoint: "/v1/responses".into(),
                transport: ClientTransport::HttpSse,
                stream: true,
                client_ip: None,
                user_agent: Some("plugin-forecast-test".into()),
                previous_response_id: None,
            },
        })
        .await
        .unwrap();
    let mut committed = false;
    while let Some(event) = started.session.next_event().await.unwrap() {
        if !committed && event.commit_requirement() == CommitRequirement::CommitBeforeDelivery {
            started.session.commit_downstream(Some(200)).await.unwrap();
            committed = true;
        }
    }
    assert!(committed);
    assert!(started.session.is_finalized());
}

#[tokio::test]
async fn core_persists_quota_facts_and_admin_pairs_them_after_generation_changes() {
    use gateway_admin::model::{accounts::AccountUsageWindowQuery, observability::TimeRange};

    let Some(mut environment) = Environment::create_command().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account = environment.account(None).await;
    let key_id = format!("key_{}", uuid::Uuid::new_v4().simple());
    let key = format!("sk-forecast-{}", uuid::Uuid::new_v4().simple());
    environment.client_key(&key_id, &key).await;
    let reset = Utc::now() + chrono::Duration::days(3);
    let mut observed = forecast();
    observed["windows"][0]["reset_at_ms"] = json!(reset.timestamp_millis());
    let mut current = observed.clone();
    current["access"] = json!({"kind":"allowed"});
    current["windows"][0]["label"] = json!("Weekly quota");
    current["windows"][0]["used_percent"] = json!(50.0);
    current["windows"][0]["limit_reached"] = json!(false);
    let current: gateway_plugin_sdk::call::provider::quota::Quota =
        serde_json::from_value(current).unwrap();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::header(
        "authorization",
        "Bearer test-only",
    ))
    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(current))
    .expect(1)
    .mount(&server)
    .await;
    let network = account_grant("network");
    let mut config = configuration(observed.clone());
    config["quota_url"] = json!(server.uri());
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts"), network])
        .await;
    // 复用 Store 的有界写泵，断言真实 Core 结算持久化；不直接种植请求行或历史文档。
    environment.store.start_command_line_writes().unwrap();
    let begin = Utc::now();
    complete_request(&core, &key).await;

    // 新代次不再上报额度事实；历史解释不能依赖当时的子进程，也不能补造新请求的事实。
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.configuration["execution_events"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let changed = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    core.snapshot_control()
        .publish_committed(ConfigRevision::new(changed.config_revision.get()).unwrap())
        .await;
    complete_request(&core, &key).await;
    environment
        .store
        .shutdown_command_line_writes()
        .await
        .unwrap();

    let history = environment
        .store
        .admin_ports()
        .accounts()
        .load_quota_forecast_history(&AccountUsageWindowQuery {
            account_id: account.to_string(),
            key: "weekly".into(),
            range: TimeRange::new(begin, Utc::now()).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(history.pending_request_count, 0);
    assert_eq!(history.usage.request_count, 2);
    assert_eq!(history.usage.tokens, 14);
    assert_eq!(history.points.len(), 1);
    assert_eq!(history.points[0].usage.request_count, 1);
    assert_eq!(history.points[0].usage.tokens, 7);
    assert_eq!(
        history.points[0]
            .provider_observation
            .expose_to_provider()
            .expose_to_provider(),
        json!({"plugin_quota_forecast_v1":observed})
            .as_object()
            .unwrap()
    );
    let other_account = environment.account(None).await;
    let unrelated = environment
        .store
        .admin_ports()
        .accounts()
        .load_quota_forecast_history(&AccountUsageWindowQuery {
            account_id: other_account.to_string(),
            key: "weekly".into(),
            range: TimeRange::new(begin, Utc::now()).unwrap(),
        })
        .await
        .unwrap();
    assert!(
        unrelated.points.is_empty(),
        "相同 Provider 和窗口不能串入其他账号历史"
    );
    assert_eq!(unrelated.usage.request_count, 0);
    assert_eq!(unrelated.usage.tokens, 0);
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    provider
        .quota(ProviderQuotaRequest {
            account_id: account.clone(),
            refresh: true,
            rolling_usage: None,
        })
        .await
        .unwrap();
    let admin = environment.bind_admin_accounts(&runtime, &core).await;
    let report = admin
        .services()
        .accounts()
        .quota_forecast(&account)
        .await
        .unwrap();
    let weekly = &report.forecasts[0];
    assert_eq!(weekly.source.as_ref().unwrap().tokens, Some(14));
    assert_eq!(weekly.unavailable_reason, None);
    assert_eq!(weekly.remaining_tokens, Some(14));
    assert_eq!(weekly.estimated_tokens, Some(28));
    assert!(
        weekly.estimated_usd.is_none(),
        "没有费用事实时不按请求数捏造金额"
    );
    assert!(weekly.incomplete_cost);
    // 只有显式刷新调用过上游；历史查询和预测都只读本地事实。
    server.verify().await;
    drop(provider);
    drop(admin);
    drop(store);
    drop(core);
    runtime.shutdown().await;
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn real_worker_quota_facts_are_projected_locally_with_exact_window_identity() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let value = forecast();
    let (runtime, core) = environment
        .provider(
            configuration(value.clone()),
            vec![account_grant("accounts")],
        )
        .await;
    let events = super::events::execute(&runtime, &core, None).await.unwrap();
    let metadata = events[0]
        .response_observation()
        .unwrap()
        .provider_metadata()
        .unwrap();
    let saved: Value = serde_json::from_str(metadata.as_json()).unwrap();
    assert_eq!(saved, json!({"plugin_quota_forecast_v1":value}));
    assert!(events[0].wire_event().is_none());
    assert!(events[0].canonical_facts().is_empty());
    let document =
        ProviderDocument::new(OpaqueProviderData::new(saved.as_object().unwrap().clone()));
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let expected = provider
        .quota_forecast_observation(&document, &window())
        .unwrap();
    assert_eq!(expected.used_percent, 25.0, "不从当前额度 50% 反推历史");
    assert_eq!(expected.reset_at.timestamp_millis(), RESET_AT_MS);
    assert_eq!(expected.plan_type.as_deref(), Some("plugin-pro"));
    // 发布对象仍持有句柄，但同步历史解释无需活着的子进程或额外权限。
    runtime.shutdown().await;
    assert_eq!(
        provider.quota_forecast_observation(&document, &window()),
        Some(expected)
    );
    for index in 0..6 {
        let mut mismatch = window();
        match index {
            0 => mismatch.key = "monthly".into(),
            1 => mismatch.group = "images".into(),
            2 => mismatch.limit_id = None,
            3 => mismatch.role = Some(ProviderQuotaWindowRole::Primary),
            4 => mismatch.window_seconds = Some(3600),
            _ => mismatch.local_usage_attribution = QuotaLocalUsageAttribution::Unavailable,
        }
        assert!(
            provider
                .quota_forecast_observation(&document, &mismatch)
                .is_none(),
            "window mismatch {index}"
        );
    }
    let legacy = ProviderDocument::new(OpaqueProviderData::new(Default::default()));
    assert!(
        provider
            .quota_forecast_observation(&legacy, &window())
            .is_none()
    );
    drop(provider);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn real_worker_rejects_invalid_or_oversized_quota_observations() {
    let mut invalid = Vec::new();
    for (field, replacement) in [
        ("window_seconds", json!(0)),
        ("used_percent", json!(-1.0)),
        ("reset_at_ms", json!(i64::MAX)),
        ("key", json!("")),
        ("group", json!("invalid\nvalue")),
        ("limit_id", json!("x".repeat(1025))),
    ] {
        let mut value = forecast();
        value["windows"][0][field] = replacement;
        invalid.push(value);
    }
    let mut duplicate = forecast();
    duplicate["windows"]
        .as_array_mut()
        .unwrap()
        .push(forecast()["windows"][0].clone());
    invalid.push(duplicate);
    for count in [18, 65] {
        let mut value = forecast();
        value["windows"] = (0..count)
            .map(|index| {
                let mut window = forecast()["windows"][0].clone();
                window["key"] = json!(format!(
                    "{index}-{}",
                    "k".repeat(if count == 18 { 1000 } else { 1 })
                ));
                window["group"] = json!("g".repeat(if count == 18 { 1000 } else { 1 }));
                window
            })
            .collect();
        invalid.push(value);
    }
    for (index, value) in invalid.into_iter().enumerate() {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        environment.account(None).await;
        let (runtime, core) = environment
            .provider(configuration(value), vec![account_grant("accounts")])
            .await;
        let error = super::events::execute(&runtime, &core, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            ProviderErrorKind::Protocol,
            "invalid quota observation {index}"
        );
        environment.release_plugin_accounts(&runtime);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}
