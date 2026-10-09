import { useEffect, useRef } from "react";
import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { TooltipProvider } from "./shared/ui/Tooltip";
import { MessageProvider } from "./shared/ui/MessageProvider";
import { useMessage } from "./shared/ui/message";
import { AppFrame } from "./shell/AppFrame";
import { AppLayout } from "./shell/AppLayout";
import { AccessTokenGate, consumeAccessTokenParam } from "./shell/AccessTokenGate";
import { CallsPage } from "./features/calls/CallsPage";
import { CallDetailsPage } from "./features/calls/CallDetailsPage";
import { CursorSettingsPage } from "./features/models/CursorSettingsPage";
import { HomePage } from "./features/home/HomePage";
import { PluginManagementPage } from "./features/plugins/PluginManagementPage";
import { SettingsPage } from "./features/settings/SettingsPage";
import { useAppStore } from "./shared/store/appStore";
import { updateStore } from "./shared/store/updateStore";
import { AUTO_UPDATE_CHECK_INTERVAL_MS, createAutomaticUpdateCheck } from "./shared/store/automaticUpdateCheck";

export function App() {
  const bootstrapped = useRef(false);
  if (!bootstrapped.current) {
    bootstrapped.current = true;
    consumeAccessTokenParam();
  }
  return (
    <TooltipProvider>
      <HashRouter>
        <Routes>
          <Route path="calls/:callId" element={<CallDetailsPage />} />
          <Route element={<AppFrame />}>
            <Route element={<AppLayout />}>
              <Route index element={<HomePage />} />
              <Route path="calls" element={<CallsPage />} />
              <Route path="harness/cursor" element={<CursorSettingsPage />} />
              <Route path="plugins" element={<PluginManagementPage />} />
              <Route path="settings" element={<SettingsPage />} />
            </Route>
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </HashRouter>
      <AccessTokenGate />
      <AppMessages />
    </TooltipProvider>
  );
}

function AppMessages() {
  const { error } = useAppStore();
  const previousError = useRef<string | null>(null);
  const automaticUpdateCheck = useRef(createAutomaticUpdateCheck(() => updateStore.check()));
  const showMessage = useMessage();

  useEffect(() => {
    if (error && error !== previousError.current) showMessage(error);
    previousError.current = error;
  }, [error, showMessage]);

  useEffect(() => {
    let disposed = false;
    const checkAutomatically = () => {
      const previousVersion = updateStore.getSnapshot().availableVersion;
      void automaticUpdateCheck.current().then((version) => {
        if (disposed || !version || version === previousVersion) return;
        showMessage(t("发现新版本 {version}，可在设置中安装", { version }), { duration: 6_000 });
      }).catch(() => {
        // Automatic checks are best-effort; a failure keeps the throttle
        // timestamp so the six-hour interval still applies. Manual checks in
        // Settings remain immediately available and report errors.
      });
    };
    const checkWhenVisible = () => {
      if (document.visibilityState === "visible") checkAutomatically();
    };

    checkAutomatically();
    window.addEventListener("focus", checkAutomatically);
    window.addEventListener("online", checkAutomatically);
    document.addEventListener("visibilitychange", checkWhenVisible);
    const timer = window.setInterval(checkAutomatically, AUTO_UPDATE_CHECK_INTERVAL_MS);
    return () => {
      disposed = true;
      window.removeEventListener("focus", checkAutomatically);
      window.removeEventListener("online", checkAutomatically);
      document.removeEventListener("visibilitychange", checkWhenVisible);
      window.clearInterval(timer);
    };
  }, [showMessage]);

  return <MessageProvider />;
}
