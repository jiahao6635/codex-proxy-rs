<script setup lang="ts">
import type {
  ClientProfileSelection,
  ProviderRequestProfile,
  ProviderRequestProfiles,
  XaiClientProfileSelection,
} from '@/api/modules/client-profiles'
import { BaseButton, BaseSegmented } from '@codex-proxy/ui'
import { computed, shallowRef, watch } from 'vue'
import { getClientProfileProviders } from '@/api/modules/client-profiles'
import { errorMessage } from '@/utils/async'
import { formatProviderLabel, providerIcon } from '@/utils/providers'
import ClientProfileEditor from './ClientProfileEditor.vue'
import ProviderRequestProfileEditor from './ProviderRequestProfileEditor.vue'
import XaiClientProfileEditor from './XaiClientProfileEditor.vue'

withDefaults(defineProps<{
  active?: boolean
  disabled?: boolean
  allowInherit?: boolean
}>(), {
  active: true,
  disabled: false,
  allowInherit: false,
})

const model = defineModel<ProviderRequestProfiles>({ required: true })
const discoveredProviders = shallowRef<string[]>([])
const provider = shallowRef('')
const loading = shallowRef(true)
const loadError = shallowRef('')

const providerIds = computed(() => [...new Set([
  ...discoveredProviders.value,
  ...Object.keys(model.value),
])])

const providerOptions = computed(() => providerIds.value.map(value => ({
  value,
  label: formatProviderLabel(value, value),
  icon: providerIcon(value),
})))
const providerSelectorStyle = computed(() => ({
  width: `${Math.max(providerOptions.value.length, 1) * 40}px`,
}))
const selectedProviderIsOrphaned = computed(() => Boolean(
  provider.value
  && !loading.value
  && !loadError.value
  && model.value[provider.value] !== undefined
  && !discoveredProviders.value.includes(provider.value),
))

const openai = profileModel<ClientProfileSelection>('openai')
const xai = profileModel<XaiClientProfileSelection>('xai')
const selectedProfile = computed<ProviderRequestProfile | null>({
  get: () => provider.value ? model.value[provider.value] ?? null : null,
  set: value => updateProfile(provider.value, value),
})

function profileModel<T extends object>(providerId: string) {
  return computed<T | null>({
    get: () => (model.value[providerId] as T | undefined) ?? null,
    set: value => updateProfile(providerId, value as ProviderRequestProfile | null),
  })
}

function updateProfile(providerId: string, value: ProviderRequestProfile | null) {
  if (!providerId)
    return
  const profiles = { ...model.value }
  if (value === null)
    delete profiles[providerId]
  else
    profiles[providerId] = value
  model.value = profiles
}

async function loadProviders() {
  loading.value = true
  loadError.value = ''
  try {
    discoveredProviders.value = (await getClientProfileProviders()).providers
  }
  catch (error) {
    loadError.value = errorMessage(error)
  }
  finally {
    loading.value = false
  }
}

watch(providerIds, (providers) => {
  if (!providers.includes(provider.value))
    provider.value = providers[0] ?? ''
}, { immediate: true })

void loadProviders()
</script>

<template>
  <div class="grid min-w-0 gap-4">
    <div v-if="!allowInherit" class="flex flex-wrap items-start justify-between gap-3">
      <slot name="heading" />
      <BaseSegmented
        v-if="providerOptions.length"
        v-model="provider"
        class="ml-auto max-w-full shrink-0"
        label="客户端身份 Provider"
        :options="providerOptions"
        :disabled="disabled || loading"
        :style="providerSelectorStyle"
        display="icon"
      />
      <p v-else class="m-0 text-cp-sm text-cp-text-tertiary" role="status">
        {{ loading ? '正在加载…' : '暂无可配置 Provider' }}
      </p>
    </div>
    <p v-else-if="!providerOptions.length" class="m-0 text-cp-sm text-cp-text-tertiary" role="status">
      {{ loading ? '正在加载…' : '暂无可配置 Provider' }}
    </p>

    <div v-if="loadError" role="alert" class="flex flex-wrap items-center justify-between gap-3 text-cp text-cp-error">
      <span>Provider 列表加载失败：{{ loadError }}</span>
      <BaseButton size="sm" :disabled="disabled" @click="loadProviders">
        重试
      </BaseButton>
    </div>

    <div
      v-if="selectedProviderIsOrphaned"
      role="status"
      class="flex flex-wrap items-center justify-between gap-3 rounded-cp bg-cp-warning-container p-4"
    >
      <div class="min-w-0">
        <p class="m-0 text-cp-sm font-semibold text-cp-warning-on-container">
          当前发布代次未注册此 Provider
        </p>
        <p class="mt-1 mb-0 text-cp-xs text-cp-warning-on-container">
          历史画像会继续保留且不会参与运行
        </p>
      </div>
      <BaseSegmented
        v-if="allowInherit"
        v-model="provider"
        class="max-w-full shrink-0"
        label="上游身份平台"
        :options="providerOptions"
        :disabled="disabled || loading"
        :style="providerSelectorStyle"
        display="icon"
      />
      <BaseButton variant="destructive" size="sm" :disabled="disabled" @click="updateProfile(provider, null)">
        移除历史画像
      </BaseButton>
    </div>
    <ClientProfileEditor
      v-else-if="provider === 'openai'"
      v-model="openai"
      :allow-inherit="allowInherit"
      :active="active"
      :disabled="disabled"
    >
      <template #source-extra>
        <BaseSegmented
          v-model="provider"
          class="max-w-full shrink-0"
          label="上游身份平台"
          :options="providerOptions"
          :disabled="disabled || loading"
          :style="providerSelectorStyle"
          display="icon"
        />
      </template>
    </ClientProfileEditor>
    <XaiClientProfileEditor
      v-else-if="provider === 'xai'"
      v-model="xai"
      :allow-inherit="allowInherit"
      :active="active"
      :disabled="disabled"
    >
      <template #source-extra>
        <BaseSegmented
          v-model="provider"
          class="max-w-full shrink-0"
          label="上游身份平台"
          :options="providerOptions"
          :disabled="disabled || loading"
          :style="providerSelectorStyle"
          display="icon"
        />
      </template>
    </XaiClientProfileEditor>
    <ProviderRequestProfileEditor
      v-else-if="provider"
      :key="provider"
      v-model="selectedProfile"
      :provider="provider"
      :allow-inherit="allowInherit"
      :active="active"
      :disabled="disabled"
    >
      <template #source-extra>
        <BaseSegmented
          v-model="provider"
          class="max-w-full shrink-0"
          label="上游身份平台"
          :options="providerOptions"
          :disabled="disabled || loading"
          :style="providerSelectorStyle"
          display="icon"
        />
      </template>
    </ProviderRequestProfileEditor>
  </div>
</template>
