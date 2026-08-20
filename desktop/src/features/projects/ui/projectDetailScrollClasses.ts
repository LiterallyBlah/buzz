/** Layout classes for project details whose selected work item owns scrolling. */
export const projectDetailScrollClasses = {
  content: {
    default: "w-full space-y-3",
    owned: "flex min-h-0 flex-1 flex-col w-full space-y-3",
  },
  scroll: {
    default:
      "flex min-h-0 min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto overscroll-y-none px-4 pb-4",
    owned:
      "flex min-h-0 min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-hidden px-4 pb-4",
  },
} as const;
