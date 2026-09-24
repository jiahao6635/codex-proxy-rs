use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

use gateway_admin::{
    model::{
        Revision,
        plugins::instances::{
            PluginCapabilityBinding, PluginFailurePolicy, PluginInstance, PluginInstanceSnapshot,
            PluginPermissionGrant,
        },
    },
    ports::plugins::PluginPackageInspector,
};
use gateway_core::{
    account::ProviderAccountId,
    diagnostics::diagnostic_json,
    engine::{
        ModelRequestId,
        observation::{WebSocketResponseAttempt, WebSocketResponseObservation},
        provider::ProviderRegistry,
    },
    event::ProtocolWireEvent,
    identity::ProviderKind,
    operation::OperationKind,
    policy::ClientApiKeyId,
    routing::{ConfigRevision, PublicModelId},
    runtime::extensions::{ExtensionPreparationPort, ExtensionSetReference},
};
use gateway_plugin_runtime::{
    PackageInspector, PackageLimits, PluginRuntime, PluginRuntimeConfig, RpcLimits,
};
use serde_json::{Value, json};

use crate::support::store::Store;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

struct Fields(BTreeMap<String, String>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "gateway_plugin"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() == "gateway_plugin" {
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

impl Capture {
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.0.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("independent observer log callback must complete");
    }

    fn assert_summary(&self, instance: &str, generation: u64, expected: Value) {
        let logs = self.0.lock().unwrap();
        let row = logs
            .iter()
            .rev()
            .find(|row| {
                row["instance_id"] == instance && row["generation"] == generation.to_string()
            })
            .expect("observer generation summary");
        assert_eq!(
            serde_json::from_str::<Value>(&row["fields"]).unwrap(),
            diagnostic_json(&expected)
        );
        assert!(!row["fields"].contains("fixture-private-payload"));
    }
}

fn grant(permission: &str) -> PluginPermissionGrant {
    PluginPermissionGrant {
        permission: permission.to_owned(),
    }
}

fn binding(contribution: &str) -> PluginCapabilityBinding {
    PluginCapabilityBinding {
        contribution: contribution.to_owned(),
        stage: "observation".to_owned(),
        order: 1,
        failure_policy: PluginFailurePolicy::Observe,
        client_key_ids: Vec::new(),
        account_group_ids: Vec::new(),
        provider_ids: vec!["openai".to_owned()],
        models: vec!["gpt-observed".to_owned()],
        identity_bindings: Vec::new(),
    }
}

async fn runtime(cache: &std::path::Path) -> (Arc<Store>, PluginRuntime) {
    let inspector = PackageInspector::new(PackageLimits::default(), "3.12.1".parse().unwrap());
    let mut artifacts = BTreeMap::new();
    let mut instances = Vec::new();
    for (variable, plugin, permissions) in [
        (
            "CPR_PLUGIN_WS_COUNTER_ARCHIVE",
            "example-ws-event-counter",
            vec![],
        ),
        (
            "CPR_PLUGIN_WS_METRICS_ARCHIVE",
            "example-ws-payload-metrics",
            vec![grant("requests")],
        ),
    ] {
        let path = std::env::var_os(variable).unwrap_or_else(|| {
            panic!("{variable} must point to a packaged independent SDK example")
        });
        let archive = std::fs::read(path).expect("independent observer archive");
        let artifact = inspector.inspect(archive.into(), None).await.unwrap();
        assert_eq!(artifact.metadata.plugin_id, plugin);
        let contribution = artifact.metadata.contributes["web_socket_observer"]
            .id
            .clone();
        instances.push(PluginInstance {
            id: plugin.to_owned(),
            name: plugin.to_owned(),
            artifact_sha256: artifact.metadata.sha256.clone(),
            enabled: true,
            trusted_process: true,
            configuration: json!({}),
            secrets: BTreeMap::new(),
            grants: permissions,
            bindings: vec![binding(&contribution)],
            revision: Revision::new(1).unwrap(),
        });
        artifacts.insert(artifact.metadata.sha256.clone(), artifact);
    }
    let store = Arc::new(Store {
        artifacts,
        snapshot: Mutex::new(PluginInstanceSnapshot {
            config_revision: Revision::new(1).unwrap(),
            instances,
        }),
    });
    let runtime = PluginRuntime::new(
        store.clone(),
        store.clone(),
        PluginRuntimeConfig {
            cache_directory: cache.to_owned(),
            host_version: "3.12.1".parse().unwrap(),
            package_limits: PackageLimits::default(),
            rpc_limits: RpcLimits::default(),
            continuation_drain: Default::default(),
            restart_circuit: Default::default(),
        },
        ProviderRegistry::default(),
        gateway_admin::ports::provider::ProviderAdminRegistry::new([]).unwrap(),
        Arc::new(gateway_host::outbound::HttpClient::new().unwrap()),
        Arc::new(gateway_host::process::ProcessSupervisor::new(
            std::num::NonZeroUsize::new(8).unwrap(),
        )),
    );
    (store, runtime)
}

fn observation(
    generation: u64,
    sequence: u64,
    model: &str,
    body: &Value,
) -> WebSocketResponseObservation {
    WebSocketResponseObservation::new(
        ModelRequestId::new("req_observer_example").unwrap(),
        ConfigRevision::new(generation).unwrap(),
        OperationKind::Generate,
        WebSocketResponseAttempt::new(
            ProviderKind::new("openai").unwrap(),
            ProviderAccountId::new("acct_observed").unwrap(),
            NonZeroU32::new(1).unwrap(),
        ),
        sequence,
        ProtocolWireEvent::json(
            "openai",
            body["type"].as_str().map(str::to_owned),
            body.clone(),
        )
        .unwrap(),
    )
    .with_client_scope(
        ClientApiKeyId::new("key_observer_example").unwrap(),
        Vec::new(),
    )
    .with_requested_model(PublicModelId::new(model).unwrap())
}

fn dispatch(
    runtime: &PluginRuntime,
    generation: &ExtensionSetReference,
    revision: u64,
    sequence: u64,
    model: &str,
    body: &Value,
) {
    runtime
        .observer_registry()
        .resolve(generation)
        .unwrap()
        .dispatch_websocket_response(
            generation.clone(),
            observation(revision, sequence, model, body),
        );
}

/// 必须单独按名称显式执行；真实包缺失时报错，不将未运行的依赖场景算作通过。
#[tokio::test]
#[ignore = "requires two packaged standalone observer examples and an isolated log subscriber"]
async fn actual_observer_examples_share_runtime_contract_with_separate_permissions_and_generations()
{
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (store, runtime) = runtime(cache.path()).await;
    let first = ExtensionPreparationPort::prepare(&runtime, ConfigRevision::new(1).unwrap())
        .await
        .unwrap();
    let body = json!({"type":"response.output_text.delta","delta":"fixture-private-payload 中文","unknown":true});
    let bytes = serde_json::to_vec(&body).unwrap().len();
    let text_bytes = body["delta"].as_str().unwrap().len();
    dispatch(&runtime, &first, 1, 1, "gpt-observed", &body);
    capture.wait_for(2).await;
    dispatch(&runtime, &first, 1, 2, "other-model", &body);
    dispatch(&runtime, &first, 1, 9, "gpt-observed", &body);
    capture.wait_for(4).await;
    capture.assert_summary(
        "example-ws-event-counter",
        1,
        json!({"events":2,"first_events":1,"maximum_sequence":9}),
    );
    capture.assert_summary("example-ws-payload-metrics", 1, json!({"delivered_events":2,"payload_bytes":bytes*2,"text_delta_bytes":text_bytes*2,"terminal_events":0}));
    {
        let mut snapshot = store.snapshot.lock().unwrap();
        snapshot.config_revision = Revision::new(2).unwrap();
        for instance in &mut snapshot.instances {
            instance.revision = Revision::new(2).unwrap();
        }
    }
    let second = ExtensionPreparationPort::prepare(&runtime, ConfigRevision::new(2).unwrap())
        .await
        .unwrap();
    dispatch(&runtime, &second, 2, 1, "gpt-observed", &body);
    dispatch(&runtime, &first, 1, 10, "gpt-observed", &body);
    capture.wait_for(8).await;
    capture.assert_summary(
        "example-ws-event-counter",
        2,
        json!({"events":1,"first_events":1,"maximum_sequence":1}),
    );
    capture.assert_summary(
        "example-ws-event-counter",
        1,
        json!({"events":3,"first_events":1,"maximum_sequence":10}),
    );
    capture.assert_summary("example-ws-payload-metrics", 2, json!({"delivered_events":1,"payload_bytes":bytes,"text_delta_bytes":text_bytes,"terminal_events":0}));
    capture.assert_summary("example-ws-payload-metrics", 1, json!({"delivered_events":3,"payload_bytes":bytes*3,"text_delta_bytes":text_bytes*3,"terminal_events":0}));
    assert_eq!(capture.0.lock().unwrap().len(), 8);
    runtime.shutdown().await;
    drop(first);
    drop(second);
    super::wait_until_empty(cache.path()).await;
}
