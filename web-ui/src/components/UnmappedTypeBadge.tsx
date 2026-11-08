import { formatUnmappedText } from '../utils/unmappedValue';
import type { UnmappedValue } from '../utils/unmappedValue';

interface UnmappedTypeBadgeProps {
  value: UnmappedValue;
  className?: string;
}

/**
 * 「类型未映射」标记：与 NULL 的灰色斜体明显不同，提示这是能力缺口而非空数据。
 */
export function UnmappedTypeBadge({ value, className = '' }: UnmappedTypeBadgeProps) {
  return (
    <span
      className={`inline-flex items-center gap-1 max-w-full ${className}`}
      title={`该列类型 ${value.type} 尚未做字段级映射，仅按原始文本回显；不代表数据为空`}
    >
      <span className="shrink-0 px-1 py-0.5 rounded text-[10px] font-semibold uppercase tracking-wide bg-amber-500/20 text-amber-300 border border-amber-500/40">
        {value.type}
      </span>
      <span className="truncate font-mono text-amber-200/90">{formatUnmappedText(value)}</span>
    </span>
  );
}
