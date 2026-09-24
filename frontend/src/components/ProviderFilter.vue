<script setup lang="ts">
import { BaseIconButton, BaseSegmented } from '@codex-proxy/ui'
import { LayoutGrid, RefreshCw } from '@lucide/vue'
import { computed } from 'vue'
import { formatProviderLabel, providerIcon } from '@/utils/providers'

const props = withDefaults(defineProps<{
  providers: string[]
  disabled?: boolean
  loading?: boolean
  error?: string
}>(), { disabled: false, loading: false, error: '' })
const emit = defineEmits<{ retry: [] }>()
const provider = defineModel<string>({ required: true })
const options = computed(() => [
  { label: '全部平台', value: '', icon: LayoutGrid },
  // 数据清理或切换时间范围后，保留已选条件，由用户决定何时清除。
  ...[...new Set([...props.providers, provider.value])].filter(Boolean).map(value => ({ label: formatProviderLabel(value), value, icon: providerIcon(value) })),
])
</script>

<template>
  <div class="flex min-w-0 max-w-full items-center gap-1">
    <div class="min-w-0 overflow-x-auto">
      <BaseSegmented
        v-model="provider"
        label="按平台筛选"
        display="icon"
        :style="{ width: `${options.length * 40 + 4}px` }"
        :options="options"
        :disabled="disabled || loading"
        :aria-busy="loading"
      />
    </div>
    <BaseIconButton v-if="error" variant="ghost" label="重新加载平台筛选" :title="error" :disabled="disabled || loading" @click="emit('retry')">
      <RefreshCw class="size-4 text-cp-error" />
    </BaseIconButton>
  </div>
</template>
