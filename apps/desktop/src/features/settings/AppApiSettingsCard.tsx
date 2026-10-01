import { useEffect, useState } from "react";
import { api, type AppApiSettings } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import styles from "./AppApiSettingsCard.module.scss";

export function AppApiSettingsCard({ servicePort }: { servicePort: number }) {
  const message = useMessage();
  const [saved, setSaved] = useState<AppApiSettings | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void api.appApiSettings().then((settings) => {
      setSaved(settings);
      setEnabled(settings.enabled);
    }).catch((cause: unknown) => message(cause instanceof Error ? cause.message : String(cause)));
  }, [message]);

  const save = async () => {
    try {
      setSaving(true);
      const settings = await api.setAppApiSettings({ enabled });
      setSaved(settings);
      setEnabled(settings.enabled);
      message(t("应用控制 API 设置已保存"));
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  };

  const changed = saved !== null && saved.enabled !== enabled;
  const address = `http://127.0.0.1:${servicePort}/byok/app/v1`;

  return <TitledCard title={t("应用控制")} action={<Button size="small" variant="primary" disabled={!changed || saving} onClick={() => void save()}>
    {saving ? t("保存中…") : t("保存")}
  </Button>}>
    <div className={styles.content}>
      <div className={styles.row}>
        <div className={styles.description}>
          <strong>{t("开启应用控制 API")}</strong>
          <small>{t("允许本机代理配置应用、接入模型，并调用桌面端使用的管理接口。")}</small>
        </div>
        <Switch label={t("开启应用控制 API")} checked={enabled} disabled={!saved || saving} onChange={setEnabled} />
      </div>
      <div className={styles.address}>
        <strong>{t("基础地址")}</strong>
        <code>{address}</code>
      </div>
      <small className={styles.hint}>{t("路径与应用内部管理接口一致，例如 /models、/plugins、/settings 和 /harness/cursor。关闭时，这个地址拒绝访问。应用窗口仍使用内部管理接口。")}</small>
      <small className={styles.hint}>{t("本机来源直接放行；非本机来源的请求需携带 Authorization: Bearer 头，值为上方“访问令牌”设置中的令牌。")}</small>
    </div>
  </TitledCard>;
}
