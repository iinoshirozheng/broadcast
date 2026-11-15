import type { ImgHTMLAttributes } from "react";
import aiLauncher from "../assets/figma-broadcast/ai-launcher.svg";
import automationPause from "../assets/figma-broadcast/automation-pause.svg";
import automationPlay from "../assets/figma-broadcast/automation-play.svg";
import breadcrumb from "../assets/figma-broadcast/breadcrumb.svg";
import dropdown from "../assets/figma-broadcast/dropdown.svg";
import filter from "../assets/figma-broadcast/filter.svg";
import home from "../assets/figma-broadcast/home.svg";
import panel from "../assets/figma-broadcast/panel.svg";
import projectFolder from "../assets/figma-broadcast/project-folder.svg";
import search from "../assets/figma-broadcast/search.svg";

const BROADCAST_ICONS = {
  aiLauncher,
  automationPause,
  automationPlay,
  breadcrumb,
  dropdown,
  filter,
  home,
  panel,
  projectFolder,
  search,
} as const;

export type BroadcastIconName = keyof typeof BROADCAST_ICONS;

const MONOCHROME_ICONS = new Set<BroadcastIconName>([
  "aiLauncher",
  "automationPlay",
  "breadcrumb",
  "dropdown",
  "filter",
  "home",
  "panel",
  "projectFolder",
  "search",
]);

export function broadcastIconSource(name: BroadcastIconName) {
  return BROADCAST_ICONS[name];
}

interface BroadcastIconProps extends Omit<ImgHTMLAttributes<HTMLImageElement>, "alt" | "src"> {
  name: BroadcastIconName;
}

export function BroadcastIcon({ name, className, ...props }: BroadcastIconProps) {
  const classes = [
    "broadcast-icon",
    MONOCHROME_ICONS.has(name) ? "broadcast-icon-monochrome" : "",
    className ?? "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <img
      {...props}
      className={classes}
      src={BROADCAST_ICONS[name]}
      alt=""
      aria-hidden="true"
      draggable={false}
    />
  );
}
