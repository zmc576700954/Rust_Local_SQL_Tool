// 展示层敏感信息清洗：唯一实现。
//
// 规则与 core_lib::ops::inspection::redact::scrub_text 保持一致：
// 只删除凭据本体（密码、token/secret/authorization 的值、AK/SK），
// 保留账号、主机、端口、库名、驱动错误码、SQLSTATE 与错误原因。
// 两侧共用 web-ui/src/utils/__fixtures__/redact-vectors.json，新增规则必须同时改这份实现与 Rust 实现。
//
// 覆盖场景：历史缓存与旧版本数据在界面/控制台展示前兜底清洗。
// 后端已在落库、接口与通知发送前清洗，这里不替代后端防线。

const CUT = '\u001f'

const DELIMITER_CHARS = new Set([
  '"',
  "'",
  '<',
  '>',
  ',',
  ';',
  ')',
  '(',
  ']',
  '[',
  '}',
  '{',
  '，',
  '。',
  '；',
  '）',
  '（',
  '、',
])

/** 与 Rust 侧 is_delimiter 一致：空白与常见包裹/分隔符。 */
function isDelimiter(value: string | undefined): boolean {
  if (value === undefined) return false
  return /\s/.test(value) || DELIMITER_CHARS.has(value)
}

function isWhitespace(value: string | undefined): boolean {
  return value !== undefined && /\s/.test(value)
}

/** 凭据键名，按长度从长到短排列，保证 `secret_access_key` 先于 `secret` 匹配。 */
const SECRET_KEYS = [
  'secret_access_key',
  'access_key_id',
  'access_key',
  'secret_key',
  'api_key',
  'authorization',
  'password',
  'passwd',
  'pwd',
  'apikey',
  'accesskey',
  'secretkey',
  'token',
  'secret',
  'ak',
  'sk',
] as const

/** 密码键：值若是 YES/NO 这类“是否提供密码”的诊断信息，必须保留。 */
const PASSWORD_KEYS = new Set(['password', 'passwd', 'pwd'])

function isPasswordKey(key: string): boolean {
  return PASSWORD_KEYS.has(key)
}

/** `using password: YES` 这类诊断值不是凭据本体。 */
function isPasswordPresenceDiagnostic(value: string): boolean {
  return ['YES', 'NO', 'TRUE', 'FALSE'].includes(value.toUpperCase())
}

/** 常见“标签”词：删除其后的连接串/凭据后，这些词会变成断头句。 */
const DANGLING_LABELS = [
  'connection string',
  'authorization',
  'connection',
  'hostname',
  'username',
  'password',
  'passwd',
  'secret',
  'server',
  'endpoint',
  'address',
  'token',
  'bearer',
  'host',
  'user',
  'basic',
  'pwd',
  'url',
  'uri',
  'dsn',
  'addr',
  '数据库地址',
  '连接串',
  '连接地址',
  '服务地址',
  '连接',
  '地址',
  '主机',
  '参数',
  '密码',
  '口令',
  '用户名',
  '账号',
]

function separator(value: string): boolean {
  return [';', ',', ':', '；', '，', '、'].includes(value)
}

function openingBracket(value: string): boolean {
  return ['(', '[', '{', '（', '【', '《'].includes(value)
}

function closingBracket(value: string): boolean {
  return [')', ']', '}', '）', '】', '》'].includes(value)
}

/**
 * 只删除 `user:pass@` 里的 `:pass`，保留账号、主机、端口与库名。
 *
 * 覆盖 `scheme://user:pass@host:port/db` 与裸 `user:pass@host` 两种形态；
 * 邮箱（`dba@example.com`）与驱动错误里的 `'shop'@'localhost'` 不受影响。
 */
function stripUserinfoPasswords(input: string): string {
  const chars = Array.from(input)
  let index = 0
  while (index < chars.length) {
    if (chars[index] !== '@') {
      index += 1
      continue
    }
    let start = index
    while (start > 0 && !isDelimiter(chars[start - 1])) {
      start -= 1
    }
    // URI 的 scheme 里也有冒号，真正的 userinfo 从最后一个 `://` 之后开始。
    let userStart = start
    for (let probe = start; probe + 2 < index; probe += 1) {
      if (chars[probe] === ':' && chars[probe + 1] === '/' && chars[probe + 2] === '/') {
        userStart = probe + 3
      }
    }
    const offset = chars.slice(userStart, index).indexOf(':')
    if (offset >= 0) {
      const colon = userStart + offset
      const next = chars.slice(0, colon).join('') + chars.slice(index).join('')
      return stripUserinfoPasswords(next)
    }
    index += 1
  }
  return input
}

