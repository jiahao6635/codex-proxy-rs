use std::{sync::Arc, time::Duration};

use gateway_admin::{
    CredentialsService,
    model::{
        plugins::instances::{PluginCapabilityBinding, PluginFailurePolicy},
        provider_credentials::{
            CredentialMutation, ImportCredentials, ProviderDocument, RotateCredential,
        },
    },
};
use gateway_core::{
    account::OpaqueProviderData,
    lifecycle::CancellationToken,
    routing::{ConfigRevision, ProviderKind},
    task::{ScheduledTask, WorkerContribution, WorkerCycleContext, WorkerRunnable},
};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn task(
    runtime: &Arc<gateway_plugin_runtime::PluginRuntime>,
    core: &gateway_core::CoreBundle,
    owner: &str,
) -> (Box<dyn ScheduledTask>, WorkerCycleContext) {
    let definitions = runtime.worker_contributions(core.snapshots()).unwrap();
    assert_eq!(definitions.len(), 4);
    for definition in definitions {
        definition.validate().unwrap();
        let WorkerContribution::Registration(registration) = definition else {
            panic!("scheduled worker");
        };
        if registration.id.owner() != owner {
            continue;
        }
        let WorkerRunnable::Scheduled {
            schedule,
            lease,
            task,
        } = registration.runnable
        else {
            panic!("scheduled worker");
        };
        assert!(lease.is_some());
        assert!(schedule.leader_lease_renewal_interval() < schedule.leader_lease_ttl());
        return (
            task,
            WorkerCycleContext::new(registration.id, None, CancellationToken::new()),
        );
    }
    panic!("missing worker {owner}")
}

fn http_grants() -> Vec<gateway_admin::model::plugins::instances::PluginPermissionGrant> {
    vec![account_grant("accounts"), account_grant("network")]
}

async fn enable_maintenance(
    environment: &Environment,
    core: &gateway_core::CoreBundle,
    enabled: bool,
) {
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance
        .bindings
        .retain(|binding| binding.contribution != "test.example.maintenance");
    if enabled {
        instance.bindings.push(PluginCapabilityBinding {
            contribution: "test.example.maintenance".into(),
            stage: "maintenance".into(),
            order: 0,
            failure_policy: PluginFailurePolicy::Reject,
            client_key_ids: vec![],
            account_group_ids: vec![],
            provider_ids: vec![],
            models: vec![],
            identity_bindings: vec![],
        });
    }
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    core.snapshot_control()
        .publish_committed(ConfigRevision::new(saved.config_revision.get()).unwrap())
        .await;
}

