use gateway_admin::{
    CredentialsService,
    model::provider_credentials::{
        ImportCredentials, PrepareCredentialImport, ProviderDocument, ProviderExportCredentialInput,
    },
};
use gateway_core::{account::OpaqueProviderData, routing::ProviderKind};
use serde_json::json;

use crate::support::environment::{Environment, account_grant, mutation};

#[tokio::test]
async fn rust_plugin_imports_and_exports_through_admin_transactions_and_core_publication() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let kind = ProviderKind::new("example").unwrap();
    let registry = runtime.admin_registry(core.snapshots());
    let admin = environment.store.admin_ports();
    let credentials = CredentialsService::new(
        registry.clone(),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    let document = json!({"accounts":[
        {"name":"first", "authentication_kind":"api_key", "material":{"key":"test-first"}},
        {"name":"second", "authentication_kind":"api_key", "material":{"key":"test-second"}}
    ]});
    let before = core.snapshots().acquire().unwrap();
    let result = credentials
        .for_provider(&kind)
        .unwrap()
        .import_document(ImportCredentials {
            outbound_proxy_id: None,
            settings: None,
            context: mutation(),
            document: provider_document(document.clone()),
        })
        .await
        .unwrap();
    assert_eq!(result.credential_ids.len(), 2);
    let after = core.snapshots().acquire().unwrap();
    assert!(after.revision().get() > before.revision().get());
    let ports = environment.store.provider_ports();
    let mut exported = Vec::new();
    for id in &result.credential_ids {
        let loaded = ports
            .accounts()
            .load_credential(
                id,
                gateway_core::account::CredentialRevision::new(1).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(loaded.account.provider(), &kind);
        assert_eq!(loaded.account.revision().get(), 1);
        let details = admin
            .accounts()
            .credential_details(&kind, id)
            .await
            .unwrap()
            .unwrap();
        exported.push(ProviderExportCredentialInput {
            account: details.credential,
            provider_material: provider_document(json!(loaded.credential.expose_to_provider())),
        });
    }
    let provider = registry.require(&kind).unwrap();
    let export = provider.export_credentials(exported).await.unwrap();
    assert_eq!(export.account_ids, result.credential_ids);
    let exported = export.document.expose_to_provider().expose_to_provider();
    assert_eq!(
        exported["accounts"][0]["material"],
        document["accounts"][0]["material"]
    );
    assert_eq!(
        exported["accounts"][1]["material"],
        document["accounts"][1]["material"]
    );
    // 整个批次准备失败时，前面的合法账号也不能提前进入 Store。
    let invalid = json!({"accounts":[document["accounts"][0], {"name":"", "authentication_kind":"api_key", "material":{"key":"test-invalid"}}]});
    assert!(
        credentials
            .for_provider(&kind)
            .unwrap()
            .import_document(ImportCredentials {
                outbound_proxy_id: None,
                settings: None,
                context: mutation(),
                document: provider_document(invalid),
            })
            .await
            .is_err()
    );
    assert_eq!(
        core.snapshots().acquire().unwrap().revision(),
        after.revision()
    );
    drop(provider);
    drop(credentials);
    drop(registry);
    drop(before);
    drop(after);
    drop(admin);
    drop(ports);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn accounts_domain_allows_plugin_to_prepare_account_creation() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: CPR_PLUGIN_TEST_DATABASE_URL / CPR_PLUGIN_TEST_REDIS_URL 未设置");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let prepared = provider
        .prepare_import(PrepareCredentialImport {
            default_outbound_proxy: None,
            document: provider_document(json!({"accounts":[{
                "name":"prepared account",
                "authentication_kind":"api_key",
                "material":{"key":"prepared-test-only"}
            }]})),
        })
        .await
        .unwrap();
    assert_eq!(prepared.credentials.len(), 1);
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn host_account_create_uses_admin_transaction_and_host_generated_id() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let marker = environment.directory.path().join("account-create.jsonl");
    let (runtime, core) = environment
        .provider(
            json!({"account_create_marker":marker}),
            vec![account_grant("accounts")],
        )
        .await;
    let admin_bundle = environment.bind_admin_accounts(&runtime, &core).await;
    let kind = ProviderKind::new("example").unwrap();
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    let prepared = provider
        .prepare_import(PrepareCredentialImport {
            default_outbound_proxy: None,
            document: provider_document(json!({"accounts":[{
                "name":"callback created account",
                "authentication_kind":"api_key",
                "material":{"key":"created-test-only"}
            }]})),
        })
        .await
        .unwrap();
    assert_eq!(prepared.credentials.len(), 1);

    let saved: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&marker)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let account_id = gateway_core::account::ProviderAccountId::new(
        saved["account_id"].as_str().unwrap().to_owned(),
    )
    .unwrap();
    assert!(account_id.as_str().starts_with("acct_"));
    assert_eq!(saved["credential_revision"], 1);
    assert_ne!(account_id, prepared.credentials[0].account_id);
    let details = environment
        .store
        .admin_ports()
        .accounts()
        .credential_details(&kind, &account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(details.credential.name, "callback created account");
    assert_eq!(details.credential.credential_revision.get(), 1);
    let audit = environment.audit_requests("import_document").await;
    assert_eq!(audit.len(), 1);
    assert!(audit[0].starts_with("plugin:"));
    assert!(audit[0].contains(":call:"));

    drop(provider);
    drop(admin_bundle);
    drop(core);
    drop(runtime);
    environment.close().await;
}

fn provider_document(value: serde_json::Value) -> ProviderDocument {
    ProviderDocument::new(OpaqueProviderData::new(value.as_object().unwrap().clone()))
}

#[tokio::test]
async fn accounts_domain_allows_export_while_missing_network_blocks_http_operations() {
    use gateway_admin::{
        model::provider_credentials::{PrepareCredentialRefresh, ProviderQuotaRequest},
        ports::provider::ProviderAdminErrorKind,
    };
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({"quota_url":server.uri(), "management_url":server.uri()}),
            vec![account_grant("accounts")],
        )
        .await;
    let kind = ProviderKind::new("example").unwrap();
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    let account = environment
        .store
        .admin_ports()
        .accounts()
        .credential_details(&kind, &id)
        .await
        .unwrap()
        .unwrap()
        .credential;
    assert_eq!(
        provider
            .prepare_refresh(PrepareCredentialRefresh {
                account: account.clone()
            })
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Invalid
    );
    assert_eq!(
        provider
            .quota(ProviderQuotaRequest {
                account_id: id.clone(),
                refresh: true,
                rolling_usage: None
            })
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Invalid
    );
    let exported = provider
        .export_credentials(vec![ProviderExportCredentialInput {
            account,
            provider_material: provider_document(json!({"key":"test-only"})),
        }])
        .await
        .unwrap();
    assert_eq!(exported.account_ids, vec![id]);
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}
