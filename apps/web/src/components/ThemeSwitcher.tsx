import type { ReactNode } from "react";
import { useBroadcastI18n } from "../i18n";

export type ThemePreference = "system" | "light" | "dark";

const THEME_ICONS: Record<ThemePreference, ReactNode> = {
  system: (
    <g strokeLinejoin="round">
      <rect x="2" y="3" width="12" height="8.5" rx="1.5" />
      <path d="M6 13.5h4" />
    </g>
  ),
  light: (
    <g>
      <circle cx="8" cy="8" r="2.75" />
      <path d="M8 1.75v1.5M8 12.75v1.5M1.75 8h1.5M12.75 8h1.5M3.58 3.58l1.06 1.06M11.36 11.36l1.06 1.06M3.58 12.42l1.06-1.06M11.36 4.64l1.06-1.06" />
    </g>
  ),
  dark: (
    <path strokeLinejoin="round" d="M13.25 9.6A5.5 5.5 0 0 1 6.4 2.75a5.5 5.5 0 1 0 6.85 6.85z" />
  ),
};

interface ThemeSwitcherProps {
  value: ThemePreference;
  onChange: (preference: ThemePreference) => void;
}

export function ThemeSwitcher({ value, onChange }: ThemeSwitcherProps) {
  const { text } = useBroadcastI18n();
  const options: Array<{ preference: ThemePreference; label: string }> = [
    { preference: "system", label: text("跟随系统", "System") },
    { preference: "light", label: text("浅色", "Light") },
    { preference: "dark", label: text("深色", "Dark") },
  ];

  return (
    <div className="theme-switcher">
      <span className="theme-switcher-label">{text("外观", "Appearance")}</span>
      <div className="theme-switcher-options" role="group" aria-label={text("外观", "Appearance")}>
        {options.map(({ preference, label }) => (
          <button
            key={preference}
            className={`theme-switcher-option${value === preference ? " active" : ""}`}
            type="button"
            aria-label={label}
            aria-pressed={value === preference}
            title={label}
            onClick={() => onChange(preference)}
          >
            <svg
              aria-hidden="true"
              width="14"
              height="14"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
            >
              {THEME_ICONS[preference]}
            </svg>
          </button>
        ))}
      </div>
    </div>
  );
}
