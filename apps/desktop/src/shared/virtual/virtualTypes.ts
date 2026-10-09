import type { CSSProperties, ReactNode } from "react"

export interface VirtualListApi {
  scrollToIndex(
    index: number,
    align?: "start" | "center" | "end" | "auto"
  ): void
  scrollToOffset(offset: number): void
  scrollToTop(): void
}

export interface VirtualListRenderContext {
  index: number
  measureElement: (node: HTMLElement | null) => void
  isScrolling: boolean
}

export interface VirtualListProps<TItem> {
  items: TItem[]
  getKey: (item: TItem, index: number) => string
  estimateSize: (item: TItem, index: number) => number
  renderItem: (item: TItem, context: VirtualListRenderContext) => ReactNode

  overscan?: number
  itemGap?: number
  deferInitialRender?: boolean
  scrollbarSize?: number
  minThumbSize?: number
  scrollbarVisibility?: "auto" | "always" | "hidden"
  scrollbarInsetTop?: CSSProperties["top"]
  className?: string
  contentClassName?: string
  style?: CSSProperties
  onRangeChange?: (range: { start: number; end: number }) => void
  onReady?: (api: VirtualListApi) => void
}
