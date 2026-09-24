use std::time::Duration;

use async_trait::async_trait;
use gateway_admin::model::{
    AdminError, AdminErrorKind,
    provider_credentials::{
        AuthorizationCommitGuard, AuthorizationMutationTarget, AuthorizationOwnerBinding,
        AuthorizationPollResult, PendingAuthorizationMutation, PollAuthorization,
        PreparedAuthorizationCommit, PreparedAuthorizationCredential, PreparedAuthorizationPoll,
    },
};
use gateway_core::routing::ProviderKind;

use super::super::{
    AdminHarness,
    accounts::{
        EventLog, FakeAccountStore, FakeProviderAdmin, context, events, prepared_create_with_id,
        recorded,
    },
};

struct Claim(EventLog);

struct FailedConsumptionClaim(EventLog);

#[tokio::test]
async fn unavailable_receipts_prevent_replaying_upstream_authorization() {
    let log = events();
    let kind = ProviderKind::new("example").unwrap();
    let services = AdminHarness::new()
        .provider(FakeProviderAdmin::new("example", log.clone()))
        .build()
        .await;
    let error = services
        .credentials()
        .poll_authorization(&kind, poll())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Unavailable);
    assert!(recorded(&log).is_empty());
}

#[async_trait]
impl AuthorizationCommitGuard for FailedConsumptionClaim {
    async fn commit(self: Box<Self>) -> Result<(), AdminError> {
        self.0.lock().unwrap().push("claim.consume_failed");
        Err(AdminError::unavailable("injected pending store failure"))
    }

    async fn abort(self: Box<Self>) -> Result<(), AdminError> {
        panic!("a committed authorization must not be aborted")
    }
}

#[tokio::test]
async fn committed_authorization_survives_cleanup_failure_and_does_not_require_a_provider_on_retry()
{
    let log = events();
    let kind = ProviderKind::new("example").unwrap();
    let provider = FakeProviderAdmin::new("example", log.clone());
    let PreparedAuthorizationPoll::Complete(prepared) = completed(&log, &["acct_receipt"]) else {
        unreachable!()
    };
    provider.queue_authorization_poll(PreparedAuthorizationPoll::Complete(Box::new(
        prepared.with_authorization_guard(Box::new(FailedConsumptionClaim(log.clone()))),
    )));
    let store = FakeAccountStore::new("example", log.clone());
    let services = AdminHarness::new()
        .provider(provider)
        .accounts(store.clone())
        .build()
        .await;
    let original = services
        .credentials()
        .poll_authorization(&kind, poll())
        .await
        .unwrap();
    // 只挂载同一个持久化端口，不再注册 Provider，证明恢复不重放任何插件操作。
    let restarted = AdminHarness::new().accounts(store.clone()).build().await;
    assert_eq!(
        restarted
            .credentials()
            .poll_authorization(&kind, poll())
            .await
            .unwrap(),
        original
    );
    assert_eq!(store.audit_requests().len(), 1);
    assert_eq!(
        recorded(&log)
            .iter()
            .filter(|event| **event == "provider.poll_authorization")
            .count(),
        1
    );
    assert!(recorded(&log).contains(&"claim.consume_failed"));
}

#[async_trait]
impl AuthorizationCommitGuard for Claim {
    async fn commit(self: Box<Self>) -> Result<(), AdminError> {
        self.0.lock().unwrap().push("claim.commit");
        Ok(())
    }

    async fn abort(self: Box<Self>) -> Result<(), AdminError> {
        self.0.lock().unwrap().push("claim.abort");
        Ok(())
    }
}

fn poll() -> PollAuthorization {
    PollAuthorization {
        context: context("poll"),
        flow_id: "flow-test".into(),
        callback_url: None,
        settings: None,
    }
}

