use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::TryStreamExt as _;
use gateway_admin::{
    CredentialsService,
    model::{
        plugins::PluginSource,
        provider_credentials::{CredentialMutation, ProviderDocument, RotateCredential},
    },
    ports::plugins::{PluginPackageInspector as _, PluginPreparation, PluginStore},
};
use gateway_core::{
    account::{AccountSelectionPolicy, OpaqueProviderData, RotationStrategy},
    engine::{
        AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
        provider::ProviderRequest,
    },
    error::{ProviderError, ProviderErrorKind},
    event::{GatewayEvent, ProviderEvent, UpstreamHttpVersion},
    lifecycle::CancellationToken,
    operation::ProviderSessionState,
    policy::ClientApiKeyId,
    routing::{ConfigRevision, ProviderKind, PublicModelId, RoutingContext, UpstreamModelId},
    upstream::UpstreamSendState,
};
use gateway_plugin_runtime::{
    ContinuationDrainConfig, PackageInspector, PackageLimits, ValidatedPackage,
};
use serde_json::json;

use crate::support::{
    archive,
    environment::{Environment, account_grant, mutation},
    worker,
};

pub(super) async fn execute(
    runtime: &gateway_plugin_runtime::PluginRuntime,
    core: &gateway_core::CoreBundle,
    state: Option<ProviderSessionState>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let kind = ProviderKind::new("example").unwrap();
    let snapshot = core.snapshots().acquire().unwrap();
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    let mut operation = admin
        .connection_test_operation(&UpstreamModelId::new("plugin-model").unwrap(), "events")
        .await
        .unwrap();
    if let Some(state) = state {
        operation.set_provider_session_state(state);
    }
    let plan = snapshot
        .plan(
            &PublicModelId::new("plugin-model").unwrap(),
            &operation,
            snapshot.all_account_scope(),
            &RoutingContext::default(),
        )
        .unwrap();
    let registry = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = registry.get(&kind).unwrap();
    let context = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_plugin_events").unwrap(),
            ClientApiKeyId::new("key_plugin_events").unwrap(),
        ),
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(15),
        AccountSelectionPolicy::new(
            RotationStrategy::RoundRobin,
            NonZeroU32::MIN,
            Duration::ZERO,
        ),
        AccountAttemptContext::default().with_account_scope(snapshot.all_account_scope()),
        None,
        CancellationToken::new(),
    );
    Arc::clone(provider)
        .execute(
            ProviderRequest::new(operation, plan.candidates()[0].clone()),
            context,
        )
        .await?
        .try_collect()
        .await
}

fn continuation_state(events: &[ProviderEvent]) -> ProviderSessionState {
    events
        .last()
        .and_then(ProviderEvent::session_update)
        .cloned()
        .expect("terminal event continuation state")
}

fn assert_replay_required(error: &ProviderError) {
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert_eq!(error.send_state(), UpstreamSendState::NotSent);
}

async fn wait_until_entry_count(path: &Path, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match std::fs::read_dir(path) {
                Ok(entries) => {
                    if entries.count() == expected {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && expected == 0 => {
                    break;
                }
                Err(error) => panic!("read plugin cache: {error}"),
            }
        }
    })
    .await
    .expect("plugin generation resources should reach the expected count");
}

async fn publish(core: &gateway_core::CoreBundle, revision: u64) {
    core.snapshot_control()
        .publish_committed(ConfigRevision::new(revision).unwrap())
        .await;
}

async fn install_artifact_version(
    store: &dyn PluginStore,
    source_digest: &str,
    version: &str,
) -> String {
    let artifact = store.load_artifact(source_digest).await.unwrap();
    let package = ValidatedPackage::read(artifact.archive, None, PackageLimits::default()).unwrap();
    let mut manifest = package.manifest().clone();
    manifest.version = version.parse().unwrap();
    let archive = archive(BTreeMap::from([
        ("plugin.json".into(), serde_json::to_vec(&manifest).unwrap()),
        ("bin/worker".into(), worker().to_vec()),
    ]));
    let artifact = PackageInspector::new(PackageLimits::default(), "1.0.0".parse().unwrap())
        .inspect(archive, None)
        .await
        .unwrap();
    let installed = store
        .install_artifact(artifact, PluginSource::Upload, &mutation())
        .await
        .unwrap();
    store
        .accept_artifact(&installed.artifact.metadata.sha256, &mutation())
        .await
        .unwrap()
        .artifact
        .metadata
        .sha256
}

