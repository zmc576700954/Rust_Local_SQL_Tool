import { describe, expect, it } from 'vitest'
import { parseError, redactSensitiveText } from './utils'
import { getErrorMessage } from './utils/ErrorDictionary'

describe('parseError', () => {
  it('maps Network Error to backend connectivity message', () => {
    const err = parseError({ message: 'Network Error' })
    expect(err.title).toContain('Network Error')
    expect(err.message).toContain('/backend')
  })

  it('maps ECONNABORTED to Timeout', () => {
    const err = parseError({ code: 'ECONNABORTED' })
    expect(err.title).toContain('Timeout')
    expect(err.message).toContain('请求超时')
  })

  it('parses JSON string error body when possible', () => {
    const err = parseError({
      response: { data: JSON.stringify({ message: 'invalid session token' }) },
    })
    expect(err.title).toContain('Auth')
  })
})

describe('ErrorDictionary', () => {
  it('maps backend error codes to stable Chinese messages', () => {
    expect(getErrorMessage('ERR_AI_RATE_LIMITED', 'x')).toContain('限流')
    expect(getErrorMessage('ERR_AI_PROXY', 'x')).toContain('代理')
    expect(getErrorMessage('ERR_EXTERNAL_UNAVAILABLE', 'x')).toContain('外部')
    expect(getErrorMessage('ERR_TIMEOUT', 'x')).toContain('超时')
  })
})

describe('redactSensitiveText', () => {
  it('redacts Authorization/api_key/password/db url password', () => {
    const s = 'Authorization: Bearer sk-abc api_key=kk password=pp mysql://u:p@host/db'
    const r = redactSensitiveText(s)
    expect(r).not.toContain('sk-abc')
    expect(r).not.toContain('api_key=kk')
    expect(r).not.toContain('password=pp')
    expect(r).not.toContain('mysql://u:p@')
    expect(r).toContain('Bearer ******')
    expect(r).toContain('mysql://u:******@')
  })
})
