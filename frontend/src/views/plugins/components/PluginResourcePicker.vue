<script setup lang="ts">
import { BaseButton, BaseCheckbox, BaseInput, BasePopover, BaseScrollbar, toast } from '@codex-proxy/ui'
import { ChevronDown } from '@lucide/vue'
import { useElementSize, useEventListener } from '@vueuse/core'
import { computed, nextTick, onScopeDispose, shallowRef, useId, useTemplateRef, watch } from 'vue'
import { getAccountGroups, getApiKeys } from '@/api'
import { errorMessage } from '@/utils/async'

const props = defineProps<{ kind: 'keys' | 'groups', disabled?: boolean, single?: boolean }>()
const selected = defineModel<string[]>({ required: true })
const options = shallowRef<{ id: string, label: string, disabled: boolean }[]>([])
const loading = shallowRef(false)
const error = shallowRef('')
const search = shallowRef('')
const open = shallowRef(false)
const trigger = useTemplateRef<HTMLButtonElement>('trigger')
const { width: triggerWidth } = useElementSize(trigger, undefined, { box: 'border-box' })
const panel = useTemplateRef<HTMLDivElement>('panel')
const panelId = useId()
const label = computed(() => props.kind === 'keys' ? '客户端 Key' : '账号分组')
let controller: AbortController | undefined
const choices = computed(() => {
  const known = new Set(options.value.map(option => option.id))
  return [...selected.value.filter(id => !known.has(id)).map(id => ({ id, label: `不可用 · ${id}`, disabled: true })), ...options.value]
})
const displayed = computed(() => choices.value.filter(option => option.label.toLocaleLowerCase().includes(search.value.toLocaleLowerCase())))
const selectionLabel = computed(() => {
  if (selected.value.length)
    return choices.value.find(option => option.id === selected.value[0])?.label ?? selected.value[0]
  if (loading.value)
    return '正在加载…'
  if (error.value)
    return '加载失败'
  return options.value.length ? '不限' : `暂无${label.value}`
})
async function load() {
  controller?.abort()
  const current = new AbortController()
  controller = current
  loading.value = true
  error.value = ''
  const items: typeof options.value = []
  try {
    const requestOptions = { signal: current.signal, silent: true }
    if (props.kind === 'keys') {
      let cursor: string | undefined
      do {
        const result = await getApiKeys({ limit: 200, cursor }, requestOptions)
        items.push(...result.items.map(item => ({ id: item.id, label: `${item.name} · ${item.prefix}`, disabled: !item.enabled })))
        cursor = result.nextCursor ?? undefined
      } while (cursor)
    }
    else {
      let page = 1
      let totalPages = 1
      do {
        const result = await getAccountGroups({ page, pageSize: 100 }, requestOptions)
        items.push(...result.items.map(item => ({ id: item.id, label: item.name, disabled: !item.enabled })))
        totalPages = result.page.totalPages
        page++
      } while (page <= totalPages)
    }
    if (controller === current)
      options.value = items
  }
  catch (cause) {
    if (!current.signal.aborted) {
      error.value = errorMessage(cause)
      // 同一弹窗可能挂载多个范围选择器，只显示一份相同错误。
      if (!toast.messages.some(message => message.type === 'error' && message.message === error.value))
        toast.error(error.value)
    }
  }
  finally {
    if (controller === current)
      loading.value = false
  }
}
function toggle(id: string, checked: boolean) {
  selected.value = checked ? (props.single ? [id] : [...selected.value, id]) : selected.value.filter(value => value !== id)
}
function handleKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape' || !open.value)
    return
  event.preventDefault()
  event.stopPropagation()
  open.value = false
  trigger.value?.focus()
}
useEventListener(panel, 'keydown', handleKeydown)
watch(open, async (value) => {
  if (!value)
    return
  search.value = ''
  await nextTick()
  if (open.value)
    (panel.value?.querySelector<HTMLElement>('input:not(:disabled), button:not(:disabled)') ?? panel.value)?.focus()
})
watch(() => props.disabled, (value) => {
  if (value)
    open.value = false
})
watch(() => props.kind, load, { immediate: true })
onScopeDispose(() => controller?.abort())
</script>

