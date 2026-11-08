import { describe, expect, it } from 'vitest';
import { formatUnmappedText, formatUnmappedValue, parseUnmappedValue } from './unmappedValue';

describe('unmappedValue', () => {
  it('真 NULL 不会被识别为未映射标记', () => {
    expect(parseUnmappedValue(null)).toBeNull();
    expect(parseUnmappedValue(undefined)).toBeNull();
  });

  it('识别后端的结构化未映射标记', () => {
    const parsed = parseUnmappedValue({
      $unmapped_type: 'mycomp',
      $value: '(1,abc)',
      $encoding: 'text',
    });
    expect(parsed).toEqual({ type: 'mycomp', text: '(1,abc)', encoding: 'text' });
    expect(formatUnmappedText(parsed!)).toBe('(1,abc)');
    expect(formatUnmappedValue(parsed!)).toBe('[未映射类型 mycomp] (1,abc)');
  });

  it('二进制未映射类型给出编码说明而不是空白', () => {
    const parsed = parseUnmappedValue({
      $unmapped_type: 'mycomp',
      $value: null,
      $encoding: 'binary',
    });
    expect(parsed).not.toBeNull();
    expect(formatUnmappedText(parsed!)).toBe('二进制编码，未解码');
  });

  it('普通对象与数组不会被误判', () => {
    expect(parseUnmappedValue({ a: 1 })).toBeNull();
    expect(parseUnmappedValue([{ $unmapped_type: 'x' }])).toBeNull();
  });
});
