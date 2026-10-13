<script setup lang="ts">
import type { UsageListRecord } from '@/api'

import { computed } from 'vue'
import UsageDetailPopover from '@/components/usage/UsageDetailPopover.vue'
import { usageLatencyDetails } from './shared/presenter'

const props = defineProps<{
  record: Pick<UsageListRecord, 'latencyDetails' | 'firstTokenLatencyMs' | 'latencyMs'>
}>()

const latencyDetails = computed(() => usageLatencyDetails(props.record))

const latencyTones = {
  good: { text: 'text-cp-success-text', color: 'var(--cp-color-success)' },
  warn: { text: 'text-cp-warning-text', color: 'var(--cp-color-warning)' },
  // 主题的 Orange 与 Warning 共用色种，混入 Error 让相邻两档可区分
  slow: { text: 'latency-slow', color: 'color-mix(in oklab, var(--cp-color-warning) 65%, var(--cp-color-error))' },
  critical: { text: 'text-cp-error-text', color: 'var(--cp-color-error)' },
}

const latencyBarStyle = computed(() => {
  const { firstOutputSeverity, totalSeverity } = latencyDetails.value
  const totalColor = totalSeverity === null ? 'var(--cp-color-fill-tertiary)' : latencyTones[totalSeverity].color
  const firstColor = firstOutputSeverity === null ? totalColor : latencyTones[firstOutputSeverity].color
  return { background: `linear-gradient(to bottom, ${firstColor} 40%, ${totalColor} 60%)` }
})
</script>

<template>
  <div class="flex items-center justify-end gap-1.5">
    <span
      class="w-1 self-stretch shrink-0 rounded-full"
      :style="latencyBarStyle"
      aria-hidden="true"
    />
    <div
      class="grid grid-cols-[auto_auto] items-center justify-end gap-x-2 gap-y-1.5 whitespace-nowrap font-mono text-cp-sm leading-none font-heavy tabular-nums"
    >
      <span class="text-cp-xs text-cp-text-quaternary">首字</span>
      <span :class="latencyDetails.firstOutputSeverity === null ? 'text-cp-text-quaternary' : latencyTones[latencyDetails.firstOutputSeverity].text">
        {{ latencyDetails.firstOutputDisplay }}
      </span>
      <span class="text-cp-xs text-cp-text-quaternary">总耗时</span>
      <span :class="latencyDetails.totalSeverity === null ? 'text-cp-text-quaternary' : latencyTones[latencyDetails.totalSeverity].text">
        {{ latencyDetails.totalDisplay }}
      </span>
    </div>

    <UsageDetailPopover trigger-label="查看延迟明细">
      <section
        v-for="(section, index) in latencyDetails.sections"
        :key="section.title"
        class="grid gap-1.5 text-cp-text-secondary"
        :class="{ 'border-t border-cp-split pt-2': index > 0 }"
      >
        <p class="m-0 flex items-center justify-between gap-3 text-cp-sm font-heavy text-cp-text">
          <span>{{ section.title }}</span>
          <span
            class="rounded px-1 py-0.5 text-[10px] leading-none font-normal"
            :class="section.source === 'official'
              ? 'bg-cp-primary-container text-cp-primary-on-container'
              : 'bg-cp-fill-tertiary text-cp-text-tertiary'"
          >
            {{ section.source === 'official' ? '官方' : '本地' }}
          </span>
        </p>
        <div
          v-for="item in section.items"
          :key="item.label"
          class="flex justify-between gap-4 text-cp-xs"
        >
          <span class="whitespace-nowrap">{{ item.label }}</span>
          <span
            class="whitespace-nowrap font-mono font-heavy"
            :class="item.emphasized ? 'text-cp-info-text' : 'text-cp-text'"
          >
            {{ item.value }}
          </span>
        </div>
      </section>
    </UsageDetailPopover>
  </div>
</template>

<style scoped>
.latency-slow {
  color: color-mix(in oklab, var(--cp-color-warning-text) 65%, var(--cp-color-error-text));
}
</style>
