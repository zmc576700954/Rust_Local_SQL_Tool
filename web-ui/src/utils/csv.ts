/**
 * RFC 4180 风格的分隔文本解析。
 *
 * 与简单 split(',') 的差异：
 * - 支持双引号包裹的字段（内部 `""` 转义、可包含逗号/制表符/换行）
 * - 未加引号的空字段解析为 `null`，`""` 解析为空串——保住 NULL 与空串的区分
 * - 自动剥离 UTF-8 BOM，兼容 CRLF
 */
export type CsvValue = string | null;

export interface ParsedDelimited {
  headers: string[];
  rows: Record<string, CsvValue>[];
}

/** 默认按首行里制表符与逗号的数量选择分隔符。 */
export function detectDelimiter(text: string): ',' | '\t' {
  const firstLine = text.split(/\r?\n/, 1)[0] ?? '';
  let tabs = 0;
  let commas = 0;
  let inQuotes = false;
  for (let i = 0; i < firstLine.length; i += 1) {
    const ch = firstLine[i];
    if (ch === '"') inQuotes = !inQuotes;
    else if (!inQuotes && ch === '\t') tabs += 1;
    else if (!inQuotes && ch === ',') commas += 1;
  }
  return tabs > commas ? '\t' : ',';
}

/** 解析成「字段数组的数组」；返回的每个字段同时带上是否被引号包裹的信息。 */
function parseRecords(text: string, delimiter: string): CsvValue[][] {
  const clean = text.replace(/^\uFEFF/, '');
  const records: CsvValue[][] = [];
  let record: CsvValue[] = [];
  let field = '';
  let quoted = false;
  let inQuotes = false;
  let i = 0;

  const pushField = () => {
    record.push(quoted ? field : field === '' ? null : field);
    field = '';
    quoted = false;
  };
  const pushRecord = () => {
    pushField();
    // 完全空白的行（只有一个 null 字段）跳过
    if (!(record.length === 1 && record[0] === null)) {
      records.push(record);
    }
    record = [];
  };

  while (i < clean.length) {
    const ch = clean[i];

    if (inQuotes) {
      if (ch === '"') {
        if (clean[i + 1] === '"') {
          field += '"';
          i += 2;
          continue;
        }
        inQuotes = false;
        i += 1;
        continue;
      }
      field += ch;
      i += 1;
      continue;
    }

    if (ch === '"' && field.trim() === '') {
      // 引号必须出现在字段开头；容忍前面的空白
      field = '';
      quoted = true;
      inQuotes = true;
      i += 1;
      continue;
    }
    if (ch === delimiter) {
      pushField();
      i += 1;
      continue;
    }
    if (ch === '\r') {
      i += 1;
      continue;
    }
    if (ch === '\n') {
      pushRecord();
      i += 1;
      continue;
    }
    field += ch;
    i += 1;
  }

  if (field.length > 0 || quoted || record.length > 0) {
    pushRecord();
  }
  return records;
}

export function parseDelimited(text: string, delimiter?: string): ParsedDelimited {
  const sep = delimiter ?? detectDelimiter(text);
  const records = parseRecords(text, sep);
  if (records.length === 0) {
    return { headers: [], rows: [] };
  }
  const headers = records[0].map((h) => (h === null ? '' : h));
  const rows = records.slice(1).map((values) => {
    const obj: Record<string, CsvValue> = {};
    headers.forEach((h, idx) => {
      obj[h] = values[idx] ?? null;
    });
    return obj;
  });
  return { headers, rows };
}
