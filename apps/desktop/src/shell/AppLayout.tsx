import { useCallback, useState } from "react";
import type { IconifyIcon } from "@iconify/react/offline";
import KeepAliveRouteOutlet from "keepalive-for-react-router";
import { NavLink, useLocation } from "react-router-dom";
import cursorIconUrl from "../shared/assets/icons/cursor.svg";
import { api } from "../shared/api";
import { PageLayout } from "./layout/PageLayout";
import { Card } from "../shared/ui/Card";
import { ConfirmDialog } from "../shared/ui/ConfirmDialog";
import controls from "../shared/ui/Controls.module.scss";
import { Icon } from "../shared/ui/Icon";
import { TooltipTrigger } from "../shared/ui/TooltipTrigger";
import { flatColorAboutIcon, flatColorAreaChartIcon, flatColorCrystalOscillatorIcon, flatColorSalesPerformanceIcon, flatColorSettingsIcon, refreshIcon } from "../shared/ui/icons";
import { useMessage } from "../shared/ui/message";
import { VirtualList } from "../shared/virtual/VirtualList";
import { appStore, useAppStore } from "../shared/store/appStore";
import { useUpdateStore } from "../shared/store/updateStore";
import styles from "./AppLayout.module.scss";
import { PageActionsTarget } from "./PageActions";

type MenuItem =
  | { kind: "page"; path: string; label: string; icon: IconifyIcon | string }
  | { kind: "external"; id: string; label: string; icon: IconifyIcon | string }
  | { kind: "group"; label: string };

const keptAlivePages = ["/", "/calls", "/settings", "/harness/cursor", "/plugins"];
const tutorialReadStorageKey = "cursor-byok:tutorial-read";
const tutorialUrl = "https://docs.leokun.cn";

export function AppLayout() {
  const { busy } = useAppStore();
  const { availableVersion } = useUpdateStore();
  const message = useMessage();
  const location = useLocation();
  const [leftActionTarget, setLeftActionTarget] = useState<HTMLDivElement | null>(null);
  const [rightActionTarget, setRightActionTarget] = useState<HTMLDivElement | null>(null);
  const [confirmTutorial, setConfirmTutorial] = useState(false);
  const [tutorialRead, setTutorialRead] = useState(() => {
    try {
      return localStorage.getItem(tutorialReadStorageKey) === "true";
    } catch {
      return false;
    }
  });
  const menuItems: MenuItem[] = [
    { kind: "page", path: "/", label: t("数据概览"), icon: flatColorAreaChartIcon },
    { kind: "page", path: "/calls", label: t("调用详细"), icon: flatColorSalesPerformanceIcon },
    { kind: "group", label: t("模型配置") },
    { kind: "page", path: "/harness/cursor", label: "Cursor", icon: cursorIconUrl },
    { kind: "group", label: t("设置") },
    { kind: "page", path: "/plugins", label: t("插件配置"), icon: flatColorCrystalOscillatorIcon },
    { kind: "page", path: "/settings", label: t("系统设置"), icon: flatColorSettingsIcon },
    { kind: "external", id: "tutorial", label: t("使用教程"), icon: flatColorAboutIcon },
  ];

  const openTutorial = useCallback(() => {
    setConfirmTutorial(false);
    void api.openExternalUrl(tutorialUrl)
      .then(() => {
        setTutorialRead(true);
        try {
          localStorage.setItem(tutorialReadStorageKey, "true");
        } catch {
          // Read state remains valid for the current session when storage is unavailable.
        }
      })
      .catch((cause) => message(cause instanceof Error ? cause.message : String(cause)));
  }, [message]);

  return <PageLayout className={styles.root}>
    <Card as="aside" className={styles.menuCard}>
      <nav className={styles.navigation} aria-label={t("主菜单")}>
        <VirtualList
          items={menuItems}
          itemKey={(item) => item.kind === "group" ? `group-${item.label}` : item.kind === "external" ? `external-${item.id}` : item.path}
          estimatedItemHeight={36}
          itemGap={3}
          className={`${styles.navigationList} scroll-shadow-bottom`}
        >
          {(item) => item.kind === "group"
          ? <div className={styles.navigationGroup} key={`group-${item.label}`}>{item.label}</div>
          : item.kind === "external"
          ? <div className={styles.navigationRow} key={item.id}>
            <button
              type="button"
              aria-label={`${item.label}${tutorialRead ? "" : `，${t("未读")}`}`}
              onClick={() => setConfirmTutorial(true)}
            >
              {typeof item.icon === "string"
                ? <Icon src={item.icon} size="1.3em" />
                : <Icon icon={item.icon} size="1.3em" />}
              <span>{item.label}</span>
              {!tutorialRead && <span className={styles.menuIndicatorDot} aria-hidden="true" />}
            </button>
          </div>
          : <div className={styles.navigationRow} key={item.path}>
            <NavLink to={item.path} end={item.path === "/"}>
              {typeof item.icon === "string"
                ? <Icon src={item.icon} size="1.3em" />
                : <Icon icon={item.icon} size="1.3em" />}
              <span>{item.label}</span>
              {item.path === "/settings" && availableVersion && <span className={styles.menuIndicatorDot} aria-hidden="true" />}
            </NavLink>
          </div>}
        </VirtualList>
      </nav>
    </Card>
    <ConfirmDialog
      id="open-tutorial-dialog"
      open={confirmTutorial}
      title={t("打开使用教程？")}
      cancelLabel={t("取消")}
      confirmLabel={t("打开教程")}
      onCancel={() => setConfirmTutorial(false)}
      onConfirm={openTutorial}
    >
      <p>{t("将在系统浏览器中打开使用教程，是否继续？")}</p>
    </ConfirmDialog>
    <main className={styles.content}>
      <div className={styles.actionRegion}>
        <Card className={styles.actions}>
          <div ref={setLeftActionTarget} className={styles.pageActions} />
          {location.pathname !== "/" && <TooltipTrigger label={t("刷新")}><button className={controls.iconButton} aria-label={t("刷新")} disabled={busy} onClick={() => void appStore.refresh()}>
            <Icon className={busy ? controls.spin : ""} icon={refreshIcon} size="1.1em" />
          </button></TooltipTrigger>}
          <div ref={setRightActionTarget} className={styles.pageActions} />
        </Card>
      </div>
      <PageActionsTarget.Provider value={{ left: leftActionTarget, right: rightActionTarget }}>
        <KeepAliveRouteOutlet
          activeCacheKey={location.pathname}
          include={keptAlivePages}
          max={keptAlivePages.length}
          enableActivity
          containerClassName={styles.keepAliveContainer}
          cacheNodeClassName={styles.keepAlivePage}
        />
      </PageActionsTarget.Provider>
    </main>
  </PageLayout>;
}