fn completed(log: &EventLog, ids: &[&str]) -> PreparedAuthorizationPoll {
    let kind = ProviderKind::new("example").unwrap();
    PreparedAuthorizationPoll::Complete(Box::new(
        PreparedAuthorizationCommit::new(
            PendingAuthorizationMutation::new(
                kind.clone(),
                AuthorizationMutationTarget::Create {
                    name: "login".into(),
                },
                AuthorizationOwnerBinding::from_context(&context("start")),
            ),
            PreparedAuthorizationCredential::Create(
                ids.iter()
                    .map(|id| prepared_create_with_id(kind.clone(), id, "account"))
                    .collect(),
            ),
        )
        .with_authorization_guard(Box::new(Claim(log.clone()))),
    ))
}

#[tokio::test]
async fn pending_poll_never_commits_or_observes_accounts() {
    let log = events();
    let provider = FakeProviderAdmin::new("example", log.clone());
    provider.queue_authorization_poll(PreparedAuthorizationPoll::Pending {
        retry_after: Duration::from_secs(5),
    });
    let store = FakeAccountStore::new("example", log.clone());
    let services = AdminHarness::new()
        .provider(provider)
        .accounts(store.clone())
        .build()
        .await;
    let result = services
        .credentials()
        .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
        .await
        .unwrap();
    assert_eq!(
        result,
        AuthorizationPollResult::Pending {
            retry_after: Duration::from_secs(5)
        }
    );
    assert_eq!(recorded(&log), ["provider.poll_authorization"]);
    assert!(store.audit_requests().is_empty());
}

#[tokio::test]
async fn multi_account_poll_commits_once_then_consumes_claim_and_publishes_all_accounts() {
    let log = events();
    let provider = FakeProviderAdmin::new("example", log.clone());
    provider.queue_authorization_poll(completed(&log, &["acct_one", "acct_two"]));
    let store = FakeAccountStore::new("example", log.clone());
    let services = AdminHarness::new()
        .provider(provider.clone())
        .accounts(store.clone())
        .build()
        .await;
    let result = services
        .credentials()
        .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
        .await
        .unwrap();
    let AuthorizationPollResult::Complete(result) = result else {
        panic!("expected completed login")
    };
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| account.account_id.as_str())
            .collect::<Vec<_>>(),
        ["acct_one", "acct_two"]
    );
    provider.wait_for_quota_requests(2).await;
    assert_eq!(
        &recorded(&log)[..4],
        [
            "provider.poll_authorization",
            "store.commit_authorization",
            "claim.commit",
            "provider.account_facts_changed"
        ]
    );
    assert_eq!(store.audit_requests(), ["poll"]);
    assert_eq!(provider.quota_requests().len(), 2);
}

#[tokio::test]
async fn failed_batch_commit_releases_claim_without_publishing_partial_success() {
    let log = events();
    let provider = FakeProviderAdmin::new("example", log.clone());
    provider.queue_authorization_poll(completed(&log, &["acct_one", "acct_two"]));
    let store = FakeAccountStore::new("example", log.clone());
    store.fail_next_commit();
    let services = AdminHarness::new()
        .provider(provider)
        .accounts(store)
        .build()
        .await;
    assert!(
        services
            .credentials()
            .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
            .await
            .is_err()
    );
    assert_eq!(
        recorded(&log),
        [
            "provider.poll_authorization",
            "store.commit_authorization",
            "claim.abort"
        ]
    );
}

#[tokio::test]
async fn invalid_account_collections_release_claim_before_store() {
    for ids in [vec![], vec!["acct_one", "acct_one"], vec!["acct_one"; 201]] {
        let log = events();
        let provider = FakeProviderAdmin::new("example", log.clone());
        provider.queue_authorization_poll(completed(&log, &ids));
        let store = FakeAccountStore::new("example", log.clone());
        let services = AdminHarness::new()
            .provider(provider)
            .accounts(store)
            .build()
            .await;
        let error = services
            .credentials()
            .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminErrorKind::Conflict);
        assert_eq!(
            recorded(&log),
            ["provider.poll_authorization", "claim.abort"]
        );
    }
}

