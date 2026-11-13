/** 实时页各处共用的应用清单与图标。图形均为本站静态常量。 */
export const appIcons: Record<string, string> = {
  qq: '<path d="M7 17 4 20v-5a8 8 0 1 1 3 2Z"/><path d="M8 9h8M8 13h5"/>',
  files: '<path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"/>',
  terminal: '<rect x="3" y="4" width="18" height="16" rx="3"/><path d="m7 9 3 3-3 3m6 0h4"/>',
  notes: '<path d="M6 3h9l4 4v14H6Z"/><path d="M14 3v5h5M9 12h7m-7 4h5"/>',
  memory: '<path d="m12 3 9 9-9 9-9-9Z"/><path d="m8 12 4 4 4-4-4-4Z"/>',
  browser:
    '<circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18"/>',
  tools: '<path d="m14 6 4-3 3 3-3 4-4-1-8 11-3-3 11-8Z"/><path d="m5 4 4 4m-6 0 4-4m8 11 5 5"/>',
};
export const appNames: Record<string, string> = {
  qq: "QQ",
  files: "文件",
  terminal: "终端",
  notes: "便签",
  memory: "记忆",
  browser: "网页",
  tools: "工具",
};
export const appIds = Object.keys(appNames);
export const appIcon = (id: string) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${
    appIcons[id]
  }</svg>`;
