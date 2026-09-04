import { useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  getDisabledPluginAccountIds,
  getDisabledPluginModelIds,
  setPluginAccountEnabled,
  setPluginModelEnabled,
  setMultiplePluginModelsEnabled,
  pluginText,
  type PluginAddMethod,
  type PluginDescriptor,
  type PluginOAuthBegin,
  type PluginProviderDescriptor,
  type PluginResourceDescriptor,
  type PluginResourceView,
} from "../../shared/api";
import { useI18n } from "../../i18n/store";
import { appStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Card } from "../../shared/ui/Card";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Icon } from "../../shared/ui/Icon";
import { refreshIcon, trashIcon } from "../../shared/ui/icons";
import { Switch } from "../../shared/ui/Switch";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import styles from "./PluginResourcePanels.module.scss";

const PAGE_SIZE = 10;

export function PluginAddPanel({ plugin, onConfigured }: { plugin: PluginDescriptor; onConfigured: () => void }) {
  return <div className={styles.panel}>
    {plugin.resources.map((resource) => <ResourceAddSection
      key={resource.type}
      plugin={plugin}
      resource={resource}
      onConfigured={onConfigured}
    />)}
    {plugin.resources.length === 0 && <span className={styles.empty}>{t("该插件不需要添加资源")}</span>}
  </div>;
}

function ResourceAddSection({ plugin, resource, onConfigured }: {
  plugin: PluginDescriptor;
  resource: PluginResourceDescriptor;
  onConfigured: () => void;
}) {
  return <>
    {resource.add.map((method) => <OAuthMethodCard
      key={method.id}
      pluginId={plugin.id}
      resourceType={resource.type}
      method={method}
      onConfigured={onConfigured}
    />)}
  </>;
}

function OAuthMethodCard({ pluginId, resourceType, method, onConfigured }: {
  pluginId: string;
  resourceType: string;
  method: PluginAddMethod;
  onConfigured: () => void;
}) {
  const { locale } = useI18n();
  const [status, setStatus] = useState<"idle" | "starting" | "polling" | "success" | "error">("idle");
  const [begun, setBegun] = useState<PluginOAuthBegin | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const stopped = useRef(false);

  const copyCode = async (code: string) => {
    await api.copyCursorText(code).catch(() => undefined);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  };

  useEffect(() => () => { stopped.current = true; }, []);

  useEffect(() => {
    if (!begun || status !== "polling") return;
    let timer = 0;
    const poll = async (intervalMs: number) => {
      if (stopped.current) return;
      try {
        const result = await api.pluginOAuthPoll(begun.sessionId);
        if (stopped.current) return;
        if (result.status === "pending") {
          timer = window.setTimeout(() => void poll(result.pollIntervalMs), Math.max(1000, result.pollIntervalMs));
          return;
        }
        if (result.status === "completed") {
          await appStore.refreshPlugins();
          if (result.modelSyncError) {
            setStatus("error");
            setError(t("账号已保存，但同步模型失败：{error}", { error: result.modelSyncError }));
            return;
          }
          setStatus("success");
          onConfigured();
          return;
        }
        setStatus("error");
        setError(result.message || t("授权被拒绝或已失败。"));
      } catch (cause) {
        if (stopped.current) return;
        setError(errorText(cause));
        timer = window.setTimeout(() => void poll(intervalMs), Math.max(1000, intervalMs));
      }
    };
    timer = window.setTimeout(() => void poll(begun.pollIntervalMs), Math.max(1000, begun.pollIntervalMs));
    return () => window.clearTimeout(timer);
  }, [begun, onConfigured, status]);

  const start = async () => {
    setStatus("starting");
    setError(null);
    try {
      const next = await api.pluginOAuthBegin(pluginId, resourceType, method.id);
      setBegun(next);
      setStatus("polling");
      await api.copyCursorText(next.userCode).catch(() => undefined);
      await api.openExternalUrl(next.verificationUrlComplete || next.verificationUrl);
    } catch (cause) {
      setStatus("error");
      setError(errorText(cause));
    }
  };

  return <Card className={styles.methodCard}>
    <strong>{pluginText(method.displayName, locale)}</strong>
    {method.description && <span>{pluginText(method.description, locale)}</span>}
    {begun && status === "polling" && <div className={styles.deviceCode}>
      <small>{t("设备验证码")}</small>
      <button
        type="button"
        title={begun.userCode}
        onClick={() => void copyCode(begun.userCode)}
      >
        {begun.userCode.startsWith("http") ? t("授权链接") : begun.userCode}
      </button>
      <button
        type="button"
        className={styles.copy}
        onClick={() => void copyCode(begun.userCode)}
      >
        {copied ? t("已复制") : t("复制")}
      </button>
    </div>}
    <div className={styles.actions}>
      <Button variant="primary" disabled={status === "starting" || status === "polling"} onClick={() => void start()}>
        {status === "starting" ? t("正在申请授权码…") : status === "polling" ? t("等待网页端确认授权中…") : t("开始登录")}
      </Button>
      {begun && status === "polling" && <Button onClick={() => void api.openExternalUrl(begun.verificationUrlComplete || begun.verificationUrl)}>{t("打开授权网页")}</Button>}
    </div>
    {status === "success" && <span className={styles.success}>{t("账号已保存，模型目录已同步。")}</span>}
    {error && <span className={styles.error} role="alert">{error}</span>}
  </Card>;
}

