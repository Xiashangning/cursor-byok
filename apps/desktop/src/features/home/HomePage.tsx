import { useEffect, useState } from "react";
import { api, getDisabledPluginModelIds, pluginText, type Overview } from "../../shared/api";
import { ContributionCalendarChart, type ActivityPoint, type ActivityUnit } from "./charts/ContributionCalendarChart";
import { DailyTokenUsageChart } from "./charts/DailyTokenUsageChart";
import { HomeMetrics } from "./metrics/HomeMetrics";
import { PageContent } from "../../shell/layout/PageContent";
import type { VirtualPageSection } from "../../shell/layout/VirtualPage";
import { OverviewTimeRangeFilter, type OverviewRangePreset } from "./overview/OverviewTimeRangeFilter";
import { PageActions } from "../../shell/PageActions";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { formatTimeInput, parseTimeInput } from "../../shared/utils/parseTimeInput";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { claudeIcon, flatColorOrganizationIcon, openAiIcon } from "../../shared/ui/icons";
import { useI18n } from "../../i18n/store";

type TimeRange = { startMs: number; endMs: number };

const CALENDAR_DAYS = 365;
const ACTIVITY_HOUR_DAYS = 15;
const DAY_MS = 24 * 60 * 60_000;
const HOUR_MS = 60 * 60_000;

function bucketTokens(bucket: Overview["token_usage_series"][number]) {
  return bucket.input_tokens + bucket.cache_read_tokens + bucket.cache_write_tokens + bucket.output_tokens;
}

