/**
 * 后端在遇到"没有字段级映射"的 PostgreSQL 类型时，不会返回空值，而是返回
 * 形如 `{ "$unmapped_type": "mycomp", "$value": "(1,abc)", "$encoding": "text" }`
 * 的结构化标记。这里统一解析，供结果集与结构浏览展示，避免与真 NULL 混淆。
 */
export interface UnmappedValue {
  type: string;
  text: string | null;
  encoding: string;
}

export function parseUnmappedValue(value: unknown): UnmappedValue | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  const type = record['$unmapped_type'];
  if (typeof type !== 'string') return null;
  const text = record['$value'];
  const encoding = record['$encoding'];
  return {
    type,
    text: typeof text === 'string' ? text : null,
    encoding: typeof encoding === 'string' ? encoding : 'unknown',
  };
}

/** 标记的展示文本：有原始文本就给文本，没有则说明编码原因。 */
export function formatUnmappedText(value: UnmappedValue): string {
  if (value.text !== null) return value.text;
  return value.encoding === 'binary' ? '二进制编码，未解码' : '无文本表示';
}

export function formatUnmappedValue(value: UnmappedValue): string {
  return `[未映射类型 ${value.type}] ${formatUnmappedText(value)}`;
}
