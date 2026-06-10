/** Shared sort-rule types for table browsing and SQL result re-sorting. */

export type SortNulls = 'default' | 'first' | 'last'
export type SortKind = 'column' | 'expression'

export interface SortRule {
  id: string          // client-side unique id, used as React key
  kind: SortKind
  column?: string     // required when kind === 'column'
  expression?: string // required when kind === 'expression'
  desc: boolean
  nulls: SortNulls
}

/** Wire payload sent to backend (matches Rust OrderCondition). */
export interface SortRulePayload {
  kind: SortKind
  column?: string
  expression?: string
  desc: boolean
  nulls: SortNulls
}