#[tokio::test]
async fn maintenance_requires_binding_and_stops_dispatch_after_revocation() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::header(
        "authorization",
        "Bearer test-only",
    ))
    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
        json!({"plan_type":"maintained", "access":{"kind":"allowed"}, "windows":[]}),
    ))
    .expect(1)
    .mount(&server)
    .await;
    let account = environment.account(None).await;
    let (runtime, core) = environment.provider(json!({"maintenance":true,"quota_url":server.uri(),"expected_maintenance_methods":["provider.quota"]}), http_grants()).await;
    let (task, context) = task(&runtime, &core, "plugin-quota");
    enable_maintenance(&environment, &core, false).await;
    task.run_cycle(context.clone()).await.unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    enable_maintenance(&environment, &core, true).await;
    task.run_cycle(context.clone()).await.unwrap();
    let quotas = environment
        .store
        .provider_ports()
        .accounts()
        .get_quotas(std::slice::from_ref(&account))
        .await
        .unwrap();
    assert_eq!(quotas.len(), 1);
    assert_eq!(quotas[0].plan_type.as_deref(), Some("maintained"));
    enable_maintenance(&environment, &core, false).await;
    task.run_cycle(context).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    drop(task);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn cancelled_maintenance_drops_rpc_and_does_not_commit_late_quota() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({"access":{"kind":"allowed"},"windows":[]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let account = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({"maintenance":true,"quota_url":server.uri()}),
            http_grants(),
        )
        .await;
    let (task, context) = task(&runtime, &core, "plugin-quota");
    let cancellation = context.cancellation().clone();
    let running = tokio::spawn(async move { task.run_cycle(context).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(
        environment
            .store
            .provider_ports()
            .accounts()
            .get_quotas(&[account])
            .await
            .unwrap()
            .is_empty()
    );
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn late_maintenance_quota_cannot_overwrite_rotated_credentials() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let mut server = crate::support::http::GatedResponse::start(json!({
        "access":{"kind":"exhausted","evidence":"usage_limit_reached"},"windows":[]
    }))
    .await;
    let account = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({"maintenance":true,"quota_url":server.uri()}),
            http_grants(),
        )
        .await;
    let (task, context) = task(&runtime, &core, "plugin-quota");
    let running = tokio::spawn(async move { task.run_cycle(context).await });
    server.received().await;
    let service = CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        environment.store.admin_ports().accounts(),
        environment.store.admin_ports().proxies(),
        core.snapshot_control(),
    );
    service
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: account.clone(),
                context: mutation(),
            },
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                json!({"key":"new-test-key"}).as_object().unwrap().clone(),
            )),
            settings: None,
        })
        .await
        .unwrap();
    server.respond().await;
    // 旧修订的结果属于正常竞争；本周期结束，但不能把旧额度写到新凭据上。
    running.await.unwrap().unwrap();
    let accounts = environment.store.provider_ports().accounts();
    let current = accounts.load_current_credential(&account).await.unwrap();
    assert_eq!(current.account.revision().get(), 2);
    assert_eq!(
        current.credential.expose_to_provider()["key"],
        "new-test-key"
    );
    assert!(accounts.get_quotas(&[account]).await.unwrap().is_empty());
    drop(accounts);
    drop(service);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn disabled_accounts_are_not_dispatched_from_an_already_loaded_page() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let mut server = crate::support::http::GatedResponse::start_batch(
        json!({"access":{"kind":"allowed"},"windows":[]}),
        4,
    )
    .await;
    let mut account_ids = Vec::new();
    for _ in 0..8 {
        account_ids.push(environment.account(None).await.as_str().to_owned());
    }
    let marker = environment
        .directory
        .path()
        .join("queued-maintenance.jsonl");
    let (runtime, core) = environment.provider(
        json!({"maintenance":true,"quota_url":server.uri(),"maintenance_context_marker":marker}),
        http_grants(),
    ).await;
    let (task, context) = task(&runtime, &core, "plugin-quota");
    let running = tokio::spawn(async move { task.run_cycle(context).await });
    server.received().await;
    // 第一组已发出，剩余账号还在这一页的并发队列中；持久化停用事实先于快照通知。
    environment
        .store
        .admin_ports()
        .accounts()
        .batch_update_accounts(
            gateway_admin::model::accounts::BatchUpdateAccounts {
                account_ids,
                enabled: Some(false),
                concurrency_limit: None,
                weight: None,
                model_access: None,
                group_ids: None,
                outbound_proxy: None,
            },
            &mutation(),
        )
        .await
        .unwrap();
    server.respond().await;
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let calls = std::fs::read_to_string(marker)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["method"] == "provider.quota" && entry["stage"] == "maintenance")
        .count();
    assert_eq!(calls, 4, "已停用的排队账号不得收到新的插件调用");
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn maintenance_refresh_commits_due_credentials_and_preserves_admin_profile() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment.provider(json!({"maintenance":true,"expected_maintenance_methods":["provider.credentials.refresh"]}), vec![account_grant("accounts"), account_grant("accounts")]).await;
    let service = CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        environment.store.admin_ports().accounts(),
        environment.store.admin_ports().proxies(),
        core.snapshot_control(),
    );
    let kind = ProviderKind::new("example").unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let imported = service.for_provider(&kind).unwrap().import_document(ImportCredentials {
        outbound_proxy_id: None, settings: None, context: mutation(), document: ProviderDocument::new(OpaqueProviderData::new(json!({"accounts":[
            {"name":"kept admin profile","authentication_kind":"oauth","material":{"key":"original-test-key"},"has_refresh_token":true,"access_token_expires_at_ms":now-1000,"next_refresh_at_ms":now-1000},
            {"name":"scheduled without expiry","authentication_kind":"oauth","material":{"key":"original-test-key"},"has_refresh_token":true,"next_refresh_at_ms":now-1000},
            {"name":"not due","authentication_kind":"oauth","material":{"key":"future-test-key"},"has_refresh_token":true,"access_token_expires_at_ms":now+86_400_000,"next_refresh_at_ms":now+86_400_000}
        ]}).as_object().unwrap().clone())),
    }).await.unwrap();
    assert_eq!(imported.credential_ids.len(), 3);
    let (task, context) = task(&runtime, &core, "plugin-credentials");
    task.run_cycle(context).await.unwrap();
    let accounts = environment.store.provider_ports().accounts();
    for id in imported.credential_ids {
        let current = accounts.load_current_credential(&id).await.unwrap();
        if matches!(
            current.account.name(),
            "kept admin profile" | "scheduled without expiry"
        ) {
            assert_eq!(current.account.revision().get(), 2);
            assert_eq!(
                current.credential.expose_to_provider()["key"],
                "refreshed-test-key"
            );
        } else {
            assert_eq!(current.account.name(), "not due");
            assert_eq!(current.account.revision().get(), 1);
            assert_eq!(
                current.credential.expose_to_provider()["key"],
                "future-test-key"
            );
        }
    }
    drop(accounts);
    drop(task);
    drop(service);
    drop(core);
    drop(runtime);
    environment.close().await;
}

fn profiles(version: &str) -> Value {
    json!({"default_configuration":{"channel":"stable"},"options":[{
        "id":"stable","label":"稳定版","configuration":{"channel":"stable"},"resolved":{"version":version},
        "presentation":{"product":"Maintenance Fixture","version":version,"target":{"os_type":"linux","os_version":"6","arch":"x86_64","terminal":"test"},"user_agent":format!("Fixture/{version}"),"attributes":[]}
    }]})
}

