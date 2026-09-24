use std::{sync::Arc, time::Duration};

use futures::future::BoxFuture;
use gateway_admin::{
    CredentialsService,
    model::{
        MutationActor, MutationContext,
        provider_credentials::{
            AuthorizationPollResult, PollAuthorization, PrepareCredentialRefresh,
            PreparedAuthorizationPoll, StartAuthorization,
        },
    },
    ports::provider::ProviderAdminErrorKind,
};
use gateway_core::{
    account::{ProviderAccountId, ProviderAccountUpdate},
    provider_ports::{
        NewOAuthPendingFlow, OAuthPendingBinding, OAuthPendingClaimOutcome,
        OAuthPendingConsumeOutcome, OAuthPendingFlowPort, OAuthPendingPutOutcome,
        OAuthPendingReleaseOutcome, ProviderStoreError, ProviderStorePorts,
    },
    routing::ProviderKind,
};
use serde_json::json;

use crate::support::environment::{Environment, account_grant, mutation};

fn owner(name: &str) -> MutationContext {
    MutationContext {
        actor: MutationActor::AdminSession {
            admin_user_id: name.into(),
        },
        request_id: uuid::Uuid::new_v4().to_string(),
    }
}

async fn environment() -> Option<Environment> {
    let environment = Environment::create().await?;
    environment.administrator("first-admin").await;
    environment.administrator("second-admin").await;
    Some(environment)
}

fn start(reauthorization: Option<ProviderAccountId>) -> StartAuthorization {
    StartAuthorization {
        input: gateway_admin::model::provider_credentials::ProviderDocument::new(
            gateway_core::account::OpaqueProviderData::new(serde_json::Map::new()),
        ),
        context: owner("first-admin"),
        name: "login account".into(),
        reauthorization,
        outbound_proxy: None,
    }
}

fn poll(flow: &str, ready: bool) -> PollAuthorization {
    PollAuthorization {
        context: owner("first-admin"),
        flow_id: flow.into(),
        callback_url: ready.then(|| "ready".into()),
        settings: None,
    }
}

fn service(
    environment: &Environment,
    runtime: &gateway_plugin_runtime::PluginRuntime,
    core: &gateway_core::CoreBundle,
) -> CredentialsService {
    let ports = environment.store.admin_ports();
    CredentialsService::new(
        runtime.admin_registry(core.snapshots()),
        ports.accounts(),
        ports.proxies(),
        core.snapshot_control(),
    )
}

