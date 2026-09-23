import type { ActivityUnit } from "./charts/ContributionCalendarChart";
import styles from "./ActivityUnitToggle.module.scss";

export function ActivityUnitToggle({ value, onChange }: { value: ActivityUnit; onChange: (unit: ActivityUnit) => void }) {
  const options: Array<{ value: ActivityUnit; label: string }> = [
    { value: "hour", label: t("小时") },
    { value: "day", label: t("天") },
  ];
  return <div className={styles.root} role="group" aria-label={t("活动热力图单位")}>
    {options.map((option) => <button
      key={option.value}
      type="button"
      aria-pressed={value === option.value}
      onClick={() => onChange(option.value)}
    >{option.label}</button>)}
  </div>;
}
