use std::time::SystemTime;

use futures::{StreamExt as _, stream};
use gateway_core::{
    account::{
        AccountCandidate, AccountEligibilityPolicy, AccountSelectionContext, AccountSelector,
        LoadedCredential,
    },
    concurrency::CapacityWait,
    engine::{AttemptContext, policy::AccountPolicyError},
    error::{ProviderError, ProviderErrorKind},
    operation::CapabilityRequirements,
    provider_ports::{
        ProviderLeaseAcquisition, ProviderLeaseGuard, ProviderLeaseRequest,
        ProviderSchedulingLeaseRequest,
    },
    upstream::UpstreamSendState,
};

use super::PluginProvider;

impl PluginProvider {
    pub(super) async fn select_account(
        &self,
        model: Option<&str>,
        requirements: &CapabilityRequirements,
        context: &AttemptContext,
    ) -> Result<(LoadedCredential, Box<dyn ProviderLeaseGuard>), ProviderError> {
        let policy = context.account_selection_policy();
        let mut waiting = CapacityWait::new(
            &self.waiting,
            policy.queue_policy(),
            context.deadline(),
            context.concurrency_wait_budget(),
        );
        let accounts = self.ports.accounts();
        let leases = self.ports.leases();
        let cooldowns = self.ports.cooldowns();
        'refresh: loop {
            let candidates = accounts
                .list_for_provider(&self.kind)
                .await
                .map_err(|_| unavailable())?;
            let candidates: Vec<_> = candidates
                .into_iter()
                .filter(|account| {
                    account.provider() == &self.kind
                        && context
                            .required_account()
                            .is_none_or(|required| account.id() == required)
                        && context.account_scope().is_none_or(|scope| {
                            scope.allows(account.id())
                                && model.is_none_or(|model| scope.allows_model(account.id(), model))
                        })
                })
                .collect();
            let candidates = self
                .filter_model_accounts(candidates, model, requirements, context)
                .await?;
            let ids: Vec<_> = candidates
                .iter()
                .map(|account| account.id().clone())
                .collect();
            let state = leases
                .load_state(context.client_api_key_ref(), &self.kind, &ids)
                .await
                .map_err(|_| unavailable())?;
            // 冷却查询有界并发，避免大账号池串行往返；事实始终由现有端口提供。
            let results: Vec<Result<_, ProviderError>> = stream::iter(candidates)
                .map(|account| {
                    let cooldowns = &cooldowns;
                    let state = &state;
                    async move {
                        let cooldown = cooldowns
                            .read(account.id())
                            .await
                            .map_err(|_| unavailable())?
                            .filter(|cooldown| {
                                cooldown.credential_revision() == account.revision()
                            });
                        let signals = state
                            .signals()
                            .get(account.id())
                            .cloned()
                            .ok_or_else(unavailable)?
                            .with_rate_limit(cooldown.map(|cooldown| cooldown.scheduling_state()));
                        Ok(AccountCandidate { account, signals })
                    }
                })
                .buffer_unordered(16)
                .collect()
                .await;
            let candidates: Vec<_> = results.into_iter().collect::<Result<_, _>>()?;
            let mut selection = AccountSelectionContext {
                policy,
                now: SystemTime::now(),
                excluded_accounts: context.excluded_accounts().clone(),
                preferred_account: context.required_account().cloned(),
                preferred_account_overrides_weight: true,
                round_robin_cursor: state.round_robin_cursor(),
                eligibility: if context.is_diagnostic_required_account() {
                    AccountEligibilityPolicy::BypassForDiagnostic
                } else {
                    AccountEligibilityPolicy::Enforce
                },
                account_scope: context.account_scope().cloned(),
            };
            let mut wait_for = AccountSelector.wait_candidates(&candidates, &selection);
            for candidate in &candidates {
                if !waiting.can_try(candidate.account.id()) {
                    selection
                        .excluded_accounts
                        .insert(candidate.account.id().clone());
                    wait_for.push(candidate.account.id().clone());
                }
            }
            while let Some(selected) = match context
                .select_account(&self.kind, model, &candidates, &selection)
                .await
            {
                Ok(selection) => selection,
                Err(AccountPolicyError::StaleCandidate) => continue 'refresh,
                Err(AccountPolicyError::Rejected) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::RequestPolicyDenied,
                        UpstreamSendState::NotSent,
                    ));
                }
                Err(AccountPolicyError::Fault) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Unavailable,
                        UpstreamSendState::NotSent,
                    ));
                }
            } {
                let account = &selected.candidate().account;
                let acquired = leases
                    .try_acquire(ProviderLeaseRequest::Scheduling(
                        ProviderSchedulingLeaseRequest::new(
                            self.kind.clone(),
                            account.id().clone(),
                            account.revision(),
                            account.effective_concurrency(policy.max_concurrent_per_account()),
                            policy.request_interval(),
                            context.deadline(),
                        ),
                    ))
                    .await
                    .map_err(|_| unavailable())?;
                match acquired {
                    ProviderLeaseAcquisition::Acquired(guard) => {
                        let loaded = accounts
                            .load_credential(account.id(), account.revision())
                            .await
                            .map_err(|_| unavailable())?;
                        if loaded.account.id() != account.id()
                            || loaded.account.provider() != &self.kind
                            || loaded.account.revision() != account.revision()
                        {
                            drop(guard);
                            continue 'refresh;
                        }
                        return Ok((loaded, guard));
                    }
                    ProviderLeaseAcquisition::Busy { .. } => {
                        wait_for.push(account.id().clone());
                        selection.excluded_accounts.insert(account.id().clone());
                    }
                }
            }
            wait_for.sort();
            wait_for.dedup();
            if wait_for.is_empty() {
                return Err(ProviderError::new(
                    ProviderErrorKind::NoEligibleAccount,
                    UpstreamSendState::NotSent,
                ));
            }
            if policy.queue_policy().max_waiting == 0 {
                return Err(ProviderError::new(
                    ProviderErrorKind::AccountCapacityUnavailable,
                    UpstreamSendState::NotSent,
                ));
            }
            waiting.wait(&wait_for).await.map_err(|error| {
                ProviderError::new(error.provider_kind(), UpstreamSendState::NotSent)
            })?;
        }
    }
}

fn unavailable() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderInfrastructureUnavailable,
        UpstreamSendState::NotSent,
    )
}
