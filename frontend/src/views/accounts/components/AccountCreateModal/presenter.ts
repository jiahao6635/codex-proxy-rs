import type { AccountAuthorizationView } from '../../composables/useAccountAuthorization'
import type { AccountCreateForm, AccountImportMode } from './model'
import type { AccountProvider } from '@/api'
import { jsonObjectError } from '@/utils/jsonObject'
import { formatProviderLabel } from '@/utils/providers'
import { apiKeyAccountError } from '../../utils/upstreamApiKey'
import { accountCreateProvider } from './model'

export function accountImportModes(provider: AccountProvider | undefined) {
  const options: Array<{ label: string, value: AccountImportMode }> = []
  if (provider?.credentials.login)
    options.push({ label: provider.credentials.login.completion === 'callback' ? 'OAuth' : '登录', value: 'oauth' })
  if (provider?.credentials.import) {
    if (provider.provider === 'openai') {
      options.push(
        { label: 'API Key', value: 'api_key' },
        { label: 'AT', value: 'access_token' },
        { label: 'RT', value: 'refresh_token' },
      )
    }
    else if (provider.provider !== 'xai') {
      options.push({ label: '填写凭据', value: 'form' })
    }
    options.push({ label: '账号文件', value: 'json' })
  }
  return options
}

export function resolveAccountCreatePresentation(input: {
  form: AccountCreateForm
  provider: AccountProvider | undefined
  authorization: AccountAuthorizationView
  callback: string
  busy: boolean
  reauthorizing: boolean
}) {
  const { form, provider, authorization } = input
  const id = accountCreateProvider(form)
  const label = formatProviderLabel(id)
  const isBatch = form.source?.kind === 'bundle'
  const configuring = !input.reauthorizing && form.step === 'settings'
  const modes = accountImportModes(provider)
  const available = isBatch || (form.mode === 'oauth' && Boolean(authorization.flow)) || modes.some(mode => mode.value === form.mode)
  const poll = (authorization.completion ?? provider?.credentials.login?.completion) === 'poll'
  let description = '粘贴或上传账号 JSON，匹配已有账号时更新凭据'
  if (isBatch)
    description = '粘贴或上传 CPR 账号包，一次导入多个平台账号'
  else if (form.mode === 'oauth')
    description = `通过浏览器授权导入 ${label} 账号`
  else if (form.mode === 'api_key')
    description = '接入 OpenAI Responses 兼容上游'
  else if (form.mode === 'form')
    description = `填写 ${label} 账号凭据`
  else if (form.mode === 'access_token')
    description = '逐行粘贴 Access Token，未包含 Refresh Token 时无法自动续期'
  else if (form.mode === 'refresh_token')
    description = '逐行粘贴 Refresh Token，导入时将自动换取 Access Token'

  let canSubmit = available && !input.busy
  if (form.mode === 'oauth')
    canSubmit &&= Boolean(authorization.flow && (poll || input.callback.trim()) && authorization.status !== 'polling')
  else if (form.mode === 'api_key')
    canSubmit &&= !apiKeyAccountError(form.apiKey)
  else if (form.mode === 'form')
    canSubmit &&= !jsonObjectError(form.importInput)
  else
    canSubmit &&= Boolean(form.importTexts[form.mode].trim())

  return {
    configuring,
    isBatch,
    provider: id,
    label,
    modeOptions: modes,
    available,
    modal: {
      title: configuring ? '账号设置' : input.reauthorizing ? '重新授权账号' : '导入账号',
      description: configuring ? '设置将应用于本次导入的账号' : input.reauthorizing ? '完成授权后更新账号凭据' : description,
      tone: configuring ? 'neutral' as const : 'info' as const,
      size: configuring ? 'md-wide' as const : 'md' as const,
    },
    oauth: {
      poll,
      callbackLabel: poll ? '回调地址或授权码（可选）' : id === 'xai' ? '回调地址或授权码' : '回调地址',
      callbackPlaceholder: id === 'openai' ? 'http://localhost:1455/auth/callback?code=...&state=...' : '回调地址、?code=...&state=... 或授权码',
      description: poll ? '打开授权链接，完成后自动检查结果' : '打开授权链接，完成浏览器授权后粘贴回调地址或授权码',
    },
    importInput: form.mode === 'access_token'
      ? { label: 'Access Token', placeholder: '每行粘贴一个 Access Token', uploadable: false }
      : form.mode === 'refresh_token'
        ? { label: 'Refresh Token', placeholder: '每行粘贴一个 Refresh Token', uploadable: false }
        : { label: isBatch ? '批量账号文件' : '账号文件', placeholder: isBatch ? '粘贴 CPR 多平台导出文件内容' : '粘贴账号 JSON 内容', uploadable: true },
    canSubmit,
    submitLabel: form.mode === 'oauth'
      ? poll ? '检查授权结果' : input.reauthorizing ? '完成重新授权' : '完成导入'
      : isBatch ? '批量导入' : '导入账号',
  }
}
