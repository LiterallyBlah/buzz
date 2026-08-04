import { useProjectUnreadCount } from "@/features/projects/projectUnreadStore";
import { SidebarMenuBadge } from "@/shared/ui/sidebar";

/**
 * Unread indicator for the sidebar's Projects entry.
 *
 * Subscribes to the project-unread store directly instead of taking a count
 * prop. The alternative is threading a number from `AppShell` through
 * `AppSidebar` into `AppSidebarPrimaryMenu` — three files, two of which are at
 * the repo's 1000-line ceiling, to move a value that has exactly one consumer.
 * Isolating the subscription in its own leaf component also keeps
 * `AppSidebarPrimaryMenu` a pure render of its props.
 *
 * Counts unread WORK ITEMS, not events: "3" means three issues/PRs have
 * something new, which is the unit the user then goes and opens.
 */
export function ProjectsUnreadBadge() {
  const unreadCount = useProjectUnreadCount();

  if (unreadCount === 0) {
    return null;
  }

  return (
    <SidebarMenuBadge
      className="right-2 rounded-full bg-primary/15 px-1.5 text-2xs text-primary peer-data-[active=true]/menu-button:bg-sidebar-active-foreground/20 peer-data-[active=true]/menu-button:text-sidebar-active-foreground"
      data-testid="sidebar-projects-unread-count"
    >
      {Math.min(unreadCount, 99)}
    </SidebarMenuBadge>
  );
}