/** 删除 `password=...` / `token: ...` 这类键值片段（连同键名）。 */
function removeSecretAssignments(input: string): string {
  const chars = Array.from(input)
  let index = 0
  while (index < chars.length) {
    const previous = chars[index - 1]
    const boundary = index === 0 || isDelimiter(previous) || previous === '&' || previous === '?'
    if (boundary) {
      const rest = chars.slice(index, index + 24).join('').toLowerCase()
      let matched: string | undefined
      for (const key of SECRET_KEYS) {
        if (rest.startsWith(key)) {
          matched = key
          break
        }
      }
      if (matched !== undefined) {
        const key = matched
        const afterKey = index + key.length
        const afterBoundary =
          afterKey === chars.length ||
          isDelimiter(chars[afterKey]) ||
          chars[afterKey] === '=' ||
          chars[afterKey] === ':'
        if (afterBoundary) {
          let cursor = afterKey
          while (cursor < chars.length && chars[cursor] === ' ') {
            cursor += 1
          }
          if (cursor < chars.length && (chars[cursor] === '=' || chars[cursor] === ':')) {
            let end = cursor + 1
            while (end < chars.length && chars[end] === ' ') {
              end += 1
            }
            if (key === 'authorization') {
              while (end < chars.length && chars[end] !== '\n') {
                end += 1
              }
            } else {
              while (end < chars.length && !isDelimiter(chars[end]) && chars[end] !== '&') {
                end += 1
              }
            }
            const value = chars.slice(cursor + 1, end).join('').trim()
            if (value === '' || (isPasswordKey(key) && isPasswordPresenceDiagnostic(value))) {
              index += 1
              continue
            }
            // URL 查询参数：把 `?password=x&next=1` 清理成 `?next=1`，
            // 不留下悬空的 `?`/`&`。
            if (index > 0 && (chars[index - 1] === '?' || chars[index - 1] === '&')) {
              const sep = chars[index - 1]
              const head = chars.slice(0, index - 1).join('')
              if (end < chars.length && chars[end] === '&') {
                return removeSecretAssignments(head + sep + chars.slice(end + 1).join(''))
              }
              return removeSecretAssignments(head + chars.slice(end).join(''))
            }
            const next = chars.slice(0, index).join('') + CUT + chars.slice(end).join('')
            return removeSecretAssignments(next)
          }
        }
      }
    }
    index += 1
  }
  return input
}

/** 找到紧邻删除标记之前的标签词，返回应删除的起始位置。 */
function danglingLabelStart(chars: string[], cutIndex: number): number | undefined {
  let end = cutIndex
  while (end > 0 && isWhitespace(chars[end - 1])) {
    end -= 1
  }
  if (end > 0 && [':', '=', '：', '＝'].includes(chars[end - 1])) {
    end -= 1
    while (end > 0 && isWhitespace(chars[end - 1])) {
      end -= 1
    }
  }
  for (const label of DANGLING_LABELS) {
    if (label.length > end) {
      continue
    }
    const start = end - label.length
    if (chars.slice(start, end).join('').toLowerCase() !== label) {
      continue
    }
    const previous = chars[start - 1]
    const boundary =
      start === 0 || isDelimiter(previous) || separator(previous) || openingBracket(previous)
    if (boundary) {
      return start
    }
  }
  return undefined
}

/** 删除切点后残留的悬空标签、空括号与多余标点。 */
function tidyCutMarkers(input: string): string {
  const chars = Array.from(input)

  // 1. 删除紧挨切点的标签词（DSN、参数：、password= 等）。
  for (;;) {
    const cutIndex = chars.indexOf(CUT)
    if (cutIndex < 0) break
    const start = danglingLabelStart(chars, cutIndex) ?? cutIndex
    chars.splice(start, cutIndex - start + 1)
  }

  // 2. 切点变成空格，避免把前后单词粘在一起。
  for (let index = 0; index < chars.length; index += 1) {
    if (chars[index] === CUT) {
      chars[index] = ' '
    }
  }

  // 3. 去掉空括号与括号内只剩标点的情况。
  for (;;) {
    let changed = false
    let index = 0
    while (index < chars.length) {
      if (openingBracket(chars[index])) {
        let end = index + 1
        while (end < chars.length && isWhitespace(chars[end])) {
          end += 1
        }
        let close = end
        while (close < chars.length && separator(chars[close])) {
          close += 1
        }
        if (close < chars.length && closingBracket(chars[close])) {
          chars.splice(index, close - index + 1)
          changed = true
          continue
        }
        if (end < chars.length && closingBracket(chars[end])) {
          chars.splice(index, end - index + 1)
          changed = true
          continue
        }
      }
      index += 1
    }
    if (!changed) break
  }

  // 4. 标点贴括号/句号时视为空段，去掉标点。
  let index = 0
  while (index < chars.length) {
    if (separator(chars[index])) {
      let next = index + 1
      while (next < chars.length && isWhitespace(chars[next])) {
        next += 1
      }
      let beforeOpen = false
      if (index > 0) {
        if (openingBracket(chars[index - 1])) {
          beforeOpen = true
        } else {
          for (let probe = index - 1; probe >= 0; probe -= 1) {
            if (!isWhitespace(chars[probe])) {
              beforeOpen = openingBracket(chars[probe])
              break
            }
          }
        }
      }
      const beforeClose = next < chars.length && closingBracket(chars[next])
      const beforeEnd = next >= chars.length
      const afterSentence = next < chars.length && (chars[next] === '。' || chars[next] === '.')
      if (beforeOpen || beforeClose || beforeEnd || afterSentence) {
        chars.splice(index, 1)
        continue
      }
    }
    index += 1
  }

  // 5. 连续标点只保留一个，但不改动正常的“逗号+空格”。
  index = 0
  while (index < chars.length) {
    if (separator(chars[index])) {
      let probe = index + 1
      while (probe < chars.length && isWhitespace(chars[probe])) {
        probe += 1
      }
      if (probe < chars.length && separator(chars[probe])) {
        let end = probe + 1
        while (end < chars.length && (isWhitespace(chars[end]) || separator(chars[end]))) {
          end += 1
        }
        chars.splice(index + 1, end - (index + 1))
      }
    }
    index += 1
  }

  // 6. 合并空格（保留换行）。
  index = 0
  while (index < chars.length) {
    if (chars[index] === ' ' || chars[index] === '\t') {
      let end = index + 1
      while (end < chars.length && (chars[end] === ' ' || chars[end] === '\t')) {
        end += 1
      }
      if (end > index + 1) {
        chars.splice(index + 1, end - (index + 1))
      }
    }
    index += 1
  }

  let start = 0
  while (start < chars.length && (isWhitespace(chars[start]) || separator(chars[start]))) {
    start += 1
  }
  let end = chars.length
  while (end > 0 && (isWhitespace(chars[end - 1]) || separator(chars[end - 1]))) {
    end -= 1
  }
  if (start >= end) {
    return ''
  }
  return chars.slice(start, end).join('')
}

