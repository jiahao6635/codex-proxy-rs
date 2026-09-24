<script setup lang="ts">
import type { AccountCreateSource } from './model'
import type { AccountProvider } from '@/api'
import { BaseSegmented } from '@codex-proxy/ui'
import { LayoutGrid } from '@lucide/vue'
import { computed } from 'vue'
import { formatProviderLabel, providerIcon } from '@/utils/providers'

const props = defineProps<{
  providers: AccountProvider[]
  disabled: boolean
}>()
const source = defineModel<AccountCreateSource | null>({ required: true })

const options = computed(() => [
  ...(props.providers.some(provider => provider.credentials.import)
    ? [{ value: 'bundle', label: '批量导入', icon: LayoutGrid }]
    : []),
  ...props.providers
    .filter(provider => provider.credentials.import || provider.credentials.login)
    .map(provider => ({
      value: `provider:${provider.provider}`,
      label: formatProviderLabel(provider.provider),
      icon: providerIcon(provider.provider),
    })),
])
const selected = computed({
  get: () => source.value?.kind === 'provider' ? `provider:${source.value.id}` : source.value?.kind ?? '',
  set: (value: string) => {
    if (!options.value.some(option => option.value === value))
      return
    source.value = value === 'bundle' ? { kind: 'bundle' } : { kind: 'provider', id: value.slice('provider:'.length) }
  },
})
</script>

<template>
  <BaseSegmented
    v-if="options.length"
    v-model="selected"
    class="w-full"
    label="选择账号平台"
    :options="options"
    :disabled="disabled"
    size="lg"
  />
  <p v-else class="m-0 text-cp-sm text-cp-text-tertiary">
    暂无可录入账号的平台
  </p>
</template>