#[tokio::test]
async fn forged_poll_owner_or_provider_cannot_commit_accounts() {
    for wrong_owner in [true, false] {
        let log = events();
        let provider = FakeProviderAdmin::new("example", log.clone());
        let PreparedAuthorizationPoll::Complete(mut prepared) = completed(&log, &["acct_one"])
        else {
            unreachable!()
        };
        if wrong_owner {
            let mut foreign = context("foreign-start");
            foreign.actor = gateway_admin::model::MutationActor::AdminSession {
                admin_user_id: "different-admin".into(),
            };
            prepared.pending = PendingAuthorizationMutation::new(
                ProviderKind::new("example").unwrap(),
                AuthorizationMutationTarget::Create {
                    name: "login".into(),
                },
                AuthorizationOwnerBinding::from_context(&foreign),
            );
        } else {
            let PreparedAuthorizationCredential::Create(accounts) = &mut prepared.credential else {
                unreachable!()
            };
            accounts[0].provider_kind = ProviderKind::new("another").unwrap();
        }
        provider.queue_authorization_poll(PreparedAuthorizationPoll::Complete(prepared));
        let store = FakeAccountStore::new("example", log.clone());
        let services = AdminHarness::new()
            .provider(provider)
            .accounts(store)
            .build()
            .await;
        let error = services
            .credentials()
            .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminErrorKind::Conflict);
        assert_eq!(
            recorded(&log),
            ["provider.poll_authorization", "claim.abort"]
        );
    }
}

#[tokio::test]
async fn pending_poll_requires_a_bounded_positive_interval() {
    for retry_after in [Duration::ZERO, Duration::from_secs(301)] {
        let log = events();
        let provider = FakeProviderAdmin::new("example", log.clone());
        provider.queue_authorization_poll(PreparedAuthorizationPoll::Pending { retry_after });
        let store = FakeAccountStore::new("example", log.clone());
        let services = AdminHarness::new()
            .provider(provider)
            .accounts(store)
            .build()
            .await;
        let error = services
            .credentials()
            .poll_authorization(&ProviderKind::new("example").unwrap(), poll())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminErrorKind::Internal);
        assert_eq!(recorded(&log), ["provider.poll_authorization"]);
    }
}

#[tokio::test]
async fn single_account_completion_rejects_multiple_accounts_before_commit() {
    let log = events();
    let kind = ProviderKind::new("example").unwrap();
    let provider = FakeProviderAdmin::new("example", log.clone());
    provider.retry_authorization_after_abort();
    provider.set_authorization_accounts(vec![
        prepared_create_with_id(kind.clone(), "acct_one", "one"),
        prepared_create_with_id(kind.clone(), "acct_two", "two"),
    ]);
    let store = FakeAccountStore::new("example", log.clone());
    let services = AdminHarness::new()
        .provider(provider)
        .accounts(store.clone())
        .build()
        .await;
    let credentials = services.credentials().for_provider(&kind).unwrap();
    credentials
        .start_authorization(
            gateway_admin::model::provider_credentials::StartAuthorization {
                input: gateway_admin::model::provider_credentials::ProviderDocument::new(
                    gateway_core::account::OpaqueProviderData::new(serde_json::Map::new()),
                ),
                context: context("start"),
                name: "login".into(),
                reauthorization: None,
                outbound_proxy: None,
            },
        )
        .await
        .unwrap();
    let error = services
        .credentials()
        .complete_authorization(
            &kind,
            gateway_admin::model::provider_credentials::CompleteAuthorization {
                context: context("complete"),
                flow_id: "flow-test".into(),
                callback_url: "https://example.invalid/callback".into(),
                settings: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), AdminErrorKind::Invalid);
    assert_eq!(
        recorded(&log),
        [
            "provider.start_authorization",
            "provider.complete_authorization",
            "authorization_guard.abort"
        ]
    );
    assert!(store.audit_requests().is_empty());
}
