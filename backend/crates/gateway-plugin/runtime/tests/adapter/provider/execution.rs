use std::{
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::StreamExt as _;
use gateway_admin::model::plugins::instances::PluginPermissionGrant;
use gateway_core::{
    account::{AccountSelectionPolicy, OutboundProxy, RotationStrategy},
    engine::{
        AccountAttemptContext, AttemptContext, EngineError, ModelRequestId, RequestAttemptContext,
        execution::{ClientTransport, ExecutionRequestMetadata, StartExecution},
        policy::RequestPolicyContext,
        probe::AccountProbeRequest,
        provider::ProviderRequest,
    },
    error::ProviderErrorKind,
    event::GatewayEvent,
    lifecycle::CancellationToken,
    metering::Decimal,
    operation::{GenerateRequest, Operation, ProtocolPayload},
    policy::{ClientApiKeyId, RateLimits},
    provider_ports::ProviderSessionAffinityKey,
    routing::{ProviderKind, PublicModelId, RoutingContext, UpstreamModelId},
    upstream::UpstreamSendState,
};
use serde_json::json;

#[tokio::test]
async fn rust_provider_uses_real_database_leases_and_core_probe_without_sending_during_prepare() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    run_provider(
        environment,
        json!({}),
        None,
        network_grant(),
        Ok(Some("Rust plugin response")),
    )
    .await;
}

#[tokio::test]
async fn rust_provider_streams_http_through_selected_account_proxy() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::header(
        "proxy-authorization",
        "Basic dXNlcjpwYXNz",
    ))
    .respond_with(
        wiremock::ResponseTemplate::new(200).set_body_string("managed response ".repeat(200)),
    )
    .expect(2)
    .mount(&proxy)
    .await;
    let response = "managed response ".repeat(200);
    run_provider(
        environment,
        json!({"http_url":"http://127.0.0.1:9/model"}),
        Some(OutboundProxy::parse(&format!("http://user:pass@{}", proxy.address())).unwrap()),
        network_grant(),
        Ok(Some(&response)),
    )
    .await;
}

#[tokio::test]
async fn rust_provider_cannot_downgrade_host_http_send_state() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("accepted"))
        .expect(1)
        .mount(&server)
        .await;
    run_provider(
        environment,
        json!({"http_url":server.uri(),"error_after_http":true}),
        None,
        network_grant(),
        Err(UpstreamSendState::Sent),
    )
    .await;
}

#[tokio::test]
async fn network_domain_does_not_require_origin_prebinding() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("domain response"))
        .expect(2)
        .mount(&server)
        .await;
    run_provider(
        environment,
        json!({"http_url":server.uri()}),
        None,
        network_grant(),
        Ok(Some("domain response")),
    )
    .await;
}

