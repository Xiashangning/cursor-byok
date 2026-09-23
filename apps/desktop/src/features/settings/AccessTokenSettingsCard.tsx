import { useEffect, useState } from "react";
import { api, setStoredAccessToken, type AccessTokenInfo } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { TextInput } from "../../shared/ui/FormControls";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import styles from "./AccessTokenSettingsCard.module.scss";

export function AccessTokenSettingsCard() {
  const message = useMessage();
  const [info, setInfo] = useState<AccessTokenInfo | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [regenerating, setRegenerating] = useState(false);

  useEffect(() => {
    void api.accessToken()
      .then(setInfo)
      .catch((cause) => message(cause instanceof Error ? cause.message : String(cause)));
  }, [message]);

  const copy = async () => {
    if (!info) return;
    try {
      await navigator.clipboard.writeText(info.token);
    } catch {
      try {
        await api.copyCursorText(info.token);
      } catch (cause) {
        message(cause instanceof Error ? cause.message : String(cause));
        return;
      }
    }
    message(t("已复制"));
  };

  const regenerate = async () => {
    try {
      setRegenerating(true);
      const next = await api.regenerateAccessToken();
      setStoredAccessToken(next.token);
      setInfo(next);
      setConfirming(false);
      message(t("访问令牌已重新生成"));
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRegenerating(false);
    }
  };

  return <TitledCard title={t("访问令牌")}>
    <div className={styles.content}>
      {info ? <>
        <TextInput
          readOnly
          className={styles.token}
          value={info.token}
          aria-label={t("访问令牌")}
          onFocus={(event) => event.currentTarget.select()}
        />
        <div className={styles.actions}>
          <Button size="small" onClick={() => void copy()}>{t("复制")}</Button>
          <Button size="small" disabled={info.source === "environment"} onClick={() => setConfirming(true)}>{t("重新生成")}</Button>
        </div>
        <small className={styles.hint}>{info.source === "environment"
          ? t("令牌由 CURSOR_ACCESS_TOKEN 环境变量固定，无法重新生成。")
          : t("远程浏览器访问时需要提供此令牌。")}</small>
      </> : <small className={styles.hint}>{t("加载中…")}</small>}
    </div>
    <ConfirmDialog
      open={confirming}
      title={t("确定要重新生成访问令牌吗？")}
      busy={regenerating}
      confirmLabel={t("重新生成")}
      onCancel={() => setConfirming(false)}
      onConfirm={() => void regenerate()}
    >
      <small className={styles.hint}>{t("重新生成后旧令牌立即失效，使用旧令牌的远程会话需要重新连接。")}</small>
    </ConfirmDialog>
  </TitledCard>;
}
