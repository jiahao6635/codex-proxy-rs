use super::*;

#[tokio::test]
async fn provider_filters_include_unregistered_accounts_even_on_an_empty_filtered_page() {
    let Some(database) = TestDatabase::create("account_provider_filters").await else {
        return;
    };
    let repository = PgProviderAccountRepository::new(database.pool.clone());
    for (index, provider) in ["openai", "retired-plugin", "retired-plugin", "all"]
        .into_iter()
        .enumerate()
    {
        let mut record = account(
            &format!("acct_filter_{index}"),
            &format!("user-filter-{index}"),
        );
        record.provider_kind = provider.into();
        repository.insert_provider_account(record).await.unwrap();
    }
    let query = AccountListQuery {
        page: 2,
        page_size: PageSize::new(1).unwrap(),
        provider_kind: Some(ProviderKind::new("openai").unwrap()),
        group_filter: None,
        search: None,
        status: None,
        sort: None,
    };
    let store = admin_account_store(&database.pool);
    let page = store
        .list_accounts(query.clone(), Default::default())
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.providers, ["all", "openai", "retired-plugin"]);
    let literal = store
        .list_accounts(
            AccountListQuery {
                page: 1,
                provider_kind: Some(ProviderKind::new("all").unwrap()),
                ..query.clone()
            },
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(literal.items.len(), 1);
    assert_eq!(literal.total, 1);
    sqlx::query("delete from provider_accounts")
        .execute(&database.pool)
        .await
        .unwrap();
    let empty = store
        .list_accounts(query, Default::default())
        .await
        .unwrap();
    assert!(empty.providers.is_empty());
    database.close().await;
}

