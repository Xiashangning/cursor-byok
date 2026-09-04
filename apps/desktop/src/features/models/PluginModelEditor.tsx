import { forwardRef, useImperativeHandle, useState } from "react";
import type { PluginModelDescriptor, PluginModelOverrideInput } from "../../shared/api";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import styles from "./CursorSettings.module.scss";

function parseOptions(value: string): string[] {
  return [...new Set(value.split(",").map((item) => item.trim()).filter(Boolean))];
}

export type PluginModelEditorHandle = { save: () => void };

type PluginModelEditorProps = {
  model: PluginModelDescriptor;
  busy: boolean;
  onSave: (input: PluginModelOverrideInput) => void;
};

export const PluginModelEditor = forwardRef<PluginModelEditorHandle, PluginModelEditorProps>(function PluginModelEditor({ model, busy, onSave }, ref) {
  const [displayName, setDisplayName] = useState(model.displayName);
  const [tooltip, setTooltip] = useState(model.description ?? "");
  const [effortText, setEffortText] = useState(model.effortOptions.join(", "));
  const [contextText, setContextText] = useState(model.contextOptions.join(", "));
  const [maxTokensText, setMaxTokensText] = useState(model.maxOutputTokens === null ? "" : String(model.maxOutputTokens));
  useImperativeHandle(ref, () => ({
    save: () => onSave({
      id: model.id,
      displayName: displayName.trim(),
      tooltip: tooltip.trim(),
      effortOptions: parseOptions(effortText),
      contextOptions: parseOptions(contextText),
      maxOutputTokens: maxTokensText === "" ? null : Math.trunc(Number(maxTokensText)),
    }),
  }));
  return <div className={styles.editor}>
    <div className={styles.grid}>
      <FormField label={t("模型名称")}><div className={styles.staticValue}>{model.modelId}</div></FormField>
      <FormField label={t("显示名称")} hint={t("仅用于界面展示，不会改变发送给模型服务的模型名称。")}>
        <TextInput value={displayName} disabled={busy} onChange={(event) => setDisplayName(event.target.value)} />
      </FormField>
      <FormField className={styles.fullWidth} label={t("备注")} hint={t("显示在 Cursor 模型说明中。")}>
        <TextInput value={tooltip} disabled={busy} onChange={(event) => setTooltip(event.target.value)} />
      </FormField>
      <FormField label={t("Effort 选项")} hint={t("用逗号分隔模型可用的 effort 值。")}>
        <TextInput aria-label={t("Effort 选项")} value={effortText} disabled={busy} onChange={(event) => setEffortText(event.target.value)} />
      </FormField>
      <FormField label={t("Context 选项")} hint={t("用逗号分隔模型可用的 context 值，例如 200k, 1m。")}>
        <TextInput aria-label={t("Context 选项")} value={contextText} disabled={busy} onChange={(event) => setContextText(event.target.value)} />
      </FormField>
      <FormField label={t("最大输出 Token")} hint={t("留空时使用默认值。")}>
        <TextInput type="number" min={1} step={1} placeholder={t("留空使用默认值")} value={maxTokensText} disabled={busy} onChange={(event) => setMaxTokensText(event.target.value)} />
      </FormField>
    </div>
  </div>;
});
