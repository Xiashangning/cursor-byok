import DOMPurify from "dompurify";
import { marked } from "marked";
import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent } from "react";
import { api } from "../api";
import { Icon } from "./Icon";
import { informationOutlineIcon } from "./icons";
import { TooltipTrigger } from "./TooltipTrigger";
import styles from "./MarkdownInput.module.scss";

type MarkdownInputProps = {
  label: string;
  hint?: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  onChange: (value: string) => void;
};

/** 多行 markdown 输入:源码/预览两种视图,固定四行高度,内容超出时在框内滚动。 */
export function MarkdownInput({ label, hint, value, placeholder, disabled, className, onChange }: MarkdownInputProps) {
  const [preview, setPreview] = useState(false);
  const scroller = useRef<HTMLElement | null>(null);
  // 只在预览视图解析 markdown,输入时不产生额外开销。
  // style 属性会破坏弹窗布局,表单控件会带来应用内导航的风险,一并禁止。
  const html = useMemo(() => preview ? DOMPurify.sanitize(marked.parse(value, { async: false }), {
    FORBID_ATTR: ["style"],
    FORBID_TAGS: ["form", "input", "button", "select", "textarea", "label"],
  }) : "", [preview, value]);

  const setScroller = useCallback((node: HTMLElement | null) => { scroller.current = node; }, []);

  // 弹窗的滚动容器在冒泡阶段接管所有滚轮事件,字段自身会因此无法滚动。
  // 在字段节点上直接拦截:内容未到边界时终止冒泡,到边界后交还弹窗继续滚动。
  useEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const onWheel = (event: WheelEvent) => {
      const canScroll = event.deltaY < 0
        ? node.scrollTop > 0
        : node.scrollTop + node.clientHeight < node.scrollHeight - 1;
      if (canScroll) event.stopPropagation();
    };
    node.addEventListener("wheel", onWheel, { passive: true });
    return () => node.removeEventListener("wheel", onWheel);
  }, [preview]);

  // 链接交给系统浏览器打开,不在应用内导航;
  // 非 HTTP 链接(如 javascript:)会被 DOMPurify 剥掉 href,这里一并去掉链接外观。
  const followLink = (event: MouseEvent<HTMLDivElement>) => {
    const anchor = (event.target as Element).closest("a");
    if (!anchor) return;
    event.preventDefault();
    const href = anchor.getAttribute("href") ?? "";
    if (/^https?:/i.test(href)) void api.openExternalUrl(href).catch(() => undefined);
    else anchor.removeAttribute("href");
  };

  return <div className={[styles.root, className].filter(Boolean).join(" ")}>
    <div className={styles.header}>
      <div className={styles.label}>{label}</div>
      {hint && <TooltipTrigger label={hint}><div className={styles.hint}><Icon icon={informationOutlineIcon} size="1.1em" /></div></TooltipTrigger>}
      <div className={styles.toggle} role="group" aria-label={label}>
        <button type="button" aria-pressed={preview} onClick={() => setPreview(true)}>{t("预览")}</button>
        <button type="button" aria-pressed={!preview} onClick={() => setPreview(false)}>{t("源码")}</button>
      </div>
    </div>
    <div className={styles.box}>
      {preview
        ? html
          ? <div ref={setScroller} className={styles.preview} onClick={followLink} dangerouslySetInnerHTML={{ __html: html }} />
          : <div ref={setScroller} className={styles.preview}><span className={styles.empty}>{t("暂无内容")}</span></div>
        : <textarea
          ref={setScroller}
          className={styles.editor}
          value={value}
          placeholder={placeholder}
          disabled={disabled}
          spellCheck={false}
          aria-label={label}
          onChange={(event) => onChange(event.target.value)}
        />}
    </div>
  </div>;
}
