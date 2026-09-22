<script setup lang="ts">
import { CircleAlert, Gauge, ListOrdered, Timer, TrendingDown } from '@lucide/vue'
import { useId } from 'vue'

import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BasePopover from '@/components/base/BasePopover.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

const enabled = defineModel<boolean>('enabled', { required: true })
const threshold = defineModel<string>('threshold', { required: true })
const windowSeconds = defineModel<string>('windowSeconds', { required: true })
const probeIntervalSeconds = defineModel<string>('probeIntervalSeconds', { required: true })
const ladder = defineModel<string>('ladder', { required: true })
const probeModel = defineModel<string>('probeModel', { required: true })
const ladderHintId = useId()
</script>

<template>
  <BaseCard
    title="降智下线"
    description="上游实际返回的模型低于请求档位时，暂停账号调度并按间隔探测恢复"
  >
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseSwitch
        v-model="enabled"
        class="col-span-full justify-self-start"
        label="启用降智下线"
        show-label
      />

      <BaseFormItem
        label="降智次数阈值"
        description="统计窗口内累计降智达到此次数后下线；返回模型正常会清空计数"
      >
        <BaseInput
          v-model="threshold"
          :disabled="!enabled"
          aria-label="降智次数阈值"
          type="number"
          min="1"
          max="1000"
          step="1"
        >
          <template #prefix>
            <TrendingDown class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">次</span>
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="统计窗口"
        description="每次降智观测后，计数有效期向后顺延"
      >
        <BaseInput
          v-model="windowSeconds"
          :disabled="!enabled"
          aria-label="统计窗口秒数"
          type="number"
          min="60"
          max="3600"
          step="1"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">秒</span>
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="探测间隔"
        description="下线后每隔此时长探测一次，返回模型回升才重新上线"
      >
        <BaseInput
          v-model="probeIntervalSeconds"
          :disabled="!enabled"
          aria-label="探测间隔秒数"
          type="number"
          min="300"
          max="604800"
          step="1"
        >
          <template #prefix>
            <Timer class="size-4" />
          </template>
          <template #suffix>
            <span class="text-cp-sm">秒</span>
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        label="探测模型"
        description="留空时使用档位表最高档，最能暴露降智"
      >
        <BaseInput
          v-model="probeModel"
          :disabled="!enabled"
          aria-label="降智探测模型"
          placeholder="留空使用最高档"
        >
          <template #prefix>
            <Gauge class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>

      <BaseFormItem
        class="col-span-full"
        label="模型档位表"
        description="逗号分隔，最高档在前；只有请求与返回模型都在表内才做比较"
      >
        <template #extra>
          <BasePopover class="-my-1" trigger="hover-click" placement="top-start" :hover-delay="240">
            <template #trigger="{ open }">
              <button
                type="button"
                class="inline-flex size-6 shrink-0 cursor-pointer items-center justify-center rounded-cp-sm border-0 bg-transparent p-0 text-cp-text-tertiary outline-none transition-colors hover:text-cp-text focus-visible:ring-2 focus-visible:ring-cp-control-outline motion-reduce:transition-none"
                aria-label="模型档位表说明"
                :aria-expanded="open"
                :aria-describedby="open ? ladderHintId : undefined"
              >
                <CircleAlert class="size-3.5" aria-hidden="true" />
              </button>
            </template>
            <p :id="ladderHintId" role="tooltip" class="m-0 max-w-72 px-3 py-2 text-cp-sm leading-relaxed text-cp-text-secondary">
              表外模型一律不判定，客户端正常请求低档模型不会被误伤
            </p>
          </BasePopover>
        </template>
        <BaseInput
          v-model="ladder"
          :disabled="!enabled"
          aria-label="模型档位表"
          placeholder="gpt-6-astra, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna"
        >
          <template #prefix>
            <ListOrdered class="size-4" />
          </template>
        </BaseInput>
      </BaseFormItem>
    </BaseForm>
  </BaseCard>
</template>
