import { describe, expect, it } from 'vitest'
import { containsSensitiveShape, scrubSensitiveText } from './redact'

const SENSITIVE_SHAPES = ['://', 'password=', '@127.0.0.1:3306', 'user:pass@']

describe('redact', () => {
  it('删除 DSN，保留库名与对象名', () => {
    const raw =
      '库 shop 的表 orders 索引 idx_created 异常；参考 mysql://root:rootpass@127.0.0.1:3306/shop?password=plain123'
    const clean = scrubSensitiveText(raw)
    expect(containsSensitiveShape(clean)).toBe(false)
    expect(clean).toContain('shop')
    expect(clean).toContain('orders')
    expect(clean).toContain('idx_created')
    for (const shape of SENSITIVE_SHAPES) {
      expect(clean).not.toContain(shape)
    }
    expect(clean).not.toContain('***')
  })

  it('删除裸 userinfo 与 @host:port', () => {
    const clean = scrubSensitiveText('user:pass@db.internal:3306/app 与 ops@10.0.0.5:5432 不可用')
    expect(containsSensitiveShape(clean)).toBe(false)
    expect(clean).not.toContain('db.internal:3306')
    expect(clean).not.toContain('10.0.0.5:5432')
  })

  it('不误伤普通邮箱与对象名', () => {
    const raw = '表 orders 的索引 idx_orders_created_at 连续 3 次为 0，联系人 dba@example.com'
    const clean = scrubSensitiveText(raw)
    expect(clean).toBe(raw)
    expect(containsSensitiveShape(clean)).toBe(false)
  })

  it('按形态断言而不是按具体口令', () => {
    const clean = scrubSensitiveText('postgres://someone:secret@db:5432/app password=whatever')
    expect(containsSensitiveShape(clean)).toBe(false)
    for (const shape of ['postgres://', 'password=', '@db:5432', 'someone:secret@']) {
      expect(clean).not.toContain(shape)
    }
  })
})