#[tokio::test]
async fn rust_provider_runs_scheduler_rpc_before_lease_and_execution() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let kind = ProviderKind::new("example").unwrap();
    let account_id = environment.account(None).await;
    let schedule_marker = environment.directory.path().join("scheduled.jsonl");
    let execution_marker = environment.directory.path().join("executed");
    let (runtime, core) = environment
        .provider(
            json!({
                "schedule_decision":{"decision":"reject"},
                "schedule_marker":schedule_marker.to_string_lossy(),
                "execution_marker":execution_marker.to_string_lossy(),
                "generation_marker":"scheduler",
            }),
            vec![crate::support::environment::account_grant("accounts")],
        )
        .await;
    let ports = environment.store.provider_ports();
    let snapshot = core.snapshots().acquire().unwrap();
    let generation = snapshot.extensions().cloned().unwrap();
    let policy = RequestPolicyContext::new(
        runtime.policy_registry().resolve(&generation).unwrap(),
        generation,
        ModelRequestId::new("req_plugin_scheduler").unwrap(),
        ClientApiKeyId::new("key_plugin_scheduler").unwrap(),
        vec![],
    );
    let admin_registry = runtime.admin_registry(core.snapshots());
    let admin = admin_registry.require(&kind).unwrap();
    let model = UpstreamModelId::new("plugin-model").unwrap();
    let operation = admin
        .connection_test_operation(&model, "scheduler")
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
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers.get(&kind).unwrap();
    let attempt = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_plugin_scheduler").unwrap(),
            ClientApiKeyId::new("key_plugin_scheduler").unwrap(),
        )
        .with_request_policy(Some(policy)),
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
    let error = match Arc::clone(provider)
        .execute(
            ProviderRequest::new(operation, plan.candidates()[0].clone()),
            attempt,
        )
        .await
    {
        Ok(_) => panic!("scheduler should reject before provider execution"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ProviderErrorKind::RequestPolicyDenied);
    assert_eq!(error.send_state(), UpstreamSendState::NotSent);
    assert_eq!(
        ports
            .leases()
            .account_in_flight(std::slice::from_ref(&account_id))
            .await
            .unwrap()[&account_id],
        0,
        "明确拒绝发生在账号租约获取前"
    );
    assert!(!execution_marker.exists(), "明确拒绝不能进入 Provider 执行");
    let schedules = std::fs::read_to_string(schedule_marker).unwrap();
    assert_eq!(schedules.lines().count(), 1, "必须真实派发一次调度 RPC");

    drop(admin);
    drop(admin_registry);
    drop(providers);
    drop(snapshot);
    drop(core);
    drop(runtime);
    drop(ports);
    environment.close().await;
}

