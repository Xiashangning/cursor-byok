import { useEffect, useState } from "react";
import { api, type ProxySettings, type ProxySettingsInput, type StorageCleanupMode, type StorageStatistics, type TabSettings } from "../../shared/api";
import { PageContent } from "../../shell/layout/PageContent";
import { AccessTokenSettingsCard } from "./AccessTokenSettingsCard";
import { AppApiSettingsCard } from "./AppApiSettingsCard";
import { AppLifecycleSettingsCard } from "./AppLifecycleSettingsCard";
import { CommitSettingsCard } from "./CommitSettingsCard";
import { ProxySettingsCard } from "./ProxySettingsCard";
import { TabSettingsCard } from "./TabSettingsCard";
import { Button } from "../../shared/ui/Button";
import { Checkbox } from "../../shared/ui/Checkbox";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Select } from "../../shared/ui/Select";
import { TitledCard } from "../../shared/ui/TitledCard";
import { setLocalePreference, useI18n, type LocalePreference } from "../../i18n/store";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { themeOptions } from "../../shared/theme/theme";
import styles from "./SettingsPage.module.scss";

export function SettingsPage() {
  const { detailed, ports, theme } = useAppStore();
  const { preference, locale } = useI18n();
  const message = useMessage();
  const [proxyPort, setProxyPort] = useState(String(ports.proxy_port));
  const [servicePort, setServicePort] = useState(String(ports.service_port));
  const [editingPorts, setEditingPorts] = useState(false);
  const [savingPorts, setSavingPorts] = useState(false);
  const [storage, setStorage] = useState<StorageStatistics | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [cleanupMode, setCleanupMode] = useState<StorageCleanupMode>("cache");
  const [outboundProxy, setOutboundProxy] = useState<ProxySettings | null>(null);
  const [proxyDraft, setProxyDraft] = useState<ProxySettingsInput>({ mode: "default", address: "", auth_enabled: false, username: "", password: "" });
  const [editingProxy, setEditingProxy] = useState(false);
  const [savingProxy, setSavingProxy] = useState(false);
  const [tabSettings, setTabSettings] = useState<TabSettings | null>(null);
  const [tabDraft, setTabDraft] = useState<TabSettings>({ mode: "public", address: "" });
  const [editingTab, setEditingTab] = useState(false);
  const [savingTab, setSavingTab] = useState(false);
  useEffect(() => {
    void Promise.all([api.statisticsStorage(), api.proxySettings(), api.tabSettings()]).then(([nextStorage, nextProxy, nextTab]) => {
      setStorage(nextStorage);
      setOutboundProxy(nextProxy);
      setProxyDraft({ mode: nextProxy.mode, address: nextProxy.address, auth_enabled: nextProxy.auth_enabled, username: nextProxy.username, password: "" });
      setTabSettings(nextTab);
      setTabDraft(nextTab);
    }).catch((cause) => message(cause instanceof Error ? cause.message : String(cause)));
  }, [message]);
  useEffect(() => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
  }, [ports.proxy_port, ports.service_port]);

  const parsePort = (value: string, label: string) => {
    const port = Number(value);
    if (!Number.isInteger(port) || port < 0 || port > 65_535) {
      throw new Error(`${label}${t("必须是 0–65535 之间的整数")}`);
    }
    return port;
  };
  const savePorts = async () => {
    try {
      const next = {
        proxy_port: parsePort(proxyPort, t("代理端口")),
        service_port: parsePort(servicePort, t("服务端口")),
      };
      setSavingPorts(true);
      if (await appStore.updatePorts(next)) {
        setEditingPorts(false);
        message(t("端口设置已保存，重启软件后生效"), { duration: 4_000 });
      }
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSavingPorts(false);
    }
  };
  const editPorts = () => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
    setEditingPorts(true);
  };
  const cancelPortEdit = () => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
    setEditingPorts(false);
  };
  const cleanStorage = async (mode: StorageCleanupMode) => {
    try {
      setClearing(true);
      const cleaned = await api.cleanStorage(mode);
      setStorage(cleaned.storage);
      setConfirmClear(false);
      await appStore.refresh();
      message(t("存储空间已清理，释放 {size}", { size: formatBytes(cleaned.freed_bytes) }));
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setClearing(false);
    }
  };
  const editProxy = () => {
    if (!outboundProxy) return;
    setProxyDraft({ mode: outboundProxy.mode, address: outboundProxy.address, auth_enabled: outboundProxy.auth_enabled, username: outboundProxy.username, password: "" });
    setEditingProxy(true);
  };
  const cancelProxyEdit = () => {
    if (outboundProxy) {
      setProxyDraft({ mode: outboundProxy.mode, address: outboundProxy.address, auth_enabled: outboundProxy.auth_enabled, username: outboundProxy.username, password: "" });
    }
    setEditingProxy(false);
  };
  const saveProxy = async () => {
    try {
      setSavingProxy(true);
      const saved = await api.setProxySettings({ ...proxyDraft, password: proxyDraft.password || undefined });
      setOutboundProxy(saved);
      setProxyDraft({ mode: saved.mode, address: saved.address, auth_enabled: saved.auth_enabled, username: saved.username, password: "" });
      setEditingProxy(false);
      message(t("代理设置已保存"));
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSavingProxy(false);
    }
  };
  const editTab = () => {
    if (!tabSettings) return;
    setTabDraft(tabSettings);
    setEditingTab(true);
  };
  const cancelTabEdit = () => {
    if (tabSettings) setTabDraft(tabSettings);
    setEditingTab(false);
  };
  const saveTab = async () => {
    try {
      if (tabDraft.mode === "custom" && !tabDraft.address.trim()) throw new Error(t("TAB 服务地址不能为空"));
      setSavingTab(true);
      const saved = await api.setTabSettings({ ...tabDraft, address: tabDraft.address.trim() });
      setTabSettings(saved);
      setTabDraft(saved);
      setEditingTab(false);
      message(t("TAB 设置已保存"));
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSavingTab(false);
    }
  };
  const formatBytes = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    const units = ["KB", "MB", "GB", "TB"];
    let value = bytes / 1024;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
    return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
  };
  const clearDescription = t("清理详细模式记录的客户端请求、服务端响应与追踪附件，以及不可达的历史数据和无服务端引用的状态缓存，并回收空闲页；同时删除缓存中的 semble 索引、克隆仓库与网页缓存（下次使用时重建）。调用记录、模型配置和应用设置会保留，此操作无法撤销。");
  const resetDescription = t("在仅清除缓存的基础上，进一步清空所有调用记录与会话数据。保留数据库中的模型配置、应用设置，插件安装、登录账户、启用的插件模型和相关配置，以及用户规则和嵌入模型权重，此操作无法撤销。");
  const content = (
    <div className={styles.page}>
      <TitledCard title={t("调用观测")}>
        <div className={styles.settingRow}>
          <div>
            <strong>{t("详细模式")}</strong>
            <small>
              {t("额外保存完整请求和流响应；默认只保存时间、状态与用量。")}
            </small>
          </div>
          <Checkbox
            label={t("详细模式")}
            checked={detailed}
            onChange={(checked) => void appStore.updateDetailed(checked)}
          />
        </div>
      </TitledCard>
      <TitledCard title={t("端口设置")} action={editingPorts ? (
        <div className={styles.cardActions}>
          <Button size="small" disabled={savingPorts} onClick={cancelPortEdit}>{t("取消")}</Button>
          <Button variant="primary" size="small" disabled={savingPorts} onClick={() => void savePorts()}>{savingPorts ? t("保存中…") : t("保存")}</Button>
        </div>
      ) : (
        <button type="button" className={styles.textButton} onClick={editPorts}>{t("编辑")}</button>
      )}>
        <div className={styles.portSettings}>
          <div className={styles.portFields}>
            {editingPorts ? <><FormField
              label={t("代理端口")}
              hint={t("Cursor 使用的本地代理端口；填写 0 时启动时随机选择。")}
            >
              <TextInput
                type="number"
                min={0}
                max={65535}
                step={1}
                value={proxyPort}
                onChange={(event) => setProxyPort(event.target.value)}
              />
            </FormField>
            <FormField
              label={t("服务端口")}
              hint={t(
                "桌面前端连接的本地管理服务端口；填写 0 时启动时随机选择。",
              )}
            >
              <TextInput
                type="number"
                min={0}
                max={65535}
                step={1}
                value={servicePort}
                onChange={(event) => setServicePort(event.target.value)}
              />
            </FormField></> : <>
              <div className={styles.portValue}><strong>{t("代理端口")}</strong><span>{ports.proxy_port}</span></div>
              <div className={styles.portValue}><strong>{t("服务端口")}</strong><span>{ports.service_port}</span></div>
            </>}
          </div>
          <div className={styles.portFooter}>
            <small>
              {t(
                "端口被占用时会自动选择新的随机端口并保存。修改后需要重启软件才会生效。",
              )}
            </small>
          </div>
        </div>
      </TitledCard>
      <AccessTokenSettingsCard />
      <AppApiSettingsCard servicePort={ports.service_port} />
      <ProxySettingsCard settings={outboundProxy} draft={proxyDraft} editing={editingProxy} saving={savingProxy} onDraftChange={setProxyDraft} onEdit={editProxy} onCancel={cancelProxyEdit} onSave={() => void saveProxy()} />
      <TabSettingsCard settings={tabSettings} draft={tabDraft} editing={editingTab} saving={savingTab} onDraftChange={setTabDraft} onEdit={editTab} onCancel={cancelTabEdit} onSave={() => void saveTab()} />
      <CommitSettingsCard />
      <AppLifecycleSettingsCard />
      <TitledCard title={t("语言")}>
        <div className={styles.settingRow}>
          <div>
            <strong>{t("界面语言")}</strong>
            <small>{t("默认跟随操作系统；不支持的系统语言使用英文。当前：{language}", { language: locale === "zh-CN" ? "简体中文" : "English" })}</small>
          </div>
          <div className={styles.languageControl}>
            <Select
              value={preference}
              ariaLabel={t("界面语言")}
              options={[
                { value: "system", label: t("跟随系统") },
                { value: "zh-CN", label: "简体中文" },
                { value: "en-US", label: "English" },
              ]}
              onChange={(value) => setLocalePreference(value as LocalePreference)}
            />
          </div>
        </div>
      </TitledCard>
      <TitledCard title={t("主题")}>
        <div className={styles.themeActions}>
          {themeOptions.map(({ id }) => (
            <button
              className={theme === id ? styles.selected : ""}
              key={id}
              onClick={() => appStore.selectTheme(id)}
            >
              {id === "default-dark" ? t("默认暗色") : t("默认亮色")}
            </button>
          ))}
        </div>
      </TitledCard>
      <TitledCard title={t("存储管理")}>
        <div className={styles.storageRow}>
          <div className={styles.storageStats}>
            <div>
              <strong>{t("统计数据")}</strong>
              <small>{storage ? formatBytes(storage.bytes) : t("计算中…")}</small>
            </div>
            <div>
              <strong>{t("缓存占用")}</strong>
              <small>{storage ? formatBytes(storage.cache_bytes) : t("计算中…")}</small>
            </div>
          </div>
          <div className={styles.storageActions}>
            <button
              type="button"
              className={styles.textButton}
              onClick={() => setConfirmClear(true)}
            >
              {t("清理存储空间")}
            </button>
          </div>
        </div>
      </TitledCard>
      <ConfirmDialog
        open={confirmClear}
        title={t("确定要清理存储空间吗？")}
        busy={clearing}
        cancelLabel={t("取消")}
        confirmLabel={t("确认清理")}
        onCancel={() => setConfirmClear(false)}
        onConfirm={() => void cleanStorage(cleanupMode)}
      >
        <div className={styles.confirmContent}>
          <label className={styles.cleanupOption}>
            <input
              type="radio"
              name="cleanup-mode"
              checked={cleanupMode === "cache"}
              onChange={() => setCleanupMode("cache")}
            />
            <span>
              <strong>{t("仅清除缓存")}</strong>
              <small>{clearDescription}</small>
            </span>
          </label>
          <label className={styles.cleanupOption}>
            <input
              type="radio"
              name="cleanup-mode"
              checked={cleanupMode === "reset"}
              onChange={() => setCleanupMode("reset")}
            />
            <span>
              <strong>{t("清除缓存和调用记录")}</strong>
              <small>{resetDescription}</small>
            </span>
          </label>
        </div>
      </ConfirmDialog>
    </div>
  );
  return <PageContent title={t("设置")} sections={[{ key: "settings", estimatedHeight: 1200, content }]} />;
}
