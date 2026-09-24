use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use gateway_admin::{
    CredentialsService,
    model::{
        plugins::instances::PluginPermissionGrant,
        provider_credentials::{
            ConsumeProviderResetCredit, CredentialMutation, ProviderDocument, RotateCredential,
        },
    },
    ports::provider::ProviderAdminErrorKind,
};
use gateway_core::{
    account::{OpaqueProviderData, ProviderAccountId},
    routing::ProviderKind,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, header, method, path},
};

use crate::support::environment::{Environment, account_grant, mutation};

fn credit() -> Value {
    json!({"id":"credit-test", "status":"available", "title":"测试重置卡", "expires_at_ms":1_900_000_000_000_i64, "reset_type":"primary"})
}

fn configuration() -> Value {
    json!({"account_operations":["reset_credits", "consume_reset_credit"],
        "reset_credits":{"outcome":"completed", "result":{"available_count":1, "credits":[credit()]}},
        "consume_reset_credit":{"outcome":"completed", "result":{"code":"reset", "credit":credit()}}})
}

fn grants(with_network: bool) -> Vec<PluginPermissionGrant> {
    let mut grants = vec![account_grant("accounts")];
    if with_network {
        grants.push(account_grant("network"));
    }
    grants
}

fn command(id: &ProviderAccountId) -> ConsumeProviderResetCredit {
    ConsumeProviderResetCredit {
        account_id: id.clone(),
        credit_id: Some("credit-test".into()),
        redeem_request_id: uuid::Uuid::new_v4(),
    }
}

