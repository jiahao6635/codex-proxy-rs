use gateway_admin::{
    CredentialsService,
    model::provider_credentials::{
        CredentialMutation, CredentialRotationCommit, ImportCredentials, PrepareCredentialRefresh,
        ProviderDocument, RotateCredential,
    },
    ports::provider::ProviderAdminErrorKind,
};
use gateway_core::{
    account::{OpaqueProviderData, ProviderAccountUpdate},
    routing::ProviderKind,
};
use serde_json::json;

use crate::support::environment::{Environment, account_grant, mutation};

#[tokio::test]
async fn refresh_holds_real_redis_leases_through_commit_and_preserves_concurrent_profile_edits() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let admin = environment.store.admin_ports();
    let ports = environment.store.provider_ports();
    let service = CredentialsService::new(
        registry.clone(),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    let kind = ProviderKind::new("example").unwrap();
    let id = service
        .for_provider(&kind)
        .unwrap()
        .import_document(import())
        .await
        .unwrap()
        .credential_ids
        .remove(0);
    let provider = registry.require(&kind).unwrap();
    let account = admin
        .accounts()
        .credential_details(&kind, &id)
        .await
        .unwrap()
        .unwrap()
        .credential;
    let prepared = provider
        .prepare_refresh(PrepareCredentialRefresh {
            account: account.clone(),
        })
        .await
        .unwrap();
    let busy = provider
        .prepare_refresh(PrepareCredentialRefresh {
            account: account.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(busy.kind(), ProviderAdminErrorKind::Conflict);
    ports
        .accounts()
        .update_account(ProviderAccountUpdate {
            account_id: id.clone(),
            name: "concurrent admin edit".into(),
            email: None,
            plan_type: None,
        })
        .await
        .unwrap();
    let (facts, guard) = prepared.into_parts();
    let result = admin
        .accounts()
        .commit_credential_refresh(
            CredentialRotationCommit {
                prepared: facts,
                settings: None,
            },
            &mutation(),
        )
        .await
        .unwrap();
    guard.finish();
    core.snapshot_control()
        .publish_committed(
            gateway_core::routing::ConfigRevision::new(result.config_revision.get()).unwrap(),
        )
        .await;
    let loaded = ports.accounts().load_current_credential(&id).await.unwrap();
    assert_eq!(loaded.account.revision().get(), 2);
    assert_eq!(loaded.account.name(), "concurrent admin edit");
    assert_eq!(
        loaded.credential.expose_to_provider()["key"],
        "refreshed-test-key"
    );
    assert_eq!(
        provider
            .prepare_refresh(PrepareCredentialRefresh { account })
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Conflict
    );
    let rotated = service
        .for_provider(&kind)
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: id.clone(),
                context: mutation(),
            },
            provider_material: document(json!({"key":"manually-rotated"})),
            settings: None,
        })
        .await
        .unwrap();
    assert_eq!(rotated.credential_revision.unwrap().get(), 3);
    let loaded = ports.accounts().load_current_credential(&id).await.unwrap();
    assert_eq!(
        loaded.credential.expose_to_provider()["key"],
        "manually-rotated"
    );
    let account = admin
        .accounts()
        .credential_details(&kind, &id)
        .await
        .unwrap()
        .unwrap()
        .credential;
    // 失败事务或取消会 drop 准备结果，不能留下账号互斥锁。
    drop(
        provider
            .prepare_refresh(PrepareCredentialRefresh {
                account: account.clone(),
            })
            .await
            .unwrap(),
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match provider
                .prepare_refresh(PrepareCredentialRefresh {
                    account: account.clone(),
                })
                .await
            {
                Ok(prepared) => {
                    drop(prepared);
                    break;
                }
                Err(error) if error.kind() == ProviderAdminErrorKind::Conflict => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                Err(error) => panic!("refresh after guard release failed: {error:?}"),
            }
        }
    })
    .await
    .unwrap();
    drop(provider);
    drop(service);
    drop(registry);
    drop(ports);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn refresh_uses_managed_http_and_never_reports_sent_failures_as_safe_to_retry() {
    for ambiguous in [false, true] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer original-test-key"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"name":"upstream profile", "authentication_kind":"oauth", "material":{"key":"upstream-rotated-key"}, "has_refresh_token":true})))
            .expect(1).mount(&server).await;
        let network = account_grant("network");
        let (runtime, core) = environment
            .provider(
                json!({"management_url":server.uri(), "management_error_after_http":ambiguous}),
                vec![account_grant("accounts"), network],
            )
            .await;
        let registry = runtime.admin_registry(core.snapshots());
        let admin = environment.store.admin_ports();
        let service = CredentialsService::new(
            registry.clone(),
            admin.accounts(),
            admin.proxies(),
            core.snapshot_control(),
        );
        let kind = ProviderKind::new("example").unwrap();
        let id = service
            .for_provider(&kind)
            .unwrap()
            .import_document(import())
            .await
            .unwrap()
            .credential_ids
            .remove(0);
        let provider = registry.require(&kind).unwrap();
        let account = admin
            .accounts()
            .credential_details(&kind, &id)
            .await
            .unwrap()
            .unwrap()
            .credential;
        let prepared = provider
            .prepare_refresh(PrepareCredentialRefresh { account })
            .await;
        if ambiguous {
            assert_eq!(
                prepared.unwrap_err().kind(),
                ProviderAdminErrorKind::Ambiguous
            );
        } else {
            let prepared = prepared.unwrap();
            assert_eq!(
                prepared
                    .facts()
                    .provider_material
                    .expose_to_provider()
                    .expose_to_provider()["key"],
                "upstream-rotated-key"
            );
            let (facts, guard) = prepared.into_parts();
            admin
                .accounts()
                .commit_credential_refresh(
                    CredentialRotationCommit {
                        prepared: facts,
                        settings: None,
                    },
                    &mutation(),
                )
                .await
                .unwrap();
            guard.finish();
        }
        let account = environment
            .store
            .provider_ports()
            .accounts()
            .load_current_credential(&id)
            .await
            .unwrap();
        assert_eq!(
            account.account.revision().get(),
            if ambiguous { 1 } else { 2 }
        );
        drop(provider);
        drop(service);
        drop(registry);
        drop(admin);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

fn import() -> ImportCredentials {
    ImportCredentials {
        outbound_proxy_id: None,
        settings: None,
        context: mutation(),
        document: document(
            json!({"accounts":[{"name":"initial profile", "authentication_kind":"oauth", "material":{"key":"original-test-key"}, "has_refresh_token":true}]}),
        ),
    }
}

fn document(value: serde_json::Value) -> ProviderDocument {
    ProviderDocument::new(OpaqueProviderData::new(value.as_object().unwrap().clone()))
}
