<script setup lang="ts">
import type {
  ProviderRequestProfile,
  ProviderRequestProfileOptions,
  ProviderRequestProfilePreview,
} from '@/api/modules/client-profiles'
import { BaseButton, BaseFormItem, BaseSegmented, BaseSelect, BaseSkeleton } from '@codex-proxy/ui'
import { isEqual } from 'es-toolkit'
import { computed, shallowRef, watch } from 'vue'
import {
  getProviderClientProfileOptions,
  previewProviderClientProfile,
} from '@/api/modules/client-profiles'
import { errorMessage } from '@/utils/async'
import { formatDateTime } from '@/utils/date'

const props = withDefaults(defineProps<{
  provider: string
  active?: boolean
  disabled?: boolean
  allowInherit?: boolean
}>(), {
  active: true,
  disabled: false,
  allowInherit: false,
})

const model = defineModel<ProviderRequestProfile | null>({ required: true })
const options = shallowRef<ProviderRequestProfileOptions>()
const preview = shallowRef<ProviderRequestProfilePreview>()
const loading = shallowRef(true)
const loadError = shallowRef('')
const previewing = shallowRef(false)
const previewError = shallowRef('')
let loadGeneration = 0

const presetOptions = computed(() => options.value?.presets.map(preset => ({
  label: preset.label,
  value: preset.id,
  description: preset.description ?? undefined,
})) ?? [])

const selectedPreset = computed({
  get: () => options.value?.presets.find(preset => isEqual(preset.configuration, model.value))?.id ?? '',
  set: (value: string) => {
    const preset = options.value?.presets.find(item => item.id === value)
    if (preset)
      model.value = cloneProfile(preset.configuration)
  },
})

const profileSource = computed({
  get: () => model.value === null ? 'global' : 'independent',
  set: (value: string) => {
    if (value === 'global') {
      model.value = null
      return
    }
    const configuration = options.value?.globalConfiguration ?? options.value?.defaultConfiguration
    if (configuration)
      model.value = cloneProfile(configuration)
  },
})

const releaseLabel = computed(() => {
  switch (preview.value?.release?.status) {
    case 'current': return '当前版本'
    case 'update_available': return '有可用更新'
    case 'failed': return '版本信息不可用'
    default: return ''
  }
})

function cloneProfile(value: ProviderRequestProfile): ProviderRequestProfile {
  return JSON.parse(JSON.stringify(value)) as ProviderRequestProfile
}

async function load() {
  const generation = ++loadGeneration
  loading.value = true
  loadError.value = ''
  try {
    const result = await getProviderClientProfileOptions(props.provider)
    if (generation !== loadGeneration)
      return
    options.value = result
    if (!props.allowInherit && model.value === null)
      model.value = cloneProfile(result.globalConfiguration ?? result.defaultConfiguration)
  }
  catch (error) {
    if (generation === loadGeneration)
      loadError.value = errorMessage(error)
  }
  finally {
    if (generation === loadGeneration)
      loading.value = false
  }
}

watch(() => props.provider, () => {
  options.value = undefined
  void load()
}, { immediate: true })

watch([model, () => props.active, () => props.provider, options], ([configuration, active], _, onCleanup) => {
  let cancelled = false
  preview.value = undefined
  previewError.value = ''
  previewing.value = active && !!options.value
  if (!active || !options.value)
    return

  const timer = setTimeout(async () => {
    try {
      const result = await previewProviderClientProfile(props.provider, configuration)
      if (!cancelled)
        preview.value = result
    }
    catch (error) {
      if (!cancelled)
        previewError.value = errorMessage(error)
    }
    finally {
      if (!cancelled)
        previewing.value = false
    }
  }, 250)
  onCleanup(() => {
    cancelled = true
    clearTimeout(timer)
  })
}, { immediate: true })
</script>