#[tokio::test]
async fn reset_queries_and_consumption_preserve_target_and_idempotency_key_without_local_inventory()
{
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = MockServer::start().await;
    let consumed = Arc::new(Mutex::new(BTreeSet::new()));
    let consumed_upstream = Arc::clone(&consumed);
    Mock::given(method("GET"))
        .and(path("/credits"))
        .and(header("authorization", "Bearer test-only"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"available_count":1, "credits":[credit()]})),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/consume"))
        .and(header("authorization", "Bearer test-only"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["credit_id"], "credit-test");
            let key = body["redeem_request_id"].as_str().unwrap();
            let first = consumed_upstream.lock().unwrap().insert(key.to_owned());
            ResponseTemplate::new(200).set_body_json(
                json!({"code":if first {"reset"} else {"already_redeemed"}, "credit":credit()}),
            )
        })
        .expect(2)
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let mut config = configuration();
    config["reset_credits_url"] = json!(format!("{}/credits", server.uri()));
    config["consume_reset_credit_url"] = json!(format!("{}/consume", server.uri()));
    let (runtime, core) = environment.provider(config, grants(true)).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let capabilities = provider.account_capabilities(&id, "api_key");
    assert!(capabilities.reset_credits && capabilities.consume_reset_credit);
    assert!(!capabilities.quota_refresh);
    for _ in 0..2 {
        let list = provider.reset_credits(&id).await.unwrap();
        assert_eq!(list.available_count, 1);
        assert_eq!(list.credits[0].id, "credit-test");
    }
    let command = command(&id);
    assert_eq!(
        provider
            .consume_reset_credit(command.clone())
            .await
            .unwrap()
            .code,
        "reset"
    );
    assert_eq!(
        provider
            .consume_reset_credit(command.clone())
            .await
            .unwrap()
            .code,
        "already_redeemed"
    );
    assert_eq!(
        *consumed.lock().unwrap(),
        BTreeSet::from([command.redeem_request_id.to_string()])
    );
    let stored = environment
        .store
        .provider_ports()
        .accounts()
        .load_current_credential(&id)
        .await
        .unwrap();
    assert_eq!(stored.account.revision().get(), 1);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.headers.contains_key("cookie"))
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn accounts_domain_allows_reset_consumption_and_declared_operations_stay_authoritative() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let (runtime, core) = environment.provider(configuration(), grants(false)).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let capabilities = provider.account_capabilities(&id, "api_key");
    assert!(capabilities.reset_credits);
    assert!(capabilities.consume_reset_credit);
    assert!(provider.reset_credits(&id).await.is_ok());
    assert!(provider.consume_reset_credit(command(&id)).await.is_ok());
    drop(provider);
    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.configuration["account_operations"] = json!(["subscription"]);
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    let revision = store.load_instances().await.unwrap().config_revision;
    core.snapshot_control()
        .publish_committed(gateway_core::routing::ConfigRevision::new(revision.get()).unwrap())
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        provider.reset_credits(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Unsupported
    );
    assert_eq!(
        provider
            .consume_reset_credit(command(&id))
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Unsupported
    );
    drop(store);
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn confirmed_reset_rejection_and_expired_credential_are_not_uncertain_consumption() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let server = MockServer::start().await;
    let mut config = configuration();
    config["reset_credits_url"] = json!(server.uri());
    config["consume_reset_credit_url"] = json!(server.uri());
    let (runtime, core) = environment.provider(config, grants(true)).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    for (status, expected) in [
        (401, ProviderAdminErrorKind::CredentialRefreshRequired),
        (403, ProviderAdminErrorKind::BadGateway),
        (503, ProviderAdminErrorKind::BadGateway),
    ] {
        server.reset().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(status))
            .expect(2)
            .mount(&server)
            .await;
        assert_eq!(
            provider.reset_credits(&id).await.unwrap_err().kind(),
            expected
        );
        assert_eq!(
            provider
                .consume_reset_credit(command(&id))
                .await
                .unwrap_err()
                .kind(),
            expected
        );
    }
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn malformed_reset_results_never_become_a_confirmed_consumption() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let server = MockServer::start().await;
    let mut config = configuration();
    config["reset_credits_url"] = json!(server.uri());
    config["consume_reset_credit_url"] = json!(server.uri());
    let (runtime, core) = environment.provider(config, grants(true)).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let mut bad_date = credit();
    bad_date["expires_at_ms"] = json!(i64::MAX);
    for credits in [
        vec![credit(), credit()],
        vec![bad_date],
        vec![credit(); 4097],
    ] {
        server.reset().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"available_count":1,"credits":credits})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            provider.reset_credits(&id).await.unwrap_err().kind(),
            ProviderAdminErrorKind::BadGateway
        );
    }
    for result in [
        json!({"code":"","credit":null}),
        json!({"code":"reset","credit":{"id":"other"}}),
        json!({"code":"reset","credit":null,"unexpected":true}),
    ] {
        server.reset().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(result))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            provider
                .consume_reset_credit(command(&id))
                .await
                .unwrap_err()
                .kind(),
            ProviderAdminErrorKind::Ambiguous
        );
    }
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn process_loss_and_forged_not_sent_after_reset_are_uncertain_and_never_retried() {
    for mode in [
        "crash",
        "fault",
        "crash_without_http",
        "malformed_without_http",
    ] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let id = environment.account(None).await;
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code":"reset","credit":null})),
            )
            .expect(if mode.ends_with("without_http") { 0 } else { 1 })
            .mount(&server)
            .await;
        let mut config = configuration();
        if !mode.ends_with("without_http") {
            config["consume_reset_credit_url"] = json!(server.uri());
        }
        match mode {
            "crash" | "crash_without_http" => config["consume_reset_credit_crash"] = json!(true),
            "fault" => {
                config["consume_reset_credit_fault"] =
                    json!({"code":"fault","message":"test-only","send_state":"not_sent"})
            }
            "malformed_without_http" => config["consume_reset_credit"] = json!({"unexpected":true}),
            _ => unreachable!(),
        }
        let (runtime, core) = environment.provider(config, grants(true)).await;
        let provider = runtime
            .admin_registry(core.snapshots())
            .require(&ProviderKind::new("example").unwrap())
            .unwrap();
        assert_eq!(
            provider
                .consume_reset_credit(command(&id))
                .await
                .unwrap_err()
                .kind(),
            ProviderAdminErrorKind::Ambiguous
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            usize::from(!mode.ends_with("without_http"))
        );
        drop(provider);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn reset_results_cannot_cross_credential_rotation_and_consumption_remains_uncertain() {
    for consume in [false, true] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let id = environment.account(None).await;
        let server = MockServer::start().await;
        let result = if consume {
            json!({"code":"reset","credit":null})
        } else {
            json!({"available_count":1,"credits":[credit()]})
        };
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(result)
                    .set_delay(Duration::from_secs(1)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut config = configuration();
        config[if consume {
            "consume_reset_credit_url"
        } else {
            "reset_credits_url"
        }] = json!(server.uri());
        let (runtime, core) = environment.provider(config, grants(true)).await;
        let kind = ProviderKind::new("example").unwrap();
        let registry = runtime.admin_registry(core.snapshots());
        let provider = registry.require(&kind).unwrap();
        let pending_id = id.clone();
        let pending = tokio::spawn(async move {
            if consume {
                provider
                    .consume_reset_credit(command(&pending_id))
                    .await
                    .map(|_| ())
            } else {
                provider.reset_credits(&pending_id).await.map(|_| ())
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let admin = environment.store.admin_ports();
        let service = CredentialsService::new(
            registry,
            admin.accounts(),
            admin.proxies(),
            core.snapshot_control(),
        );
        service
            .for_provider(&kind)
            .unwrap()
            .rotate(RotateCredential {
                mutation: CredentialMutation {
                    account_id: id,
                    context: mutation(),
                },
                provider_material: ProviderDocument::new(OpaqueProviderData::new(
                    json!({"key":"replacement-test-key"})
                        .as_object()
                        .unwrap()
                        .clone(),
                )),
                settings: None,
            })
            .await
            .unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap_err().kind(),
            if consume {
                ProviderAdminErrorKind::Ambiguous
            } else {
                ProviderAdminErrorKind::Conflict
            }
        );
        drop(service);
        drop(admin);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}
