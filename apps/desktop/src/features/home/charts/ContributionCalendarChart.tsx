import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { init, Rect, type ElementEvent } from "zrender";
import type { Locale } from "../../../i18n/runtime";
import { useI18n } from "../../../i18n/store";
import { useTooltip, type TooltipAnchor } from "../../../shared/ui/Tooltip";
import { ActivityUnitToggle } from "../ActivityUnitToggle";
import styles from "./ContributionCalendarChart.module.scss";

export type ActivityUnit = "day" | "hour";

export type ActivityPoint = {
  key: string;
  tokens: number;
};

type ContributionCalendarChartProps = {
  unit: ActivityUnit;
  onUnitChange: (unit: ActivityUnit) => void;
  data: ActivityPoint[];
};

type CalendarCell = ActivityPoint & {
  column: number;
  row: number;
  intensity: number;
};

type CellExtra = CalendarCell & {
  kind: "calendar-cell";
  x: number;
  y: number;
  width: number;
  height: number;
};

type AxisLabel = {
  key: string;
  text: string;
  left: number;
};

type CalendarLayout = {
  cells: CalendarCell[];
  columnCount: number;
  ticks: Array<{ key: string; text: string; column: number }>;
};

const emptyColor = "rgba(139, 148, 158, 0.20)";
const heatStops = ["#9be9a8", "#40c463", "#30a14e", "#216e39"];

function parseHex(hex: string) {
  return [1, 3, 5].map((offset) => parseInt(hex.slice(offset, offset + 2), 16));
}

function heatColor(intensity: number) {
  const position = Math.min(intensity, 1) * heatStops.length - 1;
  const index = Math.max(0, Math.min(heatStops.length - 1, Math.floor(position)));
  const ratio = Math.max(0, position - index);
  const from = parseHex(heatStops[index]);
  const to = parseHex(heatStops[index + 1] ?? heatStops[index]);
  const mix = (channel: number) => Math.round(from[channel] + (to[channel] - from[channel]) * ratio);
  return `rgb(${mix(0)}, ${mix(1)}, ${mix(2)})`;
}

function cellColor(intensity: number) {
  return intensity === 0 ? emptyColor : heatColor(intensity);
}

const DAY_IN_MS = 24 * 60 * 60 * 1000;
const HOUR_COLUMNS_PER_DAY = 3;
const HOURS_PER_COLUMN = 24 / HOUR_COLUMNS_PER_DAY;
const UNIT_CONFIG = {
  day: { cellAspectRatio: 0.9, cellGap: 3, rowCount: 7, axisLabelGap: 8, axisLabelWidth: 28 },
  hour: { cellAspectRatio: 1, cellGap: 2, rowCount: HOURS_PER_COLUMN, axisLabelGap: 8, axisLabelWidth: 48 },
} as const;
const resizeTransitionMs = 180;

function parseDate(date: string) {
  return new Date(`${date}T00:00:00Z`);
}

function mondayIndex(date: Date) {
  return (date.getUTCDay() + 6) % 7;
}

function cellOffset(index: number, cellSize: number, gap: number) {
  return index * (cellSize + gap);
}

function isCellExtra(value: unknown): value is CellExtra {
  return typeof value === "object" && value !== null && (value as CellExtra).kind === "calendar-cell";
}

function buildCalendarLayout(data: ActivityPoint[], locale: Locale): CalendarLayout | null {
  if (data.length === 0) return null;

  const monthFormatter = new Intl.DateTimeFormat(locale, { month: "short", timeZone: "UTC" });
  const maximum = Math.max(1, ...data.map(({ tokens }) => tokens));
  const firstDate = parseDate(data[0].key);
  const calendarStart = new Date(firstDate.getTime() - mondayIndex(firstDate) * DAY_IN_MS);
  const cells: CalendarCell[] = data.map((day) => {
    const date = parseDate(day.key);
    const daysFromStart = Math.round((date.getTime() - calendarStart.getTime()) / DAY_IN_MS);
    const intensity = day.tokens / maximum;
    return { ...day, column: Math.floor(daysFromStart / 7), row: mondayIndex(date), intensity };
  });
  const columnCount = cells.at(-1)!.column + 1;
  const ticks = cells.reduce<Array<{ key: string; text: string; column: number }>>((acc, cell) => {
    const date = parseDate(cell.key);
    const key = `${date.getUTCFullYear()}-${date.getUTCMonth()}`;
    if (acc.at(-1)?.key !== key) acc.push({ key, text: monthFormatter.format(date), column: cell.column });
    return acc;
  }, []);
  return { cells, columnCount, ticks };
}

