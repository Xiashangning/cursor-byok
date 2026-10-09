import { forwardRef, useImperativeHandle, useState } from "react";
import type { PluginModelDescriptor, PluginModelOverrideInput } from "../../shared/api";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { MarkdownInput } from "../../shared/ui/MarkdownInput";
import { Select } from "../../shared/ui/Select";
import { axisDefault, parseOptions } from "../../shared/utils/modelDefaults";
import styles from "./CursorSettings.module.scss";

export type PluginModelEditorHandle = { save: () => void };

type PluginModelEditorProps = {
  model: PluginModelDescriptor;
  busy: boolean;
  onSave: (input: PluginModelOverrideInput) => void;
};

export const PluginModelEditor = forwardRef<PluginModelEditorHandle, PluginModelEditorProps>(function PluginModelEditor({ model, busy, onSave }, ref) {
  const [displayName, setDisplayName] = useState(model.displayName);
  const [tooltip, setTooltip] = useState(model.description ?? "");
  const [effort, setEffort] = useState({ text: model.effortOptions.join(", "), options: model.effortOptions });
  const [context, setContext] = useState({ text: model.contextOptions.join(", "), options: model.contextOptions });
  const [defaultEffort, setDefaultEffort] = useState(model.defaultEffort ?? "");
  const [defaultContext, setDefaultContext] = useState(model.defaultContext ?? "");
  const [maxTokensText, setMaxTokensText] = useState(model.maxOutputTokens === null ? "" : String(model.maxOutputTokens));
  useImperativeHandle(ref, () => ({
    save: () => onSave({
      id: model.id,
      displayName: displayName.trim(),
      tooltip: tooltip.trim(),
      effortOptions: effort.options,
      contextOptions: context.options,
      maxOutputTokens: maxTokensText === "" ? null : Math.trunc(Number(maxTokensText)),
      defaultEffort: defaultEffort || null,
      defaultContext: defaultContext || null,
    }),
  }));
  return <div className={styles.editor}>
    <div className={styles.grid}>
      <FormField label={t("模型名称")}><div className={styles.staticValue}>{model.modelId}</div></FormField>
      <FormField label={t("显示名称")} hint={t("仅用于界面展示，不会改变发送给模型服务的模型名称。")}>
        <TextInput value={displayName} disabled={busy} onChange={(event) => setDisplayName(event.target.value)} />
      </FormField>
      <MarkdownInput
        className={styles.fullWidth}
        label={t("备注")}
        hint={t("显示在 Cursor 模型说明中，支持 Markdown。")}
        value={tooltip}
        disabled={busy}
        onChange={setTooltip}
      />
      <FormField label={t("Effort 选项")} hint={t("用逗号分隔模型可用的 effort 值。")}>
        <TextInput aria-label={t("Effort 选项")} value={effort.text} disabled={busy} onChange={(event) => { const options = parseOptions(event.target.value); setEffort({ text: event.target.value, options }); setDefaultEffort(axisDefault(options, defaultEffort)); }} />
      </FormField>
      <FormField label={t("Context 选项")} hint={t("用逗号分隔模型可用的 context 值，例如 200k, 1m。")}>
        <TextInput aria-label={t("Context 选项")} value={context.text} disabled={busy} onChange={(event) => { const options = parseOptions(event.target.value); setContext({ text: event.target.value, options }); setDefaultContext(axisDefault(options, defaultContext)); }} />
      </FormField>
      <div className={styles.defaultsRow}>
        <FormField label={t("默认 Effort")}>
          <Select ariaLabel={t("默认 Effort")} value={axisDefault(effort.options, defaultEffort)} disabled={busy} options={effort.options.map((value) => ({ value, label: value }))} onChange={setDefaultEffort} />
        </FormField>
        <FormField label={t("默认 Context")}>
          <Select ariaLabel={t("默认 Context")} value={axisDefault(context.options, defaultContext)} disabled={busy} options={context.options.map((value) => ({ value, label: value }))} onChange={setDefaultContext} />
        </FormField>
        <FormField label={t("最大输出 Token")} hint={t("留空时使用默认值。")}>
          <TextInput type="number" min={1} step={1} placeholder={t("留空使用默认值")} value={maxTokensText} disabled={busy} onChange={(event) => setMaxTokensText(event.target.value)} />
        </FormField>
      </div>
    </div>
  </div>;
});
