import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useBroadcastI18n } from "../i18n";
import { ConversationIcon, ProjectIcon } from "./SemanticIcons";

export interface ComposerCompletionOption {
  id: string;
  label: string;
  description: string | null;
  icon: "conversation" | "project";
  selectableIndex: number;
}

export interface ComposerCompletionGroup {
  id: string;
  label: string;
  options: ComposerCompletionOption[];
}

interface ComposerCompletionMenuProps {
  anchor: HTMLElement;
  anchorRect: DOMRect;
  getAnchorRect: () => DOMRect;
  groups: ComposerCompletionGroup[];
  activeIndex: number;
  onActiveIndexChange: (index: number) => void;
  onSelect: (index: number) => void;
  onClose: () => void;
}

export function ComposerCompletionMenu({
  anchor,
  anchorRect,
  getAnchorRect,
  groups,
  activeIndex,
  onActiveIndexChange,
  onSelect,
  onClose,
}: ComposerCompletionMenuProps) {
  const { text } = useBroadcastI18n();
  const menuRef = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: 0, top: 0 });
  const portalTarget = anchor.closest("dialog") ?? document.body;

  useLayoutEffect(() => {
    const menu = menuRef.current;
    if (!menu) return;

    const place = () => {
      const nextAnchorRect = getAnchorRect();
      const menuRect = menu.getBoundingClientRect();
      const viewport = window.visualViewport;
      const viewportLeft = viewport?.offsetLeft ?? 0;
      const viewportTop = viewport?.offsetTop ?? 0;
      const viewportWidth = viewport?.width ?? window.innerWidth;
      const viewportHeight = viewport?.height ?? window.innerHeight;
      const gap = 4;
      const edge = 8;
      const openAbove = nextAnchorRect.top - gap - menuRect.height >= viewportTop + edge;
      const left = Math.max(
        viewportLeft + edge,
        Math.min(nextAnchorRect.left, viewportLeft + viewportWidth - menuRect.width - edge),
      );
      const desiredTop = openAbove
        ? nextAnchorRect.top - menuRect.height - gap
        : nextAnchorRect.bottom + gap;
      const top = Math.max(
        viewportTop + edge,
        Math.min(desiredTop, viewportTop + viewportHeight - menuRect.height - edge),
      );
      setPosition({ left, top });
    };

    place();
    window.addEventListener("resize", place);
    document.addEventListener("scroll", place, true);
    window.visualViewport?.addEventListener("resize", place);
    window.visualViewport?.addEventListener("scroll", place);
    return () => {
      window.removeEventListener("resize", place);
      document.removeEventListener("scroll", place, true);
      window.visualViewport?.removeEventListener("resize", place);
      window.visualViewport?.removeEventListener("scroll", place);
    };
  }, [activeIndex, anchorRect, getAnchorRect, groups]);

  useLayoutEffect(() => {
    menuRef.current
      ?.querySelector<HTMLElement>(`[data-completion-index="${activeIndex}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  useEffect(() => {
    function closeFromOutside(event: PointerEvent) {
      const target = event.target as Node;
      if (!anchor.contains(target) && !menuRef.current?.contains(target)) onClose();
    }

    document.addEventListener("pointerdown", closeFromOutside);
    return () => document.removeEventListener("pointerdown", closeFromOutside);
  }, [anchor, onClose]);

  return createPortal(
    <div
      ref={menuRef}
      className="ai-chat-skill-menu issue-mention-menu composer-completion-menu"
      role="listbox"
      aria-label={text("Composer 补全", "Composer completions")}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onPointerDown={(event) => event.preventDefault()}
    >
      {groups.map(
        (group) =>
          group.options.length > 0 && (
            <section
              className="composer-completion-group"
              role="group"
              aria-label={group.label}
              key={group.id}
            >
              <header aria-hidden="true">{group.label}</header>
              {group.options.map((option) => (
                <button
                  className={option.selectableIndex === activeIndex ? "is-selected" : ""}
                  type="button"
                  role="option"
                  aria-selected={option.selectableIndex === activeIndex}
                  data-completion-index={option.selectableIndex}
                  key={option.id}
                  onPointerEnter={() => onActiveIndexChange(option.selectableIndex)}
                  onClick={() => onSelect(option.selectableIndex)}
                >
                  {option.icon === "conversation" ? (
                    <ConversationIcon color="currentColor" />
                  ) : (
                    <ProjectIcon color="currentColor" />
                  )}
                  <span>
                    <strong>{option.label}</strong>
                    {option.description && <small>{option.description}</small>}
                  </span>
                </button>
              ))}
            </section>
          ),
      )}
    </div>,
    portalTarget,
  );
}