function buildHourlyLayout(data: ActivityPoint[], locale: Locale): CalendarLayout | null {
  if (data.length === 0) return null;

  const dayFormatter = new Intl.DateTimeFormat(locale, { month: "short", day: "numeric", timeZone: "UTC" });
  const maximum = Math.max(1, ...data.map(({ tokens }) => tokens));
  const cells: CalendarCell[] = data.map((point, index) => {
    const intensity = point.tokens / maximum;
    return {
      ...point,
      column: Math.floor(index / 24) * HOUR_COLUMNS_PER_DAY + Math.floor(index % 24 / HOURS_PER_COLUMN),
      row: index % HOURS_PER_COLUMN,
      intensity,
    };
  });
  const columnCount = Math.max(1, Math.ceil(data.length / 24) * HOUR_COLUMNS_PER_DAY);
  const ticks = cells.reduce<Array<{ key: string; text: string; column: number }>>((acc, cell) => {
    if (cell.column % HOUR_COLUMNS_PER_DAY !== 0 || cell.row !== 0) return acc;
    const key = cell.key.slice(0, 10);
    if (acc.at(-1)?.key === key) return acc;
    acc.push({ key, text: dayFormatter.format(parseDate(key)), column: cell.column });
    return acc;
  }, []);
  return { cells, columnCount, ticks };
}