function localDateKey(timestampMs: number) {
  const date = new Date(timestampMs);
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function localHourKey(timestampMs: number) {
  return `${localDateKey(timestampMs)}T${String(new Date(timestampMs).getHours()).padStart(2, "0")}`;
}

function localDayStart(timestampMs: number) {
  const date = new Date(timestampMs);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function activityRange(days: number, endMs = Date.now()): TimeRange {
  const start = new Date(localDayStart(endMs));
  start.setDate(start.getDate() - (days - 1));
  return { startMs: start.getTime(), endMs };
}

function contributionCalendarData(overview: Overview, endMs: number): ActivityPoint[] {
  const tokensByDate = new Map<string, number>();
  for (const bucket of overview.token_usage_series) {
    const date = localDateKey(bucket.bucket_start_ms);
    tokensByDate.set(date, (tokensByDate.get(date) ?? 0) + bucketTokens(bucket));
  }
  const lastDay = new Date(localDayStart(Math.max(0, endMs - 1)));
  const firstDay = new Date(lastDay);
  firstDay.setDate(firstDay.getDate() - (CALENDAR_DAYS - 1));
  return Array.from({ length: CALENDAR_DAYS }, (_, offset) => {
    const date = new Date(firstDay);
    date.setDate(date.getDate() + offset);
    const key = localDateKey(date.getTime());
    return { key, tokens: tokensByDate.get(key) ?? 0 };
  });
}

function hourlyActivityData(overview: Overview, endMs: number): ActivityPoint[] {
  const tokensByHour = new Map<string, number>();
  for (const bucket of overview.token_usage_series) {
    const key = localHourKey(bucket.bucket_start_ms);
    tokensByHour.set(key, (tokensByHour.get(key) ?? 0) + bucketTokens(bucket));
  }
  const firstHour = new Date(localDayStart(Math.max(0, endMs - 1)));
  firstHour.setDate(firstHour.getDate() - (ACTIVITY_HOUR_DAYS - 1));
  const hoursToday = new Date(Math.max(0, endMs - 1)).getHours() + 1;
  return Array.from({ length: (ACTIVITY_HOUR_DAYS - 1) * 24 + hoursToday }, (_, offset) => {
    const date = new Date(firstHour);
    date.setHours(date.getHours() + offset);
    const key = localHourKey(date.getTime());
    return { key, tokens: tokensByHour.get(key) ?? 0 };
  });
}

function presetRange(preset: Exclude<OverviewRangePreset, "custom">, now = new Date()): TimeRange {
  const endMs = now.getTime();
  if (preset === "today") {
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    return { startMs: start.getTime(), endMs };
  }
  if (preset === "yesterday") {
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    start.setDate(start.getDate() - 1);
    const end = new Date(now);
    end.setHours(0, 0, 0, 0);
    return { startMs: start.getTime(), endMs: end.getTime() };
  }
  if (preset === "month") {
    const start = new Date(now);
    start.setMonth(start.getMonth() - 1);
    return { startMs: start.getTime(), endMs };
  }
  const duration = preset === "hour" ? 60 * 60_000
    : preset === "four-hours" ? 4 * 60 * 60_000
    : preset === "twenty-four-hours" ? 24 * 60 * 60_000
    : 7 * 24 * 60 * 60_000;
  return { startMs: now.getTime() - duration, endMs };
}

export function HomePage() {
  const { overview, busy, models, plugins } = useAppStore();
  const { locale } = useI18n();
  const [preset, setPreset] = useState<OverviewRangePreset>("month");
  const [granularity, setGranularity] = useState<number | undefined>(undefined);
  const [customRange, setCustomRange] = useState<TimeRange | null>(null);
  const [customOpen, setCustomOpen] = useState(false);
  const [customStart, setCustomStart] = useState("");
  const [customEnd, setCustomEnd] = useState("");
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [appliedModels, setAppliedModels] = useState<string[]>([]);
  const [rangeOverview, setRangeOverview] = useState<Overview | null>(null);
  const [rangeBusy, setRangeBusy] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const [activityUnit, setActivityUnit] = useState<ActivityUnit>("hour");
  const [activityData, setActivityData] = useState<{ unit: ActivityUnit; data: ActivityPoint[] } | null>(null);
  const selectedRange = preset === "custom" ? customRange : presetRange(preset);

  useEffect(() => {
    let active = true;
    const endMs = Date.now();
    const range = activityRange(activityUnit === "hour" ? ACTIVITY_HOUR_DAYS : CALENDAR_DAYS, endMs);
    void api.overview({
      ...range,
      bucketMs: activityUnit === "hour" ? HOUR_MS : DAY_MS,
      timezoneOffsetMinutes: new Date().getTimezoneOffset(),
    }).then((next) => {
      if (!active) return;
      const data = activityUnit === "hour"
        ? hourlyActivityData(next, endMs)
        : contributionCalendarData(next, endMs);
      setActivityData({ unit: activityUnit, data });
    });
    return () => { active = false; };
  }, [activityUnit, refreshVersion]);

  useEffect(() => {
    if (!selectedRange) return;
    let active = true;
    setRangeBusy(true);
    void api.overview({
      ...selectedRange,
      modelHashes: appliedModels,
      bucketMs: granularity,
      timezoneOffsetMinutes: new Date().getTimezoneOffset(),
    }).then((next) => {
      if (active) setRangeOverview(next);
    }).finally(() => {
      if (active) setRangeBusy(false);
    });
    return () => { active = false; };
  }, [preset, customRange, overview, refreshVersion, appliedModels, granularity]);

  const filteredOverview = rangeOverview ?? overview;
  const dailyTokenUsage = filteredOverview.token_usage_series.map((bucket) => ({
    bucketStartMs: bucket.bucket_start_ms,
    inputTokens: bucket.input_tokens,
    cacheReadTokens: bucket.cache_read_tokens,
    cacheWriteTokens: bucket.cache_write_tokens,
    outputTokens: bucket.output_tokens,
  }));
  const contribution = contributionCalendarData(overview, Date.now());
  const currentActivityData = activityData?.unit === activityUnit
    ? activityData.data
    : activityUnit === "day" ? contribution : [];
  const metrics = {
    llmCalls: filteredOverview.metrics.llm_calls,
    successfulCalls: filteredOverview.metrics.successful_calls,
    failedCalls: filteredOverview.metrics.failed_calls,
    tokenUsage: filteredOverview.metrics.token_usage,
    promptTokens: filteredOverview.metrics.prompt_tokens,
    cacheReadTokens: filteredOverview.metrics.cache_read_tokens,
    cacheWriteTokens: filteredOverview.metrics.cache_write_tokens,
  };
  const openCustom = (open: boolean) => {
    if (open && !customStart && !customEnd) {
      const now = new Date();
      setCustomEnd(formatTimeInput(now));
      setCustomStart(formatTimeInput(new Date(now.getTime() - 60 * 60_000)));
    }
    setCustomOpen(open);
  };
  const applyCustom = () => {
    const startMs = parseTimeInput(customStart);
    const endMs = parseTimeInput(customEnd);
    if (startMs === null || endMs === null || startMs >= endMs) return;
    setCustomRange({ startMs, endMs });
    setAppliedModels(selectedModels);
    setPreset("custom");
    setCustomOpen(false);
  };
  const selectPreset = (value: Exclude<OverviewRangePreset, "custom">) => {
    setPreset(value);
    setCustomOpen(false);
  };
  const refresh = async () => {
    await appStore.refresh();
    setRefreshVersion((version) => version + 1);
  };
  const iconFor = (type: string) => type === "anthropic" ? claudeIcon : openAiIcon;
  const disabledPluginModels = getDisabledPluginModelIds();
  const modelOptions = [
    ...models.map((model) => ({
      value: model.model_hash,
      label: model.display_name,
      group: modelProviderName(model),
      icon: iconFor(model.type),
    })),
    ...plugins.flatMap((plugin) => plugin.providers.flatMap((provider) =>
      provider.configured ? provider.models.filter((model) => !disabledPluginModels.has(model.id)).map((model) => ({
        value: model.id,
        label: model.displayName,
        group: pluginText(provider.displayName, locale) || model.pluginName,
        iconSrc: model.icon || undefined,
        icon: model.icon ? undefined : flatColorOrganizationIcon,
      })) : [],
    )),
  ];
  const sections: VirtualPageSection[] = [
    {
      key: "daily-token-usage",
      estimatedHeight: 240,
      content: <DailyTokenUsageChart
        data={dailyTokenUsage}
        granularity={filteredOverview.token_usage_granularity}
      />,
    },
    {
      key: "metrics",
      estimatedHeight: 130,
      content: <HomeMetrics data={metrics} refreshVersion={refreshVersion} />,
    },

    {
      key: "activity",
      estimatedHeight: 240,
      content: <ContributionCalendarChart
        unit={activityUnit}
        onUnitChange={setActivityUnit}
        data={currentActivityData}
      />,
    },
  ];

  return <>
    <PageActions><OverviewTimeRangeFilter
      value={preset}
      granularity={granularity}
      customOpen={customOpen}
      customStart={customStart}
      customEnd={customEnd}
      modelOptions={modelOptions}
      selectedModels={selectedModels}
      busy={busy || rangeBusy}
      onSelect={selectPreset}
      onGranularitySelect={setGranularity}
      onCustomOpenChange={openCustom}
      onCustomStartChange={setCustomStart}
      onCustomEndChange={setCustomEnd}
      onSelectedModelsChange={setSelectedModels}
      onCustomApply={applyCustom}
      onRefresh={() => void refresh()}
    /></PageActions>
    <PageContent title={t("概览")} sections={sections} />
  </>;
}
