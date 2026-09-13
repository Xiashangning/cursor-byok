import { useState } from "react";
import type { IconifyIcon } from "@iconify/react/offline";
import KeepAliveRouteOutlet from "keepalive-for-react-router";
import { NavLink, useLocation } from "react-router-dom";
import cursorIconUrl from "../shared/assets/icons/cursor.svg";
import { PageLayout } from "./layout/PageLayout";
import { Card } from "../shared/ui/Card";
import controls from "../shared/ui/Controls.module.scss";
import { Icon } from "../shared/ui/Icon";
import { TooltipTrigger } from "../shared/ui/TooltipTrigger";
import { flatColorAreaChartIcon, flatColorCrystalOscillatorIcon, flatColorSalesPerformanceIcon, flatColorSettingsIcon, refreshIcon } from "../shared/ui/icons";
import { VirtualList } from "../shared/virtual/VirtualList";
import { appStore, useAppStore } from "../shared/store/appStore";
import { useUpdateStore } from "../shared/store/updateStore";
import styles from "./AppLayout.module.scss";
import { PageActionsTarget } from "./PageActions";

type MenuItem =
  | { kind: "page"; path: string; label: string; icon: IconifyIcon | string }
  | { kind: "group"; label: string };

const keptAlivePages = ["/", "/calls", "/settings", "/harness/cursor", "/plugins"];

export function AppLayout() {
  const { busy } = useAppStore();
  const { availableVersion } = useUpdateStore();
  const location = useLocation();
  const [leftActionTarget, setLeftActionTarget] = useState<HTMLDivElement | null>(null);
  const [rightActionTarget, setRightActionTarget] = useState<HTMLDivElement | null>(null);
  const menuItems: MenuItem[] = [
    { kind: "page", path: "/", label: t("数据概览"), icon: flatColorAreaChartIcon },
    { kind: "page", path: "/calls", label: t("调用详细"), icon: flatColorSalesPerformanceIcon },
    { kind: "group", label: t("模型配置") },
    { kind: "page", path: "/harness/cursor", label: "Cursor", icon: cursorIconUrl },
    { kind: "group", label: t("设置") },
    { kind: "page", path: "/plugins", label: t("插件配置"), icon: flatColorCrystalOscillatorIcon },
    { kind: "page", path: "/settings", label: t("系统设置"), icon: flatColorSettingsIcon },
  ];

  return <PageLayout className={styles.root}>
    <Card as="aside" className={styles.menuCard}>
      <nav className={styles.navigation} aria-label={t("主菜单")}>
        <VirtualList
          items={menuItems}
          itemKey={(item) => item.kind === "group" ? `group-${item.label}` : item.path}
          estimatedItemHeight={36}
          itemGap={3}
          className={`${styles.navigationList} scroll-shadow-bottom`}
        >
          {(item) => item.kind === "group"
          ? <div className={styles.navigationGroup} key={`group-${item.label}`}>{item.label}</div>
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
