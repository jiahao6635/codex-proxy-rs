<script setup lang="ts">
import type { PluginUpdateSelection } from '../composables/usePluginUpdateCheck'
import type { InstalledPlugin } from '../utils/catalog'
import { BaseButton, BaseModal, BaseTag } from '@codex-proxy/ui'
import { LoaderCircle } from '@lucide/vue'
import { computed } from 'vue'
import PluginHelpPopover from './PluginHelpPopover.vue'

const props = defineProps<{ plugin: InstalledPlugin | null, result: PluginUpdateSelection | null, checking: boolean }>()
defineEmits<{ install: [selection: PluginUpdateSelection] }>()
const open = defineModel<boolean>({ required: true })
const installed = computed(() => props.plugin?.artifacts.some(artifact => artifact.acceptedAt && (props.result?.artifact
  ? artifact.metadata.sha256 === props.result.artifact.metadata.sha256
  : props.result?.release && artifact.source.kind === 'github' && artifact.source.repository === props.result.release.repository && artifact.source.tag === props.result.release.tag)))
const canInstall = computed(() => props.result?.artifact || props.result?.release?.assets.some(asset => /\.(?:tar\.gz|tgz)$/i.test(asset.name)))
</script>

<template>
  <BaseModal v-model="open" title="检查更新" description="仅查询来源，不安装或切换版本" size="md">
    <div v-if="checking" role="status" class="flex items-center gap-2 py-4 text-cp-sm text-cp-text-secondary">
      <LoaderCircle class="size-4 animate-spin motion-reduce:animate-none" />
      {{ plugin?.source?.source.kind === 'url' ? '正在下载并解析插件包' : '正在查询发布版本' }}
    </div>
    <div v-else-if="result" class="grid gap-3 text-cp-sm">
      <div class="flex flex-wrap items-center gap-2">
        <span class="min-w-0 flex-1 break-words">{{ plugin?.artifact.metadata.displayName }}</span>
        <BaseTag type="primary">
          {{ result.artifact?.metadata.version ?? result.release?.tag }}
        </BaseTag>
        <BaseTag v-if="result.release?.prerelease" type="warning">
          预发行版
        </BaseTag>
      </div>
      <div class="flex items-center gap-1.5 text-cp-xs text-cp-text-secondary">
        <span>{{ installed ? result.artifact ? '此版本已安装' : '此发布已有已安装版本' : result.artifact ? '发现未安装版本' : '发现未安装发布' }}</span>
        <PluginHelpPopover label="更新检查说明">
          GitHub 查询发布标签，标签不等于包内版本，继续安装时仍需校验插件包，URL 按包内版本和摘要比对，本地上传不支持检查更新
        </PluginHelpPopover>
      </div>
      <span v-if="!canInstall" class="text-cp-xs text-cp-text-secondary">此发布没有 tar.gz 或 tgz 插件包</span>
    </div>
    <template #footer>
      <BaseButton variant="secondary" @click="open = false">
        {{ checking ? '取消' : '关闭' }}
      </BaseButton>
      <BaseButton v-if="result && canInstall" variant="primary" @click="$emit('install', result)">
        {{ installed ? '查看安装选项' : '继续安装' }}
      </BaseButton>
    </template>
  </BaseModal>
</template>