export function PluginSettingsPanel({ plugin }: { plugin: PluginDescriptor }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [activeProviderId, setActiveProviderId] = useState<string | null>(null);

  const run = async (key: string, task: () => Promise<void>) => {
    setBusy(key);
    setError(null);
    try {
      await task();
      await appStore.refreshPlugins();
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    let active = true;
    const autoRefreshResources = async () => {
      let didRefresh = false;
      for (const res of plugin.resources) {
        if (!res.canRefresh) continue;
        for (const item of res.resources) {
          if (!active) return;
          // Auto-refresh if resource is marked invalid or missing metrics
          if (item.state.status === "invalid" || item.metrics.length === 0) {
            try {
              await api.refreshPluginResource(plugin.id, res.type, item.id);
              didRefresh = true;
            } catch {
              // ignore background refresh failure
            }
          }
        }
      }
      if (active && didRefresh) {
        await appStore.refreshPlugins();
      }
    };
    void autoRefreshResources();
    return () => { active = false; };
  }, [plugin.id]);

  if (activeProviderId) {
    const provider = plugin.providers.find((p) => p.id === activeProviderId);
    if (provider) {
      return <ProviderModelsView provider={provider} onBack={() => setActiveProviderId(null)} />;
    }
  }

  return <div className={styles.panel}>
    {plugin.providers.map((provider) => <ProviderRow
      key={provider.id}
      provider={provider}
      busy={busy !== null}
      syncing={busy === `sync:${provider.id}`}
      onSync={() => void run(`sync:${provider.id}`, async () => {
        await api.syncPluginModels(plugin.id, provider.id);
      })}
      onViewModels={() => setActiveProviderId(provider.id)}
    />)}
    {plugin.resources.map((resource) => <ResourceList
      key={resource.type}
      resource={resource}
      busyKey={busy}
      onRefresh={(item) => void run(`refresh:${item.id}`, async () => {
        await api.refreshPluginResource(plugin.id, resource.type, item.id);
      })}
      onDelete={(item) => void run(`delete:${item.id}`, async () => {
        await api.deletePluginResource(plugin.id, resource.type, item.id);
      })}
    />)}
    {error && <span className={styles.error} role="alert">{error}</span>}
  </div>;
}

function ProviderRow({ provider, busy, syncing, onSync, onViewModels }: {
  provider: PluginProviderDescriptor;
  busy: boolean;
  syncing: boolean;
  onSync: () => void;
  onViewModels: () => void;
}) {
  const { locale } = useI18n();
  const [disabledIds, setDisabledIds] = useState<Set<string>>(() => getDisabledPluginModelIds());

  useEffect(() => {
    const handleUpdate = () => setDisabledIds(getDisabledPluginModelIds());
    window.addEventListener("cursor_plugin_models_changed", handleUpdate);
    return () => window.removeEventListener("cursor_plugin_models_changed", handleUpdate);
  }, []);

  const enabledCount = provider.models.filter((m) => !disabledIds.has(m.id)).length;

  return (
    <Card className={styles.providerCard}>
      <div className={styles.providerRow}>
        <div>
          <strong>{pluginText(provider.displayName, locale)}</strong>
          <span>
            {provider.providerType}
            {" · "}
            {provider.models.length > 0
              ? `${enabledCount}/${t("{count} 个模型", { count: provider.models.length })}`
              : t("尚未同步模型")}
            {" · "}
            {provider.configured ? t("可调用") : t("未就绪")}
          </span>
        </div>
        <div className={styles.providerActions}>
          {provider.models.length > 0 && (
            <Button size="small" onClick={onViewModels}>
              {t("查看模型")}
            </Button>
          )}
          {provider.hasModels && (
            <Button size="small" variant="primary" disabled={busy} onClick={onSync}>
              {syncing ? t("正在同步…") : t("同步模型")}
            </Button>
          )}
        </div>
      </div>
    </Card>
  );
}

