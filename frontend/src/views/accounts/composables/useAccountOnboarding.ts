import type { AccountImportTask, getAccounts } from '@/api'
import { toast } from '@codex-proxy/ui'
import { computed, ref, shallowRef, watch } from 'vue'
import { createAccountImportTask, importAccounts } from '@/api'
import { useAsyncAction } from '@/composables/useAsyncAction'
import { errorMessage } from '@/utils/async'
import { parseJsonObject } from '@/utils/jsonObject'
import { formatProviderLabel, isSupportedProvider } from '@/utils/providers'
import { generateRequestId } from '@/utils/uuid'
import { accountCreateProvider, accountCreateSourceKey, accountImportSettings, accountProxyError, emptyAccountCreateForm } from '../components/AccountCreateModal/model'
import { accountImportModes } from '../components/AccountCreateModal/presenter'
import { accountImportDocuments, MAX_ACCOUNT_IMPORT_COUNT, mixedImportDocuments } from '../utils/accountImport'
import { apiKeyAccountError, emptyApiKeyAccountForm } from '../utils/upstreamApiKey'
import { useAccountAuthorization } from './useAccountAuthorization'
import { useAccountProviders } from './useAccountProviders'

type AccountRow = Awaited<ReturnType<typeof getAccounts>>['items'][number]

