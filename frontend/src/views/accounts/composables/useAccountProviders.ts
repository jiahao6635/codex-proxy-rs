import type { AccountProvider } from '@/api'
import { computed, onMounted, onScopeDispose, shallowRef } from 'vue'
import { getAccountProviders } from '@/api'
import { errorMessage } from '@/utils/async'

export function useAccountProviders() {
  const providers = shallowRef<AccountProvider[]>([])
  const loading = shallowRef(false)
  const error = shallowRef('')
  const byId = computed(() => new Map(providers.value.map(provider => [provider.provider, provider])))
  let controller: AbortController | undefined

  async function load() {
    controller?.abort()
    const current = new AbortController()
    controller = current
    loading.value = true
    error.value = ''
    try {
      const result = await getAccountProviders({ signal: current.signal, silent: true })
      if (!current.signal.aborted)
        providers.value = result
    }
    catch (cause) {
      if (!current.signal.aborted) {
        providers.value = []
        error.value = errorMessage(cause, '平台能力加载失败')
      }
    }
    finally {
      if (!current.signal.aborted)
        loading.value = false
    }
  }

  onMounted(load)
  onScopeDispose(() => controller?.abort())
  return { providers, byId, loading, error, load }
}
