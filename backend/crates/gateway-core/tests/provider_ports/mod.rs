use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use gateway_core::account::{
    AccountRuntimeSignals, CredentialRevision, OpaqueProviderData, ProviderAccountId,
};
use gateway_core::provider_ports::{
    NewOAuthPendingFlow, OAuthPendingBinding, ProviderRefreshPolicy, ProviderSchedulingState,
    ProviderSessionAffinityKey, ProviderStoreErrorKind,
};
use gateway_core::routing::ProviderKind;

#[test]
fn oauth_pending_binding_debug_redacts_raw_value() {
    let binding = OAuthPendingBinding::try_new("must-not-appear").expect("valid binding");

    assert_eq!(format!("{binding:?}"), "OAuthPendingBinding([REDACTED])");
}

#[test]
fn provider_session_affinity_key_debug_is_opaque() {
    let key = ProviderSessionAffinityKey::try_new("opaque-session-key").expect("valid key");

    assert_eq!(format!("{key:?}"), "ProviderSessionAffinityKey([OPAQUE])");
}

#[test]
fn oauth_pending_ttl_rejects_zero_and_more_than_thirty_minutes() {
    let provider = ProviderKind::new("fixture").expect("valid provider");
    let flow = OAuthPendingBinding::try_new("flow").expect("valid flow");
    let owner = OAuthPendingBinding::try_new("owner").expect("valid owner");
    let payload = OpaqueProviderData::new(serde_json::Map::new());

    for ttl in [Duration::ZERO, Duration::from_secs(30 * 60 + 1)] {
        let error = NewOAuthPendingFlow::try_new(
            provider.clone(),
            flow.clone(),
            owner.clone(),
            ttl,
            payload.clone(),
        )
        .expect_err("invalid TTL must fail");
        assert_eq!(error.kind(), ProviderStoreErrorKind::InvalidData);
    }
}

#[test]
fn refresh_policy_requires_a_positive_margin() {
    let error = ProviderRefreshPolicy::try_new(
        Duration::ZERO,
        NonZeroU32::new(1).expect("positive concurrency"),
    )
    .expect_err("zero margin must fail");

    assert_eq!(error.kind(), ProviderStoreErrorKind::InvalidData);
}

#[test]
fn refresh_policy_should_mark_tokens_due_at_the_exact_configured_margin() {
    let policy = ProviderRefreshPolicy::try_new(
        Duration::from_secs(3_600),
        NonZeroU32::new(2).expect("positive concurrency"),
    )
    .expect("valid policy");
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    let expires_at = observed_at + Duration::from_secs(7_200);

    assert!(!policy.is_refresh_due(expires_at, observed_at));
    assert!(policy.is_refresh_due(observed_at + Duration::from_secs(3_600), observed_at));
}

#[test]
fn refresh_policy_should_mark_expired_tokens_due() {
    let policy = ProviderRefreshPolicy::try_new(
        Duration::from_secs(3_600),
        NonZeroU32::new(1).expect("positive concurrency"),
    )
    .expect("valid policy");
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);

    assert!(policy.is_refresh_due(observed_at - Duration::from_secs(1), observed_at));
}

#[test]
fn scheduling_state_preserves_provider_neutral_signals() {
    let account = ProviderAccountId::new("acct_fixture").expect("valid account");
    let signals = BTreeMap::from([(
        account.clone(),
        AccountRuntimeSignals {
            in_flight: 2,
            last_started_at: None,
            quota_reset_at: None,
            quota_remaining_rank: Some(7),
            cooldown: None,
            failure_rate_basis_points: Some(125),
            first_output_latency_ms: Some(250),
        },
    )]);
    let state = ProviderSchedulingState::new(signals, 9);

    assert_eq!(state.signals()[&account].in_flight, 2);
    assert_eq!(
        state.signals()[&account].failure_rate_basis_points,
        Some(125)
    );
    assert_eq!(state.round_robin_cursor(), 9);
    assert_eq!(
        CredentialRevision::new(1).expect("positive revision").get(),
        1
    );
}

// —— 模型降智自动下线策略 ——