#[tokio::test]
async fn plugin_account_provider_filter_is_optional_and_applied_before_cursor_pagination() {
    let Some(database) = TestDatabase::create("plugin_account_scope").await else {
        return;
    };
    let repository = PgProviderAccountRepository::new(database.pool.clone());
    for id in ["acct_plugin_a", "acct_plugin_b", "acct_plugin_c"] {
        repository
            .insert_provider_account(account(id, &format!("user-{id}")))
            .await
            .unwrap();
    }
    let mut other_provider = account("acct_plugin_other", "user-other");
    other_provider.provider_kind = "xai".to_owned();
    repository
        .insert_provider_account(other_provider)
        .await
        .unwrap();
    let store = admin_account_store(&database.pool);
    let first = store
        .list_plugin_accounts(PluginAccountListQuery {
            provider_kind: Some(ProviderKind::new("openai").unwrap()),
            cursor: None,
            limit: PageSize::new(1).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        first
            .accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>(),
        ["acct_plugin_a"]
    );
    assert_eq!(
        first.next_cursor.as_ref().map(ProviderAccountId::as_str),
        Some("acct_plugin_a")
    );

    let second = store
        .list_plugin_accounts(PluginAccountListQuery {
            provider_kind: Some(ProviderKind::new("openai").unwrap()),
            cursor: first.next_cursor,
            limit: PageSize::new(1).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        second
            .accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>(),
        ["acct_plugin_b"]
    );
    assert_eq!(
        second.next_cursor.as_ref().map(ProviderAccountId::as_str),
        Some("acct_plugin_b")
    );

    // 未指定 Provider 时按全局账号 ID 跨 Provider 分页，Provider 归属来自持久记录。
    let all = store
        .list_plugin_accounts(PluginAccountListQuery {
            provider_kind: None,
            cursor: None,
            limit: PageSize::new(16).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        all.accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>(),
        [
            "acct_plugin_a",
            "acct_plugin_b",
            "acct_plugin_c",
            "acct_plugin_other"
        ]
    );
    assert!(all.next_cursor.is_none());

    database.close().await;
}

pub(super) fn credential(id: &str, user: &str) -> PreparedCredentialCreate {
    PreparedCredentialCreate {
        account_id: ProviderAccountId::new(id).unwrap(),
        provider_kind: ProviderKind::new("example").unwrap(),
        name: "login account".into(),
        email: None,
        upstream_user_id: Some(user.into()),
        upstream_account_id: None,
        plan_type: None,
        authentication_kind: "oauth".into(),
        provider_material: ProviderDocument::new(OpaqueProviderData::new(
            json!({"access_token":"test-only"})
                .as_object()
                .unwrap()
                .clone(),
        )),
        has_refresh_token: false,
        access_token_expires_at: None,
        next_refresh_at: None,
        enabled: true,
        credential_state: CredentialState::Ready,
        credential_observed_at: Utc::now(),
        outbound_proxy: None,
        model_access: None,
    }
}

pub(super) fn command(
    credentials: Vec<PreparedCredentialCreate>,
) -> (AuthorizationCommit, MutationContext) {
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "authorization-batch".into(),
    };
    (
        AuthorizationCommit {
            key: gateway_admin::model::provider_credentials::AuthorizationReceiptKey::new(
                ProviderKind::new("example").unwrap(),
                &uuid::Uuid::new_v4().to_string(),
                &context,
            )
            .unwrap(),
            settings: None,
            pending: PendingAuthorizationMutation::new(
                ProviderKind::new("example").unwrap(),
                AuthorizationMutationTarget::Create {
                    name: "login".into(),
                },
                AuthorizationOwnerBinding::from_context(&context),
            ),
            credential: AuthorizationCredentialCommit::Create(credentials),
        },
        context,
    )
}

#[tokio::test]
async fn authorization_batch_returns_actual_ids_and_revisions_from_one_audited_transaction() {
    let Some(database) = TestDatabase::create("authorization_batch").await else {
        return;
    };
    let store = admin_account_store(&database.pool);
    let (first, context) = command(vec![credential("acct_existing", "user-one")]);
    store.commit_authorization(first, &context).await.unwrap();
    let (batch, context) = command(vec![
        credential("acct_candidate", "user-one"),
        credential("acct_second", "user-two"),
    ]);
    let result = store
        .commit_authorization(batch, &context)
        .await
        .unwrap()
        .result;
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| (
                account.account_id.as_str(),
                account.credential_revision.unwrap().get()
            ))
            .collect::<Vec<_>>(),
        [("acct_existing", 2), ("acct_second", 1)]
    );
    assert_eq!(
        result.config_revision.get(),
        current_revision(&database.pool).await as u64
    );
    let accounts: i64 = sqlx::query_scalar("select count(*) from provider_accounts")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 2);
    let audit: i64 =
        sqlx::query_scalar("select count(*) from admin_audit_events where action = 'authorize'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(audit, 2, "每批一次审计，不能对每个账号分别提交");
    database.close().await;
}

#[tokio::test]
async fn authorization_batch_conflict_rolls_back_earlier_accounts_revision_and_audit() {
    let Some(database) = TestDatabase::create("authorization_batch_rollback").await else {
        return;
    };
    let store = admin_account_store(&database.pool);
    let (seed, context) = command(vec![credential("acct_existing", "user-one")]);
    store.commit_authorization(seed, &context).await.unwrap();
    let before = current_revision(&database.pool).await;
    let (batch, context) = command(vec![
        credential("acct_new", "user-new"),
        credential("acct_existing", "different-user"),
    ]);
    assert!(store.commit_authorization(batch, &context).await.is_err());
    let accounts: Vec<String> = sqlx::query_scalar("select id from provider_accounts order by id")
        .fetch_all(&database.pool)
        .await
        .unwrap();
    assert_eq!(accounts, ["acct_existing"]);
    assert_eq!(current_revision(&database.pool).await, before);
    let audit: i64 =
        sqlx::query_scalar("select count(*) from admin_audit_events where action = 'authorize'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(audit, 1);
    database.close().await;
}
