import { describe, it, expect } from 'vitest';
import { parseDelimited, detectDelimiter } from './csv';

describe('parseDelimited', () => {
  it('区分 NULL（未加引号的空字段）与空串（""）', () => {
    const { rows } = parseDelimited('a,b\n,""\n,文本\n');
    expect(rows[0].a).toBeNull();
    expect(rows[0].b).toBe('');
    expect(rows[1].a).toBeNull();
    expect(rows[1].b).toBe('文本');
  });

  it('支持带引号字段中的逗号、引号和换行', () => {
    const text = '"a","b"\n"含,逗号","他说""你好"""\n"第一行\n第二行","制表\t符"\n';
    const { headers, rows } = parseDelimited(text);
    expect(headers).toEqual(['a', 'b']);
    expect(rows[0].a).toBe('含,逗号');
    expect(rows[0].b).toBe('他说"你好"');
    expect(rows[1].a).toBe('第一行\n第二行');
    expect(rows[1].b).toBe('制表\t符');
  });

  it('CRLF 与 BOM 不影响解析', () => {
    const { headers, rows } = parseDelimited('\uFEFFid,name\r\n1,中文😀\r\n');
    expect(headers).toEqual(['id', 'name']);
    expect(rows[0]).toEqual({ id: '1', name: '中文😀' });
  });

  it('自动识别制表符分隔', () => {
    const text = 'id\tname\n1\tfoo\n';
    expect(detectDelimiter(text)).toBe('\t');
    const { headers, rows } = parseDelimited(text);
    expect(headers).toEqual(['id', 'name']);
    expect(rows[0].name).toBe('foo');
  });

  it('列数不足时补 null，而不是丢列', () => {
    const { rows } = parseDelimited('a,b,c\n1,2\n');
    expect(rows[0]).toEqual({ a: '1', b: '2', c: null });
  });
});