use gateway_core::provider_ports::{ModelDowngradePolicy, ModelDowngradeVerdict};

fn downgrade_ladder() -> Vec<String> {
    [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn downgrade_policy() -> ModelDowngradePolicy {
    ModelDowngradePolicy::try_new(true, 3, 600, 3_600, downgrade_ladder(), None)
        .expect("valid downgrade policy")
}

#[test]
fn model_downgrade_flags_lower_tier_return() {
    let policy = downgrade_policy();

    assert_eq!(
        policy.classify("gpt-6-astra", "gpt-5.6-luna"),
        ModelDowngradeVerdict::Downgraded,
    );
    assert_eq!(
        policy.classify("gpt-5.6-sol", "gpt-5.6-terra"),
        ModelDowngradeVerdict::Downgraded,
    );
}

#[test]
fn model_downgrade_accepts_same_or_higher_tier() {
    let policy = downgrade_policy();

    assert_eq!(
        policy.classify("gpt-6-astra", "gpt-6-astra"),
        ModelDowngradeVerdict::Healthy,
    );
    // 返回档位高于请求（理论上罕见）不算降智。
    assert_eq!(
        policy.classify("gpt-5.6-luna", "gpt-6-astra"),
        ModelDowngradeVerdict::Healthy,
    );
}

#[test]
fn model_downgrade_unknown_models_are_not_flagged() {
    let policy = downgrade_policy();

    // 请求模型不在档位表内：无法比较，不触发下线。
    assert_eq!(
        policy.classify("gpt-image-2", "gpt-5.6-luna"),
        ModelDowngradeVerdict::Unknown,
    );
    // 返回模型不在档位表内：同样按未知处理。
    assert_eq!(
        policy.classify("gpt-6-astra", "gpt-reserve"),
        ModelDowngradeVerdict::Unknown,
    );
}

#[test]
fn model_downgrade_normalizes_responses_prefix() {
    let policy = downgrade_policy();

    assert_eq!(
        policy.classify("responses/gpt-6-astra", "gpt-5.6-luna"),
        ModelDowngradeVerdict::Downgraded,
    );
    assert_eq!(
        policy.classify("gpt-6-astra", "responses/gpt-6-astra"),
        ModelDowngradeVerdict::Healthy,
    );
}

#[test]
fn model_downgrade_probe_model_defaults_to_top_tier() {
    let policy = downgrade_policy();
    assert_eq!(policy.probe_model(), Some("gpt-6-astra"));

    let explicit = ModelDowngradePolicy::try_new(
        true,
        1,
        60,
        300,
        downgrade_ladder(),
        Some("gpt-5.6-sol".to_owned()),
    )
    .expect("valid explicit probe model");
    assert_eq!(explicit.probe_model(), Some("gpt-5.6-sol"));
}

#[test]
fn model_downgrade_rejects_invalid_configuration() {
    // 启用但档位表为空。
    assert!(ModelDowngradePolicy::try_new(true, 3, 600, 3_600, Vec::new(), None).is_err());
    // 档位表存在重复项。
    let dup = vec!["gpt-6-astra".to_owned(), "gpt-6-astra".to_owned()];
    assert!(ModelDowngradePolicy::try_new(true, 3, 600, 3_600, dup, None).is_err());
    // 阈值越界。
    assert!(ModelDowngradePolicy::try_new(true, 0, 600, 3_600, downgrade_ladder(), None).is_err());
    // 窗口越界。
    assert!(ModelDowngradePolicy::try_new(true, 3, 30, 3_600, downgrade_ladder(), None).is_err());
    // 冻结时长越界。
    assert!(ModelDowngradePolicy::try_new(true, 3, 600, 100, downgrade_ladder(), None).is_err());
}

#[test]
fn model_downgrade_disabled_policy_is_inert() {
    let policy = ModelDowngradePolicy::disabled();
    assert!(!policy.enabled());
    assert_eq!(policy.probe_model(), None);
    assert_eq!(
        policy.classify("gpt-6-astra", "gpt-5.6-luna"),
        ModelDowngradeVerdict::Unknown,
    );
}