export function ContributionCalendarChart({ unit, onUnitChange, data }: ContributionCalendarChartProps) {
  const { locale } = useI18n();
  const config = UNIT_CONFIG[unit];
  const scrollerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLDivElement>(null);
  const layoutRef = useRef<CalendarLayout | null>(null);
  const scheduleDrawRef = useRef<() => void>(() => undefined);
  const { show: showTooltip, hide: hideTooltip } = useTooltip();
  const [axisLabels, setAxisLabels] = useState<AxisLabel[]>([]);
  const layout = useMemo(
    () => (unit === "hour" ? buildHourlyLayout(data, locale) : buildCalendarLayout(data, locale)),
    [unit, data, locale],
  );
  const tokenFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale]);
  layoutRef.current = layout;

  useLayoutEffect(() => {
    const scroller = scrollerRef.current;
    const node = canvasRef.current;
    if (!scroller || !node) return;

    const chart = init(node, {
      renderer: "canvas",
      width: 1,
      height: 1,
      useDirtyRect: false,
    });

    const handleMouseOver = (event: ElementEvent) => {
      const extra = event.target?.extra;
      if (!isCellExtra(extra)) return;
      const anchor: TooltipAnchor = {
        contextElement: node,
        getBoundingClientRect: () => {
          const bounds = node.getBoundingClientRect();
          return new DOMRect(bounds.left + extra.x, bounds.top + extra.y, extra.width, extra.height);
        },
      };
      showTooltip(anchor, undefined, <div className={styles.tooltipContent}>
        <strong>{unit === "hour" ? extra.key.replace("T", " ") : extra.key}</strong>
        <span>{t("Token 用量：{tokens}", { tokens: tokenFormatter.format(extra.tokens) })}</span>
      </div>);
    };
    const handleMouseOut = (event: ElementEvent) => {
      if (isCellExtra(event.target?.extra)) hideTooltip();
    };

    chart.on("mouseover", handleMouseOver);
    chart.on("mouseout", handleMouseOut);

    const cellRects = new Map<string, Rect>();
    let drawFrame = 0;
    let lastAvailableWidth = -1;
    let lastCanvasHeight = -1;
    let lastLayout: typeof layout = null;
    const draw = () => {
      const currentLayout = layoutRef.current;
      if (!currentLayout) return;
      const availableWidth = Math.floor(scroller.getBoundingClientRect().width);
      if (availableWidth <= 0 || (availableWidth === lastAvailableWidth && currentLayout === lastLayout)) return;
      const gapsWidth = (currentLayout.columnCount - 1) * config.cellGap;
      const cellWidth = Math.max(0, (availableWidth - gapsWidth) / currentLayout.columnCount);
      const cellHeight = cellWidth / config.cellAspectRatio;
      const width = availableWidth;
      const height = config.rowCount * cellHeight
        + (config.rowCount - 1) * config.cellGap;

      let lastLabelEnd = -Infinity;
      const nextAxisLabels = currentLayout.ticks.flatMap((tick) => {
        const left = Math.min(
          cellOffset(tick.column, cellWidth, config.cellGap),
          availableWidth - config.axisLabelWidth,
        );
        if (left < lastLabelEnd + config.axisLabelGap) return [];
        lastLabelEnd = left + config.axisLabelWidth;
        return [{ ...tick, left }];
      });
      setAxisLabels(nextAxisLabels);

      if (availableWidth !== lastAvailableWidth || height !== lastCanvasHeight) {
        node.style.width = "100%";
        node.style.height = `${height}px`;
        chart.resize({ width, height });
        lastAvailableWidth = availableWidth;
        lastCanvasHeight = height;
      }
      lastLayout = currentLayout;

      const currentKeys = new Set(currentLayout.cells.map((cell) => cell.key));
      for (const [key, rect] of cellRects) {
        if (currentKeys.has(key)) continue;
        chart.remove(rect);
        cellRects.delete(key);
      }

      for (const cell of currentLayout.cells) {
        const x = cellOffset(cell.column, cellWidth, config.cellGap);
        const y = cellOffset(cell.row, cellHeight, config.cellGap);
        const shape = {
          x,
          y,
          width: cellWidth,
          height: cellHeight,
          r: Math.min(3, Math.min(cellWidth, cellHeight) / 4),
        };
        const extra = {
          ...cell,
          kind: "calendar-cell" as const,
          x,
          y,
          width: cellWidth,
          height: cellHeight,
        } satisfies CellExtra;
        const current = cellRects.get(cell.key);

        if (current) {
          current.extra = extra;
          current.stopAnimation();
          current.animateTo(
            { shape, style: { fill: cellColor(cell.intensity) } },
            { duration: resizeTransitionMs, easing: "cubicOut" },
          );
          continue;
        }

        const rect = new Rect({
          shape,
          style: {
            fill: cellColor(cell.intensity),
            stroke: "rgba(139, 148, 158, 0.10)",
            lineWidth: 1,
          },
          cursor: "default",
          extra,
        });
        cellRects.set(cell.key, rect);
        chart.add(rect);
      }
      hideTooltip();
    };
    const scheduleDraw = () => {
      window.cancelAnimationFrame(drawFrame);
      drawFrame = window.requestAnimationFrame(draw);
    };
    scheduleDrawRef.current = scheduleDraw;
    const observer = new ResizeObserver(scheduleDraw);
    observer.observe(scroller);
    scheduleDraw();

    return () => {
      observer.disconnect();
      window.cancelAnimationFrame(drawFrame);
      scheduleDrawRef.current = () => undefined;
      chart.dispose();
    };
  }, [layout !== null, config]);

  useLayoutEffect(() => {
    scheduleDrawRef.current();
  }, [layout]);

  const sectionLabel = unit === "hour" ? t("按小时统计的 Token 用量") : t("过去一年的 Token 用量");
  return (
    <section className={styles.root} aria-label={sectionLabel}>
      <div className={styles.toolbar}><ActivityUnitToggle value={unit} onChange={onUnitChange} /></div>
      <div ref={scrollerRef} className={styles.scroller}>
        <div
          ref={canvasRef}
          className={styles.canvas}
          role="img"
          aria-label={sectionLabel}
        />
        <div className={styles.axis} aria-hidden="true">
          {axisLabels.map((label) => <span key={label.key} style={{ left: label.left }}>{label.text}</span>)}
        </div>
      </div>
    </section>
  );
}