#[tokio::test]
async fn event_envelope_preserves_raw_bytes_metadata_and_bound_continuation() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let raw = b": keep comment\r\nid: frame-1\r\nretry: 123\r\nevent: response.created\r\ndata: {\"type\":\"response.created\",\r\ndata: \"response\":{\"id\":\"response-plugin\"}}\r\n\r\n".as_slice();
    let raw_json = b"{ \"z\": 1, \"a\": 2 }\n".as_slice();
    let raw_body = b"opaque raw http body".as_slice();
    let (runtime, core) = environment.provider(json!({
        "output_formats":["canonical","openai"],
        "execution_events":[
            {"observation":{"status":200,"request_id":"opaque request-id","http_version":"http2","timings":{"headers_ms":7},"client_headers":[{"name":"retry-after","value":b"12".as_slice()}]}},
            {"facts":[{"type":"started","id":"response-plugin","model":"plugin-model"}],"wire":{"protocol":"openai","payload":{"kind":"json","event":"response.created","id":"frame-1","retry":123,"data":{"type":"response.created","response":{"id":"response-plugin"}},"raw_sse":raw}}},
            {"wire":{"protocol":"openai","payload":{"kind":"raw_sse","frame":b": comment only\n\n".as_slice()}}},
            {"wire":{"protocol":"openai","payload":{"kind":"raw_json","body":raw_json}}},
            {"observation":{"status":200,"client_headers":[{"name":"content-type","value":b"application/octet-stream".as_slice()}]},"wire":{"protocol":"openai","payload":{"kind":"raw_body","body":raw_body}}},
            {"facts":[{"type":"usage","usage":{"input_tokens":7}},{"type":"completed","id":"response-plugin","model":"plugin-model","reason":"stop"}],"wire":{"protocol":"openai","payload":{"kind":"json","event":"response.completed","data":{"type":"response.completed","response":{"id":"response-plugin"}}}},"session_update":{"payload":{"turn":"private checkpoint"}}}
        ]
    }), vec![account_grant("accounts")]).await;
    let events = execute(&runtime, &core, None).await.unwrap();
    assert_eq!(events.len(), 6);
    let observation = events[0].response_observation().unwrap();
    assert_eq!(observation.status_code(), Some(200));
    assert_eq!(
        observation.request_id().unwrap().as_str(),
        "opaque request-id"
    );
    assert_eq!(observation.http_version(), Some(UpstreamHttpVersion::Http2));
    assert_eq!(observation.timings().headers_ms, Some(7));
    assert_eq!(
        observation.client_headers()[0].value().as_ref(),
        b"12".as_slice()
    );
    assert_eq!(
        events[1]
            .wire_event()
            .unwrap()
            .raw_sse_frame()
            .unwrap()
            .as_ref(),
        raw
    );
    assert!(matches!(
        events[1].canonical_facts(),
        [GatewayEvent::Started(_)]
    ));
    assert_eq!(
        events[2]
            .wire_event()
            .unwrap()
            .raw_sse_frame()
            .unwrap()
            .as_ref(),
        b": comment only\n\n".as_slice()
    );
    assert_eq!(
        events[3]
            .wire_event()
            .unwrap()
            .raw_json_body()
            .unwrap()
            .as_ref(),
        raw_json
    );
    assert_eq!(
        events[4]
            .wire_event()
            .unwrap()
            .raw_http_body_bytes()
            .unwrap()
            .as_ref(),
        raw_body
    );
    let raw_observation = events[4].response_observation().unwrap();
    assert_eq!(raw_observation.client_headers().len(), 1);
    assert_eq!(raw_observation.client_headers()[0].name(), "content-type");
    let state = events[5].session_update().unwrap();
    assert_eq!(state.provider(), "example");
    assert_eq!(
        execute(&runtime, &core, Some(state.clone()))
            .await
            .unwrap()
            .len(),
        6
    );
    // 宿主私有绑定在 RPC 前拒绝，不能切账号、换凭据或跨实例授权重用。
    for (field, value) in [
        ("account_id", json!("another-account")),
        ("credential_revision", json!(999)),
        ("authorization", json!("other-instance")),
        ("continuation_authorization", json!("other-authority")),
        ("execution_owner", json!("not-a-host-owner")),
        ("provider", json!("other-provider")),
    ] {
        let mut payload = state.payload().clone();
        payload["binding"][field] = value;
        let invalid = ProviderSessionState::new("example", payload).unwrap();
        let error = execute(&runtime, &core, Some(invalid)).await.unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert_eq!(error.send_state(), UpstreamSendState::NotSent);
    }
    drop(events);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn declared_continuation_format_survives_artifact_upgrade_but_not_authority_changes() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account_id = environment.account(None).await;
    let grants = vec![account_grant("accounts")];
    environment
        .install_provider(
            json!({
                "continuation_state":{"format":"example.responses","write_version":1,"readable_versions":[1]},
                "execution_events":[
                    {"facts":[{"type":"started","id":"response-plugin","model":"plugin-model"}]},
                    {"facts":[{"type":"completed","id":"response-plugin","model":"plugin-model","reason":"stop"}],"session_update":{"payload":{"turn":"opaque-v1"}}}
                ]
            }),
            grants,
        )
        .await;

    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let initial_digest = snapshot.instances[0].artifact_sha256.clone();
    let artifact = store.load_artifact(&initial_digest).await.unwrap();
    let package = ValidatedPackage::read(artifact.archive, None, PackageLimits::default()).unwrap();
    let mut manifest = package.manifest().clone();
    manifest.version = "1.0.1".parse().unwrap();
    let next_archive = archive(BTreeMap::from([
        ("plugin.json".into(), serde_json::to_vec(&manifest).unwrap()),
        ("bin/worker".into(), worker().to_vec()),
    ]));
    let next_artifact = PackageInspector::new(PackageLimits::default(), "1.0.0".parse().unwrap())
        .inspect(next_archive, None)
        .await
        .unwrap();
    let installed = store
        .install_artifact(next_artifact, PluginSource::Upload, &mutation())
        .await
        .unwrap();
    let installed = store
        .accept_artifact(&installed.artifact.metadata.sha256, &mutation())
        .await
        .unwrap();
    let next_digest = installed.artifact.metadata.sha256;
    assert_ne!(next_digest, initial_digest);

    // 测试 worker 依据不可变制品摘要返回该版本自身的格式声明；实例配置在升级前后不变。
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    let mut by_artifact = serde_json::Map::new();
    by_artifact.insert(
        next_digest.clone(),
        json!({"format":"example.responses","write_version":2,"readable_versions":[1,2]}),
    );
    instance.configuration.as_object_mut().unwrap().insert(
        "continuation_state_by_artifact".into(),
        serde_json::Value::Object(by_artifact),
    );
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();

    let (runtime, core) = environment.runtime().await;
    let events = execute(&runtime, &core, None).await.unwrap();
    let state = continuation_state(&events);
    assert_eq!(
        state.payload()["binding"]["continuation_state"],
        json!({"format":"example.responses","version":1})
    );

    // 发布新制品时旧快照模拟在途请求；发布后新请求使用新解释器，旧资源直到引用释放才回收。
    let old_snapshot = core.snapshots().acquire().unwrap();
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.artifact_sha256.clone_from(&next_digest);
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    drop(events);
    let events = execute(&runtime, &core, Some(state.clone())).await.unwrap();
    let upgraded_state = continuation_state(&events);
    assert_eq!(
        upgraded_state.payload()["binding"]["continuation_state"],
        json!({"format":"example.responses","version":2})
    );
    assert!(execute(&runtime, &core, Some(upgraded_state)).await.is_ok());
    assert_eq!(
        std::fs::read_dir(environment.directory.path().join("cache"))
            .unwrap()
            .count(),
        2
    );
    drop(old_snapshot);
    wait_until_entry_count(&environment.directory.path().join("cache"), 1).await;

    for (field, value) in [("format", json!("another.format")), ("version", json!(3))] {
        let mut payload = state.payload().clone();
        payload["binding"]["continuation_state"][field] = value;
        let invalid = ProviderSessionState::new("example", payload).unwrap();
        assert_replay_required(&execute(&runtime, &core, Some(invalid)).await.unwrap_err());
    }

    drop(events);
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance
        .configuration
        .as_object_mut()
        .unwrap()
        .insert("authorization_change".into(), json!(true));
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    assert_replay_required(
        &execute(&runtime, &core, Some(state.clone()))
            .await
            .unwrap_err(),
    );

    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance
        .configuration
        .as_object_mut()
        .unwrap()
        .remove("authorization_change");
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    assert!(execute(&runtime, &core, Some(state.clone())).await.is_ok());

    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance
        .grants
        .retain(|grant| grant.permission != "accounts");
    // 授权来自制品接受事实；配置写入不能另造一份权限集合。
    assert!(
        store
            .save_instance(instance, snapshot.config_revision, &mutation())
            .await
            .is_err()
    );
    assert!(execute(&runtime, &core, Some(state.clone())).await.is_ok());

    let registry = runtime.admin_registry(core.snapshots());
    let admin = environment.store.admin_ports();
    let credentials = CredentialsService::new(
        registry.clone(),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    credentials
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id,
                context: mutation(),
            },
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                json!({"key":"rotated-continuation-test"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
            settings: None,
        })
        .await
        .unwrap();
    assert_replay_required(&execute(&runtime, &core, Some(state)).await.unwrap_err());

    drop(credentials);
    drop(registry);
    drop(admin);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}

