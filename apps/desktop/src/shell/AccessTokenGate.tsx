import { useState, type FormEvent } from "react";
import { createPortal } from "react-dom";
import { setStoredAccessToken } from "../shared/api";
import { appStore, useAppStore } from "../shared/store/appStore";
import { Button } from "../shared/ui/Button";
import { Card } from "../shared/ui/Card";
import { TextInput } from "../shared/ui/FormControls";
import styles from "./AccessTokenGate.module.scss";

/** 消费 ?token= 引导参数:写入 localStorage 并从 URL 移除,在 App 首次渲染时调用。 */
export function consumeAccessTokenParam(): void {
  const url = new URL(window.location.href);
  const token = url.searchParams.get("token");
  if (!token) return;
  setStoredAccessToken(token);
  url.searchParams.delete("token");
  window.history.replaceState(null, "", url);
}

export function AccessTokenGate() {
  const { accessTokenRequired } = useAppStore();
  const [token, setToken] = useState("");
  if (!accessTokenRequired) return null;
  const connect = (event: FormEvent) => {
    event.preventDefault();
    const value = token.trim();
    if (!value) return;
    setStoredAccessToken(value);
    appStore.setAccessTokenRequired(false);
    window.location.reload();
  };
  return createPortal(
    <div className={styles.mask}>
      <Card className={styles.card}>
        <form className={styles.form} onSubmit={connect}>
          <strong>{t("需要访问令牌")}</strong>
          <small>{t("远程访问需要提供访问令牌，你可以在部署日志或已登录设备的设置页中找到它。")}</small>
          <TextInput
            type="password"
            value={token}
            autoFocus
            autoComplete="off"
            placeholder={t("访问令牌")}
            aria-label={t("访问令牌")}
            onChange={(event) => setToken(event.target.value)}
          />
          <Button variant="primary" type="submit" disabled={!token.trim()}>{t("连接")}</Button>
        </form>
      </Card>
    </div>,
    document.body,
  );
}
