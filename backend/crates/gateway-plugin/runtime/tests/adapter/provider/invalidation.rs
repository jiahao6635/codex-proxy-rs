use std::{path::Path, time::Duration};

use gateway_admin::{
    CredentialsService,
    model::provider_credentials::{CredentialMutation, ProviderDocument, RotateCredential},
};
use gateway_core::{
    account::{OpaqueProviderData, ProviderAccountId},
    routing::{ProviderKind, UpstreamModelId},
};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn observations(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn notifications_are_not_prebound_deduplicate_and_require_network_for_http() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let other = ProviderAccountId::new("acct_outside_grant").unwrap();
    let marker = environment.directory.path().join("invalidations.jsonl");
    let denied = environment.directory.path().join("denied.jsonl");
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let (runtime, core) = environment
        .provider(
            json!({
                "account_operations":["unavailable", "facts_changed"],
                "invalidation_marker":marker, "invalidation_http_url":server.uri(),
                "invalidation_denied_marker":denied,
            }),
            vec![account_grant("accounts")],
        )
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    provider.account_unavailable(&other).await;
    provider.account_unavailable(&id).await;
    provider
        .account_facts_changed(&[id.clone(), other.clone(), id.clone()])
        .await;
    let mut changed = vec![id.as_str(), other.as_str()];
    changed.sort_unstable();
    assert_eq!(
        observations(&marker),
        vec![
            json!({"kind":"unavailable", "account_id":other.as_str()}),
            json!({"kind":"unavailable", "account_id":id.as_str()}),
            json!({"kind":"facts_changed", "account_ids":changed}),
        ]
    );
    assert_eq!(
        observations(&denied),
        vec![json!(true), json!(true), json!(true)]
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn bulk_notifications_are_bounded_and_undeclared_operations_are_not_called() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let marker = environment.directory.path().join("batches.jsonl");
    let (runtime, core) = environment
        .provider(
            json!({"account_operations":["facts_changed"], "invalidation_marker":marker}),
            vec![account_grant("accounts")],
        )
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let ids: Vec<_> = (0..513)
        .map(|index| ProviderAccountId::new(format!("acct_{index:04}")).unwrap())
        .collect();
    provider.account_unavailable(&ids[0]).await;
    provider.account_facts_changed(&[]).await;
    assert!(observations(&marker).is_empty());
    provider.account_facts_changed(&ids).await;
    let events = observations(&marker);
    assert_eq!(
        events
            .iter()
            .map(|event| event["account_ids"].as_array().unwrap().len())
            .collect::<Vec<_>>(),
        vec![256, 256, 1]
    );
    let notified: Vec<_> = events
        .iter()
        .flat_map(|event| event["account_ids"].as_array().unwrap())
        .map(|id| id.as_str().unwrap())
        .collect();
    assert_eq!(
        notified,
        ids.iter()
            .map(ProviderAccountId::as_str)
            .collect::<Vec<_>>()
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn failed_notification_cannot_undo_a_committed_credential_rotation() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let marker = environment.directory.path().join("failed.jsonl");
    let (runtime, core) = environment
        .provider(
            json!({"account_operations":["facts_changed"], "invalidation_marker":marker, "invalidation_fault":true}),
            vec![account_grant("accounts")],
        )
        .await;
    let admin = environment.store.admin_ports();
    let registry = runtime.admin_registry(core.snapshots());
    let service = CredentialsService::new(
        registry.clone(),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    service
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: id.clone(),
                context: mutation(),
            },
            provider_material: ProviderDocument::new(OpaqueProviderData::new(
                json!({"key":"rotated-test-only"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
            settings: None,
        })
        .await
        .unwrap();
    let current = environment
        .store
        .provider_ports()
        .accounts()
        .load_current_credential(&id)
        .await
        .unwrap();
    assert_eq!(
        current.credential.expose_to_provider()["key"],
        "rotated-test-only"
    );
    assert_eq!(current.account.revision().get(), 2);
    assert_eq!(
        observations(&marker),
        vec![json!({"kind":"facts_changed", "account_ids":[id.as_str()]})]
    );
    drop(service);
    drop(registry);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn notification_timeout_ends_the_whole_batch_and_preserves_the_session() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let marker = environment.directory.path().join("timeout.jsonl");
    let (runtime, core) = environment
        .provider(
            json!({"account_operations":["facts_changed"], "invalidation_marker":marker, "invalidation_delay_ms":30_000}),
            vec![account_grant("accounts")],
        )
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let ids: Vec<_> = (0..513)
        .map(|index| ProviderAccountId::new(format!("acct_{index:04}")).unwrap())
        .collect();
    tokio::time::timeout(Duration::from_secs(4), provider.account_facts_changed(&ids))
        .await
        .unwrap();
    assert_eq!(observations(&marker).len(), 1);
    provider
        .connection_test_operation(&UpstreamModelId::new("plugin-model").unwrap(), "test")
        .await
        .unwrap();
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}
