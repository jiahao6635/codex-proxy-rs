use std::{
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

use gateway_admin::{
    CredentialsService,
    model::provider_credentials::{
        CredentialMutation, ProviderDocument, ProviderQuotaRequest, RotateCredential,
    },
    ports::provider::ProviderAdminErrorKind,
};
use gateway_core::{
    account::{AccountSelectionPolicy, OpaqueProviderData, QuotaAccessState, RotationStrategy},
    engine::{
        AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
        provider::ProviderRequest,
    },
    error::ProviderErrorKind,
    lifecycle::CancellationToken,
    policy::ClientApiKeyId,
    routing::{ProviderKind, PublicModelId, RoutingContext, UpstreamModelId},
};
use serde_json::json;

use crate::support::environment::{Environment, account_grant, mutation};

#[tokio::test]
async fn quota_persists_and_survives_restart_without_inferring_access_from_percentage() {
    for exhausted in [false, true] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::header(
            "authorization",
            "Bearer test-only",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(quota(exhausted)))
        .expect(1)
        .mount(&server)
        .await;
        let id = environment.account(None).await;
        let (runtime, core) = environment
            .provider(json!({"quota_url":server.uri()}), grants())
            .await;
        let kind = ProviderKind::new("example").unwrap();
        let provider = runtime
            .admin_registry(core.snapshots())
            .require(&kind)
            .unwrap();
        let result = provider
            .quota(ProviderQuotaRequest {
                account_id: id.clone(),
                refresh: true,
                rolling_usage: None,
            })
            .await
            .unwrap();
        assert_eq!(result.plan_type.as_deref(), Some("plugin-pro"));
        assert_eq!(result.windows[0].used_percent, Some(100.0));
        assert_eq!(result.windows[0].window_seconds, Some(604800));
        let loaded = environment
            .store
            .provider_ports()
            .accounts()
            .get_account(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.quota().access(),
            if exhausted {
                QuotaAccessState::Exhausted
            } else {
                QuotaAccessState::Allowed
            }
        );
        let cached = provider
            .quota(ProviderQuotaRequest {
                account_id: id.clone(),
                refresh: false,
                rolling_usage: None,
            })
            .await
            .unwrap();
        assert_eq!(result, cached);
        // 销毁进程和 Core 后仅从持久化实例与额度文档恢复，不再次查询上游。
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
                .quota(ProviderQuotaRequest {
                    account_id: id,
                    refresh: false,
                    rolling_usage: None
                })
                .await
                .unwrap(),
            result
        );
        if exhausted {
            let snapshot = core.snapshots().acquire().unwrap();
            let operation = provider
                .connection_test_operation(&UpstreamModelId::new("plugin-model").unwrap(), "test")
                .await
                .unwrap();
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
            let execution = registry
                .get(&kind)
                .unwrap()
                .clone()
                .execute(
                    ProviderRequest::new(operation, plan.candidates()[0].clone()),
                    AttemptContext::new(
                        RequestAttemptContext::new(
                            ModelRequestId::new("req_quota_test").unwrap(),
                            ClientApiKeyId::new("key_quota_test").unwrap(),
                        ),
                        NonZeroU32::new(1).unwrap(),
                        SystemTime::now() + Duration::from_secs(5),
                        AccountSelectionPolicy::new(
                            RotationStrategy::RoundRobin,
                            NonZeroU32::new(1).unwrap(),
                            Duration::ZERO,
                        ),
                        AccountAttemptContext::default()
                            .with_account_scope(snapshot.all_account_scope()),
                        None,
                        CancellationToken::new(),
                    ),
                )
                .await;
            match execution {
                Err(error) => assert_eq!(error.kind(), ProviderErrorKind::NoEligibleAccount),
                Ok(_) => panic!("exhausted account was selected"),
            }
        }
        drop(provider);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn late_quota_response_cannot_overwrite_a_rotated_credentials_account() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let mut server = crate::support::http::GatedResponse::start(quota(true)).await;
    let id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(json!({"quota_url":server.uri()}), grants())
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let kind = ProviderKind::new("example").unwrap();
    let provider = registry.require(&kind).unwrap();
    let pending_id = id.clone();
    let pending = tokio::spawn(async move {
        provider
            .quota(ProviderQuotaRequest {
                account_id: pending_id,
                refresh: true,
                rolling_usage: None,
            })
            .await
    });
    server.received().await;
    let admin = environment.store.admin_ports();
    let service = CredentialsService::new(
        registry.clone(),
        admin.accounts(),
        admin.proxies(),
        core.snapshot_control(),
    );
    service
        .for_provider(&kind)
        .unwrap()
        .rotate(RotateCredential {
            mutation: CredentialMutation {
                account_id: id.clone(),
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
    assert_eq!(
        pending.await.unwrap().unwrap_err().kind(),
        ProviderAdminErrorKind::Conflict
    );
    let account = environment
        .store
        .provider_ports()
        .accounts()
        .get_account(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.revision().get(), 2);
    assert_eq!(account.quota().access(), QuotaAccessState::Unknown);
    assert!(
        environment
            .store
            .provider_ports()
            .accounts()
            .get_quotas(&[id])
            .await
            .unwrap()
            .is_empty()
    );
    drop(service);
    drop(registry);
    drop(admin);
    drop(core);
    drop(runtime);
    environment.close().await;
}

fn grants() -> Vec<gateway_admin::model::plugins::instances::PluginPermissionGrant> {
    vec![account_grant("accounts"), account_grant("network")]
}

fn quota(exhausted: bool) -> serde_json::Value {
    json!({
        "plan_type":"plugin-pro", "access":if exhausted { json!({"kind":"exhausted","evidence":"usage_limit_reached"}) } else { json!({"kind":"allowed"}) },
        "windows":[{"key":"weekly", "group":"shortTerm", "label":"weekly", "role":"primary", "account_wide":true, "window_seconds":604800, "used_percent":100.0, "limit_reached":true}],
    })
}

#[tokio::test]
async fn quota_requires_network_domain_before_sending() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(quota(false)))
        .expect(0)
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(
            json!({"quota_url":server.uri()}),
            vec![account_grant("accounts")],
        )
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert!(
        provider
            .quota(ProviderQuotaRequest {
                account_id: id.clone(),
                refresh: true,
                rolling_usage: None
            })
            .await
            .is_err()
    );
    assert!(
        environment
            .store
            .provider_ports()
            .accounts()
            .get_quotas(&[id])
            .await
            .unwrap()
            .is_empty()
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}