function ProviderModelsView({ provider, onBack }: { provider: PluginProviderDescriptor; onBack: () => void }) {
  const { locale } = useI18n();
  const [search, setSearch] = useState("");
  const [disabledIds, setDisabledIds] = useState<Set<string>>(() => getDisabledPluginModelIds());

  useEffect(() => {
    const handleUpdate = () => setDisabledIds(getDisabledPluginModelIds());
    window.addEventListener("cursor_plugin_models_changed", handleUpdate);
    return () => window.removeEventListener("cursor_plugin_models_changed", handleUpdate);
  }, []);

  const toggleModel = (modelId: string) => {
    const isCurrentlyDisabled = disabledIds.has(modelId);
    setPluginModelEnabled(modelId, isCurrentlyDisabled);
    setDisabledIds(getDisabledPluginModelIds());
  };

  const toggleAll = (enable: boolean) => {
    const target = filteredModels.length > 0 ? filteredModels : provider.models;
    setMultiplePluginModelsEnabled(target.map((m) => m.id), enable);
    setDisabledIds(getDisabledPluginModelIds());
  };

  const enabledCount = provider.models.filter((m) => !disabledIds.has(m.id)).length;

  const filteredModels = provider.models.filter((m) => {
    if (!search.trim()) return true;
    const q = search.toLowerCase();
    const shortId = m.id.split("/").pop() || m.id;
    return m.displayName.toLowerCase().includes(q) || shortId.toLowerCase().includes(q);
  });

  return (
    <div className={styles.modelsView}>
      <div className={styles.modelsViewHeader}>
        <TooltipTrigger label={t("返回")}>
          <button type="button" className={styles.backButton} onClick={onBack} aria-label={t("返回")}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M19 12H5M12 19l-7-7 7-7"/></svg>
          </button>
        </TooltipTrigger>
        <div className={styles.headerTitle}>
          <strong>{pluginText(provider.displayName, locale)}</strong>
          <span>{t("模型列表")}</span>
          <span className={styles.headerCountChip}>
            {enabledCount}/{provider.models.length}
          </span>
        </div>
      </div>
      <div className={styles.modelListToolbar}>
        <div className={styles.searchContainer}>
          <svg className={styles.searchIcon} width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>
          <input
            type="text"
            placeholder={t("搜索模型…")}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className={styles.modelSearchInput}
          />
          {search && (
            <button type="button" className={styles.clearSearchBtn} onClick={() => setSearch("")} aria-label="Clear">
              ✕
            </button>
          )}
        </div>
        <div className={styles.modelBatchActions}>
          <button type="button" onClick={() => toggleAll(true)} className={styles.pillButton}>
            {t("全选")}
          </button>
          <button type="button" onClick={() => toggleAll(false)} className={styles.pillButton}>
            {t("全不选")}
          </button>
        </div>
      </div>
      <div className={styles.modelsViewList}>
        {filteredModels.map((m) => {
          const shortId = m.id.split("/").pop() || m.id;
          const nameLower = m.displayName.toLowerCase();
          const idLower = m.id.toLowerCase();
          const isEnabled = !disabledIds.has(m.id);

          const isClaude = nameLower.includes("claude") || idLower.includes("claude");
          const isGemini = nameLower.includes("gemini") || idLower.includes("gemini");
          const isGpt = nameLower.includes("gpt") || idLower.includes("gpt");
          const isThinking = nameLower.includes("thinking") || idLower.includes("thinking");
          const isHigh = nameLower.includes("(high)") || idLower.includes("-high");
          const isMedium = nameLower.includes("(medium)") || idLower.includes("-medium");
          const isLow = nameLower.includes("(low)") || idLower.includes("-low");
          const isExtraLow = nameLower.includes("(extra-low)") || idLower.includes("-extra-low");
          const isImage = nameLower.includes("image") || idLower.includes("image");

          return (
            <div
              key={m.id}
              className={`${styles.modelItem} ${isEnabled ? styles.modelItemActive : styles.modelItemDisabled}`}
              onClick={() => toggleModel(m.id)}
            >
              <input
                type="checkbox"
                checked={isEnabled}
                onChange={() => toggleModel(m.id)}
                className={styles.modelCheckbox}
                onClick={(e) => e.stopPropagation()}
              />
              <div className={styles.modelInfo}>
                <span className={styles.modelName}>{pluginText(m.displayName, locale) || shortId}</span>
                <span className={styles.modelId}>{shortId}</span>
              </div>
              <div className={styles.modelBadges}>
                {isClaude && <span className={styles.claudeTag}>Claude</span>}
                {isGemini && <span className={styles.geminiTag}>Gemini</span>}
                {isGpt && <span className={styles.gptTag}>GPT-OSS</span>}
                {isThinking && <span className={styles.thinkingTag}>Thinking</span>}
                {isHigh && <span className={styles.highTag}>High</span>}
                {isMedium && <span className={styles.mediumTag}>Medium</span>}
                {isLow && <span className={styles.lowTag}>Low</span>}
                {isExtraLow && <span className={styles.extraLowTag}>Extra-Low</span>}
                {isImage && <span className={styles.imageTag}>Image</span>}
              </div>
            </div>
          );
        })}
        {filteredModels.length === 0 && (
          <div className={styles.emptySearch}>
            <span>{t("暂无数据")}</span>
          </div>
        )}
      </div>
    </div>
  );
}