#[tokio::test]
async fn login_polls_with_owner_isolation_and_commits_all_accounts_once() {
    let Some(environment) = environment().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment.provider(json!({"login_accounts":[
        {"name":"first", "authentication_kind":"api_key", "material":{"key":"first-test-key"}},
        {"name":"second", "authentication_kind":"api_key", "material":{"key":"second-test-key"}}
    ]}), vec![account_grant("accounts")]).await;
    let credentials = service(&environment, &runtime, &core);
    let provider = credentials
        .for_provider(&ProviderKind::new("example").unwrap())
        .unwrap();
    let before = core.snapshots().acquire().unwrap().revision();
    let started = provider.start_authorization(start(None)).await.unwrap();
    let mut intruder = poll(&started.flow_id, true);
    intruder.context = owner("second-admin");
    assert!(
        credentials
            .poll_authorization(&ProviderKind::new("example").unwrap(), intruder)
            .await
            .is_err()
    );
    for _ in 0..2 {
        assert_eq!(
            credentials
                .poll_authorization(
                    &ProviderKind::new("example").unwrap(),
                    poll(&started.flow_id, false)
                )
                .await
                .unwrap(),
            AuthorizationPollResult::Pending {
                retry_after: Duration::from_secs(1)
            }
        );
    }
    assert_eq!(core.snapshots().acquire().unwrap().revision(), before);
    let AuthorizationPollResult::Complete(result) = credentials
        .poll_authorization(
            &ProviderKind::new("example").unwrap(),
            poll(&started.flow_id, true),
        )
        .await
        .unwrap()
    else {
        panic!("expected completed login");
    };
    assert_eq!(result.accounts.len(), 2);
    for (index, account) in result.accounts.iter().enumerate() {
        assert_eq!(account.credential_revision.unwrap().get(), 1);
        let loaded = environment
            .store
            .provider_ports()
            .accounts()
            .load_current_credential(&account.account_id)
            .await
            .unwrap();
        assert_eq!(
            loaded.credential.expose_to_provider()["key"],
            if index == 0 {
                "first-test-key"
            } else {
                "second-test-key"
            }
        );
    }
    assert!(core.snapshots().acquire().unwrap().revision().get() > before.get());
    assert_eq!(
        credentials
            .poll_authorization(
                &ProviderKind::new("example").unwrap(),
                poll(&started.flow_id, true)
            )
            .await
            .unwrap(),
        AuthorizationPollResult::Complete(result)
    );
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn encrypted_login_survives_restart_but_rejects_changed_plugin_configuration() {
    let Some(environment) = environment().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let credentials = service(&environment, &runtime, &core);
    let kind = ProviderKind::new("example").unwrap();
    let provider = credentials.for_provider(&kind).unwrap();
    let survives = provider.start_authorization(start(None)).await.unwrap();
    let invalidated = provider.start_authorization(start(None)).await.unwrap();
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    let (runtime, core) = environment.runtime().await;
    let credentials = service(&environment, &runtime, &core);
    let provider = credentials.for_provider(&kind).unwrap();
    assert!(matches!(
        credentials
            .poll_authorization(
                &ProviderKind::new("example").unwrap(),
                poll(&survives.flow_id, true)
            )
            .await
            .unwrap(),
        AuthorizationPollResult::Complete(_)
    ));
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.configuration = json!({"new_configuration":true});
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    let (runtime, core) = environment.runtime().await;
    let credentials = service(&environment, &runtime, &core);
    let provider = credentials.for_provider(&kind).unwrap();
    assert!(
        credentials
            .poll_authorization(
                &ProviderKind::new("example").unwrap(),
                poll(&invalidated.flow_id, true)
            )
            .await
            .is_err()
    );
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}

#[tokio::test]
async fn claimed_login_blocks_other_pollers_and_reauthorization_preserves_latest_profile() {
    let Some(environment) = environment().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let id = environment.account(None).await;
    let credentials = service(&environment, &runtime, &core);
    let kind = ProviderKind::new("example").unwrap();
    let provider = credentials.for_provider(&kind).unwrap();
    let started = provider
        .start_authorization(start(Some(id.clone())))
        .await
        .unwrap();
    let registry = runtime.admin_registry(core.snapshots());
    let adapter = registry.require(&kind).unwrap();
    let prepared = adapter
        .poll_authorization(poll(&started.flow_id, true))
        .await
        .unwrap();
    assert!(matches!(prepared, PreparedAuthorizationPoll::Complete(_)));
    let error = adapter
        .poll_authorization(poll(&started.flow_id, true))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ProviderAdminErrorKind::Conflict);
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
        adapter
            .prepare_refresh(PrepareCredentialRefresh { account })
            .await
            .unwrap_err()
            .kind(),
        ProviderAdminErrorKind::Conflict
    );
    environment
        .store
        .provider_ports()
        .accounts()
        .update_account(ProviderAccountUpdate {
            account_id: id.clone(),
            name: "administrator edited during login".into(),
            email: None,
            plan_type: None,
        })
        .await
        .unwrap();
    // 丢弃尚未提交的结果同时释放登录 claim 与账号 lease，下一次轮询可接续。
    drop(prepared);
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match adapter
                .poll_authorization(poll(&started.flow_id, false))
                .await
            {
                Ok(result) => break result,
                Err(error) if error.kind() == ProviderAdminErrorKind::Conflict => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("poll after release failed: {error:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(result, PreparedAuthorizationPoll::Pending { .. }));
    let AuthorizationPollResult::Complete(result) = credentials
        .poll_authorization(
            &ProviderKind::new("example").unwrap(),
            poll(&started.flow_id, true),
        )
        .await
        .unwrap()
    else {
        panic!("expected complete");
    };
    assert_eq!(result.accounts[0].account_id, id);
    assert_eq!(result.accounts[0].credential_revision.unwrap().get(), 2);
    let loaded = environment
        .store
        .provider_ports()
        .accounts()
        .load_current_credential(&id)
        .await
        .unwrap();
    assert_eq!(loaded.account.name(), "administrator edited during login");
    assert_eq!(
        loaded.credential.expose_to_provider()["key"],
        "login-test-only"
    );
    drop(adapter);
    drop(registry);
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn invalid_login_results_cannot_commit_accounts() {
    for configuration in [
        json!({"login_accounts":[]}),
        json!({"login_retry_ms":0}),
        json!({"login_ttl_ms":-1}),
    ] {
        let Some(environment) = environment().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let (runtime, core) = environment
            .provider(configuration.clone(), vec![account_grant("accounts")])
            .await;
        let credentials = service(&environment, &runtime, &core);
        let provider = credentials
            .for_provider(&ProviderKind::new("example").unwrap())
            .unwrap();
        let revision = core.snapshots().acquire().unwrap().revision();
        match provider.start_authorization(start(None)).await {
            Ok(started) => {
                for _ in 0..2 {
                    assert!(
                        credentials
                            .poll_authorization(
                                &ProviderKind::new("example").unwrap(),
                                poll(
                                    &started.flow_id,
                                    configuration.get("login_accounts").is_some()
                                )
                            )
                            .await
                            .is_err()
                    );
                }
            }
            Err(_) => assert!(configuration.get("login_ttl_ms").is_some()),
        }
        assert_eq!(core.snapshots().acquire().unwrap().revision(), revision);
        drop(provider);
        drop(credentials);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn expired_login_cannot_be_reclaimed_or_commit_credentials() {
    let Some(environment) = environment().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(
            json!({"login_ttl_ms":1000}),
            vec![account_grant("accounts")],
        )
        .await;
    let credentials = service(&environment, &runtime, &core);
    let kind = ProviderKind::new("example").unwrap();
    let provider = credentials.for_provider(&kind).unwrap();
    let before = core.snapshots().acquire().unwrap().revision();
    let started = provider.start_authorization(start(None)).await.unwrap();
    let remaining = (started.expires_at - chrono::Utc::now())
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(remaining + Duration::from_millis(30)).await;
    let registry = runtime.admin_registry(core.snapshots());
    let adapter = registry.require(&kind).unwrap();
    assert_eq!(
        adapter
            .poll_authorization(poll(&started.flow_id, true))
            .await
            .err()
            .unwrap()
            .kind(),
        ProviderAdminErrorKind::NotFound
    );
    assert_eq!(core.snapshots().acquire().unwrap().revision(), before);
    drop(adapter);
    drop(registry);
    drop(provider);
    drop(credentials);
    drop(core);
    drop(runtime);
    environment.close().await;
}

struct PausedConsumption {
    inner: Arc<dyn OAuthPendingFlowPort>,
    reached: tokio::sync::Notify,
}

impl OAuthPendingFlowPort for PausedConsumption {
    fn put_if_absent(
        &self,
        flow: NewOAuthPendingFlow,
    ) -> BoxFuture<'_, Result<OAuthPendingPutOutcome, ProviderStoreError>> {
        self.inner.put_if_absent(flow)
    }
    fn claim_if_owner<'a>(
        &'a self,
        provider: &'a ProviderKind,
        flow: &'a OAuthPendingBinding,
        owner: &'a OAuthPendingBinding,
        claim: &'a OAuthPendingBinding,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<OAuthPendingClaimOutcome, ProviderStoreError>> {
        self.inner.claim_if_owner(provider, flow, owner, claim, ttl)
    }
    fn release_claim<'a>(
        &'a self,
        provider: &'a ProviderKind,
        flow: &'a OAuthPendingBinding,
        owner: &'a OAuthPendingBinding,
        claim: &'a OAuthPendingBinding,
    ) -> BoxFuture<'a, Result<OAuthPendingReleaseOutcome, ProviderStoreError>> {
        self.inner.release_claim(provider, flow, owner, claim)
    }
    fn consume_claim<'a>(
        &'a self,
        _: &'a ProviderKind,
        _: &'a OAuthPendingBinding,
        _: &'a OAuthPendingBinding,
        _: &'a OAuthPendingBinding,
    ) -> BoxFuture<'a, Result<OAuthPendingConsumeOutcome, ProviderStoreError>> {
        Box::pin(async move {
            self.reached.notify_one();
            // 精确停在 PostgreSQL 提交之后；中断此 future 模拟宿主丢失后续结算。
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn committed_login_recovers_after_interruption_before_redis_consumption_and_plugin_shutdown()
{
    let Some(environment) = environment().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(json!({}), vec![account_grant("accounts")])
        .await;
    let credentials = service(&environment, &runtime, &core);
    let kind = ProviderKind::new("example").unwrap();
    let started = credentials
        .for_provider(&kind)
        .unwrap()
        .start_authorization(start(None))
        .await
        .unwrap();
    drop(credentials);
    drop(core);
    drop(runtime);
    let ports = environment.store.provider_ports();
    let pending = Arc::new(PausedConsumption {
        inner: ports.oauth_pending(),
        reached: tokio::sync::Notify::new(),
    });
    let injected = ProviderStorePorts::new(
        ports.accounts(),
        ports.leases(),
        ports.session_affinity(),
        ports.session_exclusions(),
        ports.catalog_cache(),
        ports.artifact_profiles(),
        ports.credential_state(),
        ports.cooldowns(),
        ports.runtime_policy(),
        pending.clone(),
    );
    let (runtime, core) = environment.runtime_with_ports(injected).await;
    let credentials = Arc::new(service(&environment, &runtime, &core));
    let request = poll(&started.flow_id, true);
    let operation = {
        let credentials = credentials.clone();
        let kind = kind.clone();
        tokio::spawn(async move { credentials.poll_authorization(&kind, request).await })
    };
    tokio::time::timeout(Duration::from_secs(10), pending.reached.notified())
        .await
        .unwrap();
    let key = gateway_admin::model::provider_credentials::AuthorizationReceiptKey::new(
        kind.clone(),
        &started.flow_id,
        &owner("first-admin"),
    )
    .unwrap();
    let receipt = environment
        .store
        .admin_ports()
        .accounts()
        .authorization_receipt(&key)
        .await
        .unwrap()
        .unwrap();
    assert!(core.snapshots().acquire().unwrap().revision().get() < receipt.config_revision.get());
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    let claim = pending
        .inner
        .claim_if_owner(
            &kind,
            &OAuthPendingBinding::try_new(&started.flow_id).unwrap(),
            &OAuthPendingBinding::try_new("admin_session:first-admin").unwrap(),
            &OAuthPendingBinding::try_new("another-claim").unwrap(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(claim, OAuthPendingClaimOutcome::InProgress);
    assert_eq!(
        credentials
            .poll_authorization(&kind, poll(&started.flow_id, true))
            .await
            .unwrap(),
        AuthorizationPollResult::Complete(receipt.clone())
    );
    assert_eq!(
        core.snapshots().acquire().unwrap().revision().get(),
        receipt.config_revision.get()
    );
    drop(credentials);
    drop(core);
    drop(runtime);
    drop(pending);
    drop(ports);
    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.enabled = false;
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    let (runtime, core) = environment.runtime().await;
    let credentials = service(&environment, &runtime, &core);
    assert!(credentials.for_provider(&kind).is_err());
    assert_eq!(
        credentials
            .poll_authorization(&kind, poll(&started.flow_id, true))
            .await
            .unwrap(),
        AuthorizationPollResult::Complete(receipt.clone())
    );
    let account = environment
        .store
        .provider_ports()
        .accounts()
        .load_current_credential(&receipt.accounts[0].account_id)
        .await
        .unwrap();
    assert_eq!(account.account.revision().get(), 1);
    drop(credentials);
    drop(core);
    drop(runtime);
    drop(store);
    environment.close().await;
}