#[tokio::test]
async fn incompatible_upgrade_uses_retired_executor_but_current_disable_wins() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    environment
        .install_provider(
            json!({
                "execution_events":[
                    {"facts":[{"type":"started","id":"response-old","model":"plugin-model"}]},
                    {"facts":[{"type":"completed","id":"response-old","model":"plugin-model","reason":"stop"}],"session_update":{"payload":{"executor":"old"}}}
                ]
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let initial_digest = snapshot.instances[0].artifact_sha256.clone();
    let next_digest = install_artifact_version(store.as_ref(), &initial_digest, "1.0.1").await;
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.configuration["execution_events_by_artifact"] = json!({
        next_digest.clone():[
            {"facts":[{"type":"started","id":"response-new","model":"plugin-model"}]},
            {"facts":[{"type":"completed","id":"response-new","model":"plugin-model","reason":"stop"}],"session_update":{"payload":{"executor":"new"}}}
        ]
    });
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();

    let (runtime, core) = environment.runtime().await;
    let initial = execute(&runtime, &core, None).await.unwrap();
    let state = continuation_state(&initial);
    assert_eq!(state.payload()["payload"]["executor"], "old");
    drop(initial);

    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.artifact_sha256.clone_from(&next_digest);
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    let resumed = execute(&runtime, &core, Some(state.clone())).await.unwrap();
    assert_eq!(
        continuation_state(&resumed).payload()["payload"]["executor"],
        "old"
    );
    drop(resumed);

    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.enabled = false;
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    let snapshot = core.snapshots().acquire().unwrap();
    let registry = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    assert!(!registry.contains(&ProviderKind::new("example").unwrap()));
    drop(registry);
    drop(snapshot);

    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.enabled = true;
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    // 停用期间入口已移除；重新启用相同权限与配置后，未到期的旧执行器仍可排空续链。
    let resumed = execute(&runtime, &core, Some(state)).await.unwrap();
    assert_eq!(
        continuation_state(&resumed).payload()["payload"]["executor"],
        "old"
    );
    drop(resumed);

    // 当前代次仍由 Core 保活，旧执行器仍由续写排空表保活；显式关闭必须同时回收两者。
    runtime.shutdown().await;
    wait_until_entry_count(&environment.directory.path().join("cache"), 0).await;

    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}

#[tokio::test]
async fn retired_executor_expires_without_sliding_on_continuation_use() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    environment
        .install_provider(
            json!({
                "execution_events":[
                    {"facts":[{"type":"started","id":"response","model":"plugin-model"}]},
                    {"facts":[{"type":"completed","id":"response","model":"plugin-model","reason":"stop"}],"session_update":{"payload":{"turn":1}}}
                ]
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let initial_digest = snapshot.instances[0].artifact_sha256.clone();
    let next_digest = install_artifact_version(store.as_ref(), &initial_digest, "1.0.1").await;
    let (runtime, core) = environment
        .runtime_with_continuation_drain(ContinuationDrainConfig {
            retention: Duration::from_millis(500),
            maximum_generations: NonZeroUsize::new(64).unwrap(),
            maximum_per_instance: NonZeroUsize::new(2).unwrap(),
        })
        .await;
    let state = continuation_state(&execute(&runtime, &core, None).await.unwrap());
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.artifact_sha256 = next_digest;
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    let expected = store.load_instances().await.unwrap();
    let published = core
        .snapshots()
        .snapshot_for_diagnostics()
        .expect("published plugin generation");
    let diagnostics = PluginPreparation::runtime_diagnostics(
        runtime.as_ref(),
        &expected,
        Some(published.revision().get()),
        published.extensions(),
    )
    .await
    .expect("runtime diagnostics");
    assert_eq!(
        diagnostics[&expected.instances[0].id].retained_continuations,
        1
    );
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(execute(&runtime, &core, Some(state.clone())).await.is_ok());
    // 使用旧执行器不会延长从撤下时开始的固定期限。
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_replay_required(&execute(&runtime, &core, Some(state)).await.unwrap_err());
    wait_until_entry_count(&environment.directory.path().join("cache"), 1).await;

    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}

#[tokio::test]
async fn retired_executor_capacity_evicts_the_oldest_owner() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    environment
        .install_provider(
            json!({
                "execution_events":[
                    {"facts":[{"type":"started","id":"response","model":"plugin-model"}]},
                    {"facts":[{"type":"completed","id":"response","model":"plugin-model","reason":"stop"}],"session_update":{"payload":{"turn":1}}}
                ]
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let digest1 = snapshot.instances[0].artifact_sha256.clone();
    let digest2 = install_artifact_version(store.as_ref(), &digest1, "1.0.1").await;
    let digest3 = install_artifact_version(store.as_ref(), &digest1, "1.0.2").await;
    let digest4 = install_artifact_version(store.as_ref(), &digest1, "1.0.3").await;
    let (runtime, core) = environment
        .runtime_with_continuation_drain(ContinuationDrainConfig {
            retention: Duration::from_secs(30 * 60),
            maximum_generations: NonZeroUsize::new(2).unwrap(),
            maximum_per_instance: NonZeroUsize::new(2).unwrap(),
        })
        .await;
    let state1 = continuation_state(&execute(&runtime, &core, None).await.unwrap());

    let mut states = Vec::new();
    for digest in [&digest2, &digest3] {
        let snapshot = store.load_instances().await.unwrap();
        let mut instance = snapshot.instances.into_iter().next().unwrap();
        instance.artifact_sha256.clone_from(digest);
        let saved = store
            .save_instance(instance, snapshot.config_revision, &mutation())
            .await
            .unwrap();
        publish(&core, saved.config_revision.get()).await;
        states.push(continuation_state(
            &execute(&runtime, &core, None).await.unwrap(),
        ));
    }
    let snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.into_iter().next().unwrap();
    instance.artifact_sha256 = digest4;
    let saved = store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    publish(&core, saved.config_revision.get()).await;
    wait_until_entry_count(&environment.directory.path().join("cache"), 3).await;

    assert_replay_required(&execute(&runtime, &core, Some(state1)).await.unwrap_err());
    assert!(
        execute(&runtime, &core, Some(states.remove(0)))
            .await
            .is_ok()
    );
    assert!(
        execute(&runtime, &core, Some(states.remove(0)))
            .await
            .is_ok()
    );

    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}

#[tokio::test]
async fn continuation_format_contract_is_bounded_unique_and_self_readable() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({"continuation_state":{"format":"example.responses","write_version":1,"readable_versions":[1]}}),
            vec![account_grant("accounts")],
        )
        .await;
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    let mut cases = vec![
        json!({"format":"","write_version":1,"readable_versions":[1]}),
        json!({"format":"unsafe/path","write_version":1,"readable_versions":[1]}),
        json!({"format":"example.responses","write_version":0,"readable_versions":[1]}),
        json!({"format":"example.responses","write_version":1,"readable_versions":[]}),
        json!({"format":"example.responses","write_version":2,"readable_versions":[1]}),
        json!({"format":"example.responses","write_version":1,"readable_versions":[1,1]}),
    ];
    cases.push(json!({
        "format":"example.responses",
        "write_version":1,
        "readable_versions":(1_u32..=33).collect::<Vec<_>>()
    }));
    for descriptor in cases {
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration["continuation_state"] = descriptor;
        let error =
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .expect_err("invalid continuation descriptor");
        assert_eq!(error.message(), "插件续写状态兼容声明无效");
    }
    drop(store);
    environment.release_plugin_accounts(&runtime);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn event_envelope_rejects_ambiguous_frames_unsafe_headers_and_unbound_state() {
    let invalid = [
        json!({}),
        json!({"wire":{"protocol":"undeclared","payload":{"kind":"raw_sse","frame":b"data: {}\n\n".as_slice()}}}),
        json!({"wire":{"protocol":"openai","payload":{"kind":"raw_sse","frame":b"data: {}\n\ndata: {}\n\n".as_slice()}}}),
        json!({"wire":{"protocol":"openai","payload":{"kind":"raw_sse","frame":b"data: {}\n".as_slice()}}}),
        json!({"wire":{"protocol":"openai","payload":{"kind":"json","event":null,"data":{"actual":false},"raw_sse":b"data: {\"actual\":true}\n\n".as_slice()}}}),
        json!({"wire":{"protocol":"openai","payload":{"kind":"raw_json","body":b"{} {}".as_slice()}}}),
        json!({"observation":{"status":600}}),
        json!({"observation":{"client_headers":[{"name":"set-cookie","value":b"private=value".as_slice()}]}}),
        json!({"observation":{"client_headers":[{"name":"x-safe","value":b"injected\r\nheader: value".as_slice()}]}}),
        json!({"facts":[{"type":"started","id":"id","model":null}],"session_update":{"payload":{}}}),
        json!({"facts":[{"type":"completed","id":"id","model":null,"reason":"stop"}],"session_update":{"payload":{"too_large":""}}}),
        json!({"observation":{"client_headers":[{"name":"content-type","value":b"application/json".as_slice()}]},"wire":{"protocol":"openai","payload":{"kind":"raw_json","body":b"{}".as_slice()}}}),
    ];
    for (index, event) in invalid.into_iter().enumerate() {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        environment.account(None).await;
        let (runtime, core) = environment
            .provider(
                json!({"output_formats":["canonical","openai"],"execution_events":[event],"oversize_session_payload":index == 10}),
                vec![account_grant("accounts")],
            )
            .await;
        let error = execute(&runtime, &core, None).await.unwrap_err();
        assert_eq!(
            error.kind(),
            ProviderErrorKind::Protocol,
            "invalid envelope case {index}"
        );
        assert_eq!(error.send_state(), UpstreamSendState::NotSent);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn structured_execution_failure_keeps_client_body_and_host_send_watermark() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(429).set_body_string("upstream reached"))
        .expect(1)
        .mount(&upstream)
        .await;
    let network = account_grant("network");
    let body = b"{ \"error\": {\"message\":\"private upstream response\"} }\n".as_slice();
    let (runtime, core) = environment.provider(json!({
        "http_url":upstream.uri(),
        "execution_events":[{
            "observation":{"status":429,"request_id":"private-request-id","client_headers":[{"name":"retry-after","value":b"2".as_slice()}]},
            "failure":{"kind":"rate_limited","send_state":"not_sent","code":"private-code","retry_after_ms":2000,"response":{"status":429,"content_type":b"application/json".as_slice(),"body":body}}
        }]
    }), vec![account_grant("accounts"), network]).await;
    let error = execute(&runtime, &core, None).await.unwrap_err();
    assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
    assert_eq!(error.send_state(), UpstreamSendState::Sent);
    assert_eq!(error.upstream_status(), Some(429));
    assert_eq!(
        error.upstream_request_id().unwrap().as_str(),
        "private-request-id"
    );
    assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
    let response = error.client_visible_upstream_response().unwrap();
    assert_eq!(response.body().as_ref(), body);
    assert_eq!(response.headers()[0].value().as_ref(), b"2".as_slice());
    assert!(error.clone().client_visible_upstream_response().is_none());
    let diagnostic = format!("{error:?}");
    assert!(!diagnostic.contains("private"));
    drop(error);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn failure_wire_is_atomic_and_cannot_also_report_completion() {
    for completed in [false, true] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        environment.account(None).await;
        let raw = b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n".as_slice();
        let facts = if completed {
            json!([{"type":"completed","id":"response","model":null,"reason":"stop"}])
        } else {
            json!([])
        };
        let (runtime, core) = environment.provider(json!({
            "output_formats":["canonical","openai"],
            "execution_events":[{"facts":facts,"wire":{"protocol":"openai","payload":{"kind":"raw_sse","frame":raw}},"failure":{"kind":"unavailable","send_state":"sent","code":null,"retry_after_ms":null,"response":null}}]
        }), vec![account_grant("accounts")]).await;
        let mut error = execute(&runtime, &core, None).await.unwrap_err();
        if completed {
            assert_eq!(error.kind(), ProviderErrorKind::Protocol);
            assert_eq!(error.send_state(), UpstreamSendState::NotSent);
        } else {
            assert_eq!(error.kind(), ProviderErrorKind::Unavailable);
            assert_eq!(error.send_state(), UpstreamSendState::Sent);
            assert!(error.clone().take_atomic_client_events().is_empty());
            let events = error.take_atomic_client_events();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0]
                    .wire_event()
                    .unwrap()
                    .raw_sse_frame()
                    .unwrap()
                    .as_ref(),
                raw
            );
        }
        drop(error);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}