export function useAccountOnboarding(options: {
  reload: () => Promise<unknown>
  onImportTaskCreated: (task: AccountImportTask) => void
}) {
  const catalog = useAccountProviders()
  const createModalOpen = shallowRef(false)
  const reauthorizingAccount = shallowRef<AccountRow | null>(null)
  const creatingAccountAction = useAsyncAction()
  const createForm = ref(emptyAccountCreateForm())
  const authorization = useAccountAuthorization(() => finishCreate(reauthorizingAccount.value ? '账号重新授权成功' : '账号已添加'))
  const selectedProvider = computed(() => catalog.byId.value.get(accountCreateProvider(createForm.value) ?? ''))
  let submissionId: string | undefined
  watch(createForm, () => {
    submissionId = undefined
  }, { deep: true, flush: 'sync' })

  const showCreateModal = computed({
    get: () => createModalOpen.value,
    set: (value: boolean) => {
      createModalOpen.value = value
      if (!value) {
        authorization.reset()
        reauthorizingAccount.value = null
        createForm.value = emptyAccountCreateForm()
      }
    },
  })

  function requireImportProvider(provider: string | undefined) {
    if (!provider)
      throw new Error('请选择账号平台')
    if (catalog.loading.value || !catalog.byId.value.get(provider)?.credentials.import)
      throw new Error(`${formatProviderLabel(provider)} 当前不可导入，请刷新平台列表`)
    return provider
  }

  async function handleCreate() {
    if (createForm.value.mode === 'oauth') {
      await authorization.complete()
      return
    }
    await creatingAccountAction.run(async () => {
      const form = createForm.value
      const proxyError = accountProxyError(form)
      if (proxyError)
        throw new Error(proxyError)
      const settings = accountImportSettings(form)
      const outboundProxyId = form.proxyMode === 'proxy' ? form.proxyId.trim() : undefined
      const mode = form.mode
      if (mode === 'oauth')
        return
      const provider = accountCreateProvider(form)
      if (mode === 'api_key') {
        if (requireImportProvider(provider) !== 'openai')
          throw new Error('当前平台不支持 API Key 账号')
        const error = apiKeyAccountError(form.apiKey)
        if (error)
          throw new Error(error)
        await importAccounts({
          provider: 'openai',
          settings,
          outboundProxyId,
          data: { provider: 'openai', authentication_kind: 'api_key', name: form.apiKey.name.trim(), base_url: form.apiKey.base_url.trim(), api_key: form.apiKey.apiKey, transport: form.apiKey.transport },
        })
        await finishCreate('API Key 账号已添加')
        return
      }
      const documents = form.source?.kind === 'bundle'
        ? mixedImportDocuments(form.importTexts.json)
        : mode === 'form'
          ? [{ provider: requireImportProvider(provider), document: parseJsonObject(form.importInput) }]
          : accountImportDocuments(requireImportProvider(provider), mode, form.importTexts[mode])
      if (documents.length > MAX_ACCOUNT_IMPORT_COUNT)
        throw new Error(`单次最多导入 ${MAX_ACCOUNT_IMPORT_COUNT} 个条目`)
      for (const document of documents)
        requireImportProvider(document.provider)
      submissionId ??= generateRequestId()
      const task = await createAccountImportTask({
        submissionId,
        items: documents.map(entry => ({ provider: entry.provider, data: entry.document, settings, outboundProxyId })),
      })
      showCreateModal.value = false
      options.onImportTaskCreated(task)
      toast.success('导入任务已创建')
    })
  }

  async function handleAuthorizeOAuth() {
    if (authorization.busy.value)
      return
    try {
      const form = createForm.value
      const provider = selectedProvider.value
      if (catalog.loading.value || !provider?.credentials.login)
        throw new Error('当前平台不可授权，请刷新平台列表')
      const account = reauthorizingAccount.value
      const proxyError = accountProxyError(form)
      if (proxyError)
        throw new Error(proxyError)
      await authorization.start({
        start: {
          provider: provider.provider,
          name: account?.name || account?.email || `${formatProviderLabel(provider.provider)} 账号`,
          accountId: account?.id,
          outboundProxyId: account ? undefined : form.proxyMode === 'proxy' ? form.proxyId.trim() : undefined,
          input: parseJsonObject(form.loginInput, 64 * 1024),
        },
        completion: provider.credentials.login.completion,
        settings: account ? undefined : accountImportSettings(form),
      })
      if (authorization.view.value.flow)
        form.loginInput = '{}'
    }
    catch (cause) {
      toast.error(errorMessage(cause, '启动授权失败'))
    }
  }

  function openCreateAccount() {
    authorization.reset()
    reauthorizingAccount.value = null
    createForm.value = emptyAccountCreateForm()
    showCreateModal.value = true
    void catalog.load()
  }

  function openReauthorizeAccount(account: AccountRow) {
    if (isSupportedProvider(account.provider) && account.authenticationKind !== 'oauth')
      return
    authorization.reset()
    reauthorizingAccount.value = account
    createForm.value = { ...emptyAccountCreateForm(), source: { kind: 'provider', id: account.provider }, step: 'import' }
    showCreateModal.value = true
    void catalog.load()
  }

  async function finishCreate(message: string) {
    showCreateModal.value = false
    toast.success(message)
    await options.reload()
  }

  watch(() => accountCreateSourceKey(createForm.value), () => {
    createForm.value = {
      ...createForm.value,
      mode: reauthorizingAccount.value ? 'oauth' : createForm.value.source?.kind === 'bundle' ? 'json' : accountImportModes(selectedProvider.value)[0]?.value ?? 'json',
      apiKey: emptyApiKeyAccountForm(),
      importTexts: { access_token: '', refresh_token: '', json: '' },
      loginInput: '{}',
      importInput: '{}',
    }
  }, { flush: 'sync' })

  watch([
    () => accountCreateSourceKey(createForm.value),
    () => createForm.value.mode,
    () => createForm.value.step,
    () => createForm.value.proxyMode,
    () => createForm.value.proxyMode === 'proxy' ? createForm.value.proxyId.trim() : '',
  ], () => {
    authorization.reset()
    createForm.value.loginInput = '{}'
  }, { flush: 'sync' })

  return {
    showCreateModal,
    reauthorizingAccount,
    creatingAccount: creatingAccountAction.loading,
    authorizingOAuth: authorization.busy,
    authorization: authorization.view,
    authorizationCallback: authorization.callback,
    resetAuthorization: authorization.reset,
    accountProviders: catalog.providers,
    accountProvidersById: catalog.byId,
    accountProvidersLoading: catalog.loading,
    accountProvidersError: catalog.error,
    loadAccountProviders: catalog.load,
    createForm,
    handleCreate,
    handleAuthorizeOAuth,
    openCreateAccount,
    openReauthorizeAccount,
  }
}
