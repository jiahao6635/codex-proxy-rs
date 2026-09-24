use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use gateway_admin::{
    model::plugins::instances::PluginPermissionGrant, ports::provider::ProviderAdminErrorKind,
};
use gateway_core::{
    account::scope::{
        ClientRoutingScope, FrozenAccountScope, RuntimeAccount, RuntimeAccountDirectory,
    },
    engine::probe::AccountProbeRequest,
    error::GatewayErrorKind,
    operation::{GenerateRequest, Operation, ProtocolPayload},
    routing::{ProviderKind, UpstreamModelId},
};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant};

fn configuration() -> Value {
    json!({
        "model_discovery":{"include_static":false,"cache_ttl_seconds":60},
        "discovered_catalog":{"models":[{"id":"discovered-model","operations":["generate"]}],"exhaustive":true},
    })
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn grants() -> Vec<PluginPermissionGrant> {
    vec![account_grant("accounts"), account_grant("network")]
}

#[tokio::test]
async fn native_continuation_routing_requires_the_static_or_discovered_model_declaration() {
    for discovered in [false, true] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        environment.account(None).await;
        let models = json!([
            {"id":"continuation-model", "operations":["generate"], "features":["native_continuation"]},
            {"id":"plain-model", "operations":["generate"]}
        ]);
        let config = if discovered {
            json!({
                "model_discovery":{"include_static":false,"cache_ttl_seconds":60},
                "discovered_catalog":{"models":models,"exhaustive":true}
            })
        } else {
            json!({"static_models":models})
        };
        let (runtime, core) = environment
            .provider(config, vec![account_grant("accounts")])
            .await;
        let snapshot = core.snapshots().acquire().unwrap();
        for (model, supported) in [("continuation-model", true), ("plain-model", false)] {
            let operation = Operation::Generate(GenerateRequest::from_protocol_payload(
                ProtocolPayload::json_object("openai", json!({"model":model,"input":"next turn","previous_response_id":"response-before"}).as_object().unwrap().clone()).unwrap(),
            ));
            let plan = snapshot.plan(
                &gateway_core::routing::PublicModelId::new(model).unwrap(),
                &operation,
                snapshot.all_account_scope(),
                &gateway_core::routing::RoutingContext::default(),
            );
            assert_eq!(
                plan.is_ok(),
                supported,
                "discovered={discovered}, model={model}"
            );
        }
        drop(snapshot);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn directory_publication_is_independent_of_bounded_account_cache_eviction() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    // 比账号缓存多一个账号；每轮重建被淘汰项不得持续推进全局目录代次。
    for _ in 0..257 {
        environment.account(None).await;
    }
    let marker = environment.directory.path().join("large-models.jsonl");
    let mut config = configuration();
    config["models_marker"] = json!(marker);
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    assert_eq!(records(&marker).len(), 257);
    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers
        .get(&ProviderKind::new("example").unwrap())
        .unwrap();
    let generation = provider.catalog_generation();
    for _ in 0..3 {
        assert_eq!(
            provider.query_model_capabilities().await.unwrap()[0]
                .upstream_model()
                .as_str(),
            "discovered-model"
        );
        assert_eq!(provider.catalog_generation(), generation);
    }
    assert_eq!(records(&marker).len(), 257);
    drop(providers);
    drop(snapshot);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn directory_ttl_starts_after_the_complete_account_scan() {
    let diagnostics = crate::support::diagnostics::Diagnostics::default();
    let _diagnostics = diagnostics.install();
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    for _ in 0..8 {
        environment.account(None).await;
    }
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(configuration()["discovered_catalog"].clone())
                .set_delay(Duration::from_millis(550)),
        )
        .expect(8)
        .mount(&server)
        .await;
    let mut config = configuration();
    config["model_discovery"]["cache_ttl_seconds"] = json!(1);
    config["models_url"] = json!(server.uri());
    let (runtime, core) = environment.provider(config, grants()).await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        8,
        "首次目录扫描必须完成所有账号查询：{:?}",
        diagnostics.records()
    );
    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers
        .get(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        provider.query_model_capabilities().await.unwrap()[0]
            .upstream_model()
            .as_str(),
        "discovered-model"
    );
    let generation = provider.catalog_generation();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(provider.catalog_generation() > generation);
    // 到期只标记一次失效；同步代次读取不得发起后台 RPC 或重复推进版本。
    assert_eq!(provider.catalog_generation(), provider.catalog_generation());
    drop(providers);
    drop(snapshot);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn partial_directory_failure_reports_account_and_stage_without_model_payload() {
    let diagnostics = crate::support::diagnostics::Diagnostics::default();
    let _diagnostics = diagnostics.install();
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let rejected = environment.account(None).await;
    let mut config = configuration();
    config["account_catalogs"] = json!({
        rejected.as_str(): {"models":[{"id":"fixture-sensitive-model", "operations":[]}], "exhaustive":true}
    });
    let (runtime, core) = environment.provider(config, grants()).await;
    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers
        .get(&ProviderKind::new("example").unwrap())
        .unwrap();
    let models = provider.query_model_capabilities().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].upstream_model().as_str(), "discovered-model");
    let records = diagnostics.records();
    let failure = records
        .iter()
        .find(|fields| {
            fields
                .get("account_id")
                .is_some_and(|id| id.contains(rejected.as_str()))
        })
        .expect("部分失败不能在目录合并时丢失诊断");
    assert_eq!(failure["stage"], "\"validate\"");
    assert_eq!(failure["kind"], "BadGateway");
    assert_eq!(failure["timed_out"], "false");
    assert!(!format!("{records:?}").contains("fixture-sensitive-model"));
    drop(providers);
    drop(snapshot);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn account_model_cache_coalesces_reads_and_publishes_invalidation_generations() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let marker = environment.directory.path().join("models.jsonl");
    let mut config = configuration();
    config["models_marker"] = json!(marker);
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers
        .get(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(records(&marker).len(), 1);
    for result in futures::future::join_all((0..8).map(|_| admin.models(&id, false))).await {
        let result = result.unwrap();
        assert_eq!(result.models[0].id.as_str(), "discovered-model");
        assert!(result.observed_at.is_some());
    }
    assert_eq!(records(&marker).len(), 1);
    let generation = provider.catalog_generation();
    admin.account_facts_changed(std::slice::from_ref(&id)).await;
    assert!(provider.catalog_generation() > generation);
    for result in futures::future::join_all((0..8).map(|_| admin.models(&id, false))).await {
        result.unwrap();
    }
    assert_eq!(records(&marker).len(), 2);
    admin.models(&id, true).await.unwrap();
    assert_eq!(records(&marker).len(), 3);
    drop(providers);
    drop(snapshot);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn initial_model_discovery_commits_prepared_facts_without_refresh_reentry() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let marker = environment
        .directory
        .path()
        .join("prepared-account-facts.jsonl");
    let mut config = configuration();
    config["models_marker"] = json!(marker);
    config["prepared_facts_after_models"] = json!(0);
    config["prepared_account_facts"] = json!({
        "name":"plugin refreshed account",
        "authentication_kind":"api_key",
        "material":{"key":"rotated-test-only"},
        "email":"refreshed@example.invalid",
        "upstream_user_id":null,
        "upstream_account_id":null,
        "plan_type":"team",
        "has_refresh_token":false,
        "access_token_expires_at_ms":null,
        "next_refresh_at_ms":null
    });
    // 死锁预算只约束初始化与目录发布，不能被前置制品打包、校验和安装消耗。
    environment
        .install_provider(config, vec![account_grant("accounts")])
        .await;
    let (runtime, core) =
        tokio::time::timeout(Duration::from_secs(30), environment.runtime())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "initial discovery must not wait on its own snapshot publication: {error}; model queries={}",
                    records(&marker).len()
                )
            });
    // 首轮提交推进配置 revision，Core 用合并的候选 revision 再编译一次。
    assert_eq!(records(&marker).len(), 2);

    let accounts = environment.store.admin_ports().accounts();
    let details = accounts
        .credential_details(&ProviderKind::new("example").unwrap(), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(details.credential.credential_revision.get(), 2);
    assert_eq!(details.credential.name, "plugin refreshed account");
    let admin_bundle = environment.bind_admin_accounts(&runtime, &core).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();

    let result = provider.models(&id, true).await.unwrap();
    assert_eq!(result.models[0].id.as_str(), "discovered-model");

    assert_eq!(
        details.credential.email.as_deref(),
        Some("refreshed@example.invalid")
    );
    let exported = accounts
        .load_credentials_for_export(
            &ProviderKind::new("example").unwrap(),
            std::slice::from_ref(&id),
        )
        .await
        .unwrap();
    assert_eq!(
        exported[0]
            .provider_material
            .expose_to_provider()
            .expose_to_provider()["key"],
        "rotated-test-only"
    );
    let audit = environment.audit_requests("refresh_credential").await;
    assert_eq!(audit.len(), 1);
    assert!(audit[0].starts_with("plugin:"));
    assert!(audit[0].contains(":call:"));

    drop(provider);
    drop(admin_bundle);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn account_callbacks_use_host_scope_and_reject_the_stale_catalog_publication() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let other = environment.account(None).await;
    let marker = environment.directory.path().join("account-callbacks.jsonl");
    let mut config = configuration();
    config["account_callback_fixture"] = json!({
        "after_models":2,
        "marker":marker,
        "other_account_id":other.as_str(),
        "expected_key":"test-only",
        "replacement_key":"callback-rotated-test-only"
    });
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let admin_bundle = environment.bind_admin_accounts(&runtime, &core).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();

    let error = provider.models(&id, true).await.unwrap_err();
    assert_eq!(error.kind(), ProviderAdminErrorKind::Conflict);
    assert_eq!(
        records(&marker),
        [json!({
            "listed":2,
            "account_id":id.as_str(),
            "cross_account_readable":true,
            "read_revision":1,
            "saved_revision":2,
        })]
    );

    let accounts = environment.store.admin_ports().accounts();
    let details = accounts
        .credential_details(&ProviderKind::new("example").unwrap(), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(details.credential.credential_revision.get(), 2);
    assert_eq!(details.credential.name, "callback rotated account");
    let exported = accounts
        .load_credentials_for_export(
            &ProviderKind::new("example").unwrap(),
            std::slice::from_ref(&id),
        )
        .await
        .unwrap();
    assert_eq!(
        exported[0]
            .provider_material
            .expose_to_provider()
            .expose_to_provider()["key"],
        "callback-rotated-test-only"
    );
    let audit = environment.audit_requests("rotate_credential").await;
    assert_eq!(audit.len(), 1);
    assert!(audit[0].starts_with("plugin:"));
    assert!(audit[0].contains(":call:"));

    drop(provider);
    drop(admin_bundle);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn merged_catalog_keeps_client_scope_and_account_feature_gates() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let first = environment.account(None).await;
    let second = environment.account(None).await;
    let mut config = configuration();
    config["model_discovery"]["include_static"] = json!(true);
    config["account_catalogs"] = json!({
        (first.as_str()):{"models":[{"id":"first-model","operations":["generate"]},{"id":"shared","operations":["generate"]}],"exhaustive":true},
        (second.as_str()):{"models":[{"id":"second-model","operations":["generate"]},{"id":"shared","operations":["generate"],"features":["tools"],"maximum_output_tokens":32}],"exhaustive":true},
    });
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let kind = ProviderKind::new("example").unwrap();
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    assert_eq!(
        admin
            .models(&first, false)
            .await
            .unwrap()
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first-model", "plugin-model", "shared"]
    );
    let snapshot = core.snapshots().acquire().unwrap();
    let providers = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = providers.get(&kind).unwrap();
    let scope = FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(BTreeMap::from([(
            first.clone(),
            RuntimeAccount::new(kind.clone(), BTreeSet::new()),
        )]))),
        ClientRoutingScope::all_accounts(),
    );
    let client = provider
        .query_client_model_catalog(&scope, "openai", "test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        client
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        vec!["first-model", "plugin-model", "shared"]
    );
    let operation = |limit| {
        Operation::Generate(GenerateRequest::from_protocol_payload(ProtocolPayload::json_object("openai", json!({"model":"shared", "input":"hello", "tools":[{"type":"function", "name":"echo", "parameters":{"type":"object"}}], "max_output_tokens":limit}).as_object().unwrap().clone()).unwrap()))
    };
    let denied = core
        .account_probe()
        .probe(
            AccountProbeRequest {
                account_id: first,
                provider_kind: kind.clone(),
                upstream_model: UpstreamModelId::new("shared").unwrap(),
                operation: operation(16),
            },
            admin.snapshot(),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), GatewayErrorKind::NoAvailableProvider);
    let reply = core
        .account_probe()
        .probe(
            AccountProbeRequest {
                account_id: second.clone(),
                provider_kind: kind.clone(),
                upstream_model: UpstreamModelId::new("shared").unwrap(),
                operation: operation(16),
            },
            admin.snapshot(),
        )
        .await
        .unwrap();
    assert_eq!(reply.text, vec!["Rust plugin response"]);
    let denied = core
        .account_probe()
        .probe(
            AccountProbeRequest {
                account_id: second,
                provider_kind: kind,
                upstream_model: UpstreamModelId::new("shared").unwrap(),
                operation: operation(33),
            },
            admin.snapshot(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        denied.kind(),
        GatewayErrorKind::NoAvailableProvider | GatewayErrorKind::Unsupported
    ));
    drop(providers);
    drop(snapshot);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn model_http_uses_each_selected_account_without_permission_prebinding() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::header(
        "proxy-authorization",
        "Basic dXNlcjpwYXNz",
    ))
    .and(wiremock::matchers::header(
        "authorization",
        "Bearer test-only",
    ))
    .respond_with(
        wiremock::ResponseTemplate::new(200)
            .set_body_json(configuration()["discovered_catalog"].clone()),
    )
    .expect(2)
    .mount(&proxy)
    .await;
    let outbound_proxy = gateway_core::account::OutboundProxy::parse(&format!(
        "http://user:pass@{}",
        proxy.address()
    ))
    .unwrap();
    let id = environment.account(Some(outbound_proxy.clone())).await;
    let other = environment.account(Some(outbound_proxy)).await;
    let mut config = configuration();
    config["models_url"] = json!("http://127.0.0.1:9/models");
    let (runtime, core) = environment.provider(config, grants()).await;
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        admin.models(&id, false).await.unwrap().models[0]
            .id
            .as_str(),
        "discovered-model"
    );
    assert_eq!(
        admin.models(&other, false).await.unwrap().models[0]
            .id
            .as_str(),
        "discovered-model"
    );
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn malformed_refresh_does_not_replace_a_valid_account_catalog() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    let good = configuration()["discovered_catalog"].clone();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&good))
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let mut config = configuration();
    config["models_url"] = json!(server.uri());
    let (runtime, core) = environment.provider(config, grants()).await;
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    for models in [
        json!([{"id":"duplicate","operations":["generate"]},{"id":"duplicate","operations":["generate"]}]),
        json!([{"id":"bad","operations":[]}]),
        json!([{"id":"bad","operations":["generate"],"features":["tools","tools"]}]),
        json!([{"id":"bad","operations":["generate"],"maximum_output_tokens":0}]),
    ] {
        server.reset().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({"models":models,"exhaustive":true})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            admin.models(&id, true).await.unwrap_err().kind(),
            ProviderAdminErrorKind::BadGateway
        );
        assert_eq!(
            admin.models(&id, false).await.unwrap().models[0]
                .id
                .as_str(),
            "discovered-model"
        );
    }
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn invalidation_during_discovery_rejects_the_late_publication() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    let good = configuration()["discovered_catalog"].clone();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(&good)
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let marker = environment.directory.path().join("late-models.jsonl");
    let mut config = configuration();
    config["models_url"] = json!(server.uri());
    config["models_marker"] = json!(marker);
    let (runtime, core) = environment.provider(config, grants()).await;
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let invalidate = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while records(&marker).len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        admin.account_facts_changed(std::slice::from_ref(&id)).await;
    };
    let (result, ()) = tokio::join!(admin.models(&id, true), invalidate);
    assert_eq!(result.unwrap_err().kind(), ProviderAdminErrorKind::Conflict);
    admin.models(&id, false).await.unwrap();
    assert_eq!(records(&marker).len(), 3);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}
