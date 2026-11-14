import { createContext, useContext, type ReactNode } from "react";
import * as OpenCC from "opencc-js/cn2t";
import type { TaskPriority, TaskStatus } from "./types";

export type BroadcastLanguage = "zh" | "en";

interface BroadcastI18n {
  language: BroadcastLanguage;
  locale: "zh-TW" | "en";
  text: (chinese: string, english: string) => string;
}

// UI copy is authored in Simplified Chinese; render it as Traditional Chinese (Taiwan phrasing).
const toTraditional = OpenCC.Converter({ from: "cn", to: "twp" });

const I18N: Record<BroadcastLanguage, BroadcastI18n> = {
  zh: {
    language: "zh",
    locale: "zh-TW",
    text: (chinese) => toTraditional(chinese),
  },
  en: {
    language: "en",
    locale: "en",
    text: (_chinese, english) => english,
  },
};

const STATUS_LABELS: Record<BroadcastLanguage, Record<TaskStatus, string>> = {
  zh: {
    backlog: "待立项",
    todo: "等待认领",
    in_progress: "处理中",
    in_review: "等你确认",
    blocked: "遇到阻碍",
    done: "完成",
    canceled: "取消",
  },
  en: {
    backlog: "Backlog",
    todo: "To do",
    in_progress: "In progress",
    in_review: "In review",
    blocked: "Blocked",
    done: "Done",
    canceled: "Canceled",
  },
};

const PRIORITY_LABELS: Record<BroadcastLanguage, Record<TaskPriority, string>> = {
  zh: {
    none: "无优先级",
    urgent: "紧急",
    high: "高",
    medium: "中",
    low: "低",
  },
  en: {
    none: "No priority",
    urgent: "Urgent",
    high: "High",
    medium: "Medium",
    low: "Low",
  },
};

const BroadcastLanguageContext = createContext<BroadcastLanguage>("en");

export function resolveBroadcastLanguage(value: string | null | undefined): BroadcastLanguage {
  const normalized = value?.trim().replaceAll("_", "-").toLowerCase() ?? "";
  return normalized === "zh" || normalized.startsWith("zh-") ? "zh" : "en";
}

export function getBroadcastI18n(language: BroadcastLanguage): BroadcastI18n {
  return I18N[language];
}

export function taskStatusLabel(language: BroadcastLanguage, status: TaskStatus): string {
  return I18N[language].text(STATUS_LABELS.zh[status], STATUS_LABELS.en[status]);
}

export function taskPriorityLabel(language: BroadcastLanguage, priority: TaskPriority): string {
  return I18N[language].text(PRIORITY_LABELS.zh[priority], PRIORITY_LABELS.en[priority]);
}

export function BroadcastLanguageProvider({
  language,
  children,
}: {
  language: BroadcastLanguage;
  children: ReactNode;
}) {
  return (
    <BroadcastLanguageContext.Provider value={language}>
      {children}
    </BroadcastLanguageContext.Provider>
  );
}

export function useBroadcastI18n(): BroadcastI18n {
  return I18N[useContext(BroadcastLanguageContext)];
}
