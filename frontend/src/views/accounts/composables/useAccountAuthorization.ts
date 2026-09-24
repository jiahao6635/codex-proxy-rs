import type { AccountImportSettings, AccountOAuthStartResponse, AccountProvider } from '@/api'
import { computed, onScopeDispose, shallowRef } from 'vue'
import { completeAccountOAuth, pollAccountAuthorization, startAccountOAuth } from '@/api'
import { errorMessage } from '@/utils/async'

type Completion = NonNullable<AccountProvider['credentials']['login']>['completion']
type Status = 'idle' | 'starting' | 'waiting' | 'polling' | 'paused' | 'expired' | 'completing'

export interface AccountAuthorizationView {
  flow: AccountOAuthStartResponse | null
  completion: Completion | null
  status: Status
  error: string
}

interface AuthorizationRequest {
  start: Parameters<typeof startAccountOAuth>[0]
  completion: Completion
  settings?: AccountImportSettings
}

export function useAccountAuthorization(onComplete: () => Promise<void>) {
  const flow = shallowRef<AccountOAuthStartResponse | null>(null)
  const completion = shallowRef<Completion | null>(null)
  const status = shallowRef<Status>('idle')
  const error = shallowRef('')
  const callback = shallowRef('')
  const busy = computed(() => status.value === 'starting' || status.value === 'completing')
  const view = computed<AccountAuthorizationView>(() => ({
    flow: flow.value,
    completion: completion.value,
    status: status.value,
    error: error.value,
  }))
  let context: { provider: string, settings?: AccountImportSettings } | undefined
  let controller: AbortController | undefined
  let pollTimer: ReturnType<typeof setTimeout> | undefined
  let expiryTimer: ReturnType<typeof setTimeout> | undefined

  function clearTimers() {
    clearTimeout(pollTimer)
    clearTimeout(expiryTimer)
    pollTimer = undefined
    expiryTimer = undefined
  }

  function reset() {
    controller?.abort()
    controller = undefined
    clearTimers()
    context = undefined
    flow.value = null
    completion.value = null
    status.value = 'idle'
    error.value = ''
    callback.value = ''
  }

  function expired() {
    return !flow.value || Date.parse(flow.value.expiresAt) <= Date.now()
  }

  async function start(request: AuthorizationRequest) {
    reset()
    const current = new AbortController()
    controller = current
    status.value = 'starting'
    try {
      const result = await startAccountOAuth(request.start, { signal: current.signal, silent: true })
      if (current.signal.aborted)
        return
      const expiresAt = Date.parse(result.expiresAt)
      if (!Number.isFinite(expiresAt))
        throw new Error('授权有效期无效，请重新生成链接')
      context = { provider: request.start.provider, settings: request.settings }
      completion.value = request.completion
      flow.value = result
      status.value = expired() ? 'expired' : 'waiting'
      expiryTimer = setTimeout(() => {
        clearTimeout(pollTimer)
        // 不取消正在确认的提交，过期后仍接收服务端已经提交的结果。
        if (status.value !== 'polling' && status.value !== 'completing')
          status.value = 'expired'
      }, Math.max(0, Math.min(expiresAt - Date.now(), 2_147_483_647)))
      if (request.completion === 'poll' && !expired())
        void poll()
    }
    catch (cause) {
      if (!current.signal.aborted) {
        error.value = errorMessage(cause, '生成授权链接失败')
        status.value = 'idle'
      }
    }
  }

  async function poll() {
    const current = controller
    const currentFlow = flow.value
    if (!current || !currentFlow || !context || status.value === 'polling')
      return
    clearTimeout(pollTimer)
    status.value = 'polling'
    error.value = ''
    try {
      const result = await pollAccountAuthorization({
        ...context,
        flowId: currentFlow.flowId,
        callbackUrl: callback.value.trim() || undefined,
      }, { signal: current.signal, silent: true })
      if (current.signal.aborted)
        return
      if (result.status === 'complete') {
        clearTimers()
        await onComplete()
        return
      }
      status.value = expired() ? 'expired' : 'waiting'
      if (!expired()) {
        const delay = Number.isFinite(result.retryAfterMs) ? Math.max(250, result.retryAfterMs) : 1000
        pollTimer = setTimeout(() => void poll(), Math.min(delay, 2_147_483_647))
      }
    }
    catch (cause) {
      if (!current.signal.aborted) {
        status.value = expired() ? 'expired' : 'paused'
        error.value = errorMessage(cause, '授权状态查询失败，可重试查询')
      }
    }
  }

  async function complete() {
    if (completion.value === 'poll') {
      await poll()
      return
    }
    const current = controller
    const currentFlow = flow.value
    if (!current || !currentFlow || !context || busy.value)
      return
    if (!callback.value.trim()) {
      error.value = '请粘贴回调地址或授权码'
      return
    }
    status.value = 'completing'
    error.value = ''
    try {
      await completeAccountOAuth({
        ...context,
        flowId: currentFlow.flowId,
        callbackUrl: callback.value.trim(),
      }, { signal: current.signal, silent: true })
      if (!current.signal.aborted) {
        clearTimers()
        await onComplete()
      }
    }
    catch (cause) {
      if (!current.signal.aborted) {
        status.value = expired() ? 'expired' : 'paused'
        error.value = errorMessage(cause, '完成授权失败，可重试提交')
      }
    }
  }

  onScopeDispose(reset)
  return { view, callback, busy, start, complete, reset }
}
