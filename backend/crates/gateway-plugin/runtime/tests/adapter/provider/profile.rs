use futures::TryStreamExt as _;
use gateway_admin::{
    CredentialsService,
    model::{
        plugins::instances::PluginPermissionGrant,
        provider_credentials::{CredentialMutation, ProviderDocument, RotateCredential},
    },
    ports::{
        plugins::{PluginPackageInspector as _, PluginPreparation},
        provider::ProviderAdminErrorKind,
    },
};
use gateway_core::{account::OpaqueProviderData, routing::ProviderKind};
use gateway_plugin_sdk::{Capability, Contributions, Permission, Stage};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn profile() -> Value {
    json!({
        "display_name":"插件用户", "username":"plugin-user", "image_url":"https://example.test/avatar.png",
        "has_stats_error":false, "summary":{"total_text_tokens":91234, "current_streak_days":7},
        "daily_usage":[{"date":"2026-09-19", "tokens":1234}],
        "activity_insights":{"fast_mode_percent":25.0, "reasoning_effort":"high", "reasoning_effort_percent":75.0,
            "invocations":[{"invocation_type":"skill", "skill_id":"test-skill", "skill_name":"测试技能", "usage_count":8}]},
    })
}

fn subscription() -> Value {
    json!({
        "starts_at_ms":1_780_000_000_000_i64, "expires_at_ms":1_790_000_000_000_i64,
        "will_renew":true, "billing_period":"monthly", "billing_currency":"USD", "observed_at_ms":1_785_000_000_000_i64,
    })
}

fn configuration() -> Value {
    json!({"account_operations":["profile", "subscription", "avatar"], "profile":profile(), "subscription":subscription()})
}

fn grants() -> Vec<PluginPermissionGrant> {
    vec![account_grant("accounts"), account_grant("network")]
}