function ResourceList({ resource, busyKey, onRefresh, onDelete }: {
  resource: PluginResourceDescriptor;
  busyKey: string | null;
  onRefresh: (item: PluginResourceView) => void;
  onDelete: (item: PluginResourceView) => void;
}) {
  const { locale } = useI18n();
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const filtered = useMemo(
    () => resource.resources.filter((item) => item.displayName.toLowerCase().includes(query.trim().toLowerCase())),
    [resource.resources, query],
  );
  const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const visible = filtered.slice((Math.min(page, pageCount) - 1) * PAGE_SIZE, Math.min(page, pageCount) * PAGE_SIZE);

  useEffect(() => setPage(1), [query]);

  return <FormField label={pluginText(resource.displayName, locale)}>
    <div className={styles.resourceSection}>
      {resource.resources.length > PAGE_SIZE && <div className={styles.toolbar}>
        <TextInput aria-label={t("搜索资源")} placeholder={t("搜索资源")} value={query} onChange={(event) => setQuery(event.target.value)} />
      </div>}
      <div className={styles.resourceList}>
        {visible.map((item) => {
          const isRefreshing = busyKey === `refresh:${item.id}`;
          const isDeleting = busyKey === `delete:${item.id}`;
          return <ResourceRow
            key={item.id}
            item={item}
            canRefresh={resource.canRefresh}
            isRefreshing={isRefreshing}
            isDeleting={isDeleting}
            disabled={isRefreshing || isDeleting}
            onRefresh={() => onRefresh(item)}
            onDelete={() => onDelete(item)}
          />;
        })}
        {visible.length === 0 && <span className={styles.empty}>{t("还没有资源，请先添加。")}</span>}
      </div>
      {pageCount > 1 && <div className={styles.pagination}>
        <Button size="small" disabled={page <= 1} onClick={() => setPage((current) => current - 1)}>{t("上一页")}</Button>
        <span>{t("第 {page} / {total} 页", { page: Math.min(page, pageCount), total: pageCount })}</span>
        <Button size="small" disabled={page >= pageCount} onClick={() => setPage((current) => current + 1)}>{t("下一页")}</Button>
      </div>}
    </div>
  </FormField>;
}