#[tokio::test]
async fn profile_maintenance_requires_an_initial_stable_directory() {
    use gateway_admin::{
        model::{AdminErrorKind, Revision},
        ports::plugins::PluginPreparation as _,
    };

    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(
            json!({"maintenance":true,"request_profiles":profiles("1"),"request_profile_refresh":{"sequence":2,"profiles":profiles("2")}}),
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
    let source = candidate.config_revision;
    candidate.config_revision = Revision::new(source.get() + 1).unwrap();
    candidate.instances[0].revision = candidate.config_revision;
    candidate.instances[0]
        .configuration
        .as_object_mut()
        .unwrap()
        .remove("request_profiles");
    let error = runtime.prepare(source, candidate).await.unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Invalid);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn model_maintenance_pages_all_provider_accounts_without_prebinding() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    for _ in 0..20 {
        environment.account(None).await;
    }
    let extra_account = environment.account(None).await;
    let marker = environment
        .directory
        .path()
        .join("maintenance-context.jsonl");
    let credentials = account_grant("accounts");
    let (runtime, core) = environment.provider(json!({
        "maintenance":true,"maintenance_context_marker":marker,
        "model_discovery":{"include_static":false,"cache_ttl_seconds":60},
        "discovered_catalog":{"models":[{"id":"discovered-model","operations":["generate"]}],"exhaustive":true}
    }), vec![credentials]).await;
    let (task, context) = task(&runtime, &core, "plugin-models");
    let calls = || {
        std::fs::read_to_string(&marker)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry["method"] == "provider.models" && entry["stage"] == "maintenance")
            .collect::<Vec<_>>()
    };
    task.run_cycle(context.clone()).await.unwrap();
    assert_eq!(calls().len(), 16);
    task.run_cycle(context).await.unwrap();
    let calls = calls();
    assert_eq!(calls.len(), 21);
    let visited = calls
        .iter()
        .map(|entry| entry["account_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(visited.len(), 21);
    assert!(visited.contains(extra_account.as_str()));
    for call in calls {
        assert_eq!(call["credential_revision"], 1);
        assert!(call["request_id"].as_str().unwrap().starts_with("worker:"));
    }
    drop(task);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn refreshed_profiles_are_monotonic_cached_and_do_not_change_frozen_requests() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let mut changed_choice = profiles("3");
    changed_choice["default_configuration"]["channel"] = json!("changed");
    changed_choice["options"][0]["configuration"]["channel"] = json!("changed");
    let mut oversized = profiles("3");
    oversized["options"][0]["resolved"]["large"] = json!("x".repeat(16 * 1024));
    let invalid = vec![
        json!({"sequence":2,"profiles":profiles("illegal-same-sequence")}),
        json!({"sequence":3,"profiles":changed_choice}),
        json!({"sequence":0,"profiles":profiles("3")}),
        json!({"sequence":1_u64 << 53,"profiles":profiles("3")}),
        json!({"sequence":3,"profiles":oversized}),
    ];
    let invalid_count = invalid.len();
    let mut refreshes = vec![
        json!({"sequence":2,"profiles":profiles("2")}),
        json!({"sequence":1,"profiles":profiles("1")}),
    ];
    refreshes.extend(invalid);
    let (runtime, core) = environment.provider(json!({"maintenance":true,"request_profiles":profiles("1"),"request_profile_refresh":{"sequence":2,"profiles":profiles("2")},
        "request_profile_refresh_responses":refreshes,
        "expected_maintenance_methods":["provider.request_profiles.refresh"]}), vec![account_grant("accounts")]).await;
    let kind = ProviderKind::new("example").unwrap();
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    let choice = OpaqueProviderData::new(json!({"channel":"stable"}).as_object().unwrap().clone());
    let snapshot = core.snapshots().acquire().unwrap();
    let executors = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let executor = executors.get(&kind).unwrap();
    let frozen = executor.resolve_request_profile(&choice).unwrap();
    let (task, context) = task(&runtime, &core, "plugin-request-profiles");
    task.run_cycle(context.clone()).await.unwrap();
    assert_eq!(frozen.expose_to_provider()["version"], "1");
    assert_eq!(
        provider
            .preview_client_profile(&choice)
            .unwrap()
            .expose_to_provider()["version"],
        "2"
    );
    assert_eq!(provider.default_client_profile(), Some(choice.clone()));
    task.run_cycle(context.clone()).await.unwrap();
    assert_eq!(
        executor
            .resolve_request_profile(&choice)
            .unwrap()
            .expose_to_provider()["version"],
        "2"
    );
    for _ in 0..invalid_count {
        assert!(task.run_cycle(context.clone()).await.is_err());
        assert_eq!(
            executor
                .resolve_request_profile(&choice)
                .unwrap()
                .expose_to_provider()["version"],
            "2"
        );
    }
    task.run_cycle(context).await.unwrap();
    drop(executors);
    drop(snapshot);
    drop(task);
    drop(provider);
    drop(core);
    drop(runtime);
    let (runtime, core) = environment.runtime().await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    assert_eq!(
        provider
            .preview_client_profile(&choice)
            .unwrap()
            .expose_to_provider()["version"],
        "2"
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}
