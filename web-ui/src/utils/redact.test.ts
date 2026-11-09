import { describe, expect, it } from 'vitest'
import vectorsRaw from './__fixtures__/redact-vectors.json?raw'
import { containsSensitiveShape, scrubSensitiveText } from './redact'

interface RedactVector {
  name: string
  input: string
  secrets?: string[]
  must_disappear: string[]
  must_keep: string[]
  unchanged?: boolean
}

const FIXTURE = JSON.parse(vectorsRaw) as { vectors: RedactVector[] }

describe('redact', () => {
  it('删除 DSN 与查询密码，保留账号、主机、端口、库名与对象名', () => {
    const raw =
      '库 shop 的表 orders 索引 idx_created 异常；参考 mysql://root:rootpass@127.0.0.1:3306/shop?password=plain123'
    const clean = scrubSensitiveText(raw)
    expect(containsSensitiveShape(clean)).toBe(false)
    expect(clean).toContain('mysql://root@127.0.0.1:3306/shop')
    expect(clean).toContain('shop')
    expect(clean).toContain('orders')
    expect(clean).toContain('idx_created')
    for (const leaked of ['rootpass', 'plain123', 'password=']) {
      expect(clean).not.toContain(leaked)
    }
    expect(clean).not.toContain('***')
  })

  it('删除 userinfo 密码，保留账号与 @host:port', () => {
    const clean = scrubSensitiveText('user:pass@db.internal:3306/app 与 ops@10.0.0.5:5432 不可用')
    expect(containsSensitiveShape(clean)).toBe(false)
    expect(clean).not.toContain('pass@')
    expect(clean).toContain('user@db.internal:3306/app')
    expect(clean).toContain('ops@10.0.0.5:5432')
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
    expect(clean).toContain('postgres://someone@db:5432/app')
    for (const leaked of ['secret@', 'password=', 'whatever']) {
      expect(clean).not.toContain(leaked)
    }
    expect(clean).not.toContain('******')
  })

  it('共享 fixture 与 Rust 侧使用同一组向量', () => {
    expect(FIXTURE.vectors.length).toBeGreaterThan(0)
    for (const vector of FIXTURE.vectors) {
      const result = scrubSensitiveText(vector.input, vector.secrets ?? [])
      for (const fragment of vector.must_disappear) {
        expect(result, `${vector.name}: leaked ${fragment}`).not.toContain(fragment)
      }
      for (const fragment of vector.must_keep) {
        expect(result, `${vector.name}: missing ${fragment}`).toContain(fragment)
      }
      expect(result, `${vector.name}: should not emit masks`).not.toContain('***')
      expect(containsSensitiveShape(result), `${vector.name}: still sensitive`).toBe(false)
    }
  })
})