function ResourceRow({ item, canRefresh, isRefreshing, isDeleting, disabled, onRefresh, onDelete }: {
  item: PluginResourceView;
  canRefresh: boolean;
  isRefreshing: boolean;
  isDeleting: boolean;
  disabled: boolean;
  onRefresh: () => void;
  onDelete: () => void;
}) {
  const { locale } = useI18n();
  const [disabledAccountIds, setDisabledAccountIds] = useState<Set<string>>(() => getDisabledPluginAccountIds());

  useEffect(() => {
    const handleUpdate = () => setDisabledAccountIds(getDisabledPluginAccountIds());
    window.addEventListener("cursor_plugin_accounts_changed", handleUpdate);
    return () => window.removeEventListener("cursor_plugin_accounts_changed", handleUpdate);
  }, []);

  const isEnabled = !disabledAccountIds.has(item.id);

  const toggleAccount = (checked: boolean) => {
    setPluginAccountEnabled(item.id, checked);
    setDisabledAccountIds(getDisabledPluginAccountIds());
  };

  return <Card className={`${styles.resourceRow} ${!isEnabled ? styles.resourceRowDisabled : ""}`}>
    <div className={styles.resourceIdentity}>
      <Switch
        checked={isEnabled}
        disabled={disabled}
        label={item.displayName}
        onChange={toggleAccount}
      />
      <div className={styles.resourceText}>
        <div className={styles.resourceHeaderLine}>
          <strong>{item.displayName}</strong>
          {item.description && (() => {
            const desc = pluginText(item.description, locale).trim();
            const isPro = desc.toLowerCase().includes("pro") || desc.toLowerCase().includes("ultra") || desc.toLowerCase().includes("premium") || desc.toLowerCase().includes("advanced");
            const label = isPro ? (desc.toLowerCase().includes("ultra") ? "ULTRA" : "PRO") : "FREE";
            return <span className={isPro ? styles.proBadge : styles.freeBadge}>{isPro ? `🔥 ${label}` : label}</span>;
          })()}
        </div>
        {item.metrics.length > 0 && (
          <div className={styles.metricsRow}>
            {item.metrics.map((metric) => {
              const isClaude = metric.id.toLowerCase().includes("claude");
              const isGemini = metric.id.toLowerCase().includes("gemini");
              const badgeStyle = isClaude
                ? styles.claudeMetricBadge
                : isGemini
                ? styles.geminiMetricBadge
                : styles.genericMetricBadge;
              const shortLabel = isClaude ? "C" : isGemini ? "G" : pluginText(metric.label, locale);
              return (
                <span
                  key={metric.id}
                  className={`${styles.metricBadge} ${badgeStyle}`}
                  title={metric.resetAtMs ? `${pluginText(metric.label, locale)} Reset: ${new Date(metric.resetAtMs).toLocaleTimeString()}` : `${pluginText(metric.label, locale)} Quota: ${Math.round(metric.value)}%`}
                >
                  <span className={styles.metricPrefix}>{shortLabel}</span>
                  <span className={styles.metricValue}>{Math.round(metric.value)}%</span>
                </span>
              );
            })}
          </div>
        )}
      </div>
    </div>
    <div className={styles.resourceActions}>
      <StateBadge isEnabled={isEnabled} state={item.state} />
      {canRefresh && (
        <TooltipTrigger label={t("刷新")}>
          <button
            type="button"
            className={`${styles.actionIconButton} ${isRefreshing ? styles.refreshingSpin : ""}`}
            disabled={disabled}
            onClick={onRefresh}
            aria-label={t("刷新")}
          >
            <Icon icon={refreshIcon} size="1.05em" />
          </button>
        </TooltipTrigger>
      )}
      <TooltipTrigger label={t("删除")}>
        <button
          type="button"
          className={`${styles.actionIconButton} ${styles.actionDeleteButton}`}
          disabled={disabled || isDeleting}
          onClick={onDelete}
          aria-label={t("删除")}
        >
          <Icon icon={trashIcon} size="1.05em" />
        </button>
      </TooltipTrigger>
    </div>
  </Card>;
}

function StateBadge({ isEnabled = true, state }: { isEnabled?: boolean; state: PluginResourceView["state"] }) {
  if (!isEnabled) {
    return <span className={styles.disabledBadge}><span className={styles.badgeDot} />{t("已停用")}</span>;
  }
  if (state.status === "cooling") {
    return <span className={styles.coolingBadge} title={state.message ?? undefined}><span className={styles.badgeDot} />{t("冷却中")}</span>;
  }
  if (state.status === "invalid") {
    return <span className={styles.invalidBadge} title={state.message ?? undefined}><span className={styles.badgeDot} />{t("已失效")}</span>;
  }
  return <span className={styles.readyBadge}><span className={styles.badgeDot} />{t("可用")}</span>;
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}