<template>
  <div class="grid min-w-0 gap-4">
    <div v-if="allowInherit" class="flex flex-wrap items-center justify-between gap-3">
      <BaseSegmented
        v-model="profileSource"
        :label="`${provider} 客户端身份来源`"
        class="w-fit"
        :options="[{ label: '全局配置', value: 'global' }, { label: '独立配置', value: 'independent' }]"
        :disabled="disabled || loading || !!loadError"
      />
      <slot name="source-extra" />
    </div>

    <div v-if="loadError" role="alert" class="flex flex-wrap items-center justify-between gap-3 text-cp text-cp-error">
      <span>画像预设加载失败：{{ loadError }}</span>
      <BaseButton size="sm" :disabled="disabled" @click="load">
        重试
      </BaseButton>
    </div>
    <p v-else-if="loading" role="status" class="m-0 text-cp text-cp-text-secondary">
      正在加载画像预设…
    </p>
    <template v-else-if="model">
      <BaseFormItem label="身份预设">
        <BaseSelect
          v-model="selectedPreset"
          class="w-full sm:max-w-md"
          :options="presetOptions"
          :disabled="disabled"
          placeholder="选择 Provider 提供的预设"
        />
      </BaseFormItem>
      <p v-if="!selectedPreset" role="alert" class="m-0 text-cp-sm text-cp-error">
        当前画像已不在 Provider 的可用预设中，请重新选择
      </p>
    </template>

    <div class="grid min-h-24 min-w-0 content-center gap-2 rounded-cp bg-cp-fill-quaternary p-4" aria-live="polite" :aria-busy="previewing">
      <div v-if="previewing" class="grid gap-2" role="status" aria-label="正在解析客户端画像">
        <div class="flex h-lh items-center" aria-hidden="true">
          <BaseSkeleton shape="text" class="w-2/5" />
        </div>
        <div class="flex h-lh items-center" aria-hidden="true">
          <BaseSkeleton shape="text" class="w-4/5" />
        </div>
      </div>
      <p v-else-if="previewError" role="alert" class="m-0 text-cp-sm text-cp-error">
        {{ previewError }}
      </p>
      <template v-else-if="preview">
        <div class="flex flex-wrap items-baseline gap-x-2 gap-y-1">
          <strong class="text-cp font-semibold text-cp-text">{{ preview.product }}</strong>
          <span class="text-cp-sm text-cp-text-secondary">
            {{ preview.version }}<template v-if="preview.build">
              · {{ preview.build }}
            </template>
          </span>
          <span v-if="releaseLabel" class="text-cp-xs text-cp-text-tertiary">
            {{ releaseLabel }}
          </span>
        </div>
        <code class="break-all text-cp-sm text-cp-text">{{ preview.userAgent }}</code>
        <p class="m-0 text-cp-xs text-cp-text-tertiary">
          {{ preview.target.osType }} {{ preview.target.osVersion }} · {{ preview.target.arch }} · {{ preview.target.terminal }}
          <template v-if="preview.verifiedAt">
            · 核验于 {{ formatDateTime(preview.verifiedAt) }}
          </template>
        </p>
        <dl v-if="preview.attributes.length" class="m-0 flex flex-wrap gap-x-4 gap-y-1 text-cp-xs">
          <div v-for="attribute in preview.attributes" :key="`${attribute.label}:${attribute.value}`" class="flex min-w-0 gap-1">
            <dt class="text-cp-text-tertiary">
              {{ attribute.label }}
            </dt>
            <dd class="m-0 truncate text-cp-text-secondary">
              {{ attribute.value }}
            </dd>
          </div>
        </dl>
        <p v-if="preview.release?.status === 'failed' && preview.release.error" class="m-0 text-cp-sm text-cp-warning">
          {{ preview.release.error }}
        </p>
      </template>
      <p v-else class="m-0 text-cp-sm text-cp-text-tertiary">
        选择预设后显示实际上游画像
      </p>
    </div>
  </div>
</template>
