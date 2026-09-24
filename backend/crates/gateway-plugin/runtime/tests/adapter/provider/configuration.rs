use gateway_admin::{
    CredentialsService,
    model::provider_credentials::{CredentialMutation, ProviderDocument, RotateCredential},
    ports::{plugins::PluginPreparation, provider::ProviderAdminErrorKind},
};
use gateway_core::{account::OpaqueProviderData, routing::ProviderKind};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn configuration(result: Value) -> Value {
    json!({
        "credential_input_schemas": {
            "rotate": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "base_url": {"type": "string", "format": "uri", "maxLength": 2048},
                    "transport": {"type": "string", "enum": ["http", "prefer_websocket"]},
                    "api_key": {"type": "string", "writeOnly": true}
                }
            }
        },
        "account_configuration": {"public_fields": ["base_url", "transport"]},
        "account_configuration_result": result,
    })
}

#[tokio::test]
async fn declared_connection_settings_are_projected_with_the_accounts_domain() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account_id = environment.account(None).await;
    let expected = json!({
        "account_id": account_id.as_str(),
        "credential_revision": 1,
        "credential": {"key": "test-only"},
    });
    let mut config = configuration(json!({
        "base_url": "https://example.test/v1",
        "transport": "http",
    }));
    config["expected_account_configuration"] = expected;
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();

    let projected = provider
        .account_configuration(&account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Value::Object(projected.into_provider_data().into_inner()),
        json!({"base_url":"https://example.test/v1", "transport":"http"})
    );
    assert_eq!(
        environment
            .store
            .provider_ports()
            .accounts()
            .get_account(&account_id)
            .await
            .unwrap()
            .unwrap()
            .revision()
            .get(),
        1
    );

    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn unsafe_or_undeclared_connection_settings_cannot_replace_the_published_provider() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(
            configuration(json!({"base_url":"https://example.test", "transport":"http"})),
            vec![account_grant("accounts")],
        )
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let published = registry.credential_descriptors().unwrap();
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();

    for (public_fields, property) in [
        (json!(["missing"]), json!({"type":"string"})),
        (json!(["base_url", "base_url"]), json!({"type":"string"})),
        (
            json!(["base_url"]),
            json!({"type":"string", "writeOnly":true}),
        ),
        (
            json!(["base_url"]),
            json!({"type":"string", "format":"password"}),
        ),
        (json!(["base_url"]), json!({"$ref":"#/$defs/value"})),
        (
            json!(["base_url"]),
            json!({"type":"string", "allOf":[{"writeOnly":true}]}),
        ),
        (json!(["base_url"]), json!({"type":"object"})),
    ] {
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration = json!({
            "credential_input_schemas": {
                "rotate": {
                    "type":"object",
                    "properties":{"base_url":property},
                }
            },
            "account_configuration":{"public_fields":public_fields},
        });
        assert!(
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .is_err()
        );
        assert_eq!(registry.credential_descriptors().unwrap(), published);
    }

    for root_constraint in [
        json!({"writeOnly":true}),
        json!({"$ref":"#/$defs/connection"}),
        json!({"allOf":[{"properties":{"base_url":{"writeOnly":true}}}]}),
        json!({"if":{"properties":{"base_url":{"const":"https://private.test"}}}, "then":{"properties":{"base_url":{"format":"password"}}}}),
    ] {
        let mut rotate_schema = json!({
            "type":"object",
            "properties":{"base_url":{"type":"string"}},
        });
        rotate_schema
            .as_object_mut()
            .unwrap()
            .extend(root_constraint.as_object().unwrap().clone());
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration = json!({
            "credential_input_schemas":{"rotate":rotate_schema},
            "account_configuration":{"public_fields":["base_url"]},
        });
        assert!(
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .is_err()
        );
        assert_eq!(registry.credential_descriptors().unwrap(), published);
    }

    let mut candidate = snapshot;
    candidate.instances[0].grants.clear();
    // Provider 是账号驱动扩展；缺少整个 Accounts 域时必须在候选代次准备阶段拒绝。
    assert!(
        PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
            .await
            .is_err()
    );
    assert_eq!(registry.credential_descriptors().unwrap(), published);

    drop(store);
    drop(registry);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn plugin_results_cannot_add_credential_fields_or_leak_rejected_values() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account_id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            configuration(json!({
                "base_url":"https://example.test",
                "transport":"http",
                "api_key":"fixture-sensitive-value",
            })),
            vec![account_grant("accounts")],
        )
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let error = provider
        .account_configuration(&account_id)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ProviderAdminErrorKind::BadGateway);
    assert!(!format!("{error:?}").contains("fixture-sensitive-value"));

    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn credential_revision_changes_discard_a_late_configuration_projection() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account_id = environment.account(None).await;
    let marker = environment
        .directory
        .path()
        .join("configuration-started.jsonl");
    let mut config = configuration(json!({
        "base_url":"https://stale.example.test",
        "transport":"http",
    }));
    let release = environment.directory.path().join("configuration-release");
    config["account_configuration_release_marker"] = json!(release);
    config["account_configuration_started_marker"] = json!(marker.to_string_lossy().into_owned());
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let kind = ProviderKind::new("example").unwrap();
    let registry = runtime.admin_registry(core.snapshots());
    let provider = registry.require(&kind).unwrap();
    let pending_id = account_id.clone();
    let pending = tokio::spawn(async move { provider.account_configuration(&pending_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !marker.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let admin = environment.store.admin_ports();
    let credentials = CredentialsService::new(
        registry,
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    credentials
        .for_provider(&kind)
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id,
                context: mutation(),
            },
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                json!({"api_key":"replacement-test-key"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
            settings: None,
        })
        .await
        .unwrap();
    // 先确认轮换提交，再释放旧 revision 的投影，避免依赖机器负载决定先后。
    assert!(!pending.is_finished());
    std::fs::write(release, b"committed").unwrap();
    assert_eq!(
        pending.await.unwrap().unwrap_err().kind(),
        ProviderAdminErrorKind::Conflict
    );

    drop(credentials);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}
