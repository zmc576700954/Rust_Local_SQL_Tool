// 展示层敏感信息清洗：删除连接串与凭据形态，而不是替换成 *** 保留结构。
// 后端已在落库、接口与通知发送前清洗；这里覆盖历史缓存与旧版本数据。

const TOKEN_BREAK = String.raw`[^\s"'<>,;()[\]{}，。；（）（）、]`
const TOKEN_BREAK_STAR = String.raw`[^\s"'<>,;()[\]{}，。；（）（）、]*`
const SECRET_KEY = String.raw`(password|passwd|pwd|token|secret|authorization)`

const URI_PATTERN = new RegExp(String.raw`[A-Za-z][A-Za-z0-9+.-]*://${TOKEN_BREAK}*`, 'g')
const USERINFO_PATTERN = new RegExp(`${TOKEN_BREAK_STAR}:${TOKEN_BREAK_STAR}@${TOKEN_BREAK}*`, 'g')
const AT_HOST_PORT_PATTERN = new RegExp(String.raw`@[A-Za-z0-9._-]+:\d+${TOKEN_BREAK}*`, 'g')
const SECRET_ASSIGNMENT_PATTERN = new RegExp(
  String.raw`\b${SECRET_KEY}\s*[:=]\s*${TOKEN_BREAK}*`,
  'gi'
)

/** 删除 `scheme://`、`user:pass@`、`@host:port`、`password=` 形态的片段。 */
export function scrubSensitiveText(input: string): string {
  let output = input
  output = output.replace(URI_PATTERN, '')
  output = output.replace(USERINFO_PATTERN, '')
  output = output.replace(AT_HOST_PORT_PATTERN, '')
  output = output.replace(SECRET_ASSIGNMENT_PATTERN, '')
  return output.trim()
}

/** 是否仍包含需要删除的敏感形态；用于断言与调试。 */
export function containsSensitiveShape(input: string): boolean {
  if (input.includes('://')) return true
  if (/\b(password|passwd|pwd)\s*[:=]/i.test(input)) return true
  if (/@[A-Za-z0-9._-]+:\d+/.test(input)) return true
  return new RegExp(`${TOKEN_BREAK_STAR}:${TOKEN_BREAK_STAR}@`).test(input)
}
