import type { ReactNode } from "react";
import { useBroadcastI18n } from "../i18n";

export type SidebarView = "dashboard" | "issues" | "list" | "gantt" | "readme";

interface SidebarProject {
  id: string;
  name: string;
}

interface AppSidebarProps<P extends SidebarProject> {
  children: ReactNode;
  footer: ReactNode;
  boardView: SidebarView | null;
  viewsAvailable: boolean;
  showDocs: boolean;
  onSelectView: (view: SidebarView) => void;
  projects: P[];
  selectedProjectId: string;
  allProjectsSelected: boolean;
  projectsDisabled: boolean;
  onSelectAllProjects: () => void;
  onSelectProject: (project: P) => void;
}

const VIEW_ICONS: Record<SidebarView, ReactNode> = {
  dashboard: (
    <g strokeLinejoin="round">
      <rect x="2.5" y="2.5" width="4.5" height="4.5" rx="1" />
      <rect x="9" y="2.5" width="4.5" height="4.5" rx="1" />
      <rect x="2.5" y="9" width="4.5" height="4.5" rx="1" />
      <rect x="9" y="9" width="4.5" height="4.5" rx="1" />
    </g>
  ),
  issues: (
    <g strokeLinejoin="round">
      <rect x="2.5" y="2.5" width="3" height="11" rx="1" />
      <rect x="6.5" y="2.5" width="3" height="7" rx="1" />
      <rect x="10.5" y="2.5" width="3" height="9" rx="1" />
    </g>
  ),
  list: <path d="M5.5 4h8M5.5 8h8M5.5 12h8M2.5 4h.01M2.5 8h.01M2.5 12h.01" />,
  gantt: <path d="M2.5 4h6M5 8h7M8 12h5.5" />,
  readme: (
    <g strokeLinejoin="round">
      <path d="M4 2.5h5.5L12.5 5.5v8H4z" />
      <path d="M6.5 8.5h3.5M6.5 11h3.5" />
    </g>
  ),
};

function SidebarIcon({ children }: { children: ReactNode }) {
  return (
    <svg
      aria-hidden="true"
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
    >
      {children}
    </svg>
  );
}

export function AppSidebar<P extends SidebarProject>({
  children,
  footer,
  boardView,
  viewsAvailable,
  showDocs,
  onSelectView,
  projects,
  selectedProjectId,
  allProjectsSelected,
  projectsDisabled,
  onSelectAllProjects,
  onSelectProject,
}: AppSidebarProps<P>) {
  const { text } = useBroadcastI18n();
  const views: Array<{ view: SidebarView; label: string }> = [
    { view: "dashboard", label: text("仪表盘", "Dashboard") },
    { view: "issues", label: text("议题看板", "Issue board") },
    { view: "list", label: text("列表视图", "List") },
    { view: "gantt", label: text("甘特图", "Gantt") },
    ...(showDocs ? [{ view: "readme" as const, label: text("项目文档", "Project Docs") }] : []),
  ];

  return (
    <aside className="app-sidebar" aria-label={text("主导航", "Main navigation")}>
      <div className="app-sidebar-top">{children}</div>

      {viewsAvailable && (
        <nav className="app-sidebar-section" aria-label={text("看板视图", "Board views")}>
          {views.map(({ view, label }) => (
            <button
              key={view}
              className={`app-sidebar-item${boardView === view ? " active" : ""}`}
              type="button"
              aria-pressed={boardView === view}
              onClick={() => onSelectView(view)}
            >
              <SidebarIcon>{VIEW_ICONS[view]}</SidebarIcon>
              <span>{label}</span>
            </button>
          ))}
        </nav>
      )}

      <nav className="app-sidebar-section app-sidebar-projects" aria-label={text("项目", "Projects")}>
        <div className="app-sidebar-heading">{text("项目", "Projects")}</div>
        <button
          className={`app-sidebar-item${allProjectsSelected ? " active" : ""}`}
          type="button"
          aria-pressed={allProjectsSelected}
          disabled={projectsDisabled}
          onClick={() => {
            if (!allProjectsSelected) onSelectAllProjects();
          }}
        >
          <span className="app-sidebar-dot" aria-hidden="true" />
          <span>{text("所有项目", "All projects")}</span>
        </button>
        {projects.map((project) => {
          const active = project.id === selectedProjectId;
          return (
            <button
              key={project.id}
              className={`app-sidebar-item${active ? " active" : ""}`}
              type="button"
              aria-pressed={active}
              disabled={projectsDisabled}
              onClick={() => {
                if (!active) onSelectProject(project);
              }}
            >
              <span className="app-sidebar-dot" aria-hidden="true" />
              <span>{project.name}</span>
            </button>
          );
        })}
      </nav>

      <div className="app-sidebar-footer">{footer}</div>
    </aside>
  );
}
