import { isRecord } from '@/utils/object'

export function parseJsonObject(text: string, maximumBytes?: number): Record<string, unknown> {
  let value: unknown
  try {
    value = JSON.parse(text)
  }
  catch {
    throw new Error('请输入有效的 JSON 对象')
  }
  if (!isRecord(value))
    throw new Error('输入必须是 JSON 对象')
  if (maximumBytes !== undefined && new TextEncoder().encode(JSON.stringify(value)).byteLength > maximumBytes)
    throw new Error(`输入不能超过 ${maximumBytes / 1024} KiB`)
  return value
}

export function jsonObjectError(text: string, maximumBytes?: number) {
  try {
    parseJsonObject(text, maximumBytes)
    return undefined
  }
  catch (error) {
    return (error as Error).message
  }
}