<template>
  <div class="grid min-w-0 gap-2">
    <BasePopover v-model="open" placement="bottom-start" :disabled="disabled" class="w-full min-w-0">
      <template #trigger>
        <button
          ref="trigger"
          type="button"
          :disabled="disabled"
          :aria-label="`${label}：${selectionLabel}${selected.length > 1 ? `，共 ${selected.length} 项` : ''}`"
          aria-haspopup="dialog"
          :aria-expanded="open"
          :aria-controls="open ? panelId : undefined"
          class="flex h-cp-control w-full min-w-0 items-center gap-2 rounded-cp border-0 px-3.5 text-left text-cp shadow-cp-input outline-none transition-[background-color,box-shadow] duration-160 motion-reduce:transition-none"
          :class="disabled
            ? 'cursor-not-allowed bg-cp-bg-container-disabled text-cp-text-disabled shadow-none'
            : open
              ? 'cursor-pointer bg-(--cp-input-active-bg) text-cp-text shadow-cp-input-active'
              : 'cursor-pointer bg-(--cp-input-bg) text-cp-text hover:bg-(--cp-input-hover-bg) hover:shadow-cp-input-hover focus-visible:bg-(--cp-input-active-bg) focus-visible:shadow-cp-input-active'"
          @keydown="handleKeydown"
        >
          <span class="min-w-0 flex-1 truncate" :class="selected.length ? 'font-emphasis' : 'text-cp-text-quaternary'">{{ selectionLabel }}</span>
          <span v-if="selected.length > 1" class="shrink-0 text-cp-xs text-cp-text-secondary">+{{ selected.length - 1 }}</span>
          <ChevronDown class="size-4 shrink-0 text-cp-text-quaternary" :class="{ 'rotate-180': open }" />
        </button>
      </template>
      <div :id="panelId" ref="panel" role="dialog" :aria-label="`选择${label}`" tabindex="-1" :style="{ width: `${triggerWidth}px` }" class="max-w-[calc(100vw-2rem)] p-2 outline-none">
        <BaseInput v-if="choices.length > 6" v-model="search" :disabled="disabled" size="sm" :aria-label="`搜索${label}`" placeholder="输入名称筛选" class="mb-2" />
        <p v-if="loading" role="status" class="m-0 p-3 text-cp-xs text-cp-text-secondary">
          正在加载…
        </p>
        <div v-else-if="error" class="flex items-center gap-2 p-2 text-cp-xs text-cp-error-text">
          加载失败
          <BaseButton size="sm" variant="secondary" :disabled="disabled" @click="load">
            重试
          </BaseButton>
        </div>
        <BaseScrollbar v-else max-height="min(15rem, 50dvh)">
          <div class="grid gap-1">
            <BaseCheckbox
              v-for="option in displayed"
              :key="option.id"
              :model-value="selected.includes(option.id)"
              :label="`${option.label}${option.disabled ? '（不可用）' : ''}`"
              show-label
              :disabled="disabled || (option.disabled && !selected.includes(option.id))"
              class="min-w-0 rounded-cp px-3 py-2.5"
              :class="selected.includes(option.id) ? 'bg-cp-primary-container' : 'hover:bg-cp-fill-quaternary'"
              @update:model-value="toggle(option.id, $event)"
            >
              <template #label>
                <span class="block truncate" :title="option.label">{{ option.label }}{{ option.disabled ? '（不可用）' : '' }}</span>
              </template>
            </BaseCheckbox>
            <p v-if="!displayed.length" class="m-0 p-3 text-cp-xs text-cp-text-secondary">
              {{ choices.length ? '没有匹配项' : `暂无${label}，请先在${kind === 'keys' ? 'API 密钥' : '分组管理'}中创建` }}
            </p>
          </div>
        </BaseScrollbar>
      </div>
    </BasePopover>
  </div>
</template>