#[tokio::test]
async fn routing_callback_inherits_admission_and_reads_real_affinity_without_recursing() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let provider = ProviderKind::new("example").unwrap();
    let account_id = environment
        .account_for_with_concurrency(provider.as_str(), None, Some(1))
        .await;
    let _router_account = environment
        .account_for_with_concurrency("router", None, Some(1))
        .await;
    let affinity_key = ProviderSessionAffinityKey::try_new("nested-routing-affinity").unwrap();
    let ports = environment.store.provider_ports();
    ports
        .session_affinity()
        .bind(
            &provider,
            &affinity_key,
            &account_id,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

    let route_marker = environment.directory.path().join("nested-route.jsonl");
    let nested_marker = environment.directory.path().join("nested-model.jsonl");
    let affinity_marker = environment.directory.path().join("nested-affinity.jsonl");
    let child_execution_marker = environment.directory.path().join("child-executions.jsonl");
    let model_grant = crate::support::environment::account_grant("models");
    let affinity_grant = crate::support::environment::account_grant("requests");
    let router_credential = crate::support::environment::account_grant("accounts");
    environment
        .install_provider(
            json!({
                "provider_id":"router",
                "generation_marker":"nested-router",
                "route_marker":route_marker,
                "route_decision":{
                    "decision":"route",
                    "provider":"example",
                    "model":"plugin-model"
                },
                "nested_model_fixture":{
                    "request":{
                        "model":"plugin-model",
                        "protocol":"openai",
                        "operation":"generate",
                        "provider":"example",
                        "account_id":account_id.as_str()
                    },
                    "body":{"model":"plugin-model","input":"nested routing request"}
                },
                "nested_model_marker":nested_marker,
                "affinity_fixture":{
                    "request":{
                        "provider":"example",
                        "key":affinity_key.expose_to_store()
                    }
                },
                "affinity_marker":affinity_marker,
            }),
            vec![model_grant, affinity_grant, router_credential],
        )
        .await;
    environment
        .install_provider(
            json!({
                "provider_id":"example",
                "generation_marker":"nested-target",
                "execution_calls_marker":child_execution_marker,
            }),
            vec![crate::support::environment::account_grant("accounts")],
        )
        .await;
    let key_secret = "sk-nested-routing-fixture";
    let key_id = format!("key_{}", uuid::Uuid::new_v4().simple());
    environment
        .client_key_with_limits(
            &key_id,
            key_secret,
            RateLimits {
                max_concurrency: 1,
                requests_per_minute: 0,
            },
        )
        .await;
    let (runtime, core) = environment.runtime().await;
    let execution = core.execution_service();
    let client = execution.authenticate(key_secret).unwrap();
    let mut started = tokio::time::timeout(
        Duration::from_secs(10),
        execution.start(StartExecution {
            client,
            public_model: PublicModelId::new("plugin-model").unwrap(),
            operation: generate_operation(),
            metadata: ExecutionRequestMetadata {
                protocol: "openai".into(),
                endpoint: "/v1/responses".into(),
                transport: ClientTransport::HttpJson,
                stream: false,
                client_ip: None,
                user_agent: Some("nested-routing-integration".into()),
                previous_response_id: None,
            },
        }),
    )
    .await
    .expect("nested execution must reuse the admitted Key slot")
    .unwrap_or_else(|error| {
        panic!(
            "routing callback failed: {error:?}; route={:?}; affinity={:?}; nested={:?}",
            marker_records(&route_marker),
            marker_records(&affinity_marker),
            marker_records(&nested_marker),
        )
    });
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        started.session.collect_uncommitted(),
    )
    .await
    .expect("parent execution must not recurse into the router")
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.canonical_facts(), [GatewayEvent::Completed(_)]))
    );
    started.session.commit_downstream(Some(200)).await.unwrap();

    let route_calls = marker_records(&route_marker);
    assert_eq!(route_calls.len(), 1, "A→child must suppress router A");
    let nested_calls = marker_records(&nested_marker);
    assert_eq!(nested_calls.len(), 1);
    assert!(nested_calls[0]["events"].as_u64().unwrap() >= 1);
    let affinity_calls = marker_records(&affinity_marker);
    assert_eq!(affinity_calls.len(), 1);
    assert_eq!(
        affinity_calls[0]["result"]["account_id"],
        account_id.as_str()
    );
    assert_eq!(
        marker_records(&child_execution_marker).len(),
        2,
        "the child and then the parent each execute exactly once"
    );
    let markers = format!(
        "{}{}{}",
        std::fs::read_to_string(&route_marker).unwrap(),
        std::fs::read_to_string(&nested_marker).unwrap(),
        std::fs::read_to_string(&affinity_marker).unwrap()
    );
    assert!(!markers.contains(key_secret));

    drop(started);
    tokio::time::timeout(Duration::from_secs(3), async {
        while ports
            .leases()
            .account_in_flight(std::slice::from_ref(&account_id))
            .await
            .unwrap()[&account_id]
            != 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(execution);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(ports);
    environment.close().await;
}

#[tokio::test]
async fn cancelling_a_single_slot_parent_reaps_the_child_and_charges_its_cost_once() {
    let Some(mut environment) = crate::support::environment::Environment::create_command().await
    else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    // 测试不启动 Host 监督器，直接启动生产同款四个写泵，
    // 确保终态、计费和准入释放都真实落入 PostgreSQL/Redis。
    environment.store.start_command_line_writes().unwrap();
    let parent_account = environment
        .account_for_with_concurrency("parent", None, Some(1))
        .await;
    let child_account = environment
        .account_for_with_concurrency("child", None, Some(1))
        .await;
    let parent_execution_marker = environment.directory.path().join("cancel-parent.jsonl");
    let child_execution_marker = environment.directory.path().join("cancel-child.jsonl");
    let child_waiting_marker = environment.directory.path().join("child-waiting.jsonl");
    let child_cost_consumed_marker = environment.directory.path().join("child-cost.jsonl");
    let child_stream_trace_marker = environment.directory.path().join("child-stream.jsonl");
    let child_cancelled_marker = environment.directory.path().join("child-cancelled.jsonl");
    let parent_credential = crate::support::environment::account_grant("accounts");
    let model_grant = crate::support::environment::account_grant("models");
    environment
        .install_provider(
            json!({
                "provider_id":"parent",
                "generation_marker":"cancel-parent",
                "execution_calls_marker":parent_execution_marker,
                "execution_nested_model_fixture":{
                    "stream_until_after_provider_cost":true,
                    "request":{
                        "model":"child-model",
                        "protocol":"openai",
                        "operation":"generate",
                        "provider":"child",
                        "account_id":child_account.as_str()
                    },
                    "body":{"model":"child-model","input":"cancel the active child"}
                },
                "execution_nested_model_marker":child_cost_consumed_marker,
                "nested_stream_trace_marker":child_stream_trace_marker
            }),
            vec![parent_credential, model_grant],
        )
        .await;
    let child_credential = crate::support::environment::account_grant("accounts");
    environment
        .install_provider(
            json!({
                "provider_id":"child",
                "generation_marker":"cancel-child",
                "static_models":[{"id":"child-model","operations":["generate"]}],
                "output_formats":["canonical","openai"],
                "execution_calls_marker":child_execution_marker,
                "execution_wait_for_cancel_marker":child_waiting_marker,
                "execution_cancelled_marker":child_cancelled_marker,
                "execution_events":[
                    {"facts":[{"type":"started","id":"cancel-child","model":"child-model"}]},
                    {"facts":[{"type":"provider_cost","amount":"0.0125","currency":"USD"}]},
                    {
                        "facts":[
                            {"type":"content_added","index":0,"kind":"text"},
                            {"type":"text_delta","index":0,"text":"visible after cost"}
                        ],
                        "wire":{
                            "protocol":"openai",
                            "payload":{
                                "kind":"json",
                                "event":"response.output_text.delta",
                                "data":{"delta":"visible after cost"}
                            }
                        }
                    }
                ]
            }),
            vec![child_credential],
        )
        .await;
    let key_secret = "sk-nested-cancel-fixture";
    let key_id = format!("key_{}", uuid::Uuid::new_v4().simple());
    environment
        .client_key_with_limits(
            &key_id,
            key_secret,
            RateLimits {
                max_concurrency: 1,
                requests_per_minute: 0,
            },
        )
        .await;
    let ports = environment.store.provider_ports();
    let (runtime, core) = environment.runtime().await;
    let execution = core.execution_service();
    let client = execution.authenticate(key_secret).unwrap();
    let mut started = execution
        .start(StartExecution {
            client,
            public_model: PublicModelId::new("plugin-model").unwrap(),
            operation: generate_operation(),
            metadata: ExecutionRequestMetadata {
                protocol: "openai".into(),
                endpoint: "/v1/responses".into(),
                transport: ClientTransport::HttpJson,
                stream: false,
                client_ip: None,
                user_agent: Some("nested-cancel-single-slot".into()),
                previous_response_id: None,
            },
        })
        .await
        .unwrap();
    let mut next = started.session.next_event();
    tokio::select! {
        biased;
        () = wait_for_marker(&child_cost_consumed_marker) => {}
        result = &mut next => panic!(
            "parent completed before cancellation: {result:?}; parent={:?}; child={:?}; waiting={:?}; barrier={:?}; stream={:?}",
            marker_records(&parent_execution_marker),
            marker_records(&child_execution_marker),
            marker_records(&child_waiting_marker),
            marker_records(&child_cost_consumed_marker),
            marker_records(&child_stream_trace_marker),
        ),
    }
    drop(next);
    assert_eq!(marker_records(&parent_execution_marker).len(), 1);
    assert_eq!(marker_records(&child_execution_marker).len(), 1);
    assert_eq!(marker_records(&child_waiting_marker).len(), 1);
    assert!(
        marker_records(&child_cost_consumed_marker)[0]["after_provider_cost_consumed"]
            .as_bool()
            .unwrap_or(false)
    );

    started.session.cancel();
    tokio::time::timeout(Duration::from_secs(3), started.session.detach_finalize())
        .await
        .expect("parent and child cleanup must be bounded");
    wait_for_marker(&child_cancelled_marker).await;
    assert_eq!(marker_records(&child_cancelled_marker).len(), 1);

    let ledger = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ledger = environment.model_request_ledger(&key_id).await;
            if ledger.len() >= 2
                && ledger.iter().all(|request| request.completed)
                && ledger
                    .iter()
                    .all(|request| request.charged_amount.is_some())
            {
                break ledger;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let ledger = match ledger {
        Ok(ledger) => ledger,
        Err(_) => panic!(
            "parent and child ledger writes must drain; last facts: {:?}",
            environment.model_request_ledger(&key_id).await,
        ),
    };
    assert_eq!(
        ledger.len(),
        2,
        "one parent and one child request are expected"
    );
    let parent = ledger
        .iter()
        .find(|request| request.provider_kind.as_deref() == Some("parent"))
        .expect("parent ledger row");
    let child = ledger
        .iter()
        .find(|request| request.provider_kind.as_deref() == Some("child"))
        .expect("child ledger row");
    assert_ne!(parent.request_id, child.request_id);
    assert!(parent.completed);
    assert_eq!(parent.outcome, "cancelled");
    assert_eq!(parent.cost_source, "unavailable");
    assert!(parent.cost_amount.is_none());
    assert!(child.completed);
    assert_eq!(child.outcome, "cancelled");
    assert_eq!(child.request_kind.as_deref(), Some("plugin_child_model"));
    assert!(child.subagent_kind.is_some());
    assert_eq!(child.cost_source, "provider_reported");
    assert_eq!(
        child
            .cost_amount
            .as_deref()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
            .canonical(),
        "0.0125"
    );
    let charges = ledger
        .iter()
        .map(|request| {
            request
                .charged_amount
                .as_deref()
                .unwrap()
                .parse::<Decimal>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parent
            .charged_amount
            .as_deref()
            .unwrap()
            .parse::<Decimal>()
            .unwrap(),
        Decimal::ZERO
    );
    assert_eq!(
        child
            .charged_amount
            .as_deref()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
            .canonical(),
        "0.0125"
    );
    let charged = charges
        .iter()
        .copied()
        .fold(Decimal::ZERO, |total, amount| {
            total.checked_add(amount).unwrap()
        });
    assert_eq!(charged.canonical(), "0.0125");
    assert_eq!(
        charges
            .iter()
            .filter(|amount| **amount != Decimal::ZERO)
            .count(),
        1,
        "the orchestrating parent must not duplicate the child's upstream cost"
    );

    let accounts = [parent_account, child_account];
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let in_flight = ports.leases().account_in_flight(&accounts).await.unwrap();
            if in_flight.values().all(|count| *count == 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parent and child account leases must be released");

    drop(execution);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(ports);
    environment
        .store
        .shutdown_command_line_writes()
        .await
        .unwrap();
    environment.close().await;
}

fn marker_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn wait_for_marker(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while marker_records(path).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker marker must arrive");
}

#[tokio::test]
async fn provider_nested_model_side_effect_makes_a_claimed_not_sent_failure_non_replayable() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let parent_accounts = [
        environment
            .account_for_with_concurrency("parent", None, Some(1))
            .await,
        environment
            .account_for_with_concurrency("parent", None, Some(1))
            .await,
    ];
    let child_account = environment
        .account_for_with_concurrency("child", None, Some(1))
        .await;
    let parent_execution_marker = environment.directory.path().join("parent-executions.jsonl");
    let child_execution_marker = environment.directory.path().join("child-executions.jsonl");
    let nested_marker = environment.directory.path().join("provider-nested.jsonl");
    let parent_credential = crate::support::environment::account_grant("accounts");
    let model_grant = crate::support::environment::account_grant("models");
    environment
        .install_provider(
            json!({
                "provider_id":"parent",
                "generation_marker":"nested-parent",
                "execution_calls_marker":parent_execution_marker,
                "execution_nested_model_fixture":{
                    "request":{
                        "model":"child-model",
                        "protocol":"openai",
                        "operation":"generate",
                        "provider":"child",
                        "account_id":child_account.as_str()
                    },
                    "body":{"model":"child-model","input":"child side effect"}
                },
                "execution_nested_model_marker":nested_marker,
                "error_after_nested_model":true,
            }),
            vec![parent_credential, model_grant],
        )
        .await;
    let child_credential = crate::support::environment::account_grant("accounts");
    environment
        .install_provider(
            json!({
                "provider_id":"child",
                "generation_marker":"nested-child",
                "static_models":[{"id":"child-model","operations":["generate"]}],
                "execution_calls_marker":child_execution_marker,
            }),
            vec![child_credential],
        )
        .await;
    let key_secret = "sk-nested-side-effect-fixture";
    let key_id = format!("key_{}", uuid::Uuid::new_v4().simple());
    environment
        .client_key_with_limits(
            &key_id,
            key_secret,
            RateLimits {
                max_concurrency: 1,
                requests_per_minute: 0,
            },
        )
        .await;
    let (runtime, core) = environment.runtime().await;
    let execution = core.execution_service();
    let client = execution.authenticate(key_secret).unwrap();
    let mut started = execution
        .start(StartExecution {
            client,
            public_model: PublicModelId::new("plugin-model").unwrap(),
            operation: generate_operation(),
            metadata: ExecutionRequestMetadata {
                protocol: "openai".into(),
                endpoint: "/v1/responses".into(),
                transport: ClientTransport::HttpJson,
                stream: false,
                client_ip: None,
                user_agent: Some("nested-side-effect-watermark".into()),
                previous_response_id: None,
            },
        })
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        started.session.collect_uncommitted(),
    )
    .await
    .unwrap()
    .expect_err("the parent fixture must fail after the child completed");
    assert!(matches!(
        error,
        EngineError::Provider(ref error) if error.send_state() == UpstreamSendState::Ambiguous
    ));
    assert_eq!(marker_records(&parent_execution_marker).len(), 1);
    assert_eq!(marker_records(&child_execution_marker).len(), 1);
    assert_eq!(marker_records(&nested_marker).len(), 1);

    started.session.detach_finalize().await;
    let all_accounts = parent_accounts
        .iter()
        .cloned()
        .chain(std::iter::once(child_account.clone()))
        .collect::<Vec<_>>();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let in_flight = environment
                .store
                .provider_ports()
                .leases()
                .account_in_flight(&all_accounts)
                .await
                .unwrap();
            if all_accounts
                .iter()
                .all(|account| in_flight.get(account).copied().unwrap_or(0) == 0)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(execution);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    environment.close().await;
}

fn generate_operation() -> Operation {
    Operation::Generate(GenerateRequest::from_protocol_payload(
        ProtocolPayload::json_object(
            "openai",
            json!({"model":"plugin-model","input":"generate"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap(),
    ))
}

#[tokio::test]
async fn network_domain_does_not_override_the_host_live_network_policy() {
    if std::env::var("CPR_PLUGIN_TEST_LIVE_HTTP").as_deref() != Ok("1") {
        eprintln!("SKIP: CPR_PLUGIN_TEST_LIVE_HTTP 未启用");
        return;
    }
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    run_provider(
        environment,
        json!({"http_url":"https://api.github.com/zen"}),
        None,
        network_grant(),
        Err(UpstreamSendState::NotSent),
    )
    .await;
}

fn network_grant() -> PluginPermissionGrant {
    crate::support::environment::account_grant("network")
}

async fn run_provider(
    environment: crate::support::environment::Environment,
    mut configuration: serde_json::Value,
    proxy: Option<OutboundProxy>,
    network: PluginPermissionGrant,
    expected: Result<Option<&str>, UpstreamSendState>,
) {
    let kind = ProviderKind::new("example").unwrap();
    let account_id = environment.account(proxy).await;
    let ports = environment.store.provider_ports();
    let marker = environment.directory.path().join("executed");
    configuration["execution_marker"] = json!(marker);
    configuration["generation_marker"] = json!("old");
    let (runtime, core) = environment
        .provider(
            configuration,
            vec![
                crate::support::environment::account_grant("accounts"),
                network,
            ],
        )
        .await;
    let registry = runtime.provider_registry();
    let snapshot = core.snapshots().acquire().unwrap();
    let admin_registry = runtime.admin_registry(core.snapshots());
    let old_admin = admin_registry.require(&kind).unwrap();
    let model = UpstreamModelId::new("plugin-model").unwrap();
    assert_eq!(
        old_admin.models(&account_id, false).await.unwrap().models[0].id,
        model
    );
    let operation = old_admin
        .connection_test_operation(&model, "hello")
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
    let providers = registry.for_extensions(snapshot.extensions()).unwrap();
    let provider = providers.get(&kind).unwrap();
    let context = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_plugin_cold").unwrap(),
            ClientApiKeyId::new("key_plugin_test").unwrap(),
        ),
        NonZeroU32::new(1).unwrap(),
        SystemTime::now() + Duration::from_secs(30),
        AccountSelectionPolicy::new(
            RotationStrategy::RoundRobin,
            NonZeroU32::new(1).unwrap(),
            Duration::ZERO,
        ),
        AccountAttemptContext::default().with_account_scope(snapshot.all_account_scope()),
        None,
        CancellationToken::new(),
    );
    let mut response = Arc::clone(provider)
        .execute(
            ProviderRequest::new(operation.clone(), plan.candidates()[0].clone()),
            context,
        )
        .await
        .unwrap();
    assert!(!marker.exists(), "准备完成不能发送业务请求");
    assert_eq!(
        ports
            .leases()
            .account_in_flight(std::slice::from_ref(&account_id))
            .await
            .unwrap()[&account_id],
        1
    );
    let mut events = Vec::new();
    let mut failure = None;
    while let Some(event) = response.next().await {
        match event {
            Ok(event) => events.push(event),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        account_id.as_str()
    );
    match expected {
        Ok(_) => {
            assert!(failure.is_none(), "provider failed: {failure:?}");
            assert!(events.iter().any(|event| matches!(event.canonical_facts(), [GatewayEvent::ProviderCost(cost)] if cost.total().amount().canonical() == "0.0125")));
        }
        Err(state) => assert_eq!(failure.unwrap().send_state(), state),
    }
    drop(response);
    tokio::time::timeout(Duration::from_secs(3), async {
        while ports
            .leases()
            .account_in_flight(std::slice::from_ref(&account_id))
            .await
            .unwrap()
            .get(&account_id)
            .copied()
            .unwrap_or(0)
            != 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let mut candidate = environment
        .store
        .admin_ports()
        .plugins()
        .load_instances()
        .await
        .unwrap();
    candidate.instances[0].configuration["generation_marker"] = json!("new");
    let next = gateway_admin::ports::plugins::PluginPreparation::prepare(
        runtime.as_ref(),
        candidate.config_revision,
        candidate,
    )
    .await
    .unwrap();
    core.snapshots()
        .publish(snapshot.as_ref().clone().with_extensions(Some(next)));
    let current_admin = admin_registry.require(&kind).unwrap();
    if let Ok(expected_text) = expected {
        let probe = core
            .account_probe()
            .probe(
                AccountProbeRequest {
                    account_id,
                    provider_kind: kind.clone(),
                    upstream_model: UpstreamModelId::new("plugin-model").unwrap(),
                    operation,
                },
                old_admin.snapshot(),
            )
            .await
            .unwrap();
        if let Some(text) = expected_text {
            assert_eq!(probe.text, vec![text]);
        } else {
            assert!(!probe.text.concat().is_empty());
        }
    }
    drop(providers);
    drop(snapshot);
    // 此时只有管理句柄持有旧集合；发布切换不能在管理操作 await 时关闭其 RPC。
    let Operation::Generate(old_probe) = old_admin
        .connection_test_operation(&model, "old")
        .await
        .unwrap()
    else {
        panic!("probe operation")
    };
    let Operation::Generate(new_probe) = current_admin
        .connection_test_operation(&model, "new")
        .await
        .unwrap()
    else {
        panic!("probe operation")
    };
    assert_eq!(
        old_probe.protocol_payload().body()["plugin_generation"],
        "old"
    );
    assert_eq!(
        new_probe.protocol_payload().body()["plugin_generation"],
        "new"
    );
    drop(old_admin);
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::fs::read_dir(environment.directory.path().join("cache"))
            .unwrap()
            .count()
            != 1
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(current_admin);
    drop(admin_registry);
    drop(core);
    drop(runtime);
    drop(ports);
    environment.close().await;
}
