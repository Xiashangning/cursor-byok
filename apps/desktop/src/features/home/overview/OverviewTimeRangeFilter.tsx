import { autoUpdate, computePosition, flip, offset, shift, size } from "@floating-ui/dom";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { parseTimeInput } from "../../../shared/utils/parseTimeInput";
import controls from "../../../shared/ui/Controls.module.scss";
import { Icon } from "../../../shared/ui/Icon";
import { MultiSelect, type MultiSelectOption } from "../../../shared/ui/MultiSelect";
import { TooltipTrigger } from "../../../shared/ui/TooltipTrigger";
import { refreshIcon } from "../../../shared/ui/icons";
import styles from "./OverviewTimeRangeFilter.module.scss";

export type OverviewRangePreset =
  | "hour"
  | "four-hours"
  | "twenty-four-hours"
  | "today"
  | "yesterday"
  | "week"
  | "month"
  | "custom";

export function OverviewTimeRangeFilter({ value, granularity, customOpen, customStart, customEnd, modelOptions, selectedModels, busy, onSelect, onGranularitySelect, onCustomOpenChange, onCustomStartChange, onCustomEndChange, onSelectedModelsChange, onCustomApply, onRefresh }: {
  value: OverviewRangePreset;
  granularity: number | undefined;
  customOpen: boolean;
  customStart: string;
  customEnd: string;
  modelOptions: MultiSelectOption[];
  selectedModels: string[];
  busy: boolean;
  onSelect: (value: Exclude<OverviewRangePreset, "custom">) => void;
  onGranularitySelect: (bucketMs: number | undefined) => void;
  onCustomOpenChange: (open: boolean) => void;
  onCustomStartChange: (value: string) => void;
  onCustomEndChange: (value: string) => void;
  onSelectedModelsChange: (value: string[]) => void;
  onCustomApply: () => void;
  onRefresh: () => void;
}) {
  const presets: Array<{ value: Exclude<OverviewRangePreset, "custom">; label: string }> = [
    { value: "hour", label: t("近1小时") },
    { value: "four-hours", label: t("近4小时") },
    { value: "twenty-four-hours", label: t("近24小时") },
    { value: "today", label: t("今天") },
    { value: "yesterday", label: t("昨天") },
    { value: "week", label: t("近一周") },
    { value: "month", label: t("近一个月") },
  ];
  const granularityOptions = [
    { bucketMs: undefined, label: t("自动") },
    { bucketMs: 60_000, label: t("1分钟") },
    { bucketMs: 15 * 60_000, label: t("15分钟") },
    { bucketMs: 30 * 60_000, label: t("30分钟") },
    { bucketMs: 60 * 60_000, label: t("1小时") },
  ];
  const customButton = useRef<HTMLButtonElement>(null);
  const popover = useRef<HTMLDivElement>(null);
  const granularityButton = useRef<HTMLButtonElement>(null);
  const granularityMenu = useRef<HTMLDivElement>(null);
  const popoverId = useId();
  const granularityMenuId = useId();
  const [position, setPosition] = useState({ left: 0, top: 0, width: 300, maxHeight: 480 });
  const [menuPosition, setMenuPosition] = useState({ left: 0, top: 0 });
  const [granularityOpen, setGranularityOpen] = useState(false);

  useLayoutEffect(() => {
    if (!customOpen || !customButton.current || !popover.current) return;
    return autoUpdate(customButton.current, popover.current, () => void computePosition(customButton.current!, popover.current!, {
      placement: "bottom-end",
      middleware: [offset(5), flip({ padding: 10 }), shift({ padding: 10 }), size({
        padding: 10,
        apply: ({ availableHeight }) => setPosition((current) => ({
          ...current,
          maxHeight: Math.max(240, availableHeight),
        })),
      })],
    }).then(({ x, y }) => setPosition((current) => ({ ...current, left: x, top: y }))));
  }, [customOpen]);

  useLayoutEffect(() => {
    if (!granularityOpen || !granularityButton.current || !granularityMenu.current) return;
    return autoUpdate(granularityButton.current, granularityMenu.current, () =>
      void computePosition(granularityButton.current!, granularityMenu.current!, {
        placement: "bottom-start",
        middleware: [offset(4), flip({ padding: 10 }), shift({ padding: 10 })],
      }).then(({ x, y }) => setMenuPosition({ left: x, top: y })));
  }, [granularityOpen]);

  useEffect(() => {
    if (!granularityOpen) return;
    const closeOutside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!granularityButton.current?.contains(target) && !granularityMenu.current?.contains(target)) setGranularityOpen(false);
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, [granularityOpen]);

  useEffect(() => {
    if (!customOpen) return;
    const closeOutside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (
        !customButton.current?.contains(target)
        && !popover.current?.contains(target)
      ) onCustomOpenChange(false);
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, [customOpen, onCustomOpenChange]);

  const parsedStart = parseTimeInput(customStart);
  const parsedEnd = parseTimeInput(customEnd);
  const customValid = parsedStart !== null && parsedEnd !== null && parsedStart < parsedEnd;
  const selectedGranularity = granularityOptions.find((option) => option.bucketMs === granularity) ?? granularityOptions[0];

  return <div className={styles.root} aria-label={t("概览时间范围")}>
    <div className={styles.presets}>
      {presets.map((preset) => <button
        key={preset.value}
        type="button"
        aria-pressed={value === preset.value}
        onClick={() => onSelect(preset.value)}
      >{preset.label}</button>)}
      <button
        ref={customButton}
        type="button"
        aria-haspopup="dialog"
        aria-controls={customOpen ? popoverId : undefined}
        aria-expanded={customOpen}
        aria-pressed={value === "custom"}
        onClick={() => onCustomOpenChange(!customOpen)}
      >{t("自定义")}</button>
    </div>
    <button
      ref={granularityButton}
      className={styles.granularityButton}
      aria-label={t("显示粒度")}
      aria-haspopup="menu"
      aria-controls={granularityOpen ? granularityMenuId : undefined}
      aria-expanded={granularityOpen}
      onClick={() => setGranularityOpen((current) => !current)}
    >{selectedGranularity.label}</button>
    <TooltipTrigger label={t("刷新")}><button className={controls.iconButton} aria-label={t("刷新")} disabled={busy} onClick={onRefresh}>
      <Icon className={busy ? controls.spin : ""} icon={refreshIcon} size="1.1em" />
    </button></TooltipTrigger>
    {granularityOpen && createPortal(<div
      id={granularityMenuId}
      ref={granularityMenu}
      className={styles.granularityMenu}
      role="menu"
      aria-label={t("显示粒度")}
      style={menuPosition}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          setGranularityOpen(false);
          granularityButton.current?.focus();
        }
      }}
    >
      {granularityOptions.map((option) => <button
        key={String(option.bucketMs)}
        type="button"
        role="menuitem"
        aria-pressed={option.bucketMs === granularity}
        onClick={() => {
          setGranularityOpen(false);
          granularityButton.current?.focus();
          onGranularitySelect(option.bucketMs);
        }}
      >{option.label}</button>)}
    </div>, document.body)}
    {customOpen && createPortal(<div
      id={popoverId}
      ref={popover}
      className={styles.popover}
      role="dialog"
      aria-label={t("自定义概览筛选")}
      style={position}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          onCustomOpenChange(false);
          customButton.current?.focus();
        }
      }}
    >
      <label><span>{t("开始时间")}</span><input type="text" placeholder={t("如：2026-08-23 09:00、1小时前")} value={customStart} onChange={(event) => onCustomStartChange(event.target.value)} /></label>
      <label><span>{t("结束时间")}</span><input type="text" placeholder={t("如：现在、2026-08-23 18:00")} value={customEnd} onChange={(event) => onCustomEndChange(event.target.value)} /></label>
      <div className={styles.filterRow}><MultiSelect label={t("模型")} value={selectedModels} options={modelOptions} onChange={onSelectedModelsChange} /></div>
      <div className={styles.popoverActions}>
        <button type="button" className={controls.secondary} onClick={() => onCustomOpenChange(false)}>{t("取消")}</button>
        <button type="button" className={controls.primary} disabled={!customValid} onClick={onCustomApply}>{t("应用")}</button>
      </div>
    </div>, document.body)}
  </div>;
}