#[tokio::test]
async fn profile_http_uses_the_accounts_proxy_and_its_credentials() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::header(
        "proxy-authorization",
        "Basic dXNlcjpwYXNz",
    ))
    .and(wiremock::matchers::header(
        "authorization",
        "Bearer test-only",
    ))
    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(profile()))
    .expect(1)
    .mount(&proxy)
    .await;
    let id = environment
        .account(Some(
            gateway_core::account::OutboundProxy::parse(&format!(
                "http://user:pass@{}",
                proxy.address()
            ))
            .unwrap(),
        ))
        .await;
    let mut config = configuration();
    config["profile_url"] = json!("http://127.0.0.1:9/profile");
    let (runtime, core) = environment.provider(config, grants()).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        provider
            .profile_statistics(&id)
            .await
            .unwrap()
            .display_name
            .as_deref(),
        Some("插件用户")
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn avatar_rejects_malformed_headers_before_exposing_a_body() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let mut config = configuration();
    config["avatar_metadata"] =
        json!({"content_type":"image/png\r\nx-private: secret", "content_length":524288});
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert_eq!(
        provider.profile_avatar(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::BadGateway
    );
    assert!(provider.profile_statistics(&id).await.is_ok());
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn profile_subscription_and_avatar_use_managed_http_without_mutating_account_or_quota() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    for (path, body) in [("/profile", profile()), ("/subscription", subscription())] {
        wiremock::Mock::given(wiremock::matchers::path(path))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer test-only",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }
    let avatar_bytes = vec![42; 524288];
    wiremock::Mock::given(wiremock::matchers::path("/avatar"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer test-only",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(avatar_bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let id = environment.account(None).await;
    let ports = environment.store.provider_ports();
    let before = ports.accounts().get_account(&id).await.unwrap().unwrap();
    let mut config = configuration();
    for operation in ["profile", "subscription", "avatar"] {
        config[format!("{operation}_url")] = json!(format!("{}/{operation}", server.uri()));
    }
    // Accounts 域授权后，三个只读操作都应完成且不得改写账号事实。
    let (runtime, core) = environment.provider(config, grants()).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let (profile, subscription) =
        tokio::join!(provider.profile_statistics(&id), provider.subscription(&id));
    let profile = profile.unwrap();
    assert_eq!(profile.display_name.as_deref(), Some("插件用户"));
    assert_eq!(profile.summary.total_text_tokens, Some(91234));
    assert_eq!(profile.daily_usage.unwrap()[0].tokens, 1234);
    assert_eq!(profile.activity_insights.fast_mode_percent, Some(25.0));
    assert_eq!(
        profile.activity_insights.invocations.unwrap()[0].usage_count,
        Some(8)
    );
    assert_eq!(
        subscription.unwrap().unwrap().expires_at.timestamp_millis(),
        1_790_000_000_000
    );
    let avatar = provider.profile_avatar(&id).await.unwrap();
    assert_eq!(avatar.content_length, Some(avatar_bytes.len() as u64));
    let chunks: Vec<_> = avatar.body.try_collect().await.unwrap();
    assert_eq!(chunks.concat(), avatar_bytes);
    let after = ports.accounts().get_account(&id).await.unwrap().unwrap();
    assert_eq!(after.revision(), before.revision());
    assert_eq!(after.quota(), before.quota());
    assert_eq!(after.name(), before.name());
    assert_eq!(after.upstream_account_id(), before.upstream_account_id());
    drop(provider);
    drop(core);
    drop(runtime);
    drop(ports);
    environment.close().await;
}

#[tokio::test]
async fn profile_requires_declared_operation_and_network_domain_before_rpc() {
    for restriction in ["undeclared", "network"] {
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
        let mut config = configuration();
        config["profile_url"] = json!(server.uri());
        let mut grants = grants();
        match restriction {
            "undeclared" => config["account_operations"] = json!(["subscription"]),
            "network" => grants.retain(|grant| grant.permission != "network"),
            _ => unreachable!(),
        }
        let (runtime, core) = environment.provider(config, grants).await;
        let provider = runtime
            .admin_registry(core.snapshots())
            .require(&ProviderKind::new("example").unwrap())
            .unwrap();
        let error = provider.profile_statistics(&id).await.unwrap_err();
        assert_eq!(
            error.kind(),
            if restriction == "undeclared" {
                ProviderAdminErrorKind::Unsupported
            } else {
                ProviderAdminErrorKind::Invalid
            }
        );
        assert!(!format!("{error:?}").contains("test-only"));
        drop(provider);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn malformed_profile_and_subscription_results_do_not_publish_untrusted_facts() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let server = wiremock::MockServer::start().await;
    let id = environment.account(None).await;
    let mut config = configuration();
    config["profile_url"] = json!(server.uri());
    config["subscription_url"] = json!(server.uri());
    let (runtime, core) = environment.provider(config, grants()).await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    let mut bad_profiles = Vec::new();
    for (field, value) in [
        ("display_name", json!("x".repeat(1025))),
        ("username", json!("injected\nline")),
        ("image_url", json!("https://token@example.test/a")),
    ] {
        let mut value_profile = profile();
        value_profile[field] = value;
        bad_profiles.push(value_profile);
    }
    let mut bad = profile();
    bad["activity_insights"]["fast_mode_percent"] = json!(100.1);
    bad_profiles.push(bad);
    for usage in [
        json!([{"date":"2026-02-30", "tokens":1}]),
        json!([{"date":"2026-09-19", "tokens":1}, {"date":"2026-09-19", "tokens":2}]),
        json!(vec![json!({"date":"2026-09-19", "tokens":1}); 4097]),
    ] {
        let mut bad = profile();
        bad["daily_usage"] = usage;
        bad_profiles.push(bad);
    }
    for value in bad_profiles {
        server.reset().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(value))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            provider.profile_statistics(&id).await.unwrap_err().kind(),
            ProviderAdminErrorKind::BadGateway
        );
    }
    for (field, value) in [
        ("expires_at_ms", json!(i64::MAX)),
        ("starts_at_ms", json!(1_800_000_000_000_i64)),
        ("billing_currency", json!("USD\r\nprivate")),
    ] {
        let mut bad = subscription();
        bad[field] = value;
        server.reset().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(bad))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            provider.subscription(&id).await.unwrap_err().kind(),
            ProviderAdminErrorKind::BadGateway
        );
    }
    server.reset().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    assert!(provider.subscription(&id).await.unwrap().is_none());
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn credential_rotation_discards_late_profile_subscription_and_avatar_results() {
    for operation in ["profile", "subscription", "avatar"] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let body = if operation == "subscription" {
            subscription()
        } else {
            profile()
        };
        let mut server = crate::support::http::GatedResponse::start(body).await;
        let id = environment.account(None).await;
        let mut config = configuration();
        config[format!("{operation}_url")] = json!(server.uri());
        let (runtime, core) = environment.provider(config, grants()).await;
        let kind = ProviderKind::new("example").unwrap();
        let registry = runtime.admin_registry(core.snapshots());
        let provider = registry.require(&kind).unwrap();
        let pending_id = id.clone();
        let pending = tokio::spawn(async move {
            match operation {
                "profile" => provider.profile_statistics(&pending_id).await.map(|_| ()),
                "subscription" => provider.subscription(&pending_id).await.map(|_| ()),
                "avatar" => provider.profile_avatar(&pending_id).await.map(|_| ()),
                _ => unreachable!(),
            }
        });
        server.received().await;
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
        server.respond().await;
        assert_eq!(
            pending.await.unwrap().unwrap_err().kind(),
            ProviderAdminErrorKind::Conflict
        );
        drop(service);
        drop(admin);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn avatar_preserves_stream_errors_and_checks_declared_length() {
    for metadata in [
        json!({"content_type":"image/png", "content_length":524289}),
        json!({"content_type":"image/png", "content_length":1}),
        Value::Null,
    ] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let id = environment.account(None).await;
        let mut config = configuration();
        if metadata.is_null() {
            config["avatar_end_error"] = json!(true);
        } else {
            config["avatar_metadata"] = metadata;
        }
        let (runtime, core) = environment
            .provider(config, vec![account_grant("accounts")])
            .await;
        let provider = runtime
            .admin_registry(core.snapshots())
            .require(&ProviderKind::new("example").unwrap())
            .unwrap();
        let avatar = provider.profile_avatar(&id).await.unwrap();
        assert!(avatar.body.try_collect::<Vec<_>>().await.is_err());
        // 头像失败是该次调用的终态，不能破坏同进程后续资料查询。
        assert!(provider.profile_statistics(&id).await.is_ok());
        drop(provider);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}

#[tokio::test]
async fn dropping_a_backpressured_avatar_cancels_only_that_call() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(configuration(), vec![account_grant("accounts")])
        .await;
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    // 总量超过 RPC 信用窗口；未消费的流必须能释放槽位且不终止整个进程。
    for _ in 0..140 {
        drop(provider.profile_avatar(&id).await.unwrap());
    }
    assert_eq!(
        provider
            .profile_statistics(&id)
            .await
            .unwrap()
            .username
            .as_deref(),
        Some("plugin-user")
    );
    drop(provider);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn malformed_account_operation_registration_keeps_the_published_provider() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment.account(None).await;
    let (runtime, core) = environment
        .provider(configuration(), vec![account_grant("accounts")])
        .await;
    let store = environment.store.admin_ports().plugins();
    let snapshot = store.load_instances().await.unwrap();
    for operations in [json!([]), json!(["profile", "profile"]), json!(["unknown"])] {
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration["account_operations"] = operations;
        assert!(
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .is_err()
        );
    }
    // 非中间件阶段由能力固定；无效安装合同在进入候选集合前被拒绝。
    let contributes = [
        Capability::Executor,
        Capability::Authentication,
        Capability::AccountManagement,
    ]
    .into_iter()
    .map(|capability| {
        crate::support::contribution(
            capability,
            if capability == Capability::AccountManagement {
                vec![Stage::Execution]
            } else {
                capability.fixed_stages().to_vec()
            },
            if capability == Capability::Executor {
                vec!["openai".into()]
            } else {
                vec![]
            },
            if capability == Capability::Executor {
                vec!["canonical".into()]
            } else {
                vec![]
            },
        )
    })
    .collect::<Contributions>();
    let archive = crate::support::package_with_contributions(
        crate::support::worker(),
        vec![Permission::Accounts],
        contributes,
    );
    let inspector = gateway_plugin_runtime::PackageInspector::new(
        gateway_plugin_runtime::PackageLimits::default(),
        "1.0.0".parse().unwrap(),
    );
    assert_eq!(
        gateway_plugin_runtime::ValidatedPackage::read(
            archive.clone(),
            None,
            gateway_plugin_runtime::PackageLimits::default(),
        )
        .err(),
        Some(gateway_plugin_runtime::PackageError::Manifest(
            gateway_plugin_sdk::ManifestError::Invalid,
        )),
    );
    assert!(inspector.inspect(archive, None).await.is_err());
    let provider = runtime
        .admin_registry(core.snapshots())
        .require(&ProviderKind::new("example").unwrap())
        .unwrap();
    assert!(provider.profile_statistics(&id).await.is_ok());
    drop(provider);
    drop(store);
    drop(core);
    drop(runtime);
    environment.close().await;
}