export interface ScrubResult {
  text: string
  changed: boolean
}

/**
 * 强制清洗文本，返回清洗结果与是否发生变化。
 *
 * 与 `core_lib::ops::inspection::redact::scrub_text` 行为一致：
 * 只删除凭据本体，保留驱动错误码、原因、账号、主机、端口与库名。
 */
export function scrubText(input: string, secrets: readonly string[] = []): ScrubResult {
  let output = input
  for (const secret of secrets) {
    if (secret !== '') {
      output = output.split(secret).join(CUT)
    }
  }
  output = stripUserinfoPasswords(output)
  output = removeSecretAssignments(output)
  output = tidyCutMarkers(output)
  return { text: output.trim(), changed: output !== input }
}

/** 清洗文本；`secrets` 用于删除调用方已知的明文凭据（例如当前连接密码）。 */
export function scrubSensitiveText(input: string, secrets: readonly string[] = []): string {
  return scrubText(input, secrets).text
}

/** 是否仍包含凭据本体；用于测试与运行期断言。 */
export function containsSensitiveShape(input: string): boolean {
  return hasUserinfoPassword(input) || hasSecretAssignment(input)
}

/** `scheme://user:pass@host` 或裸 `user:pass@host` 里是否仍带密码。 */
function hasUserinfoPassword(text: string): boolean {
  const chars = Array.from(text)
  for (let index = 0; index < chars.length; index += 1) {
    if (chars[index] !== '@') {
      continue
    }
    let start = index
    while (start > 0 && !isDelimiter(chars[start - 1])) {
      start -= 1
    }
    let userStart = start
    for (let probe = start; probe + 2 < index; probe += 1) {
      if (chars[probe] === ':' && chars[probe + 1] === '/' && chars[probe + 2] === '/') {
        userStart = probe + 3
      }
    }
    if (chars.slice(userStart, index).includes(':')) {
      return true
    }
  }
  return false
}

/** 是否仍存在带非空值（且不是诊断值）的凭据键值。 */
function hasSecretAssignment(text: string): boolean {
  const chars = Array.from(text)
  let index = 0
  while (index < chars.length) {
    const previous = chars[index - 1]
    const boundary = index === 0 || isDelimiter(previous) || previous === '&' || previous === '?'
    if (boundary) {
      const rest = chars.slice(index, index + 24).join('').toLowerCase()
      for (const key of SECRET_KEYS) {
        if (!rest.startsWith(key)) {
          continue
        }
        const afterKey = index + key.length
        const afterBoundary =
          afterKey === chars.length ||
          isDelimiter(chars[afterKey]) ||
          chars[afterKey] === '=' ||
          chars[afterKey] === ':'
        if (!afterBoundary) {
          continue
        }
        let cursor = afterKey
        while (cursor < chars.length && chars[cursor] === ' ') {
          cursor += 1
        }
        if (cursor >= chars.length || (chars[cursor] !== '=' && chars[cursor] !== ':')) {
          continue
        }
        let end = cursor + 1
        while (end < chars.length && chars[end] === ' ') {
          end += 1
        }
        if (key === 'authorization') {
          while (end < chars.length && chars[end] !== '\n') {
            end += 1
          }
        } else {
          while (end < chars.length && !isDelimiter(chars[end]) && chars[end] !== '&') {
            end += 1
          }
        }
        const value = chars.slice(cursor + 1, end).join('').trim()
        if (value === '' || (isPasswordKey(key) && isPasswordPresenceDiagnostic(value))) {
          continue
        }
        return true
      }
    }
    index += 1
  }
  return false
}
