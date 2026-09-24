use gateway_admin::{
    CredentialsService,
    model::{
        AdminErrorKind,
        provider_capabilities::AuthorizationCompletion,
        provider_credentials::{
            AuthorizationPollResult, PollAuthorization, ProviderDocument, StartAuthorization,
        },
    },
    ports::plugins::PluginPreparation,
};
use gateway_core::{account::OpaqueProviderData, routing::ProviderKind};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn start(input: Value) -> StartAuthorization {
    StartAuthorization {
        context: mutation(),
        name: "form login".into(),
        reauthorization: None,
        outbound_proxy: None,
        input: ProviderDocument::new(OpaqueProviderData::new(input.as_object().unwrap().clone())),
    }
}

#[tokio::test]
async fn declared_login_inputs_are_validated_before_rpc_and_reach_the_rust_plugin() {
    let diagnostics = crate::support::diagnostics::Diagnostics::default();
    let _diagnostics = diagnostics.install();
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let schema = json!({
        "type":"object", "additionalProperties":false,
        "properties":{
            "token":{"type":"string", "minLength":1, "writeOnly":true},
            "region":{"type":"string", "enum":["us", "eu"]}
        },
        "required":["token", "region"]
    });
    let valid = json!({"token":"fixture-sensitive-input", "region":"eu"});
    let (runtime, core) = environment
        .provider(
            json!({
                "credential_input_schemas":{"login":schema}, "expected_login_input":valid,
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let admin = environment.store.admin_ports();
    let service = CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    let kind = ProviderKind::new("example").unwrap();
    let descriptors = service.providers().unwrap();
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].provider, kind);
    let login = descriptors[0].capabilities.login.as_ref().unwrap();
    assert_eq!(login.input_schema, schema);
    assert_eq!(login.completion, AuthorizationCompletion::Poll);
    let provider = service.for_provider(&kind).unwrap();
    for input in [
        json!({}),
        json!({"token":"fixture-sensitive-input", "region":"invalid"}),
        json!({"token":"fixture-sensitive-input", "region":"eu", "unexpected":true}),
        json!({"token":"x".repeat(64 * 1024), "region":"eu"}),
    ] {
        let error = provider
            .start_authorization(start(input))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminErrorKind::Invalid);
        assert!(!format!("{error:?}").contains("fixture-sensitive-input"));
    }
    // worker 只接受完整的合法输入；错误输入若进入 RPC，就无法得到上面要求的本地 Invalid 结果。
    let started = provider.start_authorization(start(valid)).await.unwrap();
    let result = service
        .poll_authorization(
            &kind,
            PollAuthorization {
                context: mutation(),
                flow_id: started.flow_id,
                callback_url: Some("ready".into()),
                settings: None,
            },
        )
        .await
        .unwrap();
    let AuthorizationPollResult::Complete(result) = result else {
        panic!("login must complete")
    };
    assert_eq!(result.accounts.len(), 1);
    drop(provider);
    drop(service);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn login_rpc_failure_reports_its_class_without_input_or_plugin_message() {
    let diagnostics = crate::support::diagnostics::Diagnostics::default();
    let _diagnostics = diagnostics.install();
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(
            json!({
                "credential_input_schemas":{"login":{
                    "type":"object", "properties":{"token":{"type":"string", "writeOnly":true}},
                    "required":["token"]
                }},
                "login_fault":{
                    "code":"fault", "message":"fixture-sensitive-rpc-detail", "send_state":"not_sent"
                }
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let admin = environment.store.admin_ports();
    let service = CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    let provider = service
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap();
    let error = provider
        .start_authorization(start(json!({"token":"fixture-sensitive-login-input"})))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Unavailable);
    let records = diagnostics.records();
    let rpc = records
        .iter()
        .find(|fields| {
            fields
                .get("method")
                .is_some_and(|method| method.contains("provider.login.start"))
        })
        .expect("保留原始 RPC 分类");
    assert!(rpc["error"].contains("Remote"));
    assert!(rpc["error"].contains("Fault"));
    assert!(!format!("{records:?}").contains("fixture-sensitive-rpc-detail"));
    assert!(!format!("{records:?}").contains("fixture-sensitive-login-input"));
    drop(provider);
    drop(service);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn invalid_form_schemas_cannot_replace_the_published_capabilities() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let descriptors = registry.credential_descriptors().unwrap();
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    for schemas in [
        json!({"login":{"type":"array"}}),
        json!({"login":{"type":"object", "properties":{"code":{"type":"unknown"}}}}),
        json!({"login":{"type":"object", "$ref":"https://example.test/schema.json"}}),
        json!({"login":{"type":"object", "$ref":"file:///etc/passwd"}}),
        json!({"login":{"type":"object", "description":"x".repeat(64 * 1024)}}),
        json!({"refresh":{"type":"object"}}),
    ] {
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration = json!({"credential_input_schemas":schemas});
        assert!(
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .is_err()
        );
        assert_eq!(registry.credential_descriptors().unwrap(), descriptors);
        assert!(core.snapshots().acquire().is_ok());
    }
    let provider = registry
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        provider
            .credential_capabilities()
            .login
            .unwrap()
            .input_schema,
        json!({"type":"object", "additionalProperties":false})
    );
    drop(provider);
    drop(registry);
    drop(store);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn capability_descriptions_follow_publication_and_frozen_readers_keep_their_generation() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let frozen = registry.freeze().unwrap();
    let before = frozen.credential_descriptors().unwrap();
    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    let schema =
        json!({"type":"object", "properties":{"region":{"enum":["eu"]}}, "required":["region"]});
    instance.configuration = json!({"credential_input_schemas":{"login":schema}});
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    assert_eq!(registry.credential_descriptors().unwrap(), before);
    let revision = store.load_instances().await.unwrap().config_revision;
    core.snapshot_control()
        .publish_committed(gateway_core::routing::ConfigRevision::new(revision.get()).unwrap())
        .await;
    let after = registry.credential_descriptors().unwrap();
    assert_eq!(
        after[0].capabilities.login.as_ref().unwrap().input_schema,
        schema
    );
    assert_eq!(frozen.credential_descriptors().unwrap(), before);
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.enabled = false;
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    let revision = store.load_instances().await.unwrap().config_revision;
    core.snapshot_control()
        .publish_committed(gateway_core::routing::ConfigRevision::new(revision.get()).unwrap())
        .await;
    assert!(registry.credential_descriptors().unwrap().is_empty());
    assert_eq!(frozen.credential_descriptors().unwrap(), before);
    drop(frozen);
    drop(registry);
    drop(store);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn import_and_rotation_validate_forms_before_plugin_calls_and_account_writes() {
    use gateway_admin::model::provider_credentials::{
        CredentialMutation, ImportCredentials, RotateCredential,
    };
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let document = |value: Value| {
        ProviderDocument::new(OpaqueProviderData::new(value.as_object().unwrap().clone()))
    };
    let valid_import = json!({"accounts":[{"name":"form import", "authentication_kind":"api_key", "material":{"key":"import-test-key"}}]});
    let valid_rotation = json!({"key":"rotated-test-key"});
    let (runtime, core) = environment.provider(json!({
        "credential_input_schemas": {
            "import":{"type":"object", "required":["accounts"], "properties":{"accounts":{"type":"array", "minItems":1}}},
            "rotate":{"type":"object", "required":["key"], "properties":{"key":{"type":"string", "minLength":1}}, "additionalProperties":false}
        },
        "expected_import_input":valid_import, "expected_rotation_input":valid_rotation,
    }), vec![account_grant("accounts")]).await;
    let admin = environment.store.admin_ports();
    let service = CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    let provider = service
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap();
    for input in [json!({}), json!({"accounts":[]})] {
        let error = provider
            .import_document(ImportCredentials {
                context: mutation(),
                document: document(input),
                outbound_proxy_id: None,
                settings: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminErrorKind::Invalid);
    }
    let imported = provider
        .import_document(ImportCredentials {
            context: mutation(),
            document: document(valid_import),
            outbound_proxy_id: None,
            settings: None,
        })
        .await
        .unwrap();
    let id = imported.credential_ids[0].clone();
    let before = core.snapshots().acquire().unwrap().revision();
    let error = provider
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: id.clone(),
                context: mutation(),
            },
            provider_material: document(json!({"key":42})),
            settings: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Invalid);
    assert_eq!(core.snapshots().acquire().unwrap().revision(), before);
    let rotated = provider
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: id.clone(),
                context: mutation(),
            },
            provider_material: document(valid_rotation),
            settings: None,
        })
        .await
        .unwrap();
    assert_eq!(rotated.credential_revision.unwrap().get(), 2);
    let stored = environment
        .store
        .provider_ports()
        .accounts()
        .load_current_credential(&id)
        .await
        .unwrap();
    assert_eq!(
        stored.credential.expose_to_provider()["key"],
        "rotated-test-key"
    );
    drop(provider);
    drop(service);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn account_capabilities_cover_provider_accounts_and_follow_published_generations() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let account = environment.account(None).await;
    let other = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({
                "account_operations":["profile", "subscription", "avatar"],
                "quota_url":"http://127.0.0.1:9/quota",
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let kind = ProviderKind::new("example").unwrap();
    let frozen = registry.require(&kind).unwrap();
    let before = frozen.account_capabilities(&account, "api_key");
    assert!(before.quota && before.profile && before.subscription && before.avatar);
    assert!(before.quota_refresh);
    assert!(!before.reset_credits && !before.consume_reset_credit);
    assert_eq!(frozen.account_capabilities(&other, "api_key"), before);

    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.configuration["account_operations"] = json!(["subscription"]);
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    assert_eq!(
        registry
            .require(&kind)
            .unwrap()
            .account_capabilities(&account, "api_key"),
        before
    );
    let revision = store.load_instances().await.unwrap().config_revision;
    core.snapshot_control()
        .publish_committed(gateway_core::routing::ConfigRevision::new(revision.get()).unwrap())
        .await;
    let after = registry
        .require(&kind)
        .unwrap()
        .account_capabilities(&account, "api_key");
    assert!(after.subscription);
    assert!(after.quota);
    assert!(after.quota_refresh);
    assert!(!after.profile && !after.avatar);
    assert_eq!(frozen.account_capabilities(&account, "api_key"), before);
    drop(frozen);
    drop(registry);
    drop(store);
    drop(core);
    drop(runtime);
    environment.close().await;
}
